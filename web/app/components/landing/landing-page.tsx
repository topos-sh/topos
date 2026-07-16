import { Link } from "react-router";
import { CommandBlock } from "@/components/command-block";
import { RoutingStar } from "@/components/landing/routing-star";
import { TerminalDemo } from "@/components/landing/terminal-demo";

/**
 * The public landing page ("Klein"): warm-gray print ground, near-black ink, links in ink,
 * International Klein Blue as placed objects (nav button, routing chips, check-chips) plus the
 * agent-conversation section's markers and result lines. The dark terminal glass carries a
 * single phosphor from the blue ramp.
 *
 * It is a plain presentational component so BOTH the single-tenant home FACE (the origin root's
 * anonymous view) and the multi-tenant top-level `landing.tsx` route render the same page. The
 * claim band (`awaitingOwner`) is single-tenant only — a multi-tenant deployment always passes
 * `awaitingOwner={false}` and never mints a boot workspace to claim. Sign-in affordances (the nav
 * "Sign in", the header/footer CTAs) lead into the product.
 */

const INSTALL = "curl -fsSL https://topos.sh/install | sh";
const GITHUB = "https://github.com/topos-sh/topos";
const WRAP = "mx-auto max-w-[1080px] px-6";
const ULINK = "border-b border-hairline text-dim transition-colors hover:border-ink hover:text-ink";

function InstallCommand() {
  return <CommandBlock command={INSTALL} />;
}

const VERBS: { tag: string; main?: boolean; prompt: string; out: string; ok: string }[] = [
  {
    tag: "Share",
    main: true,
    prompt: "share our incident-response skill with the team",
    out: "● Published incident-response@a7d2",
    ok: "  topos invite teammate@you.com → the mail carries your address",
  },
  {
    tag: "Join",
    prompt: "[pastes topos.sh/acme]",
    out: "● topos follow topos.sh/acme",
    ok: "  approve this device in your browser — then I keep it current.",
  },
  {
    tag: "Follow",
    prompt: "follow incident-response",
    out: "● Following incident-response@a7d2",
    ok: "  updates land at session start; your local edits stay yours",
  },
  {
    tag: "Undo",
    prompt: "the new escalation step pages the wrong team, roll it back",
    out: "● Reverted incident-response to a7d2",
    ok: "  every agent rolls back at next session",
  },
];

const COMPARISON: { statement: string; git: boolean }[] = [
  { statement: "Skills stay plain files in your agent’s own folders", git: true },
  {
    statement: "Every agent picks up changes at session start, nobody has to remember to pull",
    git: false,
  },
  {
    statement: "Anyone can propose an improvement, even people who never open a terminal",
    git: false,
  },
  {
    statement:
      "Every version is content-addressed, so agents run exactly the bytes the team approved",
    git: false,
  },
  { statement: "One command rolls every machine back to a known-good version", git: false },
];

function CheckChip() {
  return (
    <span className="inline-flex h-[22px] w-[22px] items-center justify-center rounded-md bg-accent text-[12px] text-on-accent">
      ✓
    </span>
  );
}

/**
 * The unclaimed-install band: shown ONLY while a single-tenant install still awaits its first
 * owner. The claim rides the one-time link the server printed at boot — machine control is the
 * proof — so the page points at the logs and shows the link's SHAPE, never a code.
 */
function ClaimBlock({ setupLine }: { setupLine: string }) {
  return (
    <section className="border-line-soft border-b bg-panel">
      <div className={`${WRAP} py-8`}>
        <div className="rounded-lg border border-line-soft bg-panel2 px-6 py-6 shadow-card">
          <p className="font-display text-[10px] text-accent uppercase tracking-[0.14em]">
            Set up this install
          </p>
          <h2 className="mt-3 max-w-[40ch] font-display font-semibold text-[clamp(18px,2.2vw,23px)] text-ink leading-[1.4] tracking-[-0.02em]">
            This install is waiting for its owner.
          </h2>
          <p className="mt-3 max-w-[60ch] text-dim">
            The setup link is printed in the server logs — whoever opens it creates the first
            account and owns the workspace. Look for the line:
          </p>
          <pre className="mt-4 overflow-x-auto rounded-md border border-line-soft bg-ground px-4 py-3 font-mono text-[13px] text-dim">
            → Finish setup: {setupLine}
          </pre>
        </div>
      </div>
    </section>
  );
}

