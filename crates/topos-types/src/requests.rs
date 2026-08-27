//! Wire request/response DTOs for the product's public session-lane HTTP routes.
//!
//! The JSON bodies the app accepts on `publish` / `propose` / `revert` / `review`, the read bodies
//! (current / version metadata / proposals / catalog / delivery / describe), and the login-flow
//! start/poll pair. These are **deserialization shapes** only (no logic): the serving tier parses
//! them into validated domain types at the edge (parse-don't-validate), and every candidate byte is
//! **server-rehashed** — a client-supplied id or hash is never trusted.
//!
//! **No `created_at` on any request.** The server stamps the receipt's time from its own clock; a
//! client never supplies a wall clock (an ambient time would be a replay / skew lever).
//!
//! **The write credential is the session's workspace-scoped credential in the
//! `Authorization: Bearer` header — never a body field.** Keeping the secret out of the body keeps it out of receipt request identities and
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
    /// The candidate commit's declared parents, each a 64-char lowercase-hex `version_id`: `0` parents for a
    /// genesis publish, `1` for a normal publish / propose. More than one is REFUSED — a bundle's history is
    /// a list, never a DAG. Each must already be present in the workspace; a lie changes the recomputed
    /// commit id, so the server need not trust it.
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
    /// brand-new skill's reference still lands in `everyone` when its mode admits the caller; a
    /// curated `everyone` withholds a member's default placement, disclosed on the receipt — the
    /// publish itself lands catalog-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// The external-origin provenance this publish carries to the server (a bundle imported from
    /// GitHub remembers its parent — the fork-that-remembers model). Absent for ordinary local
    /// authorship. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<WireUpstream>,
    /// The catalog `kind` this publish declares for a BRAND-NEW bundle (`"mcp"` for an MCP-server
    /// bundle; absent ⇒ `"skill"`). Genesis-only: an existing bundle's kind is fixed at birth, and
    /// a publish naming a different kind is refused before any custody write. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// The upstream-provenance block a publish may carry: where the bundle's bytes originally came
/// from (`host`/`repo`, the in-repo `path`, the imported `commit`, a recorded license). The server
/// records it as the bundle's upstream link + the version's upstream commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireUpstream {
    /// The origin host (`github.com` — the only recognized value today).
    pub host: String,
    /// The `owner/repo` pair.
    pub repo: String,
    /// The bundle's path within the repo ("" / absent = the repo root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The upstream commit the imported bytes came from, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// A license identifier/filename recorded at import time, when found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
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
    /// The catalog `kind` this proposal declares for a BRAND-NEW bundle (same semantics as
    /// [`PublishRequest::kind`] — a reroute-to-proposal genesis still carries it). **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
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
    /// The COMPLETE parent set, each a 64-char lowercase-hex commit id: `0` entries for a bundle's genesis
    /// version, `1` for every other version. A bundle's history is a LIST by construction — the server
    /// refuses any candidate frame declaring more than one parent — so this never carries two.
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
    /// The catalog's bundle kind (`"skill"` · `"mcp"`). An OPEN string on the wire and a CLOSED
    /// vocabulary at the server that mints it. Clients BRANCH on it: the kind chooses the delivery
    /// mechanics — skill-dir placement, or the MCP config converge — so a client that does not
    /// know the kind a row names refuses that row rather than placing it as something else.
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
    /// The recorded upstream origin's host (`github.com` today) — present when the bundle was
    /// imported from an external source (the fork-that-remembers-its-parent provenance). Lets a
    /// client suggest the governed copy when the same source is added again. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_host: Option<String>,
    /// The upstream `owner/repo`. Present exactly when `upstream_host` is. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_repo: Option<String>,
    /// The subdirectory inside the upstream repo (`""` = the repo root). Present exactly when
    /// `upstream_host` is. **Additive.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_path: Option<String>,
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
    /// The workspace's file bundles (possibly empty).
    pub skills: Vec<WireSkillIndexEntry>,
    /// The workspace's connected MCP servers (possibly empty), each with the document it resolves
    /// to. They are a SEPARATE list because they are made of something else: a file bundle names a
    /// version to fetch, a connected server carries its document, and there is no byte lane behind
    /// one to fetch from.
    pub mcp_servers: Vec<WireMcpIndexEntry>,
}

