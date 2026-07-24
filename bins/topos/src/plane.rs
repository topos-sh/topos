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
    DeviceLinkData, DeviceLinkDescribe, ProposeRequest, PublishRequest, RevertRequest,
    ReviewRequest, WireChannelIndex, WireMe, WireNotice, WireProposalIndex, WireProtocolCard,
    WireReach, WireSkillIndex, WireSkillLog,
};
use topos_types::{Receipt, TerminalOutcome, WireCurrentRecord, WireError};

use crate::error::ClientError;

/// A device↔workspace LINK's status, as the wire spells it — the client branches only on the
/// pending/active split (an unrecognized value reads as the stricter PENDING: no data flows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkStatus {
    /// The link is live — data flows.
    Active,
    /// The link awaits an owner's approval — no data flows; the sweep stays quiet.
    Pending,
}

impl LinkStatus {
    /// Parse a wire `link_status` field. Absent (an older producer) reads as ACTIVE — such a
    /// producer serves only active-equivalent access; any present-but-unrecognized value reads as
    /// the stricter PENDING.
    pub(crate) fn from_wire(raw: Option<&str>) -> Self {
        match raw {
            None | Some("active") => LinkStatus::Active,
            _ => LinkStatus::Pending,
        }
    }
}

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
/// record that reuses the same generation for a DIFFERENT `version_id` is always returned (and applied
/// as the new target) rather than hidden behind a generation-only 304. (The HTTP ETag is therefore
/// commit-sensitive, not just `"<generation>"`.)
#[derive(Clone, Copy)]
pub(crate) struct KnownCurrent {
    pub generation: u64,
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

    /// Bind a skill to its workspace credential on THIS read transport — the read-side twin of
    /// [`DeliverySource::bind_skill`]. The per-skill credential map is derived from `follows.json`, so a
    /// skill this device has never followed (a genesis publisher's own skill, pre-`update`; a catalog-only
    /// review target) is absent from it — and every read (`get_current` / `fetch_version`) would answer
    /// the indistinguishable "not served" until it is bound. The workspace credential already authenticates
    /// every skill in its workspace (membership IS the authorization), so binding is a lookup, never a new
    /// secret. Default: a no-op (the inert source and the test fakes carry their creds up front).
    fn bind_skill(&self, _workspace_id: &str, _skill_id: &str) {}
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
    /// The DEVICE-LOCAL agent include-list (`follow --agent`): registry slugs this skill's bytes are
    /// scoped to. Empty = unscoped (every detected agent). Never told to the plane.
    pub agents: Vec<String>,
    /// The DEVICE-LOCAL per-agent exclusions (`unfollow --agent` / `remove --agent`) — agents whose
    /// placement this device cleans and stops maintaining. Never told to the plane.
    pub excluded_agents: Vec<String>,
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
    pub generation: u64,
    /// The `current` byte-exact consent hash — what a follow describe DISCLOSES per install (the
    /// engine still re-derives it from the fetched bytes; this copy is disclosure, not trust).
    pub bundle_digest: [u8; 32],
    /// The channels delivering the skill (the `via` attribution a describe narrates).
    pub via_channels: Vec<String>,
    /// Whether the person also follows the skill directly.
    pub via_direct: bool,
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
    /// The unacked, person-scoped notices feed (verdicts, proposal closures, …). An interactive
    /// `update` narrates then ACKS them; the quiet hook fetches without acking.
    pub notices: Vec<WireNotice>,
    /// The workspace's staleness window (ms) — the ONE clock the fleet page and the client's hook
    /// warning both read: a device whose last delivery is older than this is stale.
    pub staleness_window_ms: u64,
    /// THIS device's link to the workspace. PENDING carries empty sets and a zero gauge (no data
    /// flows over a pending link) — the reconcile skips the workspace quietly.
    pub link_status: LinkStatus,
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

    /// `POST /v1/workspaces/{ws}/notices/ack` — acknowledge notices by id (person-scoped
    /// read-state). The interactive `update` acks exactly what it narrated; the quiet hook NEVER
    /// calls this (fetch-without-ack). Best-effort at the call site (a failed ack warns, never
    /// blocks a sync). Default: a clean no-op, so fixtures that never exercise notices need no impl.
    fn ack_notices(&self, _workspace_id: &str, _ids: &[String]) -> Result<(), PlaneError> {
        Ok(())
    }
}

