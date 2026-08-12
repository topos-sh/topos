import { execFileSync } from "node:child_process";
import { join, resolve } from "node:path";
import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { SessionActor } from "@/lib/auth/guards.server";
import { applyPlaneDdl } from "../helpers/plane-ddl";
import { installTestEnv } from "./helpers/test-env";

/**
 * The identity model's concurrency-critical ceremonies + the FEED predicate, against a REAL
 * scratch Postgres (the drizzle migration + the plane custody DDL applied verbatim): the
 * claim-consume race, the login flow's one-shot answers, session revocation, the
 * session-approval knob, the last-owner fence, seat removal ending sessions + feed rows, the
 * assignment/decline delivery matrix (self-service and curator-side), and the delivery wire
 * shape. Actors are minted by CAST — the one thing production code must never do (the brand is
 * module-private to guards.server.ts).
 */
const ADMIN_URL =
  process.env.TEST_DATABASE_URL ?? "postgresql://postgres:identity2@localhost:5443/postgres";
const SCRATCH = `identity_core_${Date.now()}_${Math.floor(Math.random() * 10000)}`;

function scratchUrl(): string {
  const url = new URL(ADMIN_URL);
  url.pathname = `/${SCRATCH}`;
  return url.toString();
}

async function adminQuery(sql: string): Promise<void> {
  const client = new Client({ connectionString: ADMIN_URL });
  await client.connect();
  try {
    await client.query(sql);
  } finally {
    await client.end();
  }
}

async function q<Row extends Record<string, unknown> = Record<string, unknown>>(
  sql: string,
  params: unknown[] = [],
): Promise<Row[]> {
  const { getPool } = await import("@/lib/db/index.server");
  const result = await getPool().query(sql, params);
  return result.rows as Row[];
}

let wsId = "";

async function seedUser(id: string, name: string, email: string): Promise<void> {
  await q(`INSERT INTO web."user" (id, name, email) VALUES ($1, $2, $3)`, [id, name, email]);
}

async function seatUser(userId: string, role: string): Promise<void> {
  await q(`INSERT INTO web.seat (workspace_id, user_id, role) VALUES ($1, $2, $3)`, [
    wsId,
    userId,
    role,
  ]);
}

async function seedBundle(id: string, name: string): Promise<string> {
  await q(`INSERT INTO web.bundle (id, workspace_id, name) VALUES ($1, $2, $3)`, [id, wsId, name]);
  const vid = `${id.replaceAll("_", "")}0`.padEnd(64, "a").slice(0, 64);
  await q(
    `INSERT INTO plane.version (workspace_id, bundle_id, version_id, commit_id, author_display)
     VALUES ($1, $2, $3, $3, 'seed')`,
    [wsId, id, vid],
  );
  await q(
    `INSERT INTO plane.current_pointer (workspace_id, bundle_id, version_id, moved_by_display)
     VALUES ($1, $2, $3, 'seed')`,
    [wsId, id, vid],
  );
  await q(
    `INSERT INTO plane.version_digest (workspace_id, bundle_id, version_id, bundle_digest)
     VALUES ($1, $2, $3, $4)`,
    [wsId, id, vid, "d".repeat(64)],
  );
  return vid;
}

async function placeInEveryone(bundleId: string): Promise<void> {
  await q(
    `INSERT INTO web.channel_bundle (channel_id, workspace_id, bundle_id)
     SELECT id, workspace_id, $1 FROM web.channel WHERE is_default AND workspace_id = $2`,
    [bundleId, wsId],
  );
}

function sessionActorFor(
  userId: string,
  sessionId: string,
  role: SessionActor["role"],
): SessionActor {
  return {
    userId,
    display: userId,
    workspaceId: wsId,
    sessionId,
    role,
    sessionStatus: "active",
  } as SessionActor;
}

beforeAll(async () => {
  await adminQuery(`CREATE DATABASE ${SCRATCH}`);
  installTestEnv({ DATABASE_URL: scratchUrl(), TOPOS_SETUP_CODE: "identity-core-setup-code" });
  await applyPlaneDdl(scratchUrl());
  const WEB_ROOT = resolve(__dirname, "..", "..");
  execFileSync("node", [join(WEB_ROOT, "scripts", "migrate.mjs")], {
    env: { ...process.env, DATABASE_URL: scratchUrl() },
    stdio: "pipe",
  });
  const identity = await import("@/lib/db/identity.server");
  await identity.ensureSetup("http://localhost:3000");
  wsId = (await identity.theWorkspace())?.id ?? "";
}, 60000);

afterAll(async () => {
  const { getPool } = await import("@/lib/db/index.server");
  await getPool().end();
  await adminQuery(`DROP DATABASE IF EXISTS ${SCRATCH} WITH (FORCE)`);
});

describe("the claim consume", () => {
  it("two concurrent consumes: exactly one wins; the loser is the uniform miss", async () => {
    const identity = await import("@/lib/db/identity.server");
    await seedUser("u_claim_a", "Claimer A", "claim-a@example.com");
    await seedUser("u_claim_b", "Claimer B", "claim-b@example.com");
    const [a, b] = await Promise.all([
      identity.consumeClaim("identity-core-setup-code", "u_claim_a", "Claimer A"),
      identity.consumeClaim("identity-core-setup-code", "u_claim_b", "Claimer B"),
    ]);
    const winners = [a, b].filter((r) => r !== null);
    expect(winners).toHaveLength(1);
    const seats = await q(`SELECT user_id, role FROM web.seat WHERE workspace_id = $1`, [wsId]);
    expect(seats).toHaveLength(1);
    expect(seats[0]?.role).toBe("owner");
    // Claimed: the probe goes dark.
    expect(await identity.claimableWorkspace("identity-core-setup-code")).toBeNull();
  });
});

/** The single-tenant seat pick — the slug is ignored there (the install IS its workspace). */
const SEAT = { kind: "seat" as const, workspace: "" };

