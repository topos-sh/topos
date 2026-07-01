//! The plane seams — the client's read side of `current` ([`PlaneSource`]), the durable follow-state
//! ([`FollowSource`]), and the creds-free enrollment / governance / contribute write ports.
//!
//! Each mirrors the [`crate::fs_seam::FsOps`] / `ConfigStore` precedent: a narrow trait the engine
//! consumes, a real production impl, and a fixture test double. The production impls live in
//! [`crate::plane_http`] — the blocking `ureq` transports (`UreqPlane` for the read side, `UreqEnroll`
//! for the creds-free writes) — and are wired by the composition root whenever enrollment exists on disk
//! (`instance.json`; the follow-state comes from `follows.json`, written by `follow`). Before enrollment
//! the inert impls at the bottom of this file keep every verb honest (nothing followed, nothing served).
//! The engine tests drive the same traits over in-process fixtures with no HTTP. There is deliberately
//! **no `Transport` trait** — that abstraction would be premature.

use topos_core::digest::FileMode;
use topos_core::sync::Generation as KernelGen;
use topos_types::requests::{ProposeRequest, PublishRequest, RevertRequest, ReviewRequest};
use topos_types::{Generation, Receipt, SignedCurrentRecord, TerminalOutcome, WireError};

use crate::error::ClientError;

/// The response to a conditional `get_current`: either the pointer is unchanged (a 304), or the signed
/// record (which the caller authenticates before trusting).
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
/// alarm) so one skill's failure never aborts the whole pull.
#[derive(Debug)]
pub(crate) enum PlaneError {
    /// The skill or version is not served here (not followed, or unknown) — skip the skill.
    NotFound,
    /// A connect-level transport fault (dial / TLS / timeout, before any HTTP status): the PLANE ITSELF
    /// is unreachable, not just one resource — so a sweep's circuit breaker trips on the first one and
    /// short-circuits every remaining network call this invocation. Handled like [`Self::Unavailable`]
    /// everywhere else (keep state, retry later).
    Unreachable(String),
    /// The plane answered but this read failed transiently (a 5xx / unexpected status / a truncated
    /// body) — keep state, retry later (a retryable warning). Never trips the sweep breaker.
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
pub(crate) struct KnownCurrent {
    pub generation: Generation,
    pub version_id: [u8; 32],
}

/// The client's read side of `current` + the version bytes. No write side (a pointer move rides the
/// device-signed [`ContributeSource`], never this read seam). The production impl is the `ureq`
/// [`crate::plane_http::UreqPlane`]; the engine tests feed fixtures.
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

    /// The OPEN proposals' version ids (the `@hash` handles) for a followed skill — the count feeds
    /// `pull --json`'s `proposals_awaiting`, and the handles drive `list <skill>`. A read (no signature):
    /// the per-skill read token authorizes it. The default is empty (fixtures / the inert source see no
    /// proposals); the real `UreqPlane` overrides it with the GET, mapping a 404 (no/unknown token or scope
    /// mismatch — indistinguishable) to an empty list rather than an error, so the count is best-effort.
    fn list_open_proposals(&self, _skill_id: &str) -> Result<Vec<[u8; 32]>, PlaneError> {
        Ok(Vec::new())
    }
}

/// How a skill is followed — the engine consults this to choose the consent situation. Persisted by
/// enrollment in `follows.json` (as [`crate::enroll::FollowModeDoc`], mapped 1:1 at load).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FollowMode {
    /// Auto-apply a new `current` (the standing-follow pre-authorization).
    Auto,
    /// One-tap accept each new `current` (`--manual`).
    ConfirmEach,
}

/// The per-skill follow-state the engine needs. The `workspace_id` is the EXPECTED scope — a signed
/// pointer whose scope names a different workspace (even with the same skill id and plane key) is a
/// cross-workspace replay and is refused. (The read credential lives with the TRANSPORT — a
/// [`crate::plane_http::SkillCred`] — never here: creds in the transport, consent in the follow seam.)
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

