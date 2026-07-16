import type { MetaFunction } from "react-router";
import { LandingPage } from "@/components/landing/landing-page";

/**
 * The MULTI-tenant top-level `/` — the marketing landing page, never a claim band (a multi-tenant
 * deployment mints no boot workspace, so there is nothing to claim and no probe to run). In SINGLE
 * tenancy the origin root is instead a workspace FACE (workspace-dashboard.tsx), whose anonymous
 * view renders this same `LandingPage` component WITH the first-run claim band.
 *
 * The origin ROOT is a resource address too: a non-browser DOCUMENT fetch gets the CONSTANT
 * protocol card, served whole from the server entry (handleRequest) before this route runs. A
 * browser gets the landing page.
 */
export const meta: MetaFunction = () => [
  // Absolute title: this one page already carries the brand, so it skips the `· Topos` suffix.
  { title: "Topos: align the behavior of every agent in your team" },
  {
    name: "description",
    content:
      "Your agents share skills, keep them current, and improve them together: one teammate’s fix upgrades every agent on the team.",
  },
];

export default function Landing() {
  return <LandingPage awaitingOwner={false} setupLine="" />;
}
