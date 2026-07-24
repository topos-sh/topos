import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asMember,
  asOwner,
  bootWorkspace,
  createScratchDb,
  placeInDefault,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedUser,
  versionIdFor,
} from "./helpers/scratch-db";

/**
 * The bundle LIFECYCLE DAL (queries.lifecycle.server.ts) + the name resolve
 * (resolve.server.ts) against a REAL scratch Postgres. Archive renames to free the base name
 * (recording it so unarchive restores EXACTLY), unplaces from every channel, and auto-closes
 * open proposals as 'withdrawn' with author notices; delete requires archive-first and keeps a
 * tombstone; rename leaves a resolving hint. The BYTE halves (delete's custody drop, purge)
 * call the vault over HTTP — unreachable in this environment BY DESIGN, so the suite asserts
 * the code's own honest degradation: delete answers `bytesDropped: false` with the tombstone
 * standing, purge answers `{ outcome: "fault" }`.
 */

let db: ScratchDb;
let wsId = "";
let archivedMainName = "";

async function q() {
  return import("@/lib/db/queries.lifecycle.server");
}

const owner = () => asOwner(wsId, "u_owner", "Owner");

beforeAll(async () => {
  db = await createScratchDb("web_lifecycle");
  wsId = await bootWorkspace();
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seedUser(db, "u_prop", "Proposer", "prop@example.com");
  await seatUser(db, wsId, "u_owner", "owner");
  await seatUser(db, wsId, "u_prop", "member");
  // The main bundle: placed in the default channel, one OPEN proposal awaiting review.
  await seedBundle(db, wsId, "s_main", "runbook");
  await placeInDefault(db, wsId, "s_main");
  await db.q(
    `INSERT INTO web.proposal (id, workspace_id, bundle_id, candidate_version_id, proposed_by, status)
     VALUES ('p_open', $1, 's_main', $2, 'u_prop', 'open')`,
    [wsId, "ab".repeat(32)],
  );
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("archiveBundle", () => {
  it("renames to -archived-<date>, frees the base name, unplaces, and closes open proposals with notices", async () => {
    const queries = await q();
    const outcome = await queries.archiveBundle(owner(), "s_main");
    if (outcome.outcome !== "archived") {
      throw new Error(`expected archived, got ${outcome.outcome}`);
    }
    archivedMainName = outcome.archivedName;
    expect(archivedMainName).toMatch(/^runbook-archived-\d{4}-\d{2}-\d{2}$/);

    const rows = await db.q<{ name: string; base_name: string; status: string }>(
      `SELECT name, base_name, status FROM web.bundle WHERE id = 's_main'`,
    );
    expect(rows).toEqual([{ name: archivedMainName, base_name: "runbook", status: "archived" }]);
    // Unplaced from EVERY channel — an upstream withdrawal.
    expect(await db.q(`SELECT 1 FROM web.channel_bundle WHERE bundle_id = 's_main'`)).toHaveLength(
      0,
    );
    // The open proposal auto-closed as 'withdrawn' (the no-verdict terminal), reason carried.
    const proposals = await db.q<{ status: string; resolved_reason: string; resolved_by: string }>(
      `SELECT status, resolved_reason, resolved_by FROM web.proposal WHERE id = 'p_open'`,
    );
    expect(proposals).toEqual([
      { status: "withdrawn", resolved_reason: "skill archived", resolved_by: "u_owner" },
    ]);
    // The author got the proposal_closed notice with the display snapshot payload.
    const notices = await db.q<{ kind: string; payload: Record<string, unknown> }>(
      `SELECT kind, payload FROM web.notice WHERE user_id = 'u_prop'`,
    );
    expect(notices).toHaveLength(1);
    expect(notices[0]?.kind).toBe("proposal_closed");
    expect(notices[0]?.payload).toMatchObject({
      skill_id: "s_main",
      version_id: "ab".repeat(32),
      actor: "Owner",
      outcome: "closed",
      reason: "skill archived",
    });
    const audit = await db.q(
      `SELECT 1 FROM web.audit_event WHERE kind = 'skill_archived' AND subject = 's_main'`,
    );
    expect(audit).toHaveLength(1);
  });

  it("refuses a non-active bundle and an unknown id, typed", async () => {
    const queries = await q();
    expect(await queries.archiveBundle(owner(), "s_main")).toEqual({ outcome: "not_active" });
    expect(await queries.archiveBundle(owner(), "s_nope")).toEqual({ outcome: "unknown_skill" });
  });

  it("a same-day, same-name repeat takes the counter suffix — names stay unique across every status", async () => {
    const queries = await q();
    await seedBundle(db, wsId, "s_dup1", "dup");
    const first = await queries.archiveBundle(owner(), "s_dup1");
    if (first.outcome !== "archived") {
      throw new Error("first dup archive failed");
    }
    // The base name is FREE again — a new bundle claims it, then archives the same day.
    await seedBundle(db, wsId, "s_dup2", "dup");
    const second = await queries.archiveBundle(owner(), "s_dup2");
    if (second.outcome !== "archived") {
      throw new Error("second dup archive failed");
    }
    expect(second.archivedName).toBe(`${first.archivedName}-2`);
  });

  it("archivedSkillsOf lists the retired identities, base names recorded", async () => {
    const queries = await q();
    const rows = await queries.archivedSkillsOf(asMember(wsId, "u_prop"));
    expect(rows.map((r) => [r.skillId, r.baseName])).toEqual([
      ["s_dup1", "dup"],
      ["s_dup2", "dup"],
      ["s_main", "runbook"],
    ]);
    expect(rows.every((r) => typeof r.archivedAtMs === "number")).toBe(true);
  });
});

describe("unarchiveBundle", () => {
  it("restores the base name EXACTLY; a reused name is the typed name_taken", async () => {
    const queries = await q();
    expect(await queries.unarchiveBundle(owner(), "s_dup1")).toEqual({
      outcome: "unarchived",
      name: "dup",
    });
    const rows = await db.q<{ name: string; base_name: string | null; status: string }>(
      `SELECT name, base_name, status FROM web.bundle WHERE id = 's_dup1'`,
    );
    expect(rows).toEqual([{ name: "dup", base_name: null, status: "active" }]);
    // s_dup2's base name is now occupied by the restored s_dup1.
    expect(await queries.unarchiveBundle(owner(), "s_dup2")).toEqual({ outcome: "name_taken" });
  });

  it("refuses a non-archived bundle and an unknown id, typed", async () => {
    const queries = await q();
    expect(await queries.unarchiveBundle(owner(), "s_dup1")).toEqual({ outcome: "not_archived" });
    expect(await queries.unarchiveBundle(owner(), "s_nope")).toEqual({ outcome: "unknown_skill" });
  });
});

describe("deleteBundle (archive-first; the byte half degrades honestly without a vault)", () => {
  it("refuses an active bundle and an unknown id", async () => {
    const queries = await q();
    expect(await queries.deleteBundle(owner(), "s_dup1")).toEqual({ outcome: "not_archived" });
    expect(await queries.deleteBundle(owner(), "s_nope")).toEqual({ outcome: "unknown_skill" });
  });

  it("tombstones an archived bundle; the unreachable vault means bytesDropped: false, stated plainly", async () => {
    const queries = await q();
    // No vault runs in this environment: deleteBundleBytes catches the network fault and
    // answers false — the row tombstone stands either way and the outcome says so.
    expect(await queries.deleteBundle(owner(), "s_main")).toEqual({
      outcome: "deleted",
      bytesDropped: false,
    });
    const rows = await db.q<{ name: string; status: string; has_deleted_at: boolean }>(
      `SELECT name, status, deleted_at IS NOT NULL AS has_deleted_at FROM web.bundle WHERE id = 's_main'`,
    );
    expect(rows).toEqual([{ name: archivedMainName, status: "deleted", has_deleted_at: true }]);
    const audit = await db.q(
      `SELECT 1 FROM web.audit_event WHERE kind = 'skill_deleted' AND subject = 's_main'`,
    );
    expect(audit).toHaveLength(1);
  });
});

describe("purgeVersion (vault-first; no vault here)", () => {
  it("answers { outcome: 'fault' } when the custody call cannot reach a vault — never a fake purge", async () => {
    const queries = await q();
    expect(await queries.purgeVersion(owner(), "s_dup1", versionIdFor("s_dup1"))).toEqual({
      outcome: "fault",
    });
    // Nothing was closed or audited: the byte half never confirmed.
    expect(await db.q(`SELECT 1 FROM web.audit_event WHERE kind = 'version_purged'`)).toHaveLength(
      0,
    );
  });
});

describe("renameBundle + resolveSkillName (live-then-hint)", () => {
  it("renames id-keyed, leaving the old name as a resolving hint; the redirect case reads via 'hint'", async () => {
    const queries = await q();
    const { resolveSkillName } = await import("@/lib/db/resolve.server");
    await seedBundle(db, wsId, "s_ren", "old-name");
    expect(await queries.renameBundle(owner(), "s_ren", "new-name")).toEqual({
      outcome: "renamed",
      name: "new-name",
    });
    const hints = await db.q<{ old_name: string; bundle_id: string }>(
      `SELECT old_name, bundle_id FROM web.bundle_name_hint WHERE workspace_id = $1`,
      [wsId],
    );
    expect(hints).toEqual([{ old_name: "old-name", bundle_id: "s_ren" }]);

    // The live name answers via 'name'; the old one via 'hint' onto the LIVE identity — the
    // redirect case: an active bundle reached through a hint whose asked name differs.
    expect(await resolveSkillName(asMember(wsId, "u_prop"), "new-name")).toEqual({
      skillId: "s_ren",
      name: "new-name",
      status: "active",
      via: "name",
    });
    expect(await resolveSkillName(asMember(wsId, "u_prop"), "old-name")).toEqual({
      skillId: "s_ren",
      name: "new-name",
      status: "active",
      via: "hint",
    });
    expect(await resolveSkillName(asMember(wsId, "u_prop"), "never-named")).toBeUndefined();
  });

  it("refuses the bad names, a taken name, a non-active target, and an unknown id, typed", async () => {
    const queries = await q();
    expect(await queries.renameBundle(owner(), "s_ren", "Has_Upper")).toEqual({
      outcome: "bad_name",
    });
    // The -archived- namespace belongs to the archive rename.
    expect(await queries.renameBundle(owner(), "s_ren", "x-archived-2026-01-01")).toEqual({
      outcome: "bad_name",
    });
    expect(await queries.renameBundle(owner(), "s_ren", "dup")).toEqual({ outcome: "name_taken" });
    expect(await queries.renameBundle(owner(), "s_dup2", "fresh-name")).toEqual({
      outcome: "not_active",
    });
    expect(await queries.renameBundle(owner(), "s_nope", "fresh-name")).toEqual({
      outcome: "unknown_skill",
    });
    // A no-op rename to the CURRENT name succeeds without minting a self-hint.
    expect(await queries.renameBundle(owner(), "s_ren", "new-name")).toEqual({
      outcome: "renamed",
      name: "new-name",
    });
    expect(
      await db.q(`SELECT 1 FROM web.bundle_name_hint WHERE old_name = 'new-name'`),
    ).toHaveLength(0);
  });

  it("claiming a hinted name clears the squatting hint — the new identity owns it outright", async () => {
    const queries = await q();
    const { resolveSkillName } = await import("@/lib/db/resolve.server");
    // s_ren moves on; 'new-name' becomes a hint pointing at s_ren.
    expect(await queries.renameBundle(owner(), "s_ren", "third-name")).toEqual({
      outcome: "renamed",
      name: "third-name",
    });
    expect((await resolveSkillName(asMember(wsId, "u_prop"), "new-name"))?.via).toBe("hint");
    // A different bundle claims 'new-name': the squatting hint dies with the claim.
    await seedBundle(db, wsId, "s_sq", "squat");
    expect(await queries.renameBundle(owner(), "s_sq", "new-name")).toEqual({
      outcome: "renamed",
      name: "new-name",
    });
    expect(await resolveSkillName(asMember(wsId, "u_prop"), "new-name")).toEqual({
      skillId: "s_sq",
      name: "new-name",
      status: "active",
      via: "name",
    });
  });
});