/// The reconcile-capable transport: the DELIVERY lane and the per-skill READ lane on ONE object.
/// The pairing is load-bearing — the reconcile teaches the read side a brand-new arrival's
/// credential (`bind_skill`), so the two lanes must share state; the production impl is one
/// `UreqPlane`. Callers upcast to either supertrait (`&dyn PlaneSource` for the engine ctx,
/// `&dyn DeliverySource` for the reconcile).
pub(crate) trait ReconcileTransport: PlaneSource + DeliverySource {}
impl<T: PlaneSource + DeliverySource> ReconcileTransport for T {}

// ---------------------------------------------------------------------------------------------
// The directory seam — the member-scoped describe reads plus the person/device ROW OPS the
// two-phase verbs run (subscription / curation / protection / notices), all under the workspace
// Bearer credential. Behind a port so the verbs unit-test over a fake with no HTTP; the production
// impl is `crate::plane_http::UreqDeviceClient`. Every row-op response is the standard all-outcome
// **200 envelope**, parsed LENIENTLY: `ok: true` is success (whatever `data` carries), `ok: false`
// surfaces as the typed [`ClientError::PlaneTerminal`] carrying the wire error's `code`/`outcome`
// verbatim; a pre-gate miss is the uniform 404 → [`ClientError::TargetNotFound`].
// ---------------------------------------------------------------------------------------------

/// The member-scoped directory transport: the describe reads + the subscription / curation /
/// protection / notice row ops. One method per route; the workspace id keys the credential lookup
/// AND rides the URL path (the body carries no credential material, ever).
pub(crate) trait DirectorySource {
    /// `GET /v1/workspaces/{ws}/me` — the caller's own membership describe.
    ///
    /// # Errors
    /// [`ClientError::TargetNotFound`] on the uniform 404; [`ClientError::Plane`] on a transport
    /// fault; [`ClientError::WireInvalid`] on a malformed body.
    fn me(&self, workspace_id: &str) -> Result<WireMe, ClientError>;

    /// `POST /v1/invitations/accept` — the ALREADY-ENROLLED device consuming an invite URL: the
    /// credential authenticates the person (no seat exists yet, so this is the one PERSON-scoped
    /// call on this transport — no workspace id anywhere). The OK answer carries the joined
    /// workspace + the optional first-destination hint; the invitation's own fences may refuse
    /// typed (wrong account / unverified mailbox), and a dead token is the uniform 404. Defaulted
    /// so read-only fakes need no arm.
    ///
    /// # Errors
    /// [`ClientError::TargetNotFound`] on a dead token; [`ClientError::PlaneTerminal`] on a fence
    /// refusal; [`ClientError::Plane`]/[`ClientError::WireInvalid`] on transport/parse faults.
    fn accept_invitation(&self, token: &str) -> Result<InviteAccepted, ClientError> {
        let _ = token;
        Err(ClientError::Plane(
            "this transport serves no invitation accept".into(),
        ))
    }

    /// `GET /v1/device/link?workspace=<address-slug>` — the browser-free link DESCRIBE (nothing
    /// mutates): where THIS device stands with the named workspace and what a link would be born
    /// as. PERSON-scoped — the seat is checked server-side, no link is required, so an enrolled
    /// device reaches a workspace it has never linked. An empty slug names the origin's own
    /// workspace. Defaulted so read-only fakes need no arm.
    ///
    /// # Errors
    /// [`ClientError::PlaneTerminal`] with code `NOT_A_MEMBER` for a seatless caller OR an unknown
    /// workspace name (byte-identical — no existence oracle); [`ClientError::TargetNotFound`] on
    /// the uniform 404 (a dead credential); transport/parse faults otherwise.
    fn describe_link(&self, workspace_slug: &str) -> Result<DeviceLinkDescribe, ClientError> {
        let _ = workspace_slug;
        Err(ClientError::Plane(
            "this transport serves no device link".into(),
        ))
    }

    /// `POST /v1/device/link` — link THIS device to a workspace the person's seats reach (born
    /// `active`, or `pending` under the workspace's device-approval knob). Idempotent: an existing
    /// link answers ok with its current status. Defaulted so read-only fakes need no arm.
    ///
    /// # Errors
    /// As [`describe_link`](Self::describe_link).
    fn create_link(&self, workspace_slug: &str) -> Result<DeviceLinkData, ClientError> {
        let _ = workspace_slug;
        Err(ClientError::Plane(
            "this transport serves no device link".into(),
        ))
    }

