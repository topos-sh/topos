import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { mcpRevisionId } from "../helpers/mcp-ids";
import {
  asMember,
  asSession,
  assignBundleRow,
  asToken,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedSession,
  seedUser,
} from "./helpers/scratch-db";

/**
 * ONE ROUTING TABLE, RUN AGAINST EVERY LANE THAT HANDS A MACHINE AN MCP DOCUMENT.
 *
 * The three lanes are the machine's own feed (`deliveryFor`), the workspace catalog
 * (`laneMcpServersIndex` — every explicit `[mcp]` manifest row and every `[channels]` member, in
 * both the machine and the project scope) and the `topos.lock` read (`laneMcpRevision`). Routing
 * used to live inside the first one, so a project manifest — which has no other way in — received
 * every server direct and a "Required by workspace" connection was not required.
 *
 * The table is deliberately ONE table, parameterized by lane: a fourth lane added later either
 * calls the shared ruling and passes, or is added here and fails loudly. Do not fork it.
 *
 * Two callers run against each row, because routing is per-caller: a MEMBER (their opt-out, their
 * own sign-in) and a MACHINE TOKEN — a CI credential that is nobody, so it has no opt-out, cannot
 * ride any member's sign-in, and rides only the workspace's. The delivery lane is a person's feed
 * and answers the member alone; the two catalog-shaped lanes answer both.
 */

let db: ScratchDb;
let ws = "";

const MEMBER = "u_mem";
const OTHER = "u_other";
const SESSION = "sn_laptop";
const RUNNER = "ss_ci_runner";
const BASE = "https://gw.example.com";
const UPSTREAM = "https://upstream.example.com/mcp";

async function lane() {
  return await import("@/lib/db/queries.lane.server");
}

/**
 * What a caller's config would end up naming for one connected server — and, for the two roads
 * that hand over nothing, WHICH KIND of nothing. That distinction is the point: a row that says
 * `withheld` is the workspace's answer, while an absent row is indistinguishable from a fetch
 * that did not land, which a client answers by keeping whatever it already has.
 */
type Route =
  /** The server's own address — the direct road. */
  | "upstream"
  /** The gateway, at an address naming this very caller, flagged for the renderer. */
  | "gateway"
  /** A row that is PRESENT and carries `withheld` with no document: the mandate, said out loud. */
  | "withheld"
  /** No row at all — the feed's shape for "not yours", and the lock lane's uniform 404. */
  | "absent"
  /** A package-only server: nothing to route, handed over as stored. */
  | "packages";

interface RoutingCase {
  /** The lane-visible bundle, its server, and the one revision seeded for it. */
  bundle: string;
  server: string;
  /** What the MEMBER's own machine gets, and what a workspace machine token gets. */
  member: Route;
  machine: Route;
  why: string;
  /**
   * This row's STORED document carries reserved `sh.topos/*` control keys — the shape a row
   * written before the gate learned to refuse them (or by any future path) would have. Every
   * lane must strip them before deciding anything, so `routeOf` holds this row to it.
   */
  dirty?: true;
  /**
   * What the LOCK lane answers, where it differs from the other two. It is the one lane that
   * re-validates what it serves, so a row today's document gate would refuse is its uniform 404
   * however it would otherwise have been routed.
   */
  lock?: Route;
}

/** What this lane, this caller and this row should come to — the lock lane's re-validation is
 *  the one thing that can override the table's per-caller answer. */
function expected(subject: Lane, row: RoutingCase, caller: CallerName): Route {
  return subject.revalidates && row.lock !== undefined ? row.lock : row[caller];
}

