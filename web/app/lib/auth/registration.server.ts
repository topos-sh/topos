import { AsyncLocalStorage } from "node:async_hooks";
import { sql } from "drizzle-orm";
import { composition } from "@/composition.server";
import { theWorkspace } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import { invitation } from "@/lib/db/schema.app";
import { mailDelivery } from "@/lib/mail/transport.server";

/**
 * WHO MAY CREATE AN ACCOUNT is composition-owned (`composition.registration`):
 *
 * `open` — the hosted posture: every sign-up is admitted, with ZERO database reads. Sign-up
 * alone still grants no seat and admits nothing; admission stays what it always was (an
 * invitation, a claim, or a workspace's own membership ceremonies).
 *
 * `gated` — the OSS default: every account creation demands a proof:
 *   (a) the setup claim code (first user; machine control) — the claim ceremony runs its
 *       Better Auth sign-up inside [`withRegistrationCeremony`], which flips a request-scoped
 *       flag this module reads;
 *   (b) an invited address — a pending, unexpired `invitation` row — AND armed mail: the
 *       mailbox round-trip is the identity rung, so an invitation admits a sign-up only when
 *       the deployment can actually verify it (unarmed SMTP ⇒ inviting is disabled AND a
 *       leftover invitation admits nothing — the old self-asserted-email impersonation hole
 *       stays closed);
 *   (b2) the invitation-REDEMPTION ceremony — the invite page resolved a live single-use link
 *       token and mints the invited address's account inside [`withInvitationCeremony`]: the
 *       token's delivery to that mailbox is the proof, so no second mail round-trip gates it;
 *   (c) in SINGLE tenancy only, the operator knob `workspace.registration = 'open'` on THE
 *       one workspace this install serves, off by default.
 * Everything else gets ONE constant, non-enumerating refusal — the same answer whether the
 * address is unknown, uninvited, expired, or already taken.
 *
 * TENANCY: in MULTI tenancy the per-workspace `registration` knob is NEVER consulted —
 * account creation is a server-global act, and a knob scoped to one workspace must not open
 * sign-up for the whole server. A gated multi-tenant sign-up is admitted only by a pending,
 * unexpired invitation for the email in ANY workspace (the invitation row is itself
 * workspace-bound proof) AND armed mail; the invited seat still binds only in the
 * invitation's own workspace (`bindInvitedSeats`).
 *
 * Wired as Better Auth's `user.create.before` database hook: it runs under every sign-up
 * path (email+password, magic link, a composition's social rungs), so a future rung cannot
 * accidentally reopen registration.
 */

const ceremonyContext = new AsyncLocalStorage<{ ceremony: "claim" | "invitation" }>();

/** The claim ceremony wraps its internal sign-up call so the create hook admits it. */
export function withRegistrationCeremony<T>(fn: () => Promise<T>): Promise<T> {
  return ceremonyContext.run({ ceremony: "claim" }, fn);
}

/**
 * The invitation-redemption ceremony wraps ITS account mint the same way: the invite page has
 * already resolved a live single-use token before it creates the invited email's account, so
 * the create hook admits exactly that one sign-up — a ceremony wrapper, never an email check,
 * and never a policy the hook re-derives (the token's delivery to the mailbox is the proof).
 */
export function withInvitationCeremony<T>(fn: () => Promise<T>): Promise<T> {
  return ceremonyContext.run({ ceremony: "invitation" }, fn);
}

/** The one refusal string every closed path answers with (non-enumerating by sameness). */
export const REGISTRATION_REFUSED =
  "Sign-up is not open on this server. Ask a member to invite you.";

/** Pure decision, unit-testable: may THIS email register, given the observable facts? */
export function registrationDecision(facts: {
  policy: "gated" | "open";
  tenancy: "single" | "multi";
  inClaimCeremony: boolean;
  /** True inside the invitation-redemption ceremony: a live invite token already resolved. */
  inInvitationCeremony: boolean;
  /** The single-tenant workspace's own knob; ALWAYS null in multi (never consulted there). */
  registrationKnob: "invite_only" | "open" | null;
  pendingInvitation: boolean;
  mailArmed: boolean;
}): "allow" | "refuse" {
  if (facts.policy === "open") {
    return "allow";
  }
  if (facts.inClaimCeremony || facts.inInvitationCeremony) {
    return "allow";
  }
  // The workspace knob opens sign-up only where the install IS the workspace: a knob scoped
  // to one workspace never opens a multi-tenant server.
  if (facts.tenancy === "single" && facts.registrationKnob === "open") {
    return "allow";
  }
  if (facts.pendingInvitation && facts.mailArmed) {
    return "allow";
  }
  return "refuse";
}

/**
 * Is there a pending, unexpired invitation for this (lowered) address? In single tenancy the
 * check is scoped to THE one workspace's id — every invitation references it anyway, and the
 * explicit scope keeps that true by construction. A null scope (multi tenancy) checks ANY
 * workspace: the invitation row is workspace-bound proof, and the seat later binds only in
 * its own workspace.
 */
async function hasPendingInvitation(
  loweredEmail: string,
  workspaceId: string | null,
): Promise<boolean> {
  const rows = await getDb().execute(
    sql`SELECT 1 FROM ${invitation}
        WHERE email = ${loweredEmail} AND status = 'pending'
          AND (expires_at IS NULL OR expires_at > now())
          ${workspaceId === null ? sql`` : sql`AND workspace_id = ${workspaceId}`}
        LIMIT 1`,
  );
  return rows.rows.length > 0;
}

/**
 * The hook body: gather the facts, decide. Throwing here aborts the Better Auth sign-up;
 * the sign-up routes translate ANY failure into the constant refusal, so no path
 * distinguishes "taken" from "uninvited".
 */
export async function assertRegistrationAllowed(email: string): Promise<void> {
  // The two reads-free allows first — `registrationDecision` answers "allow" for both
  // whatever the remaining facts are, so the early returns just skip the database.
  if (composition.registration === "open") {
    return;
  }
  const ceremony = ceremonyContext.getStore()?.ceremony;
  if (ceremony === "claim" || ceremony === "invitation") {
    return;
  }
  const lowered = email.trim().toLowerCase();
  const tenancy = composition.tenancy;
  let registrationKnob: "invite_only" | "open" | null = null;
  let invitationScope: string | null = null;
  if (tenancy === "single") {
    const ws = await theWorkspace();
    registrationKnob = (ws?.registration as "invite_only" | "open" | undefined) ?? null;
    invitationScope = ws?.id ?? null;
  }
  const decision = registrationDecision({
    policy: "gated",
    tenancy,
    inClaimCeremony: false,
    inInvitationCeremony: false,
    registrationKnob,
    pendingInvitation: await hasPendingInvitation(lowered, invitationScope),
    mailArmed: mailDelivery().canSend,
  });
  if (decision === "refuse") {
    throw new Error(REGISTRATION_REFUSED);
  }
}
