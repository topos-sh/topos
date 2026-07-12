import { useCallback, useEffect, useRef, useState } from "react";

/**
 * The two-machine propagation demo: Maya publishes on one harness yesterday, Dev's agent
 * follows the updated skill on another harness this morning. A scripted, replayable
 * simulation of the product's actual behavior (a static screenshot cannot show propagation
 * over time). Typed prompts get a blinking caret; output lines fade in; reduced motion
 * renders the full transcript at once.
 */

type Pane = "a" | "b";
type Kind = "prompt" | "out" | "ok" | "note";
type Step = { pane: Pane; kind: Kind; text: string; typed?: boolean; pause: number };

const SCRIPT: Step[] = [
  {
    pane: "a",
    kind: "prompt",
    text: "we’re switching to canary deploys, update the deploy skill",
    typed: true,
    pause: 500,
  },
  {
    pane: "a",
    kind: "out",
    text: "● Updated the deploy skill: ship to 10% first, watch, then roll out to everyone.",
    pause: 900,
  },
  { pane: "a", kind: "prompt", text: "share it with the team", typed: true, pause: 700 },
  {
    pane: "a",
    kind: "ok",
    text: "✓ Published. Every follower gets it at their next session.",
    pause: 1500,
  },
  {
    pane: "b",
    kind: "note",
    text: "✓ Skill update from Maya: deploy now ships a 10% canary first.",
    pause: 1000,
  },
  { pane: "b", kind: "prompt", text: "deploy my branch", typed: true, pause: 700 },
  {
    pane: "b",
    kind: "out",
    text: "● Shipping to 10% first, per the updated deploy skill.",
    pause: 700,
  },
  { pane: "b", kind: "ok", text: "✓ Canary healthy. Rolled out to everyone.", pause: 400 },
];

const LINE_COLOR: Record<Kind, string> = {
  prompt: "text-glass-ink",
  out: "text-glass-dim",
  ok: "text-accent-phos",
  note: "text-glass-faint",
};

type Committed = { key: number; pane: Pane; kind: Kind; text: string };
type Typing = { pane: Pane; kind: Kind; text: string } | null;

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function Line({ kind, text, caret }: { kind: Kind; text: string; caret?: boolean }) {
  return (
    <span className={`block ${LINE_COLOR[kind]}`}>
      {kind === "prompt" && <span className="text-accent-phos">{"❯ "}</span>}
      {text}
      {caret && <span className="caret-blink text-accent-phos">{"▋"}</span>}
    </span>
  );
}

function Window({
  title,
  harness,
  lines,
  typing,
  pane,
}: {
  title: string;
  harness: string;
  lines: Committed[];
  typing: Typing;
  pane: Pane;
}) {
  return (
    <div className="overflow-hidden rounded-lg border border-glass-line bg-glass shadow-glass">
      <div className="flex items-center gap-1.5 border-glass-line border-b px-3 py-2.5">
        <span className="h-2.5 w-2.5 rounded-full bg-glass-line" />
        <span className="h-2.5 w-2.5 rounded-full bg-glass-line" />
        <span className="h-2.5 w-2.5 rounded-full bg-glass-line" />
        <span className="mr-10 flex-1 text-center font-mono text-[11px] text-glass-faint">
          <b className="font-medium text-glass-ink">{title}</b> {"·"} {harness}
        </span>
      </div>
      <pre className="min-h-0 whitespace-pre-wrap break-words p-4 pb-5 font-mono text-[12.75px] text-glass-dim leading-[1.75] lg:min-h-[205px]">
        {lines
          .filter((l) => l.pane === pane)
          .map((l) => (
            <Line key={l.key} kind={l.kind} text={l.text} />
          ))}
        {typing && typing.pane === pane && <Line kind={typing.kind} text={typing.text} caret />}
      </pre>
    </div>
  );
}

export function TerminalDemo() {
  const [lines, setLines] = useState<Committed[]>([]);
  const [typing, setTyping] = useState<Typing>(null);
  const generation = useRef(0);
  const started = useRef(false);
  const rootRef = useRef<HTMLDivElement>(null);

  const play = useCallback(() => {
    const gen = ++generation.current;
    setLines([]);
    setTyping(null);
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      setLines(SCRIPT.map((s, i) => ({ key: i, pane: s.pane, kind: s.kind, text: s.text })));
      return;
    }
    (async () => {
      let key = 0;
      for (const step of SCRIPT) {
        if (gen !== generation.current) {
          return;
        }
        if (step.typed) {
          for (let i = 1; i <= step.text.length; i++) {
            if (gen !== generation.current) {
              return;
            }
            setTyping({ pane: step.pane, kind: step.kind, text: step.text.slice(0, i) });
            await sleep(24);
          }
          setTyping(null);
        }
        if (gen !== generation.current) {
          return;
        }
        const next: Committed = { key: key++, pane: step.pane, kind: step.kind, text: step.text };
        setLines((prev) => [...prev, next]);
        await sleep(step.pause);
      }
    })();
  }, []);

  useEffect(() => {
    const el = rootRef.current;
    if (!el) {
      return;
    }
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries[0]?.isIntersecting && !started.current) {
          started.current = true;
          setTimeout(play, 400);
        }
      },
      { threshold: 0.35 },
    );
    observer.observe(el);
    return () => {
      observer.disconnect();
      generation.current += 1; // cancel any in-flight run on unmount
    };
  }, [play]);

  return (
    <div ref={rootRef}>
      <div className="mb-4 flex items-baseline justify-between">
        <div className="font-display text-[10.5px] text-dim uppercase tracking-[0.14em]">
          One edit, two people
        </div>
        <button
          type="button"
          onClick={play}
          className="font-mono text-[11px] text-faint uppercase tracking-[0.08em] transition-colors hover:text-ink focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
        >
          {"↻"} replay
        </button>
      </div>
      <div className="grid items-start gap-6 lg:grid-cols-2">
        <div>
          <Window title="Maya" harness="Claude Code" lines={lines} typing={typing} pane="a" />
          <p className="mt-2.5 text-[12.5px] text-faint">
            Yesterday: <b className="font-medium text-ink">Maya</b>, on Claude Code, updates the
            team{"’"}s deploy skill.
          </p>
        </div>
        <div className="lg:mt-[30px]">
          <Window title="Dev" harness="OpenClaw" lines={lines} typing={typing} pane="b" />
          <p className="mt-2.5 text-[12.5px] text-faint">
            This morning: <b className="font-medium text-ink">Dev</b>, on OpenClaw, starts his day.
          </p>
        </div>
      </div>
    </div>
  );
}
