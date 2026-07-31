import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { badRequest, bearerToken, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { connectSession } from "@/lib/db/identity.server";

/**
 * `POST /api/v1/login/connect` — the LANE-SIDE second connect (`LoginConnectRequest` →
 * `LoginConnectResponse`): an already-credentialed machine asks for a FURTHER workspace's
 * session with no browser round-trip. Authorization is a Bearer that resolves to any live,
 * ACTIVE session on THIS server — resolved INSIDE the mint transaction (the ceremony locks
 * the acting session row, so a revocation racing this call serializes instead of leaving a
 * resolve-to-insert window); the acting session's own workspace is irrelevant; what matters
 * is the resolved PERSON, and SEAT STANDING in the target workspace is the trust basis: the
 * same transaction locks that seat and mints only where one stands. Anything else — bad
 * bearer, unknown or unseated workspace — is the ONE uniform 404 (never an existence
 * oracle). This route validates body SHAPE only. The plaintext credential is returned
 * exactly once over this authenticated exchange, the same practice as the flow start
 * returning its code.
 */
const BODY_CAP = 8 * 1024;
const MAX_REQUESTED_NAME = 200;
const MAX_WORKSPACE = 200;

export async function action({ request }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "POST") {
    return uniformNotFound();
  }
  const credential = bearerToken(request);
  if (credential === null) {
    return uniformNotFound();
  }
  const raw = await readCappedBody(request, BODY_CAP, "login connect body");
  if (raw instanceof Response) {
    return raw;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return badRequest("malformed JSON body");
  }
  const body = parsed as { workspace?: unknown; requested_name?: unknown };
  if (
    typeof parsed !== "object" ||
    parsed === null ||
    typeof body.workspace !== "string" ||
    body.workspace.length > MAX_WORKSPACE ||
    typeof body.requested_name !== "string" ||
    body.requested_name.trim().length === 0 ||
    body.requested_name.length > MAX_REQUESTED_NAME
  ) {
    return badRequest("malformed login connect body");
  }
  const minted = await connectSession(credential, body.workspace, body.requested_name.trim());
  if (minted === null) {
    return uniformNotFound();
  }
  return Response.json({
    credential: minted.credential,
    session_id: minted.sessionId,
    session_status: minted.sessionStatus,
    workspace: {
      workspace_id: minted.workspace.workspaceId,
      name: minted.workspace.name,
      display_name: minted.workspace.displayName,
    },
  });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
