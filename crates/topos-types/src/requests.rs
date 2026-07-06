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
//! **The write credential rides in a header, not the body.** The 64-byte Ed25519 device signature travels
//! as the `Topos-Device-Signature` request header (base64url, 86 chars); the body carries only the
//! `device_key_id` that names the key. The `op` (publish / propose / revert / review-decision) is derived
//! from the route, never the body.
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

/// `POST /v1/publish` body — a direct publish that moves `current`. The device signature is the
/// `Topos-Device-Signature` header (not a body field); the server stamps `created_at`. Under
/// `review-required` the authority refuses this closed with `APPROVAL_REQUIRED`, ingesting nothing.
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
    /// The id of the device key whose signature (in the header) authorizes this op.
    pub device_key_id: String,
    /// The `(epoch, seq)` this publish's compare-and-set targets; a stale pair yields `CONFLICT`.
    pub expected: Generation,
    /// The full candidate bundle to ingest + publish.
    pub candidate: WireCandidate,
    /// The skill's human display name (the author's skill-folder name) — UNSIGNED ADVISORY metadata the
    /// plane stores last-writer-wins and serves for display only (a follower names its folder by it; the
    /// dashboard shows it). It is NOT part of the byte-exact bundle digest, the candidate, or the device-op
    /// signing preimage — a rename never changes a version id, digest, or signature. Absent ⇒ the plane
    /// keeps any existing name (never clobbered to NULL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// `POST /v1/proposals` body — opens a proposal (a PR): ingests a full candidate **without moving
/// `current` or signing** (`NEEDS_REVIEW`). The authority's `propose` op takes the **same** input shape as
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
    /// The id of the device key whose signature (in the header) authorizes this op.
    pub device_key_id: String,
    /// The `(epoch, seq)` the proposal is born against (its base); a stale base later makes it non-current.
    pub expected: Generation,
    /// The full candidate bundle to ingest as the proposal's content.
    pub candidate: WireCandidate,
    /// The skill's human display name (the author's skill-folder name) — UNSIGNED ADVISORY metadata, carried
    /// for symmetry with [`PublishRequest`]. It rides the proposal but is never signed, digested, or part of
    /// the candidate; the plane records a name only when the pointer actually moves (a later approve/publish).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
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
    /// The id of the device key whose signature (in the header) authorizes this op.
    pub device_key_id: String,
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
/// is a standalone status flip (nothing signed).
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
    /// The id of the device key whose signature (in the header) authorizes this op.
    pub device_key_id: String,
    /// The `(epoch, seq)` the approval's compare-and-set targets (the proposal's base); a stale pair on an
    /// `approve` yields `CONFLICT`.
    pub expected: Generation,
    /// The proposal being reviewed, named by its candidate commit id (64-char lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub proposal: String,
    /// The verdict — `approve` (promote) or `reject`.
    pub decision: ReviewDecision,
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

// =================================================================================================
// Enrollment request/response DTOs — the device-flow / passcode / redeem / admin-claim wire bodies.
//
// The enrollment credentials (device code, grant, read token, device public key) are OPAQUE strings,
// never trusted as ids: the server re-derives every id from the bytes (the device key id from the public
// key, the grant by its sha256). The enroll-frame possession signature rides the `Topos-Device-Signature`
// header on redeem, NOT the body. Field names are snake_case as written.
// =================================================================================================

/// A device-auth session's intent — a CLOSED set (snake_case). `enroll` joins an existing workspace through
/// an invite; `standup` starts with NO workspace (a signed-in human's approval creates one and seats the
/// approver as its first owner).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
#[serde(rename_all = "snake_case")]
pub enum SessionIntent {
    /// Join an existing workspace (the invite-anchored flow — the default when the field is absent).
    Enroll,
    /// Create a workspace on approval (the hosted self-serve first-boot flow).
    Standup,
}

/// `POST /v1/device/authorize` body — begin an RFC-8628 device-authorization flow. With an `invite_token`
/// (the default `enroll` intent) the device enrolls against that invite; with `intent = "standup"` — or no
/// invite token at all — it starts a STANDUP session (no workspace yet; hosted planes only). The server
/// SERVER-derives the device key id from `device_public_key` (a client-asserted id is never trusted).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceAuthorizeRequest {
    /// The opaque `/i/<token>` invite token the device enrolls against. Absent ⇒ a standup start (no invite
    /// exists before the workspace does).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_token: Option<String>,
    /// The session intent. Absent defaults to `enroll` when an invite token is present, `standup` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<SessionIntent>,
    /// The device's raw 32-byte Ed25519 public key, base64url-unpadded. The server derives the device key id
    /// from it (never a client-asserted id) and binds it into the enrollment possession frame.
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
    /// The short code a human types on the verification page.
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
    /// The plane block a STANDUP start carries (base URL, deployment posture, and the signing key to
    /// TOFU-pin) — a standup device has no `/i/` bootstrap to learn these from. Absent on an enroll start.
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

