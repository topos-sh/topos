/**
 * PRECEDENCE — which of two revisions of one server is the one to hold on to.
 *
 * The catalog treats the official registry as ONE INPUT, never the authority. A swept document is
 * READ so this install can see what upstream offers — even when it declares an older `$schema`
 * than the canonical one — but it DISPLACES what this install already holds only when it is
 * STRICTLY NEWER by the signals that describe a server's capability: the server's own `version`,
 * and, to break a version tie, the MCP protocol revisions its endpoint answered a probe on.
 *
 * The `$schema` string never factors in. It is the document's metadata FORMAT, not a freshness
 * signal: an older-schema document carrying a newer server version is a real update; a newer-schema
 * document carrying the same or an older server version is not. When two revisions cannot be
 * ordered — an unparseable version, a missing probe on either side — ours stands. A catalog never
 * moves backward, or sideways, on a guess.
 *
 * This module is PURE comparison, shared by the two places precedence is decided: the upstream
 * sweep (which files a version it cannot better than ours as informational, never as a downgrade)
 * and the accept door (which refuses to move the pointer onto one that is not strictly newer, or
 * onto anything at all over a version a PERSON authored here, without a deliberate override — a
 * seed PLACEHOLDER is neither, and any candidate freely supersedes it).
 */

/** The facts precedence reads off one revision — nothing about its schema or its prose. */
export interface McpPrecedenceFacts {
  /** The `version` string inside the document — the server's own version. */
  version: string;
  /**
   * The MCP protocol revisions this endpoint answered a probe with (an array of revision strings),
   * or anything else when none was captured. Read defensively: absent is an ambiguity, not a zero.
   */
  protocolVersions: unknown;
}

/**
 * The sources a current revision can carry that a PERSON authored — a staff correction, an owner's
 * private edit. Upstream never supersedes one of these on its own; that is a deliberate act with a
 * person's name on it, and displacing it takes a deliberate override.
 *
 * A `seed` is deliberately NOT here. The catalog's migration stamps every curated server with a
 * placeholder version under `source = 'seed'` — a starting point, not a decision somebody made — so
 * it is a thing to be replaced the moment a real upstream document arrives, never a statement to
 * protect. `isSeedPlaceholder` names that case for the two callers that must let any candidate pass.
 */
const STAFF_AUTHORED_SOURCES: ReadonlySet<string> = new Set(["staff", "owner"]);

/** Was this revision written by a PERSON here, rather than pulled from upstream or seeded? */
export function isStaffAuthoredSource(source: string): boolean {
  return STAFF_AUTHORED_SOURCES.has(source);
}

/**
 * Is this current a SEED PLACEHOLDER — the version the catalog's migration stamped on a curated
 * server before any real document arrived? Such a current is freely superseded by any candidate: it
 * never requires an override and never causes an upstream version to be judged "behind", because
 * there is no real prior to move backward from. The accept guard and the upstream sweep both read
 * this to let a first real document land.
 */
export function isSeedPlaceholder(source: string): boolean {
  return source === "seed";
}

interface ParsedVersion {
  /** The dotted numeric release identifiers, kept as decimal STRINGS so an identifier past the
   *  safe-integer range still orders losslessly — e.g. `1.2.3` → `["1", "2", "3"]`. */
  release: string[];
  /** The dot-separated pre-release identifiers, or null for a plain release. */
  prerelease: string[] | null;
}

/** A run of one or more decimal digits, nothing else. */
const NUMERIC = /^\d+$/;
/** One pre-release identifier's alphabet — the semver set, and non-empty. */
const PRERELEASE_ID = /^[0-9A-Za-z-]+$/;

/**
 * Parse the subset of the version grammar the registry publishes under — a `vX.Y.Z` release with
 * an optional `-prerelease`, build metadata (`+…`) discarded as semver says it must be.
 *
 * MALFORMED IS AMBIGUOUS, NOT A RELEASE. A non-numeric release identifier, a trailing dash with no
 * pre-release after it (`2.0.0-`), or a pre-release with an empty/illegal identifier all return
 * null — the caller reads that as "cannot say" and keeps what it already holds rather than letting
 * a broken label read as a clean release that could supersede a real one.
 */
