import { AsyncLocalStorage } from "node:async_hooks";
import { sql } from "drizzle-orm";
import { getDb } from "@/lib/db/index.server";
import { invitation } from "@/lib/db/schema.app";
import { mailDelivery } from "@/lib/mail/transport.server";

/**
 * REGISTRATION IS NEVER OPEN. Every account creation demands a proof:
 *   (a) the setup claim code (first user; machine control) — the claim ceremony runs its
 *       Better Auth sign-up inside [`withRegistrationCeremony`], which flips a request-scoped
 *       flag this module reads;
 *   (b) an invited address — a pending, unexpired `invitation` row — AND armed mail: the
 *       mailbox round-trip is the identity rung, so an invitation admits a sign-up only when
 *       the deployment can actually verify it (unarmed SMTP ⇒ inviting is disabled AND a
 *       leftover invitation admits nothing — the old self-asserted-email impersonation hole
 *       stays closed);
 *   (c) the operator knob `workspace.registration = 'open'`, off by default.
 * Everything else gets ONE constant, non-enumerating refusal — the same answer whether the
 * address is unknown, uninvited, expired, or already taken.
 *
 * Wired as Better Auth's `user.create.before` database hook: it runs under every sign-up
 * path (email+password, magic link, a composition's social rungs), so a future rung cannot
 * accidentally reopen registration.
 */

const ceremonyContext = new AsyncLocalStorage<{ ceremony: "claim" }>();

/** The claim ceremony wraps its internal sign-up call so the create hook admits it. */
export function withRegistrationCeremony<T>(fn: () => Promise<T>): Promise<T> {
  return ceremonyContext.run({ ceremony: "claim" }, fn);
}

/** The one refusal string every closed path answers with (non-enumerating by sameness). */
export const REGISTRATION_REFUSED =
  "Sign-up is not open on this server. Ask a member to invite you.";

/** Pure decision, unit-testable: may THIS email register, given the observable facts? */
export function registrationDecision(facts: {
  inClaimCeremony: boolean;
  registrationKnob: "invite_only" | "open" | null;
  pendingInvitation: boolean;
  mailArmed: boolean;
}): "allow" | "refuse" {
  if (facts.inClaimCeremony) {
    return "allow";
  }
  if (facts.registrationKnob === "open") {
    return "allow";
  }
  if (facts.pendingInvitation && facts.mailArmed) {
    return "allow";
  }
  return "refuse";
}

/**
 * The hook body: gather the facts, decide. Throwing here aborts the Better Auth sign-up;
 * the sign-up routes translate ANY failure into the constant refusal, so no path
 * distinguishes "taken" from "uninvited".
 */
export async function assertRegistrationAllowed(email: string): Promise<void> {
  if (ceremonyContext.getStore()?.ceremony === "claim") {
    return;
  }
  const db = getDb();
  const lowered = email.trim().toLowerCase();
  const knobRows = await db.execute(sql`SELECT registration FROM web.workspace LIMIT 1`);
  const knob =
    (knobRows.rows[0] as { registration: "invite_only" | "open" } | undefined)?.registration ??
    null;
  const inviteRows = await db.execute(
    sql`SELECT 1 FROM ${invitation}
        WHERE email = ${lowered} AND status = 'pending'
          AND (expires_at IS NULL OR expires_at > now())
        LIMIT 1`,
  );
  const decision = registrationDecision({
    inClaimCeremony: false,
    registrationKnob: knob,
    pendingInvitation: inviteRows.rows.length > 0,
    mailArmed: mailDelivery().canSend,
  });
  if (decision === "refuse") {
    throw new Error(REGISTRATION_REFUSED);
  }
}
