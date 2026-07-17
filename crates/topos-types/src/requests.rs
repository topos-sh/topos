//! Wire request/response DTOs for the product's public device-lane HTTP routes.
//!
//! The JSON bodies the app accepts on `publish` / `propose` / `revert` / `review`, the read bodies
//! (current / version metadata / proposals / catalog / delivery / describe), and the device-auth
//! start/poll pair. These are **deserialization shapes** only (no logic): the serving tier parses
//! them into validated domain types at the edge (parse-don't-validate), and every candidate byte is
//! **server-rehashed** — a client-supplied id or hash is never trusted.
//!
//! **No `created_at` on any request.** The server stamps the receipt's time from its own clock; a
//! client never supplies a wall clock (an ambient time would be a replay / skew lever).
//!
//! **The write credential is the device credential in the `Authorization: Bearer` header — never a
//! body field.** Keeping the secret out of the body keeps it out of receipt request identities and
//! the client's persisted op-WAL, so a credential rotation between retries never breaks
//! byte-identical replay. The `op` (publish / propose / revert / review-decision) is derived from
//! the route, never the body.
//!
//! Field names are snake_case as written (no `rename_all`). Hex id fields carry the same `^[0-9a-f]{64}$`
//! constraint used across [`crate`].

use crate::results::ReviewDecision;
use serde::{Deserialize, Serialize};

/// A candidate file's mode on the wire — the two git regular-file modes as their literal octal strings
/// (`"100644"` / `"100755"`). A **closed** wire mirror of `topos_core::digest::FileMode`: that kernel enum
/// lives in a `no_std` crate `topos-types` does not depend on (and it carries no serde/schema derives), so
/// the wire leaf owns its own copy. The route handler maps it 1:1 at the edge —
/// `Regular ⇔ FileMode::Regular`, `Executable ⇔ FileMode::Executable` — for both the inbound candidate
/// ([`WireFile`]) and the outbound version metadata ([`WireVersionFile`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub enum WireFileMode {
    /// `100644` — a regular, non-executable file.
    #[serde(rename = "100644")]
    Regular,
    /// `100755` — a regular, executable file.
    #[serde(rename = "100755")]
    Executable,
}

/// One file of a candidate bundle, uploaded **by value**. The raw bytes ride as base64 in the JSON body
/// (`content_base64`, standard alphabet); the server base64-decodes them and **rehashes every byte** to
/// derive the content id — there is no reference-by-id, and a client hash is never trusted. Maps to
/// `plane-store`'s `UploadedFile { path, mode, bytes }` (decode `content_base64` → `bytes`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireFile {
    /// The bundle-relative, forward-slash path.
    pub path: String,
    /// The file mode (regular or executable).
    pub mode: WireFileMode,
    /// The raw file bytes, base64-encoded. The server decodes then rehashes them (server-side digest).
    pub content_base64: String,
}

/// A full candidate bundle: every file's bytes, the declared parents, and the author + message — the
/// shared input the `publish` and `propose` writes ingest (the `revert` write needs no candidate; the
/// server builds that forward commit from the good version). Maps to `plane-store`'s
/// `CandidateUpload { files, parents, author, message }` — each `parents` entry hex-decoded into a
/// `CommitId`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireCandidate {
    /// Every file in the candidate bundle (each server-rehashed).
    pub files: Vec<WireFile>,
    /// The candidate commit's declared parents, each a 64-char lowercase-hex `version_id` (`0` parents for a
    /// genesis publish, `1` for a normal publish / propose, `2` for an author merge). Each must already be
    /// present in the workspace; a lie changes the recomputed commit id, so the server need not trust it.
    pub parents: Vec<String>,
    /// The author device id recorded in the commit frame.
    pub author: String,
    /// The commit message (title + body composed into one string).
    pub message: String,
}

/// `POST /v1/publish` body — a direct publish that moves `current`. The acting device rides the
/// `Authorization: Bearer` workspace credential (resolved in-transaction to its registry row + the
/// membership gate); the server stamps `created_at`.
/// Under `review-required` the authority refuses this closed with `APPROVAL_REQUIRED`, ingesting nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct PublishRequest {
    /// The target workspace id (the receipt + pointer scope).
    pub workspace_id: String,
    /// The target skill id within the workspace.
    pub skill_id: String,
    /// The client-minted UUIDv4 idempotency key — the same `op_id` replays the stored receipt byte-for-byte.
    #[cfg_attr(feature = "contract-derives", schemars(extend("format" = "uuid")))]
    pub op_id: String,
    /// The generation this publish's compare-and-set targets; a stale value yields `CONFLICT`.
    pub expected: u64,
    /// The full candidate bundle to ingest + publish.
    pub candidate: WireCandidate,
    /// The skill's human display name (the author's skill-folder name) — ADVISORY metadata the plane
    /// stores last-writer-wins and serves for display only (a follower names its folder by it; the
    /// dashboard shows it). It is NOT part of the byte-exact bundle digest, the candidate, or the commit
    /// id — a rename never changes a version id or digest. Absent ⇒ the plane keeps any existing name
    /// (never clobbered to NULL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The `--to` channel placement: place the skill's reference into this channel (created on first
    /// use, member-level; a `curated` channel needs reviewer+ — the placement outcome rides the
    /// receipt's details, independently of the version gate). Absent ⇒ no explicit placement (a
    /// brand-new skill still lands in `everyone`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