export function LandingPage({
  awaitingOwner,
  setupLine,
}: {
  awaitingOwner: boolean;
  setupLine: string;
}) {
  return (
    <div className="min-h-dvh text-[15px] leading-[1.6]">
      <nav className="border-line-soft border-b">
        <div className={`${WRAP} flex h-[60px] items-center justify-between`}>
          <Link
            to="/"
            className="font-display font-semibold text-[15px] tracking-[-0.02em] focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
          >
            topos<span className="text-accent">_</span>
          </Link>
          <div className="flex items-center gap-6 text-[13.5px] text-dim">
            <a href="#demo" className="transition-colors hover:text-ink max-sm:hidden">
              How it works
            </a>
            <a href="#vs" className="transition-colors hover:text-ink max-sm:hidden">
              Why Topos
            </a>
            <a href={GITHUB} className="transition-colors hover:text-ink max-sm:hidden">
              GitHub
            </a>
            <Link to="/app" className="transition-colors hover:text-ink max-sm:hidden">
              Sign in
            </Link>
            <Link
              to="/login"
              className="rounded-md bg-accent px-3.5 py-2 font-mono text-[12.5px] text-on-accent transition-colors hover:bg-accent-deep focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2 active:scale-[0.98]"
            >
              Create a workspace
            </Link>
          </div>
        </div>
      </nav>

      {awaitingOwner && <ClaimBlock setupLine={setupLine} />}

      <header className="pt-11 pb-8 lg:pt-[58px] lg:pb-9">
        <div
          className={`${WRAP} grid items-center gap-9 lg:grid-cols-[minmax(0,1.05fr)_minmax(340px,0.95fr)] lg:gap-14`}
        >
          <div>
            <h1 className="font-display font-semibold text-[clamp(19px,2.4vw,25px)] leading-[1.45] tracking-[-0.03em]">
              Align the behavior of every <br className="max-lg:hidden" />
              agent in your team.
            </h1>
            <p className="mt-4 max-w-[47ch] text-[16px] text-dim">
              Your agents share skills, keep them current, and{" "}
              <strong className="font-medium text-ink">improve them together</strong>: one
              teammate’s fix upgrades every agent on the team.
            </p>
            <div className="mt-6 flex flex-wrap items-center gap-3.5">
              <InstallCommand />
              <span className="text-[12.5px] text-faint">
                Apache-2.0, macOS and Linux {"·"}{" "}
                <a href="https://topos.sh/install" className={ULINK}>
                  read it first →
                </a>
              </span>
            </div>
            <p className="mt-3 text-[13px] text-faint">
              No terminal?{" "}
              <Link to="/login" className={ULINK}>
                Create a workspace →
              </Link>
            </p>
          </div>
          <div className="mx-auto w-full max-w-[440px] lg:max-w-none">
            <RoutingStar />
          </div>
        </div>
      </header>

      <div id="demo" className={`${WRAP} pt-[52px] pb-2 lg:pt-[72px]`}>
        <TerminalDemo />
      </div>

      <section id="agent" className="pt-[84px] lg:pt-[116px]">
        <div className={WRAP}>
          <h2 className="max-w-[40ch] font-display font-semibold text-[clamp(18px,2.2vw,23px)] leading-[1.45] tracking-[-0.02em]">
            You don’t operate it. Your agent does.
          </h2>
          <div className="mt-[30px] grid gap-[18px] lg:grid-cols-[1.15fr_1fr]">
            {VERBS.map((verb) => (
              <div
                key={verb.tag}
                className={`rounded-lg border border-line-soft bg-panel shadow-card ${
                  verb.main ? "flex flex-col justify-center p-7" : "px-5 py-[18px]"
                }`}
              >
                <span className="mb-3.5 inline-block self-start rounded-full bg-accent px-2.5 py-[3px] font-display text-[9.5px] text-on-accent uppercase tracking-[0.14em]">
                  {verb.tag}
                </span>
                <pre className="whitespace-pre-wrap break-words font-mono text-[13px] text-dim leading-[1.75]">
                  <span className="block text-ink">
                    <span className="font-semibold text-accent">{"❯ "}</span>
                    {verb.prompt}
                  </span>
                  <span className="block">{verb.out}</span>
                  <span className="block text-accent">{verb.ok}</span>
                </pre>
              </div>
            ))}
          </div>
          <p className="mt-5 text-[13px] text-faint">
            Everything the agent does is a plain command you can run yourself: an open-source CLI (
            <code className="font-mono text-[12px] text-dim">
              topos publish, join, follow, revert
            </code>
            ) with <code className="font-mono text-[12px] text-dim">--json</code> output.
          </p>
        </div>
      </section>

      <section id="vs" className="pt-[84px] lg:pt-[116px]">
        <div className={WRAP}>
          <div className="grid items-start gap-7 lg:grid-cols-[minmax(0,0.85fr)_minmax(0,1.15fr)] lg:gap-12">
            <div>
              <h2 className="max-w-[40ch] font-display font-semibold text-[clamp(18px,2.2vw,23px)] leading-[1.45] tracking-[-0.02em]">
                A git repo stores skills. Topos keeps them improving.
              </h2>
              <p className="mt-3 max-w-[58ch] text-dim">
                The value is the loop: every improvement anyone ships reaches every agent, and every
                agent can propose the next one.
              </p>
            </div>
            <div className="overflow-hidden rounded-md border border-line-soft bg-panel">
              <div className="grid grid-cols-[1fr_64px_64px] border-line-soft border-b lg:grid-cols-[1fr_84px_84px]">
                <div />
                <div className="px-2 py-2.5 text-center font-display text-[10px] text-faint uppercase tracking-[0.12em]">
                  git repo
                </div>
                <div className="px-2 py-2.5 text-center font-display text-[10px] text-ink uppercase tracking-[0.12em]">
                  topos
                </div>
              </div>
              {COMPARISON.map((row) => (
                <div
                  key={row.statement}
                  className="grid grid-cols-[1fr_64px_64px] items-center even:bg-panel2 lg:grid-cols-[1fr_84px_84px]"
                >
                  <div className="px-5 py-[11px] text-[14px]">{row.statement}</div>
                  <div className="px-2 py-[11px] text-center font-mono text-[15px]">
                    {row.git ? (
                      <span className="text-ink">✓</span>
                    ) : (
                      <span className="text-faint">✗</span>
                    )}
                  </div>
                  <div className="px-2 py-[11px] text-center font-mono text-[15px]">
                    <CheckChip />
                  </div>
                </div>
              ))}
            </div>
          </div>
        </div>
      </section>

      <footer className="mt-24 border-line-soft border-t pt-16 pb-[72px] lg:mt-[132px]">
        <div className={`${WRAP} flex flex-wrap items-start justify-between gap-8`}>
          <div>
            <h2 className="font-display font-semibold text-[clamp(18px,2.2vw,23px)] leading-[1.45] tracking-[-0.02em]">
              Share your first skill in five minutes.
            </h2>
            <div className="mt-5">
              <InstallCommand />
            </div>
          </div>
          <div>
            <div className="mt-3 flex gap-6 text-[13px] text-faint">
              <a href={GITHUB} className="transition-colors hover:text-ink">
                GitHub
              </a>
              <a
                href={`${GITHUB}/blob/main/SECURITY.md`}
                className="transition-colors hover:text-ink"
              >
                Security model
              </a>
              <a href={`${GITHUB}#readme`} className="transition-colors hover:text-ink">
                Docs
              </a>
              <Link to="/app" className="transition-colors hover:text-ink">
                Sign in
              </Link>
            </div>
            <p className="mt-3.5 text-[12px] text-faint">Apache-2.0 {"·"} © 2026 Topos</p>
          </div>
        </div>
      </footer>
    </div>
  );
}
