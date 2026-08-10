//! On-disk persisted client documents under `~/.topos/`. Serde DTOs (no logic); each carries a
//! `schema_version` with an additive migration rule, and each is written atomically (temp → fsync →
//! rename → fsync dir; never mutated in place).
//!
//! The **load-bearing, spec-pinned** documents are typed here: `sync.json` (the durable update-status
//! state — fully pinned), `lock.json`, `map.json`, and `ops/<op_id>.json`. The identity / instance /
//! harness-cache / log documents are pinned in *field-set* only (their exact sub-shapes are not), so
//! they land with the subsystems that own them rather than being frozen on a guess.
//!
//! Private keys are NEVER stored here — these JSON docs hold references and public metadata only.

use crate::Receipt;
use serde::{Deserialize, Serialize};

/// `skills/<id>/sync.json` — the durable client sync state (the four-state sync machine's memory).
/// **Fully pinned.** The four states (CURRENT / BEHIND / DRAFT / DIVERGED) are *derived* from
/// `observed`/`applied` vs the working tree, never stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct SyncState {
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 1)))]
    pub schema_version: u32,
    /// The generation the plane most recently served — the sync target the engine drives `applied`
    /// toward.
    pub observed: u64,
    /// The `version_id` (the 64-hex commit) the served `observed` generation named — the target bytes
    /// the engine materializes and re-verifies by digest on apply.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub observed_version_id: String,
    /// Highest generation actually MATERIALIZED — advances only after a successful swap. After a server
    /// restore the served target may sit below this; that is a legitimate state the engine simply
    /// applies toward.
    pub applied: u64,
    /// The commit the working tree derives from (= the applied commit when clean).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub base_commit: String,
    /// sha256 (lowercase hex) of the current harness-dir bytes (recomputed; cheap).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub work_hash: String,
    /// A transient local pin (a `pull <skill>@<hash>` go-back) suppressing one auto fast-forward.
    pub held: bool,
    /// The draft work-tree digest the previous run OBSERVED for this bundle+scope (sha256 hex of
    /// the one edited copy's bytes), or absent when no draft was standing. The settled-draft
    /// fan-out compares the current draft against it: unchanged across two runs means SETTLED, and
    /// only a settled draft is copied onto the bundle's other placements in its scope — a mid-edit
    /// file never spreads. **Additive optional** (absent in older documents; cleared when the
    /// draft resolves).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub draft_observed: Option<String>,
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

/// `skills/<id>/map.json` — **ONE bundle's whole ownership record**: every target it owns on this
/// machine, in both shapes a target comes in, plus the hashes that drive no-op uninstall and exact
/// go-back. **Field-set pinned**; `swap_capability`'s value enum is INFERRED.
///
/// A target is either a DIRECTORY the bundle owns ([`Self::placements`] × [`Self::placement_state`],
/// written by the dir materializer) or the ENTRIES it owns inside a shared config file
/// ([`Self::entry_state`], written by the config converge). The two lists are the two delivery
/// mechanics; the record is one, so "what does this bundle own here" has exactly one answer whatever
/// its kind.
///
/// **Schema v3** (its OWN ceiling, [`crate::PLACEMENT_MAP_SCHEMA_VERSION`]): v2 gave each dir
/// placement its own durable state, strictly 1:1 with [`Self::placements`]; v3 added the config-entry
/// half. A v1 document (one placement, map-level state only) upgrades losslessly in memory on read, a
/// v2 one reads as owning no config entries; the map-level `materialized_sha` / `pre_existing_sha` /
/// `swap_capability` fields remain the FIRST dir placement's mirror, so the document stays legible to
/// inspection tools that predate the per-placement shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct PlacementMap {
    #[cfg_attr(feature = "contract-derives", schemars(extend("const" = 3)))]
    pub schema_version: u32,
    /// The target dir(s) where the skill is placed (the shared cross-agent dir and/or per-harness
    /// native dirs), 1:1 with [`Self::placement_state`].
    pub placements: Vec<String>,
    /// The `version_id` currently realized on disk.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub applied_commit: String,
    /// sha256 of the bytes topos actually wrote (the projection sha) — may differ from the source
    /// `bundle_digest` if a harness ever projected; with no projection the two match. The FIRST
    /// placement's mirror in a v2 document (the per-placement truth is `placement_state`).
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub materialized_sha: String,
    /// sha256 of whatever was in the dir BEFORE placement — restored on uninstall (no-op uninstall).
    /// The FIRST placement's mirror in a v2 document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub pre_existing_sha: Option<String>,
    /// The FIRST placement's swap capability (the per-placement truth is `placement_state`).
    pub swap_capability: SwapCapability,
    /// Per-placement durable state, strictly 1:1 with [`Self::placements`] after the read upgrade.
    /// Empty on disk only in a v1 document (the reader synthesizes the single entry from the
    /// map-level fields).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub placement_state: Vec<PlacementState>,
    /// The harness this skill was adopted into, when topos recognized one at adopt time (e.g. Claude
    /// Code); `None` for a plain directory tracked in place with no known harness. Drives where the
    /// auto-update trigger applies. **Additive optional** (a `None` placement omits it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<crate::HarnessId>,
    /// The harness layer the placement sits in (e.g. `"user"`), when a harness was recognized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_layer: Option<String>,
    /// The harness's registry slug (e.g. `claude-code`, `cursor`) the adopted dir was attributed to —
    /// recorded even when topos has no full adapter for it, so a later adapter can retroactively arm
    /// auto-updates for an already-adopted skill. A superset of [`Self::harness`]: set whenever the source sits
    /// under a known harness skill dir. **Additive optional.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_slug: Option<String>,
    /// The CONFIG-FILE targets this bundle owns — one row per entry topos wrote into an agent's own
    /// config. The second half of the ownership record (see the type doc); empty for every bundle
    /// delivered as directories, and absent from a pre-v3 document. **Additive.**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry_state: Vec<EntryPlacement>,
}

