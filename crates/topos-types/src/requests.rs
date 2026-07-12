//! Wire request/response DTOs for the plane's HTTP write + version-metadata routes.
//!
//! The JSON bodies the plane accepts on `publish` / `propose` / `revert` / `review`, plus the body the
//! version-metadata read route returns. These are **deserialization shapes** only (no logic): the route
//! handler parses them into `plane-store`/`topos-core` domain types at the edge (parse-don't-validate) and
//! **server-rehashes every candidate byte** — a client-supplied id or hash is never trusted.
//!
//! **No `created_at` on any request.** The plane stamps the receipt's time from the server clock; a client
//! never supplies a wall clock (an ambient time would be a replay / skew lever). The handler derives both
//! the RFC-3339 string and the `now: i64` it passes into the authority op.
//!
//! **The write credential is the workspace credential in the `Authorization: Bearer` header — never a
//! body field.** The plane resolves the presented secret (by its sha256) to the device's registry row
//! in-transaction: the row supplies the acting `device_key_id` and its bound principal, whose CONFIRMED
//! `workspace_member` seat is the authorization. Keeping the secret out of the body keeps it out of the
//! receipt request identities and the client's persisted op-WAL, so a credential rotation between
//! retries never breaks byte-identical replay. The `op` (publish / propose /
//! revert / review-decision) is derived from the route, never the body.
//!
//! Field names are snake_case as written (no `rename_all`). Hex id fields carry the same `^[0-9a-f]{64}$`
//! constraint used across [`crate`].

use crate::Generation;
use crate::bootstrap::{BootstrapSkill, VerifiedDomainStatus};
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
    /// The `(epoch, seq)` this publish's compare-and-set targets; a stale pair yields `CONFLICT`.
    pub expected: Generation,
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
    /// The `(epoch, seq)` the proposal is born against (its base); a stale base later makes it non-current.
    pub expected: Generation,
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
    /// The `(epoch, seq)` this revert's compare-and-set targets; a stale pair yields `CONFLICT`.
    pub expected: Generation,
    /// The GOOD version (the `version_id` whose bytes are restored) as 64-char lowercase hex.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub good: String,
    /// The author device id recorded in the forward-revert commit frame.
    pub author: String,
    /// The forward-revert commit message.
    pub message: String,
}

/// `POST /v1/reviews` body — a governance decision on an open proposal. `approve` runs the shared
/// `(epoch, seq)` compare-and-set on the proposal's base (a stale base ⇒ `CONFLICT`) and, under
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
    /// The `(epoch, seq)` the approval's compare-and-set targets (the proposal's base); a stale pair on an
    /// `approve` yields `CONFLICT`.
    pub expected: Generation,
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
    /// The `(epoch, seq)` the proposal was opened against (its base); when `current` advances past it the
    /// proposal stales and drops out of this list.
    pub base_generation: Generation,
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
    /// The catalog lifecycle status — `"active"` / `"archived"` (a deleted skill has no `current` row and
    /// so no entry). An OPEN string, deliberately: a new state can land without a schema break.
    pub status: String,
    /// The `current` version's commit id (64-char lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub version_id: String,
    /// The `current` byte-exact consent hash (64-char lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// The `current` pointer's `(epoch, seq)`.
    pub generation: Generation,
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
    /// The `current` pointer's `(epoch, seq)`.
    pub generation: Generation,
    /// When `current` last moved (epoch milliseconds).
    pub updated_at: i64,
    /// Why this device is entitled to the skill (channels ∪ direct).
    pub via: WireVia,
}

/// One unacked, person-scoped notice in the delivery feed. `kind` is an OPEN vocabulary that grows without
/// a schema break (today's values include `"verdict"` and `"proposal_closed"`); every other field is
/// present only when the notice names it. The silent currency hook fetches these without acking; an
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

/// `GET /v1/workspaces/{ws}/delivery` response body — the currency answer for ONE enrolled device: the
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
// Enrollment request/response DTOs — the device-flow / passcode / redeem / admin-claim wire bodies.
//
// The enrollment credentials (device code, grant, read token, device public key) are OPAQUE strings,
// never trusted as ids: the server re-derives every id from the bytes (the device key id from the public
// key, the grant by its sha256). Redeem binds the device by checking its body `device_public_key` equals
// the grant's bound key — a binding check, not a possession proof. Field names are snake_case as written.
// =================================================================================================

