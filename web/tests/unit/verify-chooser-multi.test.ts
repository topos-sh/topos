import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { createScratchDb, type ScratchDb, seatUser, seedUser } from "./helpers/scratch-db";

/**
 * The /verify page's MULTI-tenant chooser paths against a REAL scratch Postgres — the arms the
 * Playwright stack (single-tenant) cannot reach: a zero-seat person whose card leads with the
 * CREATE form (and whose approve births the workspace inside the fence), and a multi-seat
 * person PICKING one workspace of several. The route's loader/action are driven directly with
 * constructed Requests; the auth session is mocked (the browser-session gate is the subject of
 * guards.test.ts, not this suite), everything below it is real.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;

vi.mock("@/composition.server", () => ({
  composition: {
    tenancy: "multi" as const,
    reservedWorkspaceNames: [],
    entitlements: {
      forWorkspace: () => Promise.resolve({ allows: () => true, limit: () => null }),
    },
  },
}));

vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

let db: ScratchDb;

const ORIGIN = "http://x";

function asUser(id: string, name: string, email: string): void {
  session = { user: { id, name, email } };
}

type RouteHandler = (a: {
  request: Request;
  params: Record<string, string | undefined>;
}) => Promise<unknown>;

async function loadVerify(url: string): Promise<unknown> {
  const { loader } = await import("@/routes/verify");
  return await (loader as RouteHandler)({ request: new Request(url), params: {} });
}

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
    if (e instanceof Response) {
      return e;
    }
    throw e;
  }
}

/** Unwrap react-router's `data()` envelope where an arm used it (typed refusals). */
function unwrap(result: unknown): unknown {
  return result !== null && typeof result === "object" && "data" in (result as object)
    ? (result as { data: unknown }).data
    : result;
}

async function seedWorkspace(id: string, name: string): Promise<void> {
  await db.q(
    `INSERT INTO web.workspace (id, name, display_name, claimed_at) VALUES ($1, $2, $2, now())`,
    [id, name],
  );
}