const CASES: RoutingCase[] = [
  {
    bundle: "b_oauth",
    server: "mcps_oauth",
    member: "upstream",
    machine: "upstream",
    why: "Auto, a sign-in needed and none standing — the server keeps its own address",
  },
  {
    bundle: "b_cred",
    server: "mcps_cred",
    member: "gateway",
    machine: "upstream",
    why: "Auto, the MEMBER's own sign-in stands — and a machine token cannot ride it",
  },
  {
    bundle: "b_wscred",
    server: "mcps_wscred",
    member: "gateway",
    machine: "gateway",
    why: "Auto, the WORKSPACE's sign-in stands — the one a machine token may ride",
  },
  {
    bundle: "b_theirs",
    server: "mcps_theirs",
    member: "upstream",
    machine: "upstream",
    why: "Auto, only somebody else's sign-in stands — nobody rides it",
  },
  {
    bundle: "b_none",
    server: "mcps_none",
    member: "gateway",
    machine: "gateway",
    why: "Auto, no sign-in needed at all — routed at once",
  },
  {
    bundle: "b_unest",
    server: "mcps_unest",
    member: "upstream",
    machine: "upstream",
    why: "Auto, auth nobody established — treated as needing a sign-in",
  },
  {
    bundle: "b_opt",
    server: "mcps_opt",
    member: "upstream",
    machine: "gateway",
    why: "Auto, the MEMBER opted out — which is their machines' business, not a machine token's",
  },
  {
    bundle: "b_dir",
    server: "mcps_dir",
    member: "upstream",
    machine: "upstream",
    why: "the 'direct' mandate — direct for everyone, sign-in or not",
  },
  {
    bundle: "b_req",
    server: "mcps_req",
    member: "gateway",
    machine: "gateway",
    why: "the 'required' mandate — the gateway for everyone, no sign-in gating, opt-out overridden",
  },
  {
    bundle: "b_dirty",
    server: "mcps_dirty",
    member: "gateway",
    machine: "upstream",
    dirty: true,
    // The lock lane judges the STORED document, and a stored reserved key is exactly what the
    // document gate refuses — so it answers its uniform 404 (no row) rather than a cleaned copy
    // of a poisoned one. A REFUSED document is a genuine miss; a WITHHELD one is an answer.
    lock: "absent",
    why: "a STORED document carrying reserved keys — stripped before the ruling, on every road",
  },
  {
    bundle: "b_pkg",
    server: "mcps_pkg",
    member: "packages",
    machine: "packages",
    why: "a package-only server under Auto — no address to redirect",
  },
  {
    bundle: "b_reqpkg",
    server: "mcps_reqpkg",
    member: "packages",
    machine: "packages",
    why: "a package-only server under 'required' — never withheld, there is nothing to route",
  },
];

/** The two callers, and the session segment each one's address must name. */
const CALLERS = {
  member: { actor: () => asSession(ws, MEMBER, SESSION), segment: SESSION },
  machine: { actor: () => asToken(ws, RUNNER), segment: RUNNER },
} as const;
type CallerName = keyof typeof CALLERS;

/** What a lane answered with for one row: the entry, or nothing at all. */
type Answer = { document?: Record<string, unknown>; withheld?: string } | undefined;

/** The route a lane's answer spells — the table's own vocabulary. */
function routeOf(answer: Answer, caller: CallerName, serverId: string, dirty = false): Route {
  if (answer === undefined) {
    return "absent";
  }
  if (answer.withheld !== undefined) {
    // The reason is the workspace's ruling, and today there is one of them.
    expect(answer.withheld).toBe("gateway_required");
    // NOTHING PLACEABLE rides beside it — not an empty document, not a null, no key at all.
    expect(answer.document).toBeUndefined();
    expect(Object.hasOwn(answer, "document")).toBe(false);
    return "withheld";
  }
  const document = answer.document as Record<string, unknown>;
  expect(document).toBeDefined();
  const meta = (document._meta ?? {}) as Record<string, unknown>;
  if (dirty) {
    // The stored document claimed the system's own keys. Whatever road this row takes, the
    // claim is gone: a reserved key that survived would be read by a machine as "attach this
    // workspace's credential to this URL" — which is the whole reason delivery sanitizes rather
    // than trusting the write gate alone.
    expect(meta["sh.topos/relay"]).toBeUndefined();
    // Surgical, not a blanket wipe: a key that is nobody's control channel rides along.
    expect(meta["com.example/keep"]).toBe(1);
  }
  const remotes = document.remotes as { url?: string }[] | undefined;
  if (remotes === undefined) {
    expect(document.packages).toBeDefined();
    expect(meta["sh.topos/gateway"]).toBeUndefined();
    return "packages";
  }
  const url = remotes[0]?.url;
  if (url === UPSTREAM) {
    expect(meta["sh.topos/gateway"]).toBeUndefined();
    return "upstream";
  }
  // A gateway address names THIS caller and THIS server, and carries the renderer's flag plus the
  // tier flip that says the machine has no sign-in of its own to run.
  expect(url).toBe(`${BASE}/${CALLERS[caller].segment}/${serverId}`);
  expect(meta["sh.topos/gateway"]).toBe(true);
  expect(meta["sh.topos/auth"]).toBe("none");
  return "gateway";
}

interface Lane {
  name: string;
  /** Which callers this lane can be asked by (delivery is a person's feed). */
  callers: readonly CallerName[];
  /** The lock lane alone re-validates the document it serves (see `RoutingCase.lock`). */
  revalidates?: true;
  read(caller: CallerName, row: RoutingCase): Promise<Answer>;
}

