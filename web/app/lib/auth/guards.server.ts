import { data, redirect } from "react-router";
import { bearerToken, uniformNotFound } from "@/lib/api/wire.server";
import { deviceActorProbe } from "@/lib/db/queries.device.server";
import { planeMembership } from "@/lib/db/queries.server";
import { getAuth } from "./server";

/**
 * Authorization lives HERE — called at the top of every signed-in loader and action. The
 * shell's middleware cookie bounce is optimistic UX only. Misses on membership checks render
 * 404, never 403: the app does not confirm what exists.
 *
 * Guards MINT ACTORS: branded proof objects the data layer requires on every query. The brand
 * symbol is declared type-only and never exported, so no other module can construct an actor
 * without an explicit cast — a loader or action that skipped its guard cannot call a query,
 * and a wrong-scope actor fails the query's runtime workspace assertion. This is the
 * compile-time leg of the fail-closed design; the build-time gates in
 * scripts/check-boundary.mjs are the other leg.
 *
 * Workspace admission derives from the DIRECTORY's own roster (`plane.workspace_member`,
 * read-only) and from NOTHING else — never a web-tier membership table. The roster reads are
 * deliberately per-request: the roster is the authority and every render re-asks it.
 */

declare const actorBrand: unique symbol;

/** Proof of a signed-in identity with a VERIFIED email (every actor email is normalized). */
export interface UserActor {
  readonly [actorBrand]: true;
  readonly email: string;
}

/** A directory roster seat as the guard reads it (role + status straight off the row). */
export interface PlaneSeat {
  role: "owner" | "reviewer" | "member";
  status: "invited" | "confirmed";
}

/**
 * Proof of admission to ONE workspace: a CONFIRMED roster seat, carrying the directory's
 * role. The roster is the ONLY admission — there is no other way in.
 */
export type MemberActor = UserActor & {
  readonly workspaceId: string;
  readonly role: "owner" | "reviewer" | "member";
};

/** Proof of a CONFIRMED OWNER seat in ONE workspace — the only management-grade actor. */
export type OwnerActor = MemberActor & { readonly role: "owner" };

/** Proof of a decision-grade seat (owner or reviewer) — the review-action mint. */
export type ReviewerActor = MemberActor & { readonly role: "owner" | "reviewer" };

export type SessionData = NonNullable<Awaited<ReturnType<Auth["api"]["getSession"]>>>;
type Auth = ReturnType<typeof getAuth>;

/** Every email compare in the app goes through this. */
export function normalizeEmail(email: string): string {
  return email.trim().toLowerCase();
}

/**
 * Only a same-app path may ride a `next` query into a redirect target (an absolute URL or
 * `//host` would be an open redirect). Backslashes and percent-escapes are rejected too:
 * WHATWG URL parsing treats `\` as `/` (so `/\evil.com` normalizes off-origin), and a
 * downstream redirect layer may decode `%5C`/`%2F` first — either turns a "relative" path
 * off-origin. Legit values (e.g. `/verify/<code>`) contain neither. The fallback is the
 * dashboard.
 */
export function safeNextPath(next: string | undefined): string {
  if (!next?.startsWith("/") || next.startsWith("//")) {
    return "/workspaces";
  }
  if (next.includes("\\") || next.includes("%")) {
    return "/workspaces";
  }
  // WHATWG URL parsing STRIPS ASCII control characters before parsing, so "/\t//evil.com"
  // would normalize off-origin in any consumer that resolves the value — reject them outright.
  // biome-ignore lint/suspicious/noControlCharactersInRegex: the control range IS the check.
  if (/[\x00-\x1f\x7f]/.test(next)) {
    return "/workspaces";
  }
  return next;
}

/** The uniform miss: pages and loaders throw this, the root boundary renders it. */
export function notFound(): never {
  throw data(null, { status: 404 });
}

/** The signed-out bounce for loaders/actions. */
export async function requireSession(request: Request): Promise<SessionData> {
  const session = await getAuth().api.getSession({ headers: request.headers });
  if (!session) {
    throw redirect("/login");
  }
  return session;
}

/**
 * Printable ASCII only, checked on the RAW session email BEFORE normalization: directory
 * principals are ASCII-canonical by CHECK, and JS `toLowerCase()` folds Unicode lookalikes
 * into ASCII (U+212A KELVIN SIGN becomes `k`), so normalizing first would let a verified
 * lookalike address false-match a real roster seat. A non-ASCII email can never legitimately
 * hold a seat, so refusing the actor outright is honest.
 */
const PRINTABLE_ASCII_RE = /^[\x20-\x7e]+$/;

