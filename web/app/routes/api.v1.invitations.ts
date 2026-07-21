import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { deniedCodeEnvelope, okDataEnvelope } from "@/lib/api/row-envelopes.server";
import { badRequest, internalError, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { laneInvite, laneMe } from "@/lib/db/queries.lane.server";
import { inviteMailDelivery, sendInviteEmail } from "@/lib/mail/invite-mail.server";
import { agentDocUrl, inviteUrl, workspaceAddress } from "@/lib/ws-url.server";

/**
 * `POST /api/v1/workspaces/{ws}/invitations` — invitation as an INVITATION-ROW write: each
 * email becomes a pending 7-day claim on a future user, redeemable through the single-use link
 * the mail carries (the link travels ONLY in the mail — the inviter's receipt shows the
 * workspace address, never the token). At most ONE optional first-destination hint (`skill` or
 * `channel`, a name in this workspace) rides the invitation and is delivered by the accept.
 * Member-level unless the workspace restricts inviting to owners.
 *
 * INVITING REQUIRES ARMED MAIL: the mailbox is where the tokened link travels, so on an
 * unarmed deployment the op refuses TYPED (`MAIL_NOT_CONFIGURED`) — an invitation nobody could
 * receive would mislead the inviter. ORDERING mirrors the door's posture: a malformed body is
 * a 400 BEFORE the credential resolve; a malformed email is a 400 AFTER auth.
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
  const body = parsed as { emails?: unknown; skill?: unknown; channel?: unknown };
  if (!Array.isArray(body.emails) || !body.emails.every((e) => typeof e === "string")) {
    return badRequest("malformed invitation body: emails");
  }
  if (body.skill !== undefined && typeof body.skill !== "string") {
    return badRequest("malformed invitation body: skill");
  }
  if (body.channel !== undefined && typeof body.channel !== "string") {
    return badRequest("malformed invitation body: channel");
  }
  if (body.skill !== undefined && body.channel !== undefined) {
    return badRequest("an invitation carries at most one first destination — skill OR channel");
  }

  const actor = await requireDeviceActor(request, params.ws ?? "");
  if (!inviteMailDelivery().canSend) {
    return deniedCodeEnvelope("invite", "MAIL_NOT_CONFIGURED");
  }
  const me = await laneMe(actor);
  if (me === null) {
    return uniformNotFound();
  }
  const hint = {
    ...(body.skill !== undefined ? { skill: body.skill as string } : {}),
    ...(body.channel !== undefined ? { channel: body.channel as string } : {}),
  };
  const outcome = await laneInvite(actor, body.emails as string[], hint);
  if (outcome.outcome === "bad_email") {
    return badRequest("malformed invitee email");
  }
  if (outcome.outcome === "invited") {
    const address = workspaceAddress(request, me.name);
    const mailHint =
      body.skill !== undefined
        ? { kind: "skill", name: body.skill as string }
        : body.channel !== undefined
          ? { kind: "channel", name: body.channel as string }
          : undefined;
    // Fire-and-forget invitation mail per invitee (the rows already stand; a mail fault never
    // fails the invite — re-inviting mints a fresh link). The tokened URL goes ONLY into the
    // mail; the receipt below carries the workspace address.
    for (const one of outcome.minted) {
      void sendInviteEmail({
        to: one.email,
        inviteUrl: inviteUrl(request, me.name, one.token),
        agentUrl: agentDocUrl(request),
        workspaceDisplayName: me.displayName,
        invitedBy: actor.display,
        ...(mailHint === undefined ? {} : { hint: mailHint }),
      }).catch(() => {});
    }
    return okDataEnvelope("invite", {
      address,
      invited: outcome.minted.map((m) => m.email),
      mailed: true,
    });
  }
  if (outcome.outcome === "owner_role_required") {
    return deniedCodeEnvelope("invite", "OWNER_ROLE_REQUIRED");
  }
  if (outcome.outcome === "unknown_skill") {
    return deniedCodeEnvelope("invite", "UNKNOWN_SKILL");
  }
  if (outcome.outcome === "unknown_channel") {
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
