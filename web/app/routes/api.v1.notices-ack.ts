import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { rowOpResponse } from "@/lib/api/row-envelopes.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { laneAckNotices } from "@/lib/db/queries.lane.server";

/**
 * `POST /api/v1/workspaces/{ws}/notices/ack` — mark the caller's own notices read by id (the
 * delivery feed carries them unacked; an interactive session acks what it narrated). A JSON
 * body `{ ids }`; a malformed body is a 400 BEFORE the credential resolve. Idempotent (only
 * the person's own unacked rows move); the 200 envelope carries `{ status: "acked" }`.
 */
const BODY_CAP = 64 * 1024;

export async function action({ request, params }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "POST") {
    return uniformNotFound();
  }
  const body = await readCappedBody(request, BODY_CAP, "notices ack body");
  if (body instanceof Response) {
    return body;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    return badRequest("malformed JSON body");
  }
  const ids = (parsed as { ids?: unknown }).ids;
  if (
    typeof parsed !== "object" ||
    parsed === null ||
    !Array.isArray(ids) ||
    !ids.every((id) => typeof id === "string")
  ) {
    return badRequest("malformed notices ack body");
  }
  const actor = await requireSessionActor(request, params.ws ?? "");
  const status = await laneAckNotices(actor, ids as string[]);
  return rowOpResponse("notices", status, { acked: "acked" }, {});
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
