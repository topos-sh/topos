/**
 * WHAT A PROBE RESULT READS AS — the four words the catalog says, and the one it says when there
 * is nothing to say. Pure, and deliberately not a `.server` module: the bundle page renders these
 * strings, so they are built once and shipped rather than assembled twice in two vocabularies.
 *
 * The vocabulary is the PROBE's own and touches nothing else: delivery status keeps its closed set
 * of words, and no line here is ever a claim about whether you have a version.
 */

/** The four things one probe can conclude. Stored verbatim in `web.mcp_probe.outcome`. */
export type McpProbeOutcome =
  | "responding"
  | "sign_in_required"
  | "not_verifiable"
  | "not_responding";

/**
 * The two things `not_verifiable` can MEAN, stored verbatim in `web.mcp_probe.detail` and read
 * back by the line below. One word for both was a wrong claim half the time: a public hostname
 * with a typo in it, or one that only resolves inside somebody else's network, was reported as a
 * private address — and a reader sent looking for a firewall finds nothing to fix.
 */
export const PROBE_DETAIL_PRIVATE = "private address";
export const PROBE_DETAIL_UNRESOLVED = "name does not resolve here";

/** One recorded probe, as a surface receives it. */
export interface McpProbeRecord {
  outcome: McpProbeOutcome;
  /** ISO-8601, from the database. */
  probedAt: string;
  /** The short machine-written note the probe filed with the outcome — never a body or a header. */
  detail: string | null;
}

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/**
 * The day a probe ran, in UTC. UTC and a fixed month table on purpose: this string is rendered on
 * the server and re-rendered during hydration, and a locale- or zone-dependent spelling is a
 * hydration mismatch waiting for the first reader in another timezone.
 */
export function probeDay(probedAt: string): string {
  const at = new Date(probedAt);
  if (Number.isNaN(at.getTime())) {
    return "";
  }
  return `${at.getUTCDate()} ${MONTHS[at.getUTCMonth()]} ${at.getUTCFullYear()}`;
}

/**
 * The one line a bundle page shows about its server's health. A version with NO record has not
 * been asked about — which is what a machine reads when the probe never ran, could not run, or was
 * never able to record its answer. That is a fact about this plane, never about the server.
 */
export function probeStateLine(record: McpProbeRecord | null): string {
  if (record === null) {
    return "not checked yet";
  }
  const day = probeDay(record.probedAt);
  switch (record.outcome) {
    case "responding":
      return `responding, checked ${day}`;
    case "sign_in_required":
      return `sign-in required, checked ${day}`;
    // NEUTRAL either way — an internal server is a first-class thing for a team to share, and
    // "we could not reach it from here" is a fact about this plane. Which of the two facts it is
    // decides the reader's next move, so the line says which; a reason this build did not file
    // says neither rather than guessing one.
    case "not_verifiable":
      if (record.detail === PROBE_DETAIL_UNRESOLVED) {
        return `not verifiable from cloud (${PROBE_DETAIL_UNRESOLVED})`;
      }
      if (record.detail === PROBE_DETAIL_PRIVATE) {
        return `not verifiable from cloud (${PROBE_DETAIL_PRIVATE})`;
      }
      return "not verifiable from cloud";
    default:
      return `not responding when checked ${day}`;
  }
}