/// One CONNECTED MCP SERVER of the workspace catalog, as `GET /v1/workspaces/{ws}/skills` returns
/// it — the same identity a delivered one carries, plus the catalog lifecycle `status` every index
/// row states.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireMcpIndexEntry {
    /// The bundle id (the `<skill>` path segment).
    pub skill_id: String,
    /// The catalog's user-facing name.
    pub name: String,
    /// The catalog's bundle kind — `"mcp"` for every row of this list.
    pub kind: String,
    /// The catalog lifecycle status — `"active"` / `"archived"`. An OPEN string, deliberately.
    pub status: String,
    /// The unsigned, advisory display name (may be absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The catalog revision this connection resolves to (see [`WireDeliveryMcpServer::revision_id`]).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^mcpr_[0-9a-f]{32}$")))]
    pub revision_id: String,
    /// The `server.json` verbatim, in the official registry format. ABSENT exactly when
    /// [`Self::withheld`] is present — a workspace that withholds a server hands out no document
    /// for it at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<serde_json::Value>,
    /// **THE WORKSPACE WITHHELD THIS SERVER FROM THIS CALLER** — an ANSWER, never a miss. The
    /// connection stands and the row is real (every identity field above is populated); what the
    /// workspace declines is handing this machine a document to reach the server with.
    ///
    /// Omitting the row instead would be indistinguishable from a fetch that did not land, and a
    /// client cannot tell "you may not have this" from "I could not ask" — so it would keep the
    /// entry it already holds, which is precisely the bypass this field exists to close.
    ///
    /// An OPEN string, deliberately: `"gateway_required"` (the workspace mandates its gateway for
    /// this server, and this caller has no gateway address to be given) is today's only value, and
    /// a client that does not recognise a value must still honour the WITHHOLD — an unknown token
    /// is the ruling all the same, and only shapes the sentence a person reads.
    ///
    /// A ruling must SAY something: a present-but-blank value is not one, and a client reads it as
    /// no withhold at all. That asymmetry is deliberate and runs toward safety. A withhold REMOVES
    /// entries, so an emitter that blanked this field by accident — an ORM writing `""` where it
    /// meant null, across every row — would wipe every MCP entry on every machine that asked. Far
    /// better to leave one server's entry standing than to strip a fleet's on a serializer's slip,
    /// so the blank falls through to the ordinary document read. Send the token, or omit the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withheld: Option<String>,
    /// This connection follows ONE revision rather than the server's current. Omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    /// The resolved revision was pulled back after publication. Omitted while it stands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked: Option<bool>,
    /// When the resolved revision was published (epoch milliseconds).
    pub updated_at: i64,
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
    /// The display name of the DIRECT assignment's creator, when someone else aimed the bundle
    /// at this person (or at everyone) — the attribution a receipt or status line narrates.
    /// Absent when the caller picked it themselves, or when only channels deliver it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_by: Option<String>,
    /// `true` when the caller's OWN pick (a self-assignment) delivers it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picked: Option<bool>,
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
    /// The catalog's bundle kind (`"skill"` · `"mcp"`). An OPEN string on the wire and a CLOSED
    /// vocabulary at the server that mints it. Clients BRANCH on it: the kind chooses the delivery
    /// mechanics — skill-dir placement, or the MCP config converge — so a client that does not
    /// know the kind a row names refuses that row rather than placing it as something else.
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

