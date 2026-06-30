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
use crate::results::ReviewDecision;
use serde::{Deserialize, Serialize};

/// A candidate file's mode on the wire — the two git regular-file modes as their literal octal strings
/// (`"100644"` / `"100755"`). A **closed** wire mirror of `topos_core::digest::FileMode`: that kernel enum
/// lives in a `no_std` crate `topos-types` does not depend on (and it carries no serde/schema derives), so
/// the wire leaf owns its own copy. The route handler maps it 1:1 at the edge —
/// `Regular ⇔ FileMode::Regular`, `Executable ⇔ FileMode::Executable` — for both the inbound candidate
/// ([`WireFile`]) and the outbound version metadata ([`WireVersionFile`]).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    utoipa::ToSchema,
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
pub struct PublishRequest {
    /// The target workspace id (the receipt + pointer scope).
    pub workspace_id: String,
    /// The target skill id within the workspace.
    pub skill_id: String,
    /// The client-minted UUIDv4 idempotency key — the same `op_id` replays the stored receipt byte-for-byte.
    #[schemars(extend("format" = "uuid"))]
    pub op_id: String,
    /// The id of the device key whose signature (in the header) authorizes this op.
    pub device_key_id: String,
    /// The `(epoch, seq)` this publish's compare-and-set targets; a stale pair yields `CONFLICT`.
    pub expected: Generation,
    /// The full candidate bundle to ingest + publish.
    pub candidate: WireCandidate,
}

/// `POST /v1/proposals` body — opens a proposal (a PR): ingests a full candidate **without moving
/// `current` or signing** (`NEEDS_REVIEW`). The authority's `propose` op takes the **same** input shape as
/// `publish` (candidate + device + `op_id` + `expected`); there is **no** separate title/body on the op (a
/// title/body, if ever surfaced, would be composed into the candidate's commit message).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
pub struct ProposeRequest {
    /// The target workspace id.
    pub workspace_id: String,
    /// The target skill id within the workspace.
    pub skill_id: String,
    /// The client-minted UUIDv4 idempotency key (replays the stored receipt on retry).
    #[schemars(extend("format" = "uuid"))]
    pub op_id: String,
    /// The id of the device key whose signature (in the header) authorizes this op.
    pub device_key_id: String,
    /// The `(epoch, seq)` the proposal is born against (its base); a stale base later makes it non-current.
    pub expected: Generation,
    /// The full candidate bundle to ingest as the proposal's content.
    pub candidate: WireCandidate,
}

/// `POST /v1/reverts` body — a **forward** revert: the server constructs a new 1-parent commit carrying the
/// `good` version's bytes on top of `current` (`seq` advances; the pointer never moves backward). There is
/// **no candidate** — the server reads `good`'s tree + digest from its provenance and builds the commit.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
pub struct RevertRequest {
    /// The target workspace id.
    pub workspace_id: String,
    /// The target skill id within the workspace.
    pub skill_id: String,
    /// The client-minted UUIDv4 idempotency key (replays the stored receipt on retry).
    #[schemars(extend("format" = "uuid"))]
    pub op_id: String,
    /// The id of the device key whose signature (in the header) authorizes this op.
    pub device_key_id: String,
    /// The `(epoch, seq)` this revert's compare-and-set targets; a stale pair yields `CONFLICT`.
    pub expected: Generation,
    /// The GOOD version (the `version_id` whose bytes are restored) as 64-char lowercase hex.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
pub struct ReviewRequest {
    /// The target workspace id.
    pub workspace_id: String,
    /// The target skill id within the workspace.
    pub skill_id: String,
    /// The client-minted UUIDv4 idempotency key (replays the stored receipt on retry).
    #[schemars(extend("format" = "uuid"))]
    pub op_id: String,
    /// The id of the device key whose signature (in the header) authorizes this op.
    pub device_key_id: String,
    /// The `(epoch, seq)` the approval's compare-and-set targets (the proposal's base); a stale pair on an
    /// `approve` yields `CONFLICT`.
    pub expected: Generation,
    /// The proposal being reviewed, named by its candidate commit id (64-char lowercase hex).
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub proposal: String,
    /// The verdict — `approve` (promote) or `reject`.
    pub decision: ReviewDecision,
}

/// One file of a version's metadata on the wire — its path, mode, and content id (`object_id`), mirroring
/// `plane-store`'s `VersionFile` with the id hex-encoded. The **bytes are NOT here**: a client fetches each
/// by `object_id` through the bundle (object) read route.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
pub struct WireVersionFile {
    /// The bundle-relative, forward-slash path.
    pub path: String,
    /// The file mode (regular or executable).
    pub mode: WireFileMode,
    /// The file's content id (64-char lowercase hex) — the handle the per-blob read route resolves.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub object_id: String,
}

/// `GET /v1/workspaces/{ws}/skills/{skill}/versions/{version_id}` response body — a version's authenticated
/// metadata: its id, the COMPLETE parent set, display author + message, the consent `bundle_digest`, and the
/// per-file `(path, mode, object_id)` leaves. Mirrors `plane-store`'s `VersionMeta` with every 32-byte id
/// hex-encoded. Assembled WITHOUT reading any blob bytes; the `bundle_digest` is the pin the client's
/// per-blob fetches + its own re-hash must reproduce.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
pub struct WireVersionMeta {
    /// This version's commit id (64-char lowercase hex).
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub version_id: String,
    /// The COMPLETE parent set, each a 64-char lowercase-hex commit id (`0` for genesis, `1` normally, `2`
    /// for an author merge).
    pub parents: Vec<String>,
    /// The author device id from the commit frame.
    pub author: String,
    /// The commit message (title + body as one string).
    pub message: String,
    /// The byte-exact consent hash over the bundle (64-char lowercase hex) — the fetch + re-hash pin.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub bundle_digest: String,
    /// The per-file leaves, in the version's recorded order.
    pub files: Vec<WireVersionFile>,
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
        };
        let v = serde_json::to_value(&req).unwrap();
        // snake_case field names, candidate nested, and the server-stamped time is absent.
        assert_eq!(v["workspace_id"], "w_demo");
        assert_eq!(v["expected"]["seq"], 42);
        assert_eq!(v["candidate"]["files"][0]["mode"], "100644");
        assert!(v.get("created_at").is_none());
        let back: PublishRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back.candidate.parents, vec!["a".repeat(64)]);
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
}
