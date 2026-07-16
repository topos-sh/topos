import { execFileSync } from "node:child_process";
import { join, resolve } from "node:path";
import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { DeviceActor } from "@/lib/auth/guards.server";
import { applyPlaneDdl } from "../helpers/plane-ddl";
import { installTestEnv } from "./helpers/test-env";

/**
 * The identity model's concurrency-critical ceremonies + the entitlement predicate, against a
 * REAL scratch Postgres (the drizzle migration + the plane custody DDL applied verbatim):
 * the claim-consume race, the device flow's one-shot answers, the last-owner fence, the
 * seat-removal detach cascade, the entitlement matrix, revoke finality (the trigger), and the
 * delivery wire shape. Actors are minted by CAST — the one thing production code must never do
 * (the brand is module-private to guards.server.ts).
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

async function seedBundle(id: string, name: string): Promise<void> {
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
}

async function placeInEveryone(bundleId: string): Promise<void> {
  await q(
    `INSERT INTO web.channel_bundle (channel_id, workspace_id, bundle_id)
     SELECT id, workspace_id, $1 FROM web.channel WHERE is_default AND workspace_id = $2`,
    [bundleId, wsId],
  );
}

function deviceActorFor(userId: string, deviceId: string, role: DeviceActor["role"]): DeviceActor {
  return { userId, display: userId, workspaceId: wsId, deviceId, role } as DeviceActor;
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

describe("the device flow", () => {
  it("start → approve → poll grants IDEMPOTENTLY (re-poll after a crash still gets the grant)", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    // The single-tenant resolve ignores the recorded slug ('' is the origin-addressed form).
    const flow = await identity.startDeviceAuth("laptop", "");
    expect((await identity.pollDeviceAuth(flow.deviceCode)).status).toBe("pending");
    expect(await identity.pendingDeviceAuth(flow.userCode)).toEqual({
      requestedName: "laptop",
      requestedWorkspace: "",
    });
    const approved = await identity.approveDeviceAuth(flow.userCode, {
      userId: owner,
      display: "Owner",
    });
    expect(approved?.requestedName).toBe("laptop");
    const granted = await identity.pollDeviceAuth(flow.deviceCode);
    expect(granted.status).toBe("granted");
    // The grant REPEATS: the CLI's crash-recovery is to re-poll, so a device that received the
    // grant but crashed before persisting its credential must get the same grant again.
    const reAfterCrash = await identity.pollDeviceAuth(flow.deviceCode);
    expect(reAfterCrash.status).toBe("granted");
    expect(reAfterCrash.status === "granted" && reAfterCrash.deviceId).toBe(
      granted.status === "granted" ? granted.deviceId : "",
    );
    // The credential (the promoted device code) resolves; a bogus one does not.
    expect(await identity.deviceActor(wsId, flow.deviceCode)).not.toBeNull();
    expect(await identity.deviceActor(wsId, "not-a-credential")).toBeNull();
  });

  it("deny repeats until the sweep; a re-approve of the same code misses", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const flow = await identity.startDeviceAuth("stolen-box", "");
    expect(await identity.denyDeviceAuth(flow.userCode, { userId: owner, display: "O" })).toBe(
      true,
    );
    expect(
      await identity.approveDeviceAuth(flow.userCode, { userId: owner, display: "O" }),
    ).toBeNull();
    expect((await identity.pollDeviceAuth(flow.deviceCode)).status).toBe("denied");
    expect((await identity.pollDeviceAuth(flow.deviceCode)).status).toBe("denied");
  });

  it("a SEATLESS approver gets null (and cannot deny); a seated one then completes the flow", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    await seedUser("u_seatless", "Seatless", "seatless@example.com");
    const flow = await identity.startDeviceAuth("drifter-box", "");
    // Approval requires a seat in the flow's workspace — a seatless person's approve AND deny
    // both land the same uniform refusal, and neither consumes the pending flow.
    expect(
      await identity.approveDeviceAuth(flow.userCode, { userId: "u_seatless", display: "S" }),
    ).toBeNull();
    expect(
      await identity.denyDeviceAuth(flow.userCode, { userId: "u_seatless", display: "S" }),
    ).toBe(false);
    expect((await identity.pollDeviceAuth(flow.deviceCode)).status).toBe("pending");
    expect(
      await identity.approveDeviceAuth(flow.userCode, { userId: owner, display: "O" }),
    ).not.toBeNull();
  });

  it("an expired pending flow reports expired; the sweep reaps past-TTL rows", async () => {
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startDeviceAuth("slow-machine", "");
    await q(`UPDATE web.device_auth_session SET expires_at = now() - interval '1 minute'`);
    expect((await identity.pollDeviceAuth(flow.deviceCode)).status).toBe("expired");
    // The row lingers until a sweep (read does not delete); the sweep then reaps it.
    expect(await identity.pendingDeviceAuth(flow.userCode)).toBeNull();
    expect(await identity.sweepExpiredDeviceAuth()).toBeGreaterThanOrEqual(1);
    expect((await identity.pollDeviceAuth(flow.deviceCode)).status).toBe("expired");
  });
});

describe("revoke finality", () => {
  it("revocation is final: the trigger refuses any un-revoke", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const flow = await identity.startDeviceAuth("short-lived", "");
    await identity.approveDeviceAuth(flow.userCode, { userId: owner, display: "O" });
    const granted = await identity.pollDeviceAuth(flow.deviceCode);
    expect(granted.status).toBe("granted");
    const deviceId = granted.status === "granted" ? granted.deviceId : "";
    expect(await identity.revokeOwnDevice({ userId: owner, display: "O" }, deviceId)).toBe(true);
    // The credential dies with the row flip …
    expect(await identity.deviceActor(wsId, flow.deviceCode)).toBeNull();
    // … and the flip is one-way: the database itself refuses the resurrection.
    await expect(
      q(`UPDATE web.device SET revoked_at = NULL WHERE id = $1`, [deviceId]),
    ).rejects.toThrow(/revoke|final|one-way/i);
  });
});

describe("the last-owner fence", () => {
  it("demoting the sole owner is refused; a second owner unlocks it", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner'`))[0]
      ?.user_id as string;
    const acting = { userId: owner, display: "Owner" };
    expect(await identity.setSeatRole(acting, wsId, owner, "member")).toBe("last_owner");
    expect(await identity.removeSeat(acting, wsId, owner, "membership_removed")).toBe("last_owner");

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

describe("the entitlement predicate + delivery", () => {
  it("derives (default − optout) ∪ member channels ∪ follows − unfollows − exclusions", async () => {
    const identity = await import("@/lib/db/identity.server");
    const lane = await import("@/lib/db/queries.lane.server");
    await seedUser("u_ent", "Entitled", "entitled@example.com");
    await seatUser("u_ent", "member");
    const flow = await identity.startDeviceAuth("ent-box", "");
    await identity.approveDeviceAuth(flow.userCode, { userId: "u_ent", display: "E" });
    const granted = await identity.pollDeviceAuth(flow.deviceCode);
    const deviceId = granted.status === "granted" ? granted.deviceId : "";
    const actor = deviceActorFor("u_ent", deviceId, "member");

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
    await seedBundle("s_followed", "via-direct-follow");

    // Baseline: only the everyone-channel bundle.
    let delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).toEqual(["s_everyone"]);
    expect(delivery.skills[0]?.via).toEqual({ channels: ["everyone"], direct: false });

    // A named-channel membership adds its bundles.
    expect(await lane.laneChannelJoin(actor, "named-channel")).toBe("joined");
    // A direct follow adds the third.
    expect(await lane.followBundle(actor, "s_followed")).toBe("followed");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id).sort()).toEqual([
      "s_everyone",
      "s_followed",
      "s_named",
    ]);

    // The unfollow mask beats every source.
    expect(await lane.unfollowBundle(actor, "s_named")).toBe("unfollowed");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id).sort()).toEqual(["s_everyone", "s_followed"]);
    expect(delivery.detached).toContain("s_named");

    // A device exclusion subtracts from THIS device only and rides `excluded`.
    expect(await lane.excludeOnDevice(actor, "s_everyone")).toBe("excluded");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).toEqual(["s_followed"]);
    expect(delivery.excluded).toEqual(["s_everyone"]);

    // The default-channel opt-out removes the everyone union arm.
    expect(await lane.laneChannelLeave(actor, "everyone")).toBe("left");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id)).toEqual(["s_followed"]);
    // Rejoining the default channel deletes the opt-out and heals.
    expect(await lane.laneChannelJoin(actor, "everyone")).toBe("joined");
    // Following on this device lifts the exclusion.
    expect(await lane.followBundle(actor, "s_everyone")).toBe("followed");
    delivery = await lane.deliveryFor(actor);
    expect(delivery.skills.map((s) => s.skill_id).sort()).toEqual(["s_everyone", "s_followed"]);
  });

  it("the delivery wire shape carries the pinned fields snake_case", async () => {
    const lane = await import("@/lib/db/queries.lane.server");
    const device = (
      await q(
        `SELECT d.id FROM web.device d JOIN web."user" u ON u.id = d.user_id WHERE u.id = 'u_ent'`,
      )
    )[0]?.id as string;
    const actor = deviceActorFor("u_ent", device, "member");
    const delivery = await lane.deliveryFor(actor);
    expect(delivery.schema_version).toBe(1);
    expect(delivery.workspace_id).toBe(wsId);
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
});

describe("seat removal", () => {
  it("writes detach records for the delivered set and cascades the stance rows", async () => {
    const identity = await import("@/lib/db/identity.server");
    const owner = (await q(`SELECT user_id FROM web.seat WHERE role = 'owner' LIMIT 1`))[0]
      ?.user_id as string;
    const stancesBefore = await q(
      `SELECT bundle_id, state FROM web.bundle_subscription WHERE user_id = 'u_ent'`,
    );
    expect(stancesBefore.length).toBeGreaterThan(0);

    expect(
      await identity.removeSeat(
        { userId: owner, display: "O" },
        wsId,
        "u_ent",
        "membership_removed",
      ),
    ).toBe("ok");

    // The lapse RECORDS survive the seat (they exist to outlive it). The earlier unfollow's
    // record keeps its ORIGINAL cause (already-detached rows are never re-labelled); the
    // bundles this removal lapsed carry the removal's.
    const detached = await q<{ bundle_id: string; cause: string }>(
      `SELECT bundle_id, cause FROM web.bundle_detachment WHERE user_id = 'u_ent' ORDER BY bundle_id`,
    );
    expect(detached.length).toBeGreaterThanOrEqual(2);
    expect(detached.find((d) => d.bundle_id === "s_named")?.cause).toBe("unfollow");
    expect(detached.find((d) => d.bundle_id === "s_everyone")?.cause).toBe("membership_removed");
    expect(detached.find((d) => d.bundle_id === "s_followed")?.cause).toBe("membership_removed");
    // … while the standing policy rows die with it (re-invite starts clean).
    expect(await q(`SELECT 1 FROM web.bundle_subscription WHERE user_id = 'u_ent'`)).toHaveLength(
      0,
    );
    expect(await q(`SELECT 1 FROM web.channel_member WHERE user_id = 'u_ent'`)).toHaveLength(0);
    expect(await q(`SELECT 1 FROM web.channel_optout WHERE user_id = 'u_ent'`)).toHaveLength(0);
    expect(await q(`SELECT 1 FROM web.seat WHERE user_id = 'u_ent'`)).toHaveLength(0);
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
        registrationKnob: null,
        pendingInvitation: false,
        mailArmed: false,
      }),
    ).toBe("allow");
  });
});