/// `POST /v1/proposals` body — opens a proposal (a PR): ingests a full candidate **without moving
/// `current`** (`NEEDS_REVIEW`). The authority's `propose` op takes the **same** input shape as
/// `publish` (candidate + device + `op_id` + `expected`); there is **no** separate title/body on the op (a
/// title/body, if ever surfaced, would be composed into the candidate's commit message).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct ProposeRequest {
    /// The target workspace id.
    pub workspace_id: String,
    /// The target skill id within the workspace.
    pub skill_id: String,
    /// The client-minted UUIDv4 idempotency key (replays the stored receipt on retry).
    #[cfg_attr(feature = "contract-derives", schemars(extend("format" = "uuid")))]
    pub op_id: String,
    /// The generation the proposal is born against (its base); a stale base later makes it non-current.
    pub expected: u64,
    /// The full candidate bundle to ingest as the proposal's content.
    pub candidate: WireCandidate,
    /// The skill's human display name (the author's skill-folder name) — ADVISORY metadata, carried for
    /// symmetry with [`PublishRequest`]. It rides the proposal but is never digested or part of the
    /// candidate; the plane records a name only when the pointer actually moves (a later approve/publish).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The `--to` channel placement (same semantics as `PublishRequest.channel`): the placement
    /// applies when the proposal opens — a placement is curation, gated by the channel's mode,
    /// independent of the version's review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

/// `POST /v1/reverts` body — a **forward** revert: the server constructs a new 1-parent commit carrying the
/// `good` version's bytes on top of `current` (`seq` advances; the pointer never moves backward). There is
/// **no candidate** — the server reads `good`'s tree + digest from its provenance and builds the commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct RevertRequest {
    /// The target workspace id.
    pub workspace_id: String,
    /// The target skill id within the workspace.
    pub skill_id: String,
    /// The client-minted UUIDv4 idempotency key (replays the stored receipt on retry).
    #[cfg_attr(feature = "contract-derives", schemars(extend("format" = "uuid")))]
    pub op_id: String,
    /// The generation this revert's compare-and-set targets; a stale value yields `CONFLICT`.
    pub expected: u64,
    /// The GOOD version (the `version_id` whose bytes are restored) as 64-char lowercase hex.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub good: String,
    /// The author device id recorded in the forward-revert commit frame.
    pub author: String,
    /// The forward-revert commit message.
    pub message: String,
}

/// `POST /v1/reviews` body — a governance decision on an open proposal. `approve` runs the shared
/// generation compare-and-set on the proposal's base (a stale base ⇒ `CONFLICT`) and, under
/// `review_required`, enforces four-eyes (the proposer may not self-approve) before promoting; `reject`
/// is a standalone status flip (no pointer move).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct ReviewRequest {
    /// The target workspace id.
    pub workspace_id: String,
    /// The target skill id within the workspace.
    pub skill_id: String,
    /// The client-minted UUIDv4 idempotency key (replays the stored receipt on retry).
    #[cfg_attr(feature = "contract-derives", schemars(extend("format" = "uuid")))]
    pub op_id: String,
    /// The generation the approval's compare-and-set targets (the proposal's base); a stale value on
    /// an `approve` yields `CONFLICT`.
    pub expected: u64,
    /// The proposal being reviewed, named by its candidate commit id (64-char lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub proposal: String,
    /// The verdict — `approve` (promote), `reject` (with its reason), or `withdraw` (the author
    /// retracting their own open proposal).
    pub decision: ReviewDecision,
    /// The reject's reason, carried back to the author (REQUIRED on `reject`; absent otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One file of a version's metadata on the wire — its path, mode, and content id (`object_id`), mirroring
/// `plane-store`'s `VersionFile` with the id hex-encoded. The **bytes are NOT here**: a client fetches each
/// by `object_id` through the bundle (object) read route.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireVersionFile {
    /// The bundle-relative, forward-slash path.
    pub path: String,
    /// The file mode (regular or executable).
    pub mode: WireFileMode,
    /// The file's content id (64-char lowercase hex) — the handle the per-blob read route resolves.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub object_id: String,
}

/// `GET /v1/workspaces/{ws}/skills/{skill}/versions/{version_id}` response body — a version's authenticated
/// metadata: its id, the COMPLETE parent set, display author + message, the consent `bundle_digest`, and the
/// per-file `(path, mode, object_id)` leaves. Mirrors `plane-store`'s `VersionMeta` with every 32-byte id
/// hex-encoded. Assembled WITHOUT reading any blob bytes; the `bundle_digest` is the pin the client's
/// per-blob fetches + its own re-hash must reproduce.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireVersionMeta {
    /// This version's commit id (64-char lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    /// The COMPLETE parent set, each a 64-char lowercase-hex commit id (`0` for genesis, `1` normally, `2`
    /// for an author merge).
    pub parents: Vec<String>,
    /// The author device id from the commit frame.
    pub author: String,
    /// The commit message (title + body as one string).
    pub message: String,
    /// The byte-exact consent hash over the bundle (64-char lowercase hex) — the fetch + re-hash pin.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// The per-file leaves, in the version's recorded order.
    pub files: Vec<WireVersionFile>,
}

/// One OPEN proposal on the wire — its candidate `version_id` (the `@hash`), the `base_generation` it was
/// opened against, and when. The proposals-listing read returns ONLY these three fields: **no bytes, no
/// proposer, no roles, no rendered diff**. Mirrors `plane-store`'s `OpenProposalSummary` with the id hex-encoded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireOpenProposal {
    /// The proposal's candidate commit id (64-char lowercase hex) — the `<skill>@<version_id>` handle.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    /// The generation the proposal was opened against (its base); when `current` advances past it
    /// the proposal stales and drops out of this list.
    pub base_generation: u64,
    /// When the proposal was opened (the server-stamped RFC-3339 string).
    pub created_at: String,
}