/// The durable follow-state source. The production impl is [`crate::plane_http::FileFollow`] over the
/// `follows.json` enrollment doc; [`InertFollow`] (nothing enrolled) and the fixtures follow nothing.
pub(crate) trait FollowSource {
    /// The followed skills, each with its follow-state, keyed by stable skill id.
    fn followed(&self) -> Vec<(String, FollowContext)>;
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

// ---------------------------------------------------------------------------------------------
// The governance-write seam — the OWNER's signed-op write side (invite today), behind a port so the
// `invite` tests run against a fake WITHOUT HTTP. Creds-free: the 64-byte governance signature rides a
// header, not a read token; the base URL is baked in when the connector builds it from `instance.json`.
// The real impl is `crate::plane_http::UreqEnroll` (the same client that speaks enrollment); the fake
// lives in the invite tests.
// ---------------------------------------------------------------------------------------------

/// The owner's governance-write transport. The `invite` op drives it: POST the owner-signed governance
/// Invite op to `/v1/invites`, the 64-byte signature in the `Topos-Device-Signature` header.
pub(crate) trait GovernanceSource {
    /// `POST /v1/invites` — submit the owner-signed governance Invite op (the body is the
    /// [`InviteRequest`](topos_types::requests::InviteRequest); the signature rides the header). Maps the
    /// all-outcome **200 envelope**: `ok` ⇒ the [`InviteData`](topos_types::results::InviteData); `!ok` ⇒ a
    /// typed error carrying the wire error's code (a role-DENIED surfaces as a clear "not authorized").
    ///
    /// # Errors
    /// [`ClientError::Plane`] on a transport fault, a non-200 status, or a 200+DENIED envelope (e.g. the
    /// signer is not an owner); [`ClientError::Corrupt`] on a malformed body.
    fn create_invite(
        &self,
        body: topos_types::requests::InviteRequest,
        governance_sig: [u8; 64],
    ) -> Result<topos_types::results::InviteData, ClientError>;
}

// ---------------------------------------------------------------------------------------------
// The contribute-write seam — the device-signed write side (publish / propose / revert / review),
// behind a port so the contribute tests run against a fake WITHOUT HTTP. Creds-free: the 64-byte
// device-op signature is the auth, riding a header, not a read token; the base URL is baked in when
// the connector builds it from `instance.json`. The real impl is `crate::plane_http::UreqEnroll` (the
// same creds-free client that speaks enrollment + governance); the fake lives in the contribute tests.
// ---------------------------------------------------------------------------------------------

/// The typed result of a contribute write. Carries the three parts of the all-outcome **200 envelope**
/// verbatim — the client mirror of the plane's `SetCurrentReceipt`. EVERY terminal protocol outcome
/// (OK / NEEDS_REVIEW / CONFLICT / APPROVAL_REQUIRED / DENIED) is a `WriteReceipt`; only a transport /
/// non-200 / malformed-body fault is a [`ClientError`].
#[derive(Debug, Clone)]
pub(crate) struct WriteReceipt {
    /// The canonical all-outcome receipt (present on every write 200). `outcome` is the branch
    /// discriminant; `version_id` + `bundle_digest` + `current_generation` build the verb's `--json` data.
    pub receipt: Receipt,
    /// The flat wire error on a non-OK outcome (CONFLICT / APPROVAL_REQUIRED / DENIED) — the fine `code`,
    /// the live `current_generation` (the CONFLICT rebase target), and the typed `next_actions`. `None` on
    /// OK / NEEDS_REVIEW.
    pub error: Option<WireError>,
    /// The signed `current` pointer — `Some` ONLY when a pointer actually moved (publish / revert /
    /// review-approve OK). `None` for NEEDS_REVIEW, an OK `review --reject` (signs nothing → data `{}`), and
    /// every failure. NOT trusted here: the caller verifies it against the pinned plane key before advancing
    /// the anti-rollback floor.
    pub signed_record: Option<SignedCurrentRecord>,
}

impl WriteReceipt {
    /// The terminal outcome the verb branches on.
    pub(crate) fn outcome(&self) -> TerminalOutcome {
        self.receipt.outcome
    }
}

/// The device-signed contribute-write transport — the four POST verbs that move (or propose to move)
/// `current`. Creds-free: the 64-byte device-op signature is the auth, riding the `Topos-Device-Signature`
/// header (NOT a read token); the body carries only the `device_key_id` that names the signing key. The op
/// kind is derived from the route server-side, so the transport ships only the body + the signature and is
/// op-agnostic. EVERY terminal protocol outcome (OK / NEEDS_REVIEW / CONFLICT / APPROVAL_REQUIRED / DENIED)
/// is an `Ok(WriteReceipt)`; only a transport / non-200 / malformed-body fault is an `Err`. The real impl is
/// [`crate::plane_http::UreqEnroll`].
pub(crate) trait ContributeSource {
    /// `POST /v1/publish` — a direct publish that moves `current` (or genesis).
    ///
    /// # Errors
    /// [`ClientError::Plane`] on a transport fault / non-200 status; [`ClientError::Corrupt`] on a malformed
    /// envelope (a body that carries no receipt is corrupt).
    fn publish(
        &self,
        body: PublishRequest,
        device_sig: [u8; 64],
    ) -> Result<WriteReceipt, ClientError>;