/// One CONNECTED MCP SERVER this device should have — **the document itself**, not a pointer to
/// bytes.
///
/// A `kind: "mcp"` bundle names a row in the server catalog: there is nothing content-addressed to
/// fetch and no second round trip to make, so the document rides the delivery it belongs to and the
/// machine caches it beside the revision it came from. `revision_id` is what the device reports
/// back as the thing it holds, where a file bundle reports a commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireDeliveryMcpServer {
    /// The bundle id (the `<skill>` path segment).
    pub skill_id: String,
    /// The catalog's user-facing name.
    pub name: String,
    /// The catalog's bundle kind — `"mcp"` for every row of this list. An OPEN string on the wire
    /// and a CLOSED vocabulary at the server that mints it; a client that does not know the kind a
    /// row names refuses that row rather than placing it as something else.
    pub kind: String,
    /// The unsigned, advisory display name; absent ⇒ show `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The catalog revision this connection resolves to — a pin, else the server's current. The
    /// catalog's own version handle (`mcpr_` + 32 lowercase hex), minted by the server that holds
    /// the revision; a client treats it as an opaque string and reports it back verbatim.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^mcpr_[0-9a-f]{32}$")))]
    pub revision_id: String,
    /// The `server.json` verbatim, in the official registry format.
    pub document: serde_json::Value,
    /// This connection follows ONE revision rather than the server's current. OMITTED when it does
    /// not — the wire spells absence by absence, never by `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    /// The resolved revision was pulled back after it was published. It is STILL SERVED — a pin is
    /// a promise — and the client discloses it. Omitted while the revision stands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked: Option<bool>,
    /// When the resolved revision was published (epoch milliseconds).
    pub updated_at: i64,
    /// Why this device is entitled to the server (channels ∪ direct).
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
    /// Always `2` for this contract version (the schema pins it `const`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 2)))]
    pub schema_version: u32,
    /// The workspace this delivery is scoped to (echoed from the path).
    pub workspace_id: String,
    /// The entitled file bundles — everything this device should have (a possibly-empty list).
    pub skills: Vec<WireDeliverySkill>,
    /// The entitled MCP servers, documents inline (a possibly-empty list). ONE feed, TWO lists:
    /// a bundle's kind decides only what an entitled row is MADE OF, and a connected server is
    /// made of its document where a file bundle is made of a version to fetch.
    pub mcp_servers: Vec<WireDeliveryMcpServer>,
    /// The caller's DECLINED bundles in this workspace (declined on the web — the everywhere
    /// stance). A machine's own manifest row may still deliver one; the client reads this list
    /// to disclose that honestly ("declined on the web, delivered here by your manifest").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declined: Vec<WireDeclined>,
    /// The unacked, person-scoped notices (verdicts, proposal closures, …).
    pub notices: Vec<WireNotice>,
    /// The count of OPEN, non-stale proposals across the entitled skills (the review-inbox pressure gauge).
    pub proposals_awaiting: u64,
    /// The workspace's staleness window (epoch milliseconds) — the ONE clock the fleet page and the
    /// client's hook warning both read: a device whose last report is older than this is stale.
    pub staleness_window_ms: u64,
    /// THIS session's standing — `"active"` or `"pending"`. A `"pending"` delivery carries empty
    /// `skills` / `notices` and `proposals_awaiting: 0` — no data flows over a pending session;
    /// the client skips the workspace quietly and `topos status` shows the wait.
    pub session_status: String,
}

/// One declined bundle in a [`WireDelivery`] — the caller's own web-recorded decline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireDeclined {
    /// The bundle's id.
    pub skill_id: String,
    /// The bundle's catalog name (joined for narration).
    pub name: String,
}

/// `POST /v1/workspaces/{ws}/mcp-servers` body — SUBMIT a server to this workspace. Exactly one of
/// the two fields is set, and the difference between them is the difference between two acts: a
/// `registry_name` the catalog already holds is a CONNECTION any member may make, and a
/// `document_url` is a server this workspace writes down as its OWN, which is an owner's.
///
/// The client sends the spelling a person typed and nothing else. Reading the document, deciding
/// whether it may be shared, and naming the bundle all happen at the server — one gate, one place,
/// one answer, whichever surface asked.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct McpAddRequest {
    /// Always `2` for this contract version (the schema pins it `const`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 2)))]
    pub schema_version: u32,
    /// The official registry identity of the server (`io.github.acme/weather`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_name: Option<String>,
    /// An https link to a `server.json` document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_url: Option<String>,
}

/// The `POST /v1/workspaces/{ws}/mcp-servers` success payload — what the workspace now shares.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct McpAddedData {
    /// The bundle id the connection is filed under.
    pub skill_id: String,
    /// The bundle name the workspace shares it as — the name every machine adds it by.
    pub name: String,
    /// The workspace wrote a server of its OWN down for this, rather than connecting one the
    /// catalog already held.
    pub created: bool,
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
    /// The version this device holds: a file bundle's commit (64-char lowercase hex), or a
    /// connected server's catalog revision (`mcpr_` + 32 lowercase hex). ONE field carries both
    /// because a report is one snapshot of what a machine holds, and what a bundle's version IS
    /// belongs to its kind.
    #[cfg_attr(
        feature = "contract-derives",
        schemars(extend("pattern" = "^([0-9a-f]{64}|mcpr_[0-9a-f]{32})$"))
    )]
    pub version_id: String,
    /// Per-harness applied states for a config-placed (`mcp`) bundle: which detected agents hold
    /// the entry and how (`state` is an OPEN vocabulary — `current` / `drifted` / `not-supported` /
    /// `unprovable`; a reader ignores a state it does not recognize). Empty for file-bundle
    /// skills. **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harnesses: Vec<WireHarnessState>,
}

