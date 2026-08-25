import { data, redirect } from "react-router";
import { composition } from "@/composition.server";
import { bearerToken, machineTokenRefused, uniformNotFound } from "@/lib/api/wire.server";
import {
  seatOf,
  sessionActor,
  sessionActorByCredential,
  theWorkspace,
  workspaceByName,
} from "@/lib/db/identity.server";
import { MACHINE_TOKEN_PREFIX, tokenActor } from "@/lib/db/queries.tokens.server";
import { personDisplay } from "@/lib/person-display";
import { publicOrigin } from "@/lib/plane/public-base.server";
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

/**
 * THE MEMBER-ONLY PAGE guard — requireMemberInScope's twin for the HTML face, differing in one
 * thing: a SIGNED-OUT visitor gets the uniform 404, not a bounce to /login.
 *
 * A workspace address is members-only in every face, and the bundle face already answered a
 * stranger with the house 404 — the same answer a mistyped path gets. A sibling page under the
 * same bundle that bounced to /login instead handed a signed-out visitor a second, different
 * answer for the same address family, and read as an invitation to sign in to a workspace they
 * have no seat in. Nothing is read before the refusal, so it stays existence-blind by
 * construction: real slug and invented slug get the same body.
 *
 * Actions keep requireMemberInScope. A person who is signed out mid-form has somewhere to go, and
 * a POST is not an address anybody probes.
 */
