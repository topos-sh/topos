import {
  CREDENTIAL_HEADER_NAMES as CREDENTIAL_HEADER_NAME_LIST,
  CREDENTIAL_NAME_WORDS,
  SECRET_ENTROPY,
  SECRET_PATTERNS,
} from "./secret-patterns.generated";

/**
 * THE MCP SERVER-DOCUMENT GATE — one function, used by every door that can put a `kind: 'mcp'`
 * bundle into a workspace (the session lane's publish/propose, the web import page, and the
 * registry lane's read of what is already stored).
 *
 * What a shared MCP bundle IS, here: a server an agent can run from the exact bytes the
 * workspace holds — either a REMOTE address every machine reaches over `streamable-http`, or a
 * PACKAGE every machine installs at one pinned version — with no per-machine fill-in of the
 * address itself and no credential anywhere in the document. Everything this refuses refuses for
 * one of those reasons, and every refusal is TYPED so the caller can say plainly what is wrong
 * instead of "invalid":
 *
 *  · `MCP_INVALID`              — not JSON, or the required registry fields are missing/malformed
 *  · `MCP_NO_STREAMABLE_REMOTE` — nothing to deliver: no `streamable-http` remote AND no packages
 *  · `MCP_INSECURE_URL`         — the endpoint is plain http
 *  · `MCP_URL_TEMPLATE`         — the endpoint, or a header value, carries a `{placeholder}`
 *  · `MCP_SECRET_REFUSED`       — the document carries (or reserves a slot for) a credential
 *  · `MCP_PACKAGE_UNPINNED`     — a package names no single immutable thing (a version range,
 *                                 `latest`, an OCI reference with no digest, an MCPB
 *                                 download with no hash)
 *
 * The shape rules mirror the official registry schema (2025-12-11): `name` is exactly one slash
 * between a reverse-DNS namespace and a server name (3–200 chars), `description` is 1–100
 * characters, `version` is required and names ONE version (never a range, never `latest`), and a
 * `packages[]` entry carries `registryType` + `identifier` + `transport` with STRUCTURED
 * argument/environment objects. `registryType` is OPEN-WORLD — npm, pypi, oci, nuget, mcpb and
 * whatever the registry adds next are all publishable here; whether a given machine can SET one
 * up is the client's question, answered when it renders, not this gate's. The refusal vectors
 * that pin all of it live at the repo root — `tests/fixtures/mcp/vectors.json` — and the unit
 * suite drives THIS function through every one of them, so a rule cannot change here without the
 * vector changing too.
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
 * THE TWO NAME VOCABULARIES, from the shared vectors — `tests/fixtures/mcp/secret-patterns.json`,
 * the same file the client's gate compiles in. They were written out twice, in two languages, and
 * drifted: the client's list grew a word for signing keys and this one did not, so a package slot
 * named for one, arriving with a value, passed publication here and was refused by every client's
 * update — an authority-approved bundle, permanently unplaceable, with nothing on either side to
 * say why. One source, generated into a module, drift-gated by `check:mcp-patterns`.
 *
 * Header names carry a credential BY DEFINITION — matched whole, case-insensitively, independent
 * of `isSecret`, value shape, or entropy. A literal `Authorization: Basic …` is somebody's
 * credential whatever the flags say.
 *
 * The name WORDS are read after folding a slot's name to letters and digits, so `GITHUB_TOKEN`,
 * `github-token` and `githubToken` are one word to this. A package's environment variables and
 * command-line flags are SLOTS: the machine that runs the server fills them from its own keychain,
 * which is exactly the arrangement this product wants. So a slot named for a credential passes —
 * with `isSecret` on it or not. What does not pass is such a slot arriving with the VALUE already
 * in it: those bytes travel to every machine in the workspace, and a credential that travels is a
 * credential leaked. (Remote headers answer to their own, stricter rules below: topos writes them
 * into every agent's config verbatim, so a slot there is unplaceable rather than merely unwise.)
 */
const CREDENTIAL_HEADER_NAMES = new Set<string>(CREDENTIAL_HEADER_NAME_LIST);

/** Does this env-var / flag / package-header name say it holds a credential? */
function looksLikeCredentialName(name: string): boolean {
  const lower = name.toLowerCase();
  if (CREDENTIAL_HEADER_NAMES.has(lower)) {
    return true;
  }
  const folded = lower.replace(/[^a-z0-9]/g, "");
  return CREDENTIAL_NAME_WORDS.some((word) => folded.includes(word));
}

/** The one strict decoder: invalid UTF-8 is refused, never replaced (a replacement char can hide
 * what the raw bytes spelled). */
const strictUtf8 = new TextDecoder("utf-8", { fatal: true });

/** The one transport a shared bundle can promise: the same URL works from every machine. */
export const STREAMABLE_HTTP = "streamable-http";

