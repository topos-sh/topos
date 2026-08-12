import { Buffer } from "node:buffer";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import {
  asOwner,
  asSession,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedSession,
  seedUser,
  versionIdFor,
} from "./helpers/scratch-db";
import { type StubVault, startStubVault } from "./helpers/stub-vault";

/**
 * THE MCP PUBLISH GATE, against a real scratch Postgres and the app's ONE custody transport
 * re-pointed at an in-process stub vault (no module mocking: `vaultFetch`, the route
 * allowlist, and the typed wrappers all still run). Two things are proven here that cannot be
 * proven anywhere else:
 *
 *  · a refused document reaches NO custody route — the stub records every write it answers, so
 *    an empty recorder IS the proof that no bytes were ingested against a refusal;
 *  · the embedded-name uniqueness rule reads the OTHER bundles' stored documents through the
 *    vault, which is why the check needs a real byte store rather than a stub return value.
 *
 * A non-MCP publish is untouched by all of it — the same suite asserts that too, because "this
 * changed nothing for skills" is the load-bearing half of adding a branch to a shared flow.
 */

let db: ScratchDb;
let vault: StubVault;
let wsId = "";
let opSeq = 0;

function opId(): string {
  opSeq += 1;
  return `00000000-0000-4000-8000-${String(opSeq).padStart(12, "0")}`;
}

const WEATHER = {
  name: "io.github.acme/weather",
  description: "Current conditions for a named place.",
  version: "1.0.0",
  remotes: [{ type: "streamable-http", url: "https://weather.acme.example/mcp" }],
};

function serverJsonFile(document: unknown): {
  path: string;
  mode: string;
  content_base64: string;
} {
  return {
    path: "server.json",
    mode: "100644",
    content_base64: Buffer.from(
      typeof document === "string" ? document : JSON.stringify(document, null, 2),
      "utf8",
    ).toString("base64"),
  };
}

async function runFlow(args: {
  skillId: string;
  files: { path: string; mode: string; content_base64: string }[];
  kind?: string | null;
  expected?: number;
  forceProposal?: boolean;
  /** The birth display name — distinct per bundle where two publishes run at once, so the
   * race under test is the EMBEDDED name's and not the catalog name's. */
  displayName?: string;
}): Promise<Record<string, unknown>> {
  const { publishFlow } = await import("@/lib/api/publish-flow.server");
  const raw = JSON.stringify({ skill_id: args.skillId, op: opSeq });
  const res = await publishFlow({
    actor: asSession(wsId, "u_auth", "cs_auth", "member"),
    raw,
    opId: opId(),
    skillId: args.skillId,
    expected: args.expected ?? 0,
    candidate: { files: args.files, parents: [], author: "Author <a@b.test>", message: "genesis" },
    displayName: args.displayName ?? "Weather",
    channel: null,
    kind: args.kind ?? null,
    command: "publish",
    forceProposal: args.forceProposal ?? false,
  });
  return (await res.json()) as Record<string, unknown>;
}

/**
 * The custody rows a landed publish leaves behind — the version and the `current` pointer the
 * catalog scan joins against — WITHOUT the catalog row, plus the document in the stub's byte
 * store. The stub vault deliberately writes no `plane.*` rows, so a publish that is about to
 * register through the flow gets them here, exactly as the real vault would have left them.
 */
async function seedCustody(bundleId: string, document: unknown): Promise<void> {
  const versionId = versionIdFor(bundleId);
  await db.q(
    `INSERT INTO plane.version (workspace_id, bundle_id, version_id, commit_id, author_display)
     VALUES ($1, $2, $3, $3, 'seed')`,
    [wsId, bundleId, versionId],
  );
  await db.q(
    `INSERT INTO plane.current_pointer (workspace_id, bundle_id, version_id, moved_by_display)
     VALUES ($1, $2, $3, 'seed')`,
    [wsId, bundleId, versionId],
  );
  vault.seed(wsId, bundleId, versionId, [
    { path: "server.json", content: JSON.stringify(document, null, 2) },
  ]);
}