    /// `GET /v1/workspaces/{ws}/channels` — the channel index with the caller's membership marked.
    ///
    /// # Errors
    /// As [`me`](Self::me).
    fn channels_index(&self, workspace_id: &str) -> Result<WireChannelIndex, ClientError>;

    /// `GET /v1/workspaces/{ws}/skills` — the workspace catalog (name → skill id + digests), the
    /// resolver's and the describe's name source. The same route `list --remote` reads through
    /// [`CatalogSource`]; this typed twin keeps the verb paths on ONE connector.
    ///
    /// # Errors
    /// As [`me`](Self::me).
    fn skills_index(&self, workspace_id: &str) -> Result<WireSkillIndex, ClientError>;

    /// `GET /v1/workspaces/{ws}/proposals` — the workspace review inbox (author-message first).
    ///
    /// # Errors
    /// As [`me`](Self::me).
    // Consumed by the review-inbox / log / reach describes — built alongside the subscribe
    // methods so every verb shares ONE connector.
    #[allow(dead_code)]
    fn proposals_index(&self, workspace_id: &str) -> Result<WireProposalIndex, ClientError>;

    /// `GET /v1/workspaces/{ws}/skills/{skill}/log` — the skill's history (purge tombstones + the
    /// archived-successor hint included).
    ///
    /// # Errors
    /// As [`me`](Self::me).
    // Consumed by the review-inbox / log / reach describes — built alongside the subscribe
    // methods so every verb shares ONE connector.
    #[allow(dead_code)]
    fn skill_log(&self, workspace_id: &str, skill_id: &str) -> Result<WireSkillLog, ClientError>;

    /// `GET /v1/workspaces/{ws}/skills/{skill}/reach` — the audience a change reaches.
    ///
    /// # Errors
    /// As [`me`](Self::me).
    // Consumed by the review-inbox / log / reach describes — built alongside the subscribe
    // methods so every verb shares ONE connector.
    #[allow(dead_code)]
    fn reach(&self, workspace_id: &str, skill_id: &str) -> Result<WireReach, ClientError>;

    /// `PUT /v1/workspaces/{ws}/follows/{skill}` — the person-scoped direct follow row (also lifts
    /// a standing unfollow of the same skill, server-side).
    ///
    /// # Errors
    /// [`ClientError::PlaneTerminal`] on an `ok: false` refusal; [`ClientError::TargetNotFound`] on
    /// the uniform 404; [`ClientError::Plane`] / [`ClientError::PlaneRejected`] on transport faults.
    fn follow_skill(&self, workspace_id: &str, skill_id: &str) -> Result<(), ClientError>;

    /// `DELETE /v1/workspaces/{ws}/follows/{skill}` — the person-scoped unfollow (delivery stops on
    /// every device; bytes freeze in place).
    ///
    /// # Errors
    /// As [`follow_skill`](Self::follow_skill).
    fn unfollow_skill(&self, workspace_id: &str, skill_id: &str) -> Result<(), ClientError>;

    /// `PUT /v1/workspaces/{ws}/channels/{ch}/membership` — join a channel (person-scoped).
    ///
    /// # Errors
    /// As [`follow_skill`](Self::follow_skill) (the structural `everyone` refuses typed).
    fn channel_join(&self, workspace_id: &str, channel: &str) -> Result<(), ClientError>;

    /// `DELETE /v1/workspaces/{ws}/channels/{ch}/membership` — leave a channel (person-scoped).
    ///
    /// # Errors
    /// As [`channel_join`](Self::channel_join).
    fn channel_leave(&self, workspace_id: &str, channel: &str) -> Result<(), ClientError>;

    /// `PUT /v1/workspaces/{ws}/channels/{ch}/skills/{skill}` — place a skill's reference into a
    /// channel (created on first placement; a curated channel gates by role).
    ///
    /// # Errors
    /// As [`follow_skill`](Self::follow_skill).
    #[allow(dead_code)]
    fn channel_place(
        &self,
        workspace_id: &str,
        channel: &str,
        skill_id: &str,
    ) -> Result<(), ClientError>;

    /// `DELETE /v1/workspaces/{ws}/channels/{ch}/skills/{skill}` — remove a skill's reference.
    ///
    /// # Errors
    /// As [`follow_skill`](Self::follow_skill).
    #[allow(dead_code)]
    fn channel_unplace(
        &self,
        workspace_id: &str,
        channel: &str,
        skill_id: &str,
    ) -> Result<(), ClientError>;

