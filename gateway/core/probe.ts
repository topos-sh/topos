/**
 * The TOOL PROBE — one `tools/list` walk toward an upstream server, run because a credential was
 * just connected (or because somebody asked to see the list again), never because an agent called.
 *
 * Why it exists: a tool policy is a checklist over what the server offers, and nobody can narrow a
 * list they cannot see. Before this, the list only filled in after the first real call through the
 * gateway, so the panel's honest "nothing observed yet" doubled as a trap — narrowing to a checklist
 * of zero disables every tool. The probe makes the list a fact of CONNECTING.
 *
 * It is the same client the live path uses: `ensureUpstream` + `sendUpstreamRequest`, so era
 * detection, the credential attach, the refresh-once retry and the guarded fetch all behave exactly
 * as they would for an agent. Two things a probe does differently, both deliberate:
 *
 *  - it records NO usage row. A usage row says a person's machine called this server; a probe is
 *    the product's own bookkeeping. The host hands the probe a context whose sink drops, so this is
 *    structural rather than a rule somebody has to remember.
 *  - it TEARS ITS CONVERSATION DOWN. The probe stands in module memory under a synthetic session
 *    id, and on the 2024-11-05 transport an upstream session owns a live SSE pump; leaving either
 *    behind would be a leak nothing ever collects. The era VERDICT it learned is kept — that is
 *    shared, re-probeable knowledge about the server, and the next real call is faster for it.
 *
 * Pagination is followed and the list is recorded WHOLE OR NOT AT ALL: a page that fails records
 * nothing, because a half list written through `recordObservedTools` would mark the tools it never
 * reached as no longer offered.
 */

import { observedToolsOf, toolEntries } from "./filter";
import type { GatewayContext, ObservedTool, SessionRef } from "./ports";
import { isResponse, nextGatewayId, type JsonRpcRequest } from "./protocol";
import {
  deleteUpstreamSession,
  GATEWAY_IDENTITY,
  responseFromSse,
  sendUpstreamRequest,
  type UpstreamCall,
  type UpstreamReply,
} from "./upstream";

/** Which workspace's connection to probe, and whose sign-in to probe it with. */
export interface ProbeTarget {
  workspaceId: string;
  serverId: string;
  /** The person whose credential to prefer; null asks with the workspace's own sign-in. */
  userId: string | null;
}

export type ProbeOutcome =
  /** The list was read and written; `tools` is how many the server offers. */
  | { kind: "recorded"; tools: number }
  /** The server asks for a sign-in and this workspace has none it could have used. */
  | { kind: "no_credential" }
  /** The server did not answer, refused, or answered something that is not a tool list. */
  | { kind: "unreachable" }
  /** This workspace has no live connection to that server. */
  | { kind: "not_connected" };

/** Pages a probe walks before it stops asking — a cursor loop needs a floor under it. */
const MAX_PAGES = 20;

/**
 * The probe's stand-in caller. It is NOT a session anybody signed in with: the id exists so the
 * upstream machinery can key its module memory, and it never reaches a usage row (the sink drops)
 * or an upstream request (only the credential does). Stable per (workspace, server) so repeat
 * probes reuse one key instead of growing the map a synthetic id at a time.
 */
function probeSession(target: ProbeTarget): SessionRef {
  return {
    sessionId: `probe:${target.workspaceId}:${target.serverId}`,
    workspaceId: target.workspaceId,
    // The store resolves a person's own credential first and the workspace's second; the empty
    // string belongs to nobody, so a workspace-scoped probe asks with the workspace sign-in.
    userId: target.userId ?? "",
    displayName: "tool probe",
  };
}

/** Read one upstream reply envelope down to its result, whatever transport carried it. */
async function resultOf(
  call: UpstreamCall,
  reply: UpstreamReply,
  id: string | number,
): Promise<Record<string, unknown> | null> {
  if (reply.kind === "json") {
    return isResponse(reply.message) && "result" in reply.message ? reply.message.result : null;
  }
  if (reply.kind === "sse") {
    const message = await responseFromSse(reply.body, id, call.ctx.env.now);
    return message !== null && isResponse(message) && "result" in message ? message.result : null;
  }
  return null;
}

/** Every tool the server advertises, across pages; null when any page failed to answer. */
async function listAllTools(call: UpstreamCall): Promise<ObservedTool[] | null> {
  const tools: ObservedTool[] = [];
  const seen = new Set<string>();
  let cursor: string | null = null;
  for (let page = 0; page < MAX_PAGES; page += 1) {
    const id = nextGatewayId();
    const msg: JsonRpcRequest = {
      jsonrpc: "2.0",
      id,
      method: "tools/list",
      params: cursor === null ? {} : { cursor },
    };
    const result = await resultOf(call, await sendUpstreamRequest(call, msg, { identity: GATEWAY_IDENTITY }), id);
    if (result === null) return null;
    for (const tool of observedToolsOf(toolEntries(result))) {
      // A name repeated across pages must not reach the upsert twice — one INSERT touching a row
      // twice is an error, and the whole write would be lost with it.
      if (seen.has(tool.name)) continue;
      seen.add(tool.name);
      tools.push(tool);
    }
    const next = result["nextCursor"];
    if (typeof next !== "string" || next === "") return tools;
    cursor = next;
  }
  return tools;
}

/**
 * Probe one connected server and record what it offers. Never throws: every outcome is a value the
 * caller can log or render, because the act this follows (a sign-in landing) must not fail on it.
 */
export async function probeServerTools(
  ctx: GatewayContext,
  target: ProbeTarget,
): Promise<ProbeOutcome> {
  const server = await ctx.store.connectedServer(target.workspaceId, target.serverId);
  if (server === null) {
    return { kind: "not_connected" };
  }
  const session = probeSession(target);
  const credential = await ctx.store.credentialFor(
    target.workspaceId,
    target.serverId,
    session.userId,
  );
  if (credential === null && server.authMode !== "none") {
    return { kind: "no_credential" };
  }
  const call: UpstreamCall = {
    ctx,
    session,
    server,
    auth: { credential, secret: credential?.secret ?? null, refreshed: false },
  };
  try {
    const tools = await listAllTools(call);
    if (tools === null) {
      return { kind: "unreachable" };
    }
    await ctx.store.recordObservedTools(target.workspaceId, target.serverId, tools);
    return { kind: "recorded", tools: tools.length };
  } catch {
    return { kind: "unreachable" };
  } finally {
    // The probe's own conversation goes with the probe — see the module note.
    await deleteUpstreamSession(call).catch(() => {});
  }
}
