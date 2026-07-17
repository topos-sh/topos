import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asDevice,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedDevice,
  seedUser,
} from "./helpers/scratch-db";

/**
 * Genesis placement is EXCLUSIVE: `--to` is the targeting mechanism, so a brand-new bundle lands
 * in the default `everyone` channel ONLY when no real `--to` was named (or an explicit
 * `--to everyone`). A named `--to` places into THAT channel alone — additive `everyone`
 * placement would deliver to the whole workspace anyway, defeating the targeting. Direct DAL
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
  const actor = asDevice(wsId, "u_auth", "dk_auth", "member");
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
  await seedDevice(db, "dk_auth", "u_auth", "auth-laptop"); // the audit row's actor device
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
    const actor = asDevice(wsId, "u_auth", "dk_auth", "member");
    const reg = await getDb().transaction((tx) =>
      custody.registerGenesisBundleInTx(tx, actor, "s_g_reserved", "Topos", null),
    );
    // Byte-identical to a collision: no refusal, no oracle — the suffix walks past the reserve.
    expect(reg.name).toBe("topos-2");
  });
});
