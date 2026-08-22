import { gatewayFetch } from "./client.server";

/**
 * ASK THE SERVER FOR ITS TOOLS AGAIN — the one wrapper over the lane's `tools/refresh`.
 *
 * The tool list is written down by the gateway (it holds the sign-in, so it is the only tier that
 * can speak to the server), and read from here through the gateway schema's mirror. It is filled in
 * automatically the moment a sign-in is connected; this is the door for the other case — a server
 * whose tools changed since, or one that was offline when its sign-in landed.
 *
 * The outcome is a value, never an exception into a ceremony: an unreachable gateway is `fault`,
 * exactly as the sign-in wrappers spell it.
 */

export type RefreshToolsOutcome =
  /** The list was read and written; `tools` is how many the server now offers. */
  | { kind: "refreshed"; tools: number }
  /** The server asks for a sign-in and this workspace has none to ask with. */
  | { kind: "no_credential" }
  /** The server did not answer, or answered something that is not a tool list. */
  | { kind: "unreachable" }
  /** The gateway has no live connection to that server, or could not be reached at all. */
  | { kind: "fault" };

export async function refreshObservedTools(args: {
  workspaceId: string;
  serverId: string;
  userId: string | null;
}): Promise<RefreshToolsOutcome> {
  let res: Response;
  try {
    res = await gatewayFetch({
      method: "POST",
      template: "/internal/v1/tools/refresh",
      body: args,
    });
  } catch {
    return { kind: "fault" };
  }
  if (!res.ok) {
    return { kind: "fault" };
  }
  const body = (await res.json().catch(() => null)) as {
    outcome?: unknown;
    tools?: unknown;
  } | null;
  if (body?.outcome === "recorded" && typeof body.tools === "number") {
    return { kind: "refreshed", tools: body.tools };
  }
  if (body?.outcome === "no_credential") {
    return { kind: "no_credential" };
  }
  return body?.outcome === "unreachable" ? { kind: "unreachable" } : { kind: "fault" };
}
