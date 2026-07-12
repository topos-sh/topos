import { allowStepUpAttempt } from "@/lib/rate-limit.server";
import { requireSession } from "./guards.server";
import { verifySessionPassword } from "./server";

/**
 * STEP-UP — the re-authentication gate in front of the admin ceremonies (roster mutations,
 * skill lifecycle, purge, channel existence-admin, policy setters). A session is evidence of
 * who is browsing; these acts want evidence of who is ACTING, minted seconds ago: the person
 * re-enters their password inside the ceremony form and the action verifies it immediately
 * before the act. Deliberately STATELESS — no sudo-mode window, nothing to expire or steal;
 * every gated act re-proves. The destructive ceremonies (delete a skill, purge a version,
 * delete a channel) additionally require typing the resource's name.
 *
 * Order inside an action: guard (mint the actor) → requireStepUp → requireTypedName (where
 * destructive) → the act. A step-up failure returns a typed form error and performs NOTHING —
 * no vault call, no guarded-function call; the audit row (admin_event) still records the
 * refused attempt.
 */

/** The form field names the shared <StepUpFields> component renders. */
export const STEP_UP_PASSWORD_FIELD = "stepup_password";
export const STEP_UP_CONFIRM_FIELD = "confirm_name";

export type StepUpResult = { ok: true } | { ok: false; error: string };

/** Wrong-password and rate-limit copy — precise about what is being asked and why. */
const STEP_UP_FAILED =
  "Password check failed. Re-enter your account password to confirm this action.";
const STEP_UP_LIMITED = "Too many attempts. Wait a minute, then try again.";

/**
 * Verify the ceremony form's password re-entry against the SESSION's own account. The session
 * (not the branded actor) carries the better-auth user id the credential row hangs off — and
 * re-resolving it here keeps the check anchored to the live cookie, never a form field.
 */
export async function requireStepUp(request: Request, formData: FormData): Promise<StepUpResult> {
  const session = await requireSession(request);
  if (!allowStepUpAttempt(session.user.id)) {
    return { ok: false, error: STEP_UP_LIMITED };
  }
  const password = String(formData.get(STEP_UP_PASSWORD_FIELD) ?? "");
  const verified = await verifySessionPassword(session.user.id, password);
  if (!verified) {
    return { ok: false, error: STEP_UP_FAILED };
  }
  return { ok: true };
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
