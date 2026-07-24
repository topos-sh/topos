import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asSession,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The `skill-log` decoration must serve the RESOLVER as a person display (the shipped
 * personDisplayLeftSql rule, mirroring how the proposer is decorated) — never the raw `user.id`
 * that `proposal.resolved_by` stores. Direct DAL call (no vault, no credential): `laneLogOf`
 * only reads the actor's workspace scope.
 */

let db: ScratchDb;
let wsId = "";

async function lane() {
  return import("@/lib/db/queries.lane.server");
}

beforeAll(async () => {
  db = await createScratchDb("web_lanelog");
  wsId = await bootWorkspace();
  await seedUser(db, "u_prop", "Pat Proposer", "pat@example.com");
  await seedUser(db, "u_res", "Robin Resolver", "robin@example.com");
  await seatUser(db, wsId, "u_prop", "member");
  await seatUser(db, wsId, "u_res", "reviewer");
  await seedBundle(db, wsId, "s_log", "logbook");
  // One resolved-by-a-live-reviewer proposal and one still-open proposal.
  await db.q(
    `INSERT INTO web.proposal
       (id, workspace_id, bundle_id, candidate_version_id, proposed_by, status, resolved_by, resolved_reason, resolved_at)
     VALUES
       ('p_appr', $1, 's_log', $2, 'u_prop', 'approved', 'u_res', NULL, now()),
       ('p_open', $1, 's_log', $3, 'u_prop', 'open', NULL, NULL, NULL)`,
    [wsId, "b2".repeat(32), "a1".repeat(32)],
  );
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("laneLogOf serves the resolver's DISPLAY, never a raw user id", () => {
  it("resolved → the resolver's display; open → null", async () => {
    const l = await lane();
    const decorated = await l.laneLogOf(asSession(wsId, "u_res", "dk_res", "reviewer"), "s_log");
    expect(decorated).not.toBeNull();
    const byVersion = new Map(
      (decorated?.proposals ?? []).map((p) => [p.versionId, p.resolvedBy] as const),
    );
    // The resolved proposal serves the DISPLAY, not the raw `user.id`.
    expect(byVersion.get("b2".repeat(32))).toBe("Robin Resolver");
    expect(byVersion.get("b2".repeat(32))).not.toBe("u_res");
    // The still-open proposal has no resolver — null (never a raw id, never a stray display).
    expect(byVersion.get("a1".repeat(32))).toBeNull();
  });
});
