import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asMember,
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

/**
 * The applied report's PER-HARNESS block. A config-placed ('mcp') bundle does not land as one
 * folder on disk — it lands as an entry in each detected agent's config — so one applied
 * version no longer says everything the fleet needs: the session reports, per harness, which
 * agents hold the entry and how. The block is the CLIENT'S word (an open state vocabulary),
 * shape-checked at the door and stored verbatim as jsonb; a file bundle reports none and the
 * column stays NULL.
 *
 * The snapshot semantics are UNCHANGED and re-proven here: a report is the complete picture, so
 * an unnamed bundle's row goes, and a re-report overwrites the block it names.
 */

let db: ScratchDb;
let wsId = "";

const HARNESSES = [
  { slug: "claude-code", state: "current" },
  { slug: "cursor", state: "drifted", note: "the entry names an older command" },
];

/** The stored jsonb for one bundle on the seeded session (undefined = no row). */
async function storedHarnessState(bundleId: string): Promise<unknown> {
  const rows = await db.q<{ harness_state: unknown }>(
    `SELECT harness_state FROM web.session_bundle_state WHERE session_id = 'cs_box' AND bundle_id = $1`,
    [bundleId],
  );
  return rows.length === 0 ? undefined : rows[0]?.harness_state;
}

async function report(
  applied: {
    skillId: string;
    versionId: string;
    harnesses?: { slug: string; state: string; note?: string }[] | null;
  }[],
): Promise<string> {
  const lane = await import("@/lib/db/queries.lane.server");
  return await lane.reportApplied(asSession(wsId, "u_dev", "cs_box", "owner"), applied);
}

