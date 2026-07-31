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

// The loader's device-waiting probe + guards.server's transitive imports, mocked (this suite
// runs DB-free; identity-core owns the probe's SQL).
const pendingLoopbackFlowExists = vi.fn<(hex: string) => Promise<boolean>>();
vi.mock("@/lib/db/identity.server", () => ({
  pendingLoopbackFlowExists: (hex: string) => pendingLoopbackFlowExists(hex),
  sessionActor: vi.fn(),
  seatOf: vi.fn(),
  theWorkspace: vi.fn(),
  workspaceByName: vi.fn(),
}));

vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: {} }),
}));

let loader: typeof import("@/routes/login").loader;

beforeAll(async () => {
  installTestEnv();
  pendingLoopbackFlowExists.mockResolvedValue(false);
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

describe("a /verify `next` is REBUILT canonically, never echoed", () => {
  const device = "ab".repeat(32);

  it("keeps the shape-valid components, in the canonical order", async () => {
    const raw = `/verify?state=abcdefgh&device=${device}&port=4321`;
    expect((await call(`http://localhost/login?next=${encodeURIComponent(raw)}`)).next).toBe(
      `/verify?device=${device}&port=4321&state=abcdefgh`,
    );
  });

  it("drops anything off-shape or unknown — the resume target is server-derived", async () => {
    // A junk device, an out-of-range port, a foreign param: all dropped, none echoed.
    const raw = `/verify?device=NOT-HEX&port=80&state=!!&evil=1&device2=${device}`;
    expect((await call(`http://localhost/login?next=${encodeURIComponent(raw)}`)).next).toBe(
      "/verify",
    );
  });

  it("keeps a partial set: a bare challenge without listener coordinates survives alone", async () => {
    const raw = `/verify?device=${device}&port=99999&state=ok`;
    expect((await call(`http://localhost/login?next=${encodeURIComponent(raw)}`)).next).toBe(
      `/verify?device=${device}`,
    );
  });

  it("a non-/verify path is untouched by the rebuild (the ordinary validation applies)", async () => {
    expect((await call("http://localhost/login?next=/app")).next).toBe("/app");
    // A path that merely STARTS with /verify is not the verify page.
    const fallback = (await call("http://localhost/login?next=/verify-ish")).next;
    expect(fallback).toBe("/verify-ish");
  });
});

describe("the machine-is-waiting hint", () => {
  const device = "cd".repeat(32);

  it("rides when the rebuilt verify next carries a challenge that RESOLVES", async () => {
    pendingLoopbackFlowExists.mockResolvedValueOnce(true);
    const raw = encodeURIComponent(`/verify?device=${device}`);
    const result = await call(`http://localhost/login?next=${raw}`);
    expect(pendingLoopbackFlowExists).toHaveBeenCalledWith(device);
    expect(result.deviceWaiting).toBe(true);
  });

  it("says nothing when the challenge resolves to no live flow", async () => {
    pendingLoopbackFlowExists.mockResolvedValueOnce(false);
    const raw = encodeURIComponent(`/verify?device=${device}`);
    expect((await call(`http://localhost/login?next=${raw}`)).deviceWaiting).toBe(false);
  });

  it("never probes for a next that is not the verify page (or carries no challenge)", async () => {
    pendingLoopbackFlowExists.mockClear();
    expect((await call("http://localhost/login?next=/app")).deviceWaiting).toBe(false);
    expect((await call("http://localhost/login?next=%2Fverify")).deviceWaiting).toBe(false);
    expect(pendingLoopbackFlowExists).not.toHaveBeenCalled();
  });
});
