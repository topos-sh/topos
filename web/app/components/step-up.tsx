import { createContext, type ReactNode, useContext } from "react";
import { useSearchParams } from "react-router";
import { buttonClasses } from "@/components/ui";

/**
 * The STEP-UP fields the admin ceremonies embed in their forms: a re-authentication rung, plus a
 * type-the-name confirm on the destructive ceremonies (delete a skill, purge a version, delete a
 * channel). The matching server gate is app/lib/auth/step-up.server.ts — an action verifies the
 * rung (and the typed name) immediately before the act, so these fields are the visible half of
 * one contract. Field names must match the server's constants.
 *
 * WHICH rung shows is the person's step-up METHOD, resolved server-side (step-up.server's
 * `stepUpMethod`) and provided once per ceremony page through [`StepUpMethodProvider`]:
 *   - `password` — the account has a password; re-enter it (the OSS default, since every account
 *     is born with one). This is the context default, so a page without a provider — and the
 *     whole vanilla OSS build — renders exactly the password re-entry.
 *   - `email` — a password-less account (a magic-link/social deployment): confirm through a link
 *     mailed to the session address. The button posts the email-request intent; returning from
 *     the mailed link carries `?stepup=<token>` on the URL, which this component surfaces as a
 *     hidden token field + a "confirmed by email" note the ceremony submit then consumes.
 *   - `unavailable` — no password AND no armed mail: an honest dead-end note (never a silent
 *     failure). The server refuses the submit with the matching typed reason.
 */

/** Must equal the field/query constants in step-up.server.ts (that module is server-only, so the
 * names are mirrored here rather than imported into the client bundle). */
export const STEP_UP_PASSWORD_NAME = "stepup_password";
export const STEP_UP_CONFIRM_NAME = "confirm_name";
export const STEP_UP_TOKEN_NAME = "stepup_token";
export const STEP_UP_INTENT_NAME = "stepup_intent";
export const STEP_UP_INTENT_EMAIL = "request-email";
/** The query param the mailed confirmation link returns on — read from the URL, not a form. */
export const STEP_UP_TOKEN_QUERY = "stepup";

export type StepUpMethod = "password" | "email" | "unavailable";

/**
 * The step-up METHOD for the signed-in person, resolved server-side and provided once per
 * ceremony page. The default "password" keeps every StepUpFields outside a provider — and the
 * entire OSS build, where accounts always have a password — rendering the password re-entry.
 */
const StepUpMethodContext = createContext<StepUpMethod>("password");

export function StepUpMethodProvider({
  method,
  children,
}: {
  method: StepUpMethod;
  children: ReactNode;
}) {
  return <StepUpMethodContext.Provider value={method}>{children}</StepUpMethodContext.Provider>;
}

const FIELD_CLASSES =
  "block h-11 w-full min-w-56 rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";

/**
 * The re-authentication rung (+ optional type-the-name). `typedName` is the exact string the
 * server will require — rendering it in the label keeps the ceremony honest about what confirming
 * means. The password rung keeps autoComplete "current-password" so a password manager can fill
 * it: step-up verifies presence at the keyboard-and-vault level, not memory.
 */
export function StepUpFields({ typedName, idPrefix }: { typedName?: string; idPrefix: string }) {
  const method = useContext(StepUpMethodContext);
  const [params] = useSearchParams();
  const emailedToken = params.get(STEP_UP_TOKEN_QUERY);
  return (
    <div className="space-y-3">
      {typedName !== undefined && (
        <label className="block" htmlFor={`${idPrefix}-confirm`}>
          <span className="mb-1 block font-medium text-sm text-dim">
            Type <code className="font-mono text-ink">{typedName}</code> to confirm
          </span>
          <input
            id={`${idPrefix}-confirm`}
            type="text"
            name={STEP_UP_CONFIRM_NAME}
            required
            autoComplete="off"
            spellCheck={false}
            placeholder={typedName}
            className={FIELD_CLASSES}
          />
        </label>
      )}
      {method === "password" && (
        <label className="block" htmlFor={`${idPrefix}-password`}>
          <span className="mb-1 block font-medium text-sm text-dim">
            Confirm with your password
          </span>
          <input
            id={`${idPrefix}-password`}
            type="password"
            name={STEP_UP_PASSWORD_NAME}
            required
            autoComplete="current-password"
            className={FIELD_CLASSES}
          />
        </label>
      )}
      {method === "email" &&
        (emailedToken !== null ? (
          <>
            <input type="hidden" name={STEP_UP_TOKEN_NAME} value={emailedToken} />
            <p className="font-medium text-dim text-sm" role="status">
              Confirmed by email — complete the action below.
            </p>
          </>
        ) : (
          <div className="space-y-2">
            <p className="text-dim text-sm">
              This account signs in without a password. Confirm this action from a link we email to
              your address.
            </p>
            <button
              type="submit"
              name={STEP_UP_INTENT_NAME}
              value={STEP_UP_INTENT_EMAIL}
              className={buttonClasses("quiet")}
            >
              Email me a confirmation link
            </button>
          </div>
        ))}
      {method === "unavailable" && (
        <p className="text-red-600 text-sm" role="alert">
          This account has no password to confirm with, and email isn&apos;t set up on this server.
          Set a password or ask an admin to arm email, then try again.
        </p>
      )}
    </div>
  );
}
