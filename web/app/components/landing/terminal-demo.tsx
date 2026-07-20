/**
 * The two-machine propagation demo: Maya publishes on one harness yesterday, Dev's agent
 * follows the updated skill on another harness this morning. A STATIC transcript — the whole
 * exchange is readable at a glance, nothing types, nothing waits, nothing moves.
 */

type Pane = "a" | "b";
type Kind = "prompt" | "out" | "ok" | "note";
type Step = { pane: Pane; kind: Kind; text: string };

const SCRIPT: Step[] = [
  {
    pane: "a",
    kind: "prompt",
    text: "we’re switching to canary deploys, update the deploy skill",
  },
  {
    pane: "a",
    kind: "out",
    text: "● Updated the deploy skill: ship to 10% first, watch, then roll out to everyone.",
  },
  { pane: "a", kind: "prompt", text: "share it with the team" },
  {
    pane: "a",
    kind: "ok",
    text: "✓ Published. Every follower gets it at their next session.",
  },
  {
    pane: "b",
    kind: "note",
    text: "✓ Skill update from Maya: deploy now ships a 10% canary first.",
  },
  { pane: "b", kind: "prompt", text: "deploy my branch" },
  {
    pane: "b",
    kind: "out",
    text: "● Shipping to 10% first, per the updated deploy skill.",
  },
  { pane: "b", kind: "ok", text: "✓ Canary healthy. Rolled out to everyone." },
];

const LINE_COLOR: Record<Kind, string> = {
  prompt: "text-glass-ink",
  out: "text-glass-dim",
  ok: "text-accent-phos",
  note: "text-glass-faint",
};

function Line({ kind, text }: { kind: Kind; text: string }) {
  return (
    <span className={`block ${LINE_COLOR[kind]}`}>
      {kind === "prompt" && <span className="text-accent-phos">{"❯ "}</span>}
      {text}
    </span>
  );
}

function Window({ title, harness, pane }: { title: string; harness: string; pane: Pane }) {
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
      <pre className="whitespace-pre-wrap break-words p-4 pb-5 font-mono text-[12.75px] text-glass-dim leading-[1.75]">
        {SCRIPT.filter((s) => s.pane === pane).map((s) => (
          <Line key={s.text} kind={s.kind} text={s.text} />
        ))}
      </pre>
    </div>
  );
}

export function TerminalDemo() {
  return (
    <div>
      <div className="mb-4 font-display text-[10.5px] text-dim uppercase tracking-[0.14em]">
        One edit, two people
      </div>
      <div className="grid items-start gap-6 lg:grid-cols-2">
        <div>
          <Window title="Maya" harness="Claude Code" pane="a" />
          <p className="mt-2.5 text-[12.5px] text-faint">
            Yesterday: <b className="font-medium text-ink">Maya</b>, on Claude Code, updates the
            team{"’"}s deploy skill.
          </p>
        </div>
        <div className="lg:mt-[30px]">
          <Window title="Dev" harness="OpenClaw" pane="b" />
          <p className="mt-2.5 text-[12.5px] text-faint">
            This morning: <b className="font-medium text-ink">Dev</b>, on OpenClaw, starts his day.
          </p>
        </div>
      </div>
    </div>
  );
}
