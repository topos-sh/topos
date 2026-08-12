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

/** One recorded probe, as a surface receives it. */
export interface McpProbeRecord {
  outcome: McpProbeOutcome;
  /** ISO-8601, from the database. */
  probedAt: string;
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
    case "not_verifiable":
      return "not verifiable from cloud (private address)";
    default:
      return `not responding when checked ${day}`;
  }
}
