import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * The /login route ACTION — the JS-free fallback the real forms post to (the pre-hydration
 * window right after a CLI opens the browser). The server Better Auth API is mocked; what
 * this suite pins is the WEAVE: the hidden `next` is re-validated server-side (a /verify
 * target rebuilt canonically, an off-origin value refused by safeNextPath), the magic arm
 * lands the same callbackURL the client rung would, the password arm forwards the session
 * cookie onto the redirect, a cross-origin POST is refused before any of it — and the BELTS
 * hold: these arms bypass Better Auth's HTTP-handler limiter (they call the server API
 * directly), so the action wears its own per-client buckets, proven here past the burst.
 * Each ordinary test posts from its OWN client address (the belt keys on the trusted last
 * XFF hop), so only the belt tests share one.
 */

vi.mock("@/composition.server", () => ({
  composition: { auth: { emailAndPassword: true }, registration: "gated" },
}));

const pendingLoopbackFlowCode = vi.fn<(hex: string) => Promise<string | null>>();
vi.mock("@/lib/db/identity.server", () => ({
  pendingLoopbackFlowCode: (hex: string) => pendingLoopbackFlowCode(hex),
  sessionActor: vi.fn(),
  seatOf: vi.fn(),
  theWorkspace: vi.fn(),
  workspaceByName: vi.fn(),
}));

const signInMagicLink = vi.fn<(args: unknown) => Promise<unknown>>();
const signInEmail = vi.fn<(args: unknown) => Promise<Response>>();
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { signInMagicLink, signInEmail } }),
}));

let action: typeof import("@/routes/login").action;

const ORIGIN = "http://localhost:3000";
const DEVICE = "ab".repeat(32);

beforeAll(async () => {
  installTestEnv();
  ({ action } = await import("@/routes/login"));
});

beforeEach(() => {
  signInMagicLink.mockReset().mockResolvedValue({ status: true });
  signInEmail.mockReset();
  pendingLoopbackFlowCode.mockReset().mockResolvedValue(null);
});

let nextClient = 0;

async function post(
  form: Record<string, string>,
  opts: { origin?: string | null; ip?: string } = {},
): Promise<unknown> {
  const headers: Record<string, string> = {
    "content-type": "application/x-www-form-urlencoded",
    // The belt keys on the trusted proxy's LAST hop — a fresh address per call unless a belt
    // test pins one deliberately.
    "x-forwarded-for": opts.ip ?? `10.0.0.${++nextClient}`,
  };
  if (opts.origin !== null) {
    headers.origin = opts.origin ?? ORIGIN;
  }
  try {
    return await action({
      request: new Request(`${ORIGIN}/login`, {
        method: "POST",
        headers,
        body: new URLSearchParams(form).toString(),
      }),
      params: {},
      context: {},
    } as Parameters<typeof action>[0]);
  } catch (e) {
    return e;
  }
}

function statusOf(result: unknown): number | undefined {
  if (result instanceof Response) {
    return result.status;
  }
  return (result as { init?: { status?: number } }).init?.status;
}

describe("the magic arm", () => {
  it("sends through the server API with the CANONICALLY REBUILT next as callbackURL", async () => {
    const raw = `/verify?state=abcdefgh&device=${DEVICE}&port=4321`;
    pendingLoopbackFlowCode.mockResolvedValueOnce("JXF7-XAM2");
    const result = await post({ intent: "magic", email: "a@b.test", next: raw });
    // The payload carries the next the mail ACTUALLY used — the sent card's resend and
    // different-email arms re-render from it (the no-JS POST has no query string for the
    // loader, so a loader fallback would orphan the pending flow).
    expect(result).toEqual({
      sent: true,
      email: "a@b.test",
      next: `/verify?device=${DEVICE}&port=4321&state=abcdefgh`,
      // The glance code rides the payload: the sent card must keep showing it (the native POST
      // leaves the loader no query to re-probe from).
      deviceCode: "JXF7-XAM2",
    });
    expect(pendingLoopbackFlowCode).toHaveBeenCalledWith(DEVICE);
    expect(signInMagicLink).toHaveBeenCalledWith(
      expect.objectContaining({
        body: {
          email: "a@b.test",
          callbackURL: `/verify?device=${DEVICE}&port=4321&state=abcdefgh`,
        },
      }),
    );
  });

  it("refuses an off-origin next — safeNextPath's fallback rides instead", async () => {
    await post({ intent: "magic", email: "a@b.test", next: "//evil.com" });
    expect(signInMagicLink).toHaveBeenCalledWith(
      expect.objectContaining({ body: { email: "a@b.test", callbackURL: "/app" } }),
    );
  });

  it("a failed send answers the same coarse line the client rung shows", async () => {
    signInMagicLink.mockRejectedValueOnce(new Error("relay down"));
    const result = await post({ intent: "magic", email: "a@b.test", next: "/app" });
    expect(statusOf(result)).toBe(400);
    expect((result as { data: { error: string } }).data.error).toBe(
      "Couldn’t send the link. Check the address and try again.",
    );
  });

  it("an empty email is asked for, not sent", async () => {
    const result = await post({ intent: "magic", email: "  ", next: "/app" });
    expect(statusOf(result)).toBe(400);
    expect(signInMagicLink).not.toHaveBeenCalled();
  });
});

