import { and, eq } from "drizzle-orm";
import type { MemberActor } from "@/lib/auth/guards.server";
import { getDb } from "@/lib/db/index.server";
import { bundle, bundleNameHint } from "@/lib/db/schema.app";

/**
 * Bundle-name resolution: the live catalog first, the rename hints second — so an old name in
 * a bookmark or a doc keeps resolving until someone claims it for a new identity. `via` says
 * which arm answered; a 'hint' hit on an active bundle is the redirect case (send the browser
 * to the live name). One implementation, used by every tier of this app.
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
  const ws = actor.workspaceId;
  const db = getDb();
  const live = await db
    .select({ skillId: bundle.id, name: bundle.name, status: bundle.status })
    .from(bundle)
    .where(and(eq(bundle.workspaceId, ws), eq(bundle.name, name)))
    .limit(1);
  if (live[0] !== undefined) {
    return {
      skillId: live[0].skillId,
      name: live[0].name,
      status: live[0].status as ResolvedSkillName["status"],
      via: "name",
    };
  }
  const hinted = await db
    .select({ skillId: bundle.id, name: bundle.name, status: bundle.status })
    .from(bundleNameHint)
    .innerJoin(
      bundle,
      and(
        eq(bundle.workspaceId, bundleNameHint.workspaceId),
        eq(bundle.id, bundleNameHint.bundleId),
      ),
    )
    .where(and(eq(bundleNameHint.workspaceId, ws), eq(bundleNameHint.oldName, name)))
    .limit(1);
  if (hinted[0] !== undefined) {
    return {
      skillId: hinted[0].skillId,
      name: hinted[0].name,
      status: hinted[0].status as ResolvedSkillName["status"],
      via: "hint",
    };
  }
  return undefined;
}