/// `GET /v1/workspaces/{ws}/skills/{skill}/proposals` response body — the OPEN, non-stale proposals on a
/// rostered skill (a possibly-empty list, ordered by `(created_at, version_id)`). A staled proposal is absent
/// (keep == read == list); the list carries no bytes and no proposer — it is the thin handle a client turns
/// into a `diff` / `review` follow-up.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireProposalList {
    /// The open proposals (possibly empty).
    pub proposals: Vec<WireOpenProposal>,
}

/// One skill of the workspace catalog, as `GET /v1/workspaces/{ws}/skills` returns it: the discovery
/// metadata a member browses to decide what to follow — **NO bytes**. Mirrors `plane-store`'s
/// `SkillIndexRow` with every 32-byte id hex-encoded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireSkillIndexEntry {
    /// The skill id (the `<skill>` path segment).
    pub skill_id: String,
    /// The catalog's user-facing name (a pre-catalog seeded pointer falls back to the skill id).
    pub name: String,
    /// The catalog's bundle kind — `"skill"` for everything that exists today. Display metadata
    /// only: clients render it and never branch on it (an OPEN vocabulary, like `status`).
    /// Additive: an older producer that omits it is serving skills.
    #[serde(default = "default_bundle_kind")]
    pub kind: String,
    /// The catalog lifecycle status — `"active"` / `"archived"` (a deleted skill has no `current` row and
    /// so no entry). An OPEN string, deliberately: a new state can land without a schema break.
    pub status: String,
    /// The `current` version's commit id (64-char lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    /// The `current` byte-exact consent hash (64-char lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// The `current` pointer's generation.
    pub generation: u64,
    /// The unsigned, advisory folder display name (may be absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// When `current` last moved (epoch milliseconds).
    pub updated_at: i64,
    /// The count of OPEN, non-stale proposals on the skill.
    pub open_proposals: u64,
}

/// `GET /v1/workspaces/{ws}/skills` response body — the workspace catalog (every skill holding a `current`),
/// authorized by workspace membership (the `Authorization: Bearer` workspace credential names the
/// device; catalog
/// visibility == membership, on both cloud and self-host). Metadata only, no bytes; a possibly-empty list
/// ordered by `skill_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireSkillIndex {
    /// The workspace's skills (possibly empty).
    pub skills: Vec<WireSkillIndexEntry>,
}

/// Why THIS device is entitled to a delivered skill — the attribution the client's narration reads. A
/// delivered skill always has at least one of the two: it rides one or more channels, and/or the person
/// follows it directly (a direct follow survives every channel drop).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireVia {
    /// The channels delivering the skill (names, sorted; `everyone` is present when it delivers).
    pub channels: Vec<String>,
    /// Whether the person also follows the skill directly (independent of any channel).
    pub direct: bool,
}

/// One skill THIS device should have, in the delivery answer: the catalog identity, the pinned `current`
/// version + its consent digest, the resolved protection posture, and the `via` attribution. **NO bytes** —
/// after reconciling the client fetches each version through the per-blob bundle read.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireDeliverySkill {
    /// The skill id (the `<skill>` path segment).
    pub skill_id: String,
    /// The catalog's user-facing name (the on-disk directory name for a fresh install).
    pub name: String,
    /// The catalog's bundle kind — `"skill"` for everything that exists today. Display metadata
    /// only: clients render it and never branch on it (an OPEN vocabulary, like `protection`).
    /// Additive: an older producer that omits it is serving skills.
    #[serde(default = "default_bundle_kind")]
    pub kind: String,
    /// The unsigned, advisory display name (the author's folder name); absent ⇒ show `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The resolved per-bundle protection posture — `"open"` or `"reviewed"` (the client's publish
    /// preflight; the server re-decides authoritatively on every write). An OPEN string for forward
    /// compat — a client treats an unrecognized value as the stricter posture.
    pub protection: String,
    /// The pinned `current` version's commit id (64-char lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    /// The `current` byte-exact consent hash (64-char lowercase hex) — the fetch + re-hash pin.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// The `current` pointer's generation.
    pub generation: u64,
    /// When `current` last moved (epoch milliseconds).
    pub updated_at: i64,
    /// Why this device is entitled to the skill (channels ∪ direct).
    pub via: WireVia,
}

/// One unacked, person-scoped notice in the delivery feed. `kind` is an OPEN vocabulary that grows without
/// a schema break (today's values include `"verdict"` and `"proposal_closed"`); every other field is
/// present only when the notice names it. The silent auto-update hook fetches these without acking; an
/// interactive surface acks by id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireNotice {
    /// The notice id (the ack handle).
    pub id: String,
    /// The notice kind — an OPEN vocabulary (e.g. `"verdict"`, `"proposal_closed"`); a client ignores a
    /// kind it does not recognize.
    pub kind: String,
    /// The skill the notice concerns, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_id: Option<String>,
    /// The skill's current catalog name (joined for narration), when the notice names a skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_name: Option<String>,
    /// The version the notice concerns (64-char lowercase hex), when it names one.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// The actor whose action raised the notice, when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// The outcome (e.g. a verdict's `approve` / `reject`), when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The human reason (a review's rationale), when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// A rendered human message, when the notice carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// When the notice was created (the server-stamped RFC-3339 string).
    pub created_at: String,
}

