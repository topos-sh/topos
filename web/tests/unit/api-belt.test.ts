import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * The `/api/v1` rate belt — the door's replacement for the belt the vault wore on its own
 * listener. Deterministic via the injectable `now`; keys are the forwarded peer (or one shared
 * direct bucket), never the credential.
 */

let belt: typeof import("@/lib/api/belt.server");

beforeAll(async () => {
  installTestEnv();
  belt = await import("@/lib/api/belt.server");
});

beforeEach(() => {
  belt.resetBelt();
});

const req = (forwardedFor?: string) =>
  new Request("http://x/api/v1/workspaces/w/me", {
    headers: forwardedFor === undefined ? {} : { "x-forwarded-for": forwardedFor },
  });

describe("checkBelt", () => {
  it("passes a full bucket and answers the frozen 429 when drained", async () => {
    const t0 = 1_000_000;
    for (let i = 0; i < 1000; i += 1) {
      expect(belt.checkBelt(req(), t0)).toBeNull();
    }
    const limited = belt.checkBelt(req(), t0);
    expect(limited).not.toBeNull();
    expect((limited as Response).status).toBe(429);
    expect((limited as Response).headers.get("retry-after")).toBe("1");
  });

  it("refills over time (50/s) and separates peers", () => {
    const t0 = 2_000_000;
    for (let i = 0; i < 1000; i += 1) {
      belt.checkBelt(req("10.0.0.1"), t0);
    }
    expect(belt.checkBelt(req("10.0.0.1"), t0)).not.toBeNull();
    // A DIFFERENT forwarded peer has its own bucket.
    expect(belt.checkBelt(req("10.0.0.2"), t0)).toBeNull();
    // A second later, ~50 tokens returned to the drained peer.
    expect(belt.checkBelt(req("10.0.0.1"), t0 + 1_000)).toBeNull();
  });
});
