import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  asSession,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedSession,
  seedUser,
} from "./helpers/scratch-db";
import { type StubVault, startStubVault } from "./helpers/stub-vault";

/**
 * A GENESIS PUBLISH IS ONE TRANSACTION OR IT IS NOTHING.
 *
 * A receipt that lands in its own transaction after the registration commits is a crash window in
 * which a bundle exists with no replay record — so its retry stops being a replay and becomes a
 * second, conflicting publish. The suite proves the two rows are written by ONE transaction
 * (`xmin`, not "both exist afterwards"), that a fault inside it leaves neither, and that the
 * vault's routine 409 comes back as a typed conflict rather than a fault.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

let db: ScratchDb;
let vault: StubVault;
let wsId = "";

const MEMBER = { id: "u_auth", name: "Author", email: "author@example.com" };

beforeAll(async () => {
  vault = await startStubVault();
  db = await createScratchDb("web_claim_atomicity", {
    TOPOS_WEB_RATELIMIT: "off",
    PLANE_INTERNAL_URL: vault.url,
  });
  wsId = await bootWorkspace();
  await seedUser(db, MEMBER.id, MEMBER.name, MEMBER.email);
  await seatUser(db, wsId, MEMBER.id, "owner");
  await seedSession(db, "cs_auth", wsId, MEMBER.id);
  session = { user: MEMBER };
}, 60000);

afterAll(async () => {
  await vault.close();
  await db.drop();
});

async function laneGenesis(args: {
  bundleId: string;
  displayName: string;
  kind?: string;
  files: { path: string; mode: string; content_base64: string }[];
  upstream?: { host: string; repo: string; path: string; commit: string | null; license: null };
  opId?: string;
}): Promise<Record<string, unknown>> {
  const { publishFlow } = await import("@/lib/api/publish-flow.server");
  const raw = JSON.stringify({ skill_id: args.bundleId });
  const res = await publishFlow({
    actor: asSession(wsId, MEMBER.id, "cs_auth", "owner"),
    raw,
    opId: args.opId ?? crypto.randomUUID(),
    skillId: args.bundleId,
    expected: 0,
    candidate: { files: args.files, parents: [], author: MEMBER.name, message: "genesis" },
    displayName: args.displayName,
    channel: null,
    kind: args.kind ?? null,
    command: "publish",
    forceProposal: false,
    ...(args.upstream === undefined ? {} : { upstream: args.upstream }),
  });
  return (await res.json()) as Record<string, unknown>;
}

describe("a genesis publish registers and records its receipt together", () => {
  it("both rows land on success", async () => {
    const opId = crypto.randomUUID();
    const envelope = await laneGenesis({
      bundleId: "s_receipt_ok",
      displayName: "Receipt OK",
      files: [{ path: "SKILL.md", mode: "100644", content_base64: "IyBv" }],
      opId,
    });
    expect(envelope.ok).toBe(true);
    expect(await db.q(`SELECT id FROM web.bundle WHERE id = $1`, ["s_receipt_ok"])).toHaveLength(1);
    // The replay record for this op — without it a retry stops being a replay.
    expect(
      await db.q(`SELECT op_id FROM web.op_receipt WHERE op_id = $1::uuid`, [opId]),
    ).toHaveLength(1);

    // AND BY THE SAME TRANSACTION. `xmin` is the id of the transaction that wrote a row, so two
    // rows sharing one is proof they cannot be torn apart by a crash between them — which is the
    // whole property, and the one a "both rows exist afterwards" check cannot see.
    const [bundleRow] = await db.q<{ x: string }>(
      `SELECT xmin::text AS x FROM web.bundle WHERE id = $1`,
      ["s_receipt_ok"],
    );
    const [receiptRow] = await db.q<{ x: string }>(
      `SELECT xmin::text AS x FROM web.op_receipt WHERE op_id = $1::uuid`,
      [opId],
    );
    expect(receiptRow?.x).toBe(bundleRow?.x);
  });

  it("NEITHER lands when the transaction fails after the receipt is written", async () => {
    const opId = crypto.randomUUID();
    // A malformed upstream repo trips `bundle_upstream_repo_check` INSIDE the genesis
    // transaction — a real fault at exactly the moment the two writes must be inseparable.
    await expect(
      laneGenesis({
        bundleId: "s_receipt_torn",
        displayName: "Torn",
        files: [{ path: "SKILL.md", mode: "100644", content_base64: "IyBv" }],
        upstream: { host: "github.com", repo: "not-a-repo", path: "", commit: null, license: null },
        opId,
      }),
    ).rejects.toThrow();
    // No half-registered bundle, and no receipt claiming an op that produced nothing.
    expect(await db.q(`SELECT id FROM web.bundle WHERE id = $1`, ["s_receipt_torn"])).toEqual([]);
    expect(await db.q(`SELECT op_id FROM web.op_receipt WHERE op_id = $1::uuid`, [opId])).toEqual(
      [],
    );
  });
});

describe("a genesis publish the vault answers 409 to", () => {
  it("reports the typed CONFLICT rather than a fault", async () => {
    vault.conflictNextPublish(3);
    const envelope = await laneGenesis({
      bundleId: "s_genesis_conflict",
      displayName: "Genesis Conflict",
      files: [{ path: "SKILL.md", mode: "100644", content_base64: "IyBv" }],
    });
    // A routine race, not a 500: the client is told the pointer moved under it and can retry —
    // in the SAME words the registered-bundle path uses for the same situation.
    expect(envelope.ok).toBe(false);
    expect((envelope.error as { code: string }).code).toBe("STALE_BASE");
    // Nothing was registered against the id the vault refused.
    expect(await db.q(`SELECT id FROM web.bundle WHERE id = $1`, ["s_genesis_conflict"])).toEqual(
      [],
    );
  });

  it("still lands normally on the next attempt", async () => {
    const envelope = await laneGenesis({
      bundleId: "s_genesis_after",
      displayName: "Genesis After",
      files: [{ path: "SKILL.md", mode: "100644", content_base64: "IyBv" }],
    });
    expect(envelope.ok).toBe(true);
  });
});