/**
 * Mint a UserActor from a session — null unless the email is VERIFIED (membership and every
 * data write are keyed on email; an unverified one never becomes an actor). Callers own the
 * unverified UX (error copy in actions, 404 on pages); the mint itself is the one place a
 * plain session becomes data-layer authority.
 */
export function actorFromSession(session: SessionData | null | undefined): UserActor | null {
  if (!session?.user.emailVerified) {
    return null;
  }
  if (!PRINTABLE_ASCII_RE.test(session.user.email)) {
    return null;
  }
  return { email: normalizeEmail(session.user.email) } as UserActor;
}

/** The pure admission decision, one workspace at a time. */
export type Admission =
  | { kind: "roster"; role: "owner" | "reviewer" | "member" }
  | { kind: "miss" };

/**
 * The admission truth table, pure and DB-free (unit-tested as such):
 *   confirmed seat  → roster (the directory's role rides along);
 *   invited seat    → miss (an invite promises index visibility, never admission —
 *                     the enrollment proof is the real join; an invited OWNER seat
 *                     admits nothing either);
 *   no seat         → miss.
 */
export function resolveAdmission(seat: PlaneSeat | undefined): Admission {
  if (seat?.status === "confirmed") {
    return { kind: "roster", role: seat.role };
  }
  return { kind: "miss" };
}

/**
 * Admission to THIS workspace, derived per-request from the directory roster (confirmed
 * seats only). Misses get the uniform 404 — an invited-but-unconfirmed principal included:
 * their surface is the index row and its join instructions, never an actor.
 */
export async function requireMember(request: Request, workspaceId: string): Promise<MemberActor> {
  const session = await requireSession(request);
  const actor = actorFromSession(session);
  if (!actor) {
    notFound();
  }
  const seat = await planeMembership(actor, workspaceId);
  const admission = resolveAdmission(seat);
  if (admission.kind === "miss") {
    notFound();
  }
  return { email: actor.email, workspaceId, role: admission.role } as MemberActor;
}

/**
 * A CONFIRMED OWNER seat in THIS workspace — the management gate (policy toggle, roster
 * mutations). The database's guarded functions re-run their own role gates; this guard is
 * the web tier's matching lock. 404 on anything less.
 */
export async function requireWorkspaceOwner(
  request: Request,
  workspaceId: string,
): Promise<OwnerActor> {
  const actor = await requireMember(request, workspaceId);
  if (actor.role !== "owner") {
    notFound();
  }
  return actor as OwnerActor;
}

/**
 * A CONFIRMED owner-or-reviewer seat in THIS workspace — the decision gate for the review
 * actions (approve/reject a proposal, revert). Used ONLY inside actions: proposal PAGES stay
 * guarded by requireMember (member read-only is a legitimate page state), and the vault's
 * in-transaction role gate stays the authority behind this guard. 404 on anything less — the
 * house miss posture, never a permissions claim.
 */
export async function requireReviewer(
  request: Request,
  workspaceId: string,
): Promise<ReviewerActor> {
  const actor = await requireMember(request, workspaceId);
  if (actor.role === "member") {
    notFound();
  }
  return actor as ReviewerActor;
}

/**
 * Proof of an authenticated DEVICE — the `/api/v1` lane's actor: a presented workspace
 * credential resolved (in Postgres — this tier computes no digest) to its non-revoked registry
 * row on a CONFIRMED seat. Person and device ids come from the trusted rows, NEVER a
 * client-asserted field; the role rides along for the guarded functions that band on it.
 */
export type DeviceActor = UserActor & {
  readonly workspaceId: string;
  readonly person: string;
  readonly deviceKeyId: string;
  readonly role: "owner" | "reviewer" | "member";
};

/**
 * The device lane's front door. Every miss — no/blank/foreign-scheme Authorization, unknown
 * credential, revoked device, unknown workspace, unseated or unconfirmed principal — throws the
 * ONE uniform wire 404 (an ENVELOPE body, not the HTML miss: the caller is a device, and the
 * vault's own edge answers the identical bytes). This guard authenticates ONLY the ops this
 * tier serves itself; forwarded byte/pointer ops pass through untouched so the vault's
 * in-transaction resolve (and its replay-before-revoked ordering) stays the sole authority.
 */
export async function requireDeviceActor(
  request: Request,
  workspaceId: string,
): Promise<DeviceActor> {
  const credential = bearerToken(request);
  if (credential === null) {
    throw uniformNotFound();
  }
  const row = await deviceActorProbe(workspaceId, credential);
  if (row === null) {
    throw uniformNotFound();
  }
  return {
    email: row.person,
    workspaceId,
    person: row.person,
    deviceKeyId: row.deviceKeyId,
    role: row.role,
  } as DeviceActor;
}
