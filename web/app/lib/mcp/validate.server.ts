import { SECRET_ENTROPY, SECRET_PATTERNS } from "./secret-patterns.generated";

/**
 * THE MCP SERVER-DOCUMENT GATE — one function, used by every door that can put a `kind: 'mcp'`
 * bundle into a workspace (the session lane's publish/propose, the web import page, and the
 * registry lane's read of what is already stored).
 *
 * What a shared MCP bundle IS, here: a REMOTE server an agent can reach over
 * `streamable-http` with the exact bytes the workspace holds — no local install, no
 * per-machine fill-in, and no credential anywhere in the document. Everything this refuses
 * refuses for one of those three reasons, and every refusal is TYPED so the caller can say
 * plainly what is wrong instead of "invalid":
 *
 *  · `MCP_INVALID`              — not JSON, or the required registry fields are missing/malformed
 *  · `MCP_LOCAL_REFUSED`        — a non-empty `packages[]`: the server installs and runs locally
 *  · `MCP_NO_STREAMABLE_REMOTE` — no `streamable-http` remote (sse-only, or no remotes at all)
 *  · `MCP_INSECURE_URL`         — the endpoint is plain http
 *  · `MCP_URL_TEMPLATE`         — the endpoint carries a `{placeholder}`, so it is not an address
 *  · `MCP_SECRET_REFUSED`       — the document carries (or reserves a slot for) a credential
 *
 * The shape rules mirror the official registry schema (2025-12-11): `name` is exactly one slash
 * between a reverse-DNS namespace and a server name (3–200 chars), `description` is 1–100
 * characters, `version` is required. The refusal vectors that pin all of it live at the repo
 * root — `tests/fixtures/mcp/vectors.json` — and the unit suite drives THIS function through
 * every one of them, so a rule cannot change here without the vector changing too.
 *
 * ORDER MATTERS: the credential scan runs FIRST, over the whole raw text, immediately after the
 * JSON parses. A document carrying somebody's token must never be read further, previewed, or
 * echoed back in a field-level error message — the refusal comes before anything is taken out
 * of it.
 */

/** The registry's name grammar: `<reverse.dns.namespace>/<server-name>`, exactly one slash. */
const NAME_SHAPE = /^[a-zA-Z0-9.-]+\/[a-zA-Z0-9._-]+$/;
const NAME_MIN = 3;
const NAME_MAX = 200;
const DESCRIPTION_MAX = 100;
const VERSION_MAX = 255;
/** A hard ceiling on the document itself — a server.json is a page of text, never a payload. */
export const MAX_SERVER_JSON_BYTES = 256 * 1024;

/**
 * The WHOLE file set an MCP candidate may carry: the document (required, at the root) and an
 * optional README. Anything else is refused by name — a bundle whose behavior is one JSON
 * document must not smuggle scripts or extra payloads beside it.
 */
export const MCP_ALLOWED_FILES = ["server.json", "README.md"] as const;

/**
 * Header NAMES that carry a credential by definition — refused case-insensitively, independent
 * of `isSecret`, value shape, or entropy. A literal `Authorization: Basic …` is somebody's
 * credential whatever the flags say.
 */
const CREDENTIAL_HEADER_NAMES = new Set([
  "authorization",
  "proxy-authorization",
  "cookie",
  "set-cookie",
  "x-api-key",
  "api-key",
  "x-auth-token",
  "x-access-token",
  "private-token",
]);

/** The one strict decoder: invalid UTF-8 is refused, never replaced (a replacement char can hide
 * what the raw bytes spelled). */
const strictUtf8 = new TextDecoder("utf-8", { fatal: true });

/** The one transport a shared bundle can promise: the same URL works from every machine. */
export const STREAMABLE_HTTP = "streamable-http";

export type McpRefusalCode =
  | "MCP_INVALID"
  | "MCP_LOCAL_REFUSED"
  | "MCP_NO_STREAMABLE_REMOTE"
  | "MCP_INSECURE_URL"
  | "MCP_URL_TEMPLATE"
  | "MCP_SECRET_REFUSED";

