import { vaultFetch } from "./client.server";
import type {
  CommitBody,
  CommittedVersion,
  LaneErrorBody,
  PointerMoveBody,
  PointerState,
  PublishBody,
  PublishedVersion,
  PurgeResult,
  RevertBody,
} from "./wire";

/**
 * The custody WRITE wrappers — the byte/pointer half of every publish-family op. The app is
 * the authority: each caller has already authorized the acting identity against the seat rows
 * and resolved the protection gate; the vault only ingests bytes and CAS-moves pointers,
 * recording the pass-through attribution these bodies carry.
 *
 * The lane is STATUS-driven; every wrapper folds it into ONE typed outcome union:
 *  - `ok` — the 2xx answer;
 *  - `conflict` — a lost pointer CAS (409 CONFLICT, carrying the live generation + version);
 *  - `target_purged` / `pointed_at` — the typed 409 refusals;
 *  - `rejected` — a refused candidate/shape (400; the message is the vault's own);
 *  - `not_found` — the uniform 404;
 *  - `fault` — a 5xx or an unreachable vault.
 * CAS moves are idempotent vault-side: a crash retry that already landed answers success with
 * `replayed` marked.
 */

export type CustodyOutcome<T> =
  | { kind: "ok"; value: T }
  | { kind: "conflict"; generation?: number; versionId?: string }
  | { kind: "target_purged" }
  | { kind: "pointed_at" }
  | { kind: "rejected"; message?: string }
  | { kind: "not_found" }
  | { kind: "fault" };

async function laneError(res: Response): Promise<LaneErrorBody | undefined> {
  try {
    return (await res.json()) as LaneErrorBody;
  } catch {
    return undefined;
  }
}

async function foldOutcome<T>(res: Response): Promise<CustodyOutcome<T>> {
  if (res.ok) {
    return { kind: "ok", value: (await res.json()) as T };
  }
  if (res.status === 404) {
    return { kind: "not_found" };
  }
  const body = await laneError(res);
  if (res.status === 409) {
    if (body?.code === "TARGET_PURGED") {
      return { kind: "target_purged" };
    }
    if (body?.code === "POINTED_AT") {
      return { kind: "pointed_at" };
    }
    return { kind: "conflict", generation: body?.generation, versionId: body?.version_id };
  }
  if (res.status === 400) {
    return { kind: "rejected", message: body?.message };
  }
  return { kind: "fault" };
}

async function postCustody<T>(
  template: string,
  params: Record<string, string>,
  body: unknown,
): Promise<CustodyOutcome<T>> {
  try {
    const res = await vaultFetch({ method: "POST", template, params, body });
    return await foldOutcome<T>(res);
  } catch {
    return { kind: "fault" };
  }
}

/** Commit a candidate WITHOUT moving the pointer — a proposal's ingest. */
export function commitVersion(
  ws: string,
  bundleId: string,
  body: CommitBody,
): Promise<CustodyOutcome<CommittedVersion>> {
  return postCustody(
    "/internal/v1/workspaces/{ws}/bundles/{bundle}/versions",
    {
      ws,
      bundle: bundleId,
    },
    body,
  );
}

/** Commit + CAS-move in one op; an ABSENT expected_generation is the genesis arm. */
export function publishVersion(
  ws: string,
  bundleId: string,
  body: PublishBody,
): Promise<CustodyOutcome<PublishedVersion>> {
  return postCustody(
    "/internal/v1/workspaces/{ws}/bundles/{bundle}/publish",
    {
      ws,
      bundle: bundleId,
    },
    body,
  );
}

/** CAS-move the pointer onto an EXISTING version — the review approve's promote. */
export function movePointer(
  ws: string,
  bundleId: string,
  body: PointerMoveBody,
): Promise<CustodyOutcome<PointerState>> {
  return postCustody(
    "/internal/v1/workspaces/{ws}/bundles/{bundle}/pointer",
    {
      ws,
      bundle: bundleId,
    },
    body,
  );
}

/** The forward revert: the vault builds a commit carrying the good tree, then CAS-moves. */
export function revertPointer(
  ws: string,
  bundleId: string,
  body: RevertBody,
): Promise<CustodyOutcome<PublishedVersion>> {
  return postCustody(
    "/internal/v1/workspaces/{ws}/bundles/{bundle}/revert",
    {
      ws,
      bundle: bundleId,
    },
    body,
  );
}

/** Byte-purge ONE version (tombstone; the hash stays). Refused typed while pointed-at. */
export function purgeVersionBytes(
  ws: string,
  bundleId: string,
  versionId: string,
  attribution: string,
): Promise<CustodyOutcome<PurgeResult>> {
  return postCustody(
    "/internal/v1/workspaces/{ws}/bundles/{bundle}/versions/{version_id}/purge",
    { ws, bundle: bundleId, version_id: versionId },
    { attribution },
  );
}

/**
 * Drop a bundle's whole custody (every version's bytes) — the delete ceremony's byte half.
 * True on a 2xx; false on a fault (the caller's row tombstone stands either way and says so).
 * Idempotent vault-side.
 */
export async function deleteBundleBytes(ws: string, bundleId: string): Promise<boolean> {
  try {
    const res = await vaultFetch({
      method: "DELETE",
      template: "/internal/v1/workspaces/{ws}/bundles/{bundle}",
      params: { ws, bundle: bundleId },
    });
    return res.ok;
  } catch {
    return false;
  }
}
