/**
 * Ceremony announcements — a GENERIC browser event dispatched at the app's milestone moments
 * (a workspace born, a device approved, an onboarding step landing). This is a
 * deployment-generic integration point: vendor-free, config-free, and inert unless something
 * listens — the OSS app attaches NO listener, so out of the box every announcement dispatches
 * into silence. A downstream composition may observe `topos:ceremony` on `window`; this app
 * neither knows nor cares whether anything is on the other end.
 */

/** The event name a listener subscribes to on `window`. */
export const CEREMONY_EVENT = "topos:ceremony";

/** The ceremony moments the app announces. */
export type CeremonyKind =
  | "workspace_created"
  | "device_approved"
  | "checklist_step_completed"
  | "checklist_dismissed"
  | "first_publish_seen";

/**
 * Dispatch ONE ceremony announcement on `window` as a `CustomEvent` whose detail is the kind
 * plus any extra fields. Server-safe: with no `window` (SSR) this is a no-op — announcements
 * are a browser-only afterthought, never part of rendering. Call sites own their own
 * once-per-occurrence discipline (a ref, or `newlyCompleted` below).
 */
export function announceCeremony(
  kind: CeremonyKind,
  detail?: Record<string, string | number>,
): void {
  if (typeof window === "undefined") {
    return;
  }
  window.dispatchEvent(new CustomEvent(CEREMONY_EVENT, { detail: { kind, ...detail } }));
}

/**
 * The step-transition dedupe for effect-driven announcements: given the PREVIOUS observation
 * of boolean step flags (`null` on the mount baseline) and the CURRENT one, name the steps
 * that flipped incomplete → complete. The mount baseline flips nothing (a step already done
 * when the page arrived announces nothing), and an unchanged observation — dev strict-mode
 * re-running an effect included — yields the empty list, so a dispatch driven by this can
 * never double-fire within a page lifetime.
 */
export function newlyCompleted(
  before: Readonly<Record<string, boolean>> | null,
  now: Readonly<Record<string, boolean>>,
): string[] {
  if (before === null) {
    return [];
  }
  return Object.keys(now).filter((step) => now[step] === true && before[step] !== true);
}
