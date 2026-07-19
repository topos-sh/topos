import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedDevice,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The workspace-existence blind, proven end-to-end against a REAL scratch Postgres in MULTI
 * tenancy (where `:ws` is a guessable public name slug): every workspace-scoped surface must
 * answer a non-member (and an anonymous visitor) BYTE-IDENTICALLY to an unknown slug — status
 * AND body — so no response ever confirms a workspace exists. The one resolution under test is
 * `requireMemberInScope`/`memberInScope` (guards.server.ts): session first, then the
 * slug→workspace→seat chain, with both misses folding to the same throw.
 *
 * Three surfaces: a member PANEL page (the channels index), the workspace ROOT face (the
 * dashboard), and a device-lane API route (`/channels`) whose misses are the one uniform wire
 * envelope.
 */

vi.mock("@/composition.server", () => ({
  composition: { tenancy: "multi" as const },
}));

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

const ORIGIN = "http://x";

let db: ScratchDb;
const WS_REAL = "w_real";
const WS_OTHER = "w_other";
/** The stranger's enrolled device id doubles as its credential plaintext (seedDevice hashes it). */
const STRANGER_DEVICE = "d_stranger";

async function seedWorkspace(id: string, name: string): Promise<void> {
  await db.q(
    `INSERT INTO web.workspace (id, name, display_name, claimed_at) VALUES ($1, $2, $2, now())`,
    [id, name],
  );
}

beforeAll(async () => {
  db = await createScratchDb("web_scope404", { TOPOS_WEB_RATELIMIT: "off" });
  await seedWorkspace(WS_REAL, "acme");
  await seedWorkspace(WS_OTHER, "elsewhere");
  await seedUser(db, "u_member", "Member", "member@example.com");
  await seatUser(db, WS_REAL, "u_member", "member");
  // The stranger is a REAL signed-in user with a seat and an enrolled device — just not in
  // acme. Their probe of acme must look exactly like probing a workspace that does not exist.
  await seedUser(db, "u_stranger", "Stranger", "stranger@example.com");
  await seatUser(db, WS_OTHER, "u_stranger", "owner");
  await seedDevice(db, STRANGER_DEVICE, "u_stranger");
}, 60000);

afterAll(async () => {
  await db.drop();
});

/** Normalize a loader outcome — thrown Response, thrown data(), or a return — to status+body. */
async function outcome(run: () => Promise<unknown>): Promise<{ status: number; body: string }> {
  try {
    const returned = await run();
    if (returned instanceof Response) {
      return { status: returned.status, body: await returned.text() };
    }
    return { status: 200, body: JSON.stringify(returned) };
  } catch (thrown) {
    if (thrown instanceof Response) {
      // Redirects carry their target — indistinguishability must include it.
      const location = thrown.headers.get("location") ?? "";
      return { status: thrown.status, body: `${location}\n${await thrown.text()}` };
    }
    // React Router's data() throw: a DataWithResponseInit carrying { data, init }.
    const dataThrow = thrown as { data?: unknown; init?: { status?: number } | null };
    return {
      status: dataThrow.init?.status ?? 0,
      body: JSON.stringify(dataThrow.data ?? null),
    };
  }
}

type RouteArgs = { request: Request; params: Record<string, string | undefined> };
type RouteFn = (args: RouteArgs) => Promise<unknown>;

function pageRequest(path: string): Request {
  return new Request(`${ORIGIN}${path}`, { headers: { accept: "text/html" } });
}

describe("a member PANEL page (channels index) — the existence blind, status AND body", () => {
  async function probe(ws: string): Promise<{ status: number; body: string }> {
    const { loader } = await import("@/routes/channels-index");
    return outcome(() =>
      (loader as RouteFn)({ request: pageRequest(`/${ws}/channels`), params: { ws } }),
    );
  }

  it("a signed-in NON-MEMBER on a real slug is byte-identical to an unknown slug (the 404)", async () => {
    session = { user: { id: "u_stranger", name: "Stranger", email: "stranger@example.com" } };
    const real = await probe("acme");
    const unknown = await probe("no-such-team");
    expect(real.status).toBe(404);
    expect(real).toEqual(unknown);
  });

  it("an ANONYMOUS visitor on a real slug is byte-identical to an unknown slug (the constant login bounce)", async () => {
    session = null;
    const real = await probe("acme");
    const unknown = await probe("no-such-team");
    expect(real.status).toBe(302);
    expect(real).toEqual(unknown);
  });

  it("a MEMBER still gets the page (the blind never locks members out)", async () => {
    session = { user: { id: "u_member", name: "Member", email: "member@example.com" } };
    const real = await probe("acme");
    expect(real.status).toBe(200);
  });
});

describe("the workspace ROOT face (dashboard) — signed-in strangers see no existence signal", () => {
  async function probe(ws: string): Promise<{ status: number; body: string }> {
    const { loader } = await import("@/routes/workspace-dashboard");
    return outcome(() => (loader as RouteFn)({ request: pageRequest(`/${ws}`), params: { ws } }));
  }

  it("a signed-in NON-MEMBER on a real slug is byte-identical to an unknown slug (the 404)", async () => {
    session = { user: { id: "u_stranger", name: "Stranger", email: "stranger@example.com" } };
    const real = await probe("acme");
    const unknown = await probe("no-such-team");
    expect(real.status).toBe(404);
    expect(real).toEqual(unknown);
  });

  it("an ANONYMOUS browser gets the constant teaser on real and unknown slugs alike", async () => {
    session = null;
    const real = await probe("acme");
    const unknown = await probe("no-such-team");
    expect(real.status).toBe(200);
    expect(real).toEqual(unknown);
  });
});

describe("a device-lane API route (/channels) — the uniform wire 404, status AND body", () => {
  async function probe(ws: string, bearer?: string): Promise<{ status: number; body: string }> {
    const { loader } = await import("@/routes/api.v1.channels");
    const headers: Record<string, string> = {};
    if (bearer !== undefined) {
      headers.authorization = `Bearer ${bearer}`;
    }
    const request = new Request(`${ORIGIN}/api/v1/workspaces/${ws}/channels`, { headers });
    return outcome(() => (loader as RouteFn)({ request, params: { ws } }));
  }

  it("an ANONYMOUS call (no bearer) on a real workspace id is byte-identical to an unknown id", async () => {
    const real = await probe(WS_REAL);
    const unknown = await probe("w_no_such");
    expect(real.status).toBe(404);
    expect(real).toEqual(unknown);
  });

  it("a NON-MEMBER's valid credential on a real workspace id is byte-identical to an unknown id", async () => {
    const real = await probe(WS_REAL, STRANGER_DEVICE);
    const unknown = await probe("w_no_such", STRANGER_DEVICE);
    expect(real.status).toBe(404);
    expect(real).toEqual(unknown);
    // …and byte-identical to the anonymous miss too: one envelope for every miss on the lane.
    expect(real).toEqual(await probe(WS_REAL));
  });
});