export type McpRefusalCode =
  | "MCP_INVALID"
  | "MCP_NO_STREAMABLE_REMOTE"
  | "MCP_INSECURE_URL"
  | "MCP_URL_TEMPLATE"
  | "MCP_SECRET_REFUSED"
  | "MCP_PACKAGE_UNPINNED";

/** A header that survived the gate: a literal name and a literal value, nothing to fill in. */
export interface McpHeader {
  name: string;
  value: string;
}

/** One `packages[]` entry that survived the gate — the facts a surface shows, nothing else. */
export interface McpPackage {
  /** Open-world: npm · pypi · oci · nuget · mcpb · whatever the registry adds next. */
  registryType: string;
  identifier: string;
  /** The pinned version, when the type carries one in a field (oci pins in the identifier). */
  version: string | null;
  /** The package transport's own type: `stdio`, `streamable-http` or `sse`. */
  transport: string;
}

/** What the preview shows and what the catalog records — derived, never echoed wholesale. */
export interface McpSummary {
  name: string;
  description: string;
  version: string;
  /**
   * The placed remote's address — NULL when the document offers only packages. A shared bundle
   * is allowed to be a thing every machine installs rather than a thing every machine dials, and
   * a surface that prints an address has to ask which it is holding.
   */
  url: string | null;
  /** The placed remote's transport, or null alongside a null `url`. */
  transport: typeof STREAMABLE_HTTP | null;
  headers: McpHeader[];
  /** Every package the document offers, in its own order. Empty for a remote-only bundle. */
  packages: McpPackage[];
  /**
   * The publisher's own `_meta["sh.topos/auth"]` word, when it says something this tier
   * understands: `"oauth"` (the agent will be sent through an authorization dance on first use
   * and can finish it alone), `"manual"` (getting in costs a person a one-time step on each
   * machine — a token, an app someone registers — so no agent completes it by itself) or
   * `"none"`. NULL means the document declared nothing — which is not the same claim as "none",
   * so it is never upgraded to one, and neither is it read as the caution `manual` carries.
   */
  authHint: "oauth" | "none" | "manual" | null;
}

/** One refused document: the machine-branchable code plus the sentence a person reads. */
export interface McpRefusal {
  ok: false;
  code: McpRefusalCode;
  message: string;
}

export type McpValidation = { ok: true; summary: McpSummary } | McpRefusal;

