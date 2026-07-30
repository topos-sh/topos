import { type ReactElement, type RefObject, useCallback, useEffect, useRef, useState } from "react";
import { HarnessMark } from "@/components/landing/harness-marks";

/**
 * The landing page's fleet animation: the sticky rail of machine chips that sits beside the
 * publish and review scenes, plus the trigger discipline deciding WHEN a scene's beat plays.
 *
 * Four rules are encoded here, and every one of them was an invisible animation before it was a
 * rule — they are the load-bearing knowledge in this module:
 *
 *  1. Every "appear" beat runs on the Web Animations API, never a CSS class-toggle transition. A
 *     transition animates from the element's CURRENT value, so a replay while the element is
 *     already visible animates 1 → 1, i.e. nothing. WAAPI always plays its keyframes.
 *  2. A beat never fires while the document is hidden: browsers freeze rendering there, so the
 *     beat would play invisibly and be spent. It queues and fires on `visibilitychange`.
 *  3. No fire-once latch. Beats fire on EVERY entry into view (cooldown-deduped), and the scene
 *     resets to pristine when it leaves, so coming back replays the opening.
 *  4. An at-load fire is held ~1.7s. Browsers restore scroll position on reload, so an instant
 *     beat is spent during the load flash while nobody is looking yet.
 *
 * Reduced motion is honored throughout: state changes land instantly, nothing slides, no chip
 * flashes, and the attention pulse becomes a static ring. Sticky positioning is not motion and
 * stays. Nothing here touches scrolling — the animations are attention pointers, not a scroll
 * hijack.
 *
 * SSR-safe by construction: no window/document/matchMedia at module scope or during render. The
 * server renders the pristine state — which is the resting state a scriptless page keeps, since
 * the caller's resting styles live in CSS rather than in a hydration branch.
 */

const REDUCED_MOTION_QUERY = "(prefers-reduced-motion: reduce)";

/** Label enter/exit: long enough to read as a notification, short enough never to queue up. */
const LABEL_SLIDE_MS = 400;
/** The label box's own height — the exact distance a label travels in from below and out above. */
const LABEL_TRAVEL_PX = 13;
/** Notification-style: every label dismisses itself upward, so the fleet returns to rest. */
const LABEL_DISMISS_MS = 6000;
/** The chip's tint flash when its label changes — a pointer to where to look, not a state. */
const CHIP_LIT_MS = 750;
/** The chat row's "appear" pop. */
const POP_MS = 460;
/** Overshoot ease: the appear beats land with a small spring, the way a sent message does. */
const SPRING_EASE = "cubic-bezier(0.34, 1.56, 0.64, 1)";
/** Decelerating ease for the label slides — no overshoot inside a 13px window. */
const SLIDE_EASE = "cubic-bezier(0.22, 0.61, 0.36, 1)";
/**
 * The publish beat's one timing: the row pops in, and this long after the pop STARTS every
 * targeted machine updates together. No stagger — the simultaneity is what reads as delivery.
 * A caller sequences the beat as `play()` then `setTimeout(setLabels, POP_TO_LABELS_MS)`.
 */
export const POP_TO_LABELS_MS = 430;

const DEFAULT_COOLDOWN_MS = 4000;
const DEFAULT_HOLD_AT_LOAD_MS = 1700;
/** Fire once the scene is properly on screen; reset only once it is mostly gone. */
const ENTER_RATIO = 0.55;
const EXIT_RATIO = 0.35;

/** The label spans stack absolutely inside the chip's overflow-hidden label box. */
const LABEL_SPAN_CLASS = "absolute top-0 right-0 whitespace-nowrap";

type Timer = ReturnType<typeof setTimeout>;

/**
 * Read at CALL time, never during render — every caller below runs inside an effect or a DOM
 * event, which is what keeps this module server-renderable.
 */
function prefersReducedMotionNow(): boolean {
  return typeof window !== "undefined" && window.matchMedia(REDUCED_MOTION_QUERY).matches;
}

function canAnimate(el: Element | null): el is Element {
  return el !== null && typeof el.animate === "function";
}

export type FleetMachine = { name: string; harness: string; team: string };

/** Imperative controller for the fleet chips' notification labels. */
export type FleetController = {
  /** Notification-style label set on these machine indexes (new slides in from below, old slides up and out). */
  setLabels: (indexes: number[], text: string) => void;
  /** Slide the labels on these indexes up and away (used by resets/replays). */
  clearLabels: (indexes: number[]) => void;
};

/** What a mounted chip lends the controller: its own two elements plus its pending timers. */
type ChipSlot = {
  chip: HTMLElement;
  box: HTMLElement;
  current: HTMLSpanElement | null;
  dismiss: Timer | null;
  lit: Timer | null;
};

