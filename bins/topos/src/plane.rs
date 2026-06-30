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

use crate::error::ClientError;

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
#[derive(Clone, Debug)]
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
#[derive(Clone, Debug)]
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

/// What the client already holds as `current` — the conditional-GET validator. The source returns
/// [`PointerFetch::NotModified`] ONLY when its current matches BOTH the generation AND the commit, so a
/// record that reuses the same `(epoch,seq)` for a DIFFERENT `version_id` is always returned (and caught
/// as a reused-tuple ALARM) rather than hidden behind a generation-only 304. (The HTTP ETag is therefore
/// commit-sensitive, not just `<epoch>.<seq>`.)
#[derive(Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct KnownCurrent {
    pub generation: Generation,
    pub version_id: [u8; 32],
}

/// The client's read side of `current` + the version bytes. No write side (the client never moves the
/// pointer). No network this increment (fixtures); the HTTP wire is a later leaf.
pub(crate) trait PlaneSource {
    /// Conditional GET of a skill's signed `current` pointer. `known` is what the client already holds
    /// (its `observed` generation AND the commit recorded there): the source returns
    /// [`PointerFetch::NotModified`] only when its current matches both, else the signed record.
    fn get_current(
        &self,
        skill_id: &str,
        known: Option<KnownCurrent>,
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

// ---------------------------------------------------------------------------------------------
// The enrollment seam — the device-flow CLIENT's read/write side, behind a port so the `follow`
// tests run against a fake WITHOUT HTTP. Creds-free (it holds no read token): the device code + the
// grant are the only secrets it carries, and they are redacted from every `Debug`. The real impl is
// `crate::plane_http::UreqEnroll`; the fake lives in the follow tests.
// ---------------------------------------------------------------------------------------------

/// The RFC-8628 device-authorization grant from `device/authorize`.
#[derive(Clone)]
pub(crate) struct DeviceAuthorize {
    /// **SECRET** — the device code the client polls with. Redacted in `Debug`, never logged / in a URL.
    pub device_code: String,
    /// The short user code (also the `device_auth_id` the enroll possession frame binds).
    pub user_code: String,
    /// The verification URL a human visits to approve the session.
    pub verification_uri: String,
    /// The session lifetime, in seconds.
    pub expires_in: u64,
    /// The minimum poll interval, in seconds.
    pub interval: u64,
}

impl std::fmt::Debug for DeviceAuthorize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceAuthorize")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

/// The opaque single-use enrollment grant (the redeem credential). **SECRET** — its `Debug` is redacted
/// and it is never logged / placed in a URL / surfaced in an error.
#[derive(Clone)]
pub(crate) struct Grant(String);

impl Grant {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }
    /// The raw grant — used only to compute its `sha256` and to send it in the redeem body.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Grant(<redacted>)")
    }
}

/// The outcome of a `device/token` poll (RFC-8628). NOT an error — every variant is a legitimate poll
/// state. `Granted` carries the opaque grant (redacted `Debug`).
#[derive(Debug, Clone)]
pub(crate) enum TokenPoll {
    /// Not yet confirmed — the human hasn't approved the session yet; run `follow --resume` again later.
    /// (The two-call resume re-polls on demand, so no in-process interval is carried.)
    Pending,
    /// Polled too fast — back off (treated as still-pending by the on-demand resume).
    SlowDown,
    /// Denied at the verification page.
    Denied,
    /// The session expired before confirmation.
    Expired,
    /// Confirmed — the grant is present.
    Granted(Grant),
}

/// One minted per-skill read credential from a redeem (the `read_token` is a `0600` at-rest secret —
/// redacted `Debug`).
#[derive(Clone)]
pub(crate) struct RedeemedCred {
    pub skill_id: String,
    /// **SECRET** — redacted in `Debug`.
    pub read_token: String,
    pub expires_at: Option<i64>,
}

impl std::fmt::Debug for RedeemedCred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedeemedCred")
            .field("skill_id", &self.skill_id)
            .field("read_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// A successful redeem — the registered device key id + the minted per-skill read creds. **NO user token.**
#[derive(Debug, Clone)]
pub(crate) struct Redeem {
    pub workspace_id: String,
    pub device_key_id: String,
    pub read_creds: Vec<RedeemedCred>,
}

/// The creds-free enrollment transport (device-flow). The follow op drives it: read the TOFU bootstrap,
/// start a device-authorization, POLL for the grant (the agent only ever polls — never a user token), and
/// redeem the grant (the enroll possession signature rides a header) into per-skill read creds. The real
/// impl is `UreqEnroll`; the fake is the follow tests'.
pub(crate) trait EnrollSource {
    /// `GET /i/{token}` — the unauthenticated TOFU bootstrap (the workspace + the plane signing root).
    ///
    /// # Errors
    /// [`ClientError::Plane`] for a dead/unknown invite (404) or a transport fault; [`ClientError::Corrupt`]
    /// for a malformed body (incl. a non-Ed25519 `alg`, which fails the closed-enum deserialize).
    fn fetch_bootstrap(&self, token: &str) -> Result<topos_types::BootstrapData, ClientError>;

    /// `POST /v1/device/authorize` — begin a device-authorization against the invite.
    ///
    /// # Errors
    /// [`ClientError::Plane`] on a transport fault / non-OK status; [`ClientError::Corrupt`] on a malformed body.
    fn device_authorize(
        &self,
        token: &str,
        device_public_key: [u8; 32],
        machine_name: &str,
    ) -> Result<DeviceAuthorize, ClientError>;

    /// `POST /v1/device/token` — one poll of the session. The poll STATE (pending/slow_down/denied/expired/
    /// granted) is the `Ok` value; only a transport/parse fault is an `Err`.
    ///
    /// # Errors
    /// [`ClientError::Plane`] on a transport fault; [`ClientError::Corrupt`] on a malformed body.
    fn poll_token(&self, device_code: &str) -> Result<TokenPoll, ClientError>;

    /// `POST /v1/workspaces/{ws}/devices` — redeem the grant into a registered device + read creds. The
    /// 64-byte enroll possession signature rides the `Topos-Device-Signature` header.
    ///
    /// # Errors
    /// [`ClientError::Plane`] on a transport fault or a 200+DENIED redeem (e.g. a device-key mismatch);
    /// [`ClientError::Corrupt`] on a malformed body.
    fn redeem(
        &self,
        workspace_id: &str,
        grant: &str,
        device_public_key: [u8; 32],
        enroll_sig: [u8; 64],
    ) -> Result<Redeem, ClientError>;
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
        _known: Option<KnownCurrent>,
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