/// `GET /v1/workspaces/{ws}/delivery` response body — the update answer for ONE enrolled device: the
/// entitled skills (what this device should have), the person's detached skills (freeze-in-place, never
/// cleaned), the unacked notices feed, and the open-proposal count across the entitled set. The
/// session-start hook fetches it once per workspace and reconciles the harness against it, silently. A
/// versioned envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireDelivery {
    /// Always `1` for this contract version (the schema pins it `const`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 1)))]
    pub schema_version: u32,
    /// The workspace this delivery is scoped to (echoed from the path).
    pub workspace_id: String,
    /// The entitled skills — everything this device should have (a possibly-empty list).
    pub skills: Vec<WireDeliverySkill>,
    /// The skill ids the person detached (unfollowed, or lapsed via a channel leave / removal) and that are
    /// NOT currently re-entitled — every device freezes these in place, never cleaning them.
    pub detached: Vec<String>,
    /// The skill ids THIS DEVICE excludes ("not on this device") — the third actor in the who-acts
    /// split, alongside the person (`detached`) and upstream (absent from `skills` entirely). The copy
    /// leaves this device; the person keeps receiving it on every other device; `follow` here lifts it.
    /// Sent so the client narrates the true CAUSE instead of mistaking an exclusion written elsewhere
    /// (the web, a second tool) for an upstream withdrawal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<String>,
    /// The unacked, person-scoped notices (verdicts, proposal closures, …).
    pub notices: Vec<WireNotice>,
    /// The count of OPEN, non-stale proposals across the entitled skills (the review-inbox pressure gauge).
    pub proposals_awaiting: u64,
    /// The workspace's staleness window (epoch milliseconds) — the ONE clock the fleet page and the
    /// client's hook warning both read: a device whose last report is older than this is stale.
    /// Additive: an older producer that omits it falls back to the one-week default (a whole delivery
    /// body must never fail to parse over one new field).
    #[serde(default = "default_staleness_window_ms")]
    pub staleness_window_ms: u64,
}

/// The default bundle kind — the fallback when a producer predating the catalog `kind` omits it
/// (everything such a producer serves is a skill).
fn default_bundle_kind() -> String {
    "skill".to_owned()
}

/// The default staleness window (one week, ms) — the fallback when a producer omits the field.
fn default_staleness_window_ms() -> u64 {
    604_800_000
}

/// One applied-state row a device reports: the skill and the version it currently holds after its
/// reconcile.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireAppliedSkill {
    /// The skill id (the `<skill>` path segment).
    pub skill_id: String,
    /// The version this device holds (64-char lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
}

/// `PUT /v1/workspaces/{ws}/report` body — the fleet's applied-state report: this device's
/// `(skill, applied version)` snapshot after a reconcile. The plane upserts the snapshot, drops the
/// non-detached rows it no longer names, and stamps the staleness clock (last-writer-wins; no receipt).
/// The acting device rides the `Authorization: Bearer` workspace credential, never a body field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireAppliedReport {
    /// Always `1` for this contract version (the schema pins it `const`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 1)))]
    pub schema_version: u32,
    /// The device's applied skills (possibly empty — a device holding nothing reports an empty list).
    pub applied: Vec<WireAppliedSkill>,
}

// =================================================================================================
// Device-auth request/response DTOs — the gh-style device flow the APP serves (`POST
// /v1/device/authorize` + `POST /v1/device/token`). A device asks to join a workspace; a signed-in
// human approves it in the browser; the poll then returns the device's ONE bearer credential.
//
// Design fact: on approval the `device_code` itself is PROMOTED to the device's bearer credential
// server-side (the same sha256 stored twice — once as the flow row's code hash, once as the device
// credential hash), and the poll's `credential` field carries it back — so the CLI stores ONE secret
// from ONE field and no second mint/redeem round-trip exists.
// =================================================================================================

/// `POST /v1/device/authorize` body — begin a device-authorization flow toward a workspace named by
/// its address slug. Whether the name exists is never disclosed on this route: an unknown name runs
/// the same flow to the same uniform denial.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceAuthStartRequest {
    /// A human-readable device name shown on the approval page (a confused-deputy guard, not
    /// authority) and kept as the device's display name once approved.
    pub requested_name: String,
    /// The workspace ADDRESS slug the device asks to join (`topos.sh/<name>` minus the origin). An
    /// EMPTY string names "the workspace the origin itself addresses" (single-tenant installs, where
    /// the origin IS its one workspace); a non-empty value is the address slug as today.
    pub workspace: String,
}

/// `POST /v1/device/authorize` response — the device-authorization grant (RFC-8628-shaped names).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceAuthStartResponse {
    /// The SECRET device code the client polls `device/token` with. On approval this same secret is
    /// promoted to the device's bearer credential (see the module note), so it is stored like one.
    pub device_code: String,
    /// The short human-facing code the approval page displays (a cross-check, never typed as a
    /// secret).
    pub user_code: String,
    /// The approval URL a signed-in human visits.
    pub verification_uri: String,
    /// The approval URL with the user code already embedded — the one link to open; a client uses
    /// it VERBATIM when present.
    pub verification_uri_complete: String,
    /// The flow lifetime, in seconds.
    pub expires_in_secs: u64,
    /// The minimum poll interval, in seconds.
    pub interval_secs: u64,
}

/// `POST /v1/device/token` body — poll a device-authorization flow for its outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceAuthPollRequest {
    /// The SECRET device code from `device/authorize`.
    pub device_code: String,
}

/// A device-authorization poll status (snake_case). `granted` carries the credential + the joined
/// workspace; every other status carries only itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum DeviceAuthPollStatus {
    /// Not yet approved — keep polling at the interval.
    Pending,
    /// The flow was denied at the approval page.
    Denied,
    /// The flow expired before approval.
    Expired,
    /// Approved — `credential`, `device_id`, and `workspace` are present.
    Granted,
}

/// The workspace context a `granted` poll carries — everything the CLI needs to record what it
/// enrolled into (the id it scopes requests by, the address slug it joined at, and a display name).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceAuthWorkspace {
    /// The workspace id (the `{ws}` path segment of every subsequent request).
    pub workspace_id: String,
    /// The workspace's ADDRESS slug (what the human typed at `follow`).
    pub name: String,
    /// The workspace's display name.
    pub display_name: String,
}

