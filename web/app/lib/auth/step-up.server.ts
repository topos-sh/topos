import { serverEnv } from "@/env.server";
import { consumeStepUpConfirmation, mintStepUpConfirmation } from "@/lib/db/identity.server";
import { sendStepUpMail } from "@/lib/mail/auth-mail.server";
import { mailDelivery } from "@/lib/mail/transport.server";
import { followBase } from "@/lib/plane/follow-base.server";
import { allowStepUpAttempt } from "@/lib/rate-limit.server";
import { requireSession, safeNextPath } from "./guards.server";
import { hasCredentialPassword, verifySessionPassword } from "./server";

/**
 * STEP-UP — the re-authentication gate in front of the admin ceremonies (roster mutations,
 * skill lifecycle, purge, channel existence-admin, policy setters). A session is evidence of
 * who is browsing; these acts want evidence of who is ACTING, minted seconds ago. The RUNG is
 * the person's step-up METHOD:
 *   - `password` — the account has a password (the OSS default; every account is born with one):
 *     re-enter it and the action verifies it immediately before the act.
 *   - `email` — a password-less account (a magic-link/social deployment): confirm through a
 *     single-use link mailed to the session address, returning to the SAME ceremony page with a
 *     token the submit consumes. There is no sudo window — the token authorizes exactly the one
 *     submission that carries it, once.
 *   - `unavailable` — no password AND no armed mail: refused with a typed reason (set a password
 *     or arm SMTP), never a silent dead end.
 * Deliberately STATELESS whichever rung — nothing to expire or steal beyond the one submission;
 * every gated act re-proves. The destructive ceremonies (delete a skill, purge a version, delete
 * a channel) additionally require typing the resource's name.
 *
 * Order inside an action: guard (mint the actor) → requireStepUp → requireTypedName (where
 * destructive) → the act. A step-up failure returns a typed form error and performs NOTHING —
 * no vault call, no data-layer call; the ceremony records the refused attempt in its own
 * admin_event row (the same as the password rung's refusals).
 */

/** The form field / query names the shared <StepUpFields> component renders (mirrored there — a
 * server module cannot be imported into the client bundle). */
export const STEP_UP_PASSWORD_FIELD = "stepup_password";
export const STEP_UP_CONFIRM_FIELD = "confirm_name";
export const STEP_UP_TOKEN_FIELD = "stepup_token";
export const STEP_UP_INTENT_FIELD = "stepup_intent";
export const STEP_UP_INTENT_EMAIL = "request-email";
export const STEP_UP_TOKEN_QUERY = "stepup";

export type StepUpMethod = "password" | "email" | "unavailable";
export type StepUpResult = { ok: true } | { ok: false; error: string };

/** Wrong-rung and rate-limit copy — precise about what is being asked and why. */
const STEP_UP_FAILED =
  "Password check failed. Re-enter your account password to confirm this action.";
const STEP_UP_LIMITED = "Too many attempts. Wait a minute, then try again.";
const STEP_UP_TOKEN_FAILED =
  "That confirmation link expired or was already used. Request a new one.";
const STEP_UP_EMAILED =
  "Check your email — we sent a confirmation link. Open it to finish this action.";
const STEP_UP_SEND_FAILED = "Couldn't send the confirmation link. Try again in a moment.";
const STEP_UP_NEEDS_EMAIL =
  "Confirm this action from the link we email you — use “Email me a confirmation link”.";
const STEP_UP_NO_METHOD =
  "This account can't confirm: it has no password and email isn't set up on this server. Set a password or ask an admin to arm email.";

/**
 * The step-up METHOD for a session user: a password account re-enters its password; a
 * password-less account (magic-link/social) confirms by mail when the transport is armed, else
 * it has no confirmation rung at all. Each ceremony page resolves this once in its loader and
 * hands it to <StepUpMethodProvider> so the shared fields render the right rung.
 */
export async function stepUpMethod(userId: string): Promise<StepUpMethod> {
  if (await hasCredentialPassword(userId)) {
    return "password";
  }
  return mailDelivery().canSend ? "email" : "unavailable";
}

