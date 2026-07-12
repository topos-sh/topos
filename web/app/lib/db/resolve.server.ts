import { count } from "drizzle-orm";
import type { MemberActor } from "@/lib/auth/guards.server";
import { getDb, getPool } from "@/lib/db/index.server";
import { planeWorkspace } from "@/lib/db/schema.plane";

/**
 * Skill-name resolution over the directory's ONE resolver (`topos_resolve_skill`): the live
 * catalog first, the rename hints second — so an old name in a bookmark or a doc keeps
 * resolving until someone claims it for a new identity. `via` says which arm answered; a
 * 'hint' hit on an active skill is the redirect case (send the browser to the live name).
 */
export interface ResolvedSkillName {
  skillId: string;
  /** The LIVE catalog name (differs from the asked name on a hint hit). */
  name: string;
  status: "active" | "archived" | "deleted";
  via: "name" | "hint";
}

export async function resolveSkillName(
  actor: MemberActor,
  name: string,
): Promise<ResolvedSkillName | undefined> {
  const result = await getPool().query<ResolvedSkillName>(
    'select skill_id as "skillId", name, status, via from topos_resolve_skill($1, $2)',
    [actor.workspaceId, name],
  );
  return result.rows[0];
}

/**
 * Whether ANY workspace exists on this plane — the first-run probe, and the ONE deliberately
 * actor-less read in the data layer: it discloses a single boolean about the deployment (is
 * this a virgin plane?), never a row, and the landing page needs it before anyone can hold a
 * seat to mint an actor from. Everything else in the data layer stays actor-first.
 */
export async function hasAnyWorkspace(): Promise<boolean> {
  const rows = await getDb().select({ n: count() }).from(planeWorkspace);
  return (rows[0]?.n ?? 0) > 0;
}