/// `POST /v1/workspaces/{ws}/devices` body — redeem an enrollment grant into a registered device + minted
/// per-skill read tokens. The enrollment possession signature rides the `Topos-Device-Signature` header
/// (NOT a body field); the server re-derives the device key id from `device_public_key`.
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
    /// The device's raw 32-byte Ed25519 public key, base64url-unpadded (must equal the grant's bound key).
    pub device_public_key: String,
}

/// `POST /v1/workspaces/{ws}/devices` success payload — the confirmed enrollment: the registered device and
/// the minted per-skill read tokens. **NO user token, ever.** Rides in the all-outcome envelope's `data`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct RedeemResponse {
    /// The workspace the device enrolled into.
    pub workspace_id: String,
    /// The server-derived device key id now registered.
    pub device_key_id: String,
    /// The principal the device now acts as (the confirmed email, or a device-rooted id) — a disclosure the
    /// client persists and prints so a hijacked standup is visible ("workspace X — owner Y").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// The minted per-skill read credentials (returned ONCE; only their sha256 is stored server-side).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_creds: Vec<RedeemedSkillCred>,
}

/// One minted per-skill read credential — the `0600` at-rest secret a follower stores to pull a skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct RedeemedSkillCred {
    /// The skill the token reads.
    pub skill_id: String,
    /// The plaintext read token (returned ONCE; only its sha256 is stored server-side).
    pub read_token: String,
    /// The token expiry in epoch-ms (`None` = non-expiring — per-device revoke is the kill switch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
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
    /// The claiming device's raw 32-byte Ed25519 public key, base64url-unpadded.
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
// Governance request bodies — owner/admin device-op-signed mutations. The governance-frame signature rides
// the `Topos-Device-Signature` header; the op is derived from the route + body. `op_id` is a UUIDv4.
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

/// One skill an invite pre-offers, with an optional display name (the name is NOT bound into the invite
/// signing frame — only the skill id is — so a rename never forks the deterministic invite link).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct InviteSkill {
    /// The offered skill id.
    pub skill_id: String,
    /// An optional display name to carry on the invite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `POST /v1/invites` body — mint an `/i/<token>` invite link, seeding the invited emails onto the roster
/// (omitted `role` defaults to `member`; the client must sign the same role byte). Returns
/// [`InviteData`](crate::results::InviteData) on success.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct InviteRequest {
    /// The target workspace id (bound into the governance signing frame).
    pub workspace_id: String,
    /// The client-minted UUIDv4 idempotency key (a retry replays the deterministic link + receipt).
    #[cfg_attr(feature = "contract-derives", schemars(extend("format" = "uuid")))]
    pub op_id: String,
    /// The id of the signing OWNER's device key (the registry selects the verifying key by this).
    pub device_key_id: String,
    /// The emails to invite (seeded onto the roster as `invited`), bound as a set in the signing frame.
    pub emails: Vec<String>,
    /// The role the invitees are granted — omitted defaults to `member` (the client signs the same byte).
    #[serde(default)]
    pub role: Option<WorkspaceRole>,
    /// The skills the invite pre-offers (each with an optional display name).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<InviteSkill>,
}

/// `PUT /v1/workspaces/{ws}/roster/{email}` body — set a principal's workspace role (owner-only). The target
/// principal is the `{email}` path segment; the role rides the body (bound into the signing frame).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct RosterSetRequest {
    /// The target workspace id (bound into the governance signing frame).
    pub workspace_id: String,
    /// The client-minted UUIDv4 idempotency key.
    #[cfg_attr(feature = "contract-derives", schemars(extend("format" = "uuid")))]
    pub op_id: String,
    /// The id of the signing owner's device key.
    pub device_key_id: String,
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
    /// The target workspace id (bound into the governance signing frame).
    pub workspace_id: String,
    /// The client-minted UUIDv4 idempotency key.
    #[cfg_attr(feature = "contract-derives", schemars(extend("format" = "uuid")))]
    pub op_id: String,
    /// The id of the signing owner's device key.
    pub device_key_id: String,
}

/// `DELETE /v1/workspaces/{ws}/devices` body — revoke a registered device key (owner, or the device's own
/// principal). The revoke is INSTANT (flip `revoked` + drop the device's read tokens in one transaction).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "contract-derives",
    derive(schemars::JsonSchema, utoipa::ToSchema)
)]
pub struct DeviceRevokeRequest {
    /// The target workspace id (bound into the governance signing frame).
    pub workspace_id: String,
    /// The client-minted UUIDv4 idempotency key.
    #[cfg_attr(feature = "contract-derives", schemars(extend("format" = "uuid")))]
    pub op_id: String,
    /// The id of the SIGNING device key (the actor; not the target).
    pub device_key_id: String,
    /// The id of the device key to revoke.
    pub target_device_key_id: String,
}

