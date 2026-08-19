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
 * onto anything at all over a version this install authored, without a deliberate override).
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
 * The sources a current revision can carry that this install AUTHORED — a hand-seeded catalog
 * entry, a staff correction, an owner's private edit. Upstream never supersedes one of these on
 * its own; that is a deliberate act with a person's name on it.
 */
const STAFF_AUTHORED_SOURCES: ReadonlySet<string> = new Set(["staff", "owner", "seed"]);

/** Was this revision written by this install rather than pulled from upstream? */
export function isStaffAuthoredSource(source: string): boolean {
  return STAFF_AUTHORED_SOURCES.has(source);
}

interface ParsedVersion {
  /** The dotted numeric release identifiers, e.g. `1.2.3` → `[1, 2, 3]`. */
  release: number[];
  /** The dot-separated pre-release identifiers, or null for a plain release. */
  prerelease: string[] | null;
}

/**
 * Parse the subset of the version grammar the registry publishes under — a `vX.Y.Z` release with
 * an optional `-prerelease`, build metadata (`+…`) discarded as semver says it must be. A version
 * whose release core is not numeric is not one this can order, and returns null so the caller
 * keeps what it already holds rather than guessing.
 */
function parseVersion(raw: string): ParsedVersion | null {
  let text = raw.trim();
  if (text.startsWith("v") || text.startsWith("V")) {
    text = text.slice(1);
  }
  const plus = text.indexOf("+");
  if (plus >= 0) {
    text = text.slice(0, plus);
  }
  const dash = text.indexOf("-");
  const core = dash >= 0 ? text.slice(0, dash) : text;
  const pre = dash >= 0 ? text.slice(dash + 1) : "";
  const parts = core.split(".");
  const release: number[] = [];
  for (const part of parts) {
    if (!/^\d+$/.test(part)) {
      return null;
    }
    release.push(Number(part));
  }
  if (release.length === 0) {
    return null;
  }
  return { release, prerelease: pre.length === 0 ? null : pre.split(".") };
}

/** Compare two release cores, padding the shorter with zeros — `1.2` and `1.2.0` are one. */
function compareRelease(a: number[], b: number[]): number {
  const width = Math.max(a.length, b.length);
  for (let i = 0; i < width; i += 1) {
    const x = a[i] ?? 0;
    const y = b[i] ?? 0;
    if (x !== y) {
      return x < y ? -1 : 1;
    }
  }
  return 0;
}

/** Semver pre-release ordering: a release outranks any pre-release of it; identifiers compare
 * numerically when both are numeric, and a shorter run of identifiers is the smaller one. */
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
    const xNum = /^\d+$/.test(x);
    const yNum = /^\d+$/.test(y);
    if (xNum && yNum) {
      const diff = Number(x) - Number(y);
      if (diff !== 0) {
        return diff < 0 ? -1 : 1;
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
