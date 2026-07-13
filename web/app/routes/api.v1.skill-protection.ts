import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { rowOpResponse } from "@/lib/api/row-envelopes.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { deviceProtectSkill } from "@/lib/db/queries.device.server";

/**
 * `PUT /api/v1/workspaces/{ws}/skills/{skill}/protection` — set a skill's protection level
 * (`reviewed` | `open`). `{skill}` is the immutable id. A JSON body `{ level }`. ORDERING mirrors the
 * vault's extractor: a malformed body AND an invalid LEVEL are both a 400 BEFORE the credential
 * resolve (the vault's `parse_level` runs before `authority().protect`), so a bad level is a 400
 * unconditionally — never a membership signal a non-member could read off. Tightening to `reviewed`
 * takes reviewer+ (`REVIEWER_ROLE_REQUIRED`); loosening to `open` takes owner
 * (`OWNER_ROLE_REQUIRED`).
 */
const BODY_CAP = 64 * 1024;

const PROTECT_DENIED = {
  reviewer_role_required: "REVIEWER_ROLE_REQUIRED",
  owner_role_required: "OWNER_ROLE_REQUIRED",
} as const;

export async function action({ request, params }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "PUT") {
    return uniformNotFound();
  }
  // Body FIRST — a malformed body is a 400 before the credential is ever checked.
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
  // Level validation BEFORE auth — the vault's `parse_level` runs before the credential resolve, so
  // a bad level is a 400 for everyone (a non-member must not read a 400-vs-404 membership signal).
  if (level !== "open" && level !== "reviewed") {
    return badRequest("a skill protection level must be `reviewed` or `open`");
  }
  const actor = await requireDeviceActor(request, params.ws ?? "");
  const status = await deviceProtectSkill(actor, params.skill ?? "", level);
  return rowOpResponse("protect", status, { set: "set" }, PROTECT_DENIED);
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