/// One CONFIG-ENTRY placement's durable state — the ownership record for entries topos wrote into a
/// shared config file. The exact counterpart of [`PlacementState`] for the other target shape:
/// `fingerprint` is to an entry what `materialized_sha` is to a dir (what topos last wrote, so drift
/// is this row against the file, judged independently per row). **Field-set pinned; additive.**
///
/// Ownership of an entry is proven by the `topos-` key prefix PLUS this row: the config drivers are
/// pure, so a `topos-`-looking key with no row here is FOREIGN and is never touched or claimed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct EntryPlacement {
    /// The registry slug of the harness whose config file holds this entry.
    pub agent: String,
    /// The config file the entry lives in. A row recorded at another file is not custody of THIS
    /// surface: a surface path that moves leaves a disclosed stale row rather than re-pointed custody.
    pub file: String,
    /// The immutable config key topos minted for this bundle. Once minted the key never changes —
    /// several harnesses key OAuth tokens to the server name, so a rename would strand a sign-in.
    pub key: String,
    /// The fingerprint topos last wrote (the drivers' drift baseline).
    pub fingerprint: String,
    /// Whole-file ownership: topos created the file and still owned every byte at the last write —
    /// the precondition for deleting the file when the last entry leaves. **Additive.**
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub owns_file: bool,
    /// The bundle version whose `server.json` the entry was rendered from (provenance). **Additive.**
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version_id: String,
}

/// One placement's durable state (`map.json` v2), 1:1 with [`PlacementMap::placements`]. **Field-set
/// pinned**; the `kind` value set is INFERRED.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
pub struct PlacementState {
    /// Whether this dir is the shared cross-agent skills dir or one harness's own skills dir.
    pub kind: PlacementKind,
    /// The registry slug of the harness a `native` placement serves; `None` for `shared` (and for a
    /// plain adopted dir under no known harness).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// sha256 of the bytes topos wrote into THIS dir; `None` = the target is recorded but was never
    /// materialized (a newly added placement awaiting its first apply). Draft detection compares each
    /// dir against ITS recorded sha, so per-dir drift is classified independently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub materialized_sha: Option<String>,
    /// sha256 of whatever was in THIS dir BEFORE topos first wrote into it (sticky).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub pre_existing_sha: Option<String>,
    /// The swap capability probed for THIS dir's filesystem.
    pub swap_capability: SwapCapability,
    /// THIS dir is the user's own adopted-in-place SOURCE (`add <path>` recorded it where it
    /// stood; topos never created it). The cleaners treat it as never-deletable: when demand for
    /// the bundle ends, every placement topos wrote retires, but the adopted source dir survives
    /// byte-identical. Sticky for the life of the record. **Additive.**
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub adopted_source: bool,
}

/// What a placement dir IS — the shared cross-agent convention dir, or one harness's native skills
/// dir. **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PlacementKind {
    /// The shared cross-agent skills dir (one copy, read by every covered harness).
    Shared,
    /// One harness's own skills dir.
    Native,
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

