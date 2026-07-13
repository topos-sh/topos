import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { deniedCodeEnvelope, nowUtc, okDataEnvelope } from "@/lib/api/row-envelopes.server";
import { badRequest, internalError, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { deviceInvite, deviceMe } from "@/lib/db/queries.device.server";
import { inviteMailDelivery, sendInviteEmail } from "@/lib/mail/invite-mail.server";
import { followBase } from "@/lib/plane/follow-base.server";

/**
 * `POST /api/v1/workspaces/{ws}/invitations` — invitation as a ROSTER WRITE: seat one or more emails
 * as invited members (optionally pre-placing them into channels), and answer with the workspace
 * ADDRESS (the roster is the lock; there is no invite link). Member-level unless the workspace
 * restricts inviting to owners.
 *
 * ORDERING mirrors the vault: a malformed body is a 400 BEFORE the credential resolve; then auth
 * (404); then the membership describe supplies the address; then each email is FOLDED to the
 * canonical principal (ASCII-lowercase, the workspace_member CHECK's form) — a malformed one is a 400
 * AFTER auth (the vault's `Principal::parse` inside `invite`). On success, invitation mail is
 * best-effort fire-and-forget per invitee through the app's mail seam; the `mailed` flag is honest —
 * true only when a real relay is wired (the OSS default wires none, so `false`).
 */
const BODY_CAP = 64 * 1024;
const MAX_PRINCIPAL_LEN = 128;
// The vault's principal charset (`is_principal_char`): path-safe + `. @ +`. A folded principal is
// this charset ASCII-lowercased; the fold is total because the charset is ASCII-only.
const PRINCIPAL_CHARSET = /^[A-Za-z0-9_.@+-]+$/;

/** Fold an email to the canonical principal form, or null if it is malformed (the vault's parse). */
function foldPrincipal(email: string): string | null {
  if (email.length === 0 || email.length > MAX_PRINCIPAL_LEN || !PRINCIPAL_CHARSET.test(email)) {
    return null;
  }
  return email.toLowerCase();
}

export async function action({ request, params }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "POST") {
    return uniformNotFound();
  }
  // Body FIRST — a malformed body is a 400 before the credential is ever checked.
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
  const rawEmails = body.emails as string[];

  const actor = await requireDeviceActor(request, params.ws ?? "");
  // The caller's own membership supplies the workspace address + display name (and is the same
  // membership front door the invite op runs).
  const me = await deviceMe(actor);
  if (me === null) {
    return uniformNotFound();
  }
  // Fold each email AFTER auth — a malformed one is a 400 (the vault's `Principal::parse`).
  const invited: string[] = [];
  for (const email of rawEmails) {
    const folded = foldPrincipal(email);
    if (folded === null) {
      return badRequest("malformed invitee email");
    }
    invited.push(folded);
  }

  const { createdAt } = nowUtc();
  const outcome = await deviceInvite(actor, invited, channels, createdAt);
  if (outcome === "invited") {
    const address = `${followBase(request)}/${me.name}`;
    // Fire-and-forget invitation mail per invitee through the app's mail seam (the seat + address
    // already stand — a mail fault never fails the invite). The honest `mailed` flag is the seam's
    // capability, not whether a send happened.
    for (const to of invited) {
      void sendInviteEmail({
        to,
        address,
        workspaceDisplayName: me.displayName,
        invitedBy: actor.person,
      }).catch(() => {});
    }
    return okDataEnvelope("invite", {
      address,
      invited,
      mailed: inviteMailDelivery().canSend,
    });
  }
  if (outcome === "owner_role_required") {
    return deniedCodeEnvelope("invite", "OWNER_ROLE_REQUIRED");
  }
  if (outcome === "unknown_channel") {
    return deniedCodeEnvelope("invite", "UNKNOWN_CHANNEL");
  }
  if (outcome === "member_required") {
    return uniformNotFound();
  }
  return internalError();
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
