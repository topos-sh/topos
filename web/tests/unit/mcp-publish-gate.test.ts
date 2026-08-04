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
      "a local-install server",
      { ...WEATHER, packages: [{ identifier: "x", version: "1" }] },
      "MCP_LOCAL_REFUSED",
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
  const OTHER_VERSION = versionIdFor("s_held");

  beforeAll(async () => {
    // An MCP bundle already published here, its document readable through the vault — the
    // state the uniqueness check actually reads.
    await seedBundle(db, wsId, "s_held", "weather-held", { kind: "mcp", versionId: OTHER_VERSION });
    vault.seed(wsId, "s_held", OTHER_VERSION, [
      { path: "server.json", content: JSON.stringify(WEATHER, null, 2) },
    ]);
  });

  it("a second bundle claiming the same server name is DENIED MCP_NAME_TAKEN", async () => {
    const envelope = await runFlow({
      skillId: "s_dupe",
      kind: "mcp",
      files: [serverJsonFile(WEATHER)],
    });
    expect(errorOf(envelope)?.code).toBe("MCP_NAME_TAKEN");
    // The refusal names the bundle that holds it, so the author can go look at it.
    expect(errorOf(envelope)?.context.message).toContain("weather");
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
      files: [serverJsonFile({ ...WEATHER, version: "1.1.0" })],
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
      files: [
        serverJsonFile({ ...WEATHER, remotes: [{ type: "sse", url: "https://a.example/s" }] }),
      ],
    });
    expect(errorOf(envelope)?.code).toBe("MCP_NO_STREAMABLE_REMOTE");
    expect(vault.calls).toEqual([]);
  });

  it("an archived MCP bundle holds no name (it delivers nothing)", async () => {
    const archivedVersion = versionIdFor("s_gone");
    await seedBundle(db, wsId, "s_gone", "tides-old", {
      kind: "mcp",
      status: "archived",
      versionId: archivedVersion,
    });
    vault.seed(wsId, "s_gone", archivedVersion, [
      { path: "server.json", content: JSON.stringify({ ...WEATHER, name: "io.github.acme/surf" }) },
    ]);
    const envelope = await runFlow({
      skillId: "s_surf",
      kind: "mcp",
      files: [serverJsonFile({ ...WEATHER, name: "io.github.acme/surf" })],
    });
    expect(envelope.ok).toBe(true);
  });
});

/**
 * The name check answers before the custody call, which makes it a PRE-check: by the time a
 * bundle is actually registered, another publish may have taken the name. Every door that
 * registers therefore looks again inside its final transaction, under one per-workspace
 * advisory lock. These two are the arms that lock: the publish flow, and the unarchive that
 * re-claims a name archiving had freed.
 */
describe("the locked re-check", () => {
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
