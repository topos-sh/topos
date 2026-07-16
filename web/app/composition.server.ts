import { sendMagicLinkEmail } from "./lib/mail/magic-link-mail.server";
import { mailDelivery } from "./lib/mail/transport.server";
import { type AuthProviderConfig, defaultAuthConfig } from "./topos-web/auth-config";
import { allowAllEntitlements, type EntitlementsProvider } from "./topos-web/entitlements";
import { type NavEntry, ossNav } from "./topos-web/nav";

/**
 * The composition root — the ONE module a deployment's build resolves to pick its providers.
 * This file is the OSS composition: default auth (email+password, plus the magic-link rung
 * exactly when the mail transport is armed — a rung never advertises a dead door), allow-all
 * entitlements, the base nav, and the SINGLE-tenant address grammar. A superset build ships its
 * own composition module (aliased over this path at build time) that appends nav entries and
 * swaps providers; route composition happens in `app/routes.ts`, the other half of the same
 * seam — a deployment passes the SAME tenancy literal to `ossRoutes({ tenancy })` there.
 */

/**
 * How this deployment addresses workspaces:
 * - `single` — the install IS its one workspace: the origin root is the workspace address and
 *   the whole signed-in surface mounts at origin-rooted paths. The OSS default.
 * - `multi` — workspaces live at `/<workspace-name>`; the same page modules mount under the
 *   `:ws` name slug. No boot workspace is minted and the claim ceremony does not exist.
 */
export type Tenancy = "single" | "multi";

/**
 * Who may create an account here — COMPOSITION-owned, so a deployment (not a runtime knob alone)
 * decides its posture:
 * - `gated` — the OSS default truth-table: the claim ceremony, a pending invitation on armed
 *   SMTP, or the per-workspace `registration = 'open'` knob (single-tenant only — a workspace
 *   knob never opens a multi-tenant server). Everything else gets the one constant refusal.
 * - `open` — anyone signs up through any rung (a hosted product's posture). Sign-up alone still
 *   grants no seat and admits nothing.
 */
export type RegistrationPolicy = "gated" | "open";

export interface WebComposition {
  auth: AuthProviderConfig;
  entitlements: EntitlementsProvider;
  nav: NavEntry[];
  tenancy: Tenancy;
  registration: RegistrationPolicy;
  /**
   * Top-level path segments THIS deployment additionally reserves as workspace names, unioned
   * with the OSS route table's own statics + the future-reserve list (`topos-web/segments.ts`).
   * A superset build lists its private top-level routes here so no workspace name occludes them.
   */
  reservedWorkspaceNames: readonly string[];
}

/**
 * The auth rungs are resolved LAZILY (mail armed-ness reads the env, which a CI build lacks),
 * memoized on first read like every other env-derived config.
 */
let resolvedAuth: AuthProviderConfig | undefined;
function ossAuthConfig(): AuthProviderConfig {
  resolvedAuth ??= {
    ...defaultAuthConfig,
    ...(mailDelivery().canSend ? { magicLink: { send: sendMagicLinkEmail } } : {}),
  };
  return resolvedAuth;
}

export const composition: WebComposition = {
  get auth() {
    return ossAuthConfig();
  },
  entitlements: allowAllEntitlements,
  nav: ossNav,
  tenancy: "single",
  registration: "gated",
  reservedWorkspaceNames: [],
};