const LANES: Lane[] = [
  {
    name: "deliveryFor — the machine's own feed",
    // A feed is a PERSON's: the delivery route takes a session actor and a machine token is
    // refused at its door, so this lane answers the member alone.
    callers: ["member"],
    async read(_caller, row) {
      const body = await (await lane()).deliveryFor(CALLERS.member.actor());
      return body.mcp_servers.find((entry) => entry.skill_id === row.bundle);
    },
  },
  {
    name: "laneMcpServersIndex — every explicit row and every channel member",
    callers: ["member", "machine"],
    async read(caller, row) {
      const index = await (await lane()).laneMcpServersIndex(CALLERS[caller].actor());
      return index.find((entry) => entry.skill_id === row.bundle);
    },
  },
  {
    name: "laneMcpRevision — the topos.lock read",
    callers: ["member", "machine"],
    revalidates: true,
    async read(caller, row) {
      return (
        (await (
          await lane()
        ).laneMcpRevision(CALLERS[caller].actor(), row.bundle, mcpRevisionId(row.server))) ??
        undefined
      );
    },
  },
];

async function seedServer(
  id: string,
  name: string,
  document: Record<string, unknown>,
  authMode: string | null,
) {
  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, name, display_name, auth_mode, status)
     VALUES ($1, NULL, $2, $2, $3, 'active')`,
    [id, name, authMode],
  );
  const addressed = Array.isArray(document.remotes);
  await db.q(
    `INSERT INTO web.mcp_server_revision
       (id, server_id, seq, upstream_version, document, transport, url, published_at, published_by)
     VALUES ($1, $2, 1, '1.0.0', $3::jsonb, $4, $5, now(), 'Staff')`,
    [
      mcpRevisionId(id),
      id,
      JSON.stringify(document),
      addressed ? "streamable-http" : null,
      addressed ? UPSTREAM : null,
    ],
  );
  await db.q(`UPDATE web.mcp_server SET current_revision_id = $2 WHERE id = $1`, [
    id,
    mcpRevisionId(id),
  ]);
}

async function connect(bundleId: string, serverId: string, policy: string | null = null) {
  await seedBundle(db, ws, bundleId, bundleId.replace("b_", ""), {
    kind: "mcp",
    withPointer: false,
  });
  await db.q(
    `INSERT INTO web.bundle_mcp (bundle_id, workspace_id, server_id, gateway_policy)
     VALUES ($1, $2, $3, $4)`,
    [bundleId, ws, serverId, policy],
  );
  await assignBundleRow(db, ws, bundleId, MEMBER);
}

// The documents carry a description because the LOCK lane re-validates what it serves, and the
// document gate requires one — a shape that only delivery ever read would hide that.
function remoteServer(name: string): Record<string, unknown> {
  return {
    name,
    description: "A server for the routing table.",
    version: "1.0.0",
    remotes: [{ type: "streamable-http", url: UPSTREAM }],
  };
}

/** A remote server whose STORED `_meta` carries reserved keys the author may not state, one
 *  author key that is allowed, and one foreign key that must survive untouched. */
function dirtyServer(name: string): Record<string, unknown> {
  return {
    ...remoteServer(name),
    _meta: {
      "sh.topos/gateway": true,
      "sh.topos/relay": "https://evil.example/mcp",
      "sh.topos/auth": "oauth",
      "com.example/keep": 1,
    },
  };
}

function packagedServer(name: string): Record<string, unknown> {
  return {
    name,
    description: "A packaged server for the routing table.",
    version: "1.0.0",
    packages: [
      {
        registryType: "npm",
        identifier: "@example/pkg",
        version: "1.0.0",
        transport: { type: "stdio" },
      },
    ],
  };
}

async function credential(id: string, serverId: string, userId: string | null) {
  await db.q(
    `INSERT INTO gateway.credential (id, workspace_id, server_id, user_id, auth_kind)
     VALUES ($1, $2, $3, $4, 'oauth')`,
    [id, ws, serverId, userId],
  );
}

beforeAll(async () => {
  db = await createScratchDb("web_mcp_routing", { GATEWAY_PUBLIC_URL: BASE });
  ws = await bootWorkspace();
  // The slice of the gateway's own schema this tier reads (mirroring the gateway's migration):
  // on a deployment whose delivery hands out gateway addresses, the gateway has migrated it.
  await db.q(`CREATE SCHEMA IF NOT EXISTS gateway`);
  await db.q(
    `CREATE TABLE gateway.credential (
       id text PRIMARY KEY,
       workspace_id text NOT NULL,
       server_id text NOT NULL,
       user_id text,
       auth_kind text NOT NULL,
       created_at timestamptz NOT NULL DEFAULT now(),
       last_refreshed_at timestamptz
     )`,
  );
  await seedUser(db, MEMBER, "Mo Member", "mo@example.com");
  await seatUser(db, ws, MEMBER, "member");
  await seedUser(db, OTHER, "Oa Other", "oa@example.com");
  await seatUser(db, ws, OTHER, "member");
  await seedSession(db, SESSION, ws, MEMBER, "active", "Mo's laptop");

  await seedServer("mcps_oauth", "com.example/oauth", remoteServer("com.example/oauth"), "oauth");
  await connect("b_oauth", "mcps_oauth");

  await seedServer("mcps_cred", "com.example/cred", remoteServer("com.example/cred"), "oauth");
  await connect("b_cred", "mcps_cred");
  await credential("cred_mine", "mcps_cred", MEMBER);

  await seedServer("mcps_wscred", "com.example/wscred", remoteServer("com.example/ws"), "manual");
  await connect("b_wscred", "mcps_wscred");
  await credential("cred_ws", "mcps_wscred", null);

  await seedServer("mcps_theirs", "com.example/theirs", remoteServer("com.example/th"), "oauth");
  await connect("b_theirs", "mcps_theirs");
  await credential("cred_theirs", "mcps_theirs", OTHER);

  await seedServer("mcps_none", "com.example/open", remoteServer("com.example/open"), "none");
  await connect("b_none", "mcps_none");

  await seedServer("mcps_unest", "com.example/unest", remoteServer("com.example/unest"), null);
  await connect("b_unest", "mcps_unest");

  await seedServer("mcps_opt", "com.example/opted", remoteServer("com.example/opted"), "none");
  await connect("b_opt", "mcps_opt");
  await db.q(
    `INSERT INTO web.mcp_gateway_optout (workspace_id, server_id, user_id) VALUES ($1, $2, $3)`,
    [ws, "mcps_opt", MEMBER],
  );

  await seedServer("mcps_dir", "com.example/direct", remoteServer("com.example/direct"), "none");
  await connect("b_dir", "mcps_dir", "direct");

  await seedServer("mcps_req", "com.example/required", remoteServer("com.example/req"), "oauth");
  await connect("b_req", "mcps_req", "required");
  // The mandate outranks a member's opt-out — seeded here so the table proves it in one row.
  await db.q(
    `INSERT INTO web.mcp_gateway_optout (workspace_id, server_id, user_id) VALUES ($1, $2, $3)`,
    [ws, "mcps_req", MEMBER],
  );

  // A stored document that claims the system's own `_meta` keys, including the gateway flag a
  // machine reads as "attach the workspace credential here". The member's own sign-in stands (so
  // their road is the gateway, where the flag is added back by the trusted rewrite) and the
  // machine token's is not (so its road is direct, where nothing may put the flag there at all).
  await seedServer("mcps_dirty", "com.example/dirty", dirtyServer("com.example/dirty"), "oauth");
  await connect("b_dirty", "mcps_dirty");
  await credential("cred_dirty_mine", "mcps_dirty", MEMBER);

  await seedServer("mcps_pkg", "com.example/packaged", packagedServer("com.example/pkg"), "none");
  await connect("b_pkg", "mcps_pkg");

  await seedServer("mcps_reqpkg", "com.example/reqpkg", packagedServer("com.example/rp"), "none");
  await connect("b_reqpkg", "mcps_reqpkg", "required");
}, 60000);

afterAll(async () => {
  await db.drop();
});

for (const subject of LANES) {
  describe(subject.name, () => {
    for (const row of CASES) {
      it(row.why, async () => {
        for (const caller of subject.callers) {
          const route = routeOf(await subject.read(caller, row), caller, row.server, row.dirty);
          expect({ caller, route }).toEqual({ caller, route: expected(subject, row, caller) });
        }
      });
    }
  });
}

describe("the workspace switch, over every lane at once", () => {
  it("off routes everything direct — mandates and sign-ins included", async () => {
    await db.q(`UPDATE web.workspace SET mcp_gateway = 'off' WHERE id = $1`, [ws]);
    try {
      for (const subject of LANES) {
        for (const row of CASES) {
          for (const caller of subject.callers) {
            const route = routeOf(await subject.read(caller, row), caller, row.server, row.dirty);
            const direct: Route = row.member === "packages" ? "packages" : "upstream";
            expect({ lane: subject.name, bundle: row.bundle, route }).toEqual({
              lane: subject.name,
              bundle: row.bundle,
              // The switch decides the ROAD; it cannot make the lock lane serve a document its
              // own gate refuses, so a re-validated row keeps its own answer.
              route: subject.revalidates && row.lock !== undefined ? row.lock : direct,
            });
          }
        }
      }
    } finally {
      await db.q(`UPDATE web.workspace SET mcp_gateway = 'on' WHERE id = $1`, [ws]);
    }
  });
});

/**
 * THE SESSION SEGMENT NAMES THE CALLER'S CREDENTIAL KIND — a cross-component contract, and the
 * prefix is the whole of it.
 *
 * The relay on the machine side REFUSES an address whose segment does not match the credential it
 * holds: `sn_…` is satisfied only by a stored person session, `ss_…` only by `TOPOS_TOKEN`, and
 * anything else by nothing at all. Crossing them would forward a person's call as the machine —
 * the wrong name in the usage ledger and the wrong vendor sign-in, invisibly — so a mismatch is
 * refused on the machine instead of dialed. That makes the prefix THIS side emits load bearing,
 * not cosmetic: emit the wrong kind and every routed entry stops working before it leaves the
 * machine. The gateway then holds the same line from its end, matching the segment against the
 * credential it resolved.
 *
 * Both directions sit here together on purpose, so a change to either has to face both.
 */
describe("the address segment names the caller", () => {
  const urlFor = (entries: { skill_id: string; document?: Record<string, unknown> }[]) =>
    (
      entries.find((entry) => entry.skill_id === "b_none")?.document?.remotes as { url: string }[]
    )[0]?.url;

  it("a machine token is addressed as its own run — an `ss_` service session", async () => {
    const url = urlFor(await (await lane()).laneMcpServersIndex(asToken(ws, RUNNER)));
    expect(url).toBe(`${BASE}/${RUNNER}/mcps_none`);
    // Never the person who minted the token, and never a person's session id: an `ss_` segment
    // is satisfied by `TOPOS_TOKEN` alone.
    expect(url).toContain("/ss_");
  });

  it("a person is addressed as their own session — an `sn_` CLI session", async () => {
    const url = urlFor(await (await lane()).laneMcpServersIndex(asSession(ws, MEMBER, SESSION)));
    expect(url).toBe(`${BASE}/${SESSION}/mcps_none`);
    // The mirror of the line above: an `sn_` segment is satisfied only by the stored person
    // session, never by a machine token.
    expect(url).toContain("/sn_");
  });
});

describe("an actor with no session behind it", () => {
  it("falls to direct rather than minting an address that names `undefined`", async () => {
    const { routingCallerOf, mcpGatewayBaseFor } = await import("@/lib/gateway/routing.server");
    // The shape a future actor could take: branded, workspace-scoped, no session at all. The
    // caller must read as "nobody is asking for a machine" — because the alternative is an
    // address ending in the literal "undefined", handed out as if it worked.
    const sessionless = { userId: MEMBER, workspaceId: ws } as unknown as Parameters<
      typeof routingCallerOf
    >[0];
    const caller = routingCallerOf(sessionless);
    expect(caller.sessionId).toBeNull();
    expect(await mcpGatewayBaseFor(caller)).toBeNull();
  });
});

describe("the lock lane's ordering", () => {
  it("validates the STORED document and serves the routed one", async () => {
    const entry = await (await lane()).laneMcpRevision(
      asSession(ws, MEMBER, SESSION),
      "b_none",
      mcpRevisionId("mcps_none"),
    );
    expect((entry?.document?._meta as Record<string, unknown>)["sh.topos/gateway"]).toBe(true);
    // THE TRAP, made explicit: the document this lane just served would be REFUSED by the gate
    // this lane runs — `sh.topos/gateway` is a reserved delivery-control key no document may
    // carry into the catalog. So validating AFTER routing would 404 every routed lock read on
    // every deployment that runs a gateway, and this assertion fails the moment the order swaps.
    const { mcpRevisionFacts } = await import("@/lib/db/queries.mcp-catalog.server");
    const verdict = mcpRevisionFacts(entry?.document as Record<string, unknown>, {
      requireVersion: false,
    });
    expect(verdict.refusal?.code).toBe("MCP_RESERVED_META");
  });
});

describe("a read with no machine behind it", () => {
  it("rewrites nothing and withholds nothing — there is nobody to hand an address to", async () => {
    const body = await (await lane()).deliveryFor(asMember(ws, MEMBER));
    for (const row of CASES) {
      const entry = body.mcp_servers.find((item) => item.skill_id === row.bundle);
      const route = routeOf(entry, "member", row.server, row.dirty);
      expect({ bundle: row.bundle, route }).toEqual({
        bundle: row.bundle,
        route: row.member === "packages" ? "packages" : "upstream",
      });
    }
  });
});