describe("the login flow", () => {
  it("start records consent nothing; approval records the choice; THE POLL mints", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const flow = await identity.startLoginFlow("laptop", null);
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("pending");
    expect(await identity.pendingLoginFlow(flow.userCode, owner)).toEqual({
      requestedName: "laptop",
      userCode: flow.userCode,
      // The card carries its binding: only a loopback flow may pre-arm from the URL challenge.
      binding: "device",
      preselect: null,
      invite: null,
    });
    const approved = await identity.approveLoginFlow(
      flow.userCode,
      { userId: owner, display: "Owner" },
      SEAT,
    );
    expect(approved?.outcome).toBe("approved");
    expect(approved?.outcome === "approved" && approved.requestedName).toBe("laptop");

    // NO SESSION EXISTS YET — consent is recorded and nothing more. The bearer credential IS
    // the flow code, so a mint at approval would hand the flow's starter a live credential the
    // instant a human clicked; the exchange (the poll) is where it comes into existence.
    expect(await identity.sessionActor(wsId, flow.flowCode)).toBeNull();
    const flowRow = await q(
      `SELECT session_id, approved_workspace_id FROM web.login_flow
                             WHERE user_code = $1`,
      [flow.userCode],
    );
    expect(flowRow[0]?.session_id).toBeNull();
    expect(flowRow[0]?.approved_workspace_id).toBe(wsId);

    // THE EXCHANGE: the first poll mints — an owner's login is born active whatever the knob.
    const granted = await identity.pollLoginFlow(flow.flowCode);
    expect(granted.status).toBe("granted");
    expect(granted.status === "granted" && granted.sessionStatus).toBe("active");
    // The grant REPEATS: the CLI's crash-recovery is to re-poll, so a client that received the
    // grant but crashed before persisting its credential must get the same grant again — the
    // repeat poll finds session_id set and re-reads instead of minting a second.
    const reAfterCrash = await identity.pollLoginFlow(flow.flowCode);
    expect(reAfterCrash.status).toBe("granted");
    expect(reAfterCrash.status === "granted" && reAfterCrash.sessionId).toBe(
      granted.status === "granted" ? granted.sessionId : "",
    );
    // The credential (the promoted flow code) resolves; a bogus one does not.
    expect(await identity.sessionActor(wsId, flow.flowCode)).not.toBeNull();
    expect(await identity.sessionActor(wsId, "not-a-credential")).toBeNull();
    // The credential is WORKSPACE-SCOPED: another workspace id resolves nothing.
    expect(await identity.sessionActor("w_other", flow.flowCode)).toBeNull();
  });

  it("a LOOPBACK flow completes identically: approval anywhere, the poll mints", async () => {
    // The retired auth-code exchange is GONE: the binding decides only whether the /verify card
    // may pre-arm from the URL challenge. Approval from any browser records consent, and the
    // machine holding the flow code collects on its next poll — no third status exists between
    // pending and granted.
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const flow = await identity.startLoginFlow("laptop-loop", null, undefined, "loopback");
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("pending");
    const approved = await identity.approveLoginFlow(
      flow.userCode,
      { userId: owner, display: "Owner" },
      SEAT,
    );
    expect(approved?.outcome).toBe("approved");
    // A loopback approval carries the flow's OWN challenge (hex of its code hash — non-secret),
    // so the page wakes the listener for exactly this flow and no other card.
    const { createHash } = await import("node:crypto");
    expect(approved?.outcome === "approved" && approved.flowChallenge).toBe(
      createHash("sha256").update(flow.flowCode, "utf8").digest("hex"),
    );
    expect(await identity.sessionActor(wsId, flow.flowCode)).toBeNull();
    const granted = await identity.pollLoginFlow(flow.flowCode);
    expect(granted.status).toBe("granted");
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("granted");
    expect(await identity.sessionActor(wsId, flow.flowCode)).not.toBeNull();
  });

  it("an approved flow never polled mints nothing: past the TTL it answers expired", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const sessionsBefore = (await q(`SELECT count(*)::int AS n FROM web.cli_session`))[0]
      ?.n as number;
    const flow = await identity.startLoginFlow("abandoned-box", null);
    await identity.approveLoginFlow(flow.userCode, { userId: owner, display: "Owner" }, SEAT);
    await q(`UPDATE web.login_flow SET expires_at = now() - interval '1 minute'`);
    // Consent that was never collected lapses with the flow — nothing was minted, nothing will
    // be, and the poll says so terminally instead of minting a credential nobody is waiting on.
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("expired");
    expect(await identity.sessionActor(wsId, flow.flowCode)).toBeNull();
    const after = await q(`SELECT count(*)::int AS n FROM web.cli_session`);
    expect(after[0]?.n).toBe(sessionsBefore);
  });

  it("the exchange re-reads standing: a seat removed between consent and collection mints nothing", async () => {
    const identity = await import("@/lib/db/identity.server");
    await seedUser("u_gone", "Goner", "goner@example.com");
    await seatUser("u_gone", "member");
    const flow = await identity.startLoginFlow("gone-box", null);
    expect(
      (await identity.approveLoginFlow(flow.userCode, { userId: "u_gone", display: "G" }, SEAT))
        ?.outcome,
    ).toBe("approved");
    await q(`DELETE FROM web.seat WHERE user_id = 'u_gone'`);
    // Revocation is a row delete, effective immediately — the collection lands on the side the
    // rows say NOW, so there is nothing to mint and the honest answer is terminal.
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("expired");
  });

  it("the exchange re-reads the knob: a flip between consent and collection births pending", async () => {
    const identity = await import("@/lib/db/identity.server");
    await seedUser("u_flip", "Flipped", "flip@example.com");
    await seatUser("u_flip", "member");
    const flow = await identity.startLoginFlow("flip-box", null);
    await identity.approveLoginFlow(flow.userCode, { userId: "u_flip", display: "F" }, SEAT);
    await q(`UPDATE web.workspace SET session_approval = 'on' WHERE id = $1`, [wsId]);
    const granted = await identity.pollLoginFlow(flow.flowCode);
    expect(granted.status === "granted" && granted.sessionStatus).toBe("pending");
    await q(`UPDATE web.workspace SET session_approval = 'off' WHERE id = $1`, [wsId]);
    await q(`DELETE FROM web.seat WHERE user_id = 'u_flip'`);
  });

  it("a seat delete RACING the exchange serializes on the seat lock — expired, never a fault", async () => {
    // The exchange locks the seat FOR UPDATE before minting. Without the lock, a delete
    // committing between the seat read and the cli_session insert would fail the composite FK
    // — a 500 to the poller. With it, the exchange blocks on the rival's lock and, once the
    // delete commits, re-reads no seat: the honest terminal answer.
    const identity = await import("@/lib/db/identity.server");
    await seedUser("u_race", "Racer", "race@example.com");
    await seatUser("u_race", "member");
    const flow = await identity.startLoginFlow("race-box", null);
    await identity.approveLoginFlow(flow.userCode, { userId: "u_race", display: "R" }, SEAT);
    const rival = new Client({ connectionString: scratchUrl() });
    await rival.connect();
    try {
      await rival.query("BEGIN");
      await rival.query(`SELECT 1 FROM web.seat WHERE user_id = 'u_race' FOR UPDATE`);
      const poll = identity.pollLoginFlow(flow.flowCode);
      // Let the exchange reach the seat lock and block there before the rival deletes.
      await new Promise((resolve) => setTimeout(resolve, 150));
      await rival.query(`DELETE FROM web.seat WHERE user_id = 'u_race'`);
      await rival.query("COMMIT");
      expect((await poll).status).toBe("expired");
    } finally {
      await rival.end();
    }
  });

  it("the challenge lookup resolves a LOOPBACK flow and refuses a DEVICE one", async () => {
    // The /verify pre-arm gate, in SQL. The challenge is derivable by whoever started the flow,
    // so only the binding can make pre-resolution safe.
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const { createHash } = await import("node:crypto");
    const hex = (code: string) => createHash("sha256").update(code, "utf8").digest("hex");
    const device = await identity.startLoginFlow("box", null);
    const loop = await identity.startLoginFlow("laptop", null, undefined, "loopback");
    expect(await identity.pendingLoginFlowByChallenge(hex(device.flowCode), owner)).toBeNull();
    expect(await identity.pendingLoginFlowByChallenge(hex(loop.flowCode), owner)).not.toBeNull();
  });

  it("the preselect rides the card, display-only", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const flow = await identity.startLoginFlow("hinted-box", "acme");
    expect((await identity.pendingLoginFlow(flow.userCode, owner))?.preselect).toBe("acme");
  });

  it("deny repeats until the sweep; a re-approve of the same code misses", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const flow = await identity.startLoginFlow("stolen-box", null);
    expect(
      await identity.denyLoginFlow(flow.userCode, { userId: owner, display: "O" }),
    ).not.toBeNull();
    expect(
      await identity.approveLoginFlow(flow.userCode, { userId: owner, display: "O" }, SEAT),
    ).toBeNull();
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("denied");
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("denied");
  });

  it("deny takes NO seat — the flow is workspace-less; the audit row is server-scoped", async () => {
    const identity = await import("@/lib/db/identity.server");
    await seedUser("u_denier", "Denier", "denier@example.com");
    const flow = await identity.startLoginFlow("killed-box", null);
    // Any signed-in code-holder can kill a request to act as them — no seat exists to require.
    // A device-bound flow answers with NO challenge: there is no listener to wake.
    expect(
      await identity.denyLoginFlow(flow.userCode, { userId: "u_denier", display: "D" }),
    ).toEqual({ flowChallenge: null });
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("denied");
    const audits = await q(
      `SELECT workspace_id FROM web.audit_event
       WHERE kind = 'login_denied' AND subject = 'killed-box'`,
    );
    expect(audits).toEqual([{ workspace_id: null }]);
  });

  it("a SEATLESS approver's seat pick is refused; a seated one then completes the flow", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    await seedUser("u_seatless", "Seatless", "seatless@example.com");
    const flow = await identity.startLoginFlow("drifter-box", null);
    // The chosen standing is validated INSIDE the fence — a seatless person's pick lands the
    // same uniform refusal an expired code gets, and consumes nothing.
    expect(
      await identity.approveLoginFlow(flow.userCode, { userId: "u_seatless", display: "S" }, SEAT),
    ).toBeNull();
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("pending");
    expect(
      await identity.approveLoginFlow(flow.userCode, { userId: owner, display: "O" }, SEAT),
    ).not.toBeNull();
  });

  it("an expired pending flow reports expired; the sweep reaps past-TTL rows", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const flow = await identity.startLoginFlow("slow-machine", null);
    await q(`UPDATE web.login_flow SET expires_at = now() - interval '1 minute'`);
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("expired");
    // The row lingers until a sweep (read does not delete); the sweep then reaps it.
    expect(await identity.pendingLoginFlow(flow.userCode, owner)).toBeNull();
    expect(await identity.sweepExpiredLoginFlows()).toBeGreaterThanOrEqual(1);
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("expired");
  });
});