/**
 * The controller's guts stay OFF the exported type: callers get two verbs, `Fleet` gets the
 * registry, and nothing else can reach into the DOM the animation owns.
 */
type ControllerState = { slots: Map<number, ChipSlot>; timers: Set<Timer> };
const controllerState = new WeakMap<FleetController, ControllerState>();

/** Every timer this module opens is tracked, so unmount can sweep the whole set. */
function later(state: ControllerState, fn: () => void, ms: number): Timer {
  const timer = setTimeout(() => {
    state.timers.delete(timer);
    fn();
  }, ms);
  state.timers.add(timer);
  return timer;
}

function clearTimer(state: ControllerState, timer: Timer | null): void {
  if (timer !== null) {
    state.timers.delete(timer);
    clearTimeout(timer);
  }
}

/**
 * Slide the chip's current label up and away, and take it OUT of the DOM once the exit finishes
 * — an accumulating stack of spent labels inside a 13px box would fight the next enter.
 */
function slideLabelOut(state: ControllerState, slot: ChipSlot, reduce: boolean): void {
  const old = slot.current;
  if (!old) {
    return;
  }
  slot.current = null;
  if (reduce || !canAnimate(old)) {
    old.remove();
    return;
  }
  const exit = old.animate(
    [
      { opacity: 1, transform: "translateY(0)" },
      { opacity: 0, transform: `translateY(-${LABEL_TRAVEL_PX}px)` },
    ],
    { duration: LABEL_SLIDE_MS, easing: SLIDE_EASE, fill: "forwards" },
  );
  exit.onfinish = () => old.remove();
  // A frozen tab can leave `onfinish` unfired; the sweep-tracked fallback removes it regardless.
  later(state, () => old.remove(), LABEL_SLIDE_MS + 150);
}

/**
 * The notification swap: the old label slides up and away while the new one slides in from
 * below — visibly correct even when the target chip already carries a label, which is exactly
 * the overlap the second scene is built to show.
 */
function setChipLabel(state: ControllerState, index: number, text: string): void {
  const slot = state.slots.get(index);
  if (!slot) {
    return;
  }
  const reduce = prefersReducedMotionNow();
  clearTimer(state, slot.dismiss);
  slot.dismiss = null;
  slideLabelOut(state, slot, reduce);

  const next = document.createElement("span");
  next.className = LABEL_SPAN_CLASS;
  next.textContent = text;
  slot.box.appendChild(next);
  slot.current = next;
  if (!reduce && canAnimate(next)) {
    next.animate(
      [
        { opacity: 0, transform: `translateY(${LABEL_TRAVEL_PX}px)` },
        { opacity: 1, transform: "translateY(0)" },
      ],
      { duration: LABEL_SLIDE_MS, easing: SLIDE_EASE },
    );
    // The tint flash is decoration, so it is the one beat allowed to ride the chip's own
    // token-safe transition; under reduced motion it simply never arms.
    clearTimer(state, slot.lit);
    slot.chip.setAttribute("data-lit", "1");
    slot.lit = later(
      state,
      () => {
        slot.lit = null;
        slot.chip.removeAttribute("data-lit");
      },
      CHIP_LIT_MS,
    );
  }
  slot.dismiss = later(
    state,
    () => {
      slot.dismiss = null;
      slideLabelOut(state, slot, prefersReducedMotionNow());
    },
    LABEL_DISMISS_MS,
  );
}

function clearChipLabel(state: ControllerState, index: number): void {
  const slot = state.slots.get(index);
  if (!slot) {
    return;
  }
  clearTimer(state, slot.dismiss);
  slot.dismiss = null;
  slideLabelOut(state, slot, prefersReducedMotionNow());
}

/**
 * Creates a stable controller. Call in the parent; pass to <Fleet> and to the beat callbacks.
 * Stable identity matters twice: the beat callbacks close over it, and `<Fleet>` registers its
 * chips against it — a controller that changed identity would strand the registry. Calls made
 * before the chips mount (or after they unmount) are safe no-ops.
 */
export function useFleetController(): FleetController {
  const holder = useRef<FleetController | null>(null);
  const controller = holder.current ?? createFleetController();
  holder.current = controller;

  useEffect(() => {
    return () => {
      const state = controllerState.get(controller);
      if (!state) {
        return;
      }
      for (const timer of state.timers) {
        clearTimeout(timer);
      }
      state.timers.clear();
      for (const slot of state.slots.values()) {
        slot.dismiss = null;
        slot.lit = null;
      }
    };
  }, [controller]);

  return controller;
}

