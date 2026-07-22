import { useEffect, useRef, useState } from "react";
import { buttonClasses } from "@/components/ui";

/**
 * The two confirmation controls the admin ceremonies wear — ceremonies CONFIRM, they don't
 * re-authenticate (the server half is app/lib/auth/ceremony.server.ts):
 *
 *   - [`ConfirmNameField`] — the DESTRUCTIVE ceremonies' type-the-name confirm (delete a
 *     skill, purge a version, delete a channel). Typing the resource's exact name IS the
 *     confirmation; the matching server gate re-checks it against server state.
 *   - [`ConfirmButton`] — the lightweight IN-PLACE confirm for acts with cross-person reach
 *     that aren't type-the-name gated (remove member, role change, revert, rename, archive).
 *     Deliberately NOT a modal: the action control itself swaps to a confirm state ("Remove —
 *     confirm?" beside a Cancel) and auto-reverts when focus leaves it or after a short
 *     timeout, so a stray click never acts and a changed mind costs nothing.
 *
 * Settings/policy form saves carry neither — a plain submit is the whole ceremony there.
 */

/** Must equal CONFIRM_NAME_FIELD in ceremony.server.ts (that module is server-only, so the
 * name is mirrored here rather than imported into the client bundle). */
export const CONFIRM_NAME = "confirm_name";

const FIELD_CLASSES =
  "block h-11 w-full min-w-56 rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";

/**
 * The type-the-name confirm — `typedName` is the exact string the server will require;
 * rendering it in the label keeps the ceremony honest about what confirming means.
 */
export function ConfirmNameField({ typedName, idPrefix }: { typedName: string; idPrefix: string }) {
  return (
    <label className="block" htmlFor={`${idPrefix}-confirm`}>
      <span className="mb-1 block font-medium text-sm text-dim">
        Type <code className="font-mono text-ink">{typedName}</code> to confirm
      </span>
      <input
        id={`${idPrefix}-confirm`}
        type="text"
        name={CONFIRM_NAME}
        required
        autoComplete="off"
        spellCheck={false}
        placeholder={typedName}
        className={FIELD_CLASSES}
      />
    </label>
  );
}

/** How long an armed confirm stays armed with focus elsewhere untouched. */
const ARM_TIMEOUT_MS = 8000;

/**
 * The in-place confirm button. At rest it renders `label` as the form's SUBMIT button whose
 * activation is intercepted — the first activation (click, or Enter anywhere in the form: as
 * the default button it also catches the browser's implicit submission, so a keyboard submit
 * cannot skip the ceremony) ARMS it, performing nothing. Armed, the control swaps in place to
 * `confirmLabel` (a real submit) beside a Cancel — the second activation submits the enclosing
 * form, exactly once (a synchronous latch swallows a double-activation before the fetcher's
 * pending state can catch up). Arming auto-reverts when focus leaves the pair, after
 * [`ARM_TIMEOUT_MS`], or the moment a submit goes pending, so the control always returns to
 * rest on its own. One component, used identically by every non-typed ceremony control; each
 * ceremony form carries at most one.
 */
export function ConfirmButton({
  label,
  confirmLabel,
  pendingLabel,
  tone = "primary",
  pending = false,
}: {
  /** The resting action label ("Remove", "Save role", "Rename channel"). */
  label: string;
  /** The armed label — defaults to `<label> — confirm?`. */
  confirmLabel?: string;
  /** Shown (disabled) while the submit is in flight — defaults to the resting label. */
  pendingLabel?: string;
  tone?: "primary" | "quiet" | "danger";
  pending?: boolean;
}) {
  const [armed, setArmed] = useState(false);
  const containerRef = useRef<HTMLSpanElement>(null);
  const confirmRef = useRef<HTMLButtonElement>(null);
  // The double-activation latch: set synchronously on the confirming activation, so a second
  // click/Enter in the instant before the fetcher reports pending cannot post twice.
  const submittedRef = useRef(false);

  // Arming moves focus onto the confirm submit (the arming button unmounts under the pointer),
  // which also keeps the blur watcher below honest during the swap — and re-opens the latch
  // for this fresh arm.
  useEffect(() => {
    if (armed) {
      submittedRef.current = false;
      confirmRef.current?.focus();
    }
  }, [armed]);

  // The stray-arm timeout: an armed confirm left alone reverts on its own.
  useEffect(() => {
    if (!armed) {
      return;
    }
    const timer = setTimeout(() => setArmed(false), ARM_TIMEOUT_MS);
    return () => clearTimeout(timer);
  }, [armed]);

  // A submit in flight ends the armed state — the control rests (disabled) until the form
  // settles and revalidation brings the fresh page state.
  useEffect(() => {
    if (pending) {
      setArmed(false);
    }
  }, [pending]);

  // Disarm when focus leaves the pair. Checked a frame later: mid-swap (and between the two
  // buttons) focus transiently sits on the body, and only a SETTLED outside focus disarms.
  function handleBlur() {
    requestAnimationFrame(() => {
      const container = containerRef.current;
      if (container !== null && !container.contains(document.activeElement)) {
        setArmed(false);
      }
    });
  }

  if (!armed) {
    // type="submit" so this is the form's DEFAULT button — Enter in any of the form's fields
    // routes through it — while the intercepted activation turns every such submit into an ARM.
    return (
      <button
        type="submit"
        disabled={pending}
        onClick={(event) => {
          event.preventDefault();
          setArmed(true);
        }}
        className={buttonClasses(tone)}
      >
        {pending ? (pendingLabel ?? label) : label}
      </button>
    );
  }
  return (
    <span ref={containerRef} className="inline-flex items-center gap-2">
      <button
        ref={confirmRef}
        type="submit"
        onClick={(event) => {
          // The latch: the first activation posts; anything after it (before pending disarms
          // the control) is swallowed instead of enqueuing a duplicate action.
          if (submittedRef.current) {
            event.preventDefault();
            return;
          }
          submittedRef.current = true;
        }}
        onBlur={handleBlur}
        className={buttonClasses(tone)}
      >
        {confirmLabel ?? `${label} — confirm?`}
      </button>
      <button
        type="button"
        onClick={() => setArmed(false)}
        onBlur={handleBlur}
        className={buttonClasses("quiet")}
      >
        Cancel
      </button>
    </span>
  );
}