/** An mcp bundle that is already published here: the catalog row, its custody rows, and the
 * document the vault serves for its `current`. */
async function seedPublishedServer(
  bundleId: string,
  name: string,
  document: unknown,
): Promise<void> {
  const versionId = versionIdFor(bundleId);
  await seedBundle(db, wsId, bundleId, name, { kind: "mcp", versionId });
  vault.seed(wsId, bundleId, versionId, [
    { path: "server.json", content: JSON.stringify(document, null, 2) },
  ]);
  // A seeded server stands as if it had been published here — and a published server's registry
  // name is RECORDED, or nothing holds it against the next publish. The boot backfill is exactly
  // the step that records the name of a server that already exists, so the fixture runs the real
  // one rather than writing the row itself (which would let the row and the bytes drift apart).
  const { backfillBundleIdentities } = await import("@/lib/db/bundle-identity.server");
  const report = await backfillBundleIdentities();
  expect(report.unreadable).toEqual([]);
}

const errorOf = (envelope: Record<string, unknown>) =>
  envelope.error as { code: string; context: { message?: string } } | undefined;

beforeAll(async () => {
  vault = await startStubVault();
  db = await createScratchDb("web_mcp_gate", {
    TOPOS_WEB_RATELIMIT: "off",
    PLANE_INTERNAL_URL: vault.url,
  });
  wsId = await bootWorkspace();
  await seedUser(db, "u_auth", "Author", "author@example.com");
  await seatUser(db, wsId, "u_auth", "member");
  await seedSession(db, "cs_auth", wsId, "u_auth");
}, 60000);

afterAll(async () => {
  await vault.close();
  await db.drop();
});

beforeEach(() => {
  vault.calls.length = 0;
  vault.published.length = 0;
});

describe("what an MCP candidate must carry", () => {
  it("no server.json at the root is MCP_INVALID — and nothing reached the vault", async () => {
    const envelope = await runFlow({
      skillId: "s_no_doc",
      kind: "mcp",
      files: [serverJsonFile(WEATHER)].map((f) => ({ ...f, path: "config/server.json" })),
    });
    expect(envelope.ok).toBe(false);
    expect(errorOf(envelope)?.code).toBe("MCP_INVALID");
    expect(vault.calls).toEqual([]);
  });

  it.each([
    [
      "a package pinned to nothing",
      {
        ...WEATHER,
        packages: [
          { registryType: "npm", identifier: "@a/x", version: "^1", transport: { type: "stdio" } },
        ],
      },
      "MCP_PACKAGE_UNPINNED",
    ],
    [
      "an event-stream-only server",
      { ...WEATHER, remotes: [{ type: "sse", url: "https://a.example/sse" }] },
      "MCP_NO_STREAMABLE_REMOTE",
    ],
    [
      "a plaintext endpoint",
      { ...WEATHER, remotes: [{ type: "streamable-http", url: "http://a.example/mcp" }] },
      "MCP_INSECURE_URL",
    ],
    [
      "a templated endpoint",
      { ...WEATHER, remotes: [{ type: "streamable-http", url: "https://{t}.a.example/mcp" }] },
      "MCP_URL_TEMPLATE",
    ],
    [
      "a credential header",
      {
        ...WEATHER,
        remotes: [
          {
            type: "streamable-http",
            url: "https://a.example/mcp",
            headers: [{ name: "X-Key", value: "k", isSecret: true }],
          },
        ],
      },
      "MCP_SECRET_REFUSED",
    ],
    ["a nameless document", { description: "d", version: "1.0.0" }, "MCP_INVALID"],
  ])("refuses %s before any custody call", async (_label, document, code) => {
    const envelope = await runFlow({
      skillId: `s_bad_${code}_${opSeq}`,
      kind: "mcp",
      files: [serverJsonFile(document)],
    });
    expect(errorOf(envelope)?.code).toBe(code);
    // The refusal says WHAT is wrong — a code alone would leave the author guessing.
    expect(errorOf(envelope)?.context.message?.length ?? 0).toBeGreaterThan(10);
    expect(vault.calls).toEqual([]);
  });

  it("a valid document publishes, and the bundle is born 'mcp' holding exactly server.json", async () => {
    const envelope = await runFlow({
      skillId: "s_ok_mcp",
      kind: "mcp",
      files: [serverJsonFile(WEATHER)],
    });
    expect(envelope.ok).toBe(true);
    expect(vault.calls).toEqual([{ route: "publish", ws: wsId, bundle: "s_ok_mcp" }]);
    expect(vault.published[0]?.files.map((f) => f.path)).toEqual(["server.json"]);
    const rows = await db.q<{ kind: string }>(`SELECT kind FROM web.bundle WHERE id = $1`, [
      "s_ok_mcp",
    ]);
    expect(rows[0]?.kind).toBe("mcp");
  });
});