/// A device-auth session's intent — a CLOSED set (snake_case). `enroll` joins an existing workspace named
/// by its ADDRESS; `standup` starts with NO workspace (a signed-in human's approval creates one and seats
/// the approver as its first owner); `login` proves the person's identity and re-mints this device's
/// workspace credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum SessionIntent {
    /// Join an existing workspace, named by its address (the default when a `workspace` is present).
    Enroll,
    /// Create a workspace on approval (the hosted self-serve first-boot flow).
    Standup,
    /// Prove the person's identity and re-mint this device's credential in every workspace where that
    /// identity holds a confirmed seat (sign-in + the one credential-recovery action).
    Login,
}

/// `POST /v1/device/authorize` body — begin an RFC-8628 device-authorization flow. With a `workspace`
/// (the address name; the default `enroll` intent) the device enrolls toward that workspace; with
/// `intent = "standup"` — or neither field — it starts a STANDUP session (no workspace yet; hosted
/// planes only); with `intent = "login"` it starts a workspace-less LOGIN session whose grant redeems
/// at `POST /v1/login`. The server SERVER-derives the device key id from `device_public_key` (a
/// client-asserted id is never trusted).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceAuthorizeRequest {
    /// The workspace ADDRESS name an enroll session targets (`topos.sh/<name>` minus the origin).
    /// Whether the name exists is never disclosed on this route: an unknown name runs the same flow to
    /// the same uniform denial a not-yours workspace gets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The session intent. Absent defaults to `enroll` when a workspace is named, `standup` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<SessionIntent>,
    /// The device's raw 32-byte public key, base64url-unpadded. The server derives the device key id from
    /// it (never a client-asserted id) and binds it to the enrollment session.
    pub device_public_key: String,
    /// A human-readable machine name shown on the verification page (a confused-deputy guard, not authority).
    pub machine_name: String,
}

/// `POST /v1/device/authorize` response — the RFC-8628 device-authorization grant (the names mirror the RFC).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceAuthorizeResponse {
    /// The SECRET device code the client polls `device/token` with.
    pub device_code: String,
    /// The opaque code identifying this device-auth session; it rides inside `verification_uri_complete`
    /// (the human clicks the URL — it is not typed).
    pub user_code: String,
    /// The verification URL a human visits to approve the session.
    pub verification_uri: String,
    /// The verification URL with the user code already embedded (RFC-8628 `verification_uri_complete`) —
    /// the one link to open; a client uses it VERBATIM when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_uri_complete: Option<String>,
    /// The session lifetime, in seconds (RFC-8628 `expires_in`).
    pub expires_in: u64,
    /// The minimum poll interval, in seconds (RFC-8628 `interval`).
    pub interval: u64,
    /// The plane block a STANDUP start carries (base URL, deployment posture, enrollment method) — a
    /// standup device has no `/i/` bootstrap to learn these from. Absent on an enroll start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plane: Option<crate::bootstrap::BootstrapPlane>,
}

/// `POST /v1/device/token` body — poll a device-authorization session for its grant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceTokenRequest {
    /// The SECRET device code from `device/authorize`.
    pub device_code: String,
}

/// A device-authorization poll status — the RFC-8628 outcomes (snake_case). `granted` carries the opaque
/// grant; every other status carries only itself (no grant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum DeviceTokenStatus {
    /// Not yet confirmed — keep polling at the interval.
    Pending,
    /// Polled too fast — back off.
    SlowDown,
    /// The session was denied at the verification page.
    Denied,
    /// The session expired before confirmation.
    Expired,
    /// Confirmed — the `grant` is present.
    Granted,
}

/// The workspace context a `granted` poll carries — the id + display name a STANDUP client (which never
/// read an `/i/` bootstrap) needs to build its redeem possession frame and disclose what it joined.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceTokenWorkspace {
    /// The workspace the grant is scoped to.
    pub workspace_id: String,
    /// The workspace display name (a disclosure aid; `""` if the plane no longer has one).
    pub display_name: String,
    /// The workspace's full ADDRESS (server-built on the public link base) — the share line's root;
    /// absent when the plane predates addresses or the grant is workspace-less.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// `POST /v1/device/token` response — the poll `status`, plus the opaque single-use enrollment `grant` ONLY
/// when `status` is `granted`. A re-poll of a confirmed session re-derives the SAME grant (idempotent issue).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceTokenResponse {
    /// The poll status.
    pub status: DeviceTokenStatus,
    /// The opaque single-use enrollment grant — present ONLY when `status` is `granted` (the redeem credential).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
    /// The granted session's workspace context — present when `status` is `granted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<DeviceTokenWorkspace>,
}