describe("session revocation", () => {
  it("ending a session kills the credential; a granted poll then reads expired", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const flow = await identity.startLoginFlow("short-lived", null);
    await identity.approveLoginFlow(flow.userCode, { userId: owner, display: "O" }, SEAT);
    const granted = await identity.pollLoginFlow(flow.flowCode);
    expect(granted.status).toBe("granted");
    const sessionId = granted.status === "granted" ? granted.sessionId : "";
    expect(await identity.revokeOwnSession({ userId: owner, display: "O" }, sessionId)).toBe(
      "revoked",
    );
    // The credential dies with the row …
    expect(await identity.sessionActor(wsId, flow.flowCode)).toBeNull();
    // … the row is DELETED, never tombstoned …
    expect(await q(`SELECT 1 FROM web.cli_session WHERE id = $1`, [sessionId])).toHaveLength(0);
    // … and the flow's grant honestly reads expired (start over).
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("expired");
    // A repeat revoke finds nothing (self-only WHERE answers unknown).
    expect(await identity.revokeOwnSession({ userId: owner, display: "O" }, sessionId)).toBe(
      "unknown_session",
    );
  });

  it("the CLI logout revokes by the presented credential; a retry misses", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const flow = await identity.startLoginFlow("logout-box", null);
    await identity.approveLoginFlow(flow.userCode, { userId: owner, display: "O" }, SEAT);
    // The poll is what mints — before it, there is no session for the credential to name.
    expect(await identity.revokeSessionByCredential(flow.flowCode)).toBe(false);
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("granted");
    expect(await identity.revokeSessionByCredential(flow.flowCode)).toBe(true);
    expect(await identity.revokeSessionByCredential(flow.flowCode)).toBe(false);
  });

  it("the owner-set expiry applies to EVERY live-session surface: guard, logout, approve", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    // An active session and a pending one, both minted now (the poll is the mint).
    const active = await identity.startLoginFlow("expiry-box", null);
    await identity.approveLoginFlow(active.userCode, { userId: owner, display: "O" }, SEAT);
    expect((await identity.pollLoginFlow(active.flowCode)).status).toBe("granted");
    await seedUser("u_expiry", "Expiring", "expiry@example.com");
    await seatUser("u_expiry", "member");
    await q(`UPDATE web.workspace SET session_approval = 'on' WHERE id = $1`, [wsId]);
    const pend = await identity.startLoginFlow("expiry-pending-box", null);
    await identity.approveLoginFlow(pend.userCode, { userId: "u_expiry", display: "E" }, SEAT);
    expect((await identity.pollLoginFlow(pend.flowCode)).status).toBe("granted");
    await q(`UPDATE web.workspace SET session_approval = 'off' WHERE id = $1`, [wsId]);
    const pendingId = (
      await q(`SELECT id FROM web.cli_session WHERE status = 'pending' ORDER BY created_at DESC`)
    )[0]?.id as string;

    // Arm a 1-hour expiry and age both sessions past it.
    await q(`UPDATE web.workspace SET session_max_age_ms = 3600000 WHERE id = $1`, [wsId]);
    await q(
      `UPDATE web.cli_session SET created_at = now() - interval '2 hours'
       WHERE credential_sha256 IN (sha256(convert_to($1, 'UTF8')), sha256(convert_to($2, 'UTF8')))`,
      [active.flowCode, pend.flowCode],
    );
    // The guard refuses; the self-logout answers exactly what an unknown bearer gets (no
    // liveness oracle); an owner cannot approve a pending row whose credential is already dead.
    expect(await identity.sessionActor(wsId, active.flowCode)).toBeNull();
    expect(await identity.revokeSessionByCredential(active.flowCode)).toBe(false);
    expect(await identity.approveSession({ userId: owner, display: "O" }, wsId, pendingId)).toBe(
      "unknown_session",
    );
    // Clearing the policy brings the surfaces back in the same breath.
    await q(`UPDATE web.workspace SET session_max_age_ms = NULL WHERE id = $1`, [wsId]);
    expect(await identity.sessionActor(wsId, active.flowCode)).not.toBeNull();
    expect(await identity.approveSession({ userId: owner, display: "O" }, wsId, pendingId)).toBe(
      "approved",
    );
    expect(await identity.revokeSessionByCredential(active.flowCode)).toBe(true);
  });
});

