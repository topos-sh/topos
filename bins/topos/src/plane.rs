//! The plane-response source seam — the client's read side of `current`, behind a port so the engine is
//! exercised in-process against fixtures with **no HTTP and no network** this increment.
//!
//! This mirrors the [`crate::fs_seam::FsOps`] / `ConfigStore` precedent: a narrow trait the engine
//! consumes, an inert production impl, and a fixture test double. The conditional-GET / 304 **state
//! logic** (does the pointer name a newer generation than the client's `observed`?) is built and tested
//! NOW; the real HTTP transport (a thin `reqwest`/ETag round-trip) is a later leaf. There is deliberately
//! **no `Transport` trait** — that abstraction would be premature.
//!
//! The follow-state (which skills are followed, in which mode, in which workspace, with which read
//! credential) is the enrollment subsystem's, which lands later. This increment **consumes** it through
//! [`FollowSource`], fixture-supplied; the inert production impl follows nothing, so production `pull`
//! stays the honest no-op it is today while the engine, floor, materializer, and crash-safety are real.

use topos_core::digest::FileMode;
use topos_core::sync::Generation as KernelGen;
use topos_types::{Generation, SignedCurrentRecord};

/// The response to a conditional `get_current`: either the pointer is unchanged (a 304), or the signed
/// record (which the caller authenticates before trusting).
///
/// Constructed by the fixture test double + the future HTTP transport; the inert production source never
/// serves a record (it errors), so these variants are not built in the current non-test path.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum PointerFetch {
    /// The pointer has not moved past the client's known generation. The engine still drives `applied`
    /// toward `observed` (a prior apply may be pending).
    NotModified,
    /// The signed `current` record. NOT yet trusted — the engine verifies the signature + scope first.
    Record(SignedCurrentRecord),
}

/// A version's bytes + the commit metadata needed to **re-derive its `version_id`** locally (the
/// integrity gate recomputes `commit_id(parents, tree, author, message)` and the bundle digest, so the
/// source is never trusted on its word). Carries the full commit frame, not just files.
#[derive(Clone)]
pub(crate) struct FetchedVersion {
    /// The parent `version_id`s (the commit frame's `parents`; `parents[0]` is the trunk parent).
    pub parents: Vec<[u8; 32]>,
    /// The commit author device id (part of the `commit_id` preimage).
    pub author: String,
    /// The commit message (part of the `commit_id` preimage).
    pub message: String,
    /// The bundle's files (raw bytes + mode + bundle-relative path).
    pub files: Vec<FetchedFile>,
}

/// One fetched file. `mode` is part of the consent-bound digest, so it is carried, not inferred.
#[derive(Clone)]
pub(crate) struct FetchedFile {
    pub path: String,
    pub mode: FileMode,
    pub bytes: Vec<u8>,
}

/// Why a plane read could not be satisfied. The engine maps each to a per-skill outcome (skip / retry /
/// alarm) so one skill's failure never aborts the whole pull. The inert production source only ever
/// reports `Unavailable`; `NotFound`/`Malformed` are produced by the fixture + the future HTTP transport.
#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum PlaneError {
    /// The skill or version is not served here (not followed, or unknown) — skip the skill.
    NotFound,
    /// The plane is transiently unreachable — keep state, retry later (a retryable warning).
    Unavailable(String),
    /// The served response was structurally malformed (a corrupt/forged record or bytes) — surface it.
    Malformed(String),
}

/// The client's read side of `current` + the version bytes. No write side (the client never moves the
/// pointer). No network this increment (fixtures); the HTTP wire is a later leaf.
pub(crate) trait PlaneSource {
    /// Conditional GET of a skill's signed `current` pointer. `known` is the client's `observed`
    /// generation (the ETag): the source returns [`PointerFetch::NotModified`] when the pointer has not
    /// moved past it, else the signed record.
    fn get_current(
        &self,
        skill_id: &str,
        known: Option<Generation>,
    ) -> Result<PointerFetch, PlaneError>;