/// `GET /v1/enroll/verify/{user_code}` response — the verification-page disclosure a human reviews before
/// confirming an identity (the RFC-8628 confused-deputy guard). Carries no secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct VerificationContextResponse {
    /// The session's intent — `enroll` (join an existing workspace) or `standup` (approving CREATES one).
    /// Absent from older planes ⇒ `enroll`; the page branches its copy on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<SessionIntent>,
    /// The human-readable machine name the device offered at start.
    pub machine_name: String,
    /// A short hex fingerprint of the device's public key — a human cross-checks it against the device. A
    /// display aid only, never an authority input.
    pub device_fingerprint: String,
    /// The workspace display name the device would join.
    pub workspace_display_name: String,
    /// The org-domain claim, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_domain: Option<String>,
    /// The domain-verification state.
    pub verified_domain_status: VerifiedDomainStatus,
    /// The skills the invite pre-offers (each with an optional display name).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offered_skills: Vec<BootstrapSkill>,
}

/// `POST /v1/enroll/passcode` body — start a passcode challenge for an email on a live device-auth session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct PasscodeRequest {
    /// The user code naming the live device-auth session.
    pub user_code: String,
    /// The email the passcode proves control of.
    pub email: String,
}

/// The constant-shaped status of a started passcode challenge — always `sent`, so a non-rostered address is
/// no enumeration oracle (the cloud roster gate is enforced at redeem, never here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum PasscodeAckStatus {
    /// The challenge was accepted (and, for a valid address, mailed). The ONLY possible status.
    Sent,
}

/// `POST /v1/enroll/passcode` response — a CONSTANT-shaped ack (always `sent`); the send is fire-and-forget,
/// so neither the body nor its latency reveals whether the address was rostered.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct PasscodeAck {
    /// Always `sent`.
    pub status: PasscodeAckStatus,
}

/// `POST /v1/enroll/passcode/confirm` body — submit a passcode to confirm the session's identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct PasscodeConfirmRequest {
    /// The user code naming the live device-auth session.
    pub user_code: String,
    /// The email the passcode was sent to.
    pub email: String,
    /// The 6-digit passcode the human entered.
    pub code: String,
}

/// The outcome of a passcode confirmation (snake_case). A wrong code carries only the status — never the
/// attempts remaining (no brute-force timing/count oracle on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum PasscodeConfirmStatus {
    /// The code matched — the session's identity is confirmed (the device's next poll yields a grant).
    Confirmed,
    /// The code was wrong.
    WrongCode,
    /// The passcode expired.
    Expired,
    /// The attempt cap was hit — the passcode is locked.
    TooManyAttempts,
}

/// `POST /v1/enroll/passcode/confirm` response — the confirmation status.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct PasscodeConfirmResponse {
    /// The confirmation status.
    pub status: PasscodeConfirmStatus,
}

/// `POST /v1/workspaces/{ws}/devices` body — redeem an enrollment grant into a registered device + its
/// ONE minted workspace credential. The server checks `device_public_key` equals the grant's bound key
/// (a binding check) and re-derives the device key id from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct RedeemRequest {
    /// The workspace the device enrolls into (authoritatively the grant's; echoed for the client's clarity).
    pub workspace_id: String,
    /// The opaque single-use enrollment grant (from a `granted` device-token poll).
    pub grant: String,
    /// The device's raw 32-byte public key, base64url-unpadded (must equal the grant's bound key).
    pub device_public_key: String,
}

/// `POST /v1/workspaces/{ws}/devices` success payload — the confirmed enrollment: the registered device
/// and its ONE workspace credential (the `0600` at-rest secret this device presents on every read and
/// write in this workspace). **NO user token, ever; no per-skill token, ever.** Rides in the all-outcome
/// envelope's `data`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct RedeemResponse {
    /// The workspace the device enrolled into.
    pub workspace_id: String,
    /// The server-derived device key id now registered (the device's stable, non-secret name — the
    /// receipts/audit actor; never an authenticator).
    pub device_key_id: String,
    /// The principal the device now acts as (the confirmed email, or a device-rooted id) — a disclosure the
    /// client persists and prints so a hijacked standup is visible ("workspace X — owner Y").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// The plaintext workspace credential (returned ONCE; only its sha256 is stored server-side, on the
    /// device's registry row). No expiry — revocation is a directory row-write, re-enrollment the rotation.
    pub credential: String,
}

