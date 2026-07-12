/**
 * The entitlements seam — the third composition seam. The product speaks THREE primitives and
 * no more: SWITCHES (feature on/off), LIMITS (resource quotas), and — recorded elsewhere, never
 * read here — meters. Pricing, plans, and billing are a downstream mapping onto these
 * primitives; no product surface ever knows about money.
 *
 * The OSS default is allow-all, in-process: every switch on, every limit absent (unlimited).
 * A downstream provider is also in-process — an interface implementation reading its own
 * tables, never an RPC.
 */
export interface Entitlements {
  /** Feature switch — absent keys default ON in the OSS build. */
  allows(key: string): boolean;
  /** Resource quota — `null` means unlimited. */
  limit(key: string): number | null;
}

export interface EntitlementsProvider {
  /** Entitlements for one workspace; `null` scopes account-level surfaces. */
  forWorkspace(workspaceId: string | null): Promise<Entitlements>;
}

const unlimited: Entitlements = {
  allows: () => true,
  limit: () => null,
};

/** The OSS default: a self-hosted deployment is never gated. */
export const allowAllEntitlements: EntitlementsProvider = {
  forWorkspace: () => Promise.resolve(unlimited),
};
