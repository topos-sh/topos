//! On-disk persisted client documents under `~/.topos/`. Serde DTOs (no logic); each carries a
//! `schema_version` with an additive migration rule, and each is written atomically (temp → fsync →
//! rename → fsync dir; never mutated in place).
//!
//! The **load-bearing, spec-pinned** documents are typed here: `sync.json` (the durable currency
//! state — fully pinned), `lock.json`, `map.json`, and `ops/<op_id>.json`. The identity / instance /
//! harness-cache / log documents are pinned in *field-set* only (their exact sub-shapes are not), so
//! they land with the subsystems that own them rather than being frozen on a guess.
//!
//! Private keys are NEVER stored here — these JSON docs hold references and public metadata only.

use crate::{Generation, Receipt};
use serde::{Deserialize, Serialize};

/// `skills/<id>/sync.json` — the durable client sync state (the four-state currency machine's memory).
/// **Fully pinned.** The four states (CURRENT / BEHIND / DRAFT / DIVERGED) are *derived* from these
/// fields, never stored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SyncState {
    #[schemars(extend("const" = 1))]
    pub schema_version: u32,
    /// Highest AUTHENTICATED (signature-verified) generation ever seen — the anti-rollback floor and
    /// the retry target. Only a signed record raises it.
    pub observed: Generation,
    /// Highest generation actually MATERIALIZED — advances only after a successful swap (`≤ observed`).
    pub applied: Generation,
    /// Per observed generation, the commit it carried — a reused tuple with a *different* commit is a
    /// loud ALARM; same tuple + same commit is a no-op.
    #[serde(default)]
    pub recorded: Vec<RecordedTuple>,
    /// The commit the working tree derives from (= the applied commit when clean).
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub base_commit: String,
    /// sha256 (lowercase hex) of the current harness-dir bytes (recomputed; cheap).
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub work_hash: String,
    /// A transient local pin (a `pull <skill>@<hash>` go-back) suppressing one auto fast-forward.
    pub held: bool,
}

/// One `(generation → commit)` record in [`SyncState::recorded`]. (A list, not a JSON-object map,
/// because the key is the `(epoch,seq)` pair.)
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecordedTuple {
    pub generation: Generation,
    /// The commit (`version_id`) seen at this generation.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub commit_id: String,
}

/// `skills/<id>/lock.json` — the pinned skill identity + the byte-exact file list. **Pinned** (the
/// per-file `(path, mode, sha256, size)` tuple and the digest are frozen; the JSON spelling here is
/// the natural object form).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Lock {
    #[schemars(extend("const" = 1))]
    pub schema_version: u32,
    pub skill_id: String,
    pub name: String,
    /// The `version_id` (commit SHA-256) this lock is pinned to.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub base_commit: String,
    /// The byte-exact consent hash over the file list.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub bundle_digest: String,
    /// Files sorted by raw path bytes — the same ordering the canonical manifest uses.
    pub files: Vec<LockedFile>,
}

/// One file in [`Lock::files`]. `size` is OPERATIONAL only — it never enters the canonical manifest
/// or the digest (so the digest is placement-independent).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LockedFile {
    pub path: String,
    /// `100644` (regular) or `100755` (executable) — the only two allowed.
    #[schemars(extend("enum" = ["100644", "100755"]))]
    pub mode: String,
    /// The file's content sha256 (lowercase hex).
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub sha256: String,
    /// Size in bytes (operational metadata only).
    pub size: u64,
}

/// `skills/<id>/map.json` — where a skill is materialized + the hashes that drive no-op uninstall and
/// exact go-back. **Field-set pinned**; `swap_capability`'s value enum is INFERRED.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlacementMap {
    #[schemars(extend("const" = 1))]
    pub schema_version: u32,
    /// The target dir(s) where the skill is placed (project / global / per-category layers).
    pub placements: Vec<String>,
    /// The `version_id` currently realized on disk.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub applied_commit: String,
    /// sha256 of the bytes topos actually wrote (the projection sha) — may differ from the source
    /// `bundle_digest` if a harness ever projected; with no projection the two match.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub materialized_sha: String,
    /// sha256 of whatever was in the dir BEFORE placement — restored on uninstall (no-op uninstall).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub pre_existing_sha: Option<String>,
    pub swap_capability: SwapCapability,
}

/// Whether the placement dir supports an atomic swap, or must degrade. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SwapCapability {
    /// `renameat2(RENAME_EXCHANGE)` (Linux) / `renamex_np(RENAME_SWAP)` (macOS).
    AtomicExchange,
    /// A degraded rename-dance fallback (no single-syscall swap available).
    RenameDance,
    /// No safe atomic materialization — refuse or warn.
    Unsupported,
}

/// `ops/<op_id>.json` — the durable request identity, persisted BEFORE the first network send so an
/// uncertain write can be reconciled against the receipt. **Field-set pinned.**
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OpRecord {
    #[schemars(extend("const" = 1))]
    pub schema_version: u32,
    /// The client-minted UUIDv4 (also the filename).
    #[schemars(extend("format" = "uuid"))]
    pub op_id: String,
    /// The built commit (`version_id`) this op publishes / reverts / reviews.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub candidate_commit: String,
    /// The `(epoch,seq)` this op's compare-and-set targets.
    pub expected_generation: Generation,
    /// The stored terminal receipt, once one is known (the source of idempotent-retry truth).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_receipt: Option<Receipt>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_state_round_trips_with_recorded_tuples() {
        let s = SyncState {
            schema_version: 1,
            observed: Generation { epoch: 1, seq: 7 },
            applied: Generation { epoch: 1, seq: 7 },
            recorded: vec![RecordedTuple {
                generation: Generation { epoch: 1, seq: 7 },
                commit_id: "a".repeat(64),
            }],
            base_commit: "a".repeat(64),
            work_hash: "b".repeat(64),
            held: false,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["observed"]["seq"], 7);
        assert_eq!(v["recorded"][0]["commit_id"], "a".repeat(64));
        let back: SyncState = serde_json::from_value(v).unwrap();
        assert_eq!(back.applied.seq, 7);
        assert!(!back.held);
    }
}
