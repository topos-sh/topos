import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { createScratchDb, type ScratchDb, seatUser, seedUser } from "./helpers/scratch-db";

/**
 * The MULTI-tenant device-enrollment lane against a REAL scratch Postgres, the composition
 * mocked to `tenancy: "multi"` (the OSS build is single-tenant; a superset passes multi).
 *
 * The authorize matrix: an empty workspace has NO origin default (uniform 404); a
 * shape-invalid name is the uniform 404 (such a name can never exist); a shape-VALID name
 * mints the flow with the slug recorded and NO existence check — the unauthenticated start
 * must not be a workspace-existence oracle, and a CLI-first stranger may be enrolling toward
 * a workspace they will create mid-flow. Resolution + authorization happen at APPROVAL: the
 * approver (and the denier) must hold a seat in the workspace the recorded slug resolves to,
 * inside the same FOR-UPDATE fence; every failure is the one uniform refusal.
 */

vi.mock("@/composition.server", () => ({
  composition: { tenancy: "multi" as const },
}));

let db: ScratchDb;
let wsAcme = "";

const ORIGIN = "http://x";

type RouteAction = (a: {
  request: Request;
  params: Record<string, string | undefined>;
}) => Promise<Response>;

async function authorize(body: unknown): Promise<Response> {
  const { action } = await import("@/routes/api.v1.device-authorize");
  return await (action as RouteAction)({
    request: new Request(`${ORIGIN}/api/v1/device/authorize`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    }),
    params: {},
  });
}

async function expectUniform404(res: Response): Promise<void> {
  expect(res.status).toBe(404);
  const body = (await res.json()) as { error?: { code?: string } };
  expect(body.error?.code).toBe("NOT_FOUND");
}

/** A product-born workspace row (claimed, no boot code) — multi mints no boot workspace. */
async function seedWorkspace(id: string, name: string): Promise<void> {
  await db.q(
    `INSERT INTO web.workspace (id, name, display_name, claimed_at) VALUES ($1, $2, $2, now())`,
    [id, name],
  );
}

async function flowRow(userCode: string): Promise<{ requested_workspace: string } | undefined> {
  const rows = await db.q<{ requested_workspace: string }>(
    `SELECT requested_workspace FROM web.device_auth_session WHERE user_code = $1`,
    [userCode],
  );
  return rows[0];
}

beforeAll(async () => {
  db = await createScratchDb("web_devmulti", { TOPOS_WEB_RATELIMIT: "off" });
  wsAcme = "w_acme";
  await seedWorkspace(wsAcme, "acme");
  await seedUser(db, "u_in", "Insider", "insider@example.com");
  await seedUser(db, "u_out", "Outsider", "outsider@example.com");
  await seatUser(db, wsAcme, "u_in", "member");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("the authorize matrix (multi)", () => {
  it("an EMPTY workspace is the uniform 404 — no origin default exists", async () => {
    await expectUniform404(await authorize({ requested_name: "box", workspace: "" }));
  });

  it("a shape-invalid name is the uniform 404 (such a name can never exist)", async () => {
    await expectUniform404(await authorize({ requested_name: "box", workspace: "Bad_Slug!" }));
  });

  it("an over-long name is the uniform 404 (the shape rule caps at 100)", async () => {
    await expectUniform404(await authorize({ requested_name: "box", workspace: "a".repeat(101) }));
  });

  it("a valid slug mints the flow with the slug recorded", async () => {
    const res = await authorize({ requested_name: "in-box", workspace: "acme" });
    expect(res.status).toBe(200);
    const flow = (await res.json()) as { user_code: string };
    expect((await flowRow(flow.user_code))?.requested_workspace).toBe("acme");
  });

  it("a valid slug that names NO existing workspace still mints (no existence oracle)", async () => {
    const res = await authorize({ requested_name: "ghost-box", workspace: "ghost-team" });
    expect(res.status).toBe(200);
    const flow = (await res.json()) as { user_code: string };
    expect((await flowRow(flow.user_code))?.requested_workspace).toBe("ghost-team");
  });
});

describe("approval resolves the slug and requires a seat", () => {
  it("a member of the flow's workspace approves — device + audit land in THAT workspace", async () => {
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startDeviceAuth("member-box", "acme");
    const approved = await identity.approveDeviceAuth(flow.userCode, {
      userId: "u_in",
      display: "Insider",
    });
    expect(approved).not.toBeNull();
    const granted = await identity.pollDeviceAuth(flow.deviceCode);
    expect(granted.status).toBe("granted");
    const devices = await db.q<{ user_id: string; display_name: string }>(
      `SELECT user_id, display_name FROM web.device WHERE id = $1`,
      [approved?.deviceId as string],
    );
    expect(devices[0]).toEqual({ user_id: "u_in", display_name: "member-box" });
    const audits = await db.q<{ workspace_id: string }>(
      `SELECT workspace_id FROM web.audit_event WHERE kind = 'device_approved' AND subject = $1`,
      [approved?.deviceId as string],
    );
    expect(audits).toEqual([{ workspace_id: wsAcme }]);
  });

  it("a signed-in NON-member's approve (and deny) is the uniform refusal; the flow survives", async () => {
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startDeviceAuth("coveted-box", "acme");
    expect(
      await identity.approveDeviceAuth(flow.userCode, { userId: "u_out", display: "Outsider" }),
    ).toBeNull();
    expect(
      await identity.denyDeviceAuth(flow.userCode, { userId: "u_out", display: "Outsider" }),
    ).toBe(false);
    expect((await identity.pollDeviceAuth(flow.deviceCode)).status).toBe("pending");
    // The seated member can still deny it — the refusals consumed nothing.
    expect(
      await identity.denyDeviceAuth(flow.userCode, { userId: "u_in", display: "Insider" }),
    ).toBe(true);
  });

  it("a flow naming a NONEXISTENT slug approves null — until the workspace is created mid-flow", async () => {
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startDeviceAuth("first-box", "ghost-team");
    // Nobody can approve a flow whose workspace does not exist — whoever they are.
    expect(
      await identity.approveDeviceAuth(flow.userCode, { userId: "u_out", display: "Outsider" }),
    ).toBeNull();
    expect((await identity.pollDeviceAuth(flow.deviceCode)).status).toBe("pending");

    // The create-mid-flow ordering: the workspace is born (and the person seated) AFTER the
    // enrollment started; the SAME flow then approves into it.
    await seedWorkspace("w_ghost", "ghost-team");
    await seatUser(db, "w_ghost", "u_out", "owner");
    const approved = await identity.approveDeviceAuth(flow.userCode, {
      userId: "u_out",
      display: "Outsider",
    });
    expect(approved).not.toBeNull();
    const audits = await db.q<{ workspace_id: string }>(
      `SELECT workspace_id FROM web.audit_event WHERE kind = 'device_approved' AND subject = $1`,
      [approved?.deviceId as string],
    );
    expect(audits).toEqual([{ workspace_id: "w_ghost" }]);
    expect((await identity.pollDeviceAuth(flow.deviceCode)).status).toBe("granted");
  });
});
