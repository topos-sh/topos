import { SERVER_RELEASE_VERSION } from "@/lib/plane/contract/version";

/**
 * WHAT A REAL CLIENT SENDS on the session lane.
 *
 * The floor now sits ABOVE the release that taught the CLI to name itself, so silence is no longer
 * ambiguous: an unidentified caller is provably older than the floor and gets the 426. Every suite
 * that drives a lane route therefore identifies itself the way the binary does — a request without
 * this header is testing the refusal, not the route.
 */
export const LANE_USER_AGENT = `topos/${SERVER_RELEASE_VERSION}`;

/** A lane request's headers, with the client version stamped on. */
export function laneHeaders(extra: Record<string, string> = {}): Record<string, string> {
  return { "user-agent": LANE_USER_AGENT, ...extra };
}
