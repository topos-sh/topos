/**
 * The vault's internal custody lane — the wire this tier speaks to the vault process. One
 * lane, one trust model: every request carries the shared internal bearer, and NOTHING else —
 * the vault is identity-free (authorization already happened in this tier; attribution rides
 * the bodies as pass-through display strings the vault stores verbatim).
 *
 * The lane is STATUS-driven (it has ONE caller, so no envelope machinery): 2xx carries the
 * typed answer; 400 `BAD_REQUEST`/`REJECTED` a shape/candidate refusal; 404 the uniform miss;
 * 409 `CONFLICT` (with the live `generation` + `version_id`) / `TARGET_PURGED` / `POINTED_AT`
 * the typed refusals; 500 a store fault. These DTOs mirror the vault's lane-local serde
 * (snake_case) field-for-field.
 */

/** One candidate file: path + mode (`"100644"`/`"100755"`) + base64 bytes (server-rehashed). */
export interface LaneFile {
  path: string;
  mode: string;
  content_base64: string;
}

/** POST …/bundles/{bundle}/versions — ingest + commit WITHOUT moving the pointer. */
export interface CommitBody {
  files: LaneFile[];
  /** The candidate's parent version (64-hex); absent = genesis. */
  parent?: string;
  /** The attribution display string recorded verbatim (commit author + author_display). */
  attribution: string;
  message: string;
}

/** POST …/bundles/{bundle}/publish — ingest + commit + CAS move, one flow. */
export interface PublishBody extends CommitBody {
  /** Absent = genesis (creates the pointer at generation 1); present = the CAS target. */
  expected_generation?: number;
}

/** POST …/bundles/{bundle}/pointer — CAS move to an EXISTING version (the approve path). */
export interface PointerMoveBody {
  version_id: string;
  expected_generation?: number;
  attribution: string;
}

/** POST …/bundles/{bundle}/revert — forward commit {tree: target.tree, parents: [current]}. */
export interface RevertBody {
  to_version_id: string;
  expected_generation: number;
  attribution: string;
  /** The forward-revert commit message, recorded verbatim (the frame's inputs are the wire's —
   * a device pre-derives the forward id and verifies the move landed on exactly that version). */
  message: string;
}

/** The committed-version answer (`versions` returns exactly this; publish/revert extend it). */
export interface CommittedVersion {
  version_id: string;
  commit_id: string;
  bundle_digest: string;
  /** True when an identical candidate converged on already-committed ids. */
  deduped: boolean;
}

/** A pointer state; `replayed` marks the idempotent-CAS carve-out (a crash retry that landed). */
export interface PointerState {
  version_id: string;
  generation: number;
  moved_at_ms: number;
  moved_by_display: string;
  replayed: boolean;
}

/** The publish/revert answer: the committed version + the moved pointer. */
export interface PublishedVersion extends CommittedVersion {
  pointer: PointerState;
}

/** GET …/bundles/{bundle}/current — the pointer record + the pointed version's digest. */
export interface CustodyCurrent {
  version_id: string;
  generation: number;
  moved_at_ms: number;
  moved_by_display: string;
  bundle_digest: string;
}

/** One file of a version listing (no bytes; the object read serves those). */
export interface CustodyVersionFile {
  path: string;
  mode: string;
  object_id: string;
}

/** GET …/bundles/{bundle}/versions/{version_id} — meta + file listing. */
export interface CustodyVersionMeta {
  version_id: string;
  parents: string[];
  /** The attribution recorded at commit (a display string, not an identity). */
  author: string;
  message: string;
  bundle_digest: string;
  created_at_ms: number;
  files: CustodyVersionFile[];
}

/** One hop of the log (the first-parent chain from current, newest first, capped). */
export interface CustodyLogEntry {
  version_id: string;
  message: string;
  author_display: string;
  created_at_ms: number;
  /** The byte-purge tombstone half; the hash stays listed. */
  purged_at_ms?: number;
}

/** GET …/bundles/{bundle}/log */
export interface CustodyLog {
  versions: CustodyLogEntry[];
}

/** POST …/versions/{version_id}/purge — what the purge did. */
export interface PurgeResult {
  tombstoned: number;
  reclaimed: number;
}

/** The lane's flat error body: `{ code, message?, generation?, version_id? }`. */
export interface LaneErrorBody {
  code: string;
  message?: string;
  generation?: number;
  version_id?: string;
}