describe("the embedded name is unique per workspace", () => {
  // Its OWN registry name: an earlier case in this file publishes WEATHER for real and therefore
  // holds that name for the rest of the run, which is the point of the claim.
  const HELD = { ...WEATHER, name: "io.github.acme/held" };

  beforeAll(async () => {
    // An MCP bundle already published here, its claim on record — the state the uniqueness
    // check actually reads.
    await seedPublishedServer("s_held", "weather-held", HELD);
  });

  it("a second bundle claiming the same server name is DENIED MCP_NAME_TAKEN", async () => {
    const envelope = await runFlow({
      skillId: "s_dupe",
      kind: "mcp",
      files: [serverJsonFile(HELD)],
    });
    expect(errorOf(envelope)?.code).toBe("MCP_NAME_TAKEN");
    // The refusal names the bundle that holds it AND says where it lives, so the answer is one
    // step from the thing it is about rather than a name to go hunting for.
    expect(errorOf(envelope)?.context.message).toContain("weather-held");
    expect(errorOf(envelope)?.context.message).toContain("/mcp/weather-held");
    expect(vault.calls).toEqual([]);
  });

  it("a DIFFERENT server name is free", async () => {
    const envelope = await runFlow({
      skillId: "s_other_name",
      kind: "mcp",
      files: [serverJsonFile({ ...WEATHER, name: "io.github.acme/tides" })],
    });
    expect(envelope.ok).toBe(true);
  });

  it("republishing the SAME bundle is not a collision with itself", async () => {
    const envelope = await runFlow({
      skillId: "s_held",
      kind: "mcp",
      expected: 1,
      files: [serverJsonFile({ ...HELD, version: "1.1.0" })],
    });
    expect(envelope.ok).toBe(true);
    expect(vault.calls).toEqual([{ route: "publish", ws: wsId, bundle: "s_held" }]);
  });

  it("the PROPOSE arm is gated identically — before the commit-only ingest", async () => {
    const envelope = await runFlow({
      skillId: "s_held",
      kind: "mcp",
      expected: 1,
      forceProposal: true,
      files: [serverJsonFile({ ...HELD, remotes: [{ type: "sse", url: "https://a.example/s" }] })],
    });
    expect(errorOf(envelope)?.code).toBe("MCP_NO_STREAMABLE_REMOTE");
    expect(vault.calls).toEqual([]);
  });

  it("an archived MCP bundle holds no name (it delivers nothing)", async () => {
    // Published first (so its name IS claimed), then archived through the REAL ceremony — the
    // release is archive's job, so this proves archive does it. Seeding it archived from the
    // start would prove nothing: an unclaimed name is free either way.
    const SURF = { ...WEATHER, name: "io.github.acme/surf" };
    await seedPublishedServer("s_gone", "tides-old", SURF);
    const { archiveBundle } = await import("@/lib/db/queries.lifecycle.server");
    expect((await archiveBundle(asOwner(wsId, "u_auth", "Author"), "s_gone")).outcome).toBe(
      "archived",
    );
    const envelope = await runFlow({
      skillId: "s_surf",
      kind: "mcp",
      files: [serverJsonFile(SURF)],
    });
    expect(envelope.ok).toBe(true);
  });
});

