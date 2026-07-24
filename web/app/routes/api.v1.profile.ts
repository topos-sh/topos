import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { okDataEnvelope } from "@/lib/api/row-envelopes.server";
import { uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { profileOf } from "@/lib/db/queries.lane.server";

/**
 * `GET /api/v1/workspaces/{ws}/profile` — the caller's per-workspace PROFILE (the person-side
 * manifest, server-stored so it roams): every include/exclude line, resolved to names, pins
 * included. The `add -g`/`remove -g` receipts and `status` read this to phrase "which
 * manifest line asked for it".
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const actor = await requireSessionActor(request, params.ws ?? "");
  const entries = await profileOf(actor);
  return okDataEnvelope("profile", {
    entries: entries.map((e) => ({
      mode: e.mode,
      kind: e.kind,
      name: e.name,
      ...(e.pin === null ? {} : { pin: e.pin }),
    })),
  });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function action(): Response {
  return uniformNotFound();
}
