import { MonitorIcon } from "lucide-react";
import { type ReactNode, type RefObject, useCallback, useEffect, useRef, useState } from "react";
import {
  Fleet,
  type FleetMachine,
  POP_TO_LABELS_MS,
  ReplayGlyph,
  useFleetController,
  usePopIn,
  usePrefersReducedMotion,
  useSceneBeat,
} from "@/components/landing/fleet-animation";
import { HarnessMark } from "@/components/landing/harness-marks";
import { MicroLabel, SectionHeading, WRAP } from "@/components/landing/landing-kit";

/**
 * Bands 4 and 5 — the loop (publish → delivered → improved) and the browser review — in TWO
 * implementations over ONE set of copy:
 *
 *  - the FLEET RAIL (`rail`): a sticky column of five labeled machines holding beside both bands
 *    while the left side scrolls, so delivery is something the visitor watches happen. Publish
 *    lands on the customer-facing machines; the Approve click lands on everyone who writes in the
 *    brand's voice — and one machine is in both sets, so its label visibly swaps.
 *  - the STATIC bands: the same steps, chat, receipts, and review frame with a plain fan-out
 *    table where the rail would be. It is the narrow-screen layout, the scriptless layout, and
 *    the one-line revert.
 *
 * Nothing here hijacks scrolling; the rail is `position: sticky` and the beats are attention
 * pointers, one moment at a time (precedent: Stripe's and Linear's scroll-driven panels).
 */

/** The five machines the rail shows, in one aligned column — no offsets, no parallax. */
export const FLEET_MACHINES: FleetMachine[] = [
  { name: "Sam", harness: "OpenClaw", team: "Support" },
  { name: "Kim", harness: "OpenClaw", team: "Marketing" },
  { name: "Dev", harness: "Claude Code", team: "Engineering" },
  { name: "Ana", harness: "Hermes", team: "Design" },
  { name: "Priya", harness: "Hermes", team: "Sales" },
];

/** Publish reaches the people whose work touches customers: Sam, Dev, Priya. */
const DELIVERED = [0, 2, 4];
/**
 * Approve reaches everyone who writes in the brand's voice: Kim, Ana, Priya. Priya is in BOTH
 * sets on purpose — routing overlaps in a real company, and her label swapping (old one sliding
 * up and away as the new one arrives) is what makes the second beat legible.
 */
const APPROVED = [1, 3, 4];
const DELIVERED_LABEL = "customer-lookup added";
const APPROVED_LABEL = "brand-voice updated";

/** The rail-free stand-in for the fleet: exactly the machines the publish reached. */
const FANOUT_ROWS: FleetMachine[] = DELIVERED.map((index) => FLEET_MACHINES[index]).filter(
  (machine): machine is FleetMachine => machine !== undefined,
);

const DELIVERED_RECEIPT = "Published: delivered to Support, Sales, and Engineering";
const APPROVED_RECEIPT = "Approved: Marketing, Design, and Sales agents run it now";

/**
 * Without scripts no beat ever runs, so reveal every resting state and hand the fan-out table the
 * job the rail would have done. Reduced motion is handled in CSS beside `.landing-reveal` itself.
 */
const SCRIPTLESS_CSS = `.landing-reveal{opacity:1!important}
.landing-rail{display:none!important}
.landing-fanout-slot{display:block!important}
.landing-pulse{animation:none!important}`;

export function LoopBands({ rail }: { rail: boolean }) {
  return rail ? <RailBands /> : <StaticBands />;
}

/* ───────────────────────── the shared copy ───────────────────────── */

const STEPS: { title: string; body: ReactNode }[] = [
  {
    title: "Publish",
    body: (
      <>
        Engineering ships an internal tool. Maya tells her agent to teach the team:{" "}
        <Code>customer-lookup</Code>.
      </>
    ),
  },
  {
    title: "Delivered automatically",
    body: (
      <>
        The agents whose work touches customers pick it up: Support, Sales, Engineering. Nobody
        installs anything.
      </>
    ),
  },
  {
    title: "Improved, then reviewed",
    body: (
      <>
        Sam’s agent in Support proposes adding the refunds view. Maya approves; every agent knows
        it.
      </>
    ),
  },
];

