import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { createScratchDb, type ScratchDb, seatUser, seedUser } from "./helpers/scratch-db";

/**
 * The MULTI-tenant login lane against a REAL scratch Postgres, the composition mocked to
 * `tenancy: "multi"` (the OSS build is single-tenant; a superset passes multi).
 *
 * The authorize matrix: the start is WORKSPACE-LESS and mints the flow row ALWAYS — no
 * tenancy branch, no workspace read, so the unauthenticated start discloses nothing about
 * workspaces or accounts. An optional `preselect` slug is recorded shape-checked but
 * UNRESOLVED (a shape-invalid slug is the uniform 404 — such a name can never exist; a valid
 * one naming NOTHING still mints). The workspace is CHOSEN at approval — a seat pick, an
 * invitation accept, or a create, each validated inside the approve fence — and the SESSION
 * mints at the CLI's exchange (the first poll after approval).
 */

vi.mock("@/composition.server", () => ({
  composition: {
    tenancy: "multi" as const,
    reservedWorkspaceNames: [],
    entitlements: {
      forWorkspace: () => Promise.resolve({ allows: () => true, limit: () => null }),
    },
  },
}));

let db: ScratchDb;
let wsAcme = "";

const ORIGIN = "http://x";

type RouteAction = (a: {
  request: Request;
  params: Record<string, string | undefined>;
}) => Promise<Response>;

async function authorize(body: unknown): Promise<Response> {
  const { action } = await import("@/routes/api.v1.login-authorize");
  return await (action as RouteAction)({
    request: new Request(`${ORIGIN}/api/v1/login/authorize`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    }),
    params: {},
  });
}

