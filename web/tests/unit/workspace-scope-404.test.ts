import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { laneHeaders } from "./helpers/lane";
import {
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedSession,
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
const STRANGER_DEVICE = "sn_stranger";

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
  // The stranger is a REAL signed-in user with a seat and a live session — just not in
  // acme. Their probe of acme must look exactly like probing a workspace that does not exist.
  await seedUser(db, "u_stranger", "Stranger", "stranger@example.com");
  await seatUser(db, WS_OTHER, "u_stranger", "owner");
  await seedSession(db, STRANGER_DEVICE, WS_OTHER, "u_stranger");
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

  it("an ANONYMOUS visitor on a real slug is byte-identical to an unknown slug (the same 404)", async () => {
    // This page used to bounce to /login. The bounce was blind — constant for every slug — but it
    // was a SECOND answer for an address family whose other pages 404'd, and it read as an
    // invitation to sign in to a workspace the visitor holds no seat in.
    session = null;
    const real = await probe("acme");
    const unknown = await probe("no-such-team");
    expect(real.status).toBe(404);
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

  it("an ANONYMOUS browser gets the SAME 404 on real and unknown slugs alike", async () => {
    // A workspace address is members-only in every face. A signed-out visitor is refused before
    // anything is read, so the refusal cannot depend on whether the slug names a workspace.
    session = null;
    const real = await probe("acme");
    const unknown = await probe("no-such-team");
    expect(real.status).toBe(404);
    expect(real).toEqual(unknown);
  });
});

describe("a device-lane API route (/channels) — the uniform wire 404, status AND body", () => {
  async function probe(ws: string, bearer?: string): Promise<{ status: number; body: string }> {
    const { loader } = await import("@/routes/api.v1.channels");
    const headers = laneHeaders();
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

/**
 * THE BUNDLE'S OWN SUB-PAGES — one address family, one answer.
 *
 * `/<ws>/skills/<bundle>` answered a signed-out visitor with the house 404, but its three
 * siblings — a version, a file inside that version, and the history — bounced to /login instead.
 * Every one of those was still existence-blind on its own (the bounce is constant), but the app
 * had two different answers for one address family, and the bounce read as an invitation to sign
 * in to a workspace the visitor has no seat in. They now answer exactly what the bundle page
 * answers, to the byte.
 */
describe("a bundle's sub-pages — the same refusal the bundle page gives", () => {
  const VERSION = "6ce009dd52c7";

  const pages: [string, () => Promise<{ loader: unknown }>, Record<string, string>][] = [
    ["the bundle page", () => import("@/routes/skill-current"), {}],
    ["a version's files", () => import("@/routes/version-files"), { versionId: VERSION }],
    [
      "one file in a version",
      () => import("@/routes/file-view"),
      { versionId: VERSION, "*": "SKILL.md" },
    ],
    ["the history", () => import("@/routes/skill-history"), {}],
  ];

  async function probe(
    load: () => Promise<{ loader: unknown }>,
    ws: string,
    extra: Record<string, string>,
  ): Promise<{ status: number; body: string }> {
    const { loader } = await load();
    return outcome(() =>
      (loader as RouteFn)({
        request: pageRequest(`/${ws}/skills/release-guide`),
        params: { ws, skill: "release-guide", ...extra },
      }),
    );
  }

  for (const [what, load, extra] of pages) {
    it(`${what}: an ANONYMOUS visitor gets the 404 on a real slug and an invented one alike`, async () => {
      session = null;
      const real = await probe(load, "acme", extra);
      const unknown = await probe(load, "no-such-team", extra);
      expect(real.status).toBe(404);
      expect(real).toEqual(unknown);
    });

    it(`${what}: a signed-in NON-MEMBER gets that same 404`, async () => {
      session = { user: { id: "u_stranger", name: "Stranger", email: "stranger@example.com" } };
      const real = await probe(load, "acme", extra);
      expect(real.status).toBe(404);
      expect(real).toEqual(await probe(load, "no-such-team", extra));
    });
  }

  it("answers all four addresses with the SAME body — no shape says more than another", async () => {
    session = null;
    const answers = await Promise.all(
      pages.map(async ([, load, extra]) => await probe(load, "acme", extra)),
    );
    for (const answer of answers) {
      expect(answer).toEqual(answers[0]);
    }
  });
});

/**
 * EVERY MEMBER-ONLY PAGE, one answer.
 *
 * The ruling is uniform: any `/<ws>…` page answers a signed-out visitor the house 404, with the
 * same body a slug nobody has ever registered gets. Some pages already did; the rest bounced to
 * /login. Each bounce was existence-blind on its own — it is constant for every slug — but the
 * app had two answers for one address family, so which page a stranger happened to try told them
 * something, and the bounce read as an invitation to sign in to a workspace they hold no seat in.
 *
 * This walks the whole set through its REAL loader, twice: anonymous, and as a signed-in stranger
 * with a live session and a seat somewhere else. Both must match an invented slug to the byte,
 * and every page must match every other — no shape may say more than another.
 */
describe("every member-only page — the one refusal, byte for byte", () => {
  /** Every member-only page under a workspace address: its module, the params its route binds,
   *  and a path of the right shape (nothing reads it before the guard refuses). */
  const pages: {
    what: string;
    load: () => Promise<{ loader: unknown }>;
    params: Record<string, string>;
    path: string;
  }[] = [
    {
      what: "the channels index",
      load: () => import("@/routes/channels-index"),
      params: {},
      path: "/channels",
    },
    {
      what: "a channel's history",
      load: () => import("@/routes/channel-history"),
      params: { channel: "everyone" },
      path: "/channels/everyone/history",
    },
    {
      what: "the new-channel page",
      load: () => import("@/routes/channel-new"),
      params: {},
      path: "/channels/new",
    },
    {
      what: "a channel's settings",
      load: () => import("@/routes/channel-settings"),
      params: { channel: "everyone" },
      path: "/channels/everyone/settings",
    },
    {
      what: "an MCP server's connect page",
      load: () => import("@/routes/mcp-connect"),
      params: { server: "deepwiki" },
      path: "/mcp/deepwiki/connect",
    },
    {
      what: "the add-an-MCP-server page",
      load: () => import("@/routes/mcp-new"),
      params: {},
      path: "/mcp/new",
    },
    {
      what: "the profile page",
      load: () => import("@/routes/profile"),
      params: {},
      path: "/profile",
    },
    {
      what: "a proposal's review page",
      load: () => import("@/routes/proposal-review"),
      params: { skill: "release-guide", versionId: "a".repeat(64) },
      path: `/skills/release-guide/proposals/${"a".repeat(64)}`,
    },
    {
      what: "the workspace sessions page",
      load: () => import("@/routes/sessions"),
      params: {},
      path: "/settings/sessions",
    },
    {
      what: "the import-from-GitHub page",
      load: () => import("@/routes/skill-import"),
      params: {},
      path: "/skills/import",
    },
    {
      what: "a bundle's proposals",
      load: () => import("@/routes/skill-proposals"),
      params: { skill: "release-guide" },
      path: "/skills/release-guide/proposals",
    },
    {
      what: "a bundle's settings",
      load: () => import("@/routes/skill-settings"),
      params: { skill: "release-guide" },
      path: "/skills/release-guide/settings",
    },
    {
      what: "the visibility page",
      load: () => import("@/routes/visibility"),
      params: {},
      path: "/visibility",
    },
    {
      what: "the archive",
      load: () => import("@/routes/workspace-archive"),
      params: {},
      path: "/settings/archive",
    },
    {
      what: "the members page",
      load: () => import("@/routes/workspace-members"),
      params: {},
      path: "/members",
    },
    {
      what: "the workspace settings page",
      load: () => import("@/routes/workspace-settings"),
      params: {},
      path: "/settings",
    },
  ];

  async function probePage(
    page: (typeof pages)[number],
    ws: string,
  ): Promise<{ status: number; body: string }> {
    const { loader } = await page.load();
    return outcome(() =>
      (loader as RouteFn)({
        request: pageRequest(`/${ws}${page.path}`),
        params: { ws, ...page.params },
      }),
    );
  }

  for (const page of pages) {
    it(`${page.what}: anonymous and stranger both get the 404, real slug and invented alike`, async () => {
      session = null;
      const anonReal = await probePage(page, "acme");
      expect(anonReal.status).toBe(404);
      expect(anonReal).toEqual(await probePage(page, "no-such-team"));

      session = { user: { id: "u_stranger", name: "Stranger", email: "stranger@example.com" } };
      const strangerReal = await probePage(page, "acme");
      expect(strangerReal.status).toBe(404);
      expect(strangerReal).toEqual(await probePage(page, "no-such-team"));
      // …and the two visitors are indistinguishable from each other, which is the whole point.
      expect(strangerReal).toEqual(anonReal);
    });
  }

  it("answers every one of them with the SAME body — no page says more than another", async () => {
    session = null;
    const answers = await Promise.all(pages.map(async (page) => await probePage(page, "acme")));
    for (const answer of answers) {
      expect(answer).toEqual(answers[0]);
    }
  });
});

/**
 * THE LAYER A SIGNED-OUT VISITOR ACTUALLY MEETS.
 *
 * Every page above sits under the signed-in shell, whose middleware runs BEFORE any loader. It
 * used to bounce a cookie-less request to /login unconditionally, so the guards' answer was the
 * one nobody ever saw: fixing the loaders alone would have changed nothing in a browser. The
 * shell now gives the same answer its children do, and the fork is the address itself — a
 * workspace page is a members-only address (the house 404), a person's own page has somewhere to
 * send them.
 */
describe("the shell's signed-out refusal — the same answer its children give", () => {
  const PERSONAL = new Set(["/account/sessions", "/new"]);

  async function refuse(path: string): Promise<{ status: number; body: string }> {
    const { refuseShellSignedOut } = await import("@/lib/auth/guards.server");
    return outcome(async () => refuseShellSignedOut(pageRequest(path), PERSONAL));
  }

  it("answers a workspace page with the house 404, real slug and invented alike", async () => {
    const real = await refuse("/acme/members");
    expect(real.status).toBe(404);
    expect(real).toEqual(await refuse("/no-such-team/members"));
    expect(real).toEqual(await refuse("/acme/skills/release-guide/history"));
  });

  it("keeps the /login bounce for a page that is a person's, not a workspace's", async () => {
    expect(await refuse("/account/sessions")).toEqual({ status: 302, body: "/login\n" });
    expect(await refuse("/new")).toEqual({ status: 302, body: "/login\n" });
  });

  it("reads the DESTINATION path, so a client-side arrival is the same page", async () => {
    // React Router's single fetch asks for `<path>.data`; the person is opening `<path>`.
    expect(await refuse("/account/sessions.data")).toEqual({ status: 302, body: "/login\n" });
    expect((await refuse("/acme/members.data")).status).toBe(404);
  });
});