/// `POST /v1/device/token` response — the poll `status`; a `granted` poll carries the device's ONE
/// bearer credential (the promoted device code — returned here and stored from this ONE field),
/// its device id, and the joined workspace. A re-poll of an approved flow returns the same answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceAuthPollResponse {
    /// The poll status.
    pub status: DeviceAuthPollStatus,
    /// The device's plaintext bearer credential — present ONLY when `status` is `granted`. Returned
    /// once per poll; the server stores only its sha256.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// The registered device's id — present ONLY when `status` is `granted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// The joined workspace — present ONLY when `status` is `granted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<DeviceAuthWorkspace>,
}

// =================================================================================================
// Governance request bodies — owner/admin mutations. The ACTING device rides the `Authorization:
// Bearer` workspace credential (resolved in-transaction to its registry row; authority is the row +
// role matrix — never a body field); the op is derived from the route + body. `op_id` is a UUIDv4.
// =================================================================================================

/// `POST /v1/workspaces/{ws}/invitations` body — invitation as a ROSTER WRITE: seat each email as an
/// invited member (recording who invited whom) and optionally pre-place the person into channels.
/// There is no invite link and no role field — every CLI invitee starts as a member (roles are raised
/// later, on the web), and joining is `follow <address>` plus proof of the invited email. Member-level
/// unless the workspace's invite policy restricts inviting to owners.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct InvitationRequest {
    /// The emails to seat as invited members (folded to the canonical lowercase form server-side).
    pub emails: Vec<String>,
    /// Channel names to pre-place each invitee into (re-inviting restores placements; the structural
    /// `everyone` needs no placement and is accepted as a no-op).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
}

/// `POST /v1/workspaces/{ws}/invitations` success `data` — what the inviter pastes onward: the workspace
/// ADDRESS (the whole invitation besides the roster rows themselves).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct InvitationData {
    /// The workspace address (the share link — it carries nothing; the roster is the lock).
    pub address: String,
    /// The emails now seated as invited members (canonical form).
    pub invited: Vec<String>,
    /// Whether invitation mail was sent server-side (`true` only when the server can send mail).
    pub mailed: bool,
}

/// `DELETE /v1/workspaces/{ws}/devices` body — revoke a registered device key (owner, or the device's own
/// principal). The revoke is INSTANT (flip `revoked` in one transaction — the row's credential stops
/// authorizing fresh work the moment it commits).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceRevokeRequest {
    /// The target workspace id (scopes the op to one workspace).
    pub workspace_id: String,
    /// The client-minted UUIDv4 idempotency key.
    #[cfg_attr(feature = "contract-derives", schemars(extend("format" = "uuid")))]
    pub op_id: String,
    /// The id of the device key to revoke (the TARGET, named by its non-secret id; the actor rides the
    /// Bearer credential).
    pub target_device_key_id: String,
}

// =================================================================================================
// The adopted verb surface — subscription / curation / protection / notices / invitation wire
// bodies, the login redeem, and the member-scoped describe reads the two-phase verbs run.
// =================================================================================================

/// `PUT /v1/workspaces/{ws}/skills/{skill}/protection` and
/// `PUT /v1/workspaces/{ws}/channels/{ch}/protection` body — the safety knob. Levels are per kind:
/// `reviewed`/`open` for a skill bundle, `curated`/`open` for a channel. Tightening takes reviewer+;
/// loosening back to `open` takes an owner.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct ProtectionSetRequest {
    /// The target level (`open`, `reviewed` for skills, `curated` for channels).
    pub level: String,
}

/// `POST /v1/workspaces/{ws}/notices/ack` body — acknowledge notices by id (person-scoped read-state;
/// the silent hook fetches without acking, an interactive narration acks what it narrated).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct NoticeAckRequest {
    /// The notice ids to mark read (only the caller's own unacked rows move; unknown ids are ignored).
    pub ids: Vec<String>,
}

/// The MACHINE face of the constant protocol card — what an `Accept: application/json` GET of any
/// resource address returns, identical for every path (no content, no existence signal): just enough
/// for a client to re-root onto the API and run `follow`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireProtocolCard {
    /// Always `1` for this contract version (the schema pins it `const`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 1)))]
    pub schema_version: u32,
    /// The constant discriminant a client dispatches on (`"topos-protocol-card"`).
    pub card: String,
    /// The API base URL the client re-roots onto (the origin serving `/v1`).
    pub api_base_url: String,
}

/// `GET /v1/workspaces/{ws}/me` response — the caller's own membership describe: who you are here,
/// who added you, and the workspace's address block. Member-scoped (the uniform not-found otherwise).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireMe {
    /// The workspace id.
    pub workspace_id: String,
    /// The workspace's ADDRESS name.
    pub name: String,
    /// The workspace's display name.
    pub display_name: String,
    /// The workspace's full address (the share link — server-built on the public link base).
    pub address: String,
    /// The caller's principal (canonical form).
    pub principal: String,
    /// The caller's role (`owner` / `reviewer` / `member`).
    pub role: String,
    /// Who invited this principal (absent for a genesis owner).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invited_by: Option<String>,
    /// The workspace's invite policy (`members` or `owners`).
    pub invite_policy: String,
}

/// One skill reference inside a channel entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireChannelSkill {
    /// The referenced skill id.
    pub skill_id: String,
    /// The skill's catalog name.
    pub name: String,
}

/// One channel in the workspace's channel index.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireChannelEntry {
    /// The channel's name.
    pub name: String,
    /// The channel's mode (`open` or `curated`).
    pub mode: String,
    /// Whether this is the structural `everyone` (roster-derived membership; cannot be joined or left).
    pub builtin: bool,
    /// Whether the CALLER is a member (always `true` on `everyone`).
    pub member: bool,
    /// How many people the channel reaches (confirmed roster size for `everyone`).
    pub member_count: u64,
    /// The skills the channel references.
    pub skills: Vec<WireChannelSkill>,
}

