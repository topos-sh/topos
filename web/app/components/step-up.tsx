/**
 * The STEP-UP fields the admin ceremonies embed in their forms: a password re-entry, plus a
 * type-the-name confirm on the destructive ceremonies (delete a skill, purge a version, delete
 * a channel). The matching server gate is app/lib/auth/step-up.server.ts — an action verifies
 * the re-entered password (and the typed name) immediately before the act, so these fields are
 * the visible half of one contract. Field names must match the server's constants.
 */

/** Must equal STEP_UP_PASSWORD_FIELD / STEP_UP_CONFIRM_FIELD in step-up.server.ts (that module
 * is server-only, so the names are mirrored here rather than imported into the client bundle). */
export const STEP_UP_PASSWORD_NAME = "stepup_password";
export const STEP_UP_CONFIRM_NAME = "confirm_name";

const FIELD_CLASSES =
  "block h-11 w-full min-w-56 rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";

/**
 * Password re-entry (+ optional type-the-name). `typedName` is the exact string the server
 * will require — rendering it in the label keeps the ceremony honest about what confirming
 * means. Autocomplete stays "current-password" so a password manager can fill it: step-up
 * verifies presence at the keyboard-and-vault level, not memory.
 */
export function StepUpFields({ typedName, idPrefix }: { typedName?: string; idPrefix: string }) {
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
      <label className="block" htmlFor={`${idPrefix}-password`}>
        <span className="mb-1 block font-medium text-sm text-dim">Confirm with your password</span>
        <input
          id={`${idPrefix}-password`}
          type="password"
          name={STEP_UP_PASSWORD_NAME}
          required
          autoComplete="current-password"
          className={FIELD_CLASSES}
        />
      </label>
    </div>
  );
}