function createFleetController(): FleetController {
  const state: ControllerState = { slots: new Map(), timers: new Set() };
  const controller: FleetController = {
    setLabels(indexes, text) {
      for (const index of indexes) {
        setChipLabel(state, index, text);
      }
    },
    clearLabels(indexes) {
      for (const index of indexes) {
        clearChipLabel(state, index);
      }
    },
  };
  controllerState.set(controller, state);
  return controller;
}

/**
 * The sticky rail's labeled machine chips, in ONE aligned column — no offsets, no parallax: the
 * story is that updates land together, and drifting chips read as chips drifting.
 */
export function Fleet({
  machines,
  controller,
  heading,
  footnote,
}: {
  machines: FleetMachine[];
  controller: FleetController;
  /** Micro-label above the column, e.g. "The team's machines". */
  heading: string;
  /** Small faint line under the column, e.g. "the fleet holds beside the scenes". */
  footnote?: string;
}): ReactElement {
  return (
    <div>
      <p className="mb-2.5 font-display text-[9.5px] text-faint uppercase tracking-[0.12em]">
        {heading}
      </p>
      <div className="flex flex-col gap-2.5">
        {machines.map((machine, index) => (
          <FleetChip
            key={`${machine.name}-${machine.harness}`}
            controller={controller}
            index={index}
            machine={machine}
          />
        ))}
      </div>
      {footnote ? (
        <p className="mt-3 pl-0.5 font-mono text-[9px] text-hairline uppercase tracking-[0.1em]">
          {footnote}
        </p>
      ) : null}
    </div>
  );
}

/**
 * One chip lends its elements to the controller for as long as it is mounted — registration on
 * mount, removal on unmount — so the imperative label writes can never outlive the DOM they
 * address.
 */
function FleetChip({
  machine,
  index,
  controller,
}: {
  machine: FleetMachine;
  index: number;
  controller: FleetController;
}): ReactElement {
  const chipRef = useRef<HTMLDivElement | null>(null);
  const boxRef = useRef<HTMLSpanElement | null>(null);

  useEffect(() => {
    const state = controllerState.get(controller);
    const chip = chipRef.current;
    const box = boxRef.current;
    if (!state || !chip || !box) {
      return;
    }
    state.slots.set(index, { chip, box, current: null, dismiss: null, lit: null });
    return () => {
      const slot = state.slots.get(index);
      if (slot) {
        clearTimer(state, slot.dismiss);
        clearTimer(state, slot.lit);
        state.slots.delete(index);
      }
    };
  }, [controller, index]);

  return (
    <div
      ref={chipRef}
      className="flex items-center justify-between gap-2.5 rounded-[11px] border border-line-soft bg-panel px-3 py-2 shadow-card transition-[background-color,border-color] duration-500 data-[lit]:border-accent/25 data-[lit]:bg-accent-wash"
    >
      <span className="flex flex-col">
        <span className="flex items-center gap-1.5 font-mono text-[11px] text-ink">
          {machine.name} ·
          <HarnessMark name={machine.harness} className="h-3 w-3 shrink-0" />
          {machine.harness}
        </span>
        <span className="text-[10px] text-faint">{machine.team}</span>
      </span>
      <span
        ref={boxRef}
        className="relative h-[13px] min-w-[128px] overflow-hidden text-right font-mono text-[9px] text-accent uppercase tracking-[0.06em]"
      />
    </div>
  );
}

/**
 * The scroll/visibility trigger discipline — the four rules in the header, in one hook. Returns
 * a `fire` that BYPASSES the cooldown (wire it to the ↻ resend control); the caller keeps its
 * own reset if it needs one.
 *
 * Observe an element SHORTER than the viewport: the fire threshold is an intersection RATIO, and
 * a scene taller than the screen can never reach it.
 */