/// `POST /v1/admin-claim` body — consume a one-time self-host claim token to stand up a workspace + seat its
/// first owner. The server re-derives the device key id from `device_public_key`.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct AdminClaimRequest {
    /// The one-time admin-claim token.
    pub claim_token: String,
    /// The claiming device's raw 32-byte public key, base64url-unpadded.
    pub device_public_key: String,
    /// The display name for the standing-up workspace.
    pub display_name: String,
}

// `claim_token` is the LIVE one-time bearer owner capability — redact it so a formatted request value
// (a debug trace, a panic message) can never mint a second custody surface for it.
impl std::fmt::Debug for AdminClaimRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminClaimRequest")
            .field("claim_token", &"<redacted>")
            .field("device_public_key", &self.device_public_key)
            .field("display_name", &self.display_name)
            .finish()
    }
}

// =================================================================================================
// Governance request bodies — owner/admin mutations. The ACTING device rides the `Authorization:
// Bearer` workspace credential (resolved in-transaction to its registry row; authority is the row +
// role matrix — never a body field); the op is derived from the route + body. `op_id` is a UUIDv4.
// =================================================================================================

/// A workspace-level governance role (the RBAC roster — DISTINCT from the per-skill read roster). snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceRole {
    /// Full governance authority (invite, roster, revoke).
    Owner,
    /// A reviewer (review-gate authority; no governance authority in v0).
    Reviewer,
    /// An ordinary member (no governance authority).
    Member,
}

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

/// `PUT /v1/workspaces/{ws}/roster/{email}` body — set a principal's workspace role (owner-only). The target
/// principal is the `{email}` path segment; the role rides the body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct RosterSetRequest {
    /// The target workspace id (scopes the op to one workspace).
    pub workspace_id: String,
    /// The client-minted UUIDv4 idempotency key.
    #[cfg_attr(feature = "contract-derives", schemars(extend("format" = "uuid")))]
    pub op_id: String,
    /// The role to set on the `{email}` target.
    pub role: WorkspaceRole,
}

/// `DELETE /v1/workspaces/{ws}/roster/{email}` body — remove a principal from the workspace roster
/// (owner-only). The target principal is the `{email}` path segment; the body carries only the op identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct RosterRemoveRequest {
    /// The target workspace id (scopes the op to one workspace).
    pub workspace_id: String,
    /// The client-minted UUIDv4 idempotency key.
    #[cfg_attr(feature = "contract-derives", schemars(extend("format" = "uuid")))]
    pub op_id: String,
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

/// `PUT /v1/workspaces/{ws}/policy/review-required` body — the self-host operator toggle for the
/// `review-required` workspace policy (an idempotent set; JSON so the body stays extensible without a
/// path-shape change). Authenticated by the plane's admin token, not a device credential; the route is
/// invisible (404) on a plane with no admin token configured.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct PolicyReviewRequiredRequest {
    /// The desired policy value: `true` gates any direct publish behind a reviewer's approval.
    pub review_required: bool,
}

// =================================================================================================
// The adopted verb surface — subscription / curation / protection / notices / invitation wire
// bodies, the login redeem, and the member-scoped describe reads the two-phase verbs run.
// =================================================================================================

/// `POST /v1/login` body — redeem a LOGIN grant: prove the device key the grant is bound to and
/// receive one workspace credential per confirmed seat. The grant is the bearer credential; the
/// device public key is a binding check (nothing signs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct LoginRedeemRequest {
    /// The opaque grant from the login device flow (the bearer credential for this one exchange).
    pub grant: String,
    /// The device's raw 32-byte public key, base64url-unpadded — must equal the grant's bound key.
    pub device_public_key: String,
}

/// One workspace a login re-minted (or could not re-mint) a credential for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct LoginMembership {
    /// The workspace id.
    pub workspace_id: String,
    /// The workspace's ADDRESS name.
    pub name: String,
    /// The workspace's display name.
    pub display_name: String,
    /// The person's role on the seat (`owner` / `reviewer` / `member`).
    pub role: String,
    /// The device's non-secret key id in this workspace (server-derived).
    pub device_key_id: String,
    /// The freshly minted plaintext workspace credential — absent when the mint was refused (see
    /// `blocked`). Returned ONCE; the server stores only its hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// Why no credential was minted (e.g. this device is revoked in that workspace); absent on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

