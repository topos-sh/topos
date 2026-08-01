import { Link } from "react-router";
import { MicroLabel, SectionHeading, WRAP } from "@/components/landing/landing-kit";

/**
 * The pricing band: three tiers standing on three steps of the neutral ramp, so the hierarchy is
 * carried by the paper rather than by colour. The free self-hosted tier sits at GROUND level — an
 * outline on the page, not a card raised off it — which keeps it present and honest without
 * competing for the eye; the hosted tier is the only one raised onto `panel` with the card shadow,
 * and it holds the band's two placed objects: the chip and the one accent button.
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
    label: "Self-hosted",
    price: "$0",
    per: "forever",
    desc: "Apache-2.0. Your machines, your git, your data. Unlimited people.",
    feats: [
      "The complete product",
      "Unlimited skills and versions",
      "Contribute-back review",
      "Community support",
    ],
    surface: "ground",
  },
  {
    label: "Team",
    price: "$20",
    per: "/ seat / month",
    desc: "Hosted by us. Nothing to run, nothing to keep patched.",
    feats: [
      "Everything in self-hosted",
      "Hosted plane, backed up",
      "Fleet visibility and one-click revert",
      "Email support",
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
/** On the ground card the quiet button drops its fill too, or it reads as the raised one. */
const BUTTON_QUIET_ON_GROUND = `${BUTTON_BASE} border border-line-soft text-dim hover:bg-panel`;

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
          Free to run yourself. Paid when you want it hosted.
        </SectionHeading>
        <p className="mt-4 max-w-[58ch] text-dim">
          A seat is anyone who works with an AI agent. Skills, versions, history and contribute-back
          are unlimited on every tier.
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
                {tier.surface === "lead" ? (
                  <Link to={ctaTo} className={BUTTON_PRIMARY}>
                    {ctaLabel}
                  </Link>
                ) : tier.surface === "ground" ? (
                  <a href={`${docsHref}/install`} className={BUTTON_QUIET_ON_GROUND}>
                    Read the install guide
                  </a>
                ) : (
                  <a href="#contact" className={BUTTON_QUIET}>
                    Talk to us
                  </a>
                )}
              </div>
            </div>
          ))}
        </div>
      </div>
    </section>
  );
}
