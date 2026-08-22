import type { McpGatewayView } from "@/components/skill/mcp-gateway";
import type { MemberActor } from "@/lib/auth/guards.server";
import { mcpSignInState, mcpToolsView, mcpUsageWindow } from "@/lib/db/queries.gateway.server";
import { gatewayLane } from "@/lib/gateway/client.server";

/**
 * ASSEMBLE THE CONNECTED SERVER'S GATEWAY PANEL — the one place the page's three sections are
 * read, so a loader asks once and gets either the whole panel or nothing.
 *
 * `null` is the deployment-shaped answer: no gateway is configured, so the sections do not render
 * and — just as important — no read is issued against the `gateway` schema, which does not exist on
 * such an install. Every caller must treat null as "there is no such surface here", never as an
 * empty panel.
 */
export async function mcpGatewayView(
  actor: MemberActor,
  server: {
    serverId: string;
    displayName: string;
    authMode: string | null;
  },
): Promise<McpGatewayView | null> {
  if (gatewayLane() === null) {
    return null;
  }
  const [signIn, tools, usage] = await Promise.all([
    mcpSignInState(actor, server.serverId),
    mcpToolsView(actor, server.serverId),
    mcpUsageWindow(actor, server.serverId),
  ]);
  // Resolution mirrors the gateway's own: a person's own sign-in answers first, the workspace
  // service account second. The state line names the one a call would actually carry.
  const signedIn = signIn.mine !== null ? "mine" : signIn.workspace !== null ? "workspace" : null;
  return {
    displayName: server.displayName,
    authMode: server.authMode as McpGatewayView["authMode"],
    signedIn,
    canConnectPersonal: signIn.mine === null,
    canConnectWorkspace: actor.role === "owner" && signIn.workspace === null,
    mode: tools.mode,
    tools: tools.tools,
    usage: usage.events,
  };
}
