import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { deniedCodeEnvelope, okDataEnvelope } from "@/lib/api/row-envelopes.server";
import { badRequest, internalError, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { laneInvite, laneMe } from "@/lib/db/queries.lane.server";
import { inviteMailDelivery, sendInviteEmail } from "@/lib/mail/invite-mail.server";
import { agentDocUrl, workspaceAddress } from "@/lib/ws-url.server";

/**
 * `POST /api/v1/workspaces/{ws}/invitations` — invitation as an INVITATION-ROW write: each
 * email becomes a pending 7-day claim on a future user, and the answer carries the workspace
 * ADDRESS (the roster is the lock; there is no invite link). Member-level unless the workspace
 * restricts inviting to owners.
 *
 * INVITING REQUIRES ARMED MAIL now: the mailbox round-trip is the invited sign-up's identity
 * rung, so on an unarmed deployment the op refuses TYPED (`MAIL_NOT_CONFIGURED`) — an
 * invitation that could never be verified would admit nothing and mislead the inviter.
 * ORDERING mirrors the door's posture: a malformed body is a 400 BEFORE the credential
 * resolve; a malformed email is a 400 AFTER auth.
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
  const raw = await readCappedBody(request, BODY_CAP, "invitation body");
  if (raw instanceof Response) {
    return raw;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return badRequest("malformed JSON body");
  }
  if (typeof parsed !== "object" || parsed === null) {
    return badRequest("malformed invitation body");
  }
  const body = parsed as { emails?: unknown; channels?: unknown };
  if (!Array.isArray(body.emails) || !body.emails.every((e) => typeof e === "string")) {
    return badRequest("malformed invitation body: emails");
  }
  let channels: string[] = [];
  if (body.channels !== undefined) {
    if (!Array.isArray(body.channels) || !body.channels.every((c) => typeof c === "string")) {
      return badRequest("malformed invitation body: channels");
    }
    channels = body.channels as string[];
  }

  const actor = await requireDeviceActor(request, params.ws ?? "");
  if (!inviteMailDelivery().canSend) {
    return deniedCodeEnvelope("invite", "MAIL_NOT_CONFIGURED");
  }
  const me = await laneMe(actor);
  if (me === null) {
    return uniformNotFound();
  }
  const outcome = await laneInvite(actor, body.emails as string[], channels);
  if (outcome === "bad_email") {
    return badRequest("malformed invitee email");
  }
  if (outcome === "invited") {
    const address = workspaceAddress(request, me.name);
    const invited = (body.emails as string[]).map((e) => e.trim().toLowerCase());
    // Fire-and-forget invitation mail per invitee (the rows already stand; a mail fault never
    // fails the invite — the lapse clock simply runs).
    for (const to of invited) {
      void sendInviteEmail({
        to,
        address,
        agentUrl: agentDocUrl(request),
        workspaceDisplayName: me.displayName,
        invitedBy: actor.display,
      }).catch(() => {});
    }
    return okDataEnvelope("invite", { address, invited, mailed: true });
  }
  if (outcome === "owner_role_required") {
    return deniedCodeEnvelope("invite", "OWNER_ROLE_REQUIRED");
  }
  if (outcome === "unknown_channel") {
    return deniedCodeEnvelope("invite", "UNKNOWN_CHANNEL");
  }
  return internalError();
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
