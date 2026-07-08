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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct SyncState {
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 1)))]
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
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub base_commit: String,
    /// sha256 (lowercase hex) of the current harness-dir bytes (recomputed; cheap).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub work_hash: String,
    /// A transient local pin (a `pull <skill>@<hash>` go-back) suppressing one auto fast-forward.
    pub held: bool,
}

/// One `(generation → commit)` record in [`SyncState::recorded`]. (A list, not a JSON-object map,
/// because the key is the `(epoch,seq)` pair.)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct RecordedTuple {
    pub generation: Generation,
    /// The commit (`version_id`) seen at this generation.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub commit_id: String,
}

/// `skills/<id>/lock.json` — the pinned skill identity + the byte-exact file list. **Pinned** (the
/// per-file `(path, mode, sha256, size)` tuple and the digest are frozen; the JSON spelling here is
/// the natural object form).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct Lock {
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 1)))]
    pub schema_version: u32,
    pub skill_id: String,
    pub name: String,
    /// The `version_id` (commit SHA-256) this lock is pinned to.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub base_commit: String,
    /// The byte-exact consent hash over the file list.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// Files sorted by raw path bytes — the same ordering the canonical manifest uses.
    pub files: Vec<LockedFile>,
}

/// One file in [`Lock::files`]. `size` is OPERATIONAL only — it never enters the canonical manifest
/// or the digest (so the digest is placement-independent).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct LockedFile {
    pub path: String,
    /// `100644` (regular) or `100755` (executable) — the only two allowed.
    #[cfg_attr(feature = "contract-derives", schemars(extend("enum" = ["100644", "100755"])))]
    pub mode: String,
    /// The file's content sha256 (lowercase hex).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub sha256: String,
    /// Size in bytes (operational metadata only).
    pub size: u64,
}

/// `skills/<id>/map.json` — where a skill is materialized + the hashes that drive no-op uninstall and
/// exact go-back. **Field-set pinned**; `swap_capability`'s value enum is INFERRED.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct PlacementMap {
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 1)))]
    pub schema_version: u32,
    /// The target dir(s) where the skill is placed (project / global / per-category layers).
    pub placements: Vec<String>,
    /// The `version_id` currently realized on disk.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub applied_commit: String,
    /// sha256 of the bytes topos actually wrote (the projection sha) — may differ from the source
    /// `bundle_digest` if a harness ever projected; with no projection the two match.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub materialized_sha: String,
    /// sha256 of whatever was in the dir BEFORE placement — restored on uninstall (no-op uninstall).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub pre_existing_sha: Option<String>,
    pub swap_capability: SwapCapability,
    /// The harness this skill was adopted into, when topos recognized one at adopt time (e.g. Claude
    /// Code); `None` for a plain directory tracked in place with no known harness. Drives where the
    /// currency trigger applies. **Additive optional** (a `None` placement omits it). v0 records exactly
    /// one placement, so this single tag is 1:1 with `placements`; a per-placement shape lands if/when a
    /// skill is ever placed across layers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<crate::HarnessId>,
    /// The harness layer the placement sits in (e.g. `"user"`), when a harness was recognized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_layer: Option<String>,
    /// The harness's registry slug (e.g. `claude-code`, `cursor`) the adopted dir was attributed to —
    /// recorded even when topos has no full adapter for it, so a later adapter can retroactively arm
    /// currency for an already-adopted skill. A superset of [`Self::harness`]: set whenever the source sits
    /// under a known harness skill dir. **Additive optional.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_slug: Option<String>,
}

/// Whether the placement dir supports an atomic swap, or must degrade. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SwapCapability {
    /// `renameat2(RENAME_EXCHANGE)` (Linux) / `renamex_np(RENAME_SWAP)` (macOS).
    AtomicExchange,
    /// A degraded rename-dance fallback (no single-syscall swap available).
    RenameDance,
    /// No safe atomic materialization — refuse or warn.
    Unsupported,
}