function parseVersion(raw: string): ParsedVersion | null {
  let text = raw.trim();
  if (text === "") {
    return null;
  }
  if (text.startsWith("v") || text.startsWith("V")) {
    text = text.slice(1);
  }
  const plus = text.indexOf("+");
  if (plus >= 0) {
    text = text.slice(0, plus);
  }
  const dash = text.indexOf("-");
  const core = dash >= 0 ? text.slice(0, dash) : text;
  // `null` = no pre-release at all; `""` = a dash with nothing after it, which is malformed.
  const pre = dash >= 0 ? text.slice(dash + 1) : null;
  const release: string[] = [];
  for (const part of core.split(".")) {
    if (!NUMERIC.test(part)) {
      return null;
    }
    release.push(part);
  }
  if (release.length === 0) {
    return null;
  }
  let prerelease: string[] | null = null;
  if (pre !== null) {
    if (pre.length === 0) {
      return null;
    }
    const ids = pre.split(".");
    for (const id of ids) {
      if (!PRERELEASE_ID.test(id)) {
        return null;
      }
    }
    prerelease = ids;
  }
  return { release, prerelease };
}

/** Order two non-negative decimal strings by MAGNITUDE, losslessly — no `Number`, so identifiers
 * past `Number.MAX_SAFE_INTEGER` still compare correctly. */
function compareNumeric(a: string, b: string): number {
  const x = a.replace(/^0+(?=\d)/, "");
  const y = b.replace(/^0+(?=\d)/, "");
  if (x.length !== y.length) {
    return x.length < y.length ? -1 : 1;
  }
  return x === y ? 0 : x < y ? -1 : 1;
}

/** Compare two release cores, padding the shorter with zeros — `1.2` and `1.2.0` are one. */
function compareRelease(a: string[], b: string[]): number {
  const width = Math.max(a.length, b.length);
  for (let i = 0; i < width; i += 1) {
    const cmp = compareNumeric(a[i] ?? "0", b[i] ?? "0");
    if (cmp !== 0) {
      return cmp;
    }
  }
  return 0;
}

/** Semver pre-release ordering: a release outranks any pre-release of it; numeric identifiers
 * compare by magnitude (losslessly), numeric outranks nothing and is outranked by alphanumeric,
 * and a shorter run of identifiers is the smaller one. */
function comparePrerelease(a: string[] | null, b: string[] | null): number {
  if (a === null && b === null) {
    return 0;
  }
  if (a === null) {
    return 1;
  }
  if (b === null) {
    return -1;
  }
  const width = Math.max(a.length, b.length);
  for (let i = 0; i < width; i += 1) {
    const x = a[i];
    const y = b[i];
    if (x === undefined) {
      return -1;
    }
    if (y === undefined) {
      return 1;
    }
    const xNum = NUMERIC.test(x);
    const yNum = NUMERIC.test(y);
    if (xNum && yNum) {
      const cmp = compareNumeric(x, y);
      if (cmp !== 0) {
        return cmp;
      }
    } else if (xNum) {
      return -1;
    } else if (yNum) {
      return 1;
    } else if (x !== y) {
      return x < y ? -1 : 1;
    }
  }
  return 0;
}

/**
 * Order two server versions: -1 / 0 / 1, or NULL when either is not a version this can read. The
 * null is load-bearing — every caller reads it as "cannot say", and cannot-say keeps ours.
 */
export function compareServerVersion(a: string, b: string): number | null {
  const parsedA = parseVersion(a);
  const parsedB = parseVersion(b);
  if (parsedA === null || parsedB === null) {
    return null;
  }
  const byRelease = compareRelease(parsedA.release, parsedB.release);
  return byRelease !== 0 ? byRelease : comparePrerelease(parsedA.prerelease, parsedB.prerelease);
}

/**
 * The newest MCP protocol revision a probe captured, or null. The revisions are date-shaped
 * strings (`2025-06-18`), which sort chronologically as they sort lexically, so the max string is
 * the newest revision the endpoint reached.
 */
function newestProtocol(protocolVersions: unknown): string | null {
  if (!Array.isArray(protocolVersions)) {
    return null;
  }
  let newest: string | null = null;
  for (const entry of protocolVersions) {
    if (typeof entry === "string" && entry.length > 0 && (newest === null || entry > newest)) {
      newest = entry;
    }
  }
  return newest;
}

/**
 * Is `candidate` strictly newer than `current` — the one question every precedence decision asks?
 *
 * A strictly greater server version wins outright; an older one never does. On an EQUAL server
 * version the tie-break is protocol support: the candidate wins only if its endpoint reached a
 * strictly newer MCP revision than ours did, and a tie or an unknown on either side is an
 * ambiguity that keeps ours. The `$schema` is not consulted anywhere in here.
 */
export function isStrictlyNewer(
  candidate: McpPrecedenceFacts,
  current: McpPrecedenceFacts,
): boolean {
  const byVersion = compareServerVersion(candidate.version, current.version);
  if (byVersion === null) {
    return false;
  }
  if (byVersion !== 0) {
    return byVersion > 0;
  }
  const theirs = newestProtocol(candidate.protocolVersions);
  const ours = newestProtocol(current.protocolVersions);
  if (theirs === null || ours === null) {
    return false;
  }
  return theirs > ours;
}