beforeAll(async () => {
  db = await createScratchDb("web_harness", { TOPOS_WEB_RATELIMIT: "off" });
  wsId = await bootWorkspace();
  await seedUser(db, "u_dev", "Dev", "dev@example.com");
  await seatUser(db, wsId, "u_dev", "owner");
  await seedSession(db, "cs_box", wsId, "u_dev", "active", "laptop");
  await seedBundle(db, wsId, "s_srv", "srv", { kind: "mcp" });
  await seedBundle(db, wsId, "s_doc", "doc");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("reportApplied — the per-harness block", () => {
  it("persists the reported states verbatim, and leaves a file bundle's column NULL", async () => {
    expect(
      await report([
        { skillId: "s_srv", versionId: versionIdFor("s_srv"), harnesses: HARNESSES },
        { skillId: "s_doc", versionId: versionIdFor("s_doc") },
      ]),
    ).toBe("ok");
    expect(await storedHarnessState("s_srv")).toEqual(HARNESSES);
    expect(await storedHarnessState("s_doc")).toBeNull();
  });

  it("an EMPTY list is stored as NULL — absence and emptiness are the same fact", async () => {
    expect(
      await report([
        { skillId: "s_srv", versionId: versionIdFor("s_srv"), harnesses: [] },
        { skillId: "s_doc", versionId: versionIdFor("s_doc") },
      ]),
    ).toBe("ok");
    expect(await storedHarnessState("s_srv")).toBeNull();
  });

  it("a re-report REPLACES the block — the newest snapshot is the whole truth", async () => {
    await report([{ skillId: "s_srv", versionId: versionIdFor("s_srv"), harnesses: HARNESSES }]);
    const narrowed = [{ slug: "claude-code", state: "current" }];
    await report([{ skillId: "s_srv", versionId: versionIdFor("s_srv"), harnesses: narrowed }]);
    expect(await storedHarnessState("s_srv")).toEqual(narrowed);
  });

  it("the snapshot still DELETES what it no longer names", async () => {
    await report([
      { skillId: "s_srv", versionId: versionIdFor("s_srv"), harnesses: HARNESSES },
      { skillId: "s_doc", versionId: versionIdFor("s_doc") },
    ]);
    expect(await storedHarnessState("s_doc")).toBeNull();
    // s_doc goes unnamed: the machine no longer holds it, so its row goes with the block.
    await report([{ skillId: "s_srv", versionId: versionIdFor("s_srv"), harnesses: HARNESSES }]);
    expect(await storedHarnessState("s_doc")).toBeUndefined();
    expect(await storedHarnessState("s_srv")).toEqual(HARNESSES);
  });
});

describe("the sessions reads carry the block", () => {
  beforeAll(async () => {
    await report([
      { skillId: "s_srv", versionId: versionIdFor("s_srv"), harnesses: HARNESSES },
      { skillId: "s_doc", versionId: versionIdFor("s_doc") },
    ]);
  });

  it("workspaceSessions() — per bundle, the reported states or null", async () => {
    const sessions = await import("@/lib/db/queries.sessions.server");
    const page = await sessions.workspaceSessions(asMember(wsId, "u_dev", "owner"));
    const skills = page.sessions[0]?.skills ?? [];
    expect(skills.find((s) => s.skillId === "s_srv")?.harnesses).toEqual(HARNESSES);
    expect(skills.find((s) => s.skillId === "s_doc")?.harnesses).toBeNull();
  });

  it("yourSessionsApplying() — the same block on the viewer's own machines", async () => {
    const sessions = await import("@/lib/db/queries.sessions.server");
    const actor = asMember(wsId, "u_dev", "owner");
    expect((await sessions.yourSessionsApplying(actor, "s_srv"))[0]?.harnesses).toEqual(HARNESSES);
    expect((await sessions.yourSessionsApplying(actor, "s_doc"))[0]?.harnesses).toBeNull();
  });
});

describe("the report door shape-checks the block — before the credential resolve", () => {
  // The door checks the version spelling too — a REAL 64-hex id, so the harness block is what
  // each case is actually testing.
  const VERSION = "a1".repeat(32);

  async function put(harnesses: unknown): Promise<{ status: number; message?: string }> {
    const route = await import("@/routes/api.v1.report");
    const request = new Request(`http://x/api/v1/workspaces/${wsId}/report`, {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        schema_version: 1,
        applied: [{ skill_id: "s_srv", version_id: VERSION, harnesses }],
      }),
    });
    let res: Response;
    try {
      res = await route.action({ request, params: { ws: wsId } } as never);
    } catch (thrown) {
      if (!(thrown instanceof Response)) {
        throw thrown;
      }
      res = thrown;
    }
    if (res.status !== 400) {
      return { status: res.status };
    }
    const json = (await res.json()) as { error: { context: { message: string } } };
    return { status: 400, message: json.error.context.message };
  }

  it.each([
    ["a non-array", { "claude-code": "current" }, "malformed report entry: harnesses"],
    [
      "more entries than any installation runs",
      Array.from({ length: 13 }, (_, n) => ({ slug: `h-${n}`, state: "current" })),
      "malformed report entry: harnesses",
    ],
    ["a non-object entry", ["claude-code"], "malformed report entry: harnesses"],
    [
      "an uppercase slug",
      [{ slug: "Claude-Code", state: "current" }],
      "malformed report entry: harness slug",
    ],
    ["a missing state", [{ slug: "claude-code" }], "malformed report entry: harness state"],
    [
      "a state with a space",
      [{ slug: "claude-code", state: "not supported" }],
      "malformed report entry: harness state",
    ],
    [
      "a note that is not text at all",
      [{ slug: "claude-code", state: "drifted", note: 12 }],
      "malformed report entry: harness note",
    ],
  ])("%s is a 400 naming the field", async (_label, harnesses, message) => {
    expect(await put(harnesses)).toEqual({ status: 400, message });
  });

  it("a well-shaped block passes the door — the refusal that follows is the guard's, not the parser's", async () => {
    // No credential rides this request, so a shape-valid body reaches `requireSessionActor` and
    // gets its uniform miss: proof the parse let the block through.
    const { status } = await put([{ slug: "claude-code", state: "current" }]);
    expect(status).not.toBe(400);
  });

  /**
   * A NOTE IS DISPLAY TEXT, not evidence — so its LENGTH is the one thing the door trims rather
   * than refuses. A report is the session's COMPLETE per-workspace snapshot, and a note is
   * whatever the harness had to say: one deep path plus an OS error clears 200 characters, the
   * client cannot know it did, and the condition persists — so refusing the PUT would keep the
   * machine out of the fleet picture for exactly as long as something is wrong with it. The
   * snapshot lands; the note says as much of its story as it fits.
   */
  it("an over-long note is stored trimmed — and the snapshot LANDS", async () => {
    const route = await import("@/routes/api.v1.report");
    const note = `${"x".repeat(260)}…`;
    const request = new Request(`http://x/api/v1/workspaces/${wsId}/report`, {
      method: "PUT",
      // The seeded session's credential — this one goes all the way to the row.
      headers: { "content-type": "application/json", authorization: "Bearer cs_box" },
      body: JSON.stringify({
        schema_version: 1,
        applied: [
          {
            skill_id: "s_srv",
            version_id: VERSION,
            harnesses: [{ slug: "cursor", state: "drifted", note }],
          },
        ],
      }),
    });
    const res = await route.action({ request, params: { ws: wsId } } as never);
    expect(res.status).toBe(204);
    expect(await storedHarnessState("s_srv")).toEqual([
      { slug: "cursor", state: "drifted", note: "x".repeat(200) },
    ]);
  });

  it("trims on a code-point boundary — a lone surrogate would not survive the column", async () => {
    // The cap counts UTF-16 code units, so a cut can land inside a surrogate pair; storing half
    // of one is not representable in jsonb, which would turn a long note into a 500.
    const note = `${"y".repeat(199)}😀 tail`;
    const route = await import("@/routes/api.v1.report");
    const request = new Request(`http://x/api/v1/workspaces/${wsId}/report`, {
      method: "PUT",
      headers: { "content-type": "application/json", authorization: "Bearer cs_box" },
      body: JSON.stringify({
        schema_version: 1,
        applied: [
          {
            skill_id: "s_srv",
            version_id: VERSION,
            harnesses: [{ slug: "cursor", state: "drifted", note }],
          },
        ],
      }),
    });
    expect((await route.action({ request, params: { ws: wsId } } as never)).status).toBe(204);
    const stored = (await storedHarnessState("s_srv")) as { note: string }[];
    expect(stored[0]?.note).toBe("y".repeat(199));
  });

  it("the field is OPTIONAL — a bare row still passes the door", async () => {
    const route = await import("@/routes/api.v1.report");
    const request = new Request(`http://x/api/v1/workspaces/${wsId}/report`, {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        schema_version: 1,
        applied: [{ skill_id: "s_srv", version_id: VERSION }],
      }),
    });
    let status = 0;
    try {
      status = (await route.action({ request, params: { ws: wsId } } as never)).status;
    } catch (thrown) {
      status = (thrown as Response).status;
    }
    expect(status).not.toBe(400);
  });
});
