import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The /verify action's SINGLE-TENANT refusals against a REAL scratch Postgres, under the real
 * OSS composition (single tenancy, allow-all entitlements — exactly the deployment a crafted
 * POST would hit):
 *
 *  - the create arm DOES NOT EXIST there, twice over: the route's precheck answers the house
 *    404 before any field reads, and the fence aborts independently (a second workspace row
 *    would break every LIMIT-1 resolution of the install's one workspace);
 *  - the loopback wake-up redirect fires ONLY for the flow the page arrived armed for — the
 *    posted challenge is compared server-side against the DECIDED flow's own code hash, so a
 *    typed-code card approved from a loopback-armed URL cannot spend the listener's one wake.
 *
 * The auth session is mocked (guards.test.ts owns the browser-session gate); everything below
 * it is real.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;

vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

let db: ScratchDb;
let wsId = "";

const ORIGIN = "http://x";

type RouteHandler = (a: {
  request: Request;
  params: Record<string, string | undefined>;
}) => Promise<unknown>;

async function postVerify(form: Record<string, string>): Promise<unknown> {
  const { action } = await import("@/routes/verify");
  const body = new URLSearchParams(form);
  try {
    return await (action as RouteHandler)({
      request: new Request(`${ORIGIN}/verify`, {
        method: "POST",
        headers: {
          origin: ORIGIN,
          "content-type": "application/x-www-form-urlencoded",
        },
        body: body.toString(),
      }),
      params: {},
    });
  } catch (e) {
    return e;
  }
}

/** The HTTP status a thrown `data(...)`/Response answer carries, wherever it rides. */
function statusOf(result: unknown): number | undefined {
  if (result instanceof Response) {
    return result.status;
  }
  return (result as { init?: { status?: number } }).init?.status;
}

function challengeOf(flowCode: string): string {
  return createHash("sha256").update(flowCode, "utf8").digest("hex");
}

beforeAll(async () => {
  db = await createScratchDb("web_verifysingle", { TOPOS_WEB_RATELIMIT: "off" });
  wsId = await bootWorkspace();
  await seedUser(db, "u_own", "Owner", "owner@example.com");
  await seatUser(db, wsId, "u_own", "owner");
  session = { user: { id: "u_own", name: "Owner", email: "owner@example.com" } };
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("single tenancy refuses the create arm — route and fence alike", () => {
  it("a crafted pick=create POST is the house 404; nothing is born, the flow survives", async () => {
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("crafted-box", null);
    const result = await postVerify({
      intent: "approve",
      code: flow.userCode,
      pick: "create",
      displayName: "Second Workspace",
      slug: "second-workspace",
    });
    expect(statusOf(result)).toBe(404);
    expect(await db.q(`SELECT count(*)::int AS n FROM web.workspace`)).toEqual([{ n: 1 }]);
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("pending");
    // The one legitimate arm still completes the same flow.
    const seatPick = await postVerify({ intent: "approve", code: flow.userCode, pick: "seat:" });
    expect(seatPick).toMatchObject({ kind: "approved", name: "crafted-box" });
  });

  it("the shared precheck reads OFF here — one spelling for /new and this page", async () => {
    const { createWorkspacePrecheck } = await import("@/lib/db/workspace-create.server");
    expect(await createWorkspacePrecheck()).toBe("off");
  });

  it("the fence aborts a create choice on its own terms — the uniform null, no row", async () => {
    // The route's precheck is one caller; the fence must be safe without it.
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("fence-box", null);
    expect(
      await identity.approveLoginFlow(
        flow.userCode,
        { userId: "u_own", display: "Owner" },
        { kind: "create", displayName: "Sneaky", slug: "sneaky" },
      ),
    ).toBeNull();
    expect(await db.q(`SELECT count(*)::int AS n FROM web.workspace`)).toEqual([{ n: 1 }]);
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("pending");
  });
});

describe("the loopback wake fires only for the flow the page arrived armed for", () => {
  it("deciding a DIFFERENT flow with the listener's coordinates shows plain success, no redirect", async () => {
    const identity = await import("@/lib/db/identity.server");
    const armed = await identity.startLoginFlow("armed-laptop", null, undefined, "loopback");
    const typed = await identity.startLoginFlow("typed-box", null);
    // A typed-code card decided from the loopback-armed URL: coordinates posted, challenge
    // posted — but the DECIDED flow is not the armed one, so nothing redirects and the
    // listener's one wake stays unspent.
    const approvedOther = await postVerify({
      intent: "approve",
      code: typed.userCode,
      pick: "seat:",
      port: "4321",
      state: "abcdefgh",
      device: challengeOf(armed.flowCode),
    });
    expect(approvedOther).not.toBeInstanceOf(Response);
    expect(approvedOther).toMatchObject({ kind: "approved", name: "typed-box" });
    expect((await identity.pollLoginFlow(armed.flowCode)).status).toBe("pending");

    // The armed flow's OWN approval redirects — state-bound, outcome only, never a secret.
    const approvedArmed = await postVerify({
      intent: "approve",
      code: armed.userCode,
      pick: "seat:",
      port: "4321",
      state: "abcdefgh",
      device: challengeOf(armed.flowCode),
    });
    expect(approvedArmed).toBeInstanceOf(Response);
    const location = (approvedArmed as Response).headers.get("location") ?? "";
    const url = new URL(location);
    expect(`${url.protocol}//${url.host}${url.pathname}`).toBe("http://127.0.0.1:4321/cb");
    expect(url.searchParams.get("state")).toBe("abcdefgh");
    expect(url.searchParams.get("outcome")).toBe("approved");
    expect(url.searchParams.get("code")).toBeNull();
  });

  it("deny follows the same rule — the wake redirect only for the armed flow itself", async () => {
    const identity = await import("@/lib/db/identity.server");
    const armed = await identity.startLoginFlow("armed-deny", null, undefined, "loopback");
    const typed = await identity.startLoginFlow("typed-deny", null);
    const deniedOther = await postVerify({
      intent: "deny",
      code: typed.userCode,
      port: "4321",
      state: "abcdefgh",
      device: challengeOf(armed.flowCode),
    });
    expect(deniedOther).not.toBeInstanceOf(Response);
    expect(deniedOther).toMatchObject({ kind: "denied" });
    expect((await identity.pollLoginFlow(armed.flowCode)).status).toBe("pending");

    const deniedArmed = await postVerify({
      intent: "deny",
      code: armed.userCode,
      port: "4321",
      state: "abcdefgh",
      device: challengeOf(armed.flowCode),
    });
    expect(deniedArmed).toBeInstanceOf(Response);
    const location = (deniedArmed as Response).headers.get("location") ?? "";
    expect(new URL(location).searchParams.get("outcome")).toBe("denied");
  });
});