describe("the password arm", () => {
  it("signs in server-side, FORWARDS the session cookie, and redirects to next", async () => {
    signInEmail.mockResolvedValueOnce(
      new Response(null, { status: 200, headers: { "set-cookie": "ba.session=abc; Path=/" } }),
    );
    const result = await post({
      intent: "password",
      email: "a@b.test",
      password: "secret-password",
      next: "/verify?device=" + DEVICE,
    });
    expect(result).toBeInstanceOf(Response);
    const response = result as Response;
    expect(response.status).toBe(302);
    expect(response.headers.get("location")).toBe(`/verify?device=${DEVICE}`);
    expect(response.headers.get("set-cookie")).toContain("ba.session=abc");
  });

  it("a refused credential answers the client rung's constant line", async () => {
    signInEmail.mockResolvedValueOnce(new Response(null, { status: 401 }));
    const result = await post({
      intent: "password",
      email: "a@b.test",
      password: "wrong",
      next: "/app",
    });
    expect(statusOf(result)).toBe(400);
    expect((result as { data: { error: string } }).data.error).toBe(
      "Couldn’t sign in. Check your email and password.",
    );
  });
});

describe("the belts (the native arms bypass Better Auth's HTTP limiter)", () => {
  const RATE_BELTED = "Too many attempts — wait a moment and try again.";

  it("magic sends belt at burst 3 per client — the 4th answers 429 and sends NO mail", async () => {
    const ip = "203.0.113.7";
    for (let i = 0; i < 3; i++) {
      const ok = await post({ intent: "magic", email: "belted@b.test", next: "/app" }, { ip });
      expect(ok).toEqual({ sent: true, email: "belted@b.test", next: "/app", deviceCode: null });
    }
    const belted = await post({ intent: "magic", email: "belted@b.test", next: "/app" }, { ip });
    expect(statusOf(belted)).toBe(429);
    expect((belted as { data: { error: string } }).data.error).toBe(RATE_BELTED);
    expect(signInMagicLink).toHaveBeenCalledTimes(3);
    // ANOTHER client is untouched — the belt is per address, not global.
    const other = await post(
      { intent: "magic", email: "other@b.test", next: "/app" },
      { ip: "203.0.113.8" },
    );
    expect(other).toEqual({ sent: true, email: "other@b.test", next: "/app", deviceCode: null });
  });

  it("the sent card's RESEND arm rides the same belt — it is the same post", async () => {
    const ip = "203.0.113.9";
    // The initial send, then resends: the card's resend form posts intent=magic with the same
    // hidden fields, so the third resend (4th send) is the one the belt refuses.
    for (let i = 0; i < 3; i++) {
      await post({ intent: "magic", email: "resend@b.test", next: "/app" }, { ip });
    }
    const belted = await post({ intent: "magic", email: "resend@b.test", next: "/app" }, { ip });
    expect(statusOf(belted)).toBe(429);
    expect(signInMagicLink).toHaveBeenCalledTimes(3);
  });

  it("the OUTER belt bounds password attempts at burst 10 — the 11th answers 429, nothing signed in", async () => {
    const ip = "203.0.113.10";
    signInEmail.mockResolvedValue(new Response(null, { status: 401 }));
    for (let i = 0; i < 10; i++) {
      const tried = await post(
        { intent: "password", email: "brute@b.test", password: `guess-${i}`, next: "/app" },
        { ip },
      );
      expect(statusOf(tried)).toBe(400);
    }
    const belted = await post(
      { intent: "password", email: "brute@b.test", password: "guess-11", next: "/app" },
      { ip },
    );
    expect(statusOf(belted)).toBe(429);
    expect((belted as { data: { error: string } }).data.error).toBe(RATE_BELTED);
    // The credential check never ran past the burst — no online attempt beyond the limit.
    expect(signInEmail).toHaveBeenCalledTimes(10);
  });

  it("the outer belt lands BEFORE the body is parsed — a dry client's body is never read", async () => {
    const ip = "203.0.113.77";
    // Dry the outer bucket with ordinary posts…
    signInEmail.mockResolvedValue(new Response(null, { status: 401 }));
    for (let i = 0; i < 10; i++) {
      await post(
        { intent: "password", email: "dry@b.test", password: `guess-${i}`, next: "/app" },
        { ip },
      );
    }
    // …then present a request whose body access THROWS: the 429 must land without the
    // action ever touching it (the belt is the first thing after the origin check, so an
    // exhausted client cannot spend server work on parsing arbitrarily shaped bodies).
    const req = new Request(`${ORIGIN}/login`, {
      method: "POST",
      headers: {
        "content-type": "application/x-www-form-urlencoded",
        "x-forwarded-for": ip,
        origin: ORIGIN,
      },
      body: "intent=password",
    });
    Object.defineProperty(req, "formData", {
      value: () => {
        throw new Error("the body was parsed past a dry belt");
      },
    });
    const belted = await action({ request: req, params: {}, context: {} } as Parameters<
      typeof action
    >[0]);
    expect(statusOf(belted)).toBe(429);
    expect((belted as { data: { error: string } }).data.error).toBe(RATE_BELTED);
  });
});

describe("the door itself", () => {
  it("a POST without a matching Origin is refused before any arm runs", async () => {
    const missing = await post({ intent: "magic", email: "a@b.test" }, { origin: null });
    expect(statusOf(missing)).toBe(404);
    const foreign = await post(
      { intent: "magic", email: "a@b.test" },
      { origin: "http://evil.example" },
    );
    expect(statusOf(foreign)).toBe(404);
    expect(signInMagicLink).not.toHaveBeenCalled();
  });

  it("an unknown intent is a 400", async () => {
    expect(statusOf(await post({ intent: "mystery" }))).toBe(400);
  });
});
