import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { pollLoginFlow, workspaceRowById } from "@/lib/db/identity.server";

/**
 * `POST /api/v1/login/token` — poll the login flow (`LoginPollRequest` →
 * `LoginPollResponse`). IDEMPOTENT: a terminal answer repeats on every poll until the row is
 * swept (the client's crash recovery is to re-poll); a `granted` poll echoes the PRESENTED
 * device_code back as `credential` — the approval promoted that same secret to the session's
 * one bearer credential, which is how a hash-only store still "delivers" it: the poller
 * already holds it.
 *
 * The `workspace` decoration on a granted poll is THE FLOW'S workspace — the CLI records what
 * it logged into from this one field. It reads the workspace id the APPROVAL persisted
 * inside its fence, never a poll-time re-resolution of the mutable slug: a rename keeps the
 * decoration pointing at the approved workspace, and a delete+recreate of the slug can never
 * re-point a granted flow at a row the approval never covered. A workspace deleted inside the
 * TTL omits the field (the same arm a virgin install answers).
 */
const BODY_CAP = 8 * 1024;

export async function action({ request }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "POST") {
    return uniformNotFound();
  }
  const raw = await readCappedBody(request, BODY_CAP, "login token body");
  if (raw instanceof Response) {
    return raw;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return badRequest("malformed JSON body");
  }
  const flowCode = (parsed as { device_code?: unknown }).device_code;
  if (typeof parsed !== "object" || parsed === null || typeof flowCode !== "string") {
    return badRequest("malformed login token body");
  }
  const result = await pollLoginFlow(flowCode);
  if (result.status !== "granted") {
    return Response.json({ status: result.status });
  }
  const ws =
    result.approvedWorkspaceId === null ? null : await workspaceRowById(result.approvedWorkspaceId);
  // `session_status` is the minted session's status, read live (an owner may have approved a
  // pending session between grant and poll) — the client's first sweep needs to know whether
  // delivery flows yet or the session awaits an owner.
  return Response.json({
    status: "granted",
    credential: flowCode,
    session_id: result.sessionId,
    session_status: result.sessionStatus,
    ...(ws === null
      ? {}
      : {
          workspace: {
            workspace_id: ws.id,
            name: ws.name,
            display_name: ws.displayName,
          },
        }),
    ...(result.hint === null ? {} : { hint: result.hint }),
  });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
