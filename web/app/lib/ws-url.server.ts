import { composition } from "@/composition.server";
import { followBase } from "@/lib/plane/follow-base.server";
import { wsHref } from "@/lib/ws-path";

/**
 * The server-side companions to `ws-path.ts` — the two things a loader/action builds that the
 * client hook can't: an in-app redirect target that respects the deployment's tenancy grammar, and
 * the FULL shareable workspace address the CLI runs `topos follow <address>` on verbatim. Both read
 * `composition.tenancy` so a page never branches on the mode itself.
 */

/**
 * Build an in-app path for a redirect/link server-side. Single tenancy → origin-rooted (the
 * workspace name is ignored); multi → nested under the workspace NAME slug. `sub` is the
 * in-workspace path (no leading slash); omit it for the workspace root.
 */
export function wsPathServer(workspaceName: string, sub?: string): string {
  return wsHref(composition.tenancy === "multi" ? workspaceName : null, sub);
}

/**
 * The full shareable workspace address — what sharing/joining speak and the CLI follows verbatim.
 * Single tenancy → the BARE ORIGIN (the install IS the one workspace); multi → `<origin>/<name>`.
 * The origin resolution matches `followBase` exactly, so the address matches the printed setup line
 * and the protocol card's base.
 */
export function workspaceAddress(request: Request, workspaceName: string): string {
  const base = followBase(request);
  return composition.tenancy === "multi" ? `${base}/${workspaceName}` : base;
}

/**
 * This deployment's own agent-onboarding document (`<origin>/agent`) — what an invite (or the
 * dashboard checklist) tells a recipient's agent to fetch and follow. Same origin resolution as
 * the follow address, so the two lines in one mail can never disagree.
 */
export function agentDocUrl(request: Request): string {
  return `${followBase(request)}/agent`;
}
