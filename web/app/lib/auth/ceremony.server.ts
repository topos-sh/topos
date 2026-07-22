/**
 * CEREMONY CONFIRMATION — the admin ceremonies CONFIRM, they don't re-authenticate. A live
 * session is the authority (the guards mint the actor; the role gate rides its type); what a
 * ceremony adds is proportional CONFIRMATION of intent:
 *   - DESTRUCTIVE ceremonies (delete a skill, purge a version, delete a channel) require typing
 *     the resource's exact name — [`requireTypedName`] below, the server half of the typed
 *     confirm field.
 *   - Acts with cross-person reach (remove member, role change, revert, rename, archive) wear a
 *     lightweight in-place confirm in the UI (components/confirm.tsx) — first activation arms,
 *     the second performs; nothing server-side beyond the guard.
 *   - Settings/policy form saves are plain submits.
 *
 * Order inside an action: guard (mint the actor) → requireTypedName (where destructive) → the
 * act. A refused typed name returns a typed form error and performs NOTHING — no vault call, no
 * data-layer call; the ceremony records the refused attempt in its own admin_event row, exactly
 * as it records the landed act and the faults.
 */

/** The form field the typed-name confirm renders (mirrored in components/confirm.tsx — a
 * server module cannot be imported into the client bundle). */
export const CONFIRM_NAME_FIELD = "confirm_name";

export type CeremonyResult = { ok: true } | { ok: false; error: string };

/**
 * The destructive ceremonies' confirmation of intent: the typed name must equal the
 * resource's CURRENT name exactly (trim only — case and hyphens are part of the name).
 */
export function requireTypedName(formData: FormData, expected: string): CeremonyResult {
  const typed = String(formData.get(CONFIRM_NAME_FIELD) ?? "").trim();
  if (typed !== expected) {
    return {
      ok: false,
      error: `Type the exact name (${expected}) to confirm — this action is not undoable from here.`,
    };
  }
  return { ok: true };
}