/// One harness's applied state for a config-placed (`mcp`) bundle on this installation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct WireHarnessState {
    /// The harness registry slug (e.g. `claude-code`, `cursor`).
    pub slug: String,
    /// The state — an OPEN vocabulary (`current` / `drifted` / `not-supported` / `unprovable`).
    pub state: String,
    /// A short human-readable qualifier (why not-supported / what drifted), when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
    /// Always `2` for this contract version (the schema pins it `const`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 2)))]
    pub schema_version: u32,
    /// The device's applied skills (possibly empty — a device holding nothing reports an empty list).
    pub applied: Vec<WireAppliedSkill>,
}

// =================================================================================================
// Login-flow request/response DTOs — the gh-style browser-approval flow the APP serves (`POST
// /v1/login/authorize` + `POST /v1/login/token`; the field names keep their RFC-8628 spellings). An
// installation asks to log into THIS SERVER; the signed-in human chooses (or creates) the
// workspace in the browser and approves; the poll then MINTS and returns the SESSION's
// workspace-scoped bearer credential. The poll is the completion mechanism — a loopback flow's
// 127.0.0.1 redirect only wakes the waiting client, it carries no secret.
//
// Design fact: at the exchange the `device_code` (the flow code) itself is PROMOTED to the
// session's bearer credential server-side (the same sha256 stored twice — once as the flow row's
// code hash, once as the session credential hash), and the poll's `credential` field carries it
// back — so the CLI stores ONE secret from ONE field and no second mint/redeem round-trip exists.
// =================================================================================================

/// `POST /v1/login/authorize` body — begin a login flow toward this server. The workspace is
/// chosen (or created) at the browser approval, where the approver's seats are known; nothing
/// about workspaces or accounts is disclosed on this unauthenticated route.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceAuthStartRequest {
    /// A human-readable machine name shown on the approval page (a confused-deputy guard, not
    /// authority) and kept as the session's display name once approved.
    pub requested_name: String,
    /// The workspace ADDRESS slug a `login <workspace>` shortcut named — a PRESELECTION for the
    /// browser chooser, recorded shape-checked but unresolved (this unauthenticated start is
    /// never an existence oracle). The approval records the workspace the human actually chose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preselect: Option<String>,
    /// The invitation-link token a `login <invite-url>` carries. Recorded on the flow (as its
    /// hash) UNVALIDATED — this unauthenticated start is never a token oracle; the approval
    /// ceremony resolves it and weaves the invitation accept into its own fence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_token: Option<String>,
    /// How the approval outcome is ACCELERATED back — `"loopback"` when the client has bound a
    /// 127.0.0.1 listener and can open a browser on THIS machine, otherwise absent (the classic
    /// device grant). WRITE-ONCE on the flow: a loopback flow's approval page resolves from the
    /// URL-borne challenge with nothing typed, and the approval redirect wakes the listener; the
    /// poll remains the one completion mechanism either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect: Option<String>,
}

/// `POST /v1/login/authorize` response — the login-flow grant (RFC-8628-shaped names).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceAuthStartResponse {
    /// The SECRET flow code the client polls `login/token` with (the RFC-8628 field name). On
    /// approval this same secret is promoted to the session's bearer credential (see the module
    /// note), so it is stored like one.
    pub device_code: String,
    /// The short human-facing code the approval page displays (a cross-check, never typed as a
    /// secret).
    pub user_code: String,
    /// The approval URL a signed-in human visits. The code NEVER rides a URL: the client prints
    /// this bare address and the short code on separate lines, and the page's lookup is a POST.
    pub verification_uri: String,
    /// The flow lifetime, in seconds.
    pub expires_in_secs: u64,
    /// The minimum poll interval, in seconds.
    pub interval_secs: u64,
}

/// `POST /v1/login/token` body — poll a login flow for its outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceAuthPollRequest {
    /// The SECRET flow code from `login/authorize`.
    pub device_code: String,
}

/// A login-flow poll status (snake_case). `granted` carries the credential + the joined
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
    /// Approved and EXCHANGED — this poll minted (or re-reads) the session; `credential`,
    /// `session_id`, and `workspace` are present.
    Granted,
}

/// The workspace context a `granted` poll carries — everything the CLI needs to record what it
/// logged into (the id it scopes requests by, the address slug it joined at, and a display name).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceAuthWorkspace {
    /// The workspace id (the `{ws}` path segment of every subsequent request).
    pub workspace_id: String,
    /// The workspace's ADDRESS slug (what the human typed at `login`).
    pub name: String,
    /// The workspace's display name.
    pub display_name: String,
}