/** A header that survived the gate: a literal name and a literal value, nothing to fill in. */
export interface McpHeader {
  name: string;
  value: string;
}

/** What the preview shows and what the catalog records — derived, never echoed wholesale. */
export interface McpSummary {
  name: string;
  description: string;
  version: string;
  url: string;
  transport: typeof STREAMABLE_HTTP;
  headers: McpHeader[];
  /**
   * The publisher's own `_meta["sh.topos/auth"]` word, when it says something this tier
   * understands: `"oauth"` (the agent will be sent through an authorization dance on first
   * use) or `"none"`. NULL means the document declared nothing — which is not the same claim
   * as "none", so it is never upgraded to one.
   */
  authHint: "oauth" | "none" | null;
}

export type McpValidation =
  | { ok: true; summary: McpSummary }
  | { ok: false; code: McpRefusalCode; message: string };

function refuse(code: McpRefusalCode, message: string): McpValidation {
  return { ok: false, code, message };
}

// ── The credential scan ─────────────────────────────────────────────────────────────────────

const compiled = SECRET_PATTERNS.map((p) => ({ name: p.name, regex: new RegExp(p.regex) }));

/** Shannon entropy in bits per character. */
function entropyOf(token: string): number {
  const counts = new Map<string, number>();
  for (const ch of token) {
    counts.set(ch, (counts.get(ch) ?? 0) + 1);
  }
  let bits = 0;
  for (const n of counts.values()) {
    const p = n / token.length;
    bits -= p * Math.log2(p);
  }
  return bits;
}

/**
 * The token alphabet the entropy belt walks. Deliberately NARROW — no dot, no slash — so a
 * hostname or a URL path splits into its parts instead of concatenating into one long
 * high-entropy-looking run. Real credentials are contiguous in this alphabet; addresses are not.
 */
const TOKEN_RUN = /[A-Za-z0-9_+=-]{8,}/g;

/**
 * Does this token READ as a random secret? Entropy alone does not separate `sk-…` from an
 * ordinary English phrase (both land near 4 bits/char), so two shapes qualify and nothing else:
 *
 *  · MIXED-CLASS — long enough, entropy at or past the threshold, AND lower + upper + digit all
 *    present. Prose and slugs fail the class test; generated keys almost never do.
 *  · LONG HEX — 32 or more characters of pure lowercase/uppercase hex. Its entropy is only 4
 *    bits/char by construction, so the threshold would miss it, and it is the other common
 *    key spelling.
 */
function looksRandom(token: string): boolean {
  if (token.length >= 32 && /^[0-9a-f]+$/.test(token)) {
    return true;
  }
  if (token.length >= 32 && /^[0-9A-F]+$/.test(token)) {
    return true;
  }
  if (token.length < SECRET_ENTROPY.minLength) {
    return false;
  }
  const mixed = /[a-z]/.test(token) && /[A-Z]/.test(token) && /[0-9]/.test(token);
  return mixed && entropyOf(token) >= SECRET_ENTROPY.threshold;
}

/** The first credential the raw text carries, named for the refusal message — or null. */
export function findSecret(raw: string): string | null {
  for (const { name, regex } of compiled) {
    if (regex.test(raw)) {
      return name;
    }
  }
  for (const match of raw.matchAll(TOKEN_RUN)) {
    if (looksRandom(match[0])) {
      return "high-entropy value";
    }
  }
  return null;
}

/**
 * The same scan over the PARSED document: every decoded string — keys and values, at any depth —
 * runs the pattern + entropy belt. This is what the raw pass cannot see: a token spelled in
 * `\uXXXX` escapes decodes to the credential the raw bytes never showed.
 *
 * Traversal is an EXPLICIT stack, never recursion: `JSON.parse` is iterative, so a document
 * nested a hundred thousand levels deep parses fine — and a recursive walk over it would blow
 * the call stack and turn a typed refusal into a crash. (The gate's own depth cap refuses such
 * documents before this runs; the explicit stack keeps this function safe standalone.)
 */