/**
 * Begin the mail confirmation: mint a single-use token (its hash stored server-side, scoped to
 * this ceremony page), and mail a link back to the ceremony page the request came from carrying
 * `?stepup=<token>`. Returns a NOT-OK result whose message tells the person to check their mail —
 * the act itself never runs on this submission; the returning link's token authorizes the next
 * one, on THIS page alone. The return path is derived from the request (the ceremony page the
 * fetcher posted to) and validated same-app. The mail rung exists ONLY for password-less
 * accounts — an account with a password re-enters it, always (the composed method is never a
 * caller's choice, so a live session cannot downgrade its own proof).
 */
export async function beginStepUpConfirmation(
  request: Request,
  userId: string,
  email: string,
): Promise<StepUpResult> {
  if (await hasCredentialPassword(userId)) {
    return { ok: false, error: STEP_UP_FAILED };
  }
  if (!mailDelivery().canSend) {
    return { ok: false, error: STEP_UP_NO_METHOD };
  }
  const returnTo = safeNextPath(new URL(request.url).pathname);
  const token = await mintStepUpConfirmation(userId, returnTo);
  const link = `${followBase(request)}${returnTo}?${STEP_UP_TOKEN_QUERY}=${encodeURIComponent(token)}`;
  try {
    await sendStepUpMail(email, link);
  } catch {
    return { ok: false, error: STEP_UP_SEND_FAILED };
  }
  return { ok: false, error: STEP_UP_EMAILED };
}

/**
 * Prove who is ACTING, immediately before the act. Anchored to the live cookie (the session's
 * user id), never a form field. The METHOD is resolved server-side FIRST (password account →
 * password re-entry, always; password-less → the mail round-trip): the rungs are never a menu,
 * so a stolen session cannot pick the weaker proof for an account that has the stronger one.
 * Whichever fails, the returned message matches the account's actual method, so an email-only
 * person is never told to "re-enter your password".
 */
export async function requireStepUp(request: Request, formData: FormData): Promise<StepUpResult> {
  const session = await requireSession(request);
  const userId = session.user.id;
  // The belt arms by the app's OWN env, the same keying the auth construction uses for its
  // sign-in limiter: production wears it; a dev/test run (whose suites drive many ceremonies
  // through one identity in seconds) does not — the bucket's math is unit-tested on its own.
  if (serverEnv().APP_ENV === "production" && !allowStepUpAttempt(userId)) {
    return { ok: false, error: STEP_UP_LIMITED };
  }
  const passworded = await hasCredentialPassword(userId);
  if (passworded) {
    // The ONE rung for a password account — the mail arms below are not reachable for it.
    const password = String(formData.get(STEP_UP_PASSWORD_FIELD) ?? "");
    if (await verifySessionPassword(userId, password)) {
      return { ok: true };
    }
    return { ok: false, error: STEP_UP_FAILED };
  }
  // Password-less account. Rung A — the email-confirmation REQUEST (the button): mint + mail a
  // link back to this ceremony page. The act never runs on this submission.
  if (String(formData.get(STEP_UP_INTENT_FIELD) ?? "") === STEP_UP_INTENT_EMAIL) {
    return beginStepUpConfirmation(request, userId, session.user.email);
  }
  // Rung B — the emailed token: consumed in ONE atomic statement, single-use by construction,
  // valid only for THIS ceremony page (the mint scoped it to the page it was requested on).
  const token = String(formData.get(STEP_UP_TOKEN_FIELD) ?? "");
  if (token.length > 0) {
    const scope = safeNextPath(new URL(request.url).pathname);
    const ok = await consumeStepUpConfirmation(userId, scope, token);
    return ok ? { ok: true } : { ok: false, error: STEP_UP_TOKEN_FAILED };
  }
  // No rung satisfied — answer with the message that matches the account's real method.
  return { ok: false, error: mailDelivery().canSend ? STEP_UP_NEEDS_EMAIL : STEP_UP_NO_METHOD };
}

/**
 * The destructive ceremonies' second factor of intent: the typed name must equal the
 * resource's CURRENT name exactly (trim only — case and hyphens are part of the name).
 */
export function requireTypedName(formData: FormData, expected: string): StepUpResult {
  const typed = String(formData.get(STEP_UP_CONFIRM_FIELD) ?? "").trim();
  if (typed !== expected) {
    return {
      ok: false,
      error: `Type the exact name (${expected}) to confirm — this action is not undoable from here.`,
    };
  }
  return { ok: true };
}