describe("the session-approval knob", () => {
  it("knob on: a member's session is born pending, delivers empty, and approve activates it", async () => {
    const identity = await import("@/lib/db/identity.server");
    const lane = await import("@/lib/db/queries.lane.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    await seedUser("u_knob", "Knobbed", "knob@example.com");
    await seatUser("u_knob", "member");
    await q(`UPDATE web.workspace SET session_approval = 'on' WHERE id = $1`, [wsId]);
    const flow = await identity.startLoginFlow("held-box", null);
    await identity.approveLoginFlow(flow.userCode, { userId: "u_knob", display: "K" }, SEAT);
    const minted = await identity.pollLoginFlow(flow.flowCode);
    expect(minted.status === "granted" && minted.sessionStatus).toBe("pending");
    const sessionId = minted.status === "granted" ? minted.sessionId : "";
    // A pending session resolves only via allowPending; delivery is the shape-complete EMPTY.
    const row = await identity.sessionActor(wsId, flow.flowCode);
    expect(row?.sessionStatus).toBe("pending");
    const pendingActor = {
      userId: "u_knob",
      display: "K",
      workspaceId: wsId,
      sessionId,
      role: "member",
      sessionStatus: "pending",
    } as SessionActor;
    const empty = await lane.emptyDeliveryFor(pendingActor);
    expect(empty.session_status).toBe("pending");
    expect(empty.skills).toEqual([]);
    // Shape-complete: the declined list is present (and empty) even over a pending session.
    expect(empty.declined).toEqual([]);
    // An owner approves on the sessions page; the session then resolves active.
    expect(await identity.approveSession({ userId: owner, display: "O" }, wsId, sessionId)).toBe(
      "approved",
    );
    expect((await identity.sessionActor(wsId, flow.flowCode))?.sessionStatus).toBe("active");
    // An owner's OWN login stays born active under the knob.
    const ownerFlow = await identity.startLoginFlow("owner-box", null);
    await identity.approveLoginFlow(ownerFlow.userCode, { userId: owner, display: "O" }, SEAT);
    const ownerMinted = await identity.pollLoginFlow(ownerFlow.flowCode);
    expect(ownerMinted.status === "granted" && ownerMinted.sessionStatus).toBe("active");
    await q(`UPDATE web.workspace SET session_approval = 'off' WHERE id = $1`, [wsId]);
  });

  it("reject deletes the pending session; owner remove ends an active one", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    await q(`UPDATE web.workspace SET session_approval = 'on' WHERE id = $1`, [wsId]);
    const flow = await identity.startLoginFlow("rejected-box", null);
    await identity.approveLoginFlow(flow.userCode, { userId: "u_knob", display: "K" }, SEAT);
    const minted = await identity.pollLoginFlow(flow.flowCode);
    expect(
      await identity.rejectSession(
        { userId: owner, display: "O" },
        wsId,
        minted.status === "granted" ? minted.sessionId : "",
      ),
    ).toBe("rejected");
    expect(await identity.sessionActor(wsId, flow.flowCode)).toBeNull();
    await q(`UPDATE web.workspace SET session_approval = 'off' WHERE id = $1`, [wsId]);

    const flow2 = await identity.startLoginFlow("removed-box", null);
    await identity.approveLoginFlow(flow2.userCode, { userId: "u_knob", display: "K" }, SEAT);
    const minted2 = await identity.pollLoginFlow(flow2.flowCode);
    expect(minted2.status === "granted" && minted2.sessionStatus).toBe("active");
    expect(
      await identity.ownerRemoveSession(
        { userId: owner, display: "O" },
        wsId,
        minted2.status === "granted" ? minted2.sessionId : "",
      ),
    ).toBe("removed");
    expect(await identity.sessionActor(wsId, flow2.flowCode)).toBeNull();
  });
});

