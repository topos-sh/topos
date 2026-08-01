import { type ReactNode, useState } from "react";
import { Link } from "react-router";
import { CopyButton } from "@/components/copy-button";
import { FounderAddressBlock } from "@/components/landing/founder-email";
import { HarnessMark, hasHarnessMark } from "@/components/landing/harness-marks";
import { MicroLabel, SectionHeading, WRAP } from "@/components/landing/landing-kit";
import { LoopBands } from "@/components/landing/loop-bands";
import { PricingBand } from "@/components/landing/pricing-band";
import { RoutingStar } from "@/components/landing/routing-star";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";

/**
 * The public landing page ("Klein"): warm-gray print ground, near-black ink, links in ink,
 * International Klein Blue as placed objects — the nav button, the routing chips, the numbered
 * steps, the receipts, the approve button, the delivery-state labels — never as a wash.
 *
 * It is a plain presentational component so BOTH the single-tenant home FACE (the origin root's
 * anonymous view) and the multi-tenant top-level `landing.tsx` route render the same page. The
 * claim band (`awaitingOwner`) is single-tenant only — a multi-tenant deployment always passes
 * `awaitingOwner={false}` and never mints a boot workspace to claim. Sign-in affordances (the nav
 * "Sign in", the header/footer CTAs) lead into the product — and they speak the deployment's
 * TENANCY honestly: only a multi-tenant deployment creates workspaces, so its CTAs say "Create a
 * workspace"; a single-tenant install's say "Sign in" (its one workspace was minted at boot, and
 * ownership arrives through the claim band above, never a create flow).
 */

/**
 * Which loop-band implementation ships. `false` — today — is the STATIC bands: the numbered steps,
 * the chat, and a plain fan-out table beside it. `true` swaps in the sticky fleet rail that holds
 * beside both scenes and updates as each beat lands (desktop only; narrow screens and scriptless
 * pages fall back to the static bands on their own). Both are built and tested; it is one line
 * either way, and no test or copy changes with it.
 */
const FLEET_RAIL = false;

const INSTALL = "curl -fsSL https://topos.sh/install | sh";
const AGENT_SETUP_PROMPT = "Set up Topos for us: fetch https://topos.sh/agent and follow it.";
const GITHUB = "https://github.com/topos-sh/topos";
const SECURITY = `${GITHUB}/blob/main/SECURITY.md`;
/** The documentation site — a subpath of this origin, so it rides the deployment's own domain. */
const DOCS = "/docs";

/** The one action that matters on this surface, wearing the Klein primary-button shape. */
const PRIMARY_BUTTON =
  "inline-block rounded-md bg-accent px-5 py-3 font-mono text-[13px] text-on-accent transition-colors hover:bg-accent-deep focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2 active:scale-[0.98]";

/** The GitHub mark (octicon path), sized by className, inked by currentColor. */
function GitHubMark({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" fill="currentColor" className={className}>
      <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27s1.36.09 2 .27c1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8Z" />
    </svg>
  );
}

const AGENT_TAB = {
  value: "agent",
  label: "Paste to your agent",
  text: AGENT_SETUP_PROMPT,
  copyLabel: "Copy the agent setup prompt",
};
const HUMAN_TAB = {
  value: "human",
  label: "In a terminal",
  text: INSTALL,
  copyLabel: "Copy the install command",
};
const SETUP_TABS = [AGENT_TAB, HUMAN_TAB];

/**
 * The one setup block: a glass command block whose header row carries small inline tabs by
 * PATH — "Paste to your agent" (the paste-ready prompt) first, "In a terminal" (the by-hand
 * install) second — under one constant `❯` chip. Nothing outside the header changes shape on a switch: the label above
 * is static, and both contents stay mounted in one grid cell (the inactive one invisible) so
 * the block keeps the taller content's height and the hero never jumps. The WHOLE block is
 * LIGHT: one panel2 field, header and body alike, ink command text, a bare accent ❯ leading
 * the row, faint labels, a near-white panel pill on the active trigger — everything at the
 * rounded-md control radius, the copy affordance in its light tone at the triggers' own
 * compact size.
 */
