//! The plane seams — the client's read side of `current` ([`PlaneSource`]), the durable follow-state
//! ([`FollowSource`]), and the creds-free enrollment / governance / contribute write ports.
//!
//! Each mirrors the [`crate::fs_seam::FsOps`] / `ConfigStore` precedent: a narrow trait the engine
//! consumes, a real production impl, and a fixture test double. The production impls live in
//! [`crate::plane_http`] — the blocking `ureq` transports (`UreqPlane` for the read side, `UreqDeviceClient`
//! for the creds-free writes) — and are wired by the composition root whenever enrollment exists on disk
//! (`instance.json`; the follow-state comes from `follows.json`, written by `follow`). Before enrollment
//! the inert impls at the bottom of this file keep every verb honest (nothing followed, nothing served).
//! The engine tests drive the same traits over in-process fixtures with no HTTP. There is deliberately
//! **no `Transport` trait** — that abstraction would be premature.

use topos_core::digest::FileMode;
use topos_types::requests::{
    ProposeRequest, PublishRequest, RevertRequest, ReviewRequest, WireSkillIndex,
};
use topos_types::{Generation, Receipt, TerminalOutcome, WireCurrentRecord, WireError};

use crate::error::ClientError;

/// The response to a conditional `get_current`: either the pointer is unchanged (a 304), or the served
/// unsigned `current` record (the engine scope-checks it, then drives toward it).
pub(crate) enum PointerFetch {
    /// The pointer has not moved past the client's known generation. The engine still drives `applied`
    /// toward `observed` (a prior apply may be pending).
    NotModified,
    /// The served unsigned `current` record. The engine checks its (workspace, skill) scope, then treats
    /// it as the sync target; integrity is the content-addressed `version_id` re-verified on apply.
    Record(WireCurrentRecord),
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
/// surface) so one skill's failure never aborts the whole pull.
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
/// record that reuses the same `(epoch,seq)` for a DIFFERENT `version_id` is always returned (and applied
/// as the new target) rather than hidden behind a generation-only 304. (The HTTP ETag is therefore
/// commit-sensitive, not just `<epoch>.<seq>`.)
#[derive(Clone, Copy)]
pub(crate) struct KnownCurrent {
    pub generation: Generation,
    pub version_id: [u8; 32],
}

/// The client's read side of `current` + the version bytes. No write side (a pointer move rides the
/// [`ContributeSource`], never this read seam). The production impl is the `ureq`
/// [`crate::plane_http::UreqPlane`]; the engine tests feed fixtures.
pub(crate) trait PlaneSource {
    /// Conditional GET of a skill's `current` pointer. `known` is what the client already holds (its
    /// `observed` generation AND the commit it names): the source returns [`PointerFetch::NotModified`]
    /// only when its current matches both, else the served record.
    fn get_current(
        &self,
        skill_id: &str,
        known: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError>;

    /// Fetch a specific version's bytes + commit frame (for the durable write + the re-verify gate). The
    /// engine re-derives the `version_id` from the bytes, so a lying response fails the digest check.
    fn fetch_version(
        &self,
        skill_id: &str,
        version_id: [u8; 32],
    ) -> Result<FetchedVersion, PlaneError>;

    /// The OPEN proposals' version ids (the `@hash` handles) for a followed skill — the count feeds
    /// `pull --json`'s `proposals_awaiting`, and the handles drive `list <skill>`. A read: the workspace
    /// credential authorizes it. The default is empty (fixtures / the inert source see no
    /// proposals); the real `UreqPlane` overrides it with the GET, mapping a 404 (no credential or scope
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

/// The per-skill follow-state the engine needs. The `workspace_id` is the EXPECTED scope — a served
/// pointer whose scope names a different workspace (even with the same skill id) is a mis-scoped response
/// and is rejected. (The read credential lives with the TRANSPORT — a
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
// The delivery seam — the server-computed "what should this device have" the reconcile sweep
// drives (one call per workspace instead of a per-skill pointer fan-out), plus the applied-state
// report that feeds the fleet page. Behind a port so the reconcile is tested against a fake
// with no HTTP; the production impl is the `ureq` transport.
// ---------------------------------------------------------------------------------------------

/// One skill the workspace delivers to this device — the reconcile's install/update target.
#[derive(Debug, Clone)]
pub(crate) struct DeliverySkill {
    /// The stable plane-minted skill id (the sidecar key).
    pub skill_id: String,
    /// The catalog's user-facing name (a fresh install's directory name).
    pub name: String,
    /// Whether the bundle is effectively `reviewed` (the client's publish-preflight posture; the
    /// server re-decides authoritatively on every write).
    pub review_required: bool,
    /// The pinned current version (the sync target — the engine re-verifies bytes by digest).
    pub version_id: [u8; 32],
    pub generation: Generation,
}

/// The per-workspace delivery snapshot: what to have, and what the PERSON detached (freeze in
/// place — absence alone cannot distinguish "you detached this" from "upstream withdrew this",
/// and the two have opposite on-disk consequences).
#[derive(Debug, Clone)]
pub(crate) struct DeliverySnapshot {
    pub skills: Vec<DeliverySkill>,
    /// Skill ids the person detached (unfollowed / lapsed): bytes stay in place, frozen.
    pub detached: Vec<String>,
    /// Skill ids THIS DEVICE excludes ("not on this device") — the third actor: the copy leaves this
    /// device (the `remove` verb already cleared it), the person keeps receiving it elsewhere.
    pub excluded: Vec<String>,
    /// OPEN proposals across the delivered set (the `proposals_awaiting` gauge).
    pub proposals_awaiting: u64,
}

/// The delivery + fleet transport, per enrolled workspace. The production impl rides the workspace
/// Bearer credential; the reconcile tests feed fixtures.
pub(crate) trait DeliverySource {
    /// The enrolled workspaces this device can ask deliveries for (from `credentials.json`).
    fn workspaces(&self) -> Vec<String>;

    /// One workspace's delivery snapshot. [`PlaneError::NotFound`] means THIS DEVICE lost the whole
    /// workspace (removed from the roster / revoked): the sweep freezes everything in place and
    /// warns — never a clean.
    fn fetch_delivery(&self, workspace_id: &str) -> Result<DeliverySnapshot, PlaneError>;

    /// Report what this device holds after its reconcile (skill id → applied version). Best-effort
    /// visibility (the fleet page's truth): a failure warns, never blocks the sync.
    fn report_applied(
        &self,
        workspace_id: &str,
        applied: &[(String, [u8; 32])],
    ) -> Result<(), PlaneError>;

    /// Bind a DELIVERED skill to its workspace credential on the READ transport. The per-skill
    /// credential map is derived from `follows.json`, which by definition does not yet name a
    /// brand-new arrival — so without this, the arrival's very first version fetch would answer
    /// "not served" and cost a spurious error plus a session's delay. The workspace credential
    /// already authenticates every skill in its workspace (membership IS the authorization), so
    /// binding is a lookup, never a new secret. Default: a no-op (the fixture transports carry
    /// their creds up front).
    fn bind_skill(&self, _workspace_id: &str, _skill_id: &str) {}
}

// ---------------------------------------------------------------------------------------------
// The enrollment seam — the device-flow CLIENT's read/write side, behind a port so the `follow`
// tests run against a fake WITHOUT HTTP. Creds-free (it holds no read token): the device code + the
// grant are the only secrets it carries, and they are redacted from every `Debug`. The real impl is
// `crate::plane_http::UreqDeviceClient`; the fake lives in the follow tests.
// ---------------------------------------------------------------------------------------------

/// The RFC-8628 device-authorization grant from `device/authorize`.
#[derive(Clone)]
pub(crate) struct DeviceAuthorize {
    /// **SECRET** — the device code the client polls with. Redacted in `Debug`, never logged / in a URL.
    pub device_code: String,
    /// The short user code (also the `device_auth_id` the enroll flow presents).
    pub user_code: String,
    /// The verification URL a human visits to approve the session.
    pub verification_uri: String,
    /// The verification URL with the code already embedded (RFC-8628 `verification_uri_complete`) — the
    /// SERVER-built link, used verbatim when present (the client-side reconstruction is only the fallback
    /// for an older plane that omits it).
    pub verification_uri_complete: Option<String>,
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
            .field("verification_uri_complete", &self.verification_uri_complete)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

/// A STANDUP device-authorization start: the RFC-8628 grant PLUS the plane block a standup device has no
/// `/i/` bootstrap to learn — the base URL / posture / enrollment method.
#[derive(Debug, Clone)]
pub(crate) struct StandupAuthorize {
    pub auth: DeviceAuthorize,
    /// The plane's self-description (its API base to dial, posture, enrollment method).
    pub plane: topos_types::bootstrap::BootstrapPlane,
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

/// The workspace context a granted poll carries — the id + display name a STANDUP client (which never read
/// an `/i/` bootstrap) needs to build its redeem body and disclose what it joined. `None` on an
/// older plane's response (the enroll flow already knows its workspace from the bootstrap).
#[derive(Debug, Clone)]
pub(crate) struct GrantedWorkspace {
    pub workspace_id: String,
    pub display_name: String,
    /// The workspace's full ADDRESS (server-built on the public link base) — the standup receipt's share
    /// surface; `None` when the plane predates addresses or the grant is workspace-less.
    pub address: Option<String>,
}

/// A granted `device/token` poll: the opaque grant (redacted `Debug`) + the optional workspace context.
#[derive(Debug, Clone)]
pub(crate) struct GrantedToken {
    pub grant: Grant,
    pub workspace: Option<GrantedWorkspace>,
}

/// The outcome of a `device/token` poll (RFC-8628). NOT an error — every variant is a legitimate poll
/// state. `Granted` carries the opaque grant (redacted `Debug`).
#[derive(Debug, Clone)]
pub(crate) enum TokenPoll {
    /// Not yet confirmed — the human hasn't approved the session yet; re-invoke `follow` again later.
    /// (Re-invoking `follow` re-polls on demand, so no in-process interval is carried.)
    Pending,
    /// Polled too fast — back off (treated as still-pending by the on-demand resume).
    SlowDown,
    /// Denied at the verification page.
    Denied,
    /// The session expired before confirmation.
    Expired,
    /// Confirmed — the grant (and, for a standup session, the workspace context) is present.
    Granted(GrantedToken),
}

/// A successful redeem — the registered device key id + the ONE minted **workspace credential** (the Bearer
/// secret this device presents on every read/write/governance request in the workspace). **NO user token,
/// no per-skill token.** Hand-written `Debug` redacts the credential.
#[derive(Clone)]
pub(crate) struct Redeem {
    pub workspace_id: String,
    pub device_key_id: String,
    /// The principal this device now acts as (the confirmed email, or a device-rooted id) — a disclosure
    /// the client persists and prints so a hijacked standup is visible. `None` from an older plane.
    pub principal: Option<String>,
    /// **SECRET** — the plaintext workspace credential (returned once; redacted in `Debug`).
    pub credential: String,
}

impl std::fmt::Debug for Redeem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redeem")
            .field("workspace_id", &self.workspace_id)
            .field("device_key_id", &self.device_key_id)
            .field("principal", &self.principal)
            .field("credential", &"<redacted>")
            .finish()
    }
}

/// The creds-free enrollment transport (device-flow). The follow op drives it: read the bootstrap,
/// start a device-authorization, POLL for the grant (the agent only ever polls — never a user token), and
/// redeem the grant into the workspace credential. The real impl is `UreqDeviceClient`; the fake is the
/// follow tests'.
pub(crate) trait EnrollSource {
    /// `GET /i/{token}` — the unauthenticated invite bootstrap (the workspace + the plane API base to dial).
    ///
    /// # Errors
    /// [`ClientError::Plane`] for a dead/unknown invite (404) or a transport fault; [`ClientError::Corrupt`]
    /// for a malformed body.
    fn fetch_bootstrap(&self, token: &str) -> Result<topos_types::BootstrapData, ClientError>;

    /// `POST /v1/device/authorize` with `intent = "enroll"` — begin a device-authorization to join the
    /// workspace named by its ADDRESS. Built ahead of its caller: the address-follow dispatch that drives
    /// it is a marked seam in `ops::follow` (an `/i/` link is claims-only now), so no production path calls
    /// this yet — the later leg wires the `follow <address>` grammar onto it.
    ///
    /// # Errors
    /// [`ClientError::Plane`] on a transport fault / non-OK status; [`ClientError::Corrupt`] on a malformed body.
    #[allow(dead_code)]
    fn device_authorize(
        &self,
        workspace: &str,
        device_public_key: [u8; 32],
        machine_name: &str,
    ) -> Result<DeviceAuthorize, ClientError>;

    /// `POST /v1/device/authorize` with `intent = "standup"` and NO invite token — begin the workspace
    /// STANDUP device flow (hosted planes only). The response additionally carries the plane block (its API
    /// base to dial), which a standup device has no `/i/` bootstrap to learn from.
    ///
    /// # Errors
    /// [`ClientError::Plane`] on a transport fault / non-OK status (a plane that does not offer standup is
    /// a 404); [`ClientError::WireInvalid`] on a malformed body or a response missing the plane block.
    fn device_authorize_standup(
        &self,
        device_public_key: [u8; 32],
        machine_name: &str,
    ) -> Result<StandupAuthorize, ClientError>;

    /// `POST /v1/device/token` — one poll of the session. The poll STATE (pending/slow_down/denied/expired/
    /// granted) is the `Ok` value; only a transport/parse fault is an `Err`.
    ///
    /// # Errors
    /// [`ClientError::Plane`] on a transport fault; [`ClientError::Corrupt`] on a malformed body.
    fn poll_token(&self, device_code: &str) -> Result<TokenPoll, ClientError>;

    /// `POST /v1/workspaces/{ws}/devices` — redeem the grant into a registered device + its ONE workspace
    /// credential. The grant is the bearer credential; the body's `device_public_key` registers this device
    /// (the server checks it matches the grant's bound pubkey). Nothing is signed.
    ///
    /// # Errors
    /// [`ClientError::Plane`] on a transport fault or a 200+DENIED redeem (e.g. a device-key mismatch);
    /// [`ClientError::Corrupt`] on a malformed body.
    fn redeem(
        &self,
        workspace_id: &str,
        grant: &str,
        device_public_key: [u8; 32],
    ) -> Result<Redeem, ClientError>;

    /// `POST /v1/admin-claim` — consume a one-time claim token to stand up the workspace + seat this device
    /// as its first owner (the self-host bearer door). NOT device-signed on the wire; the body's public key
    /// is the identity anchor, and the server's same-device replay of a consumed claim re-answers Redeemed
    /// (lost-200 recovery), so a WAL retry POSTs this directly — never refetching the consumed `/i/` link.
    /// `display_name` is disclosure-only (the seated name comes from the mint-time claim row).
    ///
    /// # Errors
    /// [`ClientError::Enrollment`] on a 200+DENIED claim (consumed by another device / expired / the
    /// workspace already exists); [`ClientError::Plane`] on a transport fault; [`ClientError::WireInvalid`]
    /// on a malformed body.
    fn admin_claim(
        &self,
        claim_token: &str,
        device_public_key: [u8; 32],
        display_name: &str,
    ) -> Result<Redeem, ClientError>;
}

// ---------------------------------------------------------------------------------------------
// The governance-write seam — the invitation roster-write, behind a port so the `invite` tests run
// against a fake WITHOUT HTTP. The acting device rides the workspace Bearer credential (the in-txn authz
// resolves its registry row → principal → the invite-policy gate); the workspace id is a URL path segment
// (the body carries none). The base URL is baked in when the connector builds it from `instance.json`. The
// real impl is `crate::plane_http::UreqDeviceClient`; the fake lives in the invite tests.
// ---------------------------------------------------------------------------------------------

/// The governance-write transport. The `invite` op drives it: POST the invitation roster-write to
/// `POST /v1/workspaces/{ws}/invitations` under the workspace Bearer credential.
pub(crate) trait GovernanceSource {
    /// `POST /v1/workspaces/{ws}/invitations` — seat each email as an invited member (+ optional channel
    /// pre-placement). The body is the
    /// [`InvitationRequest`](topos_types::requests::InvitationRequest); the workspace id rides the URL path.
    /// Maps the all-outcome **200 envelope**: `ok` ⇒ the
    /// [`InvitationData`](topos_types::requests::InvitationData); `!ok` ⇒ a typed error carrying the wire
    /// error's code (a policy-DENIED surfaces as a clear "not authorized").
    ///
    /// # Errors
    /// [`ClientError::Plane`] on a transport fault, a non-200 status, or a 200+DENIED envelope (e.g. the
    /// workspace restricts inviting to owners); [`ClientError::Corrupt`] on a malformed body.
    fn invite(
        &self,
        workspace_id: &str,
        body: topos_types::requests::InvitationRequest,
    ) -> Result<topos_types::requests::InvitationData, ClientError>;
}

// ---------------------------------------------------------------------------------------------
// The contribute-write seam — the write side (publish / propose / revert / review), behind a port so the
// contribute tests run against a fake WITHOUT HTTP. The body names the acting `device_key_id`; the base
// URL is baked in when the connector builds it from `instance.json`. The op kind is derived from the ROUTE
// server-side. The real impl is `crate::plane_http::UreqDeviceClient` (the same client that speaks
// enrollment + governance); the fake lives in the contribute tests.
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
    /// The served `current` pointer — `Some` ONLY when a pointer actually moved (publish / revert /
    /// review-approve OK). `None` for NEEDS_REVIEW, an OK `review --reject` (moves nothing → data `{}`), and
    /// every failure. The caller scope-checks it and confirms it names the version this op moved to.
    pub wire_record: Option<WireCurrentRecord>,
}

impl WriteReceipt {
    /// The terminal outcome the verb branches on.
    pub(crate) fn outcome(&self) -> TerminalOutcome {
        self.receipt.outcome
    }
}

/// The contribute-write transport — the four POST verbs that move (or propose to move) `current`. The body
/// names the acting `device_key_id`; the op kind is derived from the route server-side, so the transport
/// ships only the body and is op-agnostic. EVERY terminal protocol outcome (OK / NEEDS_REVIEW / CONFLICT /
/// APPROVAL_REQUIRED / DENIED) is an `Ok(WriteReceipt)`; only a transport / non-200 / malformed-body fault
/// is an `Err`. The real impl is [`crate::plane_http::UreqDeviceClient`].
pub(crate) trait ContributeSource {
    /// `POST /v1/publish` — a direct publish that moves `current` (or genesis).
    ///
    /// # Errors
    /// [`ClientError::Plane`] on a transport fault / non-200 status; [`ClientError::Corrupt`] on a malformed
    /// envelope (a body that carries no receipt is corrupt).
    fn publish(&self, body: PublishRequest) -> Result<WriteReceipt, ClientError>;