function LoopCopy() {
  return (
    <>
      <MicroLabel>How it works</MicroLabel>
      <SectionHeading className="max-w-[46ch]">
        Publish once. The right agents get it. Improvements come back for review.
      </SectionHeading>
      <ol className="mt-6 grid gap-3 sm:grid-cols-3">
        {STEPS.map((step, index) => (
          <li
            key={step.title}
            className="rounded-lg border border-line-soft bg-panel px-4 py-3.5 shadow-card"
          >
            <span className="mb-2.5 inline-flex h-[22px] w-[22px] items-center justify-center rounded-[5px] bg-accent font-mono font-semibold text-[11.5px] text-on-accent">
              {index + 1}
            </span>
            <span className="mb-[5px] block font-display font-semibold text-[13px] text-ink tracking-[-0.01em]">
              {step.title}
            </span>
            <p className="text-[12.5px] text-dim leading-[1.55]">{step.body}</p>
          </li>
        ))}
      </ol>
      {/* The loop-closing note speaks in the receipts' voice — the founder's call. */}
      <p className="mt-2.5 pl-1 font-mono text-[10.5px] text-accent">
        ↩&nbsp; approved fixes publish back to everyone the same way
      </p>
    </>
  );
}

function ReviewCopy() {
  return (
    <>
      <MicroLabel>No terminal required</MicroLabel>
      <SectionHeading>Review every improvement in the browser.</SectionHeading>
    </>
  );
}

/* ───────────────────────── the two band assemblies ───────────────────────── */

/**
 * The rail layout: one grid spanning both bands, the fleet sticky in the right column. Below `lg`
 * the rail leaves the layout entirely and the fan-out table takes over — a CSS decision, so the
 * server's first paint is already the right one at every width.
 */
function RailBands() {
  const fleet = useFleetController();
  const reduced = usePrefersReducedMotion();
  const { ref: yesRef, sent, play, reset } = usePopIn();
  const [published, setPublished] = useState(false);
  const [approved, setApproved] = useState(false);
  const sceneRef = useRef<HTMLDivElement>(null);
  const handoff = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(
    () => () => {
      if (handoff.current) clearTimeout(handoff.current);
    },
    [],
  );

  // The publish beat: "Yes." pops into the chat, then every targeted machine updates together.
  const publish = useCallback(() => {
    play();
    if (handoff.current) clearTimeout(handoff.current);
    handoff.current = setTimeout(
      () => {
        fleet.setLabels(DELIVERED, DELIVERED_LABEL);
        setPublished(true);
      },
      reduced ? 0 : POP_TO_LABELS_MS,
    );
  }, [fleet, play, reduced]);

  // Leaving the viewport rewinds the scene so the next arrival replays the opening.
  const rewind = useCallback(() => {
    if (handoff.current) clearTimeout(handoff.current);
    reset();
    setPublished(false);
    fleet.clearLabels(DELIVERED);
  }, [fleet, reset]);

  const { fire } = useSceneBeat({ ref: sceneRef, onFire: publish, onReset: rewind });

  const approve = useCallback(() => {
    fleet.setLabels(APPROVED, APPROVED_LABEL);
    setApproved(true);
  }, [fleet]);

  // Replay re-ARMS the button rather than firing anything itself — the click stays the cause.
  const rearm = useCallback(() => {
    fleet.clearLabels(APPROVED);
    setApproved(false);
  }, [fleet]);

  return (
    <div className={`${WRAP} lg:grid lg:grid-cols-[minmax(0,1fr)_minmax(268px,300px)] lg:gap-10`}>
      <div>
        {/* Inside the block column ON PURPOSE: with scripting disabled a <noscript> is rendered
            (its <style> child is not), and as a direct grid child it would claim a cell. */}
        <noscript>
          <style>{SCRIPTLESS_CSS}</style>
        </noscript>
        <section id="demo" className="pt-[52px] lg:pt-[72px]">
          <LoopCopy />
          <div ref={sceneRef} className="mt-6 grid gap-6 lg:block">
            <div>
              <ChatCard yesRef={yesRef} sent={sent} onResend={fire} />
              <Receipt shown={published}>{DELIVERED_RECEIPT}</Receipt>
            </div>
            <div className="landing-fanout-slot lg:hidden">
              <FanoutTable />
            </div>
          </div>
        </section>

        <section className="pt-[84px] lg:pt-[116px]">
          <ReviewCopy />
          <div className="mt-6">
            <ReviewFrame approved={approved} onApprove={approve} onReplay={rearm} />
            <Receipt shown={approved}>{APPROVED_RECEIPT}</Receipt>
          </div>
        </section>
      </div>

      <div className="landing-rail max-lg:hidden">
        <div className="sticky top-[92px] mt-[168px]">
          <Fleet
            machines={FLEET_MACHINES}
            controller={fleet}
            heading="The team’s machines"
            footnote="the fleet holds beside the scenes"
          />
        </div>
      </div>
    </div>
  );
}

