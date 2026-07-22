import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { loader as deliveryLoader } from "@/routes/api.v1.delivery";
import { action as deviceAction, loader as deviceWrongMethod } from "@/routes/api.v1.device";
import { action as linkApply, loader as linkDescribe } from "@/routes/api.v1.device-link";
import { loader as meLoader } from "@/routes/api.v1.me";
import { action as reportAction } from "@/routes/api.v1.report";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The device-link WIRE — the served routes end to end against a REAL scratch Postgres: the
 * link describe/apply pair (`/api/v1/device/link`), the global self-revoke
 * (`DELETE /api/v1/device`), and the exactly-two pending-tolerant routes (`/me`, `/delivery`)
 * vs everything else's uniform 404. Identities ride the REAL ceremonies (claim → seats →
 * device flow), so the guards resolve exactly as production does; the byte shapes are pinned
 * field-for-field.
 */

const SETUP_CODE = "devlink-wire-setup-code";
const ORIGIN = "http://x";

let db: ScratchDb;
let wsId = "";
const CREDS = { owner: "", mem: "", pend: "" };
const DEVICES = { owner: "", mem: "", pend: "" };

type RouteHandler = (a: {
  request: Request;
  params: Record<string, string | undefined>;
}) => Promise<Response> | Response;

function req(method: string, path: string, opts: { cred?: string; body?: unknown } = {}): Request {
  const headers: Record<string, string> = {};
  if (opts.cred !== undefined) {
    headers.authorization = `Bearer ${opts.cred}`;
  }
  const init: RequestInit = { method, headers };
  if (opts.body !== undefined) {
    headers["content-type"] = "application/json";
    init.body = JSON.stringify(opts.body);
  }
  return new Request(`${ORIGIN}${path}`, init);
}

async function drive(
  h: unknown,
  request: Request,
  params: Record<string, string | undefined> = {},
): Promise<Response> {
  try {
    return await (h as RouteHandler)({ request, params });
  } catch (e) {
    if (e instanceof Response) {
      return e;
    }
    throw e;
  }
}

const DENIED_ACTIONS = [
  { code: "REQUEST_ACCESS", argv: [], mutates: false, needs_network: false },
  { code: "CONTACT_ADMIN", argv: [], mutates: false, needs_network: false },
];

const NOT_A_MEMBER_BODY = {
  schema_version: 1,
  command: "link",
  ok: false,
  data: {},
  warnings: [],
  next_actions: DENIED_ACTIONS,
  error: {
    code: "NOT_A_MEMBER",
    outcome: "DENIED",
    retryable: false,
    affected: {},
    context: {
      message:
        "not a member of that workspace — ask a workspace owner to invite you; an invitation link redeems on this device",
    },
    next_actions: DENIED_ACTIONS,
  },
};

function okLinkBody(data: Record<string, unknown>) {
  return {
    schema_version: 1,
    command: "link",
    ok: true,
    data,
    warnings: [],
    next_actions: [],
  };
}

async function expectUniform404(res: Response): Promise<void> {
  expect(res.status).toBe(404);
  const body = (await res.json()) as { error?: { code?: string } };
  expect(body.error?.code).toBe("NOT_FOUND");
}

async function mint(userId: string, display: string, name: string) {
  const identity = await import("@/lib/db/identity.server");
  const flow = await identity.startDeviceAuth(name, "");
  const approved = await identity.approveDeviceAuth(flow.userCode, { userId, display });
  if (approved === null) {
    throw new Error("approve refused in seed");
  }
  return { credential: flow.deviceCode, deviceId: approved.deviceId };
}