beforeAll(async () => {
  db = await createScratchDb("web_verifymulti", { TOPOS_WEB_RATELIMIT: "off" });
  await seedWorkspace("w_alpha", "alpha-team");
  await seedWorkspace("w_beta", "beta-team");
  await seedUser(db, "u_fresh", "Fresh", "fresh@example.com");
  await seedUser(db, "u_two", "TwoSeats", "two@example.com");
  await seatUser(db, "w_alpha", "u_two", "member");
  await seatUser(db, "w_beta", "u_two", "member");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("zero seats: the create form leads and the create arm births + approves", () => {
  it("the loader serves an empty chooser with creation open", async () => {
    asUser("u_fresh", "Fresh", "fresh@example.com");
    const result = (await loadVerify(`${ORIGIN}/verify`)) as {
      multi: boolean;
      chooser: { seats: unknown[]; invitations: unknown[]; createAllowed: boolean };
    };
    expect(result.multi).toBe(true);
    expect(result.chooser.seats).toEqual([]);
    expect(result.chooser.invitations).toEqual([]);
    expect(result.chooser.createAllowed).toBe(true);
  });

  it("the ?check= probe answers against the real table", async () => {
    asUser("u_fresh", "Fresh", "fresh@example.com");
    expect(await loadVerify(`${ORIGIN}/verify?check=alpha-team`)).toEqual({
      name: "alpha-team",
      available: false,
    });
    expect(await loadVerify(`${ORIGIN}/verify?check=fresh-team`)).toEqual({
      name: "fresh-team",
      available: true,
    });
  });

  it("approve with pick=create births the workspace; the poll then mints into it", async () => {
    asUser("u_fresh", "Fresh", "fresh@example.com");
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("fresh-laptop", null);
    const result = (await postVerify({
      intent: "approve",
      code: flow.userCode,
      pick: "create",
      displayName: "Fresh Team",
      slug: "fresh-team",
    })) as { kind: string; name: string; workspaceDisplay: string };
    expect(result.kind).toBe("approved");
    expect(result.name).toBe("fresh-laptop");
    expect(result.workspaceDisplay).toBe("Fresh Team");
    // The birth is complete (owner seat included) and the exchange mints into it, born active.
    const ws = await db.q<{ id: string }>(`SELECT id FROM web.workspace WHERE name = 'fresh-team'`);
    expect(ws).toHaveLength(1);
    const granted = await identity.pollLoginFlow(flow.flowCode);
    expect(granted.status).toBe("granted");
    expect(granted.status === "granted" && granted.sessionStatus).toBe("active");
    expect(granted.status === "granted" && granted.approvedWorkspaceId).toBe(ws[0]?.id);
  });

  it("a taken slug answers the typed refusal WITH the card intact; the flow stays pending", async () => {
    asUser("u_fresh", "Fresh", "fresh@example.com");
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("retry-laptop", null);
    const result = unwrap(
      await postVerify({
        intent: "approve",
        code: flow.userCode,
        pick: "create",
        displayName: "Alpha Again",
        slug: "alpha-team",
      }),
    ) as { kind: string; error: string; slug: string; pending: { userCode: string } };
    expect(result.kind).toBe("create-refused");
    expect(result.error).toBe("That address is taken — try another.");
    expect(result.slug).toBe("alpha-team");
    // The card re-resolved — the flow is still pending for the corrected retry.
    expect(result.pending.userCode).toBe(flow.userCode);
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("pending");
  });
});

describe("several seats: the pick decides", () => {
  it("the loader serves both seats; the approve lands the PICKED one", async () => {
    asUser("u_two", "TwoSeats", "two@example.com");
    const loaded = (await loadVerify(`${ORIGIN}/verify`)) as {
      chooser: { seats: { name: string }[] };
    };
    expect(loaded.chooser.seats.map((s) => s.name)).toEqual(["alpha-team", "beta-team"]);

    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("two-laptop", null);
    const result = (await postVerify({
      intent: "approve",
      code: flow.userCode,
      pick: "seat:beta-team",
    })) as { kind: string };
    expect(result.kind).toBe("approved");
    const granted = await identity.pollLoginFlow(flow.flowCode);
    expect(granted.status === "granted" && granted.approvedWorkspaceId).toBe("w_beta");
  });

  it("a pick naming a workspace the person holds NO seat in is the uniform refusal", async () => {
    asUser("u_fresh", "Fresh", "fresh@example.com");
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("sneak-laptop", null);
    const result = unwrap(
      await postVerify({
        intent: "approve",
        code: flow.userCode,
        pick: "seat:alpha-team",
      }),
    ) as { kind: string };
    expect(result.kind).toBe("refused");
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("pending");
  });
});

describe("the lane-side connect across workspaces", () => {
  it("mints where a seat stands; answers the uniform 404 where none does", async () => {
    const identity = await import("@/lib/db/identity.server");
    const { action } = await import("@/routes/api.v1.login-connect");
    // u_two holds an active session in alpha (minted through the real ceremony).
    const flow = await identity.startLoginFlow("two-cli", null);
    await identity.approveLoginFlow(
      flow.userCode,
      { userId: "u_two", display: "TwoSeats" },
      { kind: "seat", workspace: "alpha-team" },
    );
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("granted");

    const connect = async (workspace: string): Promise<Response> =>
      (await (action as RouteHandler)({
        request: new Request(`${ORIGIN}/api/v1/login/connect`, {
          method: "POST",
          headers: {
            authorization: `Bearer ${flow.flowCode}`,
            "content-type": "application/json",
          },
          body: JSON.stringify({ workspace, requested_name: "two-cli" }),
        }),
        params: {},
      })) as Response;

    // The seat in beta admits — a fresh credential for the further workspace.
    const ok = await connect("beta-team");
    expect(ok.status).toBe(200);
    const body = (await ok.json()) as { workspace: { workspace_id: string } };
    expect(body.workspace.workspace_id).toBe("w_beta");

    // No seat in the stranger's workspace — the uniform miss, never an existence oracle.
    await seedWorkspace("w_gamma", "gamma-team");
    const miss = await connect("gamma-team");
    expect(miss.status).toBe(404);
    // An unknown slug answers byte-identically.
    const unknown = await connect("no-such-team");
    expect(unknown.status).toBe(404);
  });
});