describe("the session expiry policy", () => {
  it("a session past the workspace max age refuses at the guard", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const flow = await identity.startLoginFlow("aging-box", null);
    await identity.approveLoginFlow(flow.userCode, { userId: owner, display: "O" }, SEAT);
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("granted");
    expect(await identity.sessionActor(wsId, flow.flowCode)).not.toBeNull();
    // The owner sets a max age; a session older than it stops resolving.
    await q(`UPDATE web.workspace SET session_max_age_ms = 3600000 WHERE id = $1`, [wsId]);
    await q(
      `UPDATE web.cli_session SET created_at = now() - interval '2 hours'
       WHERE credential_sha256 = sha256(convert_to($1, 'UTF8'))`,
      [flow.flowCode],
    );
    expect(await identity.sessionActor(wsId, flow.flowCode)).toBeNull();
    await q(`UPDATE web.workspace SET session_max_age_ms = NULL WHERE id = $1`, [wsId]);
    expect(await identity.sessionActor(wsId, flow.flowCode)).not.toBeNull();
    await identity.revokeSessionByCredential(flow.flowCode);
  });
});

describe("the last-owner fence", () => {
  it("demoting the sole owner is refused; a second owner unlocks it", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const acting = { userId: owner, display: "Owner" };
    expect(await identity.setSeatRole(acting, wsId, owner, "member")).toBe("last_owner");
    expect(await identity.removeSeat(acting, wsId, owner)).toBe("last_owner");

    await seedUser("u_second", "Second Owner", "second@example.com");
    await seatUser("u_second", "owner");
    expect(await identity.setSeatRole(acting, wsId, "u_second", "reviewer")).toBe("ok");
    // Back to two owners, then the original demotes cleanly.
    expect(await identity.setSeatRole(acting, wsId, "u_second", "owner")).toBe("ok");
    expect(
      await identity.setSeatRole({ userId: "u_second", display: "S" }, wsId, owner, "member"),
    ).toBe("ok");
    // Restore for later suites.
    expect(
      await identity.setSeatRole({ userId: "u_second", display: "S" }, wsId, owner, "owner"),
    ).toBe("ok");
    expect(await identity.setSeatRole(acting, wsId, "unknown-user", "member")).toBe("missing");
  });
});

