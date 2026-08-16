import type { ActionFunctionArgs } from "react-router";
import { laneGate } from "@/lib/api/compat.server";
import { deniedCodeEnvelope, rowOpResponse } from "@/lib/api/row-envelopes.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { laneProtectChannel } from "@/lib/db/queries.lane.server";

/**
 * `PUT /api/v1/workspaces/{ws}/channels/{ch}/protection` — set a channel's mode (`curated` |
 * `open`). `{ch}` is the channel NAME. A JSON body `{ level }`; the credential resolves
 * BEFORE the body (the lane-wide order — a non-member meets only the uniform 404, and body
 * validation 400s only an authenticated member). An unknown channel is the uniform 404.
 * Tightening to `curated` takes reviewer+ AND the reviews entitlement
 * (`REVIEWS_NOT_AVAILABLE`); loosening to `open` takes owner and is never entitlement-gated.
 */
const BODY_CAP = 64 * 1024;

const PROTECT_DENIED = {
  reviewer_role_required: "REVIEWER_ROLE_REQUIRED",
  owner_role_required: "OWNER_ROLE_REQUIRED",
} as const;

export async function action({ request, params }: ActionFunctionArgs): Promise<Response> {
  const gated = laneGate(request);
  if (gated !== null) {
    return gated;
  }
  if (request.method !== "PUT") {
    return uniformNotFound();
  }
  const actor = await requireSessionActor(request, params.ws ?? "");
  const body = await readCappedBody(request, BODY_CAP, "protection body");
  if (body instanceof Response) {
    return body;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    return badRequest("malformed JSON body");
  }
  const level = (parsed as { level?: unknown }).level;
  if (typeof parsed !== "object" || parsed === null || typeof level !== "string") {
    return badRequest("malformed protection body");
  }
  if (level !== "open" && level !== "curated") {
    return badRequest("a channel protection level must be `curated` or `open`");
  }
  const status = await laneProtectChannel(actor, params.channel ?? "", level);
  if (status === "reviews_unavailable") {
    return deniedCodeEnvelope(
      "protect",
      "REVIEWS_NOT_AVAILABLE",
      "Review protection is not available on this workspace.",
    );
  }
  return rowOpResponse("protect", status, { set: "set" }, PROTECT_DENIED);
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
