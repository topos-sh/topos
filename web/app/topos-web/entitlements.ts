/**
 * The entitlements seam — the third composition seam. The product speaks THREE primitives and
 * no more: SWITCHES (feature on/off), LIMITS (resource quotas), and — recorded elsewhere, never
 * read here — meters. Pricing, plans, and billing are a downstream mapping onto these
 * primitives; no product surface ever knows about money.
 *
 * The OSS default is allow-all, in-process: every switch on, every limit absent (unlimited).
 * A downstream provider is also in-process — an interface implementation reading its own
 * tables, never an RPC.
 *
 * THE KEYS THE PRODUCT CONSULTS — each named once here, with its scope, its default when the
 * provider answers nothing (`null` / `true`), and where it is enforced. Refusal copy at every
 * enforcement point is neutral ("limit for this workspace") — this seam is the only coupling a
 * downstream mapping has.
 *
 * Switches (`allows`; absent ⇒ ON):
 *  - `workspace-create` — scope `forWorkspace(null)`: whether self-serve workspace creation
 *    exists at all (`/new` and the /verify create arm).
 *  - `reviews` — per workspace: whether review protection may be ENABLED (the skill/channel
 *    protection writes and the workspace review default). Already-protected bundles keep
 *    working — the gate is on enabling, never a retroactive strip.
 *
 * Limits (`limit`; absent ⇒ unlimited, EXCEPT the floored keys below):
 *  - `members` — per workspace: seats + pending invitations, consulted at invite creation
 *    (both DAL doors) and at seat mint (the accept ceremonies; the workspace-birth owner
 *    seat is exempt).
 *  - `bundles` — per workspace: active catalog rows, consulted when a NEW bundle identity is
 *    created (every genesis door). New versions of existing bundles are never blocked.
 *  - `storage-bytes` — per workspace: stored custody bytes, consulted at publish/propose
 *    ingest (stat read fails OPEN — the ingest shares the backend and fails on real outage).
 *  - `history-days` — per workspace: how far back reverts may reach; older rows stay listed
 *    (annotated, never deleted — a wider window restores access).
 *  - `invites-per-day` — per account: FLOORED — an absent row means the built-in floor
 *    (10/day while the account is under 48h old, else 50/day), and a present row wins even
 *    when lower. The same inversion as `workspace-create-per-day`.
 *  - `workspaces-owned` — scope `forWorkspace(null)`: FLOORED at 3 — count of currently
 *    owned workspaces, consulted at workspace creation; a present row wins even when lower.
 *  - `workspace-create-per-day` — scope `forWorkspace(null)`: FLOORED (see
 *    `workspace-create.server.ts`); a present row wins even when lower.
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