describe("the feed (assignments − declines) + delivery", () => {
  it("delivers the union of what is assigned to the person and to everyone, minus declines", async () => {
    const identity = await import("@/lib/db/identity.server");
    const feed = await import("@/lib/db/queries.feed.server");
    const lane = await import("@/lib/db/queries.lane.server");
    await seedUser("u_ent", "Entitled", "entitled@example.com");
    await seatUser("u_ent", "member");
    const flow = await identity.startLoginFlow("ent-box", null);
    await identity.approveLoginFlow(flow.userCode, { userId: "u_ent", display: "E" }, SEAT);
    const granted = await identity.pollLoginFlow(flow.flowCode);
    const sessionId = granted.status === "granted" ? granted.sessionId : "";
    const actor = sessionActorFor("u_ent", sessionId, "member");

    await seedBundle("s_everyone", "via-everyone");
    await placeInEveryone("s_everyone");
    await seedBundle("s_named", "via-named-channel");
    await q(
      `INSERT INTO web.channel (id, workspace_id, name) VALUES ('c_named', $1, 'named-channel')`,
      [wsId],
    );
    await q(
      `INSERT INTO web.channel_bundle (channel_id, workspace_id, bundle_id) VALUES ('c_named', $1, 's_named')`,
      [wsId],
    );
    await seedBundle("s_picked", "picked-by-me");

    // The BASELINE is a row, not a rule: the default channel is assigned to everyone, so the
    // bundle it carries arrives with no per-person row at all.
    let delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).toEqual(["s_everyone"]);
    expect(delivery.skills[0]?.via).toEqual({ channels: ["everyone"], direct: false });

    // Carrying a channel adds its members; adding a bundle to your own feed adds the third.
    expect(await feed.assignChannelToSelf(actor, "c_named")).toBe("assigned");
    expect(await feed.addToMine(actor, "s_picked")).toBe("added");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id).sort()).toEqual([
      "s_everyone",
      "s_named",
      "s_picked",
    ]);

    // A DECLINE is the one negative row, and it beats every source that assigns the bundle.
    expect(await feed.declineBundle(actor, "s_named")).toBe("declined");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id).sort()).toEqual(["s_everyone", "s_picked"]);

    // UNPICKING takes back only the person's own assignment — nothing broader is touched.
    expect(await feed.unpickBundle(actor, "s_picked")).toBe("unpicked");
    expect(await feed.unpickBundle(actor, "s_picked")).toBe("not_picked");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).toEqual(["s_everyone"]);

    // The BASELINE cannot be dropped by one person: it is assigned to everyone, so the
    // per-bundle decline is the only window onto it.
    const everyoneChannel = (
      await q<{ id: string }>(`SELECT id FROM web.channel WHERE is_default AND workspace_id = $1`, [
        wsId,
      ])
    )[0]?.id as string;
    expect(await feed.unassignChannelFromSelf(actor, everyoneChannel)).toBe("baseline");
    expect(await feed.declineBundle(actor, "s_everyone")).toBe("declined");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).toEqual([]);

    // Clearing a decline lets the thing flow again from whatever still assigns it …
    expect(await feed.undeclineBundle(actor, "s_everyone")).toBe("cleared");
    // … and "add to mine" on a DECLINED bundle clears the decline in the same act.
    expect(await feed.addToMine(actor, "s_named")).toBe("added");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id).sort()).toEqual(["s_everyone", "s_named"]);
    const named = delivery.skills.find((s) => s.skill_id === "s_named");
    // Their own add is a self-pick: the direct assignment rides with the `picked` fact (and
    // no `assigned_by` — their own act attributes to nobody else).
    expect(named?.via).toEqual({ channels: ["named-channel"], direct: true, picked: true });

    // One row per provenance underneath, each labelled by audience and by who placed it: the
    // workspace baseline reaches everyone and nobody picked it, while the two the person chose
    // are theirs by name and carry the self flag.
    const rows = await q<{ kind: string; name: string; audience: string; self: boolean }>(
      `SELECT CASE WHEN a.bundle_id IS NULL THEN 'channel' ELSE 'skill' END AS kind,
              COALESCE(b.name, c.name) AS name,
              CASE WHEN a.user_id IS NULL THEN 'everyone' ELSE 'you' END AS audience,
              a.self
       FROM web.assignment a
       LEFT JOIN web.bundle b ON b.id = a.bundle_id
       LEFT JOIN web.channel c ON c.id = a.channel_id
       WHERE a.workspace_id = $1 AND (a.user_id = 'u_ent' OR a.user_id IS NULL)`,
      [wsId],
    );
    expect(rows.map((r) => `${r.kind}:${r.name}:${r.audience}:${r.self}`).sort()).toEqual([
      "channel:everyone:everyone:false",
      "channel:named-channel:you:true",
      "skill:via-named-channel:you:true",
    ]);
    expect(await q(`SELECT 1 FROM web.decline WHERE user_id = 'u_ent'`)).toHaveLength(0);
  });

  it("an archived bundle refuses the add typed; an unknown one is unknown_skill", async () => {
    const feed = await import("@/lib/db/queries.feed.server");
    const sessionRow = await q(`SELECT id FROM web.cli_session WHERE user_id = 'u_ent'`);
    const actor = sessionActorFor("u_ent", sessionRow[0]?.id as string, "member");
    await q(
      `INSERT INTO web.bundle (id, workspace_id, name, status, base_name, archived_at)
       VALUES ('s_arch', $1, 'old-2026-07-01', 'archived', 'old', now())`,
      [wsId],
    );
    expect(await feed.addToMine(actor, "s_arch")).toBe("skill_not_active");
    expect(await feed.addToMine(actor, "s_nope")).toBe("unknown_skill");
    expect(await feed.declineBundle(actor, "s_nope")).toBe("unknown_skill");
    expect(await feed.assignChannelToSelf(actor, "c_nope")).toBe("unknown_channel");
  });

  it("curator assignments aim at a person or at everyone, audited, and withdraw cleanly", async () => {
    const feed = await import("@/lib/db/queries.feed.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner' LIMIT 1`))[0]
      ?.user_id as string;
    const ownerActor = { userId: owner, display: "O", workspaceId: wsId, role: "owner" } as never;
    const sessionRow = await q(`SELECT id FROM web.cli_session WHERE user_id = 'u_ent'`);
    const actor = sessionActorFor("u_ent", sessionRow[0]?.id as string, "member");
    await seedBundle("s_curated", "curator-assigned");

    // Aimed at ONE person: it delivers to them, flagged direct, and nobody else holds a row.
    expect(await feed.assignBundle(ownerActor, "s_curated", { userId: "u_ent" })).toBe("assigned");
    const lane = await import("@/lib/db/queries.lane.server");
    let delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).toContain("s_curated");
    // The person did NOT place it, so it is not theirs to unpick — declining is their switch.
    expect(
      await q<{ self: boolean }>(
        `SELECT self FROM web.assignment WHERE bundle_id = 's_curated' AND user_id = 'u_ent'`,
      ),
    ).toEqual([{ self: false }]);
    expect(await feed.unpickBundle(actor, "s_curated")).toBe("not_picked");
    expect(await feed.declineBundle(actor, "s_curated")).toBe("declined");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).not.toContain("s_curated");
    await feed.undeclineBundle(actor, "s_curated");

    // Withdrawing the assignment ends the offer; the audit trail keeps both acts.
    expect(await feed.unassign(ownerActor, { bundleId: "s_curated" }, { userId: "u_ent" })).toBe(
      "unassigned",
    );
    expect(await feed.unassign(ownerActor, { bundleId: "s_curated" }, { userId: "u_ent" })).toBe(
      "not_assigned",
    );
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).not.toContain("s_curated");
    const audits = await q<{ kind: string }>(
      `SELECT kind FROM web.audit_event WHERE subject = 's_curated' ORDER BY id`,
    );
    expect(audits.map((a) => a.kind)).toEqual(["assigned", "unassigned"]);

    // Aimed at EVERYONE: one row, whole roster — and a stranger is refused typed.
    expect(await feed.assignBundle(ownerActor, "s_curated", { everyone: true })).toBe("assigned");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).toContain("s_curated");
    expect(await feed.assignBundle(ownerActor, "s_curated", { userId: "u_nobody" })).toBe(
      "unknown_member",
    );
    expect(await feed.unassign(ownerActor, { bundleId: "s_curated" }, { everyone: true })).toBe(
      "unassigned",
    );

    // The channel arms are the same act one set wider.
    expect(await feed.assignChannel(ownerActor, "c_named", { everyone: true })).toBe("assigned");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).toContain("s_named");
    expect(await feed.unassign(ownerActor, { channelId: "c_named" }, { everyone: true })).toBe(
      "unassigned",
    );
    expect(await feed.assignChannel(ownerActor, "c_nope", { everyone: true })).toBe(
      "unknown_channel",
    );
  });

  it("the delivery wire shape serves current, snake_case, with no pin field", async () => {
    const lane = await import("@/lib/db/queries.lane.server");
    const sessionRow = await q(`SELECT id FROM web.cli_session WHERE user_id = 'u_ent'`);
    const actor = sessionActorFor("u_ent", sessionRow[0]?.id as string, "member");
    const delivery = await lane.deliveryFor(actor);
    expect(delivery.schema_version).toBe(1);
    expect(delivery.workspace_id).toBe(wsId);
    expect(delivery.session_status).toBe("active");
    const skill = delivery.skills.find((s) => s.skill_id === "s_everyone");
    expect(skill).toMatchObject({
      skill_id: "s_everyone",
      name: "via-everyone",
      kind: "skill",
      protection: "open",
      bundle_digest: "d".repeat(64),
      generation: 1,
    });
    expect(typeof skill?.version_id).toBe("string");
    expect(typeof skill?.updated_at).toBe("number");
    // Channel-carried only: the optional attribution facts are OMITTED — the keys do not
    // exist, rather than riding as null/false (the wire's omit-when-absent rule).
    expect(Object.keys(skill?.via ?? {}).sort()).toEqual(["channels", "direct"]);
    expect(delivery.declined).toEqual([]);
    expect(Array.isArray(delivery.notices)).toBe(true);
    expect(typeof delivery.proposals_awaiting).toBe("number");
    expect(delivery.staleness_window_ms).toBe(604800000);
  });

  it("via attribution: assigned_by names the aimer, picked marks a self-pick, declined lists the stance", async () => {
    const feed = await import("@/lib/db/queries.feed.server");
    const lane = await import("@/lib/db/queries.lane.server");
    // TWO distinct owners, deterministically: the claim winner (display "Claimer A"/"B") aims
    // the everyone-row, the second owner ("Second Owner") the person-row — so the preference
    // assertion below really distinguishes the two creators.
    const owner = (
      await q(`SELECT user_id FROM web.seat WHERE role = 'owner' AND user_id <> 'u_second'`)
    )[0]?.user_id as string;
    const ownerActor = { userId: owner, display: "O", workspaceId: wsId, role: "owner" } as never;
    const secondActor = {
      userId: "u_second",
      display: "S",
      workspaceId: wsId,
      role: "owner",
    } as never;
    const sessionRow = await q(`SELECT id FROM web.cli_session WHERE user_id = 'u_ent'`);
    const actor = sessionActorFor("u_ent", sessionRow[0]?.id as string, "member");

    // PICKED: s_named carries the caller's own direct assignment (addToMine, earlier) — the
    // self-pick fact rides, and assigned_by does not (their own act attributes to nobody else).
    let delivery = await lane.deliveryFor(actor);
    const named = delivery.skills.find((s) => s.skill_id === "s_named");
    expect(named?.via.picked).toBe(true);
    expect("assigned_by" in (named?.via ?? {})).toBe(false);

    // ASSIGNED_BY: another member aims a bundle at the caller — the creator's display rides.
    await seedBundle("s_aimed", "curator-aimed");
    expect(await feed.assignBundle(ownerActor, "s_aimed", { everyone: true })).toBe("assigned");
    expect(await feed.assignBundle(secondActor, "s_aimed", { userId: "u_ent" })).toBe("assigned");
    delivery = await lane.deliveryFor(actor);
    const aimed = delivery.skills.find((s) => s.skill_id === "s_aimed");
    // The person-targeted row (Second Owner's) outranks the everyone one (the claim winner's);
    // the display follows the one rule — profile name, else email.
    expect(aimed?.via.assigned_by).toBe("Second Owner");
    expect("picked" in (aimed?.via ?? {})).toBe(false);
    expect(aimed?.via.direct).toBe(true);

    // DECLINED: the standing stance is served with delivery — identity + name, and the bundle
    // itself leaves the skills list.
    expect(await feed.declineBundle(actor, "s_aimed")).toBe("declined");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).not.toContain("s_aimed");
    expect(delivery.declined).toEqual([{ skill_id: "s_aimed", name: "curator-aimed" }]);

    // Clean the stance and the aim so the later suites see the state they expect.
    expect(await feed.undeclineBundle(actor, "s_aimed")).toBe("cleared");
    expect(await feed.unassign(secondActor, { bundleId: "s_aimed" }, { userId: "u_ent" })).toBe(
      "unassigned",
    );
    expect(await feed.unassign(ownerActor, { bundleId: "s_aimed" }, { everyone: true })).toBe(
      "unassigned",
    );
  });

  it("the applied report is a complete snapshot: absent bundles drop their rows", async () => {
    const lane = await import("@/lib/db/queries.lane.server");
    const sessionRow = await q(`SELECT id FROM web.cli_session WHERE user_id = 'u_ent'`);
    const sessionId = sessionRow[0]?.id as string;
    const actor = sessionActorFor("u_ent", sessionId, "member");
    const vid = "1".repeat(64);
    expect(
      await lane.reportApplied(actor, [
        { skillId: "s_everyone", versionId: vid },
        { skillId: "s_named", versionId: vid },
      ]),
    ).toBe("ok");
    let rows = await q(`SELECT bundle_id FROM web.session_bundle_state WHERE session_id = $1`, [
      sessionId,
    ]);
    expect(rows.map((r) => r.bundle_id).sort()).toEqual(["s_everyone", "s_named"]);
    // The next report no longer carries s_named — its row goes (absence is meaningful).
    expect(await lane.reportApplied(actor, [{ skillId: "s_everyone", versionId: vid }])).toBe("ok");
    rows = await q(`SELECT bundle_id FROM web.session_bundle_state WHERE session_id = $1`, [
      sessionId,
    ]);
    expect(rows.map((r) => r.bundle_id)).toEqual(["s_everyone"]);
  });
});

