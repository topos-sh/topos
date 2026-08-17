import { Link } from "react-router";
import { MicroLabel, SectionHeading, WRAP } from "@/components/landing/landing-kit";

/**
 * The pricing band: three tiers standing on three steps of the neutral ramp, so the hierarchy is
 * carried by the paper rather than by colour. The free tier sits at GROUND level — an outline on
 * the page, not a card raised off it — and carries the one accent button, the page's entry action;
 * the Team tier is the only one raised onto `panel` with the card shadow and holds the chip.
 * Self-hosting is a posture, not a plan: it lives in the one line under the card row.
 *
 * A seat is a person, not a machine and not an agent: the number a buyer can count without
 * inspecting anything.
 */

type Surface = "ground" | "panel" | "lead";

type Tier = {
  label: string;
  price: string;
  /** The unit, set beside the figure at body weight so the number keeps the display voice. */
  per: string;
  desc: string;
  feats: string[];
  surface: Surface;
};

const TIERS: Tier[] = [
  {
    label: "Free",
    price: "$0",
    per: "",
    desc: "For small teams.",
    feats: [
      "Up to 3 people, unlimited agents and machines",
      "20 shared bundles, 250 MB of storage",
      "30 days of version history",
      "Automatic updates and contribute-back",
    ],
    surface: "ground",
  },
  {
    label: "Team",
    price: "$20",
    per: "/ seat / month",
    desc: "Hosted by us. Nothing to run, nothing to keep patched.",
    feats: [
      "Unlimited people",
      "Unlimited bundles",
      "Unlimited version history",
      "Review and approval workflow",
    ],
    surface: "lead",
  },
  {
    label: "Enterprise",
    price: "$40",
    per: "/ seat / month",
    desc: "For companies that need the paperwork to line up.",
    feats: [
      "Everything in Team",
      "SSO and SCIM provisioning",
      "Audit export and retention",
      "SLA and priority support",
    ],
    surface: "panel",
  },
];

/** Three card shells, one per step of the ramp. Written out so the cascade stays readable. */
const SHELL: Record<Surface, string> = {
  ground: "border-line-soft bg-ground p-6",
  panel: "border-line-soft bg-panel p-6",
  lead: "border-line bg-panel p-7 shadow-card",
};

const BUTTON_BASE =
  "mt-6 inline-flex h-9 items-center justify-center rounded-md font-mono text-[12.5px] transition-colors focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2 active:scale-[0.98]";
const BUTTON_PRIMARY = `${BUTTON_BASE} bg-accent text-on-accent hover:bg-accent-deep`;
const BUTTON_QUIET = `${BUTTON_BASE} border border-line bg-panel text-dim hover:bg-panel2`;

function Figure({ price, per }: { price: string; per: string }) {
  return (
    <p className="mt-4 font-display font-semibold text-[27px] text-ink tracking-[-0.03em] tabular-nums">
      {price}
      <span className="ml-1.5 font-normal font-sans text-[13px] text-faint tracking-normal">
        {per}
      </span>
    </p>
  );
}

export function PricingBand({
  ctaTo,
  ctaLabel,
  docsHref,
}: {
  /** Where the hosted tier's primary action goes — the page's own tenancy-aware target. */
  ctaTo: string;
  ctaLabel: string;
  docsHref: string;
}) {
  return (
    <section id="pricing" className="pt-[84px] lg:pt-[116px]">
      <div className={WRAP}>
        <MicroLabel>Pricing</MicroLabel>
        <SectionHeading className="max-w-[44ch]">
          Start free. Pay when your team grows.
        </SectionHeading>
        <p className="mt-4 max-w-[58ch] text-dim">
          A seat is anyone who works with an AI agent. One seat covers all of that person's agents
          and machines.
        </p>

        <div className="mt-6 grid gap-4 lg:grid-cols-3">
          {TIERS.map((tier) => (
            <div
              key={tier.label}
              className={`flex flex-col rounded-lg border ${SHELL[tier.surface]}`}
            >
              <div className="flex min-h-[22px] items-center justify-between gap-2.5">
                <MicroLabel>{tier.label}</MicroLabel>
                {tier.surface === "lead" && (
                  <span className="rounded-full bg-accent px-2.5 py-[3px] font-display font-medium text-[10px] text-on-accent uppercase leading-[1.2] tracking-[0.12em]">
                    Most teams
                  </span>
                )}
              </div>

              <Figure price={tier.price} per={tier.per} />

              <p className="mt-3 min-h-[40px] text-[13.5px] text-faint leading-[1.5]">
                {tier.desc}
              </p>

              <ul className="mt-4 flex flex-col">
                {tier.feats.map((feat) => (
                  <li
                    key={feat}
                    className="border-line-soft border-t py-[9px] text-[14px] text-dim first:border-t-0"
                  >
                    {feat}
                  </li>
                ))}
              </ul>

              {/* The action sits on the card's floor, so three cards of unequal copy still line up. */}
              <div className="mt-auto flex flex-col">
                {tier.surface === "ground" ? (
                  <Link to={ctaTo} className={BUTTON_PRIMARY}>
                    {ctaLabel}
                  </Link>
                ) : (
                  <a href="#contact" className={BUTTON_QUIET}>
                    Talk to us
                  </a>
                )}
              </div>
            </div>
          ))}
        </div>

        {/* Self-hosting left the card row deliberately: it is a posture, not a plan. */}
        <p className="mt-5 text-[13.5px] text-faint">
          Prefer your own servers? Topos is Apache-2.0, and self-hosting is free.{" "}
          <a
            href={`${docsHref}/install`}
            className="text-dim underline decoration-line underline-offset-[3px] transition-colors hover:text-ink"
          >
            Read the install guide
          </a>
          .
        </p>
      </div>
    </section>
  );
}
