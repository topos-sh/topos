import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { beforeAll, describe, expect, it, vi } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * The SERVER half of the version floor (app/lib/api/compat.server.ts). Two things are proved
 * here, and they pull in opposite directions on purpose:
 *  - the floor REFUSES what it can prove is too old, and nothing else — an unreadable
 *    User-Agent, a foreign client, a version this build cannot parse all pass, because a
 *    mechanism meant to make the next wire break one sentence must not become an incident;
 *  - the shipped constants are DAY-ONE QUIET: a fleet on any current release meets the floor
 *    never, which is asserted against the repo's own version statement rather than a copy.
 */

let compat: typeof import("@/lib/api/compat.server");
let SERVER_RELEASE_VERSION: string;

/** The workspace manifest's release version — the repo's ONE statement of what this build is. */
const CARGO_VERSION = (() => {
  const manifest = readFileSync(resolve(__dirname, "..", "..", "..", "Cargo.toml"), "utf8");
  const section = manifest.split("[workspace.package]")[1] ?? "";
  const version = section.match(/^\s*version\s*=\s*"([^"]+)"/m)?.[1];
  if (version === undefined) {
    throw new Error("no [workspace.package] version in Cargo.toml");
  }
  return version;
})();

beforeAll(async () => {
  installTestEnv({ TOPOS_WEB_RATELIMIT: "off" });
  compat = await import("@/lib/api/compat.server");
  SERVER_RELEASE_VERSION = (await import("@/lib/plane/contract/version")).SERVER_RELEASE_VERSION;
});

/** The decision under the SHIPPED constants — what a real request meets today. */
function decide(userAgent: string | null) {
  return compat.decideClientFloor(userAgent, compat.MIN_CLI_VERSION, "0.1.21");
}

describe("the floor under the shipped constants", () => {
  it("refuses a client provably below it", () => {
    expect(decide("topos/0.1.14")).toEqual({ refused: true, clientVersion: "0.1.14" });
    expect(decide("topos/0.0.1")).toEqual({ refused: true, clientVersion: "0.0.1" });
  });

  it("passes the floor release itself and everything after it", () => {
    expect(decide("topos/0.1.60")).toEqual({ refused: false, clientVersion: "0.1.60" });
    expect(decide("topos/0.1.61")).toEqual({ refused: false, clientVersion: "0.1.61" });
    expect(decide("topos/1.0.0")).toEqual({ refused: false, clientVersion: "1.0.0" });
  });

  it("REFUSES every caller that does not identify itself, now that the floor outran the header", () => {
    // The floor sits above the release that taught the CLI to name itself, so silence has stopped
    // being ambiguous: every build from 0.1.21 on sends a version, and one that sends none is
    // provably older than the floor. What it is refused FOR still shows nothing of its own bytes.
    expect(decide(null)).toEqual({ refused: true, clientVersion: null });
    expect(decide("")).toEqual({ refused: true, clientVersion: null });
    // Foreign clients: a human's curl, the HTTP library's own default. The session lane is for
    // topos machines, and one that cannot say which topos it is cannot be spoken to.
    expect(decide("curl/8.7.1")).toEqual({ refused: true, clientVersion: null });
    expect(decide("ureq/3.3.0")).toEqual({ refused: true, clientVersion: null });
    // Our product token carrying something this build cannot read: unidentified, never a guess.
    expect(decide("topos/garbage")).toEqual({ refused: true, clientVersion: null });
    expect(decide("topos/")).toEqual({ refused: true, clientVersion: null });
    expect(decide("topos/0.1")).toEqual({ refused: true, clientVersion: null });
    expect(decide("topos/v0.1.45")).toEqual({ refused: true, clientVersion: null });
    // A digit run no release could carry: unidentified, so the message can never show a person
    // something that is not a plain three-number version.
    expect(decide("topos/99999999999999999999.0.0")).toEqual({
      refused: true,
      clientVersion: null,
    });
  });

  it("decides on the semver core — a pre-release/build suffix never moves the boundary", () => {
    expect(decide("topos/0.1.60-rc.1")).toEqual({ refused: false, clientVersion: "0.1.60" });
    expect(decide("topos/0.1.59-rc.1")).toEqual({ refused: true, clientVersion: "0.1.59" });
    expect(decide("topos/0.1.61+build.7")).toEqual({ refused: false, clientVersion: "0.1.61" });
  });

  it("reads the version off the FIRST product token and re-renders it from its own digits", () => {
    // A trailing comment or second product never changes the verdict.
    expect(decide("topos/0.1.14 (linux; x86_64)")).toEqual({
      refused: true,
      clientVersion: "0.1.14",
    });
    // What a refusal shows is OUR three numbers, never the caller's span of bytes.
    expect(decide("topos/0.01.014").clientVersion).toBe("0.1.14");
    // A topos token that is not first is not this client identifying itself — and unidentified is
    // now a refusal, so the smuggled token buys nothing either way.
    expect(decide("evil/1.0 topos/0.1.14")).toEqual({ refused: true, clientVersion: null });
  });
});

