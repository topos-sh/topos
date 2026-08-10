/**
 * Shared validation for the publish-family request bodies (`publish` / `propose` / `revert` /
 * `review`). Parse-don't-validate at the door: a malformed body or identifier is a 400 BEFORE
 * the credential resolve, mirroring the old extractor ordering. Nothing here trusts a client
 * hash — the vault rehashes every byte; these checks only refuse garbage early.
 */

export const HEX_64 = /^[0-9a-f]{64}$/;
/** The id rule shared by workspace/bundle ids on the wire (path-safe, 1–128). */
export const WIRE_ID = /^[A-Za-z0-9._-]{1,128}$/;
/** The canonical lowercase-hyphenated UUID spelling op ids are keyed on. */
export const OP_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
/** The catalog `kind` vocabulary — CLOSED, and this is where it closes: a client branches on
 * kind to pick a bundle's delivery mechanics, so a kind nothing implements must never enter the
 * catalog. Mirrored by the `bundle_kind_check` constraint in db/schema.app.ts; a new kind joins
 * both lists in the release that implements it. */
export const BUNDLE_KINDS = ["skill", "mcp"] as const;
export type BundleKind = (typeof BUNDLE_KINDS)[number];

const FILE_MODES = new Set(["100644", "100755"]);

/** The device wire's candidate: files by value + the declared parents + author + message. */
export interface WireCandidate {
  files: { path: string; mode: string; content_base64: string }[];
  parents: string[];
  author: string;
  message: string;
}

/** Validate a device-wire candidate; returns the typed value or a human-readable refusal. */
export function parseCandidate(raw: unknown): WireCandidate | string {
  if (typeof raw !== "object" || raw === null) {
    return "malformed candidate";
  }
  const c = raw as { files?: unknown; parents?: unknown; author?: unknown; message?: unknown };
  if (!Array.isArray(c.files)) {
    return "malformed candidate: files";
  }
  const files: WireCandidate["files"] = [];
  for (const entry of c.files as unknown[]) {
    const f = entry as { path?: unknown; mode?: unknown; content_base64?: unknown };
    if (
      typeof f !== "object" ||
      f === null ||
      typeof f.path !== "string" ||
      f.path.length === 0 ||
      typeof f.mode !== "string" ||
      !FILE_MODES.has(f.mode) ||
      typeof f.content_base64 !== "string"
    ) {
      return "malformed candidate file";
    }
    files.push({ path: f.path, mode: f.mode, content_base64: f.content_base64 });
  }
  if (
    !Array.isArray(c.parents) ||
    !c.parents.every((p) => typeof p === "string" && HEX_64.test(p))
  ) {
    return "malformed candidate: parents";
  }
  if (typeof c.author !== "string" || c.author.length === 0) {
    return "malformed candidate: author";
  }
  if (typeof c.message !== "string") {
    return "malformed candidate: message";
  }
  return { files, parents: c.parents as string[], author: c.author, message: c.message };
}

export interface PublishFamilyHead {
  workspaceId: string;
  skillId: string;
  opId: string;
  expected: number;
}

/** The shared head fields every publish-family body carries. */
export function parsePublishHead(raw: Record<string, unknown>): PublishFamilyHead | string {
  const workspaceId = raw.workspace_id;
  const skillId = raw.skill_id;
  const opId = raw.op_id;
  const expected = raw.expected;
  if (typeof workspaceId !== "string" || !WIRE_ID.test(workspaceId)) {
    return "malformed workspace_id";
  }
  if (typeof skillId !== "string" || !WIRE_ID.test(skillId)) {
    return "malformed skill_id";
  }
  if (typeof opId !== "string" || !OP_ID.test(opId)) {
    return "malformed op_id";
  }
  if (typeof expected !== "number" || !Number.isSafeInteger(expected) || expected < 0) {
    return "malformed expected generation";
  }
  return { workspaceId, skillId, opId, expected };
}

/**
 * The optional genesis `kind` a publish-family body may declare. Absent or null ⇒ null (the
 * catalog's own default, 'skill', stands); a present value must NAME A KNOWN KIND — anything
 * else is a refusal string, never a silent drop and never a stored unknown, because kind is
 * birth metadata the bundle carries for life and every client branches on it.
 */
export function parseBundleKind(raw: unknown): { kind: BundleKind | null } | string {
  if (raw === undefined || raw === null) {
    return { kind: null };
  }
  const known = typeof raw === "string" ? BUNDLE_KINDS.find((k) => k === raw) : undefined;
  if (known === undefined) {
    return `unknown kind — known kinds: ${BUNDLE_KINDS.map((k) => `'${k}'`).join(", ")}`;
  }
  return { kind: known };
}

/** RFC-3339 seconds + Z — the receipt timestamp spelling (no millis). */
export function receiptNow(): string {
  return new Date().toISOString().replace(/\.\d{3}Z$/, "Z");
}