/// `POST /v1/login/token` response — the poll `status`; a `granted` poll carries the SESSION's
/// workspace-scoped bearer credential (the promoted flow code — returned here and stored from this
/// ONE field), the session id, and the joined workspace. A re-poll of an approved flow returns the
/// same answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceAuthPollResponse {
    /// The poll status.
    pub status: DeviceAuthPollStatus,
    /// The session's plaintext bearer credential — present ONLY when `status` is `granted`.
    /// Returned once per poll; the server stores only its sha256.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// The minted SESSION's id (a session = user × workspace × installation; the credential is
    /// workspace-scoped). Present ONLY when `status` is `granted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The session's born status — `"active"`, or `"pending"` while the workspace's
    /// session-approval knob holds it. Absent ⇒ treat as active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_status: Option<String>,
    /// The CHOSEN workspace — present ONLY when `status` is `granted`. This is what the browser
    /// approval recorded (a seat picked, an invitation accepted, or a workspace created there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<DeviceAuthWorkspace>,
}

/// `POST /v1/login/connect` body — the LANE-SIDE second connect: an already-credentialed machine
/// (any live session on this server) asks for a further workspace's session with no browser
/// round-trip. Seat standing is the trust basis — the server mints only where the acting user
/// already holds a seat; anything else is the uniform miss.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct LoginConnectRequest {
    /// The target workspace's ADDRESS slug.
    pub workspace: String,
    /// A human-readable machine name kept as the new session's display name (the same field the
    /// browser flow's start carries).
    pub requested_name: String,
}

/// `POST /v1/login/connect` response — the freshly minted session. `credential` is returned
/// exactly once, over this authenticated exchange; the server stores only its sha256.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct LoginConnectResponse {
    /// The new session's plaintext bearer credential (workspace-scoped).
    pub credential: String,
    /// The new session's id.
    pub session_id: String,
    /// The session's born status — `"active"`, or `"pending"` while the workspace's
    /// session-approval knob holds it.
    pub session_status: String,
    /// The connected workspace.
    pub workspace: DeviceAuthWorkspace,
}

// =================================================================================================
// Governance request bodies — owner/admin mutations. The ACTING device rides the `Authorization:
// Bearer` workspace credential (resolved in-transaction to its registry row; authority is the row +
// role matrix — never a body field); the op is derived from the route + body. `op_id` is a UUIDv4.
// =================================================================================================

/// `POST /v1/workspaces/{ws}/invitations` body — invitation as an INVITATION-ROW write: each email
/// becomes a pending 7-day claim redeemable through the single-use link the server MAILS (the token
/// never appears in this exchange — the mailbox is its one channel). At most ONE optional
/// first-destination hint (a skill or a channel of this workspace) rides the invitation and is
/// delivered by the accept. No role field — every CLI invitee starts as a member (roles are raised
/// later, on the web). Owner-only: only a workspace owner may send (and revoke) invitations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct InvitationRequest {
    /// The emails to seat as invited members (folded to the canonical lowercase form server-side).
    pub emails: Vec<String>,
    /// The first-destination SKILL hint (a bundle name in this workspace). At most one of
    /// `skill`/`channel` may be set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// The first-destination CHANNEL hint (a channel name in this workspace). At most one of
    /// `skill`/`channel` may be set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

/// `POST /v1/workspaces/{ws}/invitations` success `data` — what the inviter pastes onward: the
/// workspace ADDRESS. Deliberately NOT the tokened link: the link travels only in the invitation
/// mail, so the inviter's receipt can never stand in for the invitee's mailbox.
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
    /// Addresses NOT re-sent this time — each was invited repeatedly and sits on a server-side
    /// cooldown; `reason` is the server's own wording (display text). Omitted when empty and
    /// defaulted on the way in, so the field is additive across releases in both directions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedInvitation>,
}