/// `GET /v1/workspaces/{ws}/channels` response — the workspace's channels with the caller's own
/// membership marked. Member-scoped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireChannelIndex {
    /// The channels (the structural `everyone` always present).
    pub channels: Vec<WireChannelEntry>,
}

/// One open proposal in the workspace's review inbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireProposalEntry {
    /// The skill the proposal targets.
    pub skill_id: String,
    /// The skill's catalog name.
    pub skill_name: String,
    /// The proposed version's commit id (64-char lowercase hex) — the review target handle.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    /// The base version the proposal was built on (64-char lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub base_version_id: String,
    /// The author's principal.
    pub proposer: String,
    /// The author's message (the proposed version's commit message) — what a reviewer reads FIRST.
    pub message: String,
    /// When the proposal was opened (the server-stamped string).
    pub created_at: String,
    /// Whether `current` has moved since the proposal's base (a stale proposal needs a re-propose).
    pub stale: bool,
    /// Whether the CALLER authored this proposal — the server computes it from the resolved user id
    /// (never email equality). The client's inbox uses it to split the outbox (yours) from the inbox
    /// (others') and to offer the author `--withdraw` instead of `--approve`. `Some` is AUTHORITATIVE
    /// either way (a served `false` is a served "not yours" — never overridden client-side); only a
    /// producer predating the field omits it (`None`), and only then does the client fall back to
    /// comparing principals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yours: Option<bool>,
}

/// `GET /v1/workspaces/{ws}/proposals` response — every OPEN proposal in the workspace, author-message
/// first. The caller splits inbox (others') from outbox (own) by `proposer`. Member-scoped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireProposalIndex {
    /// The open proposals (possibly empty).
    pub proposals: Vec<WireProposalEntry>,
}

/// One version in a skill's history. A PURGED version stays listed — who purged it and when — with its
/// bytes gone (author/message may be absent once the underlying objects are reclaimed).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireLogVersion {
    /// The version's commit id (64-char lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    /// The version author, when the commit object is still readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// The version message, when the commit object is still readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Whether this version is the skill's `current`.
    pub current: bool,
    /// When the version's bytes were purged (epoch milliseconds) — the tombstone half.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purged_at: Option<i64>,
    /// Who purged the version's bytes — the tombstone half.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purged_by: Option<String>,
}

/// One proposal event in a skill's history (open, and every resolution).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireLogProposal {
    /// The proposed version's commit id (64-char lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    /// The author's principal.
    pub proposer: String,
    /// The proposal's status (`open` / `accepted` / `rejected` / `closed`).
    pub status: String,
    /// Who resolved it, when resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    /// The resolution reason (a reject's rationale; a closure's cause), when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_reason: Option<String>,
    /// When it was resolved (server-stamped string), when resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    /// When the proposal was opened (server-stamped string).
    pub created_at: String,
}

/// `GET /v1/workspaces/{ws}/skills/{skill}/log` response — the skill's version history (purge
/// tombstones included) and its proposal events. An ARCHIVED skill stays addressable here; asking by a
/// freed base name answers the archived successor as a hint. Member-scoped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireSkillLog {
    /// The skill id.
    pub skill_id: String,
    /// The skill's current catalog name (the archived spelling once archived).
    pub name: String,
    /// The catalog's bundle kind — `"skill"` for everything that exists today. Display metadata
    /// only: clients render it and never branch on it. Additive: an older producer omits it.
    #[serde(default = "default_bundle_kind")]
    pub kind: String,
    /// The skill's lifecycle status (`active` / `archived` / `deleted`).
    pub status: String,
    /// The pre-archive name, when archived (what the skill was called before the rename freed it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_name: Option<String>,
    /// The versions, newest first where orderable (the ancestry walk stops at reclaimed history;
    /// tombstoned leftovers follow unordered).
    pub versions: Vec<WireLogVersion>,
    /// The proposal events, newest first.
    pub proposals: Vec<WireLogProposal>,
}

