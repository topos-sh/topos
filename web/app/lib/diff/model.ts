/**
 * The pure diff render model — no React, no IO. computeDiffPlan (plan.ts) classifies the two
 * file lists into DiffEntry[]; loadDiffContents (load.server.ts) turns the plan into
 * FileDiffModel[] by fetching only the blobs a rendered card actually needs.
 */

/** A file's mode as carried on the wire (the two git regular-file modes). */
export type DiffFileMode = "100644" | "100755";

export type DiffKind = "added" | "deleted" | "modified" | "mode-only" | "moved" | "unchanged";

export interface DiffEntry {
  kind: DiffKind;
  /** The candidate-side path (the NEW path for a move); the base path for a deletion. */
  path: string;
  /** The base-side path — present only on a move. */
  prevPath?: string;
  modes: { old?: DiffFileMode; new?: DiffFileMode };
  objectIds: { old?: string; new?: string };
}

/** How a planned entry is presented once its bytes were (or weren't) loaded. */
export type DiffPresentation = "text" | "binary" | "too-large" | "fetch-failed";

/** Why a file wasn't rendered as text when presentation is `too-large`. */
export type OversizeReason = "blob-cap" | "page-cap" | "file-count";

export interface FileDiffModel {
  entry: DiffEntry;
  presentation: DiffPresentation;
  /** Present only when presentation is `too-large`. */
  reason?: OversizeReason;
  oldText?: string;
  newText?: string;
  sizes: { old?: number; new?: number };
}

/** Per-blob fetch cap — a larger object renders as an honest too-large card. */
export const MAX_BLOB_BYTES = 1024 * 1024;
/** Whole-page byte budget across every fetched blob. */
export const MAX_TOTAL_RENDER_BYTES = 8 * 1024 * 1024;
/** How many changed files get a rendered card before honest overflow entries take over. */
export const MAX_FILES_RENDERED = 100;
/** Text beyond this renders as a plain (unhighlighted) diff. */
export const MAX_HIGHLIGHT_BYTES = 128 * 1024;