function SetupBlock({ label }: { label: string }) {
  const [active, setActive] = useState(AGENT_TAB.value);
  const current = SETUP_TABS.find((t) => t.value === active) ?? AGENT_TAB;
  return (
    <div>
      <p className="mb-2 font-display font-medium text-[10px] text-faint uppercase tracking-[0.12em]">
        {label}
      </p>
      <Tabs value={active} onValueChange={setActive}>
        <div className="overflow-hidden rounded-md border border-line-soft bg-panel2">
          <div className="flex items-center gap-2 border-line-soft border-b px-2.5 py-1.5">
            <span className="flex-none select-none px-1 font-mono font-semibold text-[11px] text-accent">
              ❯
            </span>
            <TabsList className="gap-1">
              {SETUP_TABS.map((t) => (
                <TabsTrigger
                  key={t.value}
                  value={t.value}
                  className="whitespace-nowrap rounded-md border border-transparent px-2 py-0.5 font-mono text-[11px] text-faint transition-colors hover:text-ink data-[state=active]:border-line data-[state=active]:bg-panel data-[state=active]:text-ink"
                >
                  {t.label}
                </TabsTrigger>
              ))}
            </TabsList>
            <span className="ml-auto">
              <CopyButton text={current.text} ariaLabel={current.copyLabel} tone="light" compact />
            </span>
          </div>
          <div className="grid">
            {SETUP_TABS.map((t) => (
              <TabsContent
                forceMount
                key={t.value}
                value={t.value}
                className="col-start-1 row-start-1 data-[state=inactive]:invisible"
              >
                <p className="break-words px-4 py-3 font-mono text-[12.5px] text-ink">{t.text}</p>
              </TabsContent>
            ))}
          </div>
        </div>
      </Tabs>
    </div>
  );
}

/**
 * The hero's two paths, BOTH always visible: the agent path (the paste-ready prompt plus the
 * install one-liner) and the browser path (a plain button; no terminal involved). The browser
 * button speaks the deployment's tenancy: only a multi-tenant deployment creates workspaces;
 * a single-tenant install signs in.
 */
function HeroPaths({ tenancy }: { tenancy: "single" | "multi" }) {
  return (
    <div className="mt-6">
      <SetupBlock label="Set up Topos (macOS & Linux)" />
      <div className="mt-6">
        <p className="mb-2 font-display font-medium text-[10px] text-faint uppercase tracking-[0.12em]">
          Or start in the browser
        </p>
        <Link to={tenancy === "multi" ? "/new" : "/login"} className={PRIMARY_BUTTON}>
          {tenancy === "multi" ? "Create a workspace" : "Sign in"}
        </Link>
      </div>
    </div>
  );
}

/** The agent apps Topos delivers to or reads alongside — no Claude app: there is no adapter for it. */
const WORKS_WITH = ["Claude Code", "OpenClaw", "Hermes", "Codex", "Cursor"];

/** Weight-500 ink emphasis — the system's only emphasis (never bold). */
function Em({ children }: { children: ReactNode }) {
  return <b className="font-medium">{children}</b>;
}

/**
 * The agent app a team's people run: the app's own logomark on a quiet inset tile beside its name —
 * and the name alone for an app whose owner publishes no mark (see `harness-marks.tsx`). The name
 * is plain text, not a chip: nothing on these cards carries a border, and the tile is the only
 * field. A card naming two apps joins them in one label beside their tiles, as one line.
 */
function AgentBadge({ apps }: { apps: string[] }) {
  return (
    <span className="flex min-w-0 items-center gap-[7px]">
      {/* Two tiles sit closer to each other than to the label, so a pair reads as one object and
          the label keeps the width it needs. */}
      <span className="flex flex-none items-center gap-[3px]">
        {apps.filter(hasHarnessMark).map((app) => (
          <span
            key={app}
            className="flex h-[26px] w-[26px] items-center justify-center rounded-[7px] bg-panel2"
          >
            <HarnessMark name={app} className="h-4 w-4" />
          </span>
        ))}
      </span>
      {/* One line on every real screen; on a very narrow one the label wraps rather than
          pushing the card wider than the page. */}
      <span className="text-right font-mono text-[11px] text-dim">{apps.join(" · ")}</span>
    </span>
  );
}

