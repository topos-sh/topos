import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { okStatusEnvelope } from "@/lib/api/row-envelopes.server";
import { uniformNotFound } from "@/lib/api/wire.server";
import { requireDevicePerson } from "@/lib/auth/guards.server";
import { revokeOwnDevice } from "@/lib/db/identity.server";

/**
 * `DELETE /api/v1/device` — the GLOBAL self-revoke (the CLI's `auth logout`): the presented
 * credential's OWN device is revoked (instant, trigger-final), every one of its links severed,
 * and its per-workspace reported state deleted — one transaction, `device_unlinked` audit rows
 * per link (cause: device revoked). No body, no target field: the credential names the device,
 * so nothing client-asserted can reach into another pocket. After revocation the credential no
 * longer resolves, so a RETRY answers the uniform 404 — the client treats it as
 * already-signed-out.
 */
export async function action({ request }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "DELETE") {
    return uniformNotFound();
  }
  const person = await requireDevicePerson(request);
  // A lost race (another request revoked between the guard and here) still answers ok — a
  // sign-out that finds the device already signed out has nothing left to do.
  await revokeOwnDevice({ userId: person.userId, display: person.display }, person.deviceId);
  return okStatusEnvelope("logout", "revoked");
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