function refuse(code: McpRefusalCode, message: string): McpRefusal {
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

/** 64 lowercase hex — the schema's own `fileSha256` shape, and the WHOLE of a sha-256 digest. */
const FILE_HASH_HEX = /^[a-f0-9]{64}$/;

/**
 * Is the run starting at `at` an INTEGRITY DIGEST rather than a credential?
 *
 * A sha-256 is 64 hex characters, which is precisely the LONG HEX shape the entropy belt refuses
 * — and the registry format puts such digests in two ordinary, published places: an OCI reference
 * (`ghcr.io/acme/mcp@sha256:<hex>`) and an MCPB download's `fileSha256`. Both are the OPPOSITE of
 * a secret: they are how a reader proves the bytes it fetched are the bytes the publisher named.
 * Without this carve-out no pinned container image and no MCPB package could ever be published
 * here, which would make the pinning rule below unsatisfiable.
 *
 * THE RUN ITSELF MUST BE A WHOLE DIGEST — 64 lowercase hex characters, nothing more and nothing
 * less. Without that the carve-out is a prefix anyone can type: `sha256:` in front of a generated
 * key exempted the key. A digest is a fixed shape, so demanding the shape costs a publisher
 * nothing and closes the door.
 *
 * Then the CONTEXT, read from the characters immediately before the run — two spellings and only
 * these two, so nothing about the run itself relaxes:
 *
 *  · `sha256:` directly in front of it (the OCI/registry digest spelling, wherever it appears);
 *  · a JSON key ENDING in that word, then `": "`, then the run (`"fileSha256": "<hex>"`).
 *
 * Hand-walked rather than pattern-matched, character for character, because the client's gate
 * carries no regex engine and the two have to answer identically. This READS a digest somebody
 * else published; it computes none (the tier's boundary gate names this carve-out by file).
 */
function isIntegrityDigest(raw: string, at: number, run: string): boolean {
  if (!FILE_HASH_HEX.test(run)) {
    return false;
  }
  const before = raw.slice(0, at);
  if (before.slice(-7).toLowerCase() === "sha256:") {
    return true;
  }
  // `… "fileSha256"  :  " ` — the quote that opens the value, the colon, the quoted key.
  let rest = before;
  if (!rest.endsWith('"')) {
    return false;
  }
  rest = rest.slice(0, -1).trimEnd();
  if (!rest.endsWith(":")) {
    return false;
  }
  rest = rest.slice(0, -1).trimEnd();
  if (!rest.endsWith('"')) {
    return false;
  }
  rest = rest.slice(0, -1);
  return rest.slice(-6).toLowerCase() === "sha256";
}

/** The first credential the raw text carries, named for the refusal message — or null. */
export function findSecret(raw: string): string | null {
  for (const { name, regex } of compiled) {
    if (regex.test(raw)) {
      return name;
    }
  }
  for (const match of raw.matchAll(TOKEN_RUN)) {
    if (looksRandom(match[0]) && !isIntegrityDigest(raw, match.index, match[0])) {
      return "high-entropy value";
    }
  }
  return null;
}

/**
 * A JSON key whose VALUE is an integrity digest by name — the `fileSha256` family — holding a
 * value that IS one. Both halves, because the key alone is a name a publisher chooses: a slot
 * called `fileSha256` carrying anything but 64 lowercase hex is not a digest, and skipping it
 * would let the name exempt whatever was put in it.
 */
function isDigestUnderKey(key: string, value: string): boolean {
  return key.slice(-6).toLowerCase() === "sha256" && FILE_HASH_HEX.test(value);
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
  // change with the traversal mechanics. A value carries the KEY it sits under, because one
  // question about a string cannot be answered from the string alone: a 64-hex value under
  // `fileSha256` is an integrity digest, and the raw pass reads that from the bytes in front of
  // it (see `isIntegrityDigest`) where this pass has only the key. Key AND value are read here —
  // the key alone is a name a publisher chooses.
  type Item = { key: string } | { value: unknown; underKey?: string };
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
      if (item.underKey !== undefined && isDigestUnderKey(item.underKey, current)) {
        continue;
      }
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
        stack.push({ value: entry, underKey: key });
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
 * A `variables` block DECLARED at all — anything but absent or null, an empty `{}` included. A
 * header wearing one is a per-machine fill-in whether or not the slots are listed, and the
 * client's placement engine refuses exactly this shape when it renders a config entry: the gate
 * refuses it here so a bundle can never be publishable and permanently unplaceable. (The
 * remote-level rule stays the narrower `hasEntries` — nothing downstream re-checks it.)
 */
function declaresVariables(value: unknown): boolean {
  return value !== undefined && value !== null;
}

/** What one remote's check answers: its headers, or the refusal the whole document takes. */
type RemoteCheck = { ok: true; headers: McpHeader[] } | McpRefusal;

// ── Packages: one immutable thing per entry ─────────────────────────────────────────────────

/**
 * The version string a range would be spelled as. Reimplemented from the official registry's own
 * rules (modelcontextprotocol/registry, MIT→Apache-2.0 transition — read, not vendored), because
 * a range is a promise this product cannot keep: "the same bytes everywhere" ends the moment a
 * machine resolves `^1.2.3` on the day it happens to install.
 *
 *  · comparator ranges — `^1.2.3` · `~1.2` · `>=1.0.0` · `<2` · `=1.0.0`
 *  · hyphen ranges — `1.2.3 - 1.4.0`
 *  · OR ranges — `1.2 || 1.3`
 *  · wildcards in a dotted version — `1.x` · `1.2.*` · `1.X`
 */
const COMPARATOR_RANGE = /^\s*(?:\^|~|>=|<=|>|<|=)\s*v?\d+(?:\.\d+){0,3}(?:-[0-9A-Za-z.-]+)?\s*$/;
const HYPHEN_RANGE =
  /^\s*v?\d+(?:\.\d+){0,3}(?:-[0-9A-Za-z.-]+)?\s-\s*v?\d+(?:\.\d+){0,3}(?:-[0-9A-Za-z.-]+)?\s*$/;
const OR_RANGE =
  /^\s*(?:v?\d+(?:\.\d+){0,3}(?:-[0-9A-Za-z.-]+)?\s*)(?:\|\|\s*v?\d+(?:\.\d+){0,3}(?:-[0-9A-Za-z.-]+)?\s*)+$/;
const DOTTED_VERSION_LIKE = /^\s*(?:v?\d+|x|X|\*)(?:\.(?:\d+|x|X|\*)){1,2}(?:-[0-9A-Za-z.-]+)?\s*$/;

export function looksLikeVersionRange(version: string): boolean {
  const trimmed = version.trim();
  if (trimmed.length === 0) {
    return false;
  }
  if (COMPARATOR_RANGE.test(trimmed) || HYPHEN_RANGE.test(trimmed) || OR_RANGE.test(trimmed)) {
    return true;
  }
  return (
    DOTTED_VERSION_LIKE.test(trimmed) &&
    (trimmed.includes("x") || trimmed.includes("X") || trimmed.includes("*"))
  );
}

/** The moving pointer the registry reserves — never a version anyone can pin to. */
const RESERVED_VERSION = "latest";

/**
 * ONE EXACT VERSION, read POSITIVELY — the grammar the registry itself publishes under, not a list
 * of the range spellings anyone thought of.
 *
 * A blacklist of ranges is the wrong shape for this rule and was: `*`, `x` and a dist-tag like
 * `next` are none of the listed range spellings, so they passed as "exact" — and npm resolves each
 * of them to whatever is current on the day a given machine installs, which is precisely the thing
 * "the same bytes everywhere" forbids. For the two registries this build knows the version grammar
 * of, the version must therefore MATCH that grammar and nothing else is accepted. Every other
 * `registryType` stays open-world and keeps the range belt below — this build cannot claim to know
 * a grammar it has never read.
 *
 * Hand-parsed rather than pattern-matched, character for character, because the client's gate
 * carries no regex engine and the two have to answer identically over the shared vectors.
 */

/** A semver numeric identifier: digits, and no leading zero past a bare `0`. */
function isNumericIdentifier(part: string): boolean {
  if (part.length === 0 || !/^[0-9]+$/.test(part)) {
    return false;
  }
  return part === "0" || !part.startsWith("0");
}

/** A dot-separated run of `[0-9A-Za-z-]` identifiers, each non-empty — a build metadata tail. */
function isDotSeparatedIdentifiers(text: string, numericRule: boolean): boolean {
  if (text.length === 0) {
    return false;
  }
  return text.split(".").every((part) => {
    if (part.length === 0 || !/^[0-9A-Za-z-]+$/.test(part)) {
      return false;
    }
    // A pre-release identifier that is all digits is a NUMBER, and semver forbids leading zeros
    // on one. Build metadata has no such rule — `+build.007` is legal.
    return !numericRule || !/^[0-9]+$/.test(part) || isNumericIdentifier(part);
  });
}

/** `X.Y.Z`, optionally `-prerelease` and `+build` — strict semver, exactly what npm publishes. */
export function isExactSemver(version: string): boolean {
  const plus = version.indexOf("+");
  const beforeBuild = plus === -1 ? version : version.slice(0, plus);
  if (plus !== -1 && !isDotSeparatedIdentifiers(version.slice(plus + 1), false)) {
    return false;
  }
  const dash = beforeBuild.indexOf("-");
  const core = dash === -1 ? beforeBuild : beforeBuild.slice(0, dash);
  if (dash !== -1 && !isDotSeparatedIdentifiers(beforeBuild.slice(dash + 1), true)) {
    return false;
  }
  const parts = core.split(".");
  return parts.length === 3 && parts.every(isNumericIdentifier);
}

/** How many digits `text` carries from `at`. */
function digitRun(text: string, at: number): number {
  let n = 0;
  while (at + n < text.length) {
    const code = text.charCodeAt(at + n);
    if (code < 48 || code > 57) {
      break;
    }
    n += 1;
  }
  return n;
}

/** PEP 440's separator between a release and the segment that follows it. */
function isPep440Separator(ch: string | undefined): boolean {
  return ch === "-" || ch === "_" || ch === ".";
}

/** One optional `<sep?><label><sep?><digits?>` segment; returns the position after it, or `at`. */
function readLabelled(text: string, at: number, labels: readonly string[]): number {
  let p = at;
  if (isPep440Separator(text[p])) {
    p += 1;
  }
  for (const label of labels) {
    if (!text.startsWith(label, p)) {
      continue;
    }
    let q = p + label.length;
    if (isPep440Separator(text[q])) {
      q += 1;
    }
    return q + digitRun(text, q);
  }
  return at;
}

/**
 * A PEP 440 public version, and only one: `[N!]N(.N)*[{a|b|rc}N][.postN][.devN][+local]`. No
 * comparison operator, no `*`, no dist-tag — those are the spellings that name a SET.
 */
export function isExactPep440(version: string): boolean {
  const text = version.toLowerCase();
  let pos = 0;
  if (text[pos] === "v") {
    pos += 1;
  }
  // An epoch, when one is spelled: digits then `!`.
  const epoch = digitRun(text, pos);
  if (epoch > 0 && text[pos + epoch] === "!") {
    pos += epoch + 1;
  }
  // The release segment: at least one number, then any run of `.N`.
  const first = digitRun(text, pos);
  if (first === 0) {
    return false;
  }
  pos += first;
  for (;;) {
    if (text[pos] !== ".") {
      break;
    }
    const next = digitRun(text, pos + 1);
    if (next === 0) {
      break;
    }
    pos += 1 + next;
  }
  pos = readLabelled(text, pos, ["preview", "pre", "alpha", "beta", "rc", "a", "b", "c"]);
  // The implicit post-release spelling (`1.0-1`) comes before the labelled one.
  const implicitPost = text[pos] === "-" ? digitRun(text, pos + 1) : 0;
  pos = implicitPost > 0 ? pos + 1 + implicitPost : readLabelled(text, pos, ["post", "rev", "r"]);
  pos = readLabelled(text, pos, ["dev"]);
  if (text[pos] === "+") {
    const local = text.slice(pos + 1);
    if (local.length === 0) {
      return false;
    }
    return local
      .split(/[-_.]/)
      .every((segment) => segment.length > 0 && /^[a-z0-9]+$/.test(segment));
  }
  return pos === text.length;
}

/** Whether this registry's version grammar is one this build reads — and how. */
function exactVersionOf(registryType: string): ((version: string) => boolean) | null {
  if (registryType === "npm") {
    return isExactSemver;
  }
  if (registryType === "pypi") {
    return isExactPep440;
  }
  return null;
}

/** A sha-256 digest as the OCI reference spells it. */
const OCI_DIGEST = /@sha256:[a-f0-9]{64}$/;

/**
 * Does this OCI reference name ONE image? The version lives inside the identifier for this type
 * (the registry's own rule: an OCI package carries no `version` field), so the answer is a
 * `@sha256:` DIGEST and nothing else.
 *
 * A TAG IS NOT A PIN. `ghcr.io/acme/mcp:1.0.0` reads like a version and is a movable label: the
 * publisher can point it at other bytes tomorrow, and two machines installing this bundle a week
 * apart would then run different code. `latest` is only the most obvious spelling of that; the
 * problem is the tag, not the word. A digest IS the bytes, which is the same promise a version
 * hash makes everywhere else in this product.
 */
function ociReferenceIsPinned(identifier: string): boolean {
  return OCI_DIGEST.test(identifier);
}

/** The transports a PACKAGE may declare (the schema's `LocalTransport`). */
const PACKAGE_TRANSPORTS = new Set(["stdio", STREAMABLE_HTTP, "sse"]);

/**
 * One KeyValueInput / Argument that arrives with its VALUE filled in — the shape the credential
 * rule below judges. A slot with no value is a slot, which is fine.
 *
 * `default` COUNTS. The official format states it as the value used when nothing else supplies
 * one, and that is exactly what a client does with it: the rendered agent config carries the
 * default verbatim. So a credential parked in `default` travels to every machine just as one
 * parked in `value` does. The two are one question, and this is the one place it is asked.
 */
function literalValueOf(entry: Record<string, unknown>): string | null {
  for (const key of ["value", "default"] as const) {
    const value = entry[key];
    if (typeof value === "string" && value.length > 0) {
      return value;
    }
  }
  return null;
}

/**
 * The named-slot rule, one sentence for env vars, flags and package headers alike.
 *
 * A slot is judged by what the DOCUMENT says about it and by what its NAME says, in that order.
 * `isSecret: true` is the publisher stating outright that the slot holds a credential — a
 * declaration, and fine on its own, because a slot with no value is exactly the arrangement this
 * product wants. With a value already in it, the declaration names what is travelling: refused
 * whatever the slot is called, because a heuristic over names is a belt and the publisher's own
 * word is not.
 */
function checkNamedSlot(entry: Record<string, unknown>, what: string): McpRefusal | null {
  const name = entry.name;
  if (typeof name !== "string" || name.length === 0) {
    return refuse("MCP_INVALID", `every ${what} needs a name`);
  }
  const filled = literalValueOf(entry) !== null;
  if (entry.isSecret === true && filled) {
    return refuse(
      "MCP_SECRET_REFUSED",
      `the ${what} ${name} is declared secret and arrives with its value filled in — a shared bundle names the slot and lets each machine fill it`,
    );
  }
  if (looksLikeCredentialName(name) && filled) {
    return refuse(
      "MCP_SECRET_REFUSED",
      `the ${what} ${name} arrives with its value filled in — a shared bundle names the slot and lets each machine fill it`,
    );
  }
  return null;
}

/** The `arguments[]` rules: STRUCTURED objects, per the registry schema — never bare strings. */
function checkArguments(value: unknown, what: string): McpRefusal | null {
  if (value === undefined || value === null) {
    return null;
  }
  if (!Array.isArray(value)) {
    return refuse("MCP_INVALID", `${what} is a list of argument objects`);
  }
  for (const entry of value) {
    if (!isRecord(entry)) {
      return refuse(
        "MCP_INVALID",
        `${what} holds argument objects, not plain strings — each one states its type`,
      );
    }
    if (entry.type === "named") {
      const named = checkNamedSlot(entry, "flag");
      if (named !== null) {
        return named;
      }
      continue;
    }
    if (entry.type !== "positional") {
      return refuse("MCP_INVALID", `every ${what} entry is a positional or a named argument`);
    }
    const hint = entry.valueHint;
    if (literalValueOf(entry) === null && (typeof hint !== "string" || hint.length === 0)) {
      return refuse("MCP_INVALID", `a positional argument needs a value or a valueHint`);
    }
  }
  return null;
}

/**
 * ONE `packages[]` entry, judged: the registry's required fields, a transport this format
 * knows, structured arguments, credential-free filled-in slots, and — the rule this product adds
 * — ONE IMMUTABLE THING per entry.
 *
 * The pinning rule is per registry type because the formats pin differently: npm and pypi carry
 * a `version` field, an OCI reference carries its digest inside the identifier, and an
 * MCPB download is identified by the hash of the file. A type this build has never heard of is
 * still publishable (the vocabulary is open-world) and is held to the one rule that reads the
 * same everywhere: a `version` it does state must name a single version.
 */
function checkPackage(entry: unknown): { ok: true; summary: McpPackage } | McpRefusal {
  if (!isRecord(entry)) {
    return refuse("MCP_INVALID", "every packages[] entry is an object");
  }
  const registryType = entry.registryType;
  if (typeof registryType !== "string" || registryType.length === 0) {
    return refuse("MCP_INVALID", "every package says which registry it comes from (registryType)");
  }
  const identifier = entry.identifier;
  if (typeof identifier !== "string" || identifier.trim().length === 0) {
    return refuse("MCP_INVALID", "every package needs an identifier");
  }
  if (/\s/.test(identifier)) {
    return refuse("MCP_INVALID", "a package identifier carries no spaces");
  }
  const transport = entry.transport;
  if (!isRecord(transport) || typeof transport.type !== "string") {
    return refuse("MCP_INVALID", "every package declares how it is spoken to (transport)");
  }
  if (!PACKAGE_TRANSPORTS.has(transport.type)) {
    return refuse(
      "MCP_INVALID",
      `a package transport is stdio, ${STREAMABLE_HTTP} or sse — ${transport.type} is none of those`,
    );
  }
  if (transport.type !== "stdio") {
    const url = transport.url;
    if (typeof url !== "string" || url.length === 0) {
      return refuse("MCP_INVALID", "a package that speaks http declares the URL it listens on");
    }
    // The SAME rules a remote's address answers to — see `checkEndpointUrl`.
    const bad = checkEndpointUrl(url);
    if (bad !== null) {
      return bad;
    }
  }
  // A package's own transport headers are the machine's to fill; a filled-in credential is not.
  const transportHeaders = transport.headers;
  if (transportHeaders !== undefined && transportHeaders !== null) {
    if (!Array.isArray(transportHeaders)) {
      return refuse("MCP_INVALID", "a transport's headers are a list");
    }
    for (const header of transportHeaders) {
      if (!isRecord(header)) {
        return refuse("MCP_INVALID", "every header is an object");
      }
      const checked = checkNamedSlot(header, "header");
      if (checked !== null) {
        return checked;
      }
    }
  }
  const environment = entry.environmentVariables;
  if (environment !== undefined && environment !== null) {
    if (!Array.isArray(environment)) {
      return refuse("MCP_INVALID", "environmentVariables is a list of named variables");
    }
    for (const variable of environment) {
      if (!isRecord(variable)) {
        return refuse("MCP_INVALID", "every environment variable is an object with a name");
      }
      const checked = checkNamedSlot(variable, "environment variable");
      if (checked !== null) {
        return checked;
      }
    }
  }
  for (const [field, list] of [
    ["runtimeArguments", entry.runtimeArguments],
    ["packageArguments", entry.packageArguments],
  ] as const) {
    const checked = checkArguments(list, field);
    if (checked !== null) {
      return checked;
    }
  }

  const rawVersion = entry.version;
  if (rawVersion !== undefined && rawVersion !== null && typeof rawVersion !== "string") {
    return refuse("MCP_INVALID", "a package version is a string");
  }
  const version = typeof rawVersion === "string" && rawVersion.length > 0 ? rawVersion : null;
  const type = registryType.toLowerCase();
  const exact = exactVersionOf(type);
  if (type === "oci") {
    if (!ociReferenceIsPinned(identifier)) {
      return refuse(
        "MCP_PACKAGE_UNPINNED",
        `${identifier} names no one image — an OCI package pins itself with an @sha256: digest, because a tag can be moved to other bytes`,
      );
    }
  } else if (type === "mcpb") {
    const hash = entry.fileSha256;
    if (typeof hash !== "string" || !FILE_HASH_HEX.test(hash)) {
      return refuse(
        "MCP_PACKAGE_UNPINNED",
        "an MCPB package is identified by the sha-256 of its file (fileSha256) — without it nobody can tell which bytes arrived",
      );
    }
  } else if (exact !== null && version === null) {
    return refuse(
      "MCP_PACKAGE_UNPINNED",
      `a ${type} package names the exact version every machine installs`,
    );
  }
  if (version !== null) {
    if (version === RESERVED_VERSION) {
      return refuse(
        "MCP_PACKAGE_UNPINNED",
        `${RESERVED_VERSION} is not a version — it is whatever the registry serves today`,
      );
    }
    if (exact === null) {
      // An open-world registry type: this build has not read its version grammar, so the honest
      // rule is the one that reads the same everywhere — a spelling that names a SET refuses.
      if (looksLikeVersionRange(version)) {
        return refuse(
          "MCP_PACKAGE_UNPINNED",
          `${version} is a range, not a version — a shared bundle installs the same bytes on every machine`,
        );
      }
    } else if (!exact(version)) {
      return refuse(
        "MCP_PACKAGE_UNPINNED",
        `${version} is not one exact version — a ${type} package names the one version every machine installs`,
      );
    }
    if (codePointLength(version) > VERSION_MAX) {
      return refuse("MCP_INVALID", "a package version is too long to be one");
    }
  }
  return {
    ok: true,
    summary: { registryType, identifier, version, transport: transport.type },
  };
}

/**
 * The one sentence both tiers say when an endpoint is spelled in a way they would read
 * differently — same words, same code, either side of the wire.
 */
const AMBIGUOUS_ENDPOINT_MESSAGE =
  "the endpoint is not a plain address — a shared endpoint spells scheme://host, with no backslash, tab or line break anywhere in it";

/**
 * Is this endpoint spelled so that EVERY reader gets the same address out of it?
 *
 * This tier hands its URLs to `new URL()` (a WHATWG parser) and the client hand-parses them, and
 * the two disagree on three spellings — each one a repair the WHATWG parser performs silently:
 *
 *  · **no `://`** — `https:example.com/mcp` gains its slashes from the special-scheme rule and
 *    parses as `https://example.com/mcp`; the client sees no authority at all.
 *  · **a backslash** — it ENDS the authority (`https://h\@x.example/mcp` is host `h`, no
 *    userinfo); the client splits at the last `@`, reads `h\` as a user name, and refuses the
 *    document as carrying a credential.
 *  · **a tab, CR or LF** — deleted from the input BEFORE parsing, so `https://x<TAB>.example/mcp`
 *    is the host `x.example`; the client refuses whitespace in a host.
 *
 * None of the three is an address anyone means to write, and each one lands a different verdict
 * per language — a document this tier accepts would be refused by every member's converge, every
 * sweep, forever. So both tiers refuse the SHAPE up front rather than each parsing its own
 * reading. Mirrors the client's `is_unambiguous_endpoint` exactly.
 */
function endpointIsUnambiguous(url: string): boolean {
  return url.includes("://") && !/[\\\t\r\n]/.test(url);
}

/**
 * How many CODE POINTS a string carries — the unit the client's gate counts in (`chars()`), and
 * the one a person means by "characters". `.length` counts UTF-16 units instead, so a single
 * emoji would cost two and this tier would refuse a description the client had already accepted.
 */
function codePointLength(text: string): number {
  return [...text].length;
}

/**
 * THE ADDRESS RULES, one function for every endpoint a document names — a `remotes[]` entry's and
 * a package transport's alike. A URL is a URL: an http one is as much in the clear when a package
 * declares it, `{tenant}` is as unfillable, and `user:pass@` is as much somebody's credential.
 * Judging one of the two and not the other left a package declaring `not a URL` publishable and
 * `https://user:pass@host` a way past the explicit URL credential rule.
 */
function checkEndpointUrl(url: string): McpRefusal | null {
  // The template check comes before the URL parse: `https://{tenant}.example/mcp` PARSES, and
  // would otherwise pass as an https address whose host is a literal brace word.
  if (/[{}]/.test(url)) {
    return refuse(
      "MCP_URL_TEMPLATE",
      "the endpoint carries a {placeholder} — it is a template, not an address every machine can use",
    );
  }
  // BEFORE the parse, and BEFORE the userinfo rule: the spellings the two tiers would read
  // DIFFERENTLY. Answering the same code on both is what this ordering buys — a document
  // refused here can never be published on one tier and then refuse forever on the other.
  if (!endpointIsUnambiguous(url)) {
    return refuse("MCP_INVALID", AMBIGUOUS_ENDPOINT_MESSAGE);
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
  return null;
}

/**
 * The hygiene and credential rules ONE `remotes[]` entry answers to, whichever entry it is: the
 * address is a plain https URL with no template, no userinfo and a host that parses; the entry
 * reserves no per-installation fill-in; and every header carries a literal, credential-free value.
 * Returns that entry's headers, so the placed remote's survive into the summary.
 */
function checkRemote(remote: Record<string, unknown>): RemoteCheck {
  // An entry with no string `url` names no address — there is nothing to judge but its headers.
  if (typeof remote.url === "string") {
    const bad = checkEndpointUrl(remote.url);
    if (bad !== null) {
      return bad;
    }
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
    if (declaresVariables(entry.variables)) {
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
    // A brace pair in the value is the header's own template: something fills it in per machine,
    // which is the one thing a shared bundle cannot carry. The client's placement engine refuses
    // this shape too — the gate refuses it first, so the refusal reaches the author instead of
    // every member's sweep.
    if (entry.value.includes("{") && entry.value.includes("}")) {
      return refuse(
        "MCP_URL_TEMPLATE",
        `the header ${entry.name} carries a {placeholder} — it is a template, not a value every machine can send`,
      );
    }
    headers.push({ name: entry.name, value: entry.value });
  }
  return { ok: true, headers };
}

/**
 * Validate one server document. `raw` is the bytes as fetched or pasted — the scan reads THEM,
 * not a re-serialization, so nothing a caller strips can hide a credential from it.
 */
export function validateServerJson(raw: Uint8Array | string): McpValidation {
  let text: string;
  if (typeof raw === "string") {
    text = raw;
    // The ceiling is on BYTES, and a paste is counted in characters: a document of multibyte text
    // can sit under any character limit a form imposes and still be over this cap once encoded.
    // Refusing here is what keeps a preview from promising what the publish door will refuse.
    // (UTF-16 length is a lower bound on the UTF-8 byte count, so an oversize string never has to
    // be encoded at all.)
    if (
      text.length > MAX_SERVER_JSON_BYTES ||
      new TextEncoder().encode(text).byteLength > MAX_SERVER_JSON_BYTES
    ) {
      return refuse("MCP_INVALID", "server.json is too large to be a server document");
    }
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

  // Every bound below counts CODE POINTS, the unit the client's gate counts in — see
  // `codePointLength`. A limit that means one thing here and another there is a document one
  // tier publishes and the other refuses.
  const name = parsed.name;
  if (
    typeof name !== "string" ||
    codePointLength(name) < NAME_MIN ||
    codePointLength(name) > NAME_MAX
  ) {
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
    codePointLength(description) > DESCRIPTION_MAX
  ) {
    return refuse("MCP_INVALID", `description is required, 1–${DESCRIPTION_MAX} characters`);
  }
  const version = parsed.version;
  if (
    typeof version !== "string" ||
    version.length === 0 ||
    codePointLength(version) > VERSION_MAX
  ) {
    return refuse("MCP_INVALID", "version is required");
  }
  // ONE version, the server's own: `latest` names whatever is served today and a range names a
  // set, and neither is a thing a workspace can pin its members to.
  if (version === RESERVED_VERSION || looksLikeVersionRange(version)) {
    return refuse(
      "MCP_INVALID",
      `version names one version — ${version} names whichever one a machine resolves`,
    );
  }

  const rawPackages = parsed.packages;
  if (rawPackages !== undefined && rawPackages !== null && !Array.isArray(rawPackages)) {
    return refuse("MCP_INVALID", "packages is a list");
  }
  const rawRemotes = parsed.remotes;
  if (rawRemotes !== undefined && rawRemotes !== null && !Array.isArray(rawRemotes)) {
    return refuse("MCP_INVALID", "remotes is a list");
  }
  const packageList: unknown[] = Array.isArray(rawPackages) ? rawPackages : [];
  const remotes: unknown[] = Array.isArray(rawRemotes) ? rawRemotes : [];

  // FIRST streamable-http wins: a document may offer several transports, and the ordering is
  // the publisher's own preference. Which one is PLACED is decided here; which ones are JUDGED
  // is every one of them, below.
  const placed = remotes.findIndex(
    (entry): entry is Record<string, unknown> =>
      isRecord(entry) && entry.type === STREAMABLE_HTTP && typeof entry.url === "string",
  );
  // NOTHING TO DELIVER is the refusal, not "no remote": a document may offer an address, a set of
  // packages, or both. Only a document offering neither leaves an agent with nothing to run.
  if (placed === -1 && packageList.length === 0) {
    return refuse(
      "MCP_NO_STREAMABLE_REMOTE",
      "no streamable-http remote and no packages — there is nothing here for an agent to run",
    );
  }

  // EVERY remote runs the hygiene and credential rules, in document order — not just the one
  // that gets placed. A sibling entry (an `sse` fallback, a second endpoint) travels in the same
  // bytes to the same machines, so a credential in its address or its headers is shared exactly
  // as widely as one in the placed remote's.
  let headers: McpHeader[] = [];
  for (const [i, entry] of remotes.entries()) {
    const checked = checkRemote(isRecord(entry) ? entry : {});
    if (!checked.ok) {
      return checked;
    }
    if (i === placed) {
      headers = checked.headers;
    }
  }
  const url = placed === -1 ? null : ((remotes[placed] as Record<string, unknown>).url as string);

  // …and every PACKAGE the same way, in document order. What a given machine can actually set up
  // is the client's question at render time; publishable is the question here.
  const packages: McpPackage[] = [];
  for (const entry of packageList) {
    const checked = checkPackage(entry);
    if (!checked.ok) {
      return checked;
    }
    packages.push(checked.summary);
  }

  // A CLOSED vocabulary read off an open field: three words this tier knows, and anything else —
  // a spelling from a newer publisher, a typo, a `true` — reads as no declaration at all rather
  // than as one of them.
  const meta = isRecord(parsed._meta) ? parsed._meta : {};
  const declared = meta["sh.topos/auth"];
  const authHint =
    declared === "oauth" || declared === "none" || declared === "manual" ? declared : null;

  return {
    ok: true,
    summary: {
      name,
      description,
      version,
      url,
      transport: url === null ? null : STREAMABLE_HTTP,
      headers,
      packages,
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