/** The rail-free bands: the same copy with a plain fan-out table and a still review frame. */
function StaticBands() {
  return (
    <>
      <section id="demo" className={`${WRAP} pt-[52px] lg:pt-[72px]`}>
        <LoopCopy />
        <div className="mt-6 grid items-center gap-x-3.5 gap-y-6 lg:grid-cols-[minmax(0,1fr)_auto_minmax(232px,0.82fr)]">
          <div>
            <ChatCard sent />
            <Receipt shown>{DELIVERED_RECEIPT}</Receipt>
          </div>
          <span aria-hidden className="hidden font-mono text-[16px] text-accent lg:block">
            →
          </span>
          <FanoutTable />
        </div>
      </section>

      <section className={`${WRAP} pt-[84px] lg:pt-[116px]`}>
        <ReviewCopy />
        <div className="mt-6">
          <ReviewFrame approved={false} />
          <Receipt shown>{APPROVED_RECEIPT}</Receipt>
        </div>
      </section>
    </>
  );
}

/* ───────────────────────── the scene pieces ───────────────────────── */

/**
 * Maya's chat with her own agent, in the modern agentic convention: her turns are right-aligned
 * bubbles, the agent's are full-width plain text with no bubble, and a sender micro-label opens
 * each turn. The closing "Yes." row is the one animated element — it rests hidden and plays in
 * when the scene arrives, carrying its own resend control.
 */
function ChatCard({
  yesRef,
  sent,
  onResend,
}: {
  yesRef?: RefObject<HTMLDivElement | null>;
  sent: boolean;
  onResend?: () => void;
}) {
  return (
    <div className="flex flex-col gap-3.5 rounded-[14px] border border-line-soft bg-panel px-[18px] py-4 shadow-card">
      <Turn who="Maya · Engineering" mine>
        We shipped the internal customer-lookup tool. Teach the team’s agents how to use it.
      </Turn>
      <Turn who="Maya’s agent · Claude Code">
        Created <Code>customer-lookup</Code>: how to query it and when to reach for it. Share it
        with the team?
      </Turn>
      <div>
        <Who mine>Maya</Who>
        <div
          ref={yesRef}
          data-shown={sent ? "true" : "false"}
          className="landing-reveal flex origin-bottom-right items-center justify-end gap-1.5"
        >
          {onResend ? <ReplayControl onClick={onResend} label="resend" /> : null}
          <span className="rounded-[14px_14px_4px_14px] bg-panel2 px-3.5 py-2 text-[13.5px] text-ink leading-[1.5]">
            Yes.
          </span>
        </div>
      </div>
    </div>
  );
}

function Who({ mine = false, children }: { mine?: boolean; children: ReactNode }) {
  return (
    <span
      className={`mb-1 block px-0.5 font-display text-[9.5px] text-faint uppercase tracking-[0.08em] ${
        mine ? "text-right" : ""
      }`}
    >
      {children}
    </span>
  );
}

function Turn({
  who,
  mine = false,
  children,
}: {
  who: string;
  mine?: boolean;
  children: ReactNode;
}) {
  return (
    <div className={`flex flex-col ${mine ? "items-end" : ""}`}>
      <Who mine={mine}>{who}</Who>
      {mine ? (
        <span className="max-w-[85%] rounded-[14px_14px_4px_14px] bg-panel2 px-3.5 py-2 text-[13.5px] text-ink leading-[1.5]">
          {children}
        </span>
      ) : (
        <p className="px-0.5 text-[13.5px] text-ink leading-[1.55]">{children}</p>
      )}
    </div>
  );
}

/** A bundle name inside running copy. */
function Code({ children }: { children: ReactNode }) {
  return (
    <code className="rounded-sm bg-panel2 px-[5px] py-px font-mono text-[11.5px] text-ink">
      {children}
    </code>
  );
}

/** A scene's outcome, stated outside its card in the small blue mono voice receipts wear. */
function Receipt({ shown, children }: { shown: boolean; children: ReactNode }) {
  return (
    <p
      data-shown={shown ? "true" : "false"}
      className="landing-reveal mt-2 pl-1 font-mono text-[10.5px] text-accent"
    >
      {children}
    </p>
  );
}