    /// `PUT /v1/workspaces/{ws}/exclusions/{skill}` — the per-DEVICE "not on this device" row (the
    /// `remove` verb's server half; the person keeps receiving the skill everywhere else).
    ///
    /// # Errors
    /// As [`follow_skill`](Self::follow_skill).
    #[allow(dead_code)]
    fn exclude_device(&self, workspace_id: &str, skill_id: &str) -> Result<(), ClientError>;

    /// `PUT /v1/workspaces/{ws}/skills/{skill}/protection` — set a bundle's protection level
    /// (`reviewed` / `open`; tightening takes reviewer+, loosening an owner — the server decides).
    ///
    /// # Errors
    /// As [`follow_skill`](Self::follow_skill).
    #[allow(dead_code)]
    fn protect_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
        level: &str,
    ) -> Result<(), ClientError>;

    /// `PUT /v1/workspaces/{ws}/channels/{ch}/protection` — set a channel's mode (`curated` / `open`).
    ///
    /// # Errors
    /// As [`follow_skill`](Self::follow_skill).
    #[allow(dead_code)]
    fn protect_channel(
        &self,
        workspace_id: &str,
        channel: &str,
        level: &str,
    ) -> Result<(), ClientError>;

    /// `POST /v1/workspaces/{ws}/notices/ack` — mark the caller's own notices read, by id (the
    /// verb-lane twin of [`DeliverySource::ack_notices`] — same route, the verbs' connector).
    ///
    /// # Errors
    /// As [`follow_skill`](Self::follow_skill).
    #[allow(dead_code)]
    fn ack_notices(&self, workspace_id: &str, ids: &[String]) -> Result<(), ClientError>;
}

// ---------------------------------------------------------------------------------------------
// The enrollment seam — the gh-style device-flow client, behind a port so the `follow`/`auth login`
// tests run against a fake WITHOUT HTTP. The device code (and, once granted, the device credential)
// are the only secrets it carries; both are redacted from every `Debug`. The real impl is
// `crate::plane_http::UreqDeviceClient`; the fakes live in the in-crate tests.
// ---------------------------------------------------------------------------------------------

/// The device-authorization grant from `POST /v1/device/authorize` (RFC-8628-shaped names).
#[derive(Clone)]
pub(crate) struct DeviceAuthStart {
    /// **SECRET** — the device code the client polls with (promoted server-side to the device's ONE
    /// bearer credential on approval). Redacted in `Debug`, never logged / in a URL.
    pub device_code: String,
    /// The short human-facing code the approval page asks for and displays back (the glance-check).
    pub user_code: String,
    /// The BARE approval URL — printed beside the code on its own line; the code never rides a URL.
    pub verification_uri: String,
    /// The flow lifetime, in seconds.
    pub expires_in_secs: u64,
    /// The minimum poll interval, in seconds.
    pub interval_secs: u64,
}

impl std::fmt::Debug for DeviceAuthStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceAuthStart")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("expires_in_secs", &self.expires_in_secs)
            .field("interval_secs", &self.interval_secs)
            .finish()
    }
}

/// The workspace context a granted poll carries — everything the client records about what it joined.
#[derive(Debug, Clone)]
pub(crate) struct EnrolledWorkspace {
    /// The workspace id (the `{ws}` path segment of every subsequent request).
    pub workspace_id: String,
    /// The workspace's ADDRESS slug (what the human typed at `follow`).
    pub name: String,
    /// The workspace's display name.
    pub display_name: String,
}

/// The first-destination hint an accepted invitation named — the post-enrollment subscribe's
/// target (`kind` is the catalog's tag — `skill` today — or the literal `channel`).
#[derive(Debug, Clone)]
pub(crate) struct GrantHint {
    pub kind: String,
    pub name: String,
}

/// The direct-accept answer (`POST /v1/invitations/accept`): the joined workspace + the optional
/// first-destination hint. The already-held device credential now reaches the workspace — no new
/// secret arrives. The accept also LINKED the accepting device (born per the workspace's
/// device-approval knob); a PENDING link answers the typed waiting receipt instead of a subscribe.
#[derive(Debug, Clone)]
pub(crate) struct InviteAccepted {
    pub workspace: EnrolledWorkspace,
    pub hint: Option<GrantHint>,
    pub link_status: LinkStatus,
}