    /// Fetch a specific version's bytes + commit frame (for the durable write + the re-verify gate).
    fn fetch_version(
        &self,
        skill_id: &str,
        version_id: [u8; 32],
    ) -> Result<FetchedVersion, PlaneError>;
}

/// How a skill is followed — the engine consults this to choose the consent situation. Fixture-supplied
/// this increment (no real `follow` verb yet); persisted by the enrollment subsystem when it lands. The
/// inert production source follows nothing, so neither mode is constructed in the current non-test path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum FollowMode {
    /// Auto-apply a new `current` (the standing-follow pre-authorization).
    Auto,
    /// One-tap accept each new `current` (`--manual`).
    ConfirmEach,
}

/// The per-skill follow-state the engine needs. The `workspace_id` is the EXPECTED scope — a signed
/// pointer whose scope names a different workspace (even with the same skill id and plane key) is a
/// cross-workspace replay and is refused. (The read credential the HTTP transport will need lands with
/// that leaf — it has no consumer yet, so it is not carried prematurely.)
#[derive(Debug, Clone)]
pub(crate) struct FollowContext {
    /// The workspace this skill is followed in — the expected pointer scope.
    pub workspace_id: String,
    pub mode: FollowMode,
    /// Whether the workspace gates moves behind review (the follower still only ever receives an
    /// already-approved `current`; this only selects the consent satisfier).
    pub review_required: bool,
    /// Whether the skill is currently followed (a `false` skill is inventoried but not pulled).
    pub following: bool,
}

/// The durable follow-state source. Fixture-supplied this increment; the inert production impl follows
/// nothing, so production `pull` reports an honestly empty state.
pub(crate) trait FollowSource {
    /// The followed skills, each with its follow-state, keyed by stable skill id.
    fn followed(&self) -> Vec<(String, FollowContext)>;
    /// Proposals awaiting *me* as a reviewer (always `0` until proposals/review land).
    fn proposals_awaiting(&self) -> u32;
}

/// Compare two wire generations with the kernel's epoch-dominant order (the wire type derives none).
pub(crate) fn gen_cmp(a: Generation, b: Generation) -> core::cmp::Ordering {
    KernelGen {
        epoch: a.epoch,
        seq: a.seq,
    }
    .cmp(&KernelGen {
        epoch: b.epoch,
        seq: b.seq,
    })
}

// ---------------------------------------------------------------------------------------------
// Inert production impls — no plane is wired yet (no enrollment, no HTTP). They keep `pull` a
// truthful no-op: nothing is followed, so the engine's followed-skills loop is empty.
// ---------------------------------------------------------------------------------------------

/// The production plane source until the HTTP transport lands: it serves nothing (every call is a
/// not-found / unreachable). It is never reached in production today because [`InertFollow`] follows
/// nothing, so the engine never calls it — but it fails closed if it ever were.
#[derive(Debug, Default)]
pub(crate) struct InertPlane;

impl PlaneSource for InertPlane {
    fn get_current(
        &self,
        _skill_id: &str,
        _known: Option<Generation>,
    ) -> Result<PointerFetch, PlaneError> {
        Err(PlaneError::Unavailable(
            "no plane transport is wired yet".into(),
        ))
    }
    fn fetch_version(
        &self,
        _skill_id: &str,
        _version_id: [u8; 32],
    ) -> Result<FetchedVersion, PlaneError> {
        Err(PlaneError::Unavailable(
            "no plane transport is wired yet".into(),
        ))
    }
}

/// The production follow source: nothing is followed yet (no `follow` verb), so `pull` is a no-op.
#[derive(Debug, Default)]
pub(crate) struct InertFollow;

impl FollowSource for InertFollow {
    fn followed(&self) -> Vec<(String, FollowContext)> {
        Vec::new()
    }
    fn proposals_awaiting(&self) -> u32 {
        0
    }
}