/// `skills/<id>/conflict.json` — the durable record that this bundle holds a **stopped** author
/// merge. **Field-set pinned** (additive; the value enums are INFERRED). This is the single
/// source of truth for the publish guard (presence ⇒ blocked — never a byte/marker scan) AND a
/// recovery journal: it is written + fsynced BEFORE the marked-up copy is written to the scope's own
/// `conflicts/<copy_dir>/`, so a crash mid-write is healed by re-rendering the already-committed
/// `result_commit` (pinned by `conflicted_digest`), never by re-merging on-disk marker bytes. The
/// agent-readable placements are NOT touched by a conflict — they keep the author's own version —
/// so nothing an agent reads is ever a half-state. Presence means the merge is LIVE unless
/// [`Self::concluded`] marks the exit that ended it (the mark is what makes a crashed exit
/// recoverable); the record is removed only by a LANDED conclusion — a clean re-merge, the
/// disclosed escape, or a reset — never by an incidental edit, and never on a document comparison.
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
    /// The `bundle_digest` of the conflict tree (= `result_commit`'s tree) — the `render_verified`
    /// pin, and the UNTOUCHED signal: a `conflicts/<copy_dir>/` still scanning to exactly this
    /// digest is a copy nobody has hand-resolved, so the escape commits the original draft instead.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub conflicted_digest: String,
    /// The single directory component under this scope's `conflicts/` dir holding the marked-up
    /// copy (`~/.topos/conflicts/<copy_dir>/`, or the project store's equivalent). Chosen once when
    /// the conflict is recorded — the bundle's name, disambiguated like a skill dir — and read back
    /// by every exit, so the folder a receipt names is the folder the escape reads and the
    /// resolution deletes. **Additive optional**: an absent (or unparseable) value means the record
    /// names no folder, and nothing is ever written, scanned, or removed for it — there is
    /// deliberately no fallback derivation from the bundle's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_dir: Option<String>,
    pub reason: ConflictReason,
    /// The exit that CONCLUDED this merge, written durably BEFORE that exit mutates any placement —
    /// the `MERGE_HEAD` discipline: the record's presence says a merge stopped, this mark says
    /// which exit ended it, and the record's removal says the conclusion landed. A record without
    /// it is a LIVE stopped merge, full stop — no document comparison ever decides liveness; a
    /// marked record is finished idempotently by the next run. **Additive optional** (absent in
    /// every record a stop writes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concluded: Option<ConcludedExit>,
    /// The conflicting paths, sorted by raw path bytes (the agent's resolution checklist).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<ConflictPath>,
}

/// Which exit concluded a stopped merge (see [`ConflictState::concluded`]). **INFERRED value set.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-derives", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ConcludedExit {
    /// `--keep-mine` — the author's chosen tree committed on `current`.
    Escape,
    /// `update --reset` — the merge resolved the team's way, every copy back to base.
    Reset,
    /// A clean re-merge landed over a (defensively) stale record.
    Merge,
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

/// The device operation an [`OpRecord`] carries — the durable record of a replayed write's kind (the op
/// kind an idempotent retry re-sends; the kind otherwise rides the route). snake_case on the wire/disk.
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
    /// `review --withdraw` — the author retracting their own proposal. A DISTINCT kind from
    /// `ReviewReject` so a crashed reject and a fresh withdraw (or vice versa) can never replay each
    /// other's stored receipt under a reused op id.
    ReviewWithdraw,
}

/// `ops/<op_id>.json` — the durable request identity, persisted (`0600`) BEFORE the first network send so
/// an uncertain write replays the SAME `op_id` (the server returns the byte-identical receipt — no
/// double-advance, no duplicate commit). It carries the full bound identity of the op — the workspace,
/// skill, kind, candidate commit, digest, and expected generation — so an idempotent replay re-sends the
/// identical request. **Field-set pinned.**
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
    /// The operation kind (the kind is part of the op's identity — an approve never replays as a
    /// reject).
    pub op: OpKind,
    /// The built commit (`version_id`) this op publishes / reverts / reviews — part of the op's bound identity.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub candidate_commit: String,
    /// The candidate's byte-exact bundle digest (the consent hash) — part of the op's bound identity.
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub bundle_digest: String,
    /// The generation this op's compare-and-set targets — part of the op's bound identity.
    pub expected_generation: u64,
    /// The GOOD version a `revert` restores (the wire `good`) — present only for a `Revert` op (the
    /// server builds the forward commit from it; it is NOT the `candidate_commit`, so a replay must carry
    /// it). `None` for every other op.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-derives", schemars(extend("pattern" = "^[0-9a-f]{64}$")))]
    pub good: Option<String>,
    /// The skill's advisory display name (the author's folder name) sent alongside a publish/propose so a
    /// replay re-sends the identical value. Advisory only — it names the follower's folder + the dashboard
    /// entry, never the digest or the op's bound identity. `None` for a revert/review and for pre-existing WALs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The `--to` channel placement sent alongside a publish/propose so a replay re-sends the identical
    /// value. `None` when no placement was requested (and for a revert/review and pre-existing WALs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// The upstream-provenance block this publish carries (an imported bundle remembers its
    /// origin) — replayed byte-identical on a crash retry. `None` for local authorship.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<crate::requests::WireUpstream>,
    /// The catalog `kind` a genesis publish/propose declares (`"mcp"`), replayed byte-identical on
    /// a crash retry. `None` for skills, reverts/reviews, and pre-existing WALs. (Distinct from
    /// [`OpRecord::op`], the operation kind.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_kind: Option<String>,
    /// The stored terminal receipt, once one is known (the source of idempotent-retry truth).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_receipt: Option<Receipt>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_state_round_trips() {
        let s = SyncState {
            schema_version: 1,
            observed: 7,
            observed_version_id: "a".repeat(64),
            applied: 7,
            base_commit: "a".repeat(64),
            work_hash: "b".repeat(64),
            held: false,
            draft_observed: None,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["observed"], 7);
        assert_eq!(v["observed_version_id"], "a".repeat(64));
        let back: SyncState = serde_json::from_value(v).unwrap();
        assert_eq!(back.applied, 7);
        assert!(!back.held);
    }
}
