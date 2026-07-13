import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { deviceChannels } from "@/lib/db/queries.device.server";
import type { paths } from "@/lib/plane/contract/schema";

/**
 * `GET /api/v1/workspaces/{ws}/channels` — the workspace channels (the structural `everyone`
 * included, name-sorted), each with the caller's membership, its member count, and its name-sorted
 * skill references. Pure directory rows; per-member and hot, never cacheable. Every field is always
 * present (no omissions) — an empty workspace still carries its `everyone`.
 */
type WireChannelIndex =
  paths["/v1/workspaces/{ws}/channels"]["get"]["responses"][200]["content"]["application/json"];

export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const actor = await requireDeviceActor(request, params.ws ?? "");
  const channels = await deviceChannels(actor);
  const body: WireChannelIndex = {
    channels: channels.map((c) => ({
      name: c.name,
      mode: c.mode,
      builtin: c.builtin,
      member: c.member,
      member_count: c.memberCount,
      skills: c.skills.map((s) => ({ skill_id: s.skillId, name: s.name })),
    })),
  };
  return Response.json(body, { headers: NO_STORE });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function action(): Response {
  return uniformNotFound();
}