/// `skills/<id>/conflict.json` — the durable record that the working tree holds an **unresolved** author
/// merge conflict. **Field-set pinned** (additive; the value enums are INFERRED). This is the single
/// source of truth for the publish guard (presence ⇒ blocked — never a byte/marker scan) AND a pre-swap
/// recovery journal: it is written + fsynced BEFORE the conflict tree is swapped onto the placement, so a
/// crash mid-materialize is healed by rendering the already-committed `result_commit` (pinned by
/// `conflicted_digest`), never by re-merging on-disk marker bytes. Cleared only by a clean resolution (a
/// clean merge) or the disclosed escape — never by an incidental edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ConflictState {
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 1)))]
    pub schema_version: u32,
    /// The three-way base the conflict was computed against (the draft's fork point).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub base_commit: String,
    /// The base's `bundle_digest` — a render pin so recovery verifies offline without re-derivation.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub base_digest: String,
    /// `current` (theirs) at the time the conflict was recorded.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub current_commit: String,
    /// `current`'s `bundle_digest` — the render pin recovery uses to rebuild the `lock`-as-base.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub current_digest: String,
    /// The author's draft (mine) snapshot the conflict was computed from (recoverable).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub draft_commit: String,
    /// The draft's `bundle_digest` — a render pin for the recoverable draft.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub draft_digest: String,
    /// The conflict tree committed as a forward 1-parent commit on `current_commit` — the deterministic
    /// render target recovery re-materializes (so it never re-merges on-disk markers).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub result_commit: String,
    /// The `bundle_digest` of the conflict tree (= `result_commit`'s tree) — the on-disk heal signal and
    /// the `render_verified` pin. Disk re-scanning to this exact digest means "the materialize completed".
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub conflicted_digest: String,
    pub reason: ConflictReason,
    /// The conflicting paths, sorted by raw path bytes (the agent's resolution checklist).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<ConflictPath>,
}

/// Why a merge could not be applied cleanly. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ConflictReason {
    /// A genuine three-way merge with at least one unresolved path.
    ThreeWay,
    /// Unrelated histories — no recorded base; a 2-way manual choice is required.
    NoBase,
}

/// One conflicting path + how it conflicts. **Field-set pinned**; `kind`'s value set is INFERRED.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct ConflictPath {
    pub path: String,
    pub kind: ConflictPathKind,
}

/// How a single path conflicts. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ConflictPathKind {
    /// A textual three-way overlap — diff3 markers were written at the path.
    Content,
    /// A non-UTF-8 file with a true three-way divergence — theirs kept at the path, mine in a sidecar.
    BinaryContent,
    /// Mine modified the file; theirs deleted it — mine kept at the path.
    ModifyDelete,
    /// Mine deleted the file; theirs modified it — theirs kept at the path.
    DeleteModify,
    /// Both sides added the path with different content — theirs at the path, mine in a sidecar.
    AddAdd,
    /// A consent-significant mode disagreement — theirs' bytes + mode at the path.
    ModeMode,
    /// A side (or the merged output) exceeded the client size cap — theirs at the path, mine in a sidecar.
    Oversize,
}

/// The device-signed operation an [`OpRecord`] carries — a serde mirror of the kernel's `DeviceOp` (which
/// lives in `topos-core`, not a dependency of this crate). The client maps it 1:1 to `DeviceOp` when
/// re-signing a replayed op. snake_case on the wire/disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    /// `publish` that moves `current` directly (or genesis).
    PublishDirect,
    /// `publish --propose` that opens a proposal.
    PublishPropose,
    /// `revert --to <good>`.
    Revert,
    /// `review --approve` of a proposal.
    ReviewApprove,
    /// `review --reject` of a proposal.
    ReviewReject,
}

/// `ops/<op_id>.json` — the durable request identity, persisted (`0600`) BEFORE the first network send so
/// an uncertain write replays the SAME `op_id` (the server returns the byte-identical receipt — no
/// double-advance, no duplicate commit). It carries the full bound identity the device-op signature binds,
/// so a replay re-signs the identical frame. **Field-set pinned.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct OpRecord {
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 1)))]
    pub schema_version: u32,
    /// The client-minted UUIDv4 (also the filename).
    #[cfg_attr(feature = "contract-derives", schemars(extend("format" = "uuid")))]
    pub op_id: String,
    /// The workspace this op targets — part of the device-op bound identity.
    pub workspace_id: String,
    /// The skill this op targets — part of the device-op bound identity.
    pub skill_id: String,
    /// The operation kind (the device-op subtype is an integrity property — an approve never replays as a
    /// reject).
    pub op: OpKind,
    /// The built commit (`version_id`) this op publishes / reverts / reviews — bound by the signature.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub candidate_commit: String,
    /// The candidate's byte-exact bundle digest (the consent hash) — bound by the signature.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// The `(epoch,seq)` this op's compare-and-set targets — bound by the signature.
    pub expected_generation: Generation,
    /// The GOOD version a `revert` restores (the wire `good`) — present only for a `Revert` op (the
    /// server builds the forward commit from it; it is NOT the `candidate_commit`, so a replay must carry
    /// it). `None` for every other op.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub good: Option<String>,
    /// The skill's advisory display name (the author's folder name) sent alongside a publish/propose so a
    /// replay re-sends the identical value. UNSIGNED — it names the follower's folder + the dashboard entry,
    /// never the digest or the signed frame. `None` for a revert/review and for pre-existing WALs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
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