export function findSecretDeep(value: unknown): string | null {
  // Two item shapes so the walk visits strings, keys, and values in exactly the order the
  // recursive spelling did — the FIRST hit names the refusal, and which one is "first" must not
  // change with the traversal mechanics.
  type Item = { key: string } | { value: unknown };
  const stack: Item[] = [{ value }];
  for (let item = stack.pop(); item !== undefined; item = stack.pop()) {
    if ("key" in item) {
      const hit = findSecret(item.key);
      if (hit !== null) {
        return hit;
      }
      continue;
    }
    const current = item.value;
    if (typeof current === "string") {
      const hit = findSecret(current);
      if (hit !== null) {
        return hit;
      }
      continue;
    }
    if (Array.isArray(current)) {
      for (let i = current.length - 1; i >= 0; i -= 1) {
        stack.push({ value: current[i] });
      }
      continue;
    }
    if (typeof current === "object" && current !== null) {
      const entries = Object.entries(current);
      for (let i = entries.length - 1; i >= 0; i -= 1) {
        const [key, entry] = entries[i] as [string, unknown];
        stack.push({ value: entry });
        stack.push({ key });
      }
    }
  }
  return null;
}

/**
 * The depth cap the parse rail wears, mirroring serde_json's default recursion limit so the two
 * gates stay vector-identical: serde refuses the 128th nested container, so documents up to 127
 * containers deep parse on both sides and anything deeper is `MCP_INVALID` on both sides. (Here
 * `JSON.parse` is iterative and would accept far deeper documents — the cap is what keeps this
 * tier from accepting a document the Rust gate refuses.)
 */
export const MAX_DOCUMENT_DEPTH = 128;

/** Whether the parsed document nests containers `MAX_DOCUMENT_DEPTH` deep or deeper — an
 * explicit-stack walk, for the same no-recursion reason as [`findSecretDeep`]. */
export function nestsTooDeep(root: unknown): boolean {
  const stack: { value: unknown; depth: number }[] = [{ value: root, depth: 1 }];
  for (let item = stack.pop(); item !== undefined; item = stack.pop()) {
    const { value, depth } = item;
    const isContainer = Array.isArray(value) || (typeof value === "object" && value !== null);
    if (!isContainer) {
      continue;
    }
    if (depth >= MAX_DOCUMENT_DEPTH) {
      return true;
    }
    const children = Array.isArray(value) ? value : Object.values(value);
    for (const child of children) {
      stack.push({ value: child, depth: depth + 1 });
    }
  }
  return false;
}

// ── The gate ────────────────────────────────────────────────────────────────────────────────

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** A non-empty object — the shape that makes `variables` a per-installation fill-in slot. */
function hasEntries(value: unknown): boolean {
  return isRecord(value) && Object.keys(value).length > 0;
}

/**
 * Validate one server document. `raw` is the bytes as fetched or pasted — the scan reads THEM,
 * not a re-serialization, so nothing a caller strips can hide a credential from it.
 */
