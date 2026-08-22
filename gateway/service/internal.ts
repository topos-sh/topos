import { z } from "zod";
import { digestsEqual, sha256Hex } from "./crypto";
import type { Logger } from "./log";
import { beginAuthorize, type OauthConfig, type OauthStore } from "./oauth";

/**
 * The web→gateway internal lane — the plane lane's twin. Served on its OWN listener (never the
 * public bind, never published by compose), and gated the same way: no token configured ⇒ the
 * whole lane answers a uniform 404 (invisible); wrong or missing bearer ⇒ an honest 401. Only
 * the token's sha256 survives boot, and the compare is constant-time over digests.
 *
 * Three routes, exactly: authorize/begin, credentials/manual (the ONE route a secret rides —
 * never logged), DELETE credentials/{id}. Everything else on the lane is the uniform 404.
 */

const SMALL_BODY_LIMIT = 64 * 1024;

const notFound = () =>
  Response.json({ code: "NOT_FOUND" }, { status: 404 });
const unauthorized = () =>
  Response.json({ code: "UNAUTHORIZED" }, { status: 401 });

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

const beginSchema = z.object({
  workspaceId: z.string().min(1),
  serverId: z.string().min(1),
  userId: z.string().min(1).nullable(),
  returnTo: z.string().min(1),
});

const manualSchema = z.object({
  workspaceId: z.string().min(1),
  serverId: z.string().min(1),
  userId: z.string().min(1).nullable(),
  secret: z.string().min(1),
  createdByDisplay: z.string().min(1),
});

async function parseBody<T>(request: Request, schema: z.ZodType<T>): Promise<T | null> {
  try {
    const text = await request.text();
    if (text.length > SMALL_BODY_LIMIT) {
      return null;
    }
    return schema.parse(JSON.parse(text));
  } catch {
    return null;
  }
}

/** The lane's store surface — OauthStore plus the one delete the lane serves. */
export interface LaneStore extends OauthStore {
  deleteCredential(credentialId: string): Promise<boolean>;
}

export interface InternalLaneDeps {
  store: LaneStore;
  config: OauthConfig;
  guardedFetch: typeof fetch;
  log: Logger;
  /** Unset = the lane is unarmed. */
  internalToken: string | undefined;
}

export function createInternalHandler(deps: InternalLaneDeps): (request: Request) => Promise<Response> {
  // Hash at construction; the raw token is not kept.
  const tokenSha256 =
    deps.internalToken === undefined ? null : Buffer.from(sha256Hex(deps.internalToken), "hex");

  return async (request: Request): Promise<Response> => {
    if (tokenSha256 === null) {
      return notFound();
    }
    const presented = bearerToken(request);
    if (
      presented === null ||
      !digestsEqual(Buffer.from(sha256Hex(presented), "hex"), tokenSha256)
    ) {
      return unauthorized();
    }
    const url = new URL(request.url);
    const segments = url.pathname.split("/").filter((s) => s !== "");

    if (
      request.method === "POST" &&
      segments.length === 4 &&
      segments[0] === "internal" &&
      segments[1] === "v1" &&
      segments[2] === "authorize" &&
      segments[3] === "begin"
    ) {
      const body = await parseBody(request, beginSchema);
      if (body === null) {
        return Response.json({ code: "BAD_REQUEST" }, { status: 400 });
      }
      const outcome = await beginAuthorize(body, deps.store, deps.config, {
        fetch: deps.guardedFetch,
        log: deps.log,
      });
      if (outcome.ok) {
        deps.log("info", "authorize flow minted", { route: "POST /internal/v1/authorize/begin" });
        return Response.json({ authorizeUrl: outcome.authorizeUrl });
      }
      if (outcome.code === "NOT_CONNECTED") {
        return notFound();
      }
      if (outcome.code === "BAD_RETURN_TO") {
        return Response.json({ code: "BAD_REQUEST" }, { status: 400 });
      }
      return Response.json({ code: outcome.code }, { status: 409 });
    }

    if (
      request.method === "POST" &&
      segments.length === 4 &&
      segments[0] === "internal" &&
      segments[1] === "v1" &&
      segments[2] === "credentials" &&
      segments[3] === "manual"
    ) {
      const body = await parseBody(request, manualSchema);
      if (body === null) {
        return Response.json({ code: "BAD_REQUEST" }, { status: 400 });
      }
      const server = await deps.store.connectedServer(body.workspaceId, body.serverId);
      if (server === null) {
        return notFound();
      }
      const credentialId = await deps.store.storeCredential({
        workspaceId: body.workspaceId,
        serverId: body.serverId,
        userId: body.userId,
        authKind: "manual",
        payload: { secret: body.secret },
        createdByDisplay: body.createdByDisplay,
      });
      deps.log("info", "manual credential stored", {
        route: "POST /internal/v1/credentials/manual",
      });
      return Response.json({ credentialId });
    }

    if (
      request.method === "DELETE" &&
      segments.length === 4 &&
      segments[0] === "internal" &&
      segments[1] === "v1" &&
      segments[2] === "credentials" &&
      segments[3] !== undefined
    ) {
      const deleted = await deps.store.deleteCredential(segments[3]);
      if (!deleted) {
        return notFound();
      }
      deps.log("info", "credential deleted", {
        route: "DELETE /internal/v1/credentials/{credentialId}",
      });
      return new Response(null, { status: 204 });
    }

    return notFound();
  };
}
