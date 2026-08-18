import { checkBelt } from "./belt.server";
import { upgradeRequired } from "./wire.server";

/**
 * The SERVER half of the version floor — the one owner of which topos releases this deployment
 * still speaks to, and the gate every `/api/v1` entrypoint runs first.
 *
 * Compatibility is an INTERVAL with one boundary per direction, and each side declares only what
 * it can verifiably know: a client names the oldest server it speaks to (read off the protocol
 * card before a login commits), a server names the oldest client — here, and on the card as
 * `min_cli_version`. Neither side negotiates and there is no second dialect: pre-1.0 wire changes
 * are clean breaks, so the boundary is simply the last release that broke the wire.
 *
 * The whole point is that the NEXT break is one sentence instead of a support incident, so the
 * check fails toward SILENCE: nothing but a version this server can read, provably below the
 * floor, ever refuses. Today that means a fleet running any current release sees nothing at all.
 */

/**
 * The oldest CLI release this server still speaks to. The floor sits at the last release that
 * broke the wire in a way a client CANNOT DEGRADE THROUGH — the login wire going server-first.
 * Later removals that a client rides out on its own do not move it: a lane route has since been
 * deleted whose loss costs an older client one best-effort field and no more. A break with no
 * graceful degradation moves this floor in the same change as the break.
 */
export const MIN_CLI_VERSION = "0.1.42";

/**
 * The first CLI release that identifies itself on the session lane. A client sending no parseable
 * version PASSES while the floor is at or below this release: every pre-mechanism topos is silent
 * by construction, and those builds are exactly the ones the floor still admits. Once a future
 * floor rises past this release, silence stops being ambiguous — anything unidentified is then
 * provably older than the floor, and is refused with the rest.
 */
const CLIENT_VERSION_HEADER_SINCE = "0.1.21";

/** The product token the CLI sends: `topos/<version>`, ahead of anything else in the header. */
const PRODUCT = "topos/";

/** The decision + the words for it — never two judgements, and never the caller's own bytes. */
export interface FloorDecision {
  /** Whether the caller is provably below the floor (the ONLY case that refuses). */
  refused: boolean;
  /**
   * The version RE-RENDERED from the three numbers parsed out of the header, or `null` when the
   * caller did not identify itself as a topos this server can read.
   */
  clientVersion: string | null;
}

/**
 * The client's version, read STRICTLY off the User-Agent's first product token: `topos/` followed
 * by three plainly-sized numbers, with a pre-release/build suffix tolerated. Everything else — no
 * header, curl, a bare ureq default, a truncated or invented version, a digit run no release
 * could carry — is UNIDENTIFIED (`null`), which is the quiet answer: an untrusted header this
 * server cannot read is not evidence of an old client. The strictness also guarantees the value
 * this returns is a plain three-number version, since a refusal shows it back to a person.
 */
function parseClientVersion(userAgent: string | null): string | null {
  const product = (userAgent ?? "").trimStart().split(/\s/)[0] ?? "";
  if (!product.startsWith(PRODUCT)) {
    return null;
  }
  const parts = (product.slice(PRODUCT.length).split(/[-+]/)[0] ?? "").split(".");
  if (parts.length !== 3 || !parts.every((part) => /^\d{1,9}$/.test(part))) {
    return null;
  }
  return parts.map((part) => Number.parseInt(part, 10)).join(".");
}

/** The semver core as a triple — a pre-release/build suffix never decides an ordering. */
function coreTriple(version: string): [number, number, number] {
  const parts = (version.split(/[-+]/)[0] ?? "").split(".");
  const at = (i: number) => {
    const n = Number.parseInt(parts[i] ?? "", 10);
    return Number.isNaN(n) ? 0 : n;
  };
  return [at(0), at(1), at(2)];
}

/** Whether `version` is strictly below `floor` on the semver core (the CLI's own comparator). */
function below(version: string, floor: string): boolean {
  const [major, minor, patch] = coreTriple(version);
  const [floorMajor, floorMinor, floorPatch] = coreTriple(floor);
  if (major !== floorMajor) {
    return major < floorMajor;
  }
  if (minor !== floorMinor) {
    return minor < floorMinor;
  }
  return patch < floorPatch;
}

/**
 * The floor decision, pure: the header bytes and both boundaries in, the verdict and its wording
 * out. Kept parameterized so the FUTURE behaviour — the day a floor rises past the release that
 * started sending the header, and silence becomes proof of age — is a test today, not a surprise
 * on the day someone raises the number.
 */
export function decideClientFloor(
  userAgent: string | null,
  minCli: string,
  headerSince: string,
): FloorDecision {
  const clientVersion = parseClientVersion(userAgent);
  const refused =
    clientVersion === null ? below(headerSince, minCli) : below(clientVersion, minCli);
  return { refused, clientVersion };
}

/** The 426 to answer a below-floor client, or null to pass. */
export function clientFloor(request: Request): Response | null {
  const { refused, clientVersion } = decideClientFloor(
    request.headers.get("user-agent"),
    MIN_CLI_VERSION,
    CLIENT_VERSION_HEADER_SINCE,
  );
  return refused ? upgradeRequired(clientVersion, MIN_CLI_VERSION) : null;
}

/**
 * The lane's front door, run first by every `/api/v1` entrypoint. The belt goes FIRST: rate
 * limiting protects this deployment from old clients exactly as it does from current ones, and
 * an unbounded refusal is still unbounded work. The floor then holds on its own — it is not the
 * belt's passenger, so turning the belt off (`TOPOS_WEB_RATELIMIT=off`, as every e2e stack and
 * edge-limited deployment does) never opens the lane to a client this server cannot speak to.
 */
export function laneGate(request: Request): Response | null {
  return checkBelt(request) ?? clientFloor(request);
}
