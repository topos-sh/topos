/**
 * Tool policy application. Policy is fetched fresh per request (revocation is a row change,
 * effective next call) and applied at exactly two points: tools/list responses are filtered to
 * the enabled set, and tools/call for a non-enabled tool is answered at the gateway — upstream
 * never sees the call. Observation always records the UNFILTERED list: the web's checklist must
 * show what the server offers, not what the policy already allows.
 */

import type { GatewayContext, ObservedTool, ToolPolicy } from "./ports";

export function isToolAllowed(policy: ToolPolicy, name: string): boolean {
  return policy.mode === "all" || policy.selected.has(name);
}

/** The named tool entries of a tools/list result. Non-array or nameless entries fail closed out. */
export function toolEntries(result: Record<string, unknown>): Record<string, unknown>[] {
  const raw = Array.isArray(result["tools"]) ? (result["tools"] as unknown[]) : [];
  return raw.filter((t): t is Record<string, unknown> => typeof t === "object" && t !== null && typeof (t as Record<string, unknown>)["name"] === "string");
}

/** What gets written down about those entries — the name and the sentence, nothing else. ONE
 *  reader, so the live path and the connect-time probe can never observe different things. */
export function observedToolsOf(tools: readonly Record<string, unknown>[]): ObservedTool[] {
  return tools.map((t) => ({
    name: t["name"] as string,
    ...(typeof t["description"] === "string" ? { description: t["description"] as string } : {}),
  }));
}

/**
 * Filter one tools/list result page in place-shape (a rebuilt object, source untouched) and
 * record what the upstream actually advertised. Non-array `tools` fails closed to an empty list.
 */
export async function filterToolsListResult(
  ctx: GatewayContext,
  workspaceId: string,
  serverId: string,
  policy: ToolPolicy,
  result: Record<string, unknown>,
): Promise<Record<string, unknown>> {
  const tools = toolEntries(result);
  const observed: ObservedTool[] = observedToolsOf(tools);
  try {
    // Fire-and-forget semantics: an observation write must never fail the live call.
    await ctx.store.recordObservedTools(workspaceId, serverId, observed);
  } catch {
    ctx.env.log("warn", "observed-tools write failed", { serverId });
  }

  const kept = policy.mode === "all" ? tools : tools.filter((t) => policy.selected.has(t["name"] as string));
  return { ...result, tools: kept };
}
