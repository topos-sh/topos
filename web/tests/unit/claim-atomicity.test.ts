import { Buffer } from "node:buffer";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  asSession,
  bootWorkspace,
  createScratchDb,
  recordSeededIdentities,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedSession,
  seedUser,
  versionIdFor,
} from "./helpers/scratch-db";
import { type StubVault, startStubVault } from "./helpers/stub-vault";

/**
 * A CLAIM MUST NOT OUTLIVE THE MOVE THAT JUSTIFIES IT.
 *
 * The identity row says "this bundle serves this name". It is written in the same transaction as
 * the pointer move that makes it true, which is only half the guarantee: the move can also
 * FAIL — routinely, with a typed answer rather than a throw. A generation conflict (someone
 * published while a reviewer was deciding) is the common one, and a transaction that commits on
 * that path records a claim for a pointer that never moved: the bundle's OLD name is released
 * while it is still being served, and the next publish may take a name the registry lane is
 * still answering to.
 *
 * The same shape guards the genesis doors: a receipt that lands in its own transaction after the
 * registration commits is a crash window in which a bundle exists with no replay record, so its
 * retry stops being a replay and becomes a second, conflicting publish.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

let db: ScratchDb;
let vault: StubVault;
let wsId = "";

const MEMBER = { id: "u_auth", name: "Author", email: "author@example.com" };

const SERVER = (name: string) => ({
  name,
  description: "A server.",
  version: "1.0.0",
  remotes: [{ type: "streamable-http", url: "https://acme.example/mcp" }],
});

const serverFile = (document: unknown) => ({
  path: "server.json",
  mode: "100644",
  content_base64: Buffer.from(JSON.stringify(document)).toString("base64"),
});

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

/** Every identity this workspace records for one bundle. */
async function claimsOf(bundleId: string): Promise<string[]> {
  const rows = await db.q<{ identity: string }>(
    `SELECT identity FROM web.bundle_identity WHERE bundle_id = $1 ORDER BY identity`,
    [bundleId],
  );
  return rows.map((r) => r.identity);
}

describe("a pointer move that does not land leaves no claim behind", () => {
  it("a typed CAS conflict rolls the claim back — the old name is still held", async () => {
    // A served server, its name on record, and a candidate that would RENAME it.
    const current = versionIdFor("s_cas");
    const candidate = versionIdFor("s_cas_candidate");
    await seedBundle(db, wsId, "s_cas", "cas", { kind: "mcp", versionId: current });
    vault.seed(wsId, "s_cas", current, [
      { path: "server.json", content: JSON.stringify(SERVER("io.github.acme/before")) },
    ]);
    await recordSeededIdentities();
    vault.seed(wsId, "s_cas", candidate, [
      { path: "server.json", content: JSON.stringify(SERVER("io.github.acme/after")) },
    ]);
    expect(await claimsOf("s_cas")).toEqual(["io.github.acme/before"]);

    const { movePointerWithKindPrecondition } = await import("@/lib/db/queries.custody.server");
    // The custody call answers the vault's routine 409 — a TYPED outcome, not a throw. Callers
    // branch on it and report a conflict; nothing moved.
    const outcome = await movePointerWithKindPrecondition({
      kind: "mcp",
      actor: asSession(wsId, MEMBER.id, "cs_auth", "owner"),
      bundleId: "s_cas",
      versionId: candidate,
      move: async () => ({ kind: "conflict" as const, generation: 7 }),
    });

    // The caller still gets its typed outcome — a rollback must not turn a conflict into a fault.
    expect(outcome.refusal).toBeNull();
    expect(outcome.refusal === null && outcome.moved).toEqual({ kind: "conflict", generation: 7 });
    // THE POINT: the name the bundle is STILL SERVING is still recorded as its own, and the name
    // it did not move onto was never taken.
    expect(await claimsOf("s_cas")).toEqual(["io.github.acme/before"]);
    const holder = await db.q<{ bundle_id: string }>(
      `SELECT bundle_id FROM web.bundle_identity WHERE identity = $1`,
      ["io.github.acme/after"],
    );
    expect(holder).toEqual([]);
  });

  it("keeps the claim when the move DOES land", async () => {
    const current = versionIdFor("s_ok_move");
    const candidate = versionIdFor("s_ok_move_candidate");
    await seedBundle(db, wsId, "s_ok_move", "ok-move", { kind: "mcp", versionId: current });
    vault.seed(wsId, "s_ok_move", current, [
      { path: "server.json", content: JSON.stringify(SERVER("io.github.acme/stay")) },
    ]);
    await recordSeededIdentities();
    vault.seed(wsId, "s_ok_move", candidate, [
      { path: "server.json", content: JSON.stringify(SERVER("io.github.acme/moved")) },
    ]);

    const { movePointerWithKindPrecondition } = await import("@/lib/db/queries.custody.server");
    const outcome = await movePointerWithKindPrecondition({
      kind: "mcp",
      actor: asSession(wsId, MEMBER.id, "cs_auth", "owner"),
      bundleId: "s_ok_move",
      versionId: candidate,
      move: async () => ({ kind: "ok" as const }),
    });
    expect(outcome.refusal).toBeNull();
    // The rename went through: the new name is held, the old one released in the same breath.
    expect(await claimsOf("s_ok_move")).toEqual(["io.github.acme/moved"]);
  });
});

/** Drive the session lane's genesis publish. */
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
      kind: "mcp",
      files: [serverFile(SERVER("io.github.acme/after-conflict"))],
    });
    expect(envelope.ok).toBe(true);
  });
});

describe("a name freed between the failed insert and the read", () => {
  it("is claimed on the retry rather than reported as free-but-unwritten", async () => {
    const { claimBundleIdentityInTx } = await import("@/lib/db/bundle-identity.server");
    const { getDb } = await import("@/lib/db/index.server");
    const { sql } = await import("drizzle-orm");
    const NAME = "io.github.acme/handover";

    await seedBundle(db, wsId, "s_hold_a", "hold-a", { kind: "mcp", withPointer: false });
    await seedBundle(db, wsId, "s_hold_b", "hold-b", { kind: "mcp", withPointer: false });
    await db.q(
      `INSERT INTO web.bundle_identity (workspace_id, bundle_id, kind, identity)
       VALUES ($1, $2, 'mcp', $3)`,
      [wsId, "s_hold_a", NAME],
    );

    // The window: the insert conflicts, and by the time the holder is looked up it has let go.
    // Simulated by releasing between the two — the same order the race produces.
    const claimed = await getDb().transaction(async (tx) => {
      const original = tx.execute.bind(tx);
      let released = false;
      // biome-ignore lint/suspicious/noExplicitAny: a one-off seam to order two statements
      (tx as any).execute = async (query: any) => {
        const result = await original(query);
        if (!released) {
          released = true;
          await original(
            sql`DELETE FROM web.bundle_identity WHERE workspace_id = ${wsId} AND bundle_id = 's_hold_a'`,
          );
        }
        return result;
      };
      return await claimBundleIdentityInTx(tx, wsId, "s_hold_b", "mcp", NAME);
    });

    // Reporting "not ok, held by nobody" would let the caller proceed with NO row written — a
    // publish that believes it owns a name it never claimed.
    expect(claimed.ok).toBe(true);
    expect(await claimsOf("s_hold_b")).toEqual([NAME]);
  });
});
