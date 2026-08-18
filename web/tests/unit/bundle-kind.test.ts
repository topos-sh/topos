import { Buffer } from "node:buffer";
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { laneHeaders } from "./helpers/lane";
import {
  asSession,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedSession,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The catalog `kind` a publish declares. Two rules, both proven here against a REAL scratch
 * Postgres:
 *
 *  · GENESIS-ONLY — a brand-new bundle is born with the declared kind ('skill' when the publish
 *    declares none), on the ONE registration path a direct publish and a forced proposal share
 *    (genesis always lands directly; there is no base to review against).
 *  · A MISMATCH REFUSES BEFORE THE BYTES MOVE — kind is birth metadata, so a publish naming a
 *    different kind than the stored one is a DENIED `KIND_MISMATCH` answered before any custody
 *    call, never a silent accept that would leave the catalog describing something the bundle
 *    is not.
 *
 * The vault is the flow's one non-Postgres reach, so it is stubbed: the stub RECORDS its calls,
 * which is how the "before any custody write" half is proven (a refusal leaves the recorder
 * empty).
 */

const vault = vi.hoisted(() => ({
  publish: [] as { ws: string; bundleId: string }[],
  commit: [] as { ws: string; bundleId: string }[],
  versionId: "e1".repeat(32),
  digest: "d2".repeat(32),
}));

vi.mock("@/lib/plane/custody.server", () => ({
  publishVersion: async (ws: string, bundleId: string) => {
    vault.publish.push({ ws, bundleId });
    return {
      kind: "ok",
      value: {
        version_id: vault.versionId,
        commit_id: vault.versionId,
        bundle_digest: vault.digest,
        deduped: false,
        pointer: {
          version_id: vault.versionId,
          generation: 1,
          moved_at_ms: 0,
          moved_by_display: "author",
          replayed: false,
        },
      },
    };
  },
  commitVersion: async (ws: string, bundleId: string) => {
    vault.commit.push({ ws, bundleId });
    return {
      kind: "ok",
      value: {
        version_id: vault.versionId,
        commit_id: vault.versionId,
        bundle_digest: vault.digest,
        deduped: false,
      },
    };
  },
}));

let db: ScratchDb;
let wsId = "";
let opSeq = 0;

/** A fresh op id per call — the receipt slot is keyed on it (a reuse would replay). */
function opId(): string {
  opSeq += 1;
  return `00000000-0000-4000-8000-${String(opSeq).padStart(12, "0")}`;
}

const CANDIDATE = {
  files: [{ path: "SKILL.md", mode: "100644", content_base64: "aGk=" }],
  parents: [] as string[],
  author: "Author <author@example.com>",
  message: "genesis",
};

/**
 * A candidate an MCP-kind publish can actually carry. Kind is what this suite is about, but a
 * bundle that IS an MCP server also passes the server-document gate (mcp-publish-gate.test.ts
 * owns that rule), so every case whose effective kind is 'mcp' hands over a real document —
 * otherwise these tests would be asserting the kind gate through a document refusal.
 */
/**
 * An MCP-shaped candidate whose embedded registry name is the BUNDLE'S OWN. A workspace admits
 * one bundle per registry name, so cases here that are about the KIND tag — not about naming —
 * must each carry their own, or the second one is refused for a reason this file is not testing.
 */
const mcpCandidateFor = (bundleId: string) => ({
  ...CANDIDATE,
  files: [
    {
      path: "server.json",
      mode: "100644",
      content_base64: Buffer.from(
        JSON.stringify({
          name: `io.github.acme/${bundleId.replaceAll("_", "-")}`,
          description: "A widget server.",
          version: "1.0.0",
          remotes: [{ type: "streamable-http", url: "https://widget.acme.example/mcp" }],
        }),
        "utf8",
      ).toString("base64"),
    },
  ],
});

/** Drive the shared flow the way both doors do, and give back the parsed envelope. */
async function runFlow(args: {
  skillId: string;
  kind?: string | null;
  expected?: number;
  forceProposal?: boolean;
  displayName?: string | null;
  /** Send the MCP-shaped candidate (for a case whose effective kind is 'mcp'). */
  mcpCandidate?: boolean;
}): Promise<Record<string, unknown>> {
  const { publishFlow } = await import("@/lib/api/publish-flow.server");
  const raw = JSON.stringify({ skill_id: args.skillId, op: opSeq });
  const res = await publishFlow({
    actor: asSession(wsId, "u_auth", "cs_auth", "member"),
    raw,
    opId: opId(),
    skillId: args.skillId,
    expected: args.expected ?? 0,
    candidate: args.mcpCandidate === true ? mcpCandidateFor(args.skillId) : CANDIDATE,
    displayName: args.displayName ?? "Widget",
    channel: null,
    kind: args.kind ?? null,
    command: "publish",
    forceProposal: args.forceProposal ?? false,
  });
  return (await res.json()) as Record<string, unknown>;
}

/** The catalog row's kind, or undefined when no row stands. */
async function kindOf(bundleId: string): Promise<string | undefined> {
  const rows = await db.q<{ kind: string }>(`SELECT kind FROM web.bundle WHERE id = $1`, [
    bundleId,
  ]);
  return rows[0]?.kind;
}

beforeAll(async () => {
  db = await createScratchDb("web_kind", { TOPOS_WEB_RATELIMIT: "off" });
  wsId = await bootWorkspace();
  await seedUser(db, "u_auth", "Author", "author@example.com");
  await seatUser(db, wsId, "u_auth", "member");
  await seedSession(db, "cs_auth", wsId, "u_auth");
}, 60000);

afterAll(async () => {
  await db.drop();
});

beforeEach(() => {
  vault.publish.length = 0;
  vault.commit.length = 0;
});

describe("genesis mints the declared kind", () => {
  it("a publish declaring 'mcp' is REFUSED — that kind is not made of files", async () => {
    const envelope = await runFlow({ skillId: "s_born_mcp", kind: "mcp", mcpCandidate: true });
    expect(envelope.ok).toBe(false);
    expect((envelope.error as { code: string }).code).toBe("KIND_HAS_NO_FILES");
    // Refused before any custody call, so no bytes were ingested against a bundle that will
    // never serve them — and no catalog row was written.
    expect(vault.publish).toEqual([]);
    expect(await kindOf("s_born_mcp")).toBeUndefined();
  });

  it("a publish declaring nothing is born 'skill' — the unchanged default", async () => {
    const envelope = await runFlow({ skillId: "s_born_bare" });
    expect(envelope.ok).toBe(true);
    expect(await kindOf("s_born_bare")).toBe("skill");
  });

  it("genesis by PROPOSAL refuses the same kind identically", async () => {
    const envelope = await runFlow({
      skillId: "s_born_prop",
      kind: "mcp",
      forceProposal: true,
      mcpCandidate: true,
    });
    expect((envelope.error as { code: string }).code).toBe("KIND_HAS_NO_FILES");
    expect(vault.commit).toEqual([]);
    expect(vault.publish).toEqual([]);
    expect(await kindOf("s_born_prop")).toBeUndefined();
  });
});

describe("an existing bundle's kind is fixed at birth", () => {
  beforeAll(async () => {
    await seedBundle(db, wsId, "s_std", "std", { kind: "skill" });
    // No pointer on the MCP rows: what they hold is the server-document gate's business
    // (mcp-publish-gate.test.ts), and an unreadable document would refuse these cases for a
    // reason that has nothing to do with kind.
    await seedBundle(db, wsId, "s_srv", "srv", { kind: "mcp", withPointer: false });
    await seedBundle(db, wsId, "s_srv_rev", "srv-rev", {
      kind: "mcp",
      protection: "reviewed",
      withPointer: false,
    });
  });

  it("a DIFFERENT kind is DENIED `KIND_MISMATCH` — and nothing reached the vault", async () => {
    const envelope = await runFlow({ skillId: "s_std", kind: "mcp", expected: 1 });
    expect(envelope.ok).toBe(false);
    expect(envelope.error).toMatchObject({
      code: "KIND_MISMATCH",
      outcome: "DENIED",
      retryable: false,
      affected: { skill: "std" },
    });
    expect(envelope.receipt).toMatchObject({ outcome: "DENIED", skill_id: "s_std" });
    // The whole point of the gate's placement: no ingest, no pointer move, no orphan bytes.
    expect(vault.publish).toEqual([]);
    expect(vault.commit).toEqual([]);
    // The write still landed its op receipt, so the CLI's op-WAL clears on the replay.
    const receipts = await db.q<{ n: string }>(
      `SELECT count(*)::int AS n FROM web.op_receipt WHERE workspace_id = $1`,
      [wsId],
    );
    expect(Number(receipts[0]?.n ?? 0)).toBeGreaterThan(0);
  });

  it("the PROPOSE arm refuses the same way — before the commit-only ingest", async () => {
    const envelope = await runFlow({
      skillId: "s_srv",
      kind: "skill",
      expected: 1,
      forceProposal: true,
    });
    expect((envelope.error as { code: string }).code).toBe("KIND_MISMATCH");
    expect(vault.commit).toEqual([]);
    expect(vault.publish).toEqual([]);
  });

  it("the gate outranks the protection reroute — a 'reviewed' bundle refuses first", async () => {
    const envelope = await runFlow({ skillId: "s_srv_rev", kind: "skill", expected: 1 });
    expect((envelope.error as { code: string }).code).toBe("KIND_MISMATCH");
    expect(vault.commit).toEqual([]);
  });

  it("RE-ASSERTING the stored kind still refuses the BYTES — that kind has none", async () => {
    const envelope = await runFlow({
      skillId: "s_srv",
      kind: "mcp",
      expected: 1,
      mcpCandidate: true,
    });
    expect((envelope.error as { code: string }).code).toBe("KIND_HAS_NO_FILES");
    expect(vault.publish).toEqual([]);
    expect(await kindOf("s_srv")).toBe("mcp");
  });

  it("naming NO kind reads the STORED one, and refuses on it", async () => {
    const envelope = await runFlow({ skillId: "s_srv", expected: 1, mcpCandidate: true });
    expect((envelope.error as { code: string }).code).toBe("KIND_HAS_NO_FILES");
    expect(vault.publish).toEqual([]);
    expect(await kindOf("s_srv")).toBe("mcp");
  });
});

describe("the door refuses an unknown kind — before any custody call", () => {
  /** The one refusal an out-of-vocabulary kind earns, whatever made it out of vocabulary. */
  const REFUSAL = "unknown kind — known kinds: 'skill', 'mcp'";

  /** A publish-family body that is valid but for whatever `kind` the case names. */
  function body(kind: unknown): Record<string, unknown> {
    return {
      workspace_id: wsId,
      skill_id: "s_shape",
      op_id: opId(),
      expected: 0,
      candidate: CANDIDATE,
      ...(kind === undefined ? {} : { kind }),
    };
  }

  async function post(
    route: "publish" | "propose",
    kind: unknown,
  ): Promise<{ status: number; json: Record<string, unknown> }> {
    const mod =
      route === "publish"
        ? await import("@/routes/api.v1.publish")
        : await import("@/routes/api.v1.propose");
    const request = new Request(`http://x/api/v1/${route}`, {
      method: "POST",
      // Authenticated: the door resolves the credential FIRST (auth-before-body), so the kind
      // refusal is a member's 400 — an unauthenticated caller meets only the uniform 404.
      headers: laneHeaders({
        "content-type": "application/json",
        authorization: "Bearer cs_auth",
      }),
      body: JSON.stringify(body(kind)),
    });
    let res: Response;
    try {
      res = await mod.action({ request } as never);
    } catch (thrown) {
      // The guards refuse by THROWING their uniform response (react-router's data throw).
      if (!(thrown instanceof Response)) {
        throw thrown;
      }
      res = thrown;
    }
    return { status: res.status, json: (await res.json()) as Record<string, unknown> };
  }

  it.each([
    // The one that matters: a well-formed slug for a kind NOTHING implements. The door used to
    // wave this through on shape alone, and the catalog would have stored a bundle no machine
    // knows how to deliver.
    ["a kind no client implements", "knowledge"],
    ["a plausible future kind", "agent"],
    ["an uppercase spelling", "MCP"],
    ["a leading digit", "1mcp"],
    ["an underscore", "mcp_server"],
    ["an over-long slug", "m".repeat(33)],
    ["a non-string", 7],
    ["the empty string", ""],
  ])("publish: %s is a 400", async (_label, kind) => {
    const { status, json } = await post("publish", kind);
    expect(status).toBe(400);
    expect((json.error as { context: { message: string } }).context.message).toBe(REFUSAL);
    // The refusal is decided at the door — nothing was ingested.
    expect(vault.publish).toEqual([]);
  });

  it("propose refuses byte-identically", async () => {
    const { status, json } = await post("propose", "knowledge");
    expect(status).toBe(400);
    expect((json.error as { context: { message: string } }).context.message).toBe(REFUSAL);
    expect(vault.commit).toEqual([]);
  });

  it("an explicit null is ABSENT, not malformed (the wire's optional spelling)", async () => {
    // A null passes the parse and the op proceeds all the way to a landed publish — the
    // honest proof that null reads as "no kind declared", never as a malformed value.
    const { status } = await post("publish", null);
    expect(status).toBe(200);
  });
});

describe("the vocabulary is a CHECK in the schema, not only a door", () => {
  /** The catalog's own answer to a write that skipped the door — a bug, a script, a fat finger. */
  async function insertKind(id: string, name: string, kind: string): Promise<unknown> {
    return await db
      .q(`INSERT INTO web.bundle (id, workspace_id, name, kind) VALUES ($1, $2, $3, $4)`, [
        id,
        wsId,
        name,
        kind,
      ])
      .then(() => undefined)
      .catch((e: unknown) => e);
  }

  it("a row naming an unknown kind is refused by bundle_kind_check", async () => {
    const error = await insertKind("s_alien", "alien", "knowledge");
    expect((error as { constraint?: string } | undefined)?.constraint).toBe("bundle_kind_check");
  });

  it("both known kinds land", async () => {
    expect(await insertKind("s_chk_skill", "chk-skill", "skill")).toBeUndefined();
    expect(await insertKind("s_chk_mcp", "chk-mcp", "mcp")).toBeUndefined();
  });
});
