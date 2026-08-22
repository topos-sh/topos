import type { GatewayContext, GatewayRequest } from "../core/ports";
import type { Logger } from "./log";
import { completeCallback, type OauthConfig, type OauthStore } from "./oauth";

/**
 * The PUBLIC listener's routes: `/{sessionId}/{serverId}` (every method — the engine answers),
 * `/oauth/callback`, `/healthz`. Anything else is the uniform 404 — no path echo, no existence
 * signal. The internal lane is NOT here: it lives on its own listener (internal.ts), so no
 * public request can reach it whatever this router does.
 */

export type EngineHandler = (ctx: GatewayContext, req: GatewayRequest) => Promise<Response>;

const notFound = () => Response.json({ code: "NOT_FOUND" }, { status: 404 });

function bearerToken(request: Request): string | null {
  const header = request.headers.get("authorization");
  if (header === null) {
    return null;
  }
  const at = header.indexOf(" ");
  if (at < 0 || header.slice(0, at).toLowerCase() !== "bearer") {
    return null;
  }
  const token = header.slice(at + 1).trim();
  return token === "" ? null : token;
}

export interface PublicDeps {
  engine: EngineHandler;
  context: GatewayContext;
  store: OauthStore;
  config: OauthConfig;
  guardedFetch: typeof fetch;
  log: Logger;
}

export function createPublicHandler(deps: PublicDeps): (request: Request) => Promise<Response> {
  return async (request: Request): Promise<Response> => {
    const url = new URL(request.url);
    const segments = url.pathname.split("/").filter((s) => s !== "");

    if (request.method === "GET" && segments.length === 1 && segments[0] === "healthz") {
      return new Response("ok");
    }

    if (
      request.method === "GET" &&
      segments.length === 2 &&
      segments[0] === "oauth" &&
      segments[1] === "callback"
    ) {
      const outcome = await completeCallback(url.searchParams, deps.store, deps.config, {
        fetch: deps.guardedFetch,
        log: deps.log,
      });
      if (!outcome.ok) {
        return new Response("This sign-in link is no longer valid.", { status: 400 });
      }
      return new Response(null, { status: 302, headers: { location: outcome.returnTo } });
    }

    const [sessionId, serverId] = segments;
    if (segments.length === 2 && sessionId !== undefined && serverId !== undefined) {
      return deps.engine(deps.context, {
        sessionId,
        serverId,
        request,
        bearer: bearerToken(request),
      });
    }

    return notFound();
  };
}