async function tokenPoll(deviceCode: string): Promise<Response> {
  const { action } = await import("@/routes/api.v1.login-token");
  return await (action as RouteAction)({
    request: new Request(`${ORIGIN}/api/v1/login/token`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ device_code: deviceCode }),
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

async function flowRow(
  userCode: string,
): Promise<{ preselect_workspace: string | null } | undefined> {
  const rows = await db.q<{ preselect_workspace: string | null }>(
    `SELECT preselect_workspace FROM web.login_flow WHERE user_code = $1`,
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
  it("a shape-invalid preselect is the uniform 404 (such a name can never exist)", async () => {
    await expectUniform404(await authorize({ requested_name: "box", preselect: "Bad_Slug!" }));
  });

  it("an over-long preselect is the uniform 404 (the shape rule caps at 100)", async () => {
    await expectUniform404(await authorize({ requested_name: "box", preselect: "a".repeat(101) }));
  });

  it("no preselect mints the flow — the start is workspace-less", async () => {
    const res = await authorize({ requested_name: "bare-box" });
    expect(res.status).toBe(200);
    const flow = (await res.json()) as { user_code: string };
    expect((await flowRow(flow.user_code))?.preselect_workspace).toBeNull();
  });

  it("a valid preselect is recorded verbatim", async () => {
    const res = await authorize({ requested_name: "in-box", preselect: "acme" });
    expect(res.status).toBe(200);
    const flow = (await res.json()) as { user_code: string };
    expect((await flowRow(flow.user_code))?.preselect_workspace).toBe("acme");
  });

  it("a preselect naming NO existing workspace still mints (no existence oracle)", async () => {
    const res = await authorize({ requested_name: "ghost-box", preselect: "ghost-team" });
    expect(res.status).toBe(200);
    const flow = (await res.json()) as { user_code: string };
    expect((await flowRow(flow.user_code))?.preselect_workspace).toBe("ghost-team");
  });
});

describe("the approval's choice arms", () => {
  it("a SEAT pick: consent lands in that workspace; THE POLL mints the session there", async () => {
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("member-box", "acme");
    const approved = await identity.approveLoginFlow(
      flow.userCode,
      { userId: "u_in", display: "Insider" },
      { kind: "seat", workspace: "acme" },
    );
    expect(approved?.outcome).toBe("approved");
    // No session yet — the approval records consent + the chosen workspace and nothing more.
    expect(
      await db.q(`SELECT 1 FROM web.cli_session WHERE display_name = 'member-box'`),
    ).toHaveLength(0);
    const granted = await identity.pollLoginFlow(flow.flowCode);
    expect(granted.status).toBe("granted");
    const sessionId = granted.status === "granted" ? granted.sessionId : "";
    const sessions = await db.q<{ user_id: string; display_name: string; workspace_id: string }>(
      `SELECT user_id, display_name, workspace_id FROM web.cli_session WHERE id = $1`,
      [sessionId],
    );
    expect(sessions[0]).toEqual({
      user_id: "u_in",
      display_name: "member-box",
      workspace_id: wsAcme,
    });
    const audits = await db.q<{ workspace_id: string }>(
      `SELECT workspace_id FROM web.audit_event WHERE kind = 'session_created' AND subject = $1`,
      [sessionId],
    );
    expect(audits).toEqual([{ workspace_id: wsAcme }]);
  });

  it("a seat pick where NO seat stands is the uniform refusal; the flow survives", async () => {
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("coveted-box", "acme");
    expect(
      await identity.approveLoginFlow(
        flow.userCode,
        { userId: "u_out", display: "Outsider" },
        { kind: "seat", workspace: "acme" },
      ),
    ).toBeNull();
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("pending");
    // The seated member can still complete it — the refusal consumed nothing.
    expect(
      (
        await identity.approveLoginFlow(
          flow.userCode,
          { userId: "u_in", display: "Insider" },
          { kind: "seat", workspace: "acme" },
        )
      )?.outcome,
    ).toBe("approved");
  });

  it("a CREATE choice births the workspace inside the fence; the approver owns it", async () => {
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("first-box", "fresh-team");
    const approved = await identity.approveLoginFlow(
      flow.userCode,
      { userId: "u_out", display: "Outsider" },
      { kind: "create", displayName: "Fresh Team", slug: "fresh-team" },
    );
    expect(approved?.outcome).toBe("approved");
    expect(approved?.outcome === "approved" && approved.workspaceName).toBe("fresh-team");
    // The identical one-transaction birth /new runs: workspace + everyone channel + owner
    // seat + the baseline everyone-assignment + the audit row, via-tagged.
    const ws = await db.q<{ id: string; display_name: string }>(
      `SELECT id, display_name FROM web.workspace WHERE name = 'fresh-team'`,
    );
    expect(ws[0]?.display_name).toBe("Fresh Team");
    const newWsId = ws[0]?.id as string;
    expect(
      await db.q(`SELECT 1 FROM web.channel WHERE workspace_id = $1 AND is_default`, [newWsId]),
    ).toHaveLength(1);
    expect(
      await db.q(
        `SELECT 1 FROM web.seat WHERE workspace_id = $1 AND user_id = 'u_out' AND role = 'owner'`,
        [newWsId],
      ),
    ).toHaveLength(1);
    expect(
      await db.q(`SELECT 1 FROM web.assignment WHERE workspace_id = $1 AND user_id IS NULL`, [
        newWsId,
      ]),
    ).toHaveLength(1);
    const audit = await db.q<{ details: { via?: string } }>(
      `SELECT details FROM web.audit_event
       WHERE kind = 'workspace_created' AND workspace_id = $1`,
      [newWsId],
    );
    expect(audit[0]?.details?.via).toBe("login");
    // The mint follows at the exchange — the creator is the owner, so born active.
    const granted = await identity.pollLoginFlow(flow.flowCode);
    expect(granted.status).toBe("granted");
    expect(granted.status === "granted" && granted.sessionStatus).toBe("active");
    expect(granted.status === "granted" && granted.approvedWorkspaceId).toBe(newWsId);
  });

  it("a TAKEN create slug answers typed and rolls back — the flow stays pending", async () => {
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("squat-box", null);
    const taken = await identity.approveLoginFlow(
      flow.userCode,
      { userId: "u_out", display: "Outsider" },
      { kind: "create", displayName: "Acme Again", slug: "acme" },
    );
    expect(taken).toEqual({ outcome: "taken" });
    // A RESERVED slug answers the same typed refusal, indistinguishably.
    const reserved = await identity.approveLoginFlow(
      flow.userCode,
      { userId: "u_out", display: "Outsider" },
      { kind: "create", displayName: "Login", slug: "login" },
    );
    expect(reserved).toEqual({ outcome: "taken" });
    // Nothing committed either time: the flow is still pending for the retry.
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("pending");
    expect(
      (
        await identity.approveLoginFlow(
          flow.userCode,
          { userId: "u_out", display: "Outsider" },
          { kind: "create", displayName: "Squat Team", slug: "squat-team" },
        )
      )?.outcome,
    ).toBe("approved");
  });

  it("an INVITATION choice accepts inside the fence — addressee-fenced, by id", async () => {
    const identity = await import("@/lib/db/identity.server");
    await seedUser(db, "u_invited", "Invited", "invited@example.com");
    await db.q(`UPDATE web."user" SET email_verified = true WHERE id = 'u_invited'`);
    await db.q(
      `INSERT INTO web.invitation (id, workspace_id, email, role, status)
       VALUES ('inv_choice', $1, 'invited@example.com', 'member', 'pending')`,
      [wsAcme],
    );
    // A DIFFERENT signed-in account picking the invitation is refused — the accept fences.
    const foreign = await identity.startLoginFlow("foreign-box", null);
    expect(
      await identity.approveLoginFlow(
        foreign.userCode,
        { userId: "u_out", display: "Outsider" },
        { kind: "invitation", id: "inv_choice" },
      ),
    ).toBeNull();
    // The addressee's pick accepts + seats + records the choice; the poll mints.
    const flow = await identity.startLoginFlow("invited-box", null);
    const approved = await identity.approveLoginFlow(
      flow.userCode,
      { userId: "u_invited", display: "Invited" },
      { kind: "invitation", id: "inv_choice" },
    );
    expect(approved?.outcome).toBe("approved");
    expect(approved?.outcome === "approved" && approved.workspaceName).toBe("acme");
    expect(
      await db.q(`SELECT 1 FROM web.seat WHERE workspace_id = $1 AND user_id = 'u_invited'`, [
        wsAcme,
      ]),
    ).toHaveLength(1);
    expect(
      await db.q(`SELECT 1 FROM web.invitation WHERE id = 'inv_choice' AND status = 'accepted'`),
    ).toHaveLength(1);
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("granted");
  });

  it("a flow-carried invite token PRE-BINDS: the posted choice is ignored", async () => {
    // The cross-workspace consistency property: a crafted flow cannot aim an invitation at A
    // while the posted pick names B — while the token binds, the token decides.
    const identity = await import("@/lib/db/identity.server");
    await seedWorkspace("w_beta", "beta-team");
    await seatUser(db, "w_beta", "u_in", "member");
    await seedUser(db, "u_woven", "Woven", "woven@example.com");
    await db.q(`UPDATE web."user" SET email_verified = true WHERE id = 'u_woven'`);
    const token = "woven-token-plaintext";
    await db.q(
      `INSERT INTO web.invitation (id, workspace_id, email, role, status, token_sha256)
       VALUES ('inv_woven', $1, 'woven@example.com', 'member', 'pending',
               sha256(convert_to($2, 'UTF8')))`,
      [wsAcme, token],
    );
    const flow = await identity.startLoginFlow("woven-box", null, token);
    const approved = await identity.approveLoginFlow(
      flow.userCode,
      { userId: "u_woven", display: "Woven" },
      // A pick naming a DIFFERENT workspace — ignored, the live token pre-binds to acme.
      { kind: "seat", workspace: "beta-team" },
    );
    expect(approved?.outcome).toBe("approved");
    expect(approved?.outcome === "approved" && approved.workspaceName).toBe("acme");
    const granted = await identity.pollLoginFlow(flow.flowCode);
    expect(granted.status === "granted" && granted.approvedWorkspaceId).toBe(wsAcme);
    const audits = await db.q<{ details: { via?: string } }>(
      `SELECT details FROM web.audit_event
       WHERE kind = 'login_approved' AND subject = 'woven-box'`,
    );
    expect(audits[0]?.details?.via).toBe("invite-token");
  });

  it("a DEAD flow-carried token falls through to the posted choice", async () => {
    // The chooser's honest-line fallback: the token bound nothing, so the approver's own
    // standing (their seat) completes the login like any ordinary flow.
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("stale-invite-box", null, "token-nobody-stored");
    const approved = await identity.approveLoginFlow(
      flow.userCode,
      { userId: "u_in", display: "Insider" },
      { kind: "seat", workspace: "acme" },
    );
    expect(approved?.outcome).toBe("approved");
    expect(approved?.outcome === "approved" && approved.workspaceName).toBe("acme");
    // The unaccepted token decorates NO hint into a workspace it never named.
    const granted = await identity.pollLoginFlow(flow.flowCode);
    expect(granted.status === "granted" && granted.hint).toBeNull();
  });
});

describe("the granted poll's workspace decoration", () => {
  it("names the CHOSEN workspace, not the first workspace row", async () => {
    // Several workspaces exist (acme was seeded first — the arbitrary LIMIT-1 row); the flow is
    // approved into beta-team, and the token route's decoration must name it: the CLI records
    // what it logged into from this one field.
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("beta-box", "beta-team");
    const approved = await identity.approveLoginFlow(
      flow.userCode,
      { userId: "u_in", display: "Insider" },
      { kind: "seat", workspace: "beta-team" },
    );
    expect(approved?.outcome).toBe("approved");

    const res = await tokenPoll(flow.flowCode);
    expect(res.status).toBe(200);
    const body = (await res.json()) as {
      status: string;
      credential: string;
      session_id: string;
      session_status: string;
      workspace: { workspace_id: string; name: string; display_name: string };
    };
    expect(body.status).toBe("granted");
    expect(body.credential).toBe(flow.flowCode);
    expect(body.session_status).toBe("active");
    expect(body.workspace).toEqual({
      workspace_id: "w_beta",
      name: "beta-team",
      display_name: "beta-team",
    });
    const sessions = await db.q(`SELECT 1 FROM web.cli_session WHERE id = $1`, [body.session_id]);
    expect(sessions).toHaveLength(1);
  });

  it("reads the approval-persisted ID — a rename or delete+recreate never re-points a grant", async () => {
    // The approval chose and PERSISTED the workspace id inside its fence; the poll's decoration
    // must follow that id (the current name comes along), not re-resolve the slug — a recreate
    // under the old slug is a row the approval never covered.
    const identity = await import("@/lib/db/identity.server");
    await seedWorkspace("w_gamma", "gamma-team");
    await seatUser(db, "w_gamma", "u_in", "member");
    const flow = await identity.startLoginFlow("gamma-box", "gamma-team");
    const approved = await identity.approveLoginFlow(
      flow.userCode,
      { userId: "u_in", display: "Insider" },
      { kind: "seat", workspace: "gamma-team" },
    );
    expect(approved?.outcome).toBe("approved");
    // Exchange first, then rename the workspace and squat the OLD slug with a new one.
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("granted");
    await db.q(`UPDATE web.workspace SET name = 'gamma-renamed' WHERE id = 'w_gamma'`);
    await seedWorkspace("w_squat", "gamma-team");

    const res = await tokenPoll(flow.flowCode);
    expect(res.status).toBe(200);
    const body = (await res.json()) as { workspace: { workspace_id: string; name: string } };
    // The APPROVED workspace, under its current name — never the squatter.
    expect(body.workspace.workspace_id).toBe("w_gamma");
    expect(body.workspace.name).toBe("gamma-renamed");
  });
});
