import type { ActionFunctionArgs } from "react-router";
import { serverEnv } from "@/env.server";
import { checkBelt } from "@/lib/api/belt.server";
import {
  badRequest,
  internalError,
  NO_STORE,
  readCappedBody,
  uniformNotFound,
} from "@/lib/api/wire.server";
import { sendPasscodeEmail } from "@/lib/mail/passcode-mail.server";
import { vaultFetch } from "@/lib/plane/client.server";

/**
 * `POST /api/v1/enroll/passcode` — the passcode START, served by this tier since the mail
 * unification (the vault's `routes/door.rs` stub pins this wire; the vault only MINTS, over its
 * internal lane). The handler mints, fire-and-forgets the mail through the app's ONE seam, and
 * answers the CONSTANT `{"status":"sent"}` ack.
 *
 * The no-enumeration-oracle posture, preserved across the move: the MINT is awaited (its shape
 * and latency never depend on whether the address is rostered — the roster gate is enforced at
 * redeem, exactly as before), the SEND never is, and a send failure is swallowed — so neither
 * the ack body nor its latency says anything about the address.
 */
const BODY_CAP = 64 * 1024;

export async function action({ request }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "POST") {
    return uniformNotFound();
  }
  const body = await readCappedBody(request, BODY_CAP, "passcode body");
  if (body instanceof Response) {
    return body;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    return badRequest("malformed JSON body");
  }
  const req = parsed as { user_code?: unknown; email?: unknown };
  if (
    typeof req.user_code !== "string" ||
    req.user_code.length === 0 ||
    typeof req.email !== "string" ||
    req.email.length === 0
  ) {
    return badRequest("malformed passcode body");
  }
  // Mint over the internal lane (the email is parsed INSIDE the vault's authority op — this tier
  // never judges it). A dead user code is the vault's uniform 404, relayed as this tier's own.
  const minted = await vaultFetch({
    method: "POST",
    template: "/internal/v1/enroll/passcode",
    body: { user_code: req.user_code, email: req.email },
  });
  if (minted.status === 404) {
    return uniformNotFound();
  }
  if (!minted.ok) {
    return internalError();
  }
  const mint = (await minted.json()) as { passcode: string; workspace_display_name: string };
  // Fire-and-forget through the mail seam — the result is deliberately dropped (no oracle; the
  // code expires on its own clock and the human simply requests another).
  const verifyBaseUrl = serverEnv().TOPOS_PUBLIC_URL ?? new URL(request.url).origin;
  void sendPasscodeEmail({
    to: req.email,
    code: mint.passcode,
    workspaceDisplayName: mint.workspace_display_name,
    verifyBaseUrl,
  }).catch(() => {});
  return Response.json({ status: "sent" }, { headers: NO_STORE });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405. */
export function loader(): Response {
  return uniformNotFound();
}