    /// `POST /v1/proposals` — open a proposal (ingest a candidate WITHOUT moving `current`).
    ///
    /// # Errors
    /// As [`publish`](Self::publish).
    fn propose(&self, body: ProposeRequest) -> Result<WriteReceipt, ClientError>;

    /// `POST /v1/reverts` — a forward revert (the server builds the forward commit from `good`'s tree).
    ///
    /// # Errors
    /// As [`publish`](Self::publish).
    fn revert(&self, body: RevertRequest) -> Result<WriteReceipt, ClientError>;

    /// `POST /v1/reviews` — approve or reject a proposal (the verdict rides `ReviewRequest.decision`).
    ///
    /// # Errors
    /// As [`publish`](Self::publish).
    fn review(&self, body: ReviewRequest) -> Result<WriteReceipt, ClientError>;
}

// ---------------------------------------------------------------------------------------------
// The catalog-read seam — the WORKSPACE-CATALOG read side (`list --remote`), behind a port so the list
// tests run against a fake WITHOUT HTTP. The read is authenticated by the workspace's Bearer credential
// (catalog visibility == workspace membership, resolved from the registry row), not a per-skill token.
// Metadata only (no bytes). The real impl is `crate::plane_http::UreqDeviceClient` (the same client that
// speaks enrollment / governance / contribute, holding the per-workspace credential map); the fake lives
// in the `list` tests.
// ---------------------------------------------------------------------------------------------

/// The catalog-read transport: `GET /v1/workspaces/{ws}/skills` returns the workspace's discovery metadata
/// (every skill holding a `current`), so a member can see what to follow next. The transport presents the
/// workspace's Bearer credential (looked up by `workspace_id` in its own credential map).
pub(crate) trait CatalogSource {
    /// Read a workspace's skill catalog (metadata only). The real impl maps a **404** (not a member / no
    /// such workspace — the indistinguishable "no catalog") to an EMPTY index rather than an error, so a
    /// caller merging several workspaces degrades cleanly; a workspace with no stored credential and any
    /// other non-200 are [`PlaneError::Unavailable`] (or [`PlaneError::Unreachable`] on a connect-level
    /// fault) — each degraded to a per-workspace warning by the `list --remote` merge.
    ///
    /// # Errors
    /// [`PlaneError::Unreachable`] / [`PlaneError::Unavailable`] on a transport / non-200 / missing-credential
    /// fault; [`PlaneError::Malformed`] on a corrupt body or an unsafe workspace-id path segment.
    fn fetch_catalog(&self, workspace_id: &str) -> Result<WireSkillIndex, PlaneError>;
}

/// Compare two wire generations with the epoch-dominant order (epoch first, then seq; the wire type
/// derives no `Ord`).
pub(crate) fn gen_cmp(a: Generation, b: Generation) -> core::cmp::Ordering {
    (a.epoch, a.seq).cmp(&(b.epoch, b.seq))
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