/**
 * One plain sentence a first-timer reads cold, on the same "once — every" rhythm, so cause and
 * effect live inside ordinary grammar. The bundle names sit beneath as quiet chips: a skimmer can
 * ignore them, a curious reader learns that Topos carries skills, memories, and docs alike.
 */
const TEAMS: {
  team: string;
  apps: string[];
  scene: ReactNode;
  bundles: { name: string; kind: string }[];
}[] = [
  {
    team: "Marketing",
    apps: ["OpenClaw"],
    scene: (
      <>
        Update the brand voice <Em>once</Em>. <Em>Every</Em> teammate’s drafts follow it.
      </>
    ),
    bundles: [
      { name: "brand-voice", kind: "skill" },
      { name: "campaign-facts", kind: "docs" },
    ],
  },
  {
    team: "Sales",
    apps: ["Hermes"],
    scene: (
      <>
        A client talks to <Em>one</Em> rep. <Em>Every</Em> rep’s agent remembers the conversation.
      </>
    ),
    bundles: [{ name: "client-history", kind: "memory" }],
  },
  {
    team: "Support",
    apps: ["OpenClaw"],
    scene: (
      <>
        Change a policy <Em>once</Em>. <Em>Every</Em> answer follows it, even a new hire’s.
      </>
    ),
    bundles: [
      { name: "product-docs", kind: "docs" },
      { name: "escalation-rules", kind: "skill" },
    ],
  },
  {
    team: "Engineering",
    apps: ["Claude Code", "Codex"],
    scene: (
      <>
        Fix the deploy process <Em>once</Em>. <Em>Every</Em> agent deploys the new way.
      </>
    ),
    bundles: [
      { name: "deploy", kind: "skill" },
      { name: "incident-runbook", kind: "docs" },
    ],
  },
];