export function validateServerJson(raw: Uint8Array | string): McpValidation {
  let text: string;
  if (typeof raw === "string") {
    text = raw;
  } else {
    // STRICT decode: an invalid byte refuses outright — a lossy replacement char could both hide
    // what the bytes spelled and let an unreadable document parse as if it were readable.
    try {
      text = strictUtf8.decode(raw);
    } catch {
      return refuse("MCP_INVALID", "the document is not valid UTF-8");
    }
  }
  if (text.length === 0) {
    return refuse("MCP_INVALID", "the document is empty");
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return refuse("MCP_INVALID", "that is not JSON — a server.json document is a JSON object");
  }
  // The depth cap sits WITH the parse, before anything walks the document: on the Rust side a
  // document this deep never parses at all (serde's recursion limit), so refusing here — before
  // even the credential scan — is what keeps the two gates answering identically.
  if (nestsTooDeep(parsed)) {
    return refuse("MCP_INVALID", "document nesting too deep");
  }

  // FIRST, before anything is read out of the document: does it carry a credential? Two passes —
  // the raw text (nothing a caller strips can hide from it) and the DECODED strings (nothing a
  // publisher escapes can hide from that one).
  const secret = findSecret(text) ?? findSecretDeep(parsed);
  if (secret !== null) {
    return refuse(
      "MCP_SECRET_REFUSED",
      `the document carries what looks like a credential (${secret}) — a shared bundle never holds one`,
    );
  }

  if (!isRecord(parsed)) {
    return refuse("MCP_INVALID", "a server.json document is a JSON object");
  }

  const name = parsed.name;
  if (typeof name !== "string" || name.length < NAME_MIN || name.length > NAME_MAX) {
    return refuse("MCP_INVALID", `name is required, ${NAME_MIN}–${NAME_MAX} characters`);
  }
  if (!NAME_SHAPE.test(name)) {
    return refuse(
      "MCP_INVALID",
      "name must be a reverse-DNS namespace and a server name with exactly one slash between them",
    );
  }
  const description = parsed.description;
  if (
    typeof description !== "string" ||
    description.length === 0 ||
    description.length > DESCRIPTION_MAX
  ) {
    return refuse("MCP_INVALID", `description is required, 1–${DESCRIPTION_MAX} characters`);
  }
  const version = parsed.version;
  if (typeof version !== "string" || version.length === 0 || version.length > VERSION_MAX) {
    return refuse("MCP_INVALID", "version is required");
  }

  // A non-empty packages[] is the registry's way of saying "install and run this locally".
  // That is a different kind of thing from a shared address, so it is refused rather than
  // half-supported.
  if (Array.isArray(parsed.packages) && parsed.packages.length > 0) {
    return refuse(
      "MCP_LOCAL_REFUSED",
      "this server installs and runs locally (packages[]) — Topos shares remote servers",
    );
  }

  const remotes = Array.isArray(parsed.remotes) ? parsed.remotes : [];
  // FIRST streamable-http wins: a document may offer several transports, and the ordering is
  // the publisher's own preference.
  const remote = remotes.find(
    (entry): entry is Record<string, unknown> =>
      isRecord(entry) && entry.type === STREAMABLE_HTTP && typeof entry.url === "string",
  );
  if (remote === undefined) {
    return refuse(
      "MCP_NO_STREAMABLE_REMOTE",
      "no streamable-http remote — Topos places servers an agent reaches over that transport",
    );
  }

  const url = remote.url as string;
  // The template check comes before the URL parse: `https://{tenant}.example/mcp` PARSES, and
  // would otherwise pass as an https address whose host is a literal brace word.
  if (/[{}]/.test(url)) {
    return refuse(
      "MCP_URL_TEMPLATE",
      "the endpoint carries a {placeholder} — it is a template, not an address every machine can use",
    );
  }
  let parsedUrl: URL;
  try {
    parsedUrl = new URL(url);
  } catch {
    return refuse("MCP_INVALID", "the endpoint is not a URL");
  }
  // userinfo in the address IS a credential — refused before the scheme is even judged, so an
  // http URL carrying one still names the real problem.
  if (parsedUrl.username !== "" || parsedUrl.password !== "") {
    return refuse(
      "MCP_SECRET_REFUSED",
      "the endpoint URL carries credentials (user:password@) — a shared bundle never holds one",
    );
  }
  if (parsedUrl.protocol !== "https:") {
    return refuse("MCP_INSECURE_URL", "the endpoint must be https");
  }
  // A remote-level `variables` block only exists to fill a template in. There is no template
  // left by now, so it is a fill-in slot with nothing to fill — and the thing it would fill is
  // exactly what this gate exists to keep out.
  if (hasEntries(remote.variables)) {
    return refuse(
      "MCP_SECRET_REFUSED",
      "the endpoint declares per-installation variables — a shared bundle carries the same bytes everywhere",
    );
  }

  const rawHeaders = Array.isArray(remote.headers) ? remote.headers : [];
  const headers: McpHeader[] = [];
  for (const entry of rawHeaders) {
    if (!isRecord(entry) || typeof entry.name !== "string" || entry.name.length === 0) {
      return refuse("MCP_INVALID", "every header needs a name");
    }
    // A credential-bearing header NAME refuses whatever the value or the flags say — before
    // isSecret, before the value shape, independent of entropy.
    if (CREDENTIAL_HEADER_NAMES.has(entry.name.toLowerCase())) {
      return refuse(
        "MCP_SECRET_REFUSED",
        `the header ${entry.name} carries a credential by definition — a shared bundle never holds one`,
      );
    }
    if (entry.isSecret === true) {
      return refuse(
        "MCP_SECRET_REFUSED",
        `the header ${entry.name} is declared secret — a shared bundle never carries a credential`,
      );
    }
    if (hasEntries(entry.variables)) {
      return refuse(
        "MCP_SECRET_REFUSED",
        `the header ${entry.name} is assembled from per-installation variables`,
      );
    }
    // A header with no literal value is a slot somebody fills in on each machine — the same
    // thing `isSecret` names out loud, whether or not `isRequired` says so.
    if (typeof entry.value !== "string" || entry.value.length === 0) {
      return refuse(
        "MCP_SECRET_REFUSED",
        `the header ${entry.name} has no value — it is a slot for a per-machine credential`,
      );
    }
    headers.push({ name: entry.name, value: entry.value });
  }

  const meta = isRecord(parsed._meta) ? parsed._meta : {};
  const declared = meta["sh.topos/auth"];
  const authHint = declared === "oauth" ? "oauth" : declared === "none" ? "none" : null;

  return {
    ok: true,
    summary: {
      name,
      description,
      version,
      url,
      transport: STREAMABLE_HTTP,
      headers,
      authHint,
    },
  };
}

