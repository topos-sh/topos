import { type AuthProviderConfig, defaultAuthConfig } from "./topos-web/auth-config";
import { allowAllEntitlements, type EntitlementsProvider } from "./topos-web/entitlements";
import { type NavEntry, ossNav } from "./topos-web/nav";

/**
 * The composition root — the ONE module a deployment's build resolves to pick its providers.
 * This file is the OSS composition: default auth (email+password), allow-all entitlements,
 * the base nav. A superset build ships its own composition module (aliased over this path at
 * build time) that appends nav entries and swaps providers; route composition happens in
 * `app/routes.ts`, the other half of the same seam.
 */
export interface WebComposition {
  auth: AuthProviderConfig;
  entitlements: EntitlementsProvider;
  nav: NavEntry[];
}

export const composition: WebComposition = {
  auth: defaultAuthConfig,
  entitlements: allowAllEntitlements,
  nav: ossNav,
};