export async function memberPageInScope(
  request: Request,
  params: { ws?: string },
): Promise<ScopedMember> {
  assertSameOrigin(request);
  const actor = actorFromSession(await getAuth().api.getSession({ headers: request.headers }));
  if (actor === null) {
    notFound();
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
  assertSameOrigin(request);
  const session = await getAuth().api.getSession({ headers: request.headers });
  if (!session) {
    throw redirect("/login");
  }
  return session;
}

/**
 * A cookie-authorized WRITE must come from this app's own origin.
 *
 * The session cookie is `SameSite=Lax`, which blocks a cross-SITE form post — but "site" is the
 * registrable domain, so a sibling host (`wiki.corp.example` beside a self-hosted
 * `topos.corp.example`, or any subdomain an attacker gets a foothold on) is same-site and its
 * POST arrives with the cookie attached. Every people-affecting ceremony in the product is a
 * form post authorized by that cookie, and the two-step confirms are client-side UI — a forged
 * submit skips the button entirely, and type-the-name compares a field the forging page can
 * fill with a name it already knows. So provenance is checked here, once, for the whole
 * cookie-authorized surface.
 *
 * GET/HEAD are exempt: they are not writes, and no loader in this app mutates. The bearer lane
 * is unaffected — it never reads a cookie, so it has nothing to forge with, and it does not
 * pass through this guard. A missing `Origin` on a write fails closed: every browser has sent
 * it on cross-origin-capable requests for years, and a non-browser caller belongs on the
 * bearer lane.
 */
export function assertSameOrigin(request: Request): void {
  if (request.method === "GET" || request.method === "HEAD") {
    return;
  }
  const origin = request.headers.get("origin");
  if (origin === null) {
    throw uniformNotFound();
  }
  let presented: string;
  try {
    presented = new URL(origin).host;
  } catch {
    throw uniformNotFound();
  }
  // Both spellings of "this app" count. Behind a TLS-terminating proxy the container sees its
  // own internal host on `request.url` while the browser sends the PUBLIC one, so comparing
  // against either alone would refuse every real write on one topology or the other.
  const requestHost = new URL(request.url).host;
  const publicHost = new URL(publicOrigin(request)).host;
  if (presented !== requestHost && presented !== publicHost) {
    throw uniformNotFound();
  }
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
    // A machine token where only a person may act — typed, so the caller debugs the right
    // thing. RESOLUTION came first: a person's random credential can begin with the token
    // prefix (1 in 64^4), and a prefix alone must never unseat a real session.
    if (credential.startsWith(MACHINE_TOKEN_PREFIX)) {
      throw machineTokenRefused();
    }
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

/**
 * The session lane's front door for routes whose workspace rides in the BODY (the publish
 * family): the SAME resolve as `requireSessionActor`, keyed by the credential alone — the
 * credential is workspace-scoped and hash-unique, so at most one live session answers, and
 * its row names the one workspace it may act in. This lets the route authenticate BEFORE
 * reading the request body, so an unauthenticated caller can never make this tier buffer a
 * publish-sized body. The route MUST then hold the parsed body's workspace against
 * `actor.workspaceId` (fold a mismatch to the uniform 404) — the same answer the
 * workspace-keyed lookup gave a foreign-workspace body. ACTIVE sessions only; every miss is
 * the ONE uniform wire 404.
 */
export async function requireSessionActorPreBody(request: Request): Promise<SessionActor> {
  const credential = bearerToken(request);
  if (credential === null) {
    throw uniformNotFound();
  }
  const row = await sessionActorByCredential(credential);
  if (row === null || row.sessionStatus !== "active") {
    // Resolution before classification — see requireSessionActor.
    if (row === null && credential.startsWith(MACHINE_TOKEN_PREFIX)) {
      throw machineTokenRefused();
    }
    throw uniformNotFound();
  }
  return {
    userId: row.userId,
    display: row.userDisplay,
    workspaceId: row.workspaceId,
    sessionId: row.sessionId,
    role: row.role,
    sessionStatus: row.sessionStatus,
  } as SessionActor;
}

/**
 * The machine-token principal — NOT a person: no user, no seat, no role. Only the read lanes
 * accept it (`requireReadActor`); every session-only guard answers a token with the typed
 * read-only refusal. `machine: true` is the discriminant a route branches on.
 */
export type TokenActor = {
  readonly [actorBrand]: true;
  readonly machine: true;
  readonly workspaceId: string;
  readonly tokenId: string;
  readonly tokenName: string;
  readonly serviceSessionId: string;
};

/** The read lanes' actor: a person's session, or a machine token. */
export type ReadActor = SessionActor | TokenActor;

export function isTokenActor(actor: ReadActor): actor is TokenActor {
  return "machine" in actor && actor.machine === true;
}

/**
 * The read lanes' front door: the session resolve exactly as `requireSessionActor`, plus the
 * machine-token path — a `tpt_…` bearer resolves token → live service session (upserted from
 * the optional `x-topos-machine` reported name), fail-closed to the SAME uniform 404 an
 * unknown session credential gets (no token-liveness oracle). Which lanes call this is the
 * whole authorization surface of a token: catalog and object reads, and the applied report.
 */
export async function requireReadActor(
  request: Request,
  workspaceId: string,
  opts: { allowPending?: boolean } = {},
): Promise<ReadActor> {
  const credential = bearerToken(request);
  if (credential === null) {
    throw uniformNotFound();
  }
  if (credential.startsWith(MACHINE_TOKEN_PREFIX)) {
    const row = await tokenActor(workspaceId, credential, request.headers.get("x-topos-machine"));
    if (row === null) {
      // Not a live token — but a person's random credential can carry the prefix by chance,
      // so resolve it as a session before answering. Every miss stays the uniform 404 (a read
      // lane accepts tokens; the typed read-only refusal would be a lie here).
      const person = await sessionActor(workspaceId, credential);
      if (person === null || (person.sessionStatus !== "active" && opts.allowPending !== true)) {
        throw uniformNotFound();
      }
      return {
        userId: person.userId,
        display: person.userDisplay,
        workspaceId,
        sessionId: person.sessionId,
        role: person.role,
        sessionStatus: person.sessionStatus,
      } as SessionActor;
    }
    return {
      machine: true,
      // The RESOLVED id, never the path's ref — the ref may be the address slug (a CI checkout
      // knows only its committed file's address), and every downstream query keys on the id.
      workspaceId: row.workspaceId,
      tokenId: row.tokenId,
      tokenName: row.tokenName,
      serviceSessionId: row.serviceSessionId,
      // The brand is TYPE-ONLY (`declare const`): minting is this guard's cast, exactly as the
      // session actors are minted.
    } as TokenActor;
  }
  return await requireSessionActor(request, workspaceId, opts);
}