/**
 * The review page, schematic — a component rather than a screenshot, so it never drifts stale and
 * keeps the visitor on proposer → diff → approve. Its labels and button copy mirror the real
 * review page's own text.
 */
function ReviewFrame({
  approved,
  onApprove,
  onReplay,
}: {
  approved: boolean;
  onApprove?: () => void;
  onReplay?: () => void;
}) {
  return (
    <div className="max-w-[540px] overflow-hidden rounded-lg border border-line bg-panel shadow-card">
      <div className="flex items-center gap-2.5 border-line-soft border-b bg-panel2 px-3 py-2">
        <span className="flex gap-[5px]" aria-hidden="true">
          <span className="block h-[9px] w-[9px] rounded-full bg-line" />
          <span className="block h-[9px] w-[9px] rounded-full bg-line" />
          <span className="block h-[9px] w-[9px] rounded-full bg-line" />
        </span>
        <span className="flex-1 truncate rounded-md border border-line-soft bg-panel px-2.5 py-0.5 font-mono text-[11px] text-faint">
          topos.sh/acme/skills/brand-voice/proposals
        </span>
      </div>
      <div className="px-[18px] py-4">
        <div className="mb-2.5 flex flex-wrap items-baseline justify-between gap-2.5">
          <span className="font-display font-semibold text-[13px] text-ink">
            brand-voice · proposal
          </span>
          <span className="text-[11.5px] text-faint">proposed by Kim · Marketing</span>
        </div>
        <div className="mb-3 overflow-hidden rounded-md border border-line-soft font-mono text-[11.5px] leading-[1.6]">
          <p className="bg-panel2 px-3 py-[5px] text-faint">
            <span className="text-hairline">−&nbsp;&nbsp;</span>Use title case for headings and
            exclamation points for energy!
          </p>
          <p className="bg-accent-wash px-3 py-[5px] text-ink">
            <span className="text-accent">+&nbsp;&nbsp;</span>Use sentence case for headings. No
            exclamation points.
          </p>
        </div>
        {onApprove ? (
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={approved ? undefined : onApprove}
              disabled={approved}
              className={`rounded-md px-3.5 py-[7px] font-mono text-[12px] transition-colors focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2 ${
                approved
                  ? "bg-panel2 text-dim"
                  : "landing-pulse bg-accent text-on-accent hover:bg-accent-deep"
              }`}
            >
              {approved ? "Approved" : "Approve"}
            </button>
            {approved && onReplay ? <ReplayControl onClick={onReplay} label="replay" /> : null}
          </div>
        ) : (
          <span className="inline-block rounded-md bg-accent px-3.5 py-[7px] font-mono text-[12px] text-on-accent">
            Approve
          </span>
        )}
      </div>
    </div>
  );
}

/** The rail-free stand-in for the fleet: the machines the publish reached, and their new state. */
function FanoutTable() {
  return (
    <div className="landing-fanout rounded-[14px] border border-line-soft bg-panel px-4 py-3.5 shadow-card">
      <p className="mb-2.5 font-display text-[9.5px] text-faint uppercase tracking-[0.1em]">
        The team’s machines
      </p>
      {/* Every row is ruled, the first one included — the top rule is what separates the rows
          from the heading above them. */}
      {FANOUT_ROWS.map((machine) => (
        <div
          key={machine.name}
          className="flex items-center justify-between gap-2.5 border-line-soft border-t py-2"
        >
          <span className="flex items-center gap-2">
            <MonitorIcon aria-hidden className="h-[13px] w-[13px] shrink-0 text-faint" />
            <span className="flex flex-col">
              <span className="flex items-center gap-1.5 font-mono text-[11.5px] text-ink">
                {machine.name} ·
                <HarnessMark name={machine.harness} className="h-3 w-3 shrink-0" />
                {machine.harness}
              </span>
              <span className="text-[10.5px] text-faint">{machine.team}</span>
            </span>
          </span>
          <span className="text-right font-mono text-[10px] text-accent uppercase tracking-[0.07em]">
            {DELIVERED_LABEL}
          </span>
        </div>
      ))}
    </div>
  );
}

/** The quiet ↻ control the scenes carry: resend beside "Yes.", replay beside a used Approve. */
function ReplayControl({ onClick, label }: { onClick: () => void; label: string }) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="inline-flex items-center gap-1 rounded-sm px-1 py-0.5 font-mono text-[9.5px] text-faint transition-colors hover:text-accent focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
    >
      <ReplayGlyph />
      {label}
    </button>
  );
}
