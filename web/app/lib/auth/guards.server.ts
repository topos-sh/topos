import { data, redirect } from "react-router";
import { bearerToken, uniformNotFound } from "@/lib/api/wire.server";
import { deviceActor, seatOf } from "@/lib/db/identity.server";
import { getAuth } from "./server";

/**
 * Authorization lives HERE — called at the top of every signed-in loader and action. The
 * shell's middleware cookie bounce is optimistic UX only. Misses on membership checks render
 * 404, never 403: the app does not confirm what exists.
 *
 * Guards MINT ACTORS: branded proof objects the data layer requires on every query. The brand
 * symbol is declared type-only and never exported, so no other module can construct an actor
 * without an explicit cast — a loader or action that skipped its guard cannot call a query,
 * and a wrong-scope actor fails the query's runtime workspace assertion.
 *
 * ONE identity: a session resolves to `user.id`, and every admission resolves session →
 * user.id → seat, per request. Email is a login name and a display attribute — NOTHING here
 * (or anywhere in the data layer) authorizes by email equality, and no email normalization
 * or lookalike defense exists because no email is ever compared.
 */

declare const actorBrand: unique symbol;

/** Proof of a signed-in identity: the user id (THE identity) + a display snapshot. */
export interface UserActor {
  readonly [actorBrand]: true;
  readonly userId: string;
  readonly display: string;
}

/**
 * Proof of admission to ONE workspace: a seat, carrying its role. The seat table is the ONLY
 * admission — there is no other way in.
 */
export type MemberActor = UserActor & {
  readonly workspaceId: string;
  readonly role: "owner" | "reviewer" | "member";
};

/** Proof of an OWNER seat in ONE workspace — the only management-grade actor. */
export type OwnerActor = MemberActor & { readonly role: "owner" };

/** Proof of a decision-grade seat (owner or reviewer) — the review-action mint. */
export type ReviewerActor = MemberActor & { readonly role: "owner" | "reviewer" };

export type SessionData = NonNullable<Awaited<ReturnType<Auth["api"]["getSession"]>>>;
type Auth = ReturnType<typeof getAuth>;

/**
 * Only a same-app path may ride a `next` query into a redirect target (an absolute URL or
 * `//host` would be an open redirect). Backslashes and percent-escapes are rejected too:
 * WHATWG URL parsing treats `\` as `/` (so `/\evil.com` normalizes off-origin), and a
 * downstream redirect layer may decode `%5C`/`%2F` first — either turns a "relative" path
 * off-origin. Legit values (e.g. `/verify?code=…`) contain neither. The fallback is the
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
 * Mint a UserActor from a session. The id is the identity; the display snapshot (name, else
 * the email as a readable fallback) rides into audit rows. Verification status does NOT gate
 * the mint — authority is seats, and how an account was born (claim, invitation, open knob)
 * already decided its legitimacy.
 */
export function actorFromSession(session: SessionData | null | undefined): UserActor | null {
  if (!session?.user.id) {
    return null;
  }
  const display =
    session.user.name.trim().length > 0 ? session.user.name : (session.user.email ?? "unknown");
  return { userId: session.user.id, display } as UserActor;
}

/** The pure admission decision, one workspace at a time. */
export type Admission = { kind: "seat"; role: "owner" | "reviewer" | "member" } | { kind: "miss" };

/**
 * The admission truth table, pure and DB-free: a seat admits with its role; no seat is a
 * miss. (Invitations are claims on FUTURE users in their own table — holding one admits
 * nothing; the verified sign-up ceremony converts it into a seat.)
 */
export function resolveAdmission(
  seat: { role: "owner" | "reviewer" | "member" } | undefined,
): Admission {
  if (seat) {
    return { kind: "seat", role: seat.role };
  }
  return { kind: "miss" };
}

/** Admission to THIS workspace, derived per-request from the seat table. Misses 404. */
export async function requireMember(request: Request, workspaceId: string): Promise<MemberActor> {
  const session = await requireSession(request);
  const actor = actorFromSession(session);
  if (!actor) {
    notFound();
  }
  const admission = resolveAdmission(await seatOf(actor.userId, workspaceId));
  if (admission.kind === "miss") {
    notFound();
  }
  return {
    userId: actor.userId,
    display: actor.display,
    workspaceId,
    role: admission.role,
  } as MemberActor;
}

/**
 * An OWNER seat in THIS workspace — the management gate (policy toggles, roster mutations,
 * lifecycle ceremonies). 404 on anything less.
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
 * An owner-or-reviewer seat in THIS workspace — the decision gate for review actions
 * (approve/reject a proposal, revert). Used ONLY inside actions: proposal PAGES stay guarded
 * by requireMember (member read-only is a legitimate page state). 404 on anything less.
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
 * Proof of an authenticated DEVICE — the `/api/v1` lane's actor: the presented bearer
 * resolved (hash computed in Postgres — this tier computes no digest) credential → device →
 * user → seat, fail-closed. Person and device ids come from the trusted rows, NEVER a
 * client-asserted field.
 */
export type DeviceActor = UserActor & {
  readonly workspaceId: string;
  readonly deviceId: string;
  readonly role: "owner" | "reviewer" | "member";
};

/**
 * The device lane's front door. Every miss — no/blank/foreign-scheme Authorization, unknown
 * credential, revoked device, unknown workspace, unseated user — throws the ONE uniform wire
 * 404 (an ENVELOPE body, not the HTML miss: the caller is a device). Since the identity
 * unification this guard authenticates EVERY device-lane op; the custody forwarder runs
 * behind it, app-authorized.
 */
export async function requireDeviceActor(
  request: Request,
  workspaceId: string,
): Promise<DeviceActor> {
  const credential = bearerToken(request);
  if (credential === null) {
    throw uniformNotFound();
  }
  const row = await deviceActor(workspaceId, credential);
  if (row === null) {
    throw uniformNotFound();
  }
  return {
    userId: row.userId,
    display: row.userDisplay,
    workspaceId,
    deviceId: row.deviceId,
    role: row.role,
  } as DeviceActor;
}