/**
 * The name check answers before the custody call, which makes it a PRE-check: by the time a
 * bundle is actually registered, another publish may have taken the name. Every door that
 * registers therefore CLAIMS inside its final transaction, where the identity key — not a
 * comparison — decides. These two are the arms that claim: the publish flow, and the unarchive
 * that re-takes a name archiving had freed.
 */
describe("the claim inside the final transaction", () => {
  it("two concurrent genesis publishes of one embedded name serialize", async () => {
    const RACE = { ...WEATHER, name: "io.github.acme/race" };
    await seedCustody("s_race_a", RACE);
    await seedCustody("s_race_b", RACE);
    const envelopes = await Promise.all([
      runFlow({
        skillId: "s_race_a",
        kind: "mcp",
        displayName: "Race A",
        files: [serverJsonFile(RACE)],
      }),
      runFlow({
        skillId: "s_race_b",
        kind: "mcp",
        displayName: "Race B",
        files: [serverJsonFile(RACE)],
      }),
    ]);
    // Exactly one of them owns the name; the other is told so in the same words the pre-check
    // uses. Which one wins is the lock's business, not the test's.
    expect(envelopes.filter((e) => e.ok === true)).toHaveLength(1);
    const denied = envelopes.filter((e) => e.ok === false);
    expect(denied).toHaveLength(1);
    expect(errorOf(denied[0] ?? {})?.code).toBe("MCP_NAME_TAKEN");
    // And the catalog agrees: one active mcp bundle claims the name, not two.
    const rows = await db.q<{ n: number }>(
      `SELECT count(*)::int AS n FROM web.bundle
       WHERE workspace_id = $1 AND kind = 'mcp' AND status = 'active'
         AND id IN ('s_race_a', 's_race_b')`,
      [wsId],
    );
    expect(Number(rows[0]?.n)).toBe(1);
  });

  it("unarchiving an mcp bundle re-runs the name scan", async () => {
    const PIER = { ...WEATHER, name: "io.github.acme/pier" };
    const owner = asOwner(wsId, "u_auth", "Author");
    const { archiveBundle, unarchiveBundle } = await import("@/lib/db/queries.lifecycle.server");
    // A published server, archived — which frees BOTH its names: the catalog one and the
    // registry name its document embeds.
    await seedPublishedServer("s_pier_a", "pier", PIER);
    expect((await archiveBundle(owner, "s_pier_a")).outcome).toBe("archived");
    // Someone else publishes that registry name in the meantime, under a catalog name of their
    // own — so the base-name check has nothing to say and only the embedded one can refuse.
    await seedPublishedServer("s_pier_b", "pier-two", PIER);
    expect(await unarchiveBundle(owner, "s_pier_a")).toEqual({ outcome: "mcp_name_taken" });

    // A plain skill's unarchive never asks any of this — it has no second name to claim.
    await seedBundle(db, wsId, "s_plain_arch", "plain-arch");
    expect((await archiveBundle(owner, "s_plain_arch")).outcome).toBe("archived");
    expect(await unarchiveBundle(owner, "s_plain_arch")).toEqual({
      outcome: "unarchived",
      name: "plain-arch",
    });
  });

  it("an mcp bundle with no published current unarchives — it claims no registry name", async () => {
    const owner = asOwner(wsId, "u_auth", "Author");
    const { archiveBundle, unarchiveBundle } = await import("@/lib/db/queries.lifecycle.server");
    // Born but never published: no `current`, so nothing is served for it and no registry name
    // is claimed. A refusal here would strand it in the archive over a name it never held.
    await seedBundle(db, wsId, "s_unpub", "unpub-server", { kind: "mcp", withPointer: false });
    expect((await archiveBundle(owner, "s_unpub")).outcome).toBe("archived");
    expect(await unarchiveBundle(owner, "s_unpub")).toEqual({
      outcome: "unarchived",
      name: "unpub-server",
    });
  });

  it("an mcp bundle whose document can't be read refuses as unreadable, not as a taken name", async () => {
    const owner = asOwner(wsId, "u_auth", "Author");
    const { archiveBundle, unarchiveBundle } = await import("@/lib/db/queries.lifecycle.server");
    // A live `current` the vault holds no bytes for: the tier cannot establish what would be
    // served. That fails closed like a collision does, but it is a DIFFERENT fact, and the
    // outcome has to say which one it is — a name nobody else claims is not "taken".
    await seedBundle(db, wsId, "s_dark", "dark-server", { kind: "mcp" });
    expect((await archiveBundle(owner, "s_dark")).outcome).toBe("archived");
    expect(await unarchiveBundle(owner, "s_dark")).toEqual({ outcome: "mcp_document_unreadable" });
  });
});

