import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { rowOpResponse } from "@/lib/api/row-envelopes.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { profileIncludeBundle, profileRemoveBundle } from "@/lib/db/queries.lane.server";

/**
 * `PUT | DELETE /api/v1/workspaces/{ws}/profile/skills/{skill}` — edit the caller's OWN
 * per-workspace profile: PUT writes the include line (`add -g`; an optional JSON body pins a
 * version — `{"pin": "<64-hex>"}`; an exclude on the same bundle flips to include), DELETE
 * removes it (`remove -g`) — and when a broader layer (an included channel, the baseline)
 * still provides the bundle, the removal records an EXCLUDE line instead; the answer's status
 * says which happened so the receipt can name the inverse. `{skill}` is the immutable id.
 * Naturally idempotent.
 */
const BODY_CAP = 8 * 1024;
const HEX_64 = /^[0-9a-f]{64}$/;

const PROFILE_DENIED = {
  skill_not_active: "SKILL_NOT_ACTIVE",
} as const;

export async function action({ request, params }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "PUT" && request.method !== "DELETE") {
    return uniformNotFound();
  }
  const skill = params.skill ?? "";
  if (request.method === "PUT") {
    // The optional pin body — absent or empty means "track current".
    let pin: string | null = null;
    const raw = await readCappedBody(request, BODY_CAP, "profile body");
    if (raw instanceof Response) {
      return raw;
    }
    if (raw.trim().length > 0) {
      let parsed: unknown;
      try {
        parsed = JSON.parse(raw);
      } catch {
        return badRequest("malformed JSON body");
      }
      const body = (parsed ?? {}) as { pin?: unknown };
      if (body.pin !== undefined) {
        if (typeof body.pin !== "string" || !HEX_64.test(body.pin)) {
          return badRequest("malformed profile body: pin must be 64-char lowercase hex");
        }
        pin = body.pin;
      }
    }
    const actor = await requireSessionActor(request, params.ws ?? "");
    const status = await profileIncludeBundle(actor, skill, pin);
    return rowOpResponse("add", status, { included: "included" }, PROFILE_DENIED);
  }
  const actor = await requireSessionActor(request, params.ws ?? "");
  const status = await profileRemoveBundle(actor, skill);
  return rowOpResponse(
    "remove",
    status,
    { removed: "removed", excluded: "excluded", not_in_profile: "not_in_profile" },
    PROFILE_DENIED,
  );
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