/// `GET /v1/workspaces/{ws}/skills/{skill}/reach` response — the audience a change to this skill
/// reaches (the describe number behind `publish` and `protect`). Member-scoped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireReach {
    /// How many people are entitled to the skill.
    pub persons: u64,
    /// How many registered, non-revoked devices those people hold here.
    pub devices: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_file_mode_serializes_to_octal_strings() {
        assert_eq!(
            serde_json::to_string(&WireFileMode::Regular).unwrap(),
            "\"100644\""
        );
        assert_eq!(
            serde_json::to_string(&WireFileMode::Executable).unwrap(),
            "\"100755\""
        );
        assert_eq!(
            serde_json::from_str::<WireFileMode>("\"100755\"").unwrap(),
            WireFileMode::Executable
        );
    }

    #[test]
    fn publish_request_round_trips_snake_case_no_created_at() {
        let req = PublishRequest {
            channel: None,
            workspace_id: "w_demo".to_owned(),
            skill_id: "s_prdescribe".to_owned(),
            op_id: "f47ac10b-58cc-4372-a567-0e02b2c3d479".to_owned(),
            expected: 42,
            candidate: WireCandidate {
                files: vec![WireFile {
                    path: "SKILL.md".to_owned(),
                    mode: WireFileMode::Regular,
                    content_base64: "aGVsbG8=".to_owned(),
                }],
                parents: vec!["a".repeat(64)],
                author: "d_test".to_owned(),
                message: "topos: publish".to_owned(),
            },
            display_name: Some("Deploy".to_owned()),
        };
        let v = serde_json::to_value(&req).unwrap();
        // snake_case field names, candidate nested, the server-stamped time absent — and NO credential
        // material: the workspace credential rides the Authorization header, never a body field.
        assert_eq!(v["workspace_id"], "w_demo");
        assert_eq!(v["expected"], 42);
        assert_eq!(v["candidate"]["files"][0]["mode"], "100644");
        assert_eq!(v["display_name"], "Deploy");
        assert!(v.get("created_at").is_none());
        assert!(v.get("device_key_id").is_none() && v.get("credential").is_none());
        let back: PublishRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back.candidate.parents, vec!["a".repeat(64)]);
        assert_eq!(back.display_name.as_deref(), Some("Deploy"));
        // A body without display_name still deserializes (additive-compat), yielding None.
        let old: PublishRequest = serde_json::from_value(serde_json::json!({
            "workspace_id": "w_demo",
            "skill_id": "s_prdescribe",
            "op_id": "f47ac10b-58cc-4372-a567-0e02b2c3d479",
            "expected": 42,
            "candidate": { "files": [], "parents": [], "author": "d", "message": "m" },
        }))
        .unwrap();
        assert!(old.display_name.is_none());
    }

    #[test]
    fn version_meta_round_trips() {
        let meta = WireVersionMeta {
            version_id: "a".repeat(64),
            parents: vec!["b".repeat(64)],
            author: "d_test".to_owned(),
            message: "topos: add".to_owned(),
            bundle_digest: "c".repeat(64),
            files: vec![WireVersionFile {
                path: "run.sh".to_owned(),
                mode: WireFileMode::Executable,
                object_id: "d".repeat(64),
            }],
        };
        let v = serde_json::to_value(&meta).unwrap();
        assert_eq!(v["files"][0]["mode"], "100755");
        let back: WireVersionMeta = serde_json::from_value(v).unwrap();
        assert_eq!(back.version_id, "a".repeat(64));
        assert_eq!(back.files[0].object_id, "d".repeat(64));
    }

    #[test]
    fn proposal_list_round_trips() {
        let list = WireProposalList {
            proposals: vec![WireOpenProposal {
                version_id: "e".repeat(64),
                base_generation: 7,
                created_at: "2026-06-30T00:00:00Z".to_owned(),
            }],
        };
        let v = serde_json::to_value(&list).unwrap();
        assert_eq!(v["proposals"][0]["version_id"], "e".repeat(64));
        assert_eq!(v["proposals"][0]["base_generation"], 7);
        assert_eq!(v["proposals"][0]["created_at"], "2026-06-30T00:00:00Z");
        let back: WireProposalList = serde_json::from_value(v).unwrap();
        assert_eq!(back.proposals.len(), 1);
        assert_eq!(back.proposals[0].base_generation, 7);
        // An empty list is a valid response (no open proposals).
        let empty: WireProposalList =
            serde_json::from_value(serde_json::json!({ "proposals": [] })).unwrap();
        assert!(empty.proposals.is_empty());
    }

    #[test]
    fn device_auth_poll_is_snake_case_and_granted_carries_the_one_credential() {
        assert_eq!(
            serde_json::to_string(&DeviceAuthPollStatus::Pending).unwrap(),
            "\"pending\""
        );
        // A pending poll carries only the status.
        let pending = DeviceAuthPollResponse {
            status: DeviceAuthPollStatus::Pending,
            credential: None,
            device_id: None,
            workspace: None,
        };
        let v = serde_json::to_value(&pending).unwrap();
        assert_eq!(v["status"], "pending");
        assert!(v.get("credential").is_none() && v.get("workspace").is_none());
        // A granted poll carries the ONE credential (the promoted device code), the device id, and
        // the joined workspace — everything the CLI stores, from one field each.
        let granted = DeviceAuthPollResponse {
            status: DeviceAuthPollStatus::Granted,
            credential: Some("dc_secret".to_owned()),
            device_id: Some("dev_1".to_owned()),
            workspace: Some(DeviceAuthWorkspace {
                workspace_id: "w_acme".to_owned(),
                name: "acme".to_owned(),
                display_name: "Acme".to_owned(),
            }),
        };
        let v = serde_json::to_value(&granted).unwrap();
        assert_eq!(v["status"], "granted");
        assert_eq!(v["credential"], "dc_secret");
        assert_eq!(v["device_id"], "dev_1");
        assert_eq!(v["workspace"]["name"], "acme");
        // The start pair round-trips; the poll target is the same secret the start returned.
        let start = DeviceAuthStartResponse {
            device_code: "dc_secret".to_owned(),
            user_code: "AAAA-BBBB".to_owned(),
            verification_uri: "https://topos.example/devices".to_owned(),
            verification_uri_complete: "https://topos.example/devices?code=AAAA-BBBB".to_owned(),
            expires_in_secs: 900,
            interval_secs: 5,
        };
        let v = serde_json::to_value(&start).unwrap();
        assert_eq!(v["expires_in_secs"], 900);
        assert_eq!(v["interval_secs"], 5);
        let req: DeviceAuthStartRequest = serde_json::from_value(serde_json::json!({
            "requested_name": "laptop",
            "workspace": "acme",
        }))
        .unwrap();
        assert_eq!(req.workspace, "acme");
        let poll: DeviceAuthPollRequest =
            serde_json::from_value(serde_json::json!({ "device_code": "dc_secret" })).unwrap();
        assert_eq!(poll.device_code, "dc_secret");
    }

    #[test]
    fn skill_index_entry_carries_name_and_status() {
        let entry = WireSkillIndexEntry {
            skill_id: "s_prdescribe".to_owned(),
            name: "pr-describe".to_owned(),
            kind: "skill".to_owned(),
            status: "active".to_owned(),
            version_id: "a".repeat(64),
            bundle_digest: "b".repeat(64),
            generation: 3,
            display_name: None,
            updated_at: 1_700_000_000_000,
            open_proposals: 0,
        };
        let v = serde_json::to_value(&entry).unwrap();
        // The new fields ride under their snake_case spellings.
        assert_eq!(v["name"], "pr-describe");
        assert_eq!(v["status"], "active");
        // An absent display_name omits (skip_serializing_if).
        assert!(v.get("display_name").is_none());
        let back: WireSkillIndexEntry = serde_json::from_value(v).unwrap();
        assert_eq!(back.name, "pr-describe");
        assert_eq!(back.status, "active");
    }

    #[test]
    fn delivery_round_trips_snake_case_and_omits_absent_notice_fields() {
        let delivery = WireDelivery {
            schema_version: 1,
            workspace_id: "w_demo".to_owned(),
            skills: vec![WireDeliverySkill {
                skill_id: "s_prdescribe".to_owned(),
                name: "pr-describe".to_owned(),
                kind: "skill".to_owned(),
                display_name: Some("PR describe".to_owned()),
                protection: "reviewed".to_owned(),
                version_id: "a".repeat(64),
                bundle_digest: "b".repeat(64),
                generation: 7,
                updated_at: 1_700_000_000_000,
                via: WireVia {
                    channels: vec!["everyone".to_owned()],
                    direct: true,
                },
            }],
            detached: vec!["s_old".to_owned()],
            excluded: Vec::new(),
            notices: vec![WireNotice {
                id: "ntc_1".to_owned(),
                kind: "verdict".to_owned(),
                skill_id: Some("s_prdescribe".to_owned()),
                skill_name: Some("pr-describe".to_owned()),
                version_id: None,
                actor: Some("alice@acme.com".to_owned()),
                outcome: Some("approve".to_owned()),
                reason: Some("looks good".to_owned()),
                message: None,
                created_at: "2026-06-25T00:00:00Z".to_owned(),
            }],
            proposals_awaiting: 2,
            staleness_window_ms: 604_800_000,
        };
        let v = serde_json::to_value(&delivery).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["workspace_id"], "w_demo");
        assert_eq!(v["skills"][0]["skill_id"], "s_prdescribe");
        assert_eq!(v["skills"][0]["protection"], "reviewed");
        // `via` carries the channels + direct flag under snake_case.
        assert_eq!(v["skills"][0]["via"]["channels"][0], "everyone");
        assert_eq!(v["skills"][0]["via"]["direct"], true);
        assert_eq!(v["detached"][0], "s_old");
        assert_eq!(v["notices"][0]["kind"], "verdict");
        assert_eq!(v["notices"][0]["reason"], "looks good");
        // Absent optional notice fields omit (skip_serializing_if) — never serialized as null.
        assert!(v["notices"][0].get("version_id").is_none());
        assert!(v["notices"][0].get("message").is_none());
        // The ONE staleness clock rides every delivery.
        assert_eq!(v["staleness_window_ms"], 604_800_000_u64);
        let back: WireDelivery = serde_json::from_value(v).unwrap();
        assert_eq!(back.skills.len(), 1);
        assert_eq!(back.skills[0].via.channels, vec!["everyone".to_owned()]);
        assert_eq!(back.proposals_awaiting, 2);
        // An empty delivery (nothing entitled, nothing detached) still round-trips.
        let empty: WireDelivery = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "workspace_id": "w_demo",
            "skills": [],
            "detached": [],
            "notices": [],
            "proposals_awaiting": 0,
            "staleness_window_ms": 604800000,
        }))
        .unwrap();
        assert!(empty.skills.is_empty() && empty.notices.is_empty());
    }

    #[test]
    fn applied_report_round_trips_snake_case() {
        let report = WireAppliedReport {
            schema_version: 1,
            applied: vec![WireAppliedSkill {
                skill_id: "s_prdescribe".to_owned(),
                version_id: "c".repeat(64),
            }],
        };
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["applied"][0]["skill_id"], "s_prdescribe");
        assert_eq!(v["applied"][0]["version_id"], "c".repeat(64));
        let back: WireAppliedReport = serde_json::from_value(v).unwrap();
        assert_eq!(back.applied.len(), 1);
        // An empty report (a device that holds nothing yet) round-trips.
        let empty: WireAppliedReport =
            serde_json::from_value(serde_json::json!({ "schema_version": 1, "applied": [] }))
                .unwrap();
        assert!(empty.applied.is_empty());
    }

    #[test]
    fn invitation_bodies_round_trip() {
        // The invitation is a roster write: emails + optional channel pre-placements, no role
        // field (every CLI invitee starts as a member) and no link (the address is the answer).
        let req = InvitationRequest {
            emails: vec!["alice@acme.com".to_owned()],
            channels: vec!["ops".to_owned()],
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["emails"][0], "alice@acme.com");
        assert_eq!(v["channels"][0], "ops");
        // The acting device rides the Authorization header — never a body field.
        assert!(v.get("device_key_id").is_none() && v.get("credential").is_none());
        assert!(v.get("role").is_none(), "invitations mint members only");
        // Channels omit when empty (skip_serializing_if) and default on the way in.
        let bare: InvitationRequest =
            serde_json::from_value(serde_json::json!({ "emails": ["bob@acme.com"] })).unwrap();
        assert!(bare.channels.is_empty());
        let data = InvitationData {
            address: "https://topos.example/acme".to_owned(),
            invited: vec!["alice@acme.com".to_owned()],
            mailed: false,
        };
        let v = serde_json::to_value(&data).unwrap();
        assert_eq!(v["address"], "https://topos.example/acme");
        assert_eq!(v["mailed"], false);
    }
}