/**
 * ITEM PAIR (deny before custody, the re-publish arm): a RE-publish whose document claims a name
 * another bundle here already holds refuses at the PRE-check — before `publishVersion`, so its
 * `current` never moves against a name the workspace then denies it. The claim stays
 * the authority for the narrowed race window (see the named residual in publish-flow); this pins
 * the common case: no custody call at all on a provable collision.
 */
describe("the pre-check refuses a re-publish before custody moves", () => {
  it("a re-publish claiming another bundle's name is DENIED with zero custody calls", async () => {
    const KEEP = { ...WEATHER, name: "io.github.acme/keeper" };
    const MOVE = { ...WEATHER, name: "io.github.acme/mover" };
    await seedPublishedServer("s_keeper", "keeper", KEEP);
    await seedPublishedServer("s_mover", "mover", MOVE);

    // s_mover renames itself into s_keeper's embedded name.
    const envelope = await runFlow({
      skillId: "s_mover",
      kind: "mcp",
      expected: 1,
      files: [serverJsonFile({ ...KEEP, version: "2.0.0" })],
    });
    expect(errorOf(envelope)?.code).toBe("MCP_NAME_TAKEN");
    expect(errorOf(envelope)?.context.message).toContain("keeper");
    // The refusal preceded custody: the pointer never moved, no bytes were ingested.
    expect(vault.calls).toEqual([]);
  });
});

/**
 * THE CLAIM IS THE REFUSAL — the properties the identity row has to have, proven behaviorally
 * against real Postgres rather than by reading the SQL.
 *
 * Uniqueness used to be a per-workspace advisory lock around a capped scan of every published
 * document; it is now one row whose key admits a single claimant. Two things must hold for that
 * swap to be safe: a claim must be ATOMIC WITH the transaction that made it true (an uncommitted
 * claim already excludes a rival, and a rollback frees the name), and a claim must never RAISE —
 * a duplicate is an answer a person reads, so a unique-violation would take down the ceremony
 * carrying it instead of refusing.
 */
