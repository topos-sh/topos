import { execFileSync } from "node:child_process";
import { join, resolve } from "node:path";
import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { SessionActor } from "@/lib/auth/guards.server";
import { applyPlaneDdl } from "../helpers/plane-ddl";
import { installTestEnv } from "./helpers/test-env";

/**
 * The identity model's concurrency-critical ceremonies + the profile-demand predicate,
 * against a REAL scratch Postgres (the drizzle migration + the plane custody DDL applied
 * verbatim): the claim-consume race, the login flow's one-shot answers, session revocation,
 * the session-approval knob, the last-owner fence, seat removal ending sessions + profile,
 * the demand ∩ entitlement delivery matrix, pins, and the delivery wire shape. Actors are
 * minted by CAST — the one thing production code must never do (the brand is module-private
 * to guards.server.ts).
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

describe("the login flow", () => {
  it("start → approve → poll grants IDEMPOTENTLY (re-poll after a crash still gets the grant)", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    // The single-tenant resolve ignores the recorded slug ('' is the origin-addressed form).
    const flow = await identity.startLoginFlow("laptop", "");
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("pending");
    expect(await identity.pendingLoginFlow(flow.userCode)).toEqual({
      requestedName: "laptop",
      requestedWorkspace: "",
      userCode: flow.userCode,
      inviteWorkspace: null,
    });
    const approved = await identity.approveLoginFlow(flow.userCode, {
      userId: owner,
      display: "Owner",
    });
    expect(approved?.requestedName).toBe("laptop");
    // An owner's login is its own approval: born active whatever the knob.
    expect(approved?.sessionStatus).toBe("active");
    const granted = await identity.pollLoginFlow(flow.flowCode);
    expect(granted.status).toBe("granted");
    // The grant REPEATS: the CLI's crash-recovery is to re-poll, so a client that received the
    // grant but crashed before persisting its credential must get the same grant again.
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

  it("deny repeats until the sweep; a re-approve of the same code misses", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const flow = await identity.startLoginFlow("stolen-box", "");
    expect(await identity.denyLoginFlow(flow.userCode, { userId: owner, display: "O" })).toBe(true);
    expect(
      await identity.approveLoginFlow(flow.userCode, { userId: owner, display: "O" }),
    ).toBeNull();
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("denied");
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("denied");
  });

  it("a SEATLESS approver gets null (and cannot deny); a seated one then completes the flow", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    await seedUser("u_seatless", "Seatless", "seatless@example.com");
    const flow = await identity.startLoginFlow("drifter-box", "");
    // Approval requires a seat in the flow's workspace — a seatless person's approve AND deny
    // both land the same uniform refusal, and neither consumes the pending flow.
    expect(
      await identity.approveLoginFlow(flow.userCode, { userId: "u_seatless", display: "S" }),
    ).toBeNull();
    expect(
      await identity.denyLoginFlow(flow.userCode, { userId: "u_seatless", display: "S" }),
    ).toBe(false);
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("pending");
    expect(
      await identity.approveLoginFlow(flow.userCode, { userId: owner, display: "O" }),
    ).not.toBeNull();
  });

  it("an expired pending flow reports expired; the sweep reaps past-TTL rows", async () => {
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("slow-machine", "");
    await q(`UPDATE web.login_flow SET expires_at = now() - interval '1 minute'`);
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("expired");
    // The row lingers until a sweep (read does not delete); the sweep then reaps it.
    expect(await identity.pendingLoginFlow(flow.userCode)).toBeNull();
    expect(await identity.sweepExpiredLoginFlows()).toBeGreaterThanOrEqual(1);
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("expired");
  });
});

describe("session revocation", () => {
  it("ending a session kills the credential; a granted poll then reads expired", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const flow = await identity.startLoginFlow("short-lived", "");
    await identity.approveLoginFlow(flow.userCode, { userId: owner, display: "O" });
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
    const flow = await identity.startLoginFlow("logout-box", "");
    await identity.approveLoginFlow(flow.userCode, { userId: owner, display: "O" });
    expect(await identity.revokeSessionByCredential(flow.flowCode)).toBe(true);
    expect(await identity.revokeSessionByCredential(flow.flowCode)).toBe(false);
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
    const flow = await identity.startLoginFlow("held-box", "");
    const approved = await identity.approveLoginFlow(flow.userCode, {
      userId: "u_knob",
      display: "K",
    });
    expect(approved?.sessionStatus).toBe("pending");
    const sessionId = approved?.sessionId ?? "";
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
    // An owner approves on the sessions page; the session then resolves active.
    expect(await identity.approveSession({ userId: owner, display: "O" }, wsId, sessionId)).toBe(
      "approved",
    );
    expect((await identity.sessionActor(wsId, flow.flowCode))?.sessionStatus).toBe("active");
    // An owner's OWN login stays born active under the knob.
    const ownerFlow = await identity.startLoginFlow("owner-box", "");
    const ownerApproved = await identity.approveLoginFlow(ownerFlow.userCode, {
      userId: owner,
      display: "O",
    });
    expect(ownerApproved?.sessionStatus).toBe("active");
    await q(`UPDATE web.workspace SET session_approval = 'off' WHERE id = $1`, [wsId]);
  });

  it("reject deletes the pending session; owner remove ends an active one", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    await q(`UPDATE web.workspace SET session_approval = 'on' WHERE id = $1`, [wsId]);
    const flow = await identity.startLoginFlow("rejected-box", "");
    const approved = await identity.approveLoginFlow(flow.userCode, {
      userId: "u_knob",
      display: "K",
    });
    expect(
      await identity.rejectSession(
        { userId: owner, display: "O" },
        wsId,
        approved?.sessionId ?? "",
      ),
    ).toBe("rejected");
    expect(await identity.sessionActor(wsId, flow.flowCode)).toBeNull();
    await q(`UPDATE web.workspace SET session_approval = 'off' WHERE id = $1`, [wsId]);

    const flow2 = await identity.startLoginFlow("removed-box", "");
    const approved2 = await identity.approveLoginFlow(flow2.userCode, {
      userId: "u_knob",
      display: "K",
    });
    expect(approved2?.sessionStatus).toBe("active");
    expect(
      await identity.ownerRemoveSession(
        { userId: owner, display: "O" },
        wsId,
        approved2?.sessionId ?? "",
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
    const flow = await identity.startLoginFlow("aging-box", "");
    await identity.approveLoginFlow(flow.userCode, { userId: owner, display: "O" });
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

describe("demand ∩ entitlement (the profile) + delivery", () => {
  it("derives (baseline − channel excludes) ∪ included channels ∪ includes − excludes", async () => {
    const identity = await import("@/lib/db/identity.server");
    const lane = await import("@/lib/db/queries.lane.server");
    await seedUser("u_ent", "Entitled", "entitled@example.com");
    await seatUser("u_ent", "member");
    const flow = await identity.startLoginFlow("ent-box", "");
    await identity.approveLoginFlow(flow.userCode, { userId: "u_ent", display: "E" });
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
    await seedBundle("s_included", "via-direct-include");

    // Baseline: only the everyone-channel bundle (the default channel is implicit).
    let delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).toEqual(["s_everyone"]);
    expect(delivery.skills[0]?.via).toEqual({ channels: ["everyone"], direct: false });

    // A channel include adds its bundles; a bundle include adds the third.
    expect(await lane.profileIncludeChannel(actor, "named-channel")).toBe("included");
    expect(await lane.profileIncludeBundle(actor, "s_included", null)).toBe("included");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id).sort()).toEqual([
      "s_everyone",
      "s_included",
      "s_named",
    ]);

    // Removing a channel-provided bundle records an EXCLUDE (the one negative state) — and
    // the exclude beats every providing source.
    expect(await lane.profileRemoveBundle(actor, "s_named")).toBe("excluded");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id).sort()).toEqual(["s_everyone", "s_included"]);

    // Removing a direct include (nothing else provides it) just deletes the line.
    expect(await lane.profileRemoveBundle(actor, "s_included")).toBe("removed");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).toEqual(["s_everyone"]);

    // Excluding the DEFAULT channel subtracts the baseline.
    expect(await lane.profileRemoveChannel(actor, "everyone")).toBe("excluded");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).toEqual([]);
    // Re-including the baseline clears the exclude.
    expect(await lane.profileIncludeChannel(actor, "everyone")).toBe("included");
    // Re-adding a previously excluded bundle flips the stance back to include.
    expect(await lane.profileIncludeBundle(actor, "s_named", null)).toBe("included");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id).sort()).toEqual(["s_everyone", "s_named"]);

    // The profile read serves the resolved lines.
    const profile = await lane.profileOf(actor);
    expect(profile.map((e) => `${e.mode}:${e.kind}:${e.name}`).sort()).toEqual([
      "include:channel:named-channel",
      "include:skill:via-named-channel",
    ]);
  });

  it("a pinned include serves the pinned version; a stale pin falls back to current", async () => {
    const lane = await import("@/lib/db/queries.lane.server");
    const sessionRow = await q(`SELECT id FROM web.cli_session WHERE user_id = 'u_ent'`);
    const actor = sessionActorFor("u_ent", sessionRow[0]?.id as string, "member");
    const vid = await seedBundle("s_pinned", "pinned-skill");
    // A second version becomes current; the pin holds the first.
    const v2 = "e".repeat(64);
    await q(
      `INSERT INTO plane.version (workspace_id, bundle_id, version_id, commit_id, author_display)
       VALUES ($1, 's_pinned', $2, $2, 'seed')`,
      [wsId, v2],
    );
    await q(
      `UPDATE plane.current_pointer SET version_id = $2, generation = generation + 1
       WHERE workspace_id = $1 AND bundle_id = 's_pinned'`,
      [wsId, v2],
    );
    await q(
      `INSERT INTO plane.version_digest (workspace_id, bundle_id, version_id, bundle_digest)
       VALUES ($1, 's_pinned', $2, $3)`,
      [wsId, v2, "f".repeat(64)],
    );
    expect(await lane.profileIncludeBundle(actor, "s_pinned", vid)).toBe("included");
    let delivery = await lane.deliveryFor(actor);
    let pinned = delivery.skills.find((s) => s.skill_id === "s_pinned");
    expect(pinned?.version_id).toBe(vid);
    expect(pinned?.pinned).toBe(true);
    // The pinned version is purged: delivery falls back to current, honestly un-pinned.
    await q(
      `DELETE FROM plane.version_digest WHERE workspace_id = $1 AND bundle_id = 's_pinned' AND version_id = $2`,
      [wsId, vid],
    );
    delivery = await lane.deliveryFor(actor);
    pinned = delivery.skills.find((s) => s.skill_id === "s_pinned");
    expect(pinned?.version_id).toBe(v2);
    expect(pinned?.pinned).toBeUndefined();
    await lane.profileRemoveBundle(actor, "s_pinned");
  });

  it("the delivery wire shape carries the pinned fields snake_case", async () => {
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
    expect(Array.isArray(delivery.notices)).toBe(true);
    expect(typeof delivery.proposals_awaiting).toBe("number");
    expect(delivery.staleness_window_ms).toBe(604800000);
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
  it("ends the person's sessions (audited) and cascades the profile away", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner' LIMIT 1`))[0]
      ?.user_id as string;
    const profileBefore = await q(`SELECT 1 FROM web.profile_entry WHERE user_id = 'u_ent'`);
    expect(profileBefore.length).toBeGreaterThan(0);
    const sessionsBefore = await q(`SELECT id FROM web.cli_session WHERE user_id = 'u_ent'`);
    expect(sessionsBefore.length).toBeGreaterThan(0);

    expect(await identity.removeSeat({ userId: owner, display: "O" }, wsId, "u_ent")).toBe("ok");

    // The standing rows die with the seat (re-invite starts clean) …
    expect(await q(`SELECT 1 FROM web.profile_entry WHERE user_id = 'u_ent'`)).toHaveLength(0);
    expect(await q(`SELECT 1 FROM web.cli_session WHERE user_id = 'u_ent'`)).toHaveLength(0);
    expect(await q(`SELECT 1 FROM web.seat WHERE user_id = 'u_ent'`)).toHaveLength(0);
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