describe("seat removal", () => {
  it("ends the person's sessions (audited) and cascades their feed rows away", async () => {
    const identity = await import("@/lib/db/identity.server");
    const feed = await import("@/lib/db/queries.feed.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner' LIMIT 1`))[0]
      ?.user_id as string;
    const sessionRow = await q(`SELECT id FROM web.cli_session WHERE user_id = 'u_ent'`);
    const actor = sessionActorFor("u_ent", sessionRow[0]?.id as string, "member");
    // Both row kinds, so the cascade is proven on each.
    await feed.declineBundle(actor, "s_everyone");
    const assignedBefore = await q(
      `SELECT 1 FROM web.assignment WHERE user_id = 'u_ent' AND workspace_id = $1`,
      [wsId],
    );
    expect(assignedBefore.length).toBeGreaterThan(0);
    expect(
      await q(`SELECT 1 FROM web.decline WHERE user_id = 'u_ent' AND workspace_id = $1`, [wsId]),
    ).toHaveLength(1);
    const sessionsBefore = await q(`SELECT id FROM web.cli_session WHERE user_id = 'u_ent'`);
    expect(sessionsBefore.length).toBeGreaterThan(0);

    expect(await identity.removeSeat({ userId: owner, display: "O" }, wsId, "u_ent")).toBe("ok");

    // The standing rows die with the seat (re-invite starts clean) …
    expect(await q(`SELECT 1 FROM web.assignment WHERE user_id = 'u_ent'`)).toHaveLength(0);
    expect(await q(`SELECT 1 FROM web.decline WHERE user_id = 'u_ent'`)).toHaveLength(0);
    expect(await q(`SELECT 1 FROM web.cli_session WHERE user_id = 'u_ent'`)).toHaveLength(0);
    expect(await q(`SELECT 1 FROM web.seat WHERE user_id = 'u_ent'`)).toHaveLength(0);
    // … while the workspace's own everyone-rows are untouched: they are a workspace fact, not
    // a person's, and the baseline must survive any roster change.
    expect(
      await q(`SELECT 1 FROM web.assignment WHERE workspace_id = $1 AND user_id IS NULL`, [wsId]),
    ).not.toHaveLength(0);
    // … and the ending is AUDITED, cause-tagged (history outlives the rows).
    const audits = await q(
      `SELECT 1 FROM web.audit_event
       WHERE workspace_id = $1 AND kind = 'session_ended' AND details ->> 'cause' = 'seat_removed'`,
      [wsId],
    );
    expect(audits.length).toBeGreaterThanOrEqual(1);
  });
});

describe("registrationDecision", () => {
  it("the FULL decision table — both policies × both tenancies × ceremony × knob × invitation × mail", async () => {
    const { registrationDecision } = await import("@/lib/auth/registration.server");
    const bools = [false, true] as const;
    let checked = 0;
    for (const policy of ["gated", "open"] as const) {
      for (const tenancy of ["single", "multi"] as const) {
        for (const inClaimCeremony of bools) {
          for (const registrationKnob of ["invite_only", "open", null] as const) {
            for (const pendingInvitation of bools) {
              for (const mailArmed of bools) {
                // The spec, restated independently of the implementation: an `open`
                // composition admits everything; gated admits the claim ceremony, the
                // SINGLE-tenant workspace knob (a workspace-scoped knob never opens a
                // multi-tenant server), or a pending invitation WITH armed mail (the
                // mailbox round-trip is the proof).
                const expected =
                  policy === "open" ||
                  inClaimCeremony ||
                  (tenancy === "single" && registrationKnob === "open") ||
                  (pendingInvitation && mailArmed)
                    ? "allow"
                    : "refuse";
                expect(
                  registrationDecision({
                    policy,
                    tenancy,
                    inClaimCeremony,
                    inInvitationCeremony: false,
                    registrationKnob,
                    pendingInvitation,
                    mailArmed,
                  }),
                ).toBe(expected);
                checked++;
              }
            }
          }
        }
      }
    }
    expect(checked).toBe(96);
  });

  it("pins the load-bearing rows", async () => {
    const { registrationDecision } = await import("@/lib/auth/registration.server");
    // The workspace `open` knob NEVER opens a multi-tenant server.
    expect(
      registrationDecision({
        policy: "gated",
        tenancy: "multi",
        inClaimCeremony: false,
        inInvitationCeremony: false,
        registrationKnob: "open",
        pendingInvitation: false,
        mailArmed: true,
      }),
    ).toBe("refuse");
    // An invitation WITHOUT armed mail admits nothing — the mailbox round-trip is the proof.
    expect(
      registrationDecision({
        policy: "gated",
        tenancy: "single",
        inClaimCeremony: false,
        inInvitationCeremony: false,
        registrationKnob: "invite_only",
        pendingInvitation: true,
        mailArmed: false,
      }),
    ).toBe("refuse");
    // The open composition admits with every other fact false — and reads nothing.
    expect(
      registrationDecision({
        policy: "open",
        tenancy: "multi",
        inClaimCeremony: false,
        inInvitationCeremony: false,
        registrationKnob: null,
        pendingInvitation: false,
        mailArmed: false,
      }),
    ).toBe("allow");
  });
});