export function useSceneBeat({
  ref,
  onFire,
  onReset,
  cooldownMs = DEFAULT_COOLDOWN_MS,
  holdAtLoadMs = DEFAULT_HOLD_AT_LOAD_MS,
}: {
  /** The element observed for entry into view. */
  ref: RefObject<HTMLElement | null>;
  /** Runs on every entry into view (cooldown-deduped) and on `fire()`. */
  onFire: () => void;
  /** Runs when the scene leaves view (ratio < 0.35) so the next entry replays the opening. */
  onReset: () => void;
  cooldownMs?: number;
  holdAtLoadMs?: number;
}): { fire: () => void } {
  const onFireRef = useRef(onFire);
  const onResetRef = useRef(onReset);
  const fireRef = useRef<(() => void) | null>(null);

  // Kept fresh in their own effect (declared first, so it commits before the observer below):
  // the observer must never be torn down and rebuilt just because a callback identity changed.
  useEffect(() => {
    onFireRef.current = onFire;
    onResetRef.current = onReset;
  }, [onFire, onReset]);

  useEffect(() => {
    const el = ref.current;
    if (!el) {
      return;
    }
    const mountedAt = Date.now();
    const timers = new Set<Timer>();
    let cooldownUntil = 0;
    let queuedWhileHidden = false;
    let inView = false;

    function tryFire(bypassCooldown: boolean): void {
      // Rule 2 — a hidden document freezes rendering: queue the beat instead of spending it.
      if (document.visibilityState === "hidden") {
        queuedWhileHidden = true;
        return;
      }
      const now = Date.now();
      if (!bypassCooldown && now < cooldownUntil) {
        return;
      }
      cooldownUntil = now + cooldownMs;
      queuedWhileHidden = false;
      // Rule 4 — scroll restoration can put the scene in view at load; hold the beat until the
      // load flash is over so it lands while somebody is watching.
      const sinceLoad = now - mountedAt;
      if (sinceLoad < holdAtLoadMs) {
        const timer = setTimeout(() => {
          timers.delete(timer);
          onFireRef.current();
        }, holdAtLoadMs - sinceLoad);
        timers.add(timer);
        return;
      }
      onFireRef.current();
    }

    function onVisibilityChange(): void {
      if (document.visibilityState === "visible" && queuedWhileHidden) {
        tryFire(false);
      }
    }
    document.addEventListener("visibilitychange", onVisibilityChange);

    // Rule 3 — no latch: entry fires, exit resets, so returning replays the opening.
    let observer: IntersectionObserver | null = null;
    if (typeof IntersectionObserver === "function") {
      observer = new IntersectionObserver(
        (entries) => {
          for (const entry of entries) {
            if (entry.intersectionRatio >= ENTER_RATIO) {
              if (!inView) {
                inView = true;
                tryFire(false);
              }
            } else if (entry.intersectionRatio < EXIT_RATIO && inView) {
              inView = false;
              onResetRef.current();
            }
          }
        },
        { threshold: [EXIT_RATIO, ENTER_RATIO] },
      );
      observer.observe(el);
    }

    fireRef.current = () => tryFire(true);

    return () => {
      fireRef.current = null;
      document.removeEventListener("visibilitychange", onVisibilityChange);
      observer?.disconnect();
      for (const timer of timers) {
        clearTimeout(timer);
      }
      timers.clear();
    };
  }, [ref, cooldownMs, holdAtLoadMs]);

  const fire = useCallback(() => {
    fireRef.current?.();
  }, []);

  return { fire };
}

/**
 * The WAAPI "appear" pop for a chat row that must animate on EVERY fire, including replays —
 * rule 1: a class-toggle transition would animate 1 → 1 on a replay and show nothing.
 *
 * `sent` drives the pristine hidden state and the server renders `sent === false`, so the
 * static/no-JS fallback must never depend on this component.
 */
export function usePopIn(): {
  ref: RefObject<HTMLDivElement | null>;
  sent: boolean;
  play: () => void;
  reset: () => void;
} {
  const ref = useRef<HTMLDivElement | null>(null);
  const [sent, setSent] = useState(false);

  const play = useCallback(() => {
    setSent(true);
    const el = ref.current;
    if (!canAnimate(el) || prefersReducedMotionNow()) {
      return;
    }
    el.animate(
      [
        { opacity: 0, transform: "translateY(14px) scale(0.9)" },
        { opacity: 1, transform: "none" },
      ],
      { duration: POP_MS, easing: SPRING_EASE },
    );
  }, []);

  const reset = useCallback(() => setSent(false), []);

  return { ref, sent, play, reset };
}

/** True only after mount, and only when the user asks for reduced motion. */
export function usePrefersReducedMotion(): boolean {
  const [reduced, setReduced] = useState(false);

  useEffect(() => {
    const query = window.matchMedia(REDUCED_MOTION_QUERY);
    setReduced(query.matches);
    const onChange = (event: MediaQueryListEvent) => setReduced(event.matches);
    query.addEventListener("change", onChange);
    return () => query.removeEventListener("change", onChange);
  }, []);

  return reduced;
}

/**
 * The ↻ glyph for the resend/replay controls — an inline SVG, because the system's sigils are
 * drawn, never typed.
 */
export function ReplayGlyph({ className }: { className?: string }): ReactElement {
  return (
    <svg
      viewBox="0 0 24 24"
      aria-hidden="true"
      fill="none"
      stroke="currentColor"
      strokeWidth="2.4"
      strokeLinecap="round"
      className={className ?? "h-2.5 w-2.5"}
    >
      <path d="M3 12a9 9 0 1 0 3-6.7" />
      <path d="M3 4v5h5" />
    </svg>
  );
}
