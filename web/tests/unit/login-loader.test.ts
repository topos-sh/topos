import { beforeAll, describe, expect, it, vi } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * The login loader maps the COMPOSITION's auth rungs into plain client flags — no server config
 * reaches the bundle, and no tenancy branch enters the decision. The rungs come from
 * `composition.auth` and the sign-up posture from `composition.registration`, mocked here so a
 * single import exercises every shape (the OSS build composes only email+password, gated).
 * `next` rides through `safeNextPath` (real), same-app only.
 */

let authConfig: {
  emailAndPassword: boolean;
  magicLink?: unknown;
  socialProviders?: Record<string, unknown>;
} = { emailAndPassword: true };
let registrationPolicy: "gated" | "open" = "gated";

vi.mock("@/composition.server", () => ({
  composition: {
    get auth() {
      return authConfig;
    },
    get registration() {
      return registrationPolicy;
    },
  },
}));

let loader: typeof import("@/routes/login").loader;

beforeAll(async () => {
  installTestEnv();
  ({ loader } = await import("@/routes/login"));
});

function call(url: string) {
  return loader({ request: new Request(url) } as Parameters<typeof loader>[0]);
}

describe("the login loader's rung flags", () => {
  it("vanilla OSS (email+password only): no magic link, no social", async () => {
    authConfig = { emailAndPassword: true };
    const data = await call("http://localhost/login");
    expect(data.magicLink).toBe(false);
    expect(data.socialProviders).toEqual([]);
    expect(data.emailAndPassword).toBe(true);
  });

  it("a composed magic-link delivery flips the flag on", async () => {
    authConfig = { emailAndPassword: true, magicLink: { send: async () => {} } };
    const data = await call("http://localhost/login");
    expect(data.magicLink).toBe(true);
  });

  it("surfaces composed social provider IDS (never their secrets)", async () => {
    authConfig = {
      emailAndPassword: true,
      socialProviders: { google: { clientId: "x", clientSecret: "shh" } },
    };
    const data = await call("http://localhost/login");
    expect(data.socialProviders).toEqual(["google"]);
    // The values are ids only — the loader never leaks the credential object.
    expect(JSON.stringify(data)).not.toContain("shh");
  });

  it("carries emailAndPassword=false when a composition disables the password rung", async () => {
    authConfig = { emailAndPassword: false, magicLink: { send: async () => {} } };
    const data = await call("http://localhost/login");
    expect(data.emailAndPassword).toBe(false);
    expect(data.magicLink).toBe(true);
  });

  it("the gated composition (the OSS default) carries registrationOpen=false", async () => {
    authConfig = { emailAndPassword: true };
    registrationPolicy = "gated";
    const data = await call("http://localhost/login");
    expect(data.registrationOpen).toBe(false);
  });

  it("an open composition carries registrationOpen=true — one plain flag, no policy object", async () => {
    authConfig = { emailAndPassword: true };
    registrationPolicy = "open";
    const data = await call("http://localhost/login");
    expect(data.registrationOpen).toBe(true);
    registrationPolicy = "gated";
  });

  it("validates `next` to a same-app path; an off-origin value is rejected", async () => {
    authConfig = { emailAndPassword: true };
    expect((await call("http://localhost/login?next=/verify")).next).toBe("/verify");
    // The off-origin value never rides through: the fallback is some same-app path (the exact
    // default is safeNextPath's concern), never the attacker's `//host`.
    const fallback = (await call("http://localhost/login?next=//evil.com")).next;
    expect(fallback).not.toBe("//evil.com");
    expect(fallback.startsWith("/")).toBe(true);
    expect(fallback.startsWith("//")).toBe(false);
  });
});
