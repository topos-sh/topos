import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { okStatusEnvelope } from "@/lib/api/row-envelopes.server";
import { bearerToken, uniformNotFound } from "@/lib/api/wire.server";
import { revokeSessionByCredential } from "@/lib/db/identity.server";

/**
 * `DELETE /api/v1/session` — the CLI's `topos logout <workspace>` for ONE session: the
 * presented credential's OWN session is ended (the row deleted, its reported state cascading
 * away; `session_ended` audit row, cause: self). No body, no target field: the credential IS
 * the session, so nothing client-asserted can reach into another pocket. After the delete the
 * credential no longer resolves, so a RETRY answers the uniform 404 — the client treats it as
 * already-signed-out. A multi-workspace logout is one DELETE per stored credential,
 * client-side.
 */
export async function action({ request }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "DELETE") {
    return uniformNotFound();
  }
  const credential = bearerToken(request);
  if (credential === null) {
    return uniformNotFound();
  }
  const ended = await revokeSessionByCredential(credential);
  if (!ended) {
    return uniformNotFound();
  }
  return okStatusEnvelope("logout", "revoked");
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