beforeAll(async () => {
  db = await createScratchDb("web_devlink_wire", {
    TOPOS_SETUP_CODE: SETUP_CODE,
    TOPOS_WEB_RATELIMIT: "off",
  });
  wsId = await bootWorkspace();
  const identity = await import("@/lib/db/identity.server");
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seedUser(db, "u_mem", "Member", "mem@example.com");
  await seedUser(db, "u_out", "Outsider", "out@example.com");
  const claimed = await identity.consumeClaim(SETUP_CODE, "u_owner", "Owner");
  if (claimed === null) {
    throw new Error("claim seed failed");
  }
  await seatUser(db, wsId, "u_mem", "member", "u_owner");

  const owner = await mint("u_owner", "Owner", "owner-box");
  CREDS.owner = owner.credential;
  DEVICES.owner = owner.deviceId;
  const mem = await mint("u_mem", "Member", "mem-box");
  CREDS.mem = mem.credential;
  DEVICES.mem = mem.deviceId;
  // A member device whose link is born PENDING (the knob held it).
  await db.q(`UPDATE web.workspace SET device_approval = 'on' WHERE id = $1`, [wsId]);
  const pend = await mint("u_mem", "Member", "pend-box");
  CREDS.pend = pend.credential;
  DEVICES.pend = pend.deviceId;
  await db.q(`UPDATE web.workspace SET device_approval = 'off' WHERE id = $1`, [wsId]);
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("GET /device/link (the describe)", () => {
  it("no credential — the uniform 404, like every miss on the lane", async () => {
    await expectUniform404(await drive(linkDescribe, req("GET", "/api/v1/device/link")));
    await expectUniform404(
      await drive(linkDescribe, req("GET", "/api/v1/device/link", { cred: "bogus" })),
    );
  });

  it("a linked member: standing + the forward look, the all-outcome 200 envelope", async () => {
    const res = await drive(
      linkDescribe,
      req("GET", "/api/v1/device/link?workspace=", { cred: CREDS.mem }),
    );
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual(
      okLinkBody({
        workspace_id: wsId,
        name: "team",
        display_name: "team",
        address: "http://x",
        role: "member",
        link_status: "active",
        born: "active",
      }),
    );
  });

  it("seatless caller and unknown workspace answer ONE byte-identical NOT_A_MEMBER", async () => {
    // The outsider holds NO seat — mint their device while seated, then remove the seat.
    await seatUser(db, wsId, "u_out", "member", "u_owner");
    const out = await mint("u_out", "Outsider", "out-box");
    await db.q(`DELETE FROM web.seat WHERE workspace_id = $1 AND user_id = 'u_out'`, [wsId]);

    const seatless = await drive(
      linkDescribe,
      req("GET", "/api/v1/device/link?workspace=team", { cred: out.credential }),
    );
    const unknown = await drive(
      linkDescribe,
      req("GET", "/api/v1/device/link?workspace=never-was", { cred: CREDS.mem }),
    );
    expect(seatless.status).toBe(200);
    const seatlessBody = await seatless.json();
    expect(seatlessBody).toEqual(NOT_A_MEMBER_BODY);
    expect(await unknown.json()).toEqual(seatlessBody);
  });
});

describe("POST /device/link (the apply)", () => {
  it("creates idempotently: a fresh link lands, a repeat answers ok with the current status", async () => {
    const identity = await import("@/lib/db/identity.server");
    // Unlink first so the apply has something to create.
    await identity.selfUnlinkDevice({ userId: "u_mem", display: "M" }, DEVICES.mem, wsId);
    const applied = await drive(
      linkApply,
      req("POST", "/api/v1/device/link", { cred: CREDS.mem, body: { workspace: "" } }),
    );
    expect(await applied.json()).toEqual(
      okLinkBody({
        workspace_id: wsId,
        name: "team",
        display_name: "team",
        address: "http://x",
        link_status: "active",
      }),
    );
    const again = await drive(
      linkApply,
      req("POST", "/api/v1/device/link", { cred: CREDS.mem, body: { workspace: "team" } }),
    );
    expect(await again.json()).toEqual(
      okLinkBody({
        workspace_id: wsId,
        name: "team",
        display_name: "team",
        address: "http://x",
        link_status: "active",
      }),
    );
    const rows = await db.q(
      `SELECT 1 FROM web.device_link WHERE device_id = $1 AND workspace_id = $2`,
      [DEVICES.mem, wsId],
    );
    expect(rows).toHaveLength(1);
  });

  it("a malformed body is a 400; an unknown workspace the NOT_A_MEMBER refusal", async () => {
    const bad = await drive(
      linkApply,
      req("POST", "/api/v1/device/link", { cred: CREDS.mem, body: { workspace: 7 } }),
    );
    expect(bad.status).toBe(400);
    const unknown = await drive(
      linkApply,
      req("POST", "/api/v1/device/link", { cred: CREDS.mem, body: { workspace: "never-was" } }),
    );
    expect(await unknown.json()).toEqual(NOT_A_MEMBER_BODY);
  });
});

describe("the pending link's wire (exactly TWO tolerant routes)", () => {
  it("GET /me answers typed with link_status pending — the person IS seated", async () => {
    const res = await drive(
      meLoader,
      req("GET", `/api/v1/workspaces/${wsId}/me`, { cred: CREDS.pend }),
      { ws: wsId },
    );
    expect(res.status).toBe(200);
    const body = (await res.json()) as Record<string, unknown>;
    expect(body.link_status).toBe("pending");
    expect(body.role).toBe("member");
  });

  it("GET /delivery answers the shape-complete EMPTY body", async () => {
    const res = await drive(
      deliveryLoader,
      req("GET", `/api/v1/workspaces/${wsId}/delivery`, { cred: CREDS.pend }),
      { ws: wsId },
    );
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({
      schema_version: 1,
      workspace_id: wsId,
      link_status: "pending",
      skills: [],
      detached: [],
      notices: [],
      proposals_awaiting: 0,
      staleness_window_ms: 604800000,
    });
  });

  it("EVERYTHING else folds pending into the uniform 404 — the report PUT included", async () => {
    const res = await drive(
      reportAction,
      req("PUT", `/api/v1/workspaces/${wsId}/report`, {
        cred: CREDS.pend,
        body: { schema_version: 1, applied: [] },
      }),
      { ws: wsId },
    );
    await expectUniform404(res);
  });

  it("an owner's approval opens the lane for the same credential", async () => {
    const identity = await import("@/lib/db/identity.server");
    expect(
      await identity.approveDeviceLink({ userId: "u_owner", display: "O" }, wsId, DEVICES.pend),
    ).toBe("approved");
    const res = await drive(
      deliveryLoader,
      req("GET", `/api/v1/workspaces/${wsId}/delivery`, { cred: CREDS.pend }),
      { ws: wsId },
    );
    const body = (await res.json()) as Record<string, unknown>;
    expect(body.link_status).toBe("active");
  });
});

describe("DELETE /device (the global self-revoke)", () => {
  it("revokes the credential's own device, severs its links, answers the logout envelope; a retry is the uniform 404", async () => {
    const revoked = await mint("u_mem", "Member", "logout-box");
    const res = await drive(
      deviceAction,
      req("DELETE", "/api/v1/device", { cred: revoked.credential }),
    );
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({
      schema_version: 1,
      command: "logout",
      ok: true,
      data: { status: "revoked" },
      warnings: [],
      next_actions: [],
    });
    const rows = await db.q<{ revoked_at: string | null }>(
      `SELECT revoked_at FROM web.device WHERE id = $1`,
      [revoked.deviceId],
    );
    expect(rows[0]?.revoked_at).not.toBeNull();
    expect(
      await db.q(`SELECT 1 FROM web.device_link WHERE device_id = $1`, [revoked.deviceId]),
    ).toHaveLength(0);
    // The credential no longer resolves: the retry answers the uniform 404 — the client
    // treats it as already-signed-out.
    await expectUniform404(
      await drive(deviceAction, req("DELETE", "/api/v1/device", { cred: revoked.credential })),
    );
  });

  it("a wrong method on the path is the uniform 404 (the door owns it)", async () => {
    await expectUniform404(await drive(deviceWrongMethod, req("GET", "/api/v1/device")));
    await expectUniform404(
      await drive(deviceAction, req("POST", "/api/v1/device", { cred: CREDS.mem })),
    );
  });
});
