import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { deniedCodeEnvelope, okDataEnvelope } from "@/lib/api/row-envelopes.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireDevicePerson } from "@/lib/auth/guards.server";
import { acceptInvitationByToken } from "@/lib/db/identity.server";

/**
 * `POST /api/v1/invitations/accept` — the ALREADY-ENROLLED device consuming an invite URL: it
 * is authenticated (Bearer → device → person, seat-LESS by construction — the caller has no
 * seat in the invitation's workspace yet), so the accept runs directly over the device lane
 * with no browser. The same ceremony fences apply as on the invitation page: the invitation
 * binds to the invited email's account (wrong account → a typed DENIED naming no address), an
 * unverified mailbox is refused toward the browser link, and an invalid/expired/consumed token
 * answers the uniform wire 404 — no token oracle.
 *
 * The OK answer carries the joined workspace and the optional first-destination hint — what
 * the CLI's post-accept subscribe describes. Nothing lands on any device from this accept:
 * bytes still move only through the device-side two-phase describe/consent.
 */
const BODY_CAP = 8 * 1024;
const MAX_TOKEN = 512;

export async function action({ request }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "POST") {
    return uniformNotFound();
  }
  const raw = await readCappedBody(request, BODY_CAP, "invitation accept body");
  if (raw instanceof Response) {
    return raw;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return badRequest("malformed JSON body");
  }
  const token = (parsed as { token?: unknown }).token;
  if (
    typeof parsed !== "object" ||
    parsed === null ||
    typeof token !== "string" ||
    token.length === 0 ||
    token.length > MAX_TOKEN
  ) {
    return badRequest("malformed invitation accept body");
  }

  const person = await requireDevicePerson(request);
  // The accept transaction ALSO links the accepting device in the same fence (born per the
  // ONE rule — the accepter is typically a member, so a device-approval 'on' knob bears a
  // pending link, invitation or not); `link_status` reports what the device now holds.
  const result = await acceptInvitationByToken(
    token,
    { userId: person.userId, display: person.display },
    { mailboxProven: false, deviceId: person.deviceId },
  );
  switch (result.outcome) {
    case "accepted":
      return okDataEnvelope("invite_accept", {
        workspace: {
          workspace_id: result.workspaceId,
          name: result.workspaceName,
          display_name: result.workspaceDisplayName,
        },
        link_status: result.linkStatus ?? "pending",
        ...(result.hint === null ? {} : { hint: result.hint }),
      });
    case "wrong_account":
      // The caller HOLDS the token (the mailed link), so refusing typed is honest — but the
      // refusal names no address on either side.
      return deniedCodeEnvelope("invite_accept", "INVITE_OTHER_ACCOUNT");
    case "unverified":
      return deniedCodeEnvelope("invite_accept", "EMAIL_UNVERIFIED");
    default:
      // gone — invalid, expired, revoked, or already consumed: the uniform non-answer.
      return uniformNotFound();
  }
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