    /// `POST /v1/proposals` — open a proposal (ingest a candidate WITHOUT moving `current`).
    ///
    /// # Errors
    /// As [`publish`](Self::publish).
    fn propose(
        &self,
        body: ProposeRequest,
        device_sig: [u8; 64],
    ) -> Result<WriteReceipt, ClientError>;

    /// `POST /v1/reverts` — a forward revert (the server builds the forward commit from `good`'s tree).
    ///
    /// # Errors
    /// As [`publish`](Self::publish).
    fn revert(
        &self,
        body: RevertRequest,
        device_sig: [u8; 64],
    ) -> Result<WriteReceipt, ClientError>;

    /// `POST /v1/reviews` — approve or reject a proposal (the verdict rides `ReviewRequest.decision`).
    ///
    /// # Errors
    /// As [`publish`](Self::publish).
    fn review(
        &self,
        body: ReviewRequest,
        device_sig: [u8; 64],
    ) -> Result<WriteReceipt, ClientError>;
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
// Inert impls — what the composition root wires when NO enrollment exists on disk (a fresh install,
// or one that never ran `follow`). They keep `pull` a truthful no-op: nothing is followed, so the
// engine's followed-skills loop is empty. Once `instance.json` exists, the real `ureq` transports
// replace them.
// ---------------------------------------------------------------------------------------------

/// The not-enrolled plane source: it serves nothing (every call is a fail-closed unavailable). It is
/// never reached in practice because [`InertFollow`] follows nothing, so the engine never calls it.
#[derive(Debug, Default)]
pub(crate) struct InertPlane;

impl PlaneSource for InertPlane {
    fn get_current(
        &self,
        _skill_id: &str,
        _known: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError> {
        Err(PlaneError::Unavailable(
            "not enrolled with a plane; run `topos follow <link>` first".into(),
        ))
    }
    fn fetch_version(
        &self,
        _skill_id: &str,
        _version_id: [u8; 32],
    ) -> Result<FetchedVersion, PlaneError> {
        Err(PlaneError::Unavailable(
            "not enrolled with a plane; run `topos follow <link>` first".into(),
        ))
    }
}

/// The not-enrolled follow source: nothing is followed, so `pull` is a no-op.
#[derive(Debug, Default)]
pub(crate) struct InertFollow;

impl FollowSource for InertFollow {
    fn followed(&self) -> Vec<(String, FollowContext)> {
        Vec::new()
    }
}
