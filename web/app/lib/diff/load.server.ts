import type { PlaneResult } from "@/lib/plane/errors";
import { classifyBytes, decodeTextVerbatim } from "./classify";
import {
  type DiffEntry,
  type FileDiffModel,
  MAX_BLOB_BYTES,
  MAX_FILES_RENDERED,
  MAX_TOTAL_RENDER_BYTES,
} from "./model";

/**
 * The one IO seam: injected so the loader is testable with plain stubs. The fetcher closes
 * over its read scope (the page binds the actor + skill), so the loader itself names no
 * credential and no actor — it only asks for object bytes under a cap.
 */
export interface DiffBlobSource {
  getBundleCapped(objectId: string, maxBytes: number): Promise<PlaneResult<Uint8Array>>;
}

const FETCH_CONCURRENCY = 6;

/**
 * Turn a diff plan into per-file render models. Fetches ONLY the blobs a rendered card needs
 * (unchanged, mode-only, and moved entries are never fetched — their classification is already
 * proven by object ids), at most FETCH_CONCURRENCY at a time, each side capped at
 * MAX_BLOB_BYTES. A single failed fetch degrades that one card ('fetch-failed'); the page
 * survives. The whole-page byte budget and file-count cap produce honest overflow entries.
 */
export async function loadDiffContents(
  plan: readonly DiffEntry[],
  io: DiffBlobSource,
): Promise<FileDiffModel[]> {
  const renderable = plan.filter((e) => e.kind !== "unchanged");
  const models = new Array<FileDiffModel>(renderable.length);
  // One fetch per distinct object id, shared across entries.
  const inFlight = new Map<string, Promise<PlaneResult<Uint8Array>>>();
  let totalBytes = 0;
  // The count of cards that have STARTED a real diff fetch — the true "rendered cards" budget.
  // Moved / mode-only entries render without a fetch and must NOT consume this budget (else a
  // server can pad the list with cheap moves to push a real modification past the cap).
  let startedCards = 0;

  const fetchObject = (objectId: string): Promise<PlaneResult<Uint8Array>> => {
    let p = inFlight.get(objectId);
    if (p === undefined) {
      p = io.getBundleCapped(objectId, MAX_BLOB_BYTES).then((result) => {
        if (result.ok) {
          totalBytes += result.data.byteLength;
        }
        return result;
      });
      inFlight.set(objectId, p);
    }
    return p;
  };

  // Entries needing no blob fetch resolve immediately and consume neither budget.
  const fetchIndices: number[] = [];
  renderable.forEach((entry, i) => {
    if (entry.kind === "mode-only" || entry.kind === "moved") {
      models[i] = { entry, presentation: "text", sizes: {} };
    } else {
      fetchIndices.push(i);
    }
  });

  // A worker pool over the fetch-needing entries, IN ORDER. Because each worker awaits its fetches,
  // the running `totalBytes` and `startedCards` are live when the next entry is decided — so the
  // page-byte budget (dead under a parallel `.map`) and the file-count budget both bite.
  // Memory is bounded to ~MAX_TOTAL_RENDER_BYTES + FETCH_CONCURRENCY×2×MAX_BLOB_BYTES.
  let cursor = 0;
  const worker = async (): Promise<void> => {
    while (cursor < fetchIndices.length) {
      const i = fetchIndices[cursor++];
      if (i === undefined) {
        return;
      }
      const entry = renderable[i];
      if (entry === undefined) {
        continue;
      }
      if (startedCards >= MAX_FILES_RENDERED) {
        models[i] = { entry, presentation: "too-large", reason: "file-count", sizes: {} };
        continue;
      }
      if (totalBytes >= MAX_TOTAL_RENDER_BYTES) {
        models[i] = { entry, presentation: "too-large", reason: "page-cap", sizes: {} };
        continue;
      }
      startedCards += 1;
      models[i] = await renderEntry(entry, fetchObject);
    }
  };
  await Promise.all(Array.from({ length: FETCH_CONCURRENCY }, () => worker()));
  return models;
}

async function renderEntry(
  entry: DiffEntry,
  fetchObject: (objectId: string) => Promise<PlaneResult<Uint8Array>>,
): Promise<FileDiffModel> {
  const [oldRes, newRes] = await Promise.all([
    entry.objectIds.old !== undefined ? fetchObject(entry.objectIds.old) : undefined,
    entry.objectIds.new !== undefined ? fetchObject(entry.objectIds.new) : undefined,
  ]);
  const failure = firstFailure(oldRes, newRes);
  if (failure !== undefined) {
    if (failure.kind === "too_large") {
      return { entry, presentation: "too-large", reason: "blob-cap", sizes: {} };
    }
    return { entry, presentation: "fetch-failed", sizes: {} };
  }
  const oldBytes = oldRes?.ok === true ? oldRes.data : undefined;
  const newBytes = newRes?.ok === true ? newRes.data : undefined;
  const sizes = { old: oldBytes?.byteLength, new: newBytes?.byteLength };
  const oldKind = oldBytes !== undefined ? classifyBytes(oldBytes) : undefined;
  const newKind = newBytes !== undefined ? classifyBytes(newBytes) : undefined;
  if (oldKind === "binary" || newKind === "binary") {
    return { entry, presentation: "binary", sizes };
  }
  return {
    entry,
    presentation: "text",
    oldText: oldBytes !== undefined ? decodeTextVerbatim(oldBytes) : undefined,
    newText: newBytes !== undefined ? decodeTextVerbatim(newBytes) : undefined,
    sizes,
  };
}

function firstFailure(
  ...results: (PlaneResult<Uint8Array> | undefined)[]
): { kind: string } | undefined {
  for (const r of results) {
    if (r !== undefined && !r.ok) {
      return r;
    }
  }
  return undefined;
}
