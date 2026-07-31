import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * The /login route ACTION — the JS-free fallback the real forms post to (the pre-hydration
 * window right after a CLI opens the browser). The server Better Auth API is mocked; what
 * this suite pins is the WEAVE: the hidden `next` is re-validated server-side (a /verify
 * target rebuilt canonically, an off-origin value refused by safeNextPath), the magic arm
 * lands the same callbackURL the client rung would, the password arm forwards the session
 * cookie onto the redirect, and a cross-origin POST is refused before any of it.
 */

vi.mock("@/composition.server", () => ({
  composition: { auth: { emailAndPassword: true }, registration: "gated" },
}));

vi.mock("@/lib/db/identity.server", () => ({
  pendingLoopbackFlowExists: vi.fn(),
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
});

async function post(
  form: Record<string, string>,
  opts: { origin?: string | null } = {},
): Promise<unknown> {
  const headers: Record<string, string> = {
    "content-type": "application/x-www-form-urlencoded",
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
    const result = await post({ intent: "magic", email: "a@b.test", next: raw });
    expect(result).toEqual({ sent: true, email: "a@b.test" });
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
