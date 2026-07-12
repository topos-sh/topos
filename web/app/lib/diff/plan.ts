import type { DiffEntry, DiffFileMode } from "./model";

/** The per-file leaf shape the plan needs — structurally satisfied by WireVersionFile. */
export interface PlanFile {
  path: string;
  mode: DiffFileMode;
  object_id: string;
}

/**
 * Classify base → candidate file lists into a diff plan. Pure set logic over
 * (path, mode, object_id) — no byte is read here, and no similarity is ever inferred:
 *
 * - same path, same object_id, same mode  → unchanged (never fetched)
 * - same path, same object_id, diff mode  → mode-only (never fetched)
 * - same path, different object_id        → modified
 * - path only in candidate / only in base → added / deleted
 * - EXACT-MOVE pairing: an unpaired deletion and an unpaired addition pair into `moved`
 *   ONLY when they share one object_id one-to-one with equal modes — provable byte
 *   identity. Anything ambiguous (several candidates for one object_id, or a mode
 *   change) falls back to add+delete.
 *
 * Sorted by path; a move sorts under its NEW path.
 */
export function computeDiffPlan(
  baseFiles: readonly PlanFile[],
  candidateFiles: readonly PlanFile[],
): DiffEntry[] {
  const base = new Map<string, PlanFile>(baseFiles.map((f) => [f.path, f]));
  const cand = new Map<string, PlanFile>(candidateFiles.map((f) => [f.path, f]));

  const entries: DiffEntry[] = [];
  const deletions: PlanFile[] = [];
  const additions: PlanFile[] = [];

  for (const [path, b] of base) {
    const c = cand.get(path);
    if (c === undefined) {
      deletions.push(b);
      continue;
    }
    if (b.object_id === c.object_id) {
      entries.push({
        kind: b.mode === c.mode ? "unchanged" : "mode-only",
        path,
        modes: { old: b.mode, new: c.mode },
        objectIds: { old: b.object_id, new: c.object_id },
      });
    } else {
      entries.push({
        kind: "modified",
        path,
        modes: { old: b.mode, new: c.mode },
        objectIds: { old: b.object_id, new: c.object_id },
      });
    }
  }
  for (const [path, c] of cand) {
    if (!base.has(path)) {
      additions.push(c);
    }
    void path;
  }

  // Exact-move pairing: one unpaired deletion + one unpaired addition sharing ONE object_id,
  // one-to-one, with equal modes. Everything else stays add+delete.
  const deletedByObject = groupByObjectId(deletions);
  const addedByObject = groupByObjectId(additions);
  const movedDeletedPaths = new Set<string>();
  const movedAddedPaths = new Set<string>();

  for (const [objectId, dels] of deletedByObject) {
    const adds = addedByObject.get(objectId);
    if (dels.length !== 1 || adds === undefined || adds.length !== 1) {
      continue;
    }
    const del = dels[0];
    const add = adds[0];
    if (del === undefined || add === undefined || del.mode !== add.mode) {
      continue;
    }
    movedDeletedPaths.add(del.path);
    movedAddedPaths.add(add.path);
    entries.push({
      kind: "moved",
      path: add.path,
      prevPath: del.path,
      modes: { old: del.mode, new: add.mode },
      objectIds: { old: objectId, new: objectId },
    });
  }

  for (const d of deletions) {
    if (!movedDeletedPaths.has(d.path)) {
      entries.push({
        kind: "deleted",
        path: d.path,
        modes: { old: d.mode },
        objectIds: { old: d.object_id },
      });
    }
  }
  for (const a of additions) {
    if (!movedAddedPaths.has(a.path)) {
      entries.push({
        kind: "added",
        path: a.path,
        modes: { new: a.mode },
        objectIds: { new: a.object_id },
      });
    }
  }

  entries.sort((x, y) => (x.path < y.path ? -1 : x.path > y.path ? 1 : 0));
  return entries;
}

function groupByObjectId(files: readonly PlanFile[]): Map<string, PlanFile[]> {
  const groups = new Map<string, PlanFile[]>();
  for (const f of files) {
    const list = groups.get(f.object_id);
    if (list === undefined) {
      groups.set(f.object_id, [f]);
    } else {
      list.push(f);
    }
  }
  return groups;
}
