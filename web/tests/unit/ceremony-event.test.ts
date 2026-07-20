import { afterEach, describe, expect, it, vi } from "vitest";
import { announceCeremony, CEREMONY_EVENT, newlyCompleted } from "@/lib/ceremony-event";

/**
 * The generic ceremony-event seam: server-safe no-op without a `window`, one `topos:ceremony`
 * CustomEvent per call with the kind folded into the detail, and the step-transition dedupe
 * (`newlyCompleted`) that keeps effect-driven announcements to ONE dispatch per real flip —
 * the mount baseline and a strict-mode-style re-run of the same observation dispatch nothing.
 * (There is no component-test harness in this suite — the vitest environment is node — so the
 * transition/dedupe behavior is pinned here on the helper the components drive.)
 */

/** A stand-in `window`: an EventTarget recording every ceremony detail it hears. */
function listeningWindow(): { heard: unknown[] } {
  const target = new EventTarget();
  const heard: unknown[] = [];
  target.addEventListener(CEREMONY_EVENT, (event) => {
    heard.push((event as CustomEvent).detail);
  });
  vi.stubGlobal("window", target);
  return { heard };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("announceCeremony", () => {
  it("is a no-op with no window (SSR)", () => {
    expect("window" in globalThis).toBe(false);
    expect(() => announceCeremony("workspace_created")).not.toThrow();
  });

  it("dispatches ONE topos:ceremony CustomEvent with the kind as the detail", () => {
    const { heard } = listeningWindow();
    announceCeremony("workspace_created");
    expect(heard).toEqual([{ kind: "workspace_created" }]);
  });

  it("folds extra detail fields in beside the kind", () => {
    const { heard } = listeningWindow();
    announceCeremony("checklist_step_completed", { step: "publish_skill" });
    expect(heard).toEqual([{ kind: "checklist_step_completed", step: "publish_skill" }]);
  });

  it("the event name is the documented constant", () => {
    expect(CEREMONY_EVENT).toBe("topos:ceremony");
  });
});

describe("newlyCompleted (the step-transition dedupe)", () => {
  it("the mount baseline flips nothing — even steps already done", () => {
    expect(newlyCompleted(null, { a: true, b: false })).toEqual([]);
  });

  it("names exactly the steps that flipped false→true", () => {
    expect(newlyCompleted({ a: false, b: false, c: true }, { a: true, b: false, c: true })).toEqual(
      ["a"],
    );
    expect(newlyCompleted({ a: false, b: false }, { a: true, b: true })).toEqual(["a", "b"]);
  });

  it("an unchanged observation yields nothing (a re-run cannot double-fire)", () => {
    expect(newlyCompleted({ a: true, b: false }, { a: true, b: false })).toEqual([]);
  });

  it("a step going back to incomplete announces nothing", () => {
    expect(newlyCompleted({ a: true }, { a: false })).toEqual([]);
  });

  it("drives a SINGLE dispatch across a strict-mode-style doubled effect", () => {
    const { heard } = listeningWindow();
    // The component pattern: a ref holds the last observation; each effect run announces the
    // flips and records what it saw. Every run is REPLAYED (run twice back-to-back) the way
    // dev strict-mode re-fires effects.
    let seen: Record<string, boolean> | null = null;
    const observe = (now: Record<string, boolean>) => {
      for (const step of newlyCompleted(seen, now)) {
        announceCeremony("checklist_step_completed", { step });
      }
      seen = now;
    };
    const observeDoubled = (now: Record<string, boolean>) => {
      observe(now);
      observe(now);
    };
    observeDoubled({ publish_skill: false }); // mount baseline — silent
    expect(heard).toEqual([]);
    observeDoubled({ publish_skill: true }); // the one real transition
    observeDoubled({ publish_skill: true }); // a later unchanged re-render
    expect(heard).toEqual([{ kind: "checklist_step_completed", step: "publish_skill" }]);
  });
});