/// `PUT /v1/workspaces/{ws}/policy/review-required` body — the self-host operator toggle for the
/// `review-required` workspace policy (an idempotent set; JSON so the body stays extensible without a
/// path-shape change). Authenticated by the plane's admin token, not a device-op signature; the route is
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
            workspace_id: "w_demo".to_owned(),
            skill_id: "s_prdescribe".to_owned(),
            op_id: "f47ac10b-58cc-4372-a567-0e02b2c3d479".to_owned(),
            device_key_id: "dk_demo".to_owned(),
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
        // snake_case field names, candidate nested, and the server-stamped time is absent.
        assert_eq!(v["workspace_id"], "w_demo");
        assert_eq!(v["expected"]["seq"], 42);
        assert_eq!(v["candidate"]["files"][0]["mode"], "100644");
        assert_eq!(v["display_name"], "Deploy");
        assert!(v.get("created_at").is_none());
        let back: PublishRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back.candidate.parents, vec!["a".repeat(64)]);
        assert_eq!(back.display_name.as_deref(), Some("Deploy"));
        // An OLD body without display_name still deserializes (additive-compat), yielding None.
        let old: PublishRequest = serde_json::from_value(serde_json::json!({
            "workspace_id": "w_demo",
            "skill_id": "s_prdescribe",
            "op_id": "f47ac10b-58cc-4372-a567-0e02b2c3d479",
            "device_key_id": "dk_demo",
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
            }),
        };
        let v = serde_json::to_value(&granted).unwrap();
        assert_eq!(v["status"], "granted");
        assert_eq!(v["grant"], "g_opaque");
        assert_eq!(v["workspace"]["workspace_id"], "w_acme");
        assert_eq!(v["workspace"]["display_name"], "Acme");
        // An OLD response without the workspace block still deserializes (additive-compat).
        let old: DeviceTokenResponse =
            serde_json::from_value(serde_json::json!({ "status": "granted", "grant": "g" }))
                .unwrap();
        assert!(old.workspace.is_none());
    }

    #[test]
    fn device_authorize_request_intent_and_optional_invite_are_additive() {
        assert_eq!(
            serde_json::to_string(&SessionIntent::Standup).unwrap(),
            "\"standup\""
        );
        assert_eq!(
            serde_json::to_string(&SessionIntent::Enroll).unwrap(),
            "\"enroll\""
        );
        // The OLD enroll body (invite_token only) still parses; intent defaults to absent.
        let old: DeviceAuthorizeRequest = serde_json::from_value(serde_json::json!({
            "invite_token": "tok",
            "device_public_key": "AAAA",
            "machine_name": "laptop",
        }))
        .unwrap();
        assert_eq!(old.invite_token.as_deref(), Some("tok"));
        assert!(old.intent.is_none());
        // The STANDUP body: no invite token, intent standup.
        let standup: DeviceAuthorizeRequest = serde_json::from_value(serde_json::json!({
            "intent": "standup",
            "device_public_key": "AAAA",
            "machine_name": "laptop",
        }))
        .unwrap();
        assert!(standup.invite_token.is_none());
        assert_eq!(standup.intent, Some(SessionIntent::Standup));
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
    fn redeem_response_carries_read_creds_and_no_user_token() {
        let resp = RedeemResponse {
            workspace_id: "w_acme".to_owned(),
            device_key_id: "dk_abc".to_owned(),
            principal: Some("alice@acme.com".to_owned()),
            read_creds: vec![RedeemedSkillCred {
                skill_id: "s_deploy".to_owned(),
                read_token: "rt_secret".to_owned(),
                expires_at: None,
            }],
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["read_creds"][0]["skill_id"], "s_deploy");
        assert_eq!(v["principal"], "alice@acme.com");
        // NO user token field, ever.
        assert!(v.get("user_token").is_none());
        assert!(v.get("token").is_none());
        let back: RedeemResponse = serde_json::from_value(v).unwrap();
        assert_eq!(back.read_creds.len(), 1);
        // An OLD response without the principal still deserializes (additive-compat).
        let old: RedeemResponse = serde_json::from_value(serde_json::json!({
            "workspace_id": "w_acme",
            "device_key_id": "dk_abc",
        }))
        .unwrap();
        assert!(old.principal.is_none());
    }

    #[test]
    fn workspace_role_is_snake_case_and_invite_request_round_trips() {
        assert_eq!(
            serde_json::to_string(&WorkspaceRole::Reviewer).unwrap(),
            "\"reviewer\""
        );
        let req = InviteRequest {
            workspace_id: "w_acme".to_owned(),
            op_id: "f47ac10b-58cc-4372-a567-0e02b2c3d479".to_owned(),
            device_key_id: "dk_owner".to_owned(),
            emails: vec!["alice@acme.com".to_owned()],
            role: Some(WorkspaceRole::Member),
            skills: vec![InviteSkill {
                skill_id: "s_deploy".to_owned(),
                name: Some("Deploy".to_owned()),
            }],
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["role"], "member");
        assert_eq!(v["emails"][0], "alice@acme.com");
        let back: InviteRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back.skills[0].skill_id, "s_deploy");
        // An omitted role deserializes to None (the handler defaults it to member).
        let no_role: InviteRequest = serde_json::from_value(serde_json::json!({
            "workspace_id": "w_acme",
            "op_id": "f47ac10b-58cc-4372-a567-0e02b2c3d479",
            "device_key_id": "dk_owner",
            "emails": ["bob@acme.com"],
        }))
        .unwrap();
        assert!(no_role.role.is_none());
    }
}