describe("the future rule — when the floor outruns the header", () => {
  it("refuses silence once the floor rises past the release that started sending a version", () => {
    // The day a wire break lands above 0.1.21, a caller sending nothing is PROVABLY older than
    // the floor (every release from 0.1.21 on names itself), so silence stops passing.
    expect(compat.decideClientFloor(null, "0.1.30", "0.1.21")).toEqual({
      refused: true,
      clientVersion: null,
    });
    expect(compat.decideClientFloor("curl/8.7.1", "0.1.30", "0.1.21")).toEqual({
      refused: true,
      clientVersion: null,
    });
    // An identified client is still judged on its own number, either side of that line.
    expect(compat.decideClientFloor("topos/0.1.30", "0.1.30", "0.1.21").refused).toBe(false);
    expect(compat.decideClientFloor("topos/0.1.29", "0.1.30", "0.1.21").refused).toBe(true);
  });
});

describe("day-one quiet — the shipped constants, asserted", () => {
  it("has crossed the header line, so silence is refused with the rest", () => {
    // DELIBERATE, and the reason is the break this floor carries: a catalog row may now arrive
    // with no `document` key — the spelling of a WITHHELD server — and every client before 0.1.60
    // declares that field required, so one such row fails to deserialize and takes the whole
    // catalog read down with it. That is not a degradation a client rides out, and the floor is
    // well past the release that started sending a version — so a caller that names none is
    // refused with the rest.
    expect(decide(null).refused).toBe(true);
    expect(compat.MIN_CLI_VERSION).toBe("0.1.60");
  });

  it("keeps the floor at or below this build's own release", () => {
    // A floor raised past the release that carries it would refuse the fleet on deploy day.
    expect(SERVER_RELEASE_VERSION).toBe(CARGO_VERSION);
    expect(decide(`topos/${SERVER_RELEASE_VERSION}`).refused).toBe(false);
  });

  it("passes the CLI this repo currently builds", () => {
    // The parsed version is the re-rendered semver CORE: a release-candidate build's `-rc.N`
    // suffix never survives into the decision, so the expectation strips it the same way — this
    // must hold on an rc tag too, where the release gates run this suite.
    expect(decide(`topos/${CARGO_VERSION}`)).toEqual({
      refused: false,
      clientVersion: CARGO_VERSION.split(/[-+]/)[0],
    });
  });
});

describe("clientFloor — the wired decision", () => {
  const req = (userAgent?: string) =>
    new Request("http://x/api/v1/workspaces/w/me", {
      headers: userAgent === undefined ? {} : { "user-agent": userAgent },
    });

  it("passes a current client and refuses every unidentified caller", () => {
    expect(compat.clientFloor(req(`topos/${CARGO_VERSION}`))).toBeNull();
    expect((compat.clientFloor(req()) as Response).status).toBe(426);
    expect((compat.clientFloor(req("curl/8.7.1")) as Response).status).toBe(426);
  });

  it("answers the 426 for a below-floor client, naming the floor and nothing of the header", async () => {
    const res = compat.clientFloor(req("topos/0.1.1 (secret-looking bytes)"));
    expect(res).not.toBeNull();
    const refusal = res as Response;
    expect(refusal.status).toBe(426);
    const body = (await refusal.json()) as {
      error: { code: string; context: { message: string; min_cli_version: string } };
    };
    expect(body.error.code).toBe("CLI_UPDATE_REQUIRED");
    expect(body.error.context.min_cli_version).toBe(compat.MIN_CLI_VERSION);
    expect(body.error.context.message).toContain("topos 0.1.1");
    // The caller's own bytes never make it back out.
    expect(body.error.context.message).not.toContain("secret-looking");
  });
});

describe("laneGate — the lane's front door", () => {
  const lane = (userAgent: string) =>
    new Request("http://x/api/v1/workspaces/w/me", { headers: { "user-agent": userAgent } });

  it("holds the floor with the belt knob OFF", () => {
    // This file runs belt-off, as every e2e stack and edge-limited deployment does. The floor is
    // not the belt's passenger: turning rate limiting off must never open the lane to a client
    // this server cannot speak to.
    expect((compat.laneGate(lane("topos/0.1.1")) as Response).status).toBe(426);
    expect(compat.laneGate(lane(`topos/${CARGO_VERSION}`))).toBeNull();
  });

  it("runs the BELT first — a drained bucket answers 429, old client or not", async () => {
    // Rate limiting applies to old clients too: an unanswerable refusal is still unbounded work,
    // so the 429 must win over the 426 when both apply.
    vi.resetModules();
    installTestEnv({ TOPOS_WEB_RATELIMIT: "on" });
    const belted = await import("@/lib/api/compat.server");
    (await import("@/lib/api/belt.server")).resetBelt();

    let limited: Response | null = null;
    for (let i = 0; i < 5000 && limited === null; i++) {
      limited = belted.laneGate(lane(`topos/${CARGO_VERSION}`));
    }
    expect(limited?.status).toBe(429);
    expect((belted.laneGate(lane("topos/0.1.1")) as Response).status).toBe(429);
  });
});
