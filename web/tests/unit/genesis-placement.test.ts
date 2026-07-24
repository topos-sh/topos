import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asSession,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedSession,
  seedUser,
} from "./helpers/scratch-db";

/**
 * Genesis placement is EXCLUSIVE: `--to` is the targeting mechanism, so a brand-new bundle lands
 * in the default `everyone` channel ONLY when no `--to` was named. A named `--to` (`everyone`
 * included) places into THAT channel alone — additive `everyone` placement would deliver to the
 * whole workspace anyway, defeating the targeting. EVERY placement is mode-gated (custody is
 * never curation-blocked; REACH is — including the default channel): a curated channel withholds
 * a member's placement with `curated_role_required`, disclosed on the registration. Direct DAL
 * call inside a transaction (registerGenesisBundleInTx is a Tx step).
 */

let db: ScratchDb;
let wsId = "";

/** Run the genesis registration inside one transaction and return the channels it placed into. */
async function genesisPlacementChannels(
  bundleId: string,
  displayName: string | null,
  toChannel: string | null,
): Promise<string[]> {
  const { getDb } = await import("@/lib/db/index.server");
  const custody = await import("@/lib/db/queries.custody.server");
  const actor = asSession(wsId, "u_auth", "dk_auth", "member");
  await getDb().transaction((tx) =>
    custody.registerGenesisBundleInTx(tx, actor, bundleId, displayName, toChannel),
  );
  const rows = await db.q<{ name: string }>(
    `SELECT ch.name FROM web.channel_bundle cb
     JOIN web.channel ch ON ch.id = cb.channel_id
     WHERE cb.bundle_id = $1 ORDER BY ch.name`,
    [bundleId],
  );
  return rows.map((r) => r.name);
}

beforeAll(async () => {
  db = await createScratchDb("web_genesis");
  wsId = await bootWorkspace();
  await seedUser(db, "u_auth", "Author", "author@example.com");
  await seatUser(db, wsId, "u_auth", "member");
  await seedSession(db, "dk_auth", wsId, "u_auth"); // the audit row's actor session
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("registerGenesisBundleInTx — exclusive placement", () => {
  it("no `--to` lands in `everyone` (the unchanged whole-workspace default)", async () => {
    expect(await genesisPlacementChannels("s_g_default", "Gadget Default", null)).toEqual([
      "everyone",
    ]);
  });

  it("a real `--to ops` lands in `ops` ONLY — never additionally in `everyone`", async () => {
    expect(await genesisPlacementChannels("s_g_ops", "Gadget Ops", "ops")).toEqual(["ops"]);
  });

  it("an explicit `--to everyone` lands in `everyone` exactly once (no duplicate)", async () => {
    expect(await genesisPlacementChannels("s_g_everyone", "Gadget Everyone", "everyone")).toEqual([
      "everyone",
    ]);
  });

  it("the catalog name `topos` is reserved (the CLI's built-in skill) — minted past like a taken name", async () => {
    const { getDb } = await import("@/lib/db/index.server");
    const custody = await import("@/lib/db/queries.custody.server");
    const actor = asSession(wsId, "u_auth", "dk_auth", "member");
    const reg = await getDb().transaction((tx) =>
      custody.registerGenesisBundleInTx(tx, actor, "s_g_reserved", "Topos", null),
    );
    // Byte-identical to a collision: no refusal, no oracle — the suffix walks past the reserve.
    expect(reg.name).toBe("topos-2");
  });
});

describe("registerGenesisBundleInTx — a curated `everyone` gates REACH, never custody", () => {
  /** Register under `role`, returning the registration AND the channels the bundle landed in. */
  async function genesisUnderCurated(
    bundleId: string,
    displayName: string,
    toChannel: string | null,
    role: "member" | "reviewer",
  ) {
    const { getDb } = await import("@/lib/db/index.server");
    const custody = await import("@/lib/db/queries.custody.server");
    const actor =
      role === "reviewer"
        ? asSession(wsId, "u_rev", "dk_rev", "reviewer")
        : asSession(wsId, "u_auth", "dk_auth", "member");
    const reg = await getDb().transaction((tx) =>
      custody.registerGenesisBundleInTx(tx, actor, bundleId, displayName, toChannel),
    );
    const rows = await db.q<{ name: string }>(
      `SELECT ch.name FROM web.channel_bundle cb
       JOIN web.channel ch ON ch.id = cb.channel_id
       WHERE cb.bundle_id = $1 ORDER BY ch.name`,
      [bundleId],
    );
    return { reg, channels: rows.map((r) => r.name) };
  }

  beforeAll(async () => {
    await seedUser(db, "u_rev", "Reviewer", "reviewer@example.com");
    await seatUser(db, wsId, "u_rev", "reviewer");
    await seedSession(db, "dk_rev", wsId, "u_rev");
    await db.q(`UPDATE web.channel SET mode = 'curated' WHERE workspace_id = $1 AND is_default`, [
      wsId,
    ]);
  });

  it("a member's bare genesis stays CATALOG-ONLY — no row, the withheld placement disclosed", async () => {
    const { reg, channels } = await genesisUnderCurated("s_c_member", "Cur Member", null, "member");
    expect(channels).toEqual([]);
    expect(reg.placement).toBe("curated_role_required");
  });

  it("a member's explicit `--to everyone` rides the same gate as any named curated channel", async () => {
    const { reg, channels } = await genesisUnderCurated(
      "s_c_to_ev",
      "Cur To Ev",
      "everyone",
      "member",
    );
    expect(channels).toEqual([]);
    expect(reg.placement).toBe("curated_role_required");
  });

  it("a reviewer's bare genesis still places — the curated gate passes reviewer+", async () => {
    const { reg, channels } = await genesisUnderCurated(
      "s_c_rev",
      "Cur Reviewer",
      null,
      "reviewer",
    );
    expect(channels).toEqual(["everyone"]);
    expect(reg.placement).toBeUndefined();
  });
});