describe("the identity claim", () => {
  it("excludes a rival while uncommitted, and frees the name when the transaction rolls back", async () => {
    const { claimBundleIdentityInTx, identityHolder } = await import(
      "@/lib/db/bundle-identity.server"
    );
    const { getDb } = await import("@/lib/db/index.server");
    const { sql } = await import("drizzle-orm");
    const NAME = "io.github.acme/ghost";

    await expect(
      getDb().transaction(async (tx) => {
        await tx.execute(sql`
          INSERT INTO web.bundle (id, workspace_id, name, kind, status)
          VALUES ('s_ghost', ${wsId}, 'ghost', 'mcp', 'active')`);
        await tx.execute(sql`
          INSERT INTO web.bundle (id, workspace_id, name, kind, status)
          VALUES ('s_rival', ${wsId}, 'rival', 'mcp', 'active')`);
        const first = await claimBundleIdentityInTx(tx, wsId, "s_ghost", "mcp", NAME);
        expect(first.ok).toBe(true);
        // The rival asks for the same name on the same (uncommitted) transaction: refused, and
        // told WHOSE it is — and, crucially, the transaction is still usable afterwards.
        const second = await claimBundleIdentityInTx(tx, wsId, "s_rival", "mcp", NAME);
        expect(second.ok).toBe(false);
        expect(second.ok === false && second.heldBy?.name).toBe("ghost");
        // Re-claiming your OWN name is not a collision with yourself.
        expect((await claimBundleIdentityInTx(tx, wsId, "s_ghost", "mcp", NAME)).ok).toBe(true);
        throw new Error("rollback — leave no trace");
      }),
    ).rejects.toThrow("rollback");

    // Rolled back, so the name was never taken: nothing to clean up, nothing left claimed.
    expect(await identityHolder(wsId, "mcp", NAME, null)).toBeNull();
  });

  it("moves a bundle's claim when it renames itself, leaving the old name free", async () => {
    const { claimBundleIdentityInTx, identityHolder } = await import(
      "@/lib/db/bundle-identity.server"
    );
    const { getDb } = await import("@/lib/db/index.server");
    const { sql } = await import("drizzle-orm");
    const OLD = "io.github.acme/before";
    const NEW = "io.github.acme/after";
    await getDb().transaction(async (tx) => {
      await tx.execute(sql`
        INSERT INTO web.bundle (id, workspace_id, name, kind, status)
        VALUES ('s_rename', ${wsId}, 'rename', 'mcp', 'active')`);
      await claimBundleIdentityInTx(tx, wsId, "s_rename", "mcp", OLD);
      await claimBundleIdentityInTx(tx, wsId, "s_rename", "mcp", NEW);
    });
    expect((await identityHolder(wsId, "mcp", NEW, null))?.name).toBe("rename");
    // A re-publish that renames the server RELEASES the name it used to answer to.
    expect(await identityHolder(wsId, "mcp", OLD, null)).toBeNull();
  });

  it("a re-publish's final transaction completes with every other pool client held", async () => {
    const { getPool } = await import("@/lib/db/index.server");
    const RESERVE = { ...WEATHER, name: "io.github.acme/reserve" };
    await seedPublishedServer("s_reserve", "reserve", RESERVE);

    const pool = getPool();
    const max = (pool as unknown as { options: { max?: number } }).options.max ?? 10;
    // Hold every client but ONE — the final transaction takes that one, and everything it does
    // must ride it. A second checkout from inside that transaction could never come.
    const held = await Promise.all(Array.from({ length: max - 1 }, () => pool.connect()));
    try {
      const envelope = await runFlow({
        skillId: "s_reserve",
        kind: "mcp",
        expected: 1,
        files: [serverJsonFile({ ...RESERVE, version: "1.0.1" })],
      });
      expect(envelope.ok).toBe(true);
    } finally {
      for (const client of held) {
        client.release();
      }
    }
  }, 20_000);
});

/**
 * THE TWO DOORS THAT MOVE A POINTER WITHOUT PUBLISHING BYTES: approving a proposal, and
 * reverting. Both make the version they land on this bundle's `current`, so both make the
 * registry name INSIDE that version this workspace's — the very claim publish, re-publish and
 * unarchive each refuse to grant twice. Without the check the sequence is: propose a rename onto
 * a free name · someone else publishes and claims it · approve the proposal — and the workspace
 * ends up with two active mcp bundles embedding one name, which makes the registry lane's
 * `…/servers/{name}` a coin flip.
 *
 * Driven through the REAL served actions with a reviewer's bearer, against the scratch database
 * and the stub vault: a denial that never reached a custody route is proven by an empty recorder.
 */