/// One address an invitation submission SKIPPED rather than re-sent
/// ([`InvitationData::skipped`]) — not an error: the rest of the submission landed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct SkippedInvitation {
    /// The folded address that was skipped.
    pub email: String,
    /// Why, in the server's words (display text, e.g. `already invited recently`).
    pub reason: String,
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
    /// Always `2` for this contract version (the schema pins it `const`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 2)))]
    pub schema_version: u32,
    /// The constant discriminant a client dispatches on (`"topos-protocol-card"`).
    pub card: String,
    /// The API base URL the client re-roots onto (the origin serving `/v1`).
    pub api_base_url: String,
    /// The serving build's release version (no leading `v`; a candidate build's `-rc.N` suffix
    /// rides along — a consumer orders on the semver core). Absent from a producer that predates
    /// the field — a client reads such a card as "nothing declared" and proceeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
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
    /// THIS session's status — `"active"` or `"pending"` (a pending session awaits an owner's
    /// approval; no skill data flows over it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_status: Option<String>,
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
    /// Whether this is the structural `everyone` (delivered by default; a profile exclude is the
    /// opt-out).
    pub builtin: bool,
    /// Whether the CALLER's profile includes this channel (on `everyone`: whether no exclude
    /// stands).
    pub included: bool,
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
    /// (others') and to offer the author `--withdraw` instead of `--approve`. AUTHORITATIVE in both
    /// directions: a served `false` is a served "not yours", never overridden client-side.
    pub yours: bool,
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
    /// WHEN the version was committed (epoch milliseconds) — the same field name and units a
    /// `pull` event carries, so one history line reads the same whatever produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
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
    /// The catalog's bundle kind (`"skill"` · `"mcp"`) — the same fact the catalog and delivery
    /// carry: an OPEN string on the wire, a CLOSED vocabulary at the server that mints it. A log
    /// read renders it; the rows that DELIVER a bundle branch on it.
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
            upstream: None,
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
            kind: None,
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
            session_id: None,
            session_status: None,
            workspace: None,
        };
        let v = serde_json::to_value(&pending).unwrap();
        assert_eq!(v["status"], "pending");
        assert!(v.get("credential").is_none() && v.get("workspace").is_none());
        // A granted poll carries the ONE credential (the promoted device code), the session id,
        // and the CHOSEN workspace — everything the CLI stores, from one field each.
        let granted = DeviceAuthPollResponse {
            status: DeviceAuthPollStatus::Granted,
            credential: Some("dc_secret".to_owned()),
            session_id: Some("sn_1".to_owned()),
            session_status: Some("pending".to_owned()),
            workspace: Some(DeviceAuthWorkspace {
                workspace_id: "w_acme".to_owned(),
                name: "acme".to_owned(),
                display_name: "Acme".to_owned(),
            }),
        };
        let v = serde_json::to_value(&granted).unwrap();
        assert_eq!(v["status"], "granted");
        assert_eq!(v["credential"], "dc_secret");
        assert_eq!(v["session_id"], "sn_1");
        assert_eq!(v["workspace"]["name"], "acme");
        // The session's born status rides the grant (the exchange mints per the one rule).
        assert_eq!(v["session_status"], "pending");
        // The start pair round-trips; the poll target is the same secret the start returned.
        let start = DeviceAuthStartResponse {
            device_code: "dc_secret".to_owned(),
            user_code: "AAAA-BBBB".to_owned(),
            verification_uri: "https://topos.example/verify".to_owned(),
            expires_in_secs: 900,
            interval_secs: 5,
        };
        let v = serde_json::to_value(&start).unwrap();
        assert_eq!(v["expires_in_secs"], 900);
        assert_eq!(v["interval_secs"], 5);
        // A bare start names no workspace (the browser chooser decides); a `login <ws>` shortcut
        // rides `preselect`.
        let req: DeviceAuthStartRequest = serde_json::from_value(serde_json::json!({
            "requested_name": "laptop",
        }))
        .unwrap();
        assert_eq!(req.preselect, None);
        let req: DeviceAuthStartRequest = serde_json::from_value(serde_json::json!({
            "requested_name": "laptop",
            "preselect": "acme",
        }))
        .unwrap();
        assert_eq!(req.preselect.as_deref(), Some("acme"));
        let poll: DeviceAuthPollRequest =
            serde_json::from_value(serde_json::json!({ "device_code": "dc_secret" })).unwrap();
        assert_eq!(poll.device_code, "dc_secret");
    }

    #[test]
    fn login_connect_round_trips_the_lane_side_second_connect() {
        let req: LoginConnectRequest = serde_json::from_value(serde_json::json!({
            "workspace": "beta",
            "requested_name": "laptop",
        }))
        .unwrap();
        assert_eq!(req.workspace, "beta");
        assert_eq!(req.requested_name, "laptop");
        let resp = LoginConnectResponse {
            credential: "cs_secret".to_owned(),
            session_id: "sn_2".to_owned(),
            session_status: "active".to_owned(),
            workspace: DeviceAuthWorkspace {
                workspace_id: "w_beta".to_owned(),
                name: "beta".to_owned(),
                display_name: "Beta".to_owned(),
            },
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["credential"], "cs_secret");
        assert_eq!(v["session_id"], "sn_2");
        assert_eq!(v["session_status"], "active");
        assert_eq!(v["workspace"]["workspace_id"], "w_beta");
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
            upstream_host: None,
            upstream_repo: None,
            upstream_path: None,
        };
        let v = serde_json::to_value(&entry).unwrap();
        // The new fields ride under their snake_case spellings.
        assert_eq!(v["name"], "pr-describe");
        assert_eq!(v["status"], "active");
        // An absent display_name omits (skip_serializing_if), as do the upstream fields.
        assert!(v.get("display_name").is_none());
        assert!(v.get("upstream_host").is_none());
        let back: WireSkillIndexEntry = serde_json::from_value(v).unwrap();
        assert_eq!(back.name, "pr-describe");
        assert_eq!(back.status, "active");
    }

    #[test]
    fn delivery_round_trips_snake_case_and_omits_absent_notice_fields() {
        let delivery = WireDelivery {
            schema_version: crate::WIRE_SCHEMA_VERSION,
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
                    assigned_by: Some("Alice".to_owned()),
                    picked: None,
                },
            }],
            mcp_servers: vec![WireDeliveryMcpServer {
                skill_id: "s_weather".to_owned(),
                name: "weather".to_owned(),
                kind: "mcp".to_owned(),
                display_name: None,
                revision_id: format!("mcpr_{}", "c".repeat(32)),
                document: serde_json::json!({ "name": "io.github.acme/weather" }),
                pinned: Some(true),
                revoked: None,
                updated_at: 1_700_000_000_001,
                via: WireVia {
                    channels: Vec::new(),
                    direct: true,
                    assigned_by: None,
                    picked: Some(true),
                },
            }],
            declined: vec![WireDeclined {
                skill_id: "s_old".to_owned(),
                name: "old-skill".to_owned(),
            }],
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
            session_status: "active".to_owned(),
        };
        let v = serde_json::to_value(&delivery).unwrap();
        assert_eq!(v["schema_version"], crate::WIRE_SCHEMA_VERSION);
        assert_eq!(v["workspace_id"], "w_demo");
        assert_eq!(v["skills"][0]["skill_id"], "s_prdescribe");
        assert_eq!(v["skills"][0]["protection"], "reviewed");
        // `via` carries the channels + direct flag under snake_case, plus the attribution pair
        // (absent halves omit — never null).
        assert_eq!(v["skills"][0]["via"]["channels"][0], "everyone");
        assert_eq!(v["skills"][0]["via"]["direct"], true);
        assert_eq!(v["skills"][0]["via"]["assigned_by"], "Alice");
        assert!(v["skills"][0]["via"].get("picked").is_none());
        // The declined list rides id + name pairs.
        assert_eq!(v["declined"][0]["skill_id"], "s_old");
        assert_eq!(v["declined"][0]["name"], "old-skill");
        // The SECOND list: a connected server carries its document, not a version to fetch, and
        // its two flags spell absence by absence.
        assert_eq!(v["mcp_servers"][0]["skill_id"], "s_weather");
        assert_eq!(
            v["mcp_servers"][0]["document"]["name"],
            "io.github.acme/weather"
        );
        assert_eq!(v["mcp_servers"][0]["pinned"], true);
        assert!(v["mcp_servers"][0].get("revoked").is_none());
        assert!(v["mcp_servers"][0].get("display_name").is_none());
        assert_eq!(v["notices"][0]["kind"], "verdict");
        assert_eq!(v["notices"][0]["reason"], "looks good");
        // Absent optional notice fields omit (skip_serializing_if) — never serialized as null.
        assert!(v["notices"][0].get("version_id").is_none());
        assert!(v["notices"][0].get("message").is_none());
        // The ONE staleness clock rides every delivery.
        assert_eq!(v["staleness_window_ms"], 604_800_000_u64);
        assert_eq!(v["session_status"], "active");
        let back: WireDelivery = serde_json::from_value(v).unwrap();
        assert_eq!(back.skills.len(), 1);
        assert_eq!(back.skills[0].via.channels, vec!["everyone".to_owned()]);
        assert_eq!(back.mcp_servers[0].name, "weather");
        assert_eq!(back.mcp_servers[0].pinned, Some(true));
        assert_eq!(back.mcp_servers[0].revoked, None);
        assert_eq!(back.proposals_awaiting, 2);
        // A PENDING delivery (no data flows over a pending session) still round-trips — empty
        // sets, a zero gauge, and the pending status the client's sweep skips on; the absent
        // `declined` defaults empty.
        let pending: WireDelivery = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "workspace_id": "w_demo",
            "skills": [],
            "mcp_servers": [],
            "notices": [],
            "proposals_awaiting": 0,
            "staleness_window_ms": 604800000,
            "session_status": "pending",
        }))
        .unwrap();
        assert!(pending.skills.is_empty() && pending.notices.is_empty());
        assert!(pending.mcp_servers.is_empty());
        assert!(pending.declined.is_empty());
        assert_eq!(pending.session_status, "pending");
    }

    #[test]
    fn applied_report_round_trips_snake_case() {
        let report = WireAppliedReport {
            schema_version: crate::WIRE_SCHEMA_VERSION,
            applied: vec![WireAppliedSkill {
                skill_id: "s_prdescribe".to_owned(),
                version_id: "c".repeat(64),
                harnesses: Vec::new(),
            }],
        };
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["schema_version"], crate::WIRE_SCHEMA_VERSION);
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
        // The invitation body: emails + at most one first-destination hint, no role field (every
        // CLI invitee starts as a member) and no token anywhere (the link travels only in mail).
        let req = InvitationRequest {
            emails: vec!["alice@acme.com".to_owned()],
            skill: None,
            channel: Some("ops".to_owned()),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["emails"][0], "alice@acme.com");
        assert_eq!(v["channel"], "ops");
        assert!(v.get("skill").is_none(), "an unset hint omits");
        // The acting device rides the Authorization header — never a body field.
        assert!(v.get("device_key_id").is_none() && v.get("credential").is_none());
        assert!(v.get("role").is_none(), "invitations mint members only");
        assert!(
            v.get("token").is_none(),
            "no token ever rides this exchange"
        );
        // The hints omit when unset (skip_serializing_if) and default on the way in.
        let bare: InvitationRequest =
            serde_json::from_value(serde_json::json!({ "emails": ["bob@acme.com"] })).unwrap();
        assert!(bare.skill.is_none() && bare.channel.is_none());
        let data = InvitationData {
            address: "https://topos.example/acme".to_owned(),
            invited: vec!["alice@acme.com".to_owned()],
            mailed: false,
            skipped: Vec::new(),
        };
        let v = serde_json::to_value(&data).unwrap();
        assert_eq!(v["address"], "https://topos.example/acme");
        assert_eq!(v["mailed"], false);
        // `skipped` omits when empty and defaults on the way in — additive both ways, so an
        // older server's answer (no field) and an older client's parse both keep working.
        assert!(v.get("skipped").is_none());
        let back: InvitationData = serde_json::from_value(v).unwrap();
        assert!(back.skipped.is_empty());
        let with: InvitationData = serde_json::from_value(serde_json::json!({
            "address": "https://topos.example/acme",
            "invited": [],
            "mailed": false,
            "skipped": [{ "email": "bob@acme.com", "reason": "already invited recently" }],
        }))
        .unwrap();
        assert_eq!(with.skipped[0].email, "bob@acme.com");
        assert_eq!(with.skipped[0].reason, "already invited recently");
    }

    #[test]
    fn the_protocol_card_carries_its_version_declaration_and_reads_without_one() {
        // The two declarations ride the constant card so a client can judge, before it commits to
        // a login, whether the two ends still speak the same wire.
        let card = WireProtocolCard {
            schema_version: crate::WIRE_SCHEMA_VERSION,
            card: "topos-protocol-card".to_owned(),
            api_base_url: "https://topos.example/api".to_owned(),
            server_version: Some("0.1.20".to_owned()),
        };
        let v = serde_json::to_value(&card).unwrap();
        assert_eq!(v["server_version"], "0.1.20");
        let back: WireProtocolCard = serde_json::from_value(v).unwrap();
        assert_eq!(back.server_version.as_deref(), Some("0.1.20"));
        // A card that declares no version still parses — the field is absent, never zero-valued,
        // and an undeclared version is never read as a claim about anything. (The card a server
        // serves may carry further keys this build does not read; they are ignored, not refused.)
        let old: WireProtocolCard = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "card": "topos-protocol-card",
            "api_base_url": "https://topos.example/api",
            "min_cli_version": "0.1.15",
        }))
        .unwrap();
        assert!(old.server_version.is_none());
        // …and an undeclared version omits on the way out, so the card stays byte-identical to
        // what it has always been.
        let v = serde_json::to_value(&old).unwrap();
        assert!(v.get("server_version").is_none());
    }
}