/** Six answers in full view — no accordion, because a hidden answer is an unanswered question. */
const FAQ: { q: string; a: ReactNode }[] = [
  {
    q: "What does my team actually install?",
    a: (
      <>
        Plain files, in your agent’s own folders: the same skill bundles and reference docs someone
        would write by hand. The open-source <code className="font-mono text-[13px]">topos</code>{" "}
        command keeps them current in the background; nothing about how your agent works changes.
      </>
    ),
  },
  {
    q: "Which agents does it work with?",
    a: (
      <>
        Topos delivers to Claude Code, OpenClaw, and Hermes today. What it writes is the ordinary
        skill format, so Codex, Cursor, and the wider skills ecosystem read the same files.
      </>
    ),
  },
  {
    q: "What happens to someone’s local edits?",
    a: (
      <>
        They are never overwritten. A local change is kept as a draft beside the team’s version, and
        your agent offers it back as a proposal when it is worth sharing.
      </>
    ),
  },
  {
    q: "Where do our skills live, and is there lock-in?",
    a: (
      <>
        In a plain git repo. Self-host the whole thing or use our cloud, and take it with you if you
        ever leave.{" "}
        <a
          href={SECURITY}
          target="_blank"
          rel="noreferrer"
          className="border-hairline border-b transition-colors hover:border-ink"
        >
          The security model
          <span className="sr-only"> (opens in a new tab)</span>
        </a>{" "}
        spells out exactly what the server holds.
      </>
    ),
  },
  {
    q: "What does it cost?",
    a: (
      <>
        Nothing while we are in closed beta, and self-hosting is free forever. We will publish
        pricing before that changes.
      </>
    ),
  },
  {
    q: "We’re early. Will you set it up with us?",
    a: <>Yes. Email me below and I’ll get your team’s first shared skills flowing with you.</>,
  },
];

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
            The setup link is printed in the server logs; whoever opens it creates the first account
            and owns the workspace. Look for the line:
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
  tenancy,
}: {
  awaitingOwner: boolean;
  setupLine: string;
  /** The deployment's address grammar — decides whether the CTAs may promise workspace creation. */
  tenancy: "single" | "multi";
}) {
  const ctaTo = tenancy === "multi" ? "/new" : "/login";
  const ctaLabel = tenancy === "multi" ? "Create a workspace" : "Sign in";

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
            <a href="#pricing" className="transition-colors hover:text-ink max-sm:hidden">
              Pricing
            </a>
            <a href="#faq" className="transition-colors hover:text-ink max-sm:hidden">
              FAQ
            </a>
            <a href={DOCS} className="transition-colors hover:text-ink max-sm:hidden">
              Docs
            </a>
            <a
              href={GITHUB}
              target="_blank"
              rel="noreferrer"
              className="flex items-center gap-1.5 transition-colors hover:text-ink max-sm:hidden"
            >
              <GitHubMark className="h-4 w-4" />
              GitHub
              <span className="sr-only"> (opens in a new tab)</span>
            </a>
            {tenancy === "multi" && (
              // In single tenancy the accent button below IS the sign-in — one affordance, no twin.
              <Link to="/app" className="transition-colors hover:text-ink max-sm:hidden">
                Sign in
              </Link>
            )}
            <Link
              to={ctaTo}
              className="rounded-md bg-accent px-3.5 py-2 font-mono text-[12.5px] text-on-accent transition-colors hover:bg-accent-deep focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2 active:scale-[0.98]"
            >
              {ctaLabel}
            </Link>
          </div>
        </div>
      </nav>

      {awaitingOwner && <ClaimBlock setupLine={setupLine} />}

      <header className="pt-20 pb-16 lg:pt-[136px] lg:pb-[120px]">
        <div
          className={`${WRAP} grid items-center gap-9 lg:grid-cols-[minmax(0,1.05fr)_minmax(340px,0.95fr)] lg:gap-14`}
        >
          <div>
            <h1 className="font-display font-semibold text-[clamp(19px,2.4vw,25px)] leading-[1.45] tracking-[-0.03em]">
              Keep every AI agent in <br className="max-lg:hidden" />
              your company up to date.
            </h1>
            <p className="mt-4 max-w-[54ch] text-[16px] text-dim">
              Topos syncs skills, memory files, and context across your team’s agents (Claude Code,
              Codex, Cursor, and more), and anyone can{" "}
              <strong className="font-medium text-ink">contribute improvements back</strong>.
            </p>
            <HeroPaths tenancy={tenancy} />
          </div>
          <div className="mx-auto w-full max-w-[440px] lg:max-w-none">
            <RoutingStar />
          </div>
        </div>
      </header>

      {/* The proof row: what this is, and what it plugs into. No traction numbers, no star count. */}
      <section className="border-line-soft border-y bg-panel">
        <div
          className={`${WRAP} flex flex-wrap items-center gap-x-7 gap-y-2.5 py-3.5 text-[12.5px]`}
        >
          <a
            href={GITHUB}
            target="_blank"
            rel="noreferrer"
            className="flex items-center gap-1.5 text-dim transition-colors hover:text-ink"
          >
            <GitHubMark className="h-3.5 w-3.5" />
            Open source {"·"} Apache-2.0
            <span className="sr-only"> (opens in a new tab)</span>
          </a>
          <span className="flex flex-wrap items-center gap-2">
            <span className="font-display font-medium text-[10px] text-faint uppercase tracking-[0.12em]">
              Works with
            </span>
            {WORKS_WITH.map((app) => (
              <span
                key={app}
                className="flex items-center gap-1.5 whitespace-nowrap rounded-full border border-line bg-panel2 px-2.5 py-0.5 font-mono text-[11px] text-dim"
              >
                <HarnessMark name={app} />
                {app}
              </span>
            ))}
          </span>
        </div>
      </section>

      <section className="pt-[52px] lg:pt-[72px]">
        <div className={WRAP}>
          <MicroLabel>Every team, every agent app</MicroLabel>
          <SectionHeading className="max-w-[44ch]">
            Give every team an agent that already knows how your company works.
          </SectionHeading>
          <div className="mt-6 grid gap-3 sm:grid-cols-2">
            {TEAMS.map((team) => (
              <div
                key={team.team}
                className="flex flex-col rounded-lg border border-line-soft bg-panel px-[18px] pt-[15px] pb-[14px] shadow-card"
              >
                {/* The header holds the mark tile's height whether or not a tile is there, so
                    every card's sentence starts on the same line. */}
                <div className="mb-2.5 flex min-h-[26px] items-center justify-between gap-2.5">
                  <span className="font-display font-semibold text-[14px] text-ink tracking-[-0.01em]">
                    {team.team}
                  </span>
                  <AgentBadge apps={team.apps} />
                </div>
                <p className="text-[14px] text-ink leading-[1.55]">{team.scene}</p>
                <div className="mt-[13px] flex flex-wrap gap-1.5 border-line-soft border-t pt-[11px]">
                  {team.bundles.map((bundle) => (
                    <span
                      key={bundle.name}
                      className="whitespace-nowrap rounded-full bg-panel2 px-[9px] py-0.5 font-mono text-[10px] text-dim"
                    >
                      {bundle.name} {"·"} <span className="text-faint">{bundle.kind}</span>
                    </span>
                  ))}
                </div>
              </div>
            ))}
          </div>
        </div>
      </section>

      <LoopBands rail={FLEET_RAIL} />

      <PricingBand ctaTo={ctaTo} ctaLabel={ctaLabel} docsHref={DOCS} />

      {/* Directly under pricing, so the Enterprise arm's "Talk to us" lands on the next screen
          rather than past the FAQ. It opens on its heading — no micro-label — which is what marks
          it as the page's one personal address rather than another catalogued band. */}
      <section id="contact" className="pt-[84px] lg:pt-[116px]">
        <div className={WRAP}>
          <SectionHeading>Setting this up for a team? Email me.</SectionHeading>
          <p className="mt-4 max-w-[62ch] text-dim">
            Topos is in closed beta and I set up the first teams personally. Tell me how your team
            works with agents and I’ll get your first shared skills flowing, and your feedback
            shapes what gets built next.
          </p>
          <p className="mt-3 text-[13px] text-faint">Robert, founder</p>
          <div className="mt-5">
            <FounderAddressBlock />
          </div>
        </div>
      </section>

      <section id="faq" className="pt-[84px] lg:pt-[116px]">
        <div className={WRAP}>
          <MicroLabel>FAQ</MicroLabel>
          <SectionHeading>Questions people ask first.</SectionHeading>
          <dl className="mt-6 grid gap-x-12 gap-y-7 lg:grid-cols-2">
            {FAQ.map((item) => (
              <div key={item.q}>
                <dt className="font-display font-semibold text-[13.5px] text-ink tracking-[-0.01em]">
                  {item.q}
                </dt>
                <dd className="mt-2 max-w-[54ch] text-[14px] text-dim leading-[1.6]">{item.a}</dd>
              </div>
            ))}
          </dl>
        </div>
      </section>

      <footer className="mt-[88px] border-line-soft border-t pt-11 pb-[72px]">
        <div
          className={`${WRAP} flex flex-wrap items-center justify-between gap-x-8 gap-y-5 text-[13px] text-faint`}
        >
          <div className="flex flex-wrap items-center gap-x-7 gap-y-4">
            <Link to={ctaTo} className={PRIMARY_BUTTON}>
              {ctaLabel}
            </Link>
            <a
              href={GITHUB}
              target="_blank"
              rel="noreferrer"
              className="flex items-center gap-1.5 transition-colors hover:text-ink"
            >
              <GitHubMark className="h-3.5 w-3.5" />
              GitHub
              <span className="sr-only"> (opens in a new tab)</span>
            </a>
            <a href={DOCS} className="transition-colors hover:text-ink">
              Docs
            </a>
            <Link to="/app" className="transition-colors hover:text-ink">
              Sign in
            </Link>
          </div>
          <p className="text-[12px]">Apache-2.0 {"·"} © 2026 Topos</p>
        </div>
      </footer>
    </div>
  );
}