describe("promoting a version claims its embedded name", () => {
  const REVIEWER_BEARER = "dk_reviewer";
  const ORIGIN = "http://x";

  async function reviewsPost(body: Record<string, unknown>): Promise<Record<string, unknown>> {
    const { action } = await import("@/routes/api.v1.reviews");
    const res = await action({
      request: new Request(`${ORIGIN}/api/v1/reviews`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${REVIEWER_BEARER}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({ workspace_id: wsId, op_id: opId(), ...body }),
      }),
      params: {},
      context: {},
    } as unknown as Parameters<typeof action>[0]);
    return (await res.json()) as Record<string, unknown>;
  }

  async function revertsPost(body: Record<string, unknown>): Promise<Record<string, unknown>> {
    const { action } = await import("@/routes/api.v1.reverts");
    const res = await action({
      request: new Request(`${ORIGIN}/api/v1/reverts`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${REVIEWER_BEARER}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({
          workspace_id: wsId,
          op_id: opId(),
          expected: 1,
          author: "Reviewer <r@b.test>",
          message: "back to the good one",
          ...body,
        }),
      }),
      params: {},
      context: {},
    } as unknown as Parameters<typeof action>[0]);
    return (await res.json()) as Record<string, unknown>;
  }

  /** A candidate version standing in the vault, pointed at by nothing — a proposal's bytes. */
  function seedCandidate(bundleId: string, versionId: string, document: unknown): void {
    vault.seed(wsId, bundleId, versionId, [
      { path: "server.json", content: JSON.stringify(document, null, 2) },
    ]);
  }

  async function openProposal(
    id: string,
    bundleId: string,
    candidateVersionId: string,
  ): Promise<void> {
    await db.q(
      `INSERT INTO web.proposal (id, workspace_id, bundle_id, candidate_version_id, proposed_by, status)
       VALUES ($1, $2, $3, $4, 'u_rev', 'open')`,
      [id, wsId, bundleId, candidateVersionId],
    );
  }

  beforeAll(async () => {
    await seedUser(db, "u_rev", "Reviewer", "reviewer@example.com");
    await seatUser(db, wsId, "u_rev", "reviewer");
    await seedSession(db, REVIEWER_BEARER, wsId, "u_rev");
  });

  it("refuses an approve whose candidate renames the server onto a taken name", async () => {
    const HELD = { ...WEATHER, name: "io.github.acme/harbour" };
    const RENAMER = { ...WEATHER, name: "io.github.acme/estuary" };
    // The name the candidate would claim is already another active bundle's.
    await seedPublishedServer("s_harbour", "harbour", HELD);
    await seedPublishedServer("s_renamer", "estuary", RENAMER);
    const candidate = "1a".repeat(32);
    seedCandidate("s_renamer", candidate, { ...HELD, version: "9.9.9" });
    await openProposal("p_rename", "s_renamer", candidate);

    const envelope = await reviewsPost({
      skill_id: "s_renamer",
      expected: 1,
      proposal: candidate,
      decision: "approve",
    });
    expect(envelope.ok).toBe(false);
    expect(errorOf(envelope)?.code).toBe("MCP_NAME_TAKEN");
    expect(errorOf(envelope)?.context.message).toContain("harbour");
    // The pointer never moved — the refusal preceded the custody call, and the proposal stands.
    expect(vault.calls).toEqual([]);
    const rows = await db.q<{ status: string }>(`SELECT status FROM web.proposal WHERE id = $1`, [
      "p_rename",
    ]);
    expect(rows[0]?.status).toBe("open");
  });

  it("approves a candidate whose embedded name is free", async () => {
    const FREE = { ...WEATHER, name: "io.github.acme/lagoon" };
    await seedPublishedServer("s_lagoon", "lagoon", FREE);
    const candidate = "2b".repeat(32);
    seedCandidate("s_lagoon", candidate, { ...FREE, version: "1.2.0" });
    await openProposal("p_lagoon", "s_lagoon", candidate);

    const envelope = await reviewsPost({
      skill_id: "s_lagoon",
      expected: 1,
      proposal: candidate,
      decision: "approve",
    });
    expect(envelope.ok).toBe(true);
    expect(vault.calls).toEqual([{ route: "pointer", ws: wsId, bundle: "s_lagoon" }]);
    const rows = await db.q<{ status: string }>(`SELECT status FROM web.proposal WHERE id = $1`, [
      "p_lagoon",
    ]);
    expect(rows[0]?.status).toBe("approved");
  });

  it("refuses a revert that would carry a taken name forward", async () => {
    const HELD = { ...WEATHER, name: "io.github.acme/jetty" };
    const NOW = { ...WEATHER, name: "io.github.acme/slipway" };
    await seedPublishedServer("s_jetty", "jetty", HELD);
    await seedPublishedServer("s_slipway", "slipway", NOW);
    // The good version predates the rename: reverting to it re-claims s_jetty's name.
    const good = "3c".repeat(32);
    seedCandidate("s_slipway", good, { ...HELD, version: "0.9.0" });

    const envelope = await revertsPost({ skill_id: "s_slipway", good });
    expect(envelope.ok).toBe(false);
    expect(errorOf(envelope)?.code).toBe("MCP_NAME_TAKEN");
    expect(errorOf(envelope)?.context.message).toContain("jetty");
    expect(vault.calls).toEqual([]);
  });

  it("reverts to a good version whose embedded name is this bundle's own", async () => {
    const OWN = { ...WEATHER, name: "io.github.acme/quay" };
    await seedPublishedServer("s_quay", "quay", OWN);
    const good = "4d".repeat(32);
    seedCandidate("s_quay", good, { ...OWN, version: "0.9.0" });

    const envelope = await revertsPost({ skill_id: "s_quay", good });
    expect(envelope.ok).toBe(true);
    expect(vault.calls).toEqual([{ route: "revert", ws: wsId, bundle: "s_quay" }]);
  });

  it("still answers UNKNOWN_VERSION when the vault holds no such candidate", async () => {
    // The name check must not swallow the vault's own answer: a candidate that does not exist
    // cannot become `current`, so the move gets to say so in its own words.
    const GONE = { ...WEATHER, name: "io.github.acme/sandbar" };
    await seedPublishedServer("s_sandbar", "sandbar", GONE);
    const candidate = "6f".repeat(32);
    await openProposal("p_sandbar", "s_sandbar", candidate);

    const envelope = await reviewsPost({
      skill_id: "s_sandbar",
      expected: 1,
      proposal: candidate,
      decision: "approve",
    });
    expect(errorOf(envelope)?.code).toBe("UNKNOWN_VERSION");
  });

  it("leaves a plain skill's approve exactly as it was", async () => {
    await seedBundle(db, wsId, "s_skill_prop", "skill-prop");
    const candidate = "5e".repeat(32);
    vault.seed(wsId, "s_skill_prop", candidate, [{ path: "SKILL.md", content: "# v2" }]);
    await openProposal("p_skill", "s_skill_prop", candidate);

    const envelope = await reviewsPost({
      skill_id: "s_skill_prop",
      expected: 1,
      proposal: candidate,
      decision: "approve",
    });
    expect(envelope.ok).toBe(true);
    expect(vault.calls).toEqual([{ route: "pointer", ws: wsId, bundle: "s_skill_prop" }]);
  });
});

describe("every other kind is untouched", () => {
  it("a skill publish carrying no server.json lands exactly as before", async () => {
    const envelope = await runFlow({
      skillId: "s_plain_skill",
      files: [
        {
          path: "SKILL.md",
          mode: "100644",
          content_base64: Buffer.from("# hi").toString("base64"),
        },
      ],
    });
    expect(envelope.ok).toBe(true);
    expect(vault.calls).toEqual([{ route: "publish", ws: wsId, bundle: "s_plain_skill" }]);
  });

  it("a skill publish that happens to carry a server.json is not gated by MCP rules", async () => {
    const envelope = await runFlow({
      skillId: "s_skill_with_json",
      files: [serverJsonFile({ anything: "at all" })],
    });
    expect(envelope.ok).toBe(true);
  });
});