/** One candidate file, as bytes — the shape the whole-candidate gate below judges. */
export interface McpCandidateFile {
  path: string;
  bytes: Uint8Array;
}

/**
 * Validate a WHOLE MCP candidate: the exact file set ([`MCP_ALLOWED_FILES`] — `server.json`
 * required, `README.md` optional), a credential scan over EVERY allowed file's bytes (raw for
 * the sibling; raw + decoded strings for the JSON document, inside [`validateServerJson`]), and
 * then the full document gate. The one function both publish doors and the local-adopt gate
 * answer to — a bundle whose behavior is one JSON document must not smuggle scripts or extra
 * payloads beside it.
 */
export function validateCandidateFiles(files: McpCandidateFile[]): McpValidation {
  const server = files.find((f) => f.path === "server.json");
  if (server === undefined) {
    return refuse(
      "MCP_INVALID",
      "an MCP bundle carries server.json at its root — this candidate has none",
    );
  }
  const allowed = new Set<string>(MCP_ALLOWED_FILES);
  for (const file of files) {
    if (!allowed.has(file.path)) {
      return refuse(
        "MCP_INVALID",
        `an MCP bundle may hold only ${MCP_ALLOWED_FILES.join(", ")} — ${file.path} is not part of one`,
      );
    }
  }
  if (server.bytes.byteLength > MAX_SERVER_JSON_BYTES) {
    return refuse("MCP_INVALID", "server.json is too large to be a server document");
  }
  // Every sibling's bytes run the credential scan (the document runs its own, twice over, inside
  // the gate below).
  for (const file of files) {
    if (file.path === "server.json") {
      continue;
    }
    let text: string;
    try {
      text = strictUtf8.decode(file.bytes);
    } catch {
      return refuse("MCP_INVALID", `${file.path} is not valid UTF-8`);
    }
    const secret = findSecret(text);
    if (secret !== null) {
      return refuse(
        "MCP_SECRET_REFUSED",
        `${file.path} carries what looks like a credential (${secret}) — a shared bundle never holds one`,
      );
    }
  }
  return validateServerJson(server.bytes);
}

/**
 * The catalog name a server document suggests: the tail segment of its registry name (the part
 * after the one slash). The catalog's own birth-name fold (`mintCatalogName`) does the
 * sanitizing — this only decides WHICH half of the name is the candidate.
 */
export function suggestedNameFor(serverName: string): string {
  return serverName.split("/").at(-1) ?? serverName;
}
