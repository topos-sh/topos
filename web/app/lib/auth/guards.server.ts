import { data, redirect } from "react-router";
import { composition } from "@/composition.server";
import { bearerToken, uniformNotFound } from "@/lib/api/wire.server";
import { seatOf, sessionActor, theWorkspace, workspaceByName } from "@/lib/db/identity.server";
import { personDisplay } from "@/lib/person-display";
import { getAuth } from "./server";

/** The workspace row a scoped page resolves — the non-null result of the tenancy lookup. */
export type ScopedWorkspace = NonNullable<Awaited<ReturnType<typeof theWorkspace>>>;

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
    return "/app";
  }
  if (next.includes("\\") || next.includes("%")) {
    return "/app";
  }
  // WHATWG URL parsing STRIPS ASCII control characters before parsing, so "/\t//evil.com"
  // would normalize off-origin in any consumer that resolves the value — reject them outright.
  // biome-ignore lint/suspicious/noControlCharactersInRegex: the control range IS the check.
  if (/[\x00-\x1f\x7f]/.test(next)) {
    return "/app";
  }
  return next;
}

/** The uniform miss: pages and loaders throw this, the root boundary renders it. */
export function notFound(): never {
  throw data(null, { status: 404 });
}

/**
 * The workspace a scoped page addresses, resolved through the deployment's tenancy grammar:
 * SINGLE → the one boot-minted workspace (`params.ws` is absent — the URL is origin-rooted);
 * MULTI → the workspace the `:ws` NAME slug names. Every workspace-scoped loader/action resolves
 * through this and then keeps using the id-keyed guards/queries (`requireMember(actor,
 * workspace.id)`). A miss is the uniform 404 — never a 403, never an existence oracle: an unknown
 * slug and a non-member both land the same house 404.
 */
export async function workspaceInScope(params: { ws?: string }): Promise<ScopedWorkspace> {
  if (composition.tenancy === "multi") {
    const name = params.ws;
    if (name === undefined || name.length === 0) {
      notFound();
    }
    const ws = await workspaceByName(name);
    if (ws === null) {
      notFound();
    }
    return ws;
  }
  const ws = await theWorkspace();
  if (ws === null) {
    notFound();
  }
  return ws;
}

/** A membership-resolved workspace scope: the workspace row + the admitted actor, together. */
export interface ScopedMember {
  workspace: ScopedWorkspace;
  actor: MemberActor;
}

/**
 * THE membership-or-404 resolution every workspace-scoped page runs — the one place the
 * slug→workspace→seat chain is written. Order matters for the existence blind: the caller
 * resolves its SESSION first (requireMemberInScope below, or a face's own anonymous split), so
 * by the time this runs the only remaining outcomes are the canonical page (a seat) or the
 * uniform 404 (unknown slug and seatless visitor alike — same throw, same body, no oracle).
 */
export async function memberInScope(
  actor: UserActor,
  params: { ws?: string },
): Promise<ScopedMember> {
  const workspace = await workspaceInScope(params);
  const admission = resolveAdmission(await seatOf(actor.userId, workspace.id));
  if (admission.kind === "miss") {
    notFound();
  }
  return {
    workspace,
    actor: {
      userId: actor.userId,
      display: actor.display,
      workspaceId: workspace.id,
      role: admission.role,
    } as MemberActor,
  };
}

/**
 * The workspace-scoped guard for member-only loaders/actions: session FIRST (an anonymous or
 * invalid-session request bounces to the constant /login BEFORE any workspace read, so a real
 * slug and an invented one answer byte-identically), then the one memberInScope resolution
 * (unknown slug and non-member land the same uniform 404).
 */
export async function requireMemberInScope(
  request: Request,
  params: { ws?: string },
): Promise<ScopedMember> {
  const session = await requireSession(request);
  const actor = actorFromSession(session);
  if (!actor) {
    throw redirect("/login");
  }
  return memberInScope(actor, params);
}

/** requireMemberInScope, then the owner gate — for pages owner-gated from the top. 404 below owner. */
export async function requireOwnerInScope(
  request: Request,
  params: { ws?: string },
): Promise<{ workspace: ScopedWorkspace; actor: OwnerActor }> {
  const { workspace, actor } = await requireMemberInScope(request, params);
  if (actor.role !== "owner") {
    notFound();
  }
  return { workspace, actor: actor as OwnerActor };
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
  const display = personDisplay(session.user.name, session.user.email ?? "unknown");
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
 * Proof of an authenticated SESSION — the `/api/v1` lane's actor: the presented bearer
 * resolved (hash computed in Postgres — this tier computes no digest) credential → live
 * session → seat, fail-closed. The credential is WORKSPACE-SCOPED: a session resolves only
 * against its own workspace's paths. Person and session ids come from the trusted rows,
 * NEVER a client-asserted field. `sessionStatus` is 'active' under the default guard; only
 * the two pending-tolerant routes ever see 'pending'.
 */
export type SessionActor = UserActor & {
  readonly workspaceId: string;
  readonly sessionId: string;
  readonly role: "owner" | "reviewer" | "member";
  readonly sessionStatus: "active" | "pending";
};

/**
 * The session lane's front door. Every miss — no/blank/foreign-scheme Authorization, unknown
 * credential, ended session, another workspace's session, unseated user, an expired session
 * (the workspace's max-age policy) — throws the ONE uniform wire 404 (an ENVELOPE body, not
 * the HTML miss: the caller is a machine). The default requires an ACTIVE session; exactly
 * two routes (`/me`, `/delivery`) pass `allowPending` — a live pending row proves standing,
 * so they answer typed with `session_status` instead of pretending the workspace does not
 * exist. Everything else folds a pending session into the same uniform 404 an unknown
 * credential gets.
 */
export async function requireSessionActor(
  request: Request,
  workspaceId: string,
  opts: { allowPending?: boolean } = {},
): Promise<SessionActor> {
  const credential = bearerToken(request);
  if (credential === null) {
    throw uniformNotFound();
  }
  const row = await sessionActor(workspaceId, credential);
  if (row === null) {
    throw uniformNotFound();
  }
  if (row.sessionStatus !== "active" && opts.allowPending !== true) {
    throw uniformNotFound();
  }
  return {
    userId: row.userId,
    display: row.userDisplay,
    workspaceId,
    sessionId: row.sessionId,
    role: row.role,
    sessionStatus: row.sessionStatus,
  } as SessionActor;
}
