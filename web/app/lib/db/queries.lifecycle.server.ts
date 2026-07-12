import { and, asc, eq } from "drizzle-orm";
import type { MemberActor } from "@/lib/auth/guards.server";
import { getDb } from "@/lib/db/index.server";
import { planeCatalog } from "@/lib/db/schema.plane";

/**
 * The lifecycle DATA ACCESS LAYER — the archived-skills read the /workspaces/:ws/archive page
 * renders. Like every DAL function it REQUIRES the actor whose authority it exercises (actors are
 * mintable only by the guards), and its scope is the actor's OWN workspace — a wrong-scope actor
 * fails loudly rather than leaking. The write path (archive/unarchive/delete/purge/rename) rides
 * the vault's internal lane, never a table write here: this tier holds no DML on the catalog.
 */

/** One archived catalog row — the identity retired but still addressable in history/lifecycle. */
export interface ArchivedSkillRow {
  /** The immutable custody key — what every vault lifecycle call keys on. */
  skillId: string;
  /** The current (archived) catalog name — `<base>-archived-<date>`; the delete confirm types THIS. */
  name: string;
  /** The freed base name the skill carried before archiving (null on very old rows). */
  baseName: string | null;
  /** BIGINT epoch-milliseconds — `new Date(ms)` at the display edge only (null on old rows). */
  archivedAtMs: number | null;
}

/**
 * The workspace's archived skills, name order. `status = 'archived'` only: active identities are
 * the catalog surface, deleted ones are tombstones no page lists. A member may read this list (the
 * page is member-visible); the per-row owner actions re-guard on their own.
 */
export async function archivedSkillsOf(actor: MemberActor): Promise<ArchivedSkillRow[]> {
  return getDb()
    .select({
      skillId: planeCatalog.skillId,
      name: planeCatalog.name,
      baseName: planeCatalog.baseName,
      archivedAtMs: planeCatalog.archivedAt,
    })
    .from(planeCatalog)
    .where(
      and(eq(planeCatalog.workspaceId, actor.workspaceId), eq(planeCatalog.status, "archived")),
    )
    .orderBy(asc(planeCatalog.name));
}

/** One archived row by its immutable skill id — the action's re-read (the typed-name anchor). */
export async function archivedSkillById(
  actor: MemberActor,
  skillId: string,
): Promise<ArchivedSkillRow | undefined> {
  const rows = await getDb()
    .select({
      skillId: planeCatalog.skillId,
      name: planeCatalog.name,
      baseName: planeCatalog.baseName,
      archivedAtMs: planeCatalog.archivedAt,
    })
    .from(planeCatalog)
    .where(
      and(
        eq(planeCatalog.workspaceId, actor.workspaceId),
        eq(planeCatalog.skillId, skillId),
        eq(planeCatalog.status, "archived"),
      ),
    )
    .limit(1);
  return rows[0];
}