/// `POST /v1/login` success `data` — the proven identity + one entry per confirmed seat.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct LoginData {
    /// The proven principal (canonical form).
    pub principal: String,
    /// One entry per workspace where the principal holds a confirmed seat (possibly empty).
    pub memberships: Vec<LoginMembership>,
}

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
    fn admin_claim_request_debug_redacts_the_claim_token() {
        let req = AdminClaimRequest {
            claim_token: "one_time_bearer_secret".to_owned(),
            device_public_key: "pubkey_b64".to_owned(),
            display_name: "Acme".to_owned(),
        };
        let dbg = format!("{req:?}");
        assert!(dbg.contains("<redacted>"), "got {dbg}");
        assert!(
            !dbg.contains("one_time_bearer_secret"),
            "the live bearer must never appear in Debug: {dbg}"
        );
        // The non-secret fields still print (the redaction is surgical).
        assert!(dbg.contains("pubkey_b64") && dbg.contains("Acme"));
    }

    #[test]
    fn publish_request_round_trips_snake_case_no_created_at() {
        let req = PublishRequest {
            channel: None,
            workspace_id: "w_demo".to_owned(),
            skill_id: "s_prdescribe".to_owned(),
            op_id: "f47ac10b-58cc-4372-a567-0e02b2c3d479".to_owned(),
            expected: Generation { epoch: 1, seq: 42 },
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
        assert_eq!(v["expected"]["seq"], 42);
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
            "expected": { "epoch": 1, "seq": 42 },
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
                base_generation: Generation { epoch: 1, seq: 7 },
                created_at: "2026-06-30T00:00:00Z".to_owned(),
            }],
        };
        let v = serde_json::to_value(&list).unwrap();
        assert_eq!(v["proposals"][0]["version_id"], "e".repeat(64));
        assert_eq!(v["proposals"][0]["base_generation"]["seq"], 7);
        assert_eq!(v["proposals"][0]["created_at"], "2026-06-30T00:00:00Z");
        let back: WireProposalList = serde_json::from_value(v).unwrap();
        assert_eq!(back.proposals.len(), 1);
        assert_eq!(back.proposals[0].base_generation.epoch, 1);
        // An empty list is a valid response (no open proposals).
        let empty: WireProposalList =
            serde_json::from_value(serde_json::json!({ "proposals": [] })).unwrap();
        assert!(empty.proposals.is_empty());
    }

    #[test]
    fn device_token_status_is_snake_case_and_granted_carries_a_grant() {
        assert_eq!(
            serde_json::to_string(&DeviceTokenStatus::SlowDown).unwrap(),
            "\"slow_down\""
        );
        // A pending poll carries only the status (no grant, no workspace).
        let pending = DeviceTokenResponse {
            status: DeviceTokenStatus::Pending,
            grant: None,
            workspace: None,
        };
        let v = serde_json::to_value(&pending).unwrap();
        assert_eq!(v["status"], "pending");
        assert!(v.get("grant").is_none(), "no grant unless granted");
        assert!(v.get("workspace").is_none(), "no workspace unless granted");
        // A granted poll carries the opaque grant + the workspace context.
        let granted = DeviceTokenResponse {
            status: DeviceTokenStatus::Granted,
            grant: Some("g_opaque".to_owned()),
            workspace: Some(DeviceTokenWorkspace {
                workspace_id: "w_acme".to_owned(),
                display_name: "Acme".to_owned(),
                address: Some("https://topos.example/acme".to_owned()),
            }),
        };
        let v = serde_json::to_value(&granted).unwrap();
        assert_eq!(v["status"], "granted");
        assert_eq!(v["grant"], "g_opaque");
        assert_eq!(v["workspace"]["workspace_id"], "w_acme");
        assert_eq!(v["workspace"]["display_name"], "Acme");
        assert_eq!(v["workspace"]["address"], "https://topos.example/acme");
        // An OLD response without the workspace block still deserializes (additive-compat).
        let old: DeviceTokenResponse =
            serde_json::from_value(serde_json::json!({ "status": "granted", "grant": "g" }))
                .unwrap();
        assert!(old.workspace.is_none());
    }

    #[test]
    fn device_authorize_request_intent_and_optional_workspace_are_additive() {
        assert_eq!(
            serde_json::to_string(&SessionIntent::Standup).unwrap(),
            "\"standup\""
        );
        assert_eq!(
            serde_json::to_string(&SessionIntent::Enroll).unwrap(),
            "\"enroll\""
        );
        assert_eq!(
            serde_json::to_string(&SessionIntent::Login).unwrap(),
            "\"login\""
        );
        // The ENROLL body names the workspace ADDRESS; intent may default to absent.
        let enroll: DeviceAuthorizeRequest = serde_json::from_value(serde_json::json!({
            "workspace": "acme",
            "device_public_key": "AAAA",
            "machine_name": "laptop",
        }))
        .unwrap();
        assert_eq!(enroll.workspace.as_deref(), Some("acme"));
        assert!(enroll.intent.is_none());
        // The STANDUP body: no workspace, intent standup.
        let standup: DeviceAuthorizeRequest = serde_json::from_value(serde_json::json!({
            "intent": "standup",
            "device_public_key": "AAAA",
            "machine_name": "laptop",
        }))
        .unwrap();
        assert!(standup.workspace.is_none());
        assert_eq!(standup.intent, Some(SessionIntent::Standup));
        // The LOGIN body: no workspace, intent login.
        let login: DeviceAuthorizeRequest = serde_json::from_value(serde_json::json!({
            "intent": "login",
            "device_public_key": "AAAA",
            "machine_name": "laptop",
        }))
        .unwrap();
        assert!(login.workspace.is_none());
        assert_eq!(login.intent, Some(SessionIntent::Login));
        // An unknown intent is a CLOSED-enum parse failure, not a silent default.
        assert!(
            serde_json::from_value::<DeviceAuthorizeRequest>(serde_json::json!({
                "intent": "takeover",
                "device_public_key": "AAAA",
                "machine_name": "laptop",
            }))
            .is_err()
        );
    }

    #[test]
    fn device_authorize_response_standup_extras_are_optional() {
        // An OLD response (no complete URI, no plane block) still deserializes.
        let old: DeviceAuthorizeResponse = serde_json::from_value(serde_json::json!({
            "device_code": "dc",
            "user_code": "AAAA-BBBB",
            "verification_uri": "https://plane.test/verify",
            "expires_in": 900,
            "interval": 5,
        }))
        .unwrap();
        assert!(old.verification_uri_complete.is_none());
        assert!(old.plane.is_none());
    }

    #[test]
    fn passcode_ack_is_constant_shaped() {
        let ack = PasscodeAck {
            status: PasscodeAckStatus::Sent,
        };
        assert_eq!(serde_json::to_value(&ack).unwrap()["status"], "sent");
    }

    #[test]
    fn passcode_confirm_status_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&PasscodeConfirmStatus::TooManyAttempts).unwrap(),
            "\"too_many_attempts\""
        );
        assert_eq!(
            serde_json::to_string(&PasscodeConfirmStatus::WrongCode).unwrap(),
            "\"wrong_code\""
        );
    }

    #[test]
    fn redeem_response_carries_one_credential_and_no_user_token() {
        let resp = RedeemResponse {
            workspace_id: "w_acme".to_owned(),
            device_key_id: "dk_abc".to_owned(),
            principal: Some("alice@acme.com".to_owned()),
            credential: "wsc_secret".to_owned(),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["credential"], "wsc_secret");
        assert_eq!(v["principal"], "alice@acme.com");
        // NO user token field, ever — and no per-skill token list.
        assert!(v.get("user_token").is_none());
        assert!(v.get("token").is_none());
        assert!(v.get("read_creds").is_none());
        let back: RedeemResponse = serde_json::from_value(v).unwrap();
        assert_eq!(back.credential, "wsc_secret");
        // A response without the principal still deserializes (additive-compat).
        let old: RedeemResponse = serde_json::from_value(serde_json::json!({
            "workspace_id": "w_acme",
            "device_key_id": "dk_abc",
            "credential": "wsc_secret",
        }))
        .unwrap();
        assert!(old.principal.is_none());
    }

    #[test]
    fn skill_index_entry_carries_name_and_status() {
        let entry = WireSkillIndexEntry {
            skill_id: "s_prdescribe".to_owned(),
            name: "pr-describe".to_owned(),
            status: "active".to_owned(),
            version_id: "a".repeat(64),
            bundle_digest: "b".repeat(64),
            generation: Generation { epoch: 1, seq: 3 },
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
                display_name: Some("PR describe".to_owned()),
                protection: "reviewed".to_owned(),
                version_id: "a".repeat(64),
                bundle_digest: "b".repeat(64),
                generation: Generation { epoch: 1, seq: 7 },
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
    fn workspace_role_is_snake_case_and_invitation_bodies_round_trip() {
        assert_eq!(
            serde_json::to_string(&WorkspaceRole::Reviewer).unwrap(),
            "\"reviewer\""
        );
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