/// A GRANTED device-authorization poll: the device's ONE bearer credential (the promoted device code),
/// the registered device's id, and the joined workspace. Hand-written `Debug` redacts the credential.
#[derive(Clone)]
pub(crate) struct EnrolledGrant {
    /// **SECRET** — the device's plaintext bearer credential (returned by the poll; stored `0600`).
    pub credential: String,
    /// The registered device's id (the non-secret handle a self-revoke names). On the SESSION
    /// wire this carries the minted session id when the producer sent no device id.
    pub device_id: String,
    /// The minted SESSION's id (the session-model wire; `None` from an older producer).
    pub session_id: Option<String>,
    /// The joined workspace.
    pub workspace: EnrolledWorkspace,
    /// The invitation's first-destination hint — present when the flow carried an invite token
    /// whose (now accepted) invitation named one.
    pub hint: Option<GrantHint>,
    /// The FIRST device↔workspace link's born status (approval mints registration + link
    /// together server-side). PENDING ⇒ persist the enrollment, then the waiting receipt — no
    /// subscribe attempt.
    pub link_status: LinkStatus,
}

impl std::fmt::Debug for EnrolledGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrolledGrant")
            .field("credential", &"<redacted>")
            .field("device_id", &self.device_id)
            .field("session_id", &self.session_id)
            .field("workspace", &self.workspace)
            .field("hint", &self.hint)
            .field("link_status", &self.link_status)
            .finish()
    }
}

/// The outcome of a `POST /v1/device/token` poll. NOT an error — every variant is a legitimate poll
/// state; only a transport/parse fault is an `Err`. A re-poll of an approved flow returns the same
/// granted answer, so a crash between the grant and the sidecar writes recovers by re-polling.
#[derive(Debug, Clone)]
pub(crate) enum DeviceAuthPoll {
    /// Not yet approved — keep polling at the interval (re-invoking the verb re-polls on demand).
    Pending,
    /// Denied at the approval page.
    Denied,
    /// The flow expired before approval.
    Expired,
    /// Approved — the credential, device id, and workspace are present.
    Granted(EnrolledGrant),
}

/// The enrollment transport (the gh-style device flow the app serves). `follow <address>` and
/// `auth login` drive it: read the constant protocol card, start a device authorization toward a
/// workspace ADDRESS, and poll for the outcome — the granted poll carries the device's ONE bearer
/// credential (no separate redeem round-trip exists). The real impl is
/// [`crate::plane_http::UreqDeviceClient`]; the fakes live in the in-crate tests.
pub(crate) trait EnrollSource {
    /// `GET <url>` with `Accept: application/json` — the unauthenticated CARD read of any resource
    /// address: the constant protocol card (re-root onto its `api_base_url`). Identical at every
    /// path — no content, no existence signal.
    ///
    /// # Errors
    /// [`ClientError::Plane`] on a transport fault / non-2xx; [`ClientError::WireInvalid`] on a body
    /// that is not a protocol card.
    fn fetch_card(&self, url: &str) -> Result<WireProtocolCard, ClientError>;

    /// `POST /v1/device/authorize` — begin a device-authorization toward the workspace named by its
    /// ADDRESS slug (whether the name exists is never disclosed here — an unknown name runs the same
    /// flow to the same uniform denial). `requested_name` is the human-readable device name shown on
    /// the approval page (a confused-deputy aid, not authority). `invite_token` is the invitation
    /// link's token when this enrollment came from `follow <invite-url>` — recorded on the flow so
    /// the approval weaves the accept in; never validated at this unauthenticated start.
    ///
    /// # Errors
    /// [`ClientError::Plane`] on a transport fault / non-OK status; [`ClientError::WireInvalid`] on a
    /// malformed body.
    fn device_auth_start(
        &self,
        workspace: &str,
        requested_name: &str,
        invite_token: Option<&str>,
    ) -> Result<DeviceAuthStart, ClientError>;

    /// `POST /v1/device/token` — one poll of the flow. The poll STATE (pending / denied / expired /
    /// granted) is the `Ok` value; only a transport/parse fault is an `Err`.
    ///
    /// # Errors
    /// [`ClientError::Plane`] on a transport fault; [`ClientError::WireInvalid`] on a malformed body
    /// (including a `granted` poll missing its credential / device / workspace).
    fn device_auth_poll(&self, device_code: &str) -> Result<DeviceAuthPoll, ClientError>;
}

// ---------------------------------------------------------------------------------------------
// The governance-write seam — the invitation roster-write, behind a port so the `invite` tests run
// against a fake WITHOUT HTTP. The acting device rides the workspace Bearer credential (the in-txn authz
// resolves its registry row → principal → the owner gate); the workspace id is a URL path segment
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

    /// `DELETE /v1/device` — the GLOBAL self-revoke (`auth logout`'s one call, no body): revokes
    /// THIS credential's device server-side — the links and per-workspace reported state are
    /// deleted there. After revocation a retry answers the uniform 404 (the caller treats that as
    /// already-signed-out). Default: an erroring body, so fakes that never exercise governance
    /// need no impl.
    ///
    /// # Errors
    /// [`ClientError::PlaneTerminal`] on an `ok: false` refusal; [`ClientError::TargetNotFound`] on
    /// the uniform 404 (already revoked); [`ClientError::Plane`] on a transport fault.
    fn revoke_device(&self) -> Result<(), ClientError> {
        Err(ClientError::Plane(
            "this transport serves no device revoke".into(),
        ))
    }

    /// `DELETE /v1/session` — end the SESSION this transport's own credential names (`logout`'s
    /// one call per workspace, no body: the credential IS the session, so nothing client-asserted
    /// can reach another pocket). After the delete the credential no longer resolves, so a retry
    /// answers the uniform 404 — already signed out. Default: an erroring body, so fakes that
    /// never exercise sessions need no impl.
    ///
    /// # Errors
    /// [`ClientError::PlaneTerminal`] on an `ok: false` refusal; [`ClientError::TargetNotFound`]
    /// on the uniform 404 (already ended); [`ClientError::Plane`] on a transport fault.
    fn revoke_session(&self) -> Result<(), ClientError> {
        Err(ClientError::Plane(
            "this transport serves no session revoke".into(),
        ))
    }
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
    /// The canonical all-outcome receipt, present whenever the plane attached one — `outcome` is the branch
    /// discriminant; `version_id` + `bundle_digest` + `current_generation` build the verb's `--json` data.
    /// `None` ONLY on a receipt-LESS DENIED envelope (an old server that never attached a receipt, or an
    /// already-stored wedged receipt a same-`op_id` replay re-serves): there the flat `error` is the
    /// terminal answer and [`outcome`](Self::outcome) reads `error.outcome`. Every `ok:true` outcome carries
    /// `Some` (`map_write_envelope` keeps a receipt-less success as `WireInvalid`), so the OK verb paths may
    /// require it. A receipt-less DENIED is still an `Ok(WriteReceipt)`, so `run_write` SETTLES (deletes) the
    /// op-WAL on it — the typed way out for an install wedged behind a stored receipt-less envelope.
    pub receipt: Option<Receipt>,
    /// The flat wire error on a non-OK outcome (CONFLICT / APPROVAL_REQUIRED / DENIED) — the fine `code`,
    /// the live `current_generation` (the CONFLICT rebase target), and the typed `next_actions`. `None` on
    /// OK / NEEDS_REVIEW; the sole carrier of the outcome on a receipt-less DENIED.
    pub error: Option<WireError>,
    /// The served `current` pointer — `Some` ONLY when a pointer actually moved (publish / revert /
    /// review-approve OK). `None` for NEEDS_REVIEW, an OK `review --reject` (moves nothing → data `{}`), and
    /// every failure. The caller scope-checks it and confirms it names the version this op moved to.
    pub wire_record: Option<WireCurrentRecord>,
}

impl WriteReceipt {
    /// The terminal outcome the verb branches on — the receipt's `outcome` when present, else the flat
    /// wire error's (the receipt-less DENIED case). A body carrying neither is impossible past
    /// `map_write_envelope` (a receipt-less non-denial is `WireInvalid`); the permanent-failure default is
    /// a defensive floor, never reached in practice.
    pub(crate) fn outcome(&self) -> TerminalOutcome {
        match &self.receipt {
            Some(r) => r.outcome,
            None => self
                .error
                .as_ref()
                .map_or(TerminalOutcome::PermanentFailure, |e| e.outcome),
        }
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
            "not enrolled with a plane; run `topos follow <workspace-address>` first".into(),
        ))
    }
    fn fetch_version(
        &self,
        _skill_id: &str,
        _version_id: [u8; 32],
    ) -> Result<FetchedVersion, PlaneError> {
        Err(PlaneError::Unavailable(
            "not enrolled with a plane; run `topos follow <workspace-address>` first".into(),
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
