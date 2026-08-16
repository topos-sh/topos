import { and, eq, inArray, sql } from "drizzle-orm";
import { composition } from "@/composition.server";
import type { MemberActor, SessionActor } from "@/lib/auth/guards.server";
import { kindEntry } from "@/lib/bundle-base";
import { claimBundleIdentityInTx } from "@/lib/db/bundle-identity.server";
import { auditInTx, mintProposalId } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import {
  bundle,
  channel,
  channelBundle,
  notice,
  opReceipt,
  proposal,
  workspace,
} from "@/lib/db/schema.app";
import { planeCurrentPointer, planeVersion } from "@/lib/db/schema.custody";
import { serverDocumentOf } from "@/lib/mcp/catalog.server";
import { type McpGateRefusal, mcpNameTakenRefusal } from "@/lib/mcp/publish-gate.server";
import { custodyVersionMetaFresh } from "@/lib/plane/reads.server";

/**
 * The custody-op ORCHESTRATION's data half — everything the publish/propose/review/revert
 * routes read and write in the app's own rows: the op-receipt idempotency slots, the genesis
 * registration (bundle row + placement), the proposal rows, and the
 * verdict notices. The byte half lives in app/lib/plane/custody.server.ts; the routes sequence
 * the two (vault call first, then the final row transaction carrying the receipt).
 */

type Tx = Parameters<Parameters<ReturnType<typeof getDb>["transaction"]>[0]>[0];

/** The transaction handle a final-transaction body is handed — the DAL's own type, exported so
 *  an orchestration layer can name it without importing the raw driver. */
export type FinalTx = Tx;

// ── Op receipts (session-op idempotency) ──────────────────────────────────────────────────────

export type ReceiptLookup =
  | { kind: "miss" }
  | { kind: "replay"; outcome: unknown }
  | { kind: "key_reuse" };

/**
 * The replay probe: same (workspace, session, op_id) + same request bytes replays the stored
 * outcome VERBATIM; the same key with DIFFERENT bytes is a refused key reuse. The request hash
 * is computed IN Postgres (this tier computes no digest).
 */
export async function findReceipt(
  actor: SessionActor,
  opId: string,
  rawBody: string,
): Promise<ReceiptLookup> {
  const rows = await getDb().execute(sql`
    SELECT outcome, (request_sha256 = sha256(convert_to(${rawBody}, 'UTF8'))) AS body_match
    FROM ${opReceipt}
    WHERE workspace_id = ${actor.workspaceId} AND session_id = ${actor.sessionId}
      AND op_id = ${opId}::uuid
  `);
  const row = rows.rows[0] as { outcome: unknown; body_match: boolean } | undefined;
  if (row === undefined) {
    return { kind: "miss" };
  }
  return row.body_match ? { kind: "replay", outcome: row.outcome } : { kind: "key_reuse" };
}

/** Insert the terminal outcome's receipt slot (same-transaction with the op's row writes). */
export async function insertReceiptInTx(
  tx: Tx,
  actor: SessionActor,
  opId: string,
  rawBody: string,
  outcome: unknown,
): Promise<void> {
  await tx.execute(sql`
    INSERT INTO ${opReceipt} (workspace_id, session_id, op_id, request_sha256, outcome)
    VALUES (${actor.workspaceId}, ${actor.sessionId}, ${opId}::uuid,
            sha256(convert_to(${rawBody}, 'UTF8')), ${JSON.stringify(outcome)}::jsonb)
    ON CONFLICT (workspace_id, session_id, op_id) DO NOTHING
  `);
}

// ── The publish gate's reads ─────────────────────────────────────────────────────────────────

export interface PublishTarget {
  bundleId: string;
  name: string;
  status: string;
  /** The catalog kind the bundle was born with — fixed at genesis (a publish naming a
   * different one is refused before any custody write). */
  kind: string;
  /** The RESOLVED protection: the per-bundle pin, else the workspace default. */
  protection: "open" | "reviewed";
}

/** The publish/revert gate's read: the bundle row + the resolved protection cascade. A
 * MemberActor suffices (a SessionActor IS one structurally) — the review pages share it. */
export async function publishTargetOf(
  actor: MemberActor,
  bundleId: string,
): Promise<PublishTarget | undefined> {
  const rows = await getDb()
    .select({
      bundleId: bundle.id,
      name: bundle.name,
      status: bundle.status,
      kind: bundle.kind,
      protection: sql<string>`COALESCE(${bundle.protection}, ${workspace.protectionDefault}, 'open')`,
    })
    .from(bundle)
    .innerJoin(workspace, eq(workspace.id, bundle.workspaceId))
    .where(and(eq(bundle.workspaceId, actor.workspaceId), eq(bundle.id, bundleId)))
    .limit(1);
  const row = rows[0];
  return row === undefined
    ? undefined
    : { ...row, protection: row.protection as PublishTarget["protection"] };
}

/**
 * Whether a bundle id is already registered to a DIFFERENT workspace.
 *
 * A genesis publish keeps the CLIENT-SUPPLIED id, and `bundle.id` is unique across the whole
 * catalog — so a caller who names another workspace's id reaches the genesis arm (their own
 * workspace has no such row) while every id-keyed write below still addresses the foreign row.
 * The upstream row is the sharp one: its arbiter is the bundle id ALONE, so its upsert would
 * rewrite the other workspace's provenance in place. Refusing here — before the vault ingest,
 * so no orphan bytes land either — closes that at the root.
 *
 * A same-workspace id is NOT this: `publishTargetOf` is workspace-scoped and status-agnostic,
 * so an id already in the caller's own catalog never reaches the genesis arm at all, and a
 * retried genesis still converges on its own row.
 */
export async function bundleIdHeldElsewhere(
  actor: MemberActor,
  bundleId: string,
): Promise<boolean> {
  const rows = await getDb()
    .select({ workspaceId: bundle.workspaceId })
    .from(bundle)
    .where(eq(bundle.id, bundleId))
    .limit(1);
  const row = rows[0];
  return row !== undefined && row.workspaceId !== actor.workspaceId;
}

// ── Genesis registration ─────────────────────────────────────────────────────────────────────

/**
 * The birth-name fold (the one implementation, ported from SQL): the display name folded to
 * the agent-skills charset, else the bundle id folded, else the literal 'skill'; capped at 64
 * with no leading/trailing hyphen.
 */
export function mintCatalogName(displayName: string | null, bundleId: string): string {
  const fold = (input: string): string =>
    input
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 64)
      .replace(/^-+|-+$/g, "");
  const fromDisplay = displayName === null ? "" : fold(displayName);
  if (fromDisplay.length > 0) {
    return fromDisplay;
  }
  const fromId = fold(bundleId);
  return fromId.length > 0 ? fromId : "skill";
}

/**
 * Catalog names reserved for the CLI's own artifacts — the directories it owns inside an agent's
 * skills root: `topos` (the built-in skill) and `topos-mcp` (the plugin directory an MCP-capable
 * harness is served through). A workspace bundle that minted either name would compete for a
 * directory the client already governs.
 *
 * The reservation binds at EVERY door that writes a catalog name, not just the genesis mint —
 * a rename reaches the same namespace by a different route. Server-side on purpose: a reserved
 * name answers byte-identically to a taken one everywhere, so the list is never enumerable
 * through a form.
 */
const RESERVED_BUNDLE_NAMES = new Set(["topos", "topos-mcp"]);

/** Is `name` reserved for the client's own directories? (see [`RESERVED_BUNDLE_NAMES`]) */
export function isReservedBundleName(name: string): boolean {
  return RESERVED_BUNDLE_NAMES.has(name);
}

// ── The bundle cap (`bundles`) ──────────────────────────────────────────────────────────────

/** The bundle cap's one refusal — the same shape every genesis door already renders. */
const BUNDLE_LIMIT_REFUSAL: McpGateRefusal = {
  code: "BUNDLE_LIMIT_REACHED",
  message: "This workspace is at its bundle limit.",
};

async function activeBundleCount(executor: Pick<Tx, "execute">, ws: string): Promise<number> {
  const rows = await executor.execute(
    sql`SELECT count(*)::int AS n FROM ${bundle}
        WHERE workspace_id = ${ws} AND status = 'active'`,
  );
  return (rows.rows[0] as { n: number } | undefined)?.n ?? 0;
}

/**
 * The bundle cap (`bundles` — active catalog rows), consulted ONLY when a NEW bundle identity
 * is being created; new versions of existing bundles never come through here. A no-op without
 * a limit (the OSS default). Two spellings of one check:
 *  - `bundleCapRefusal` runs BEFORE the vault call — the cheap read that keeps the common
 *    refusal from leaving ingested bytes behind;
 *  - `bundleCapRefusalInTx` is the AUTHORITY, inside the registration transaction under an
 *    advisory lock, so two concurrent geneses at the boundary serialize instead of both
 *    slipping past the count.
 */
export async function bundleCapRefusal(actor: PublishActor): Promise<McpGateRefusal | null> {
  const entitlements = await composition.entitlements.forWorkspace(actor.workspaceId);
  const limit = entitlements.limit("bundles");
  if (limit === null) {
    return null;
  }
  return (await activeBundleCount(getDb(), actor.workspaceId)) >= limit
    ? BUNDLE_LIMIT_REFUSAL
    : null;
}

export async function bundleCapRefusalInTx(
  tx: Tx,
  actor: PublishActor,
): Promise<McpGateRefusal | null> {
  const entitlements = await composition.entitlements.forWorkspace(actor.workspaceId);
  const limit = entitlements.limit("bundles");
  if (limit === null) {
    return null;
  }
  await tx.execute(sql`SELECT pg_advisory_xact_lock(hashtext(${`bundles:${actor.workspaceId}`}))`);
  return (await activeBundleCount(tx, actor.workspaceId)) >= limit ? BUNDLE_LIMIT_REFUSAL : null;
}

// ── The history window (`history-days`) ─────────────────────────────────────────────────────

/**
 * The workspace's revert-reach window in days, or null when none is set (the OSS default —
 * unlimited). Nothing is ever deleted under a window: rows older than the cutoff stay listed
 * (annotated) and only the REVERT doors refuse them, so a wider window restores access whole.
 */
export async function historyWindowDays(workspaceId: string): Promise<number | null> {
  const entitlements = await composition.entitlements.forWorkspace(workspaceId);
  return entitlements.limit("history-days");
}

/**
 * Whether a revert target sits OUTSIDE the workspace's history window: the version's own
 * recorded creation time (the custody mirror) against now minus the window. No window, or a
 * version the mirror does not hold, is `false` — the revert flow's own refusals (unknown
 * version, purged target) stay the authority on what exists.
 */
export async function versionOutsideHistoryWindow(
  actor: PublishActor,
  bundleId: string,
  versionId: string,
): Promise<boolean> {
  const days = await historyWindowDays(actor.workspaceId);
  if (days === null) {
    return false;
  }
  const rows = await getDb()
    .select({ createdAt: planeVersion.createdAt })
    .from(planeVersion)
    .where(
      and(
        eq(planeVersion.workspaceId, actor.workspaceId),
        eq(planeVersion.bundleId, bundleId),
        eq(planeVersion.versionId, versionId),
      ),
    )
    .limit(1);
  const createdAt = rows[0]?.createdAt;
  if (createdAt === undefined) {
    return false;
  }
  return createdAt.getTime() < Date.now() - days * 24 * 60 * 60 * 1000;
}

/**
 * The creation times of a set of versions (the history page's lock annotation read) — one
 * query over the custody mirror; versions the mirror does not hold are simply absent.
 */
export async function versionCreatedAtMap(
  actor: PublishActor,
  bundleId: string,
  versionIds: string[],
): Promise<Map<string, Date>> {
  if (versionIds.length === 0) {
    return new Map();
  }
  const rows = await getDb()
    .select({ versionId: planeVersion.versionId, createdAt: planeVersion.createdAt })
    .from(planeVersion)
    .where(
      and(
        eq(planeVersion.workspaceId, actor.workspaceId),
        eq(planeVersion.bundleId, bundleId),
        inArray(planeVersion.versionId, versionIds),
      ),
    );
  return new Map(rows.map((r) => [r.versionId, r.createdAt]));
}

export interface GenesisRegistration {
  bundleId: string;
  name: string;
  /** The placement's outcome, when a channel was named — or when the DEFAULT `everyone`
   * placement was withheld by its curated mode (`curated_role_required`). */
  placement?: "placed" | "curated_role_required" | "channel_not_found";
}

/**
 * NO CHANNEL AT ALL — the third thing a genesis destination can say. `toChannel` reads three
 * ways: a channel NAME places into that channel, `null` takes the workspace default, and this
 * places NOWHERE — the bundle lands in the catalog and reaches nobody until someone puts it in a
 * channel later. A symbol rather than a reserved word, so no name typed into a form or sent over
 * a wire can ever spell it.
 */
export const NO_CHANNEL: unique symbol = Symbol("no-channel");

/** Where a genesis publish's REACH goes: a named channel · the default · nowhere. */
export type GenesisDestination = string | null | typeof NO_CHANNEL;

/**
 * Register a NEW bundle at its genesis publish, inside the caller's final transaction: the
 * bundle row (name minted from the display name with suffix-on-collision — `name`, `name-2`,
 * `name-3`…; `kind` is the catalog tag the publish declared, 'skill' when it declared none —
 * birth-only, never rewritten) and an EXCLUSIVE placement. The author gets no standing row of
 * their own: publishing places the bundle, it does not assign it to the publisher.
 * Placement is exclusive because `--to` is the targeting mechanism: with NO `--to` the bundle
 * lands in the default `everyone` channel; with a `--to` channel named (`everyone` included — no
 * string-match bypass) it lands in THAT channel alone; with `NO_CHANNEL` it lands in none, which
 * is a destination a caller ASKS for and never a fallback this function picks. EVERY placement is
 * gated by the channel's mode (custody is never curation-blocked; REACH is — including the default
 * channel, including genesis): a curated channel withholds a member's placement with
 * `curated_role_required`, riding the receipt details independent of the version gate. A
 * withheld placement leaves the bundle in NO channel (catalog-only) — the disclosure rides the
 * receipt, and the bundle is registered either way.
 */
/** The actor shape BOTH publish doors satisfy: the session lane's SessionActor and the web
 * add-from-GitHub page's MemberActor (sessionId rides into audit when present). */
export interface PublishActor {
  readonly userId: string;
  readonly display: string;
  readonly workspaceId: string;
  readonly role: "owner" | "reviewer" | "member";
  readonly sessionId?: string;
}

export async function registerGenesisBundleInTx(
  tx: Tx,
  actor: PublishActor,
  bundleId: string,
  displayName: string | null,
  toChannel: GenesisDestination,
  kind: string | null = null,
): Promise<GenesisRegistration> {
  const ws = actor.workspaceId;
  const base = mintCatalogName(displayName, bundleId);
  let name = base;
  for (let n = 2; ; n++) {
    // `topos` is reserved for the CLI's built-in skill — treated exactly like a taken name
    // (suffix-on-collision, no oracle, no new refusal shape), so no workspace skill can ever
    // shadow the built-in in an agent's skill dirs.
    const reserved = RESERVED_BUNDLE_NAMES.has(name);
    const taken = reserved
      ? []
      : await tx
          .select({ id: bundle.id })
          .from(bundle)
          .where(and(eq(bundle.workspaceId, ws), eq(bundle.name, name)))
          .limit(1);
    if (!reserved && taken.length === 0) {
      break;
    }
    name = `${base.slice(0, 60)}-${n}`;
  }
  // Idempotent on the bundle id: a same-id genesis that races its own retry no-ops the insert
  // (the row already stands with this id; every downstream write below keys on the id, not the
  // name, so they converge). A distinct bundle folding to the SAME catalog name concurrently
  // still trips the name-unique constraint and self-heals on the next retry's suffix pick.
  await tx
    .insert(bundle)
    .values({
      id: bundleId,
      workspaceId: ws,
      name,
      displayName,
      // What this bundle IS, recorded once at birth ('skill' when the publish declares
      // nothing) — the catalog tag every later publish is checked against.
      kind: kind ?? "skill",
      createdBy: actor.userId,
    })
    .onConflictDoNothing({ target: bundle.id });
  // EXCLUSIVE placement: the default `everyone` channel ONLY when no `--to` was named (`--to`
  // targets a subset; adding `everyone` too would deliver to the whole workspace anyway,
  // defeating the targeting). REACH is curation-gated even here: a CURATED default channel
  // withholds a member's placement (`curated_role_required` rides the receipt details — the
  // same outcome a named curated `--to` answers), while the publish itself — custody — still
  // lands. The member never asked for a channel, so the default's refusal never fails the op.
  let placement: GenesisRegistration["placement"];
  if (toChannel === null) {
    // FOR UPDATE — the same pin the named `--to` placement takes: a concurrent open→curated
    // flip of the default channel must conflict here, never slip a member's placement past
    // the curation gate mid-transaction.
    const everyone = await tx
      .select({ id: channel.id, mode: channel.mode })
      .from(channel)
      .where(and(eq(channel.workspaceId, ws), eq(channel.isDefault, true)))
      .limit(1)
      .for("update");
    if (everyone[0] !== undefined) {
      if (everyone[0].mode === "curated" && actor.role === "member") {
        placement = "curated_role_required";
      } else {
        await tx
          .insert(channelBundle)
          .values({ channelId: everyone[0].id, workspaceId: ws, bundleId, addedBy: actor.userId })
          .onConflictDoNothing();
      }
    }
  }
  // A named `--to` — `everyone` included — rides the ONE gated path every channel placement
  // runs (the old `everyone` string-match bypassed the mode gate). `NO_CHANNEL` falls through
  // both arms with nothing written and nothing to report: no reach was asked for, so there is
  // no outcome to disclose.
  if (typeof toChannel === "string") {
    placement = await placeIntoChannelInTx(tx, actor, bundleId, toChannel);
  }
  await auditInTx(tx, {
    workspaceId: ws,
    actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
    kind: "skill_registered",
    subject: bundleId,
    outcome: "ok",
    details: { name },
  });
  return { bundleId, name, ...(placement === undefined ? {} : { placement }) };
}

/** The `--to` placement inside a publish transaction — mode-gated, EXISTING channels only:
 * publish never mints a channel (channel creation is a deliberate curation act, done on the
 * web). The CLI verifies existence before sending; this in-transaction refusal closes the
 * race where the channel is deleted between that check and the write — the publish itself
 * (custody) still lands, and `channel_not_found` rides the receipt details. */
export async function placeIntoChannelInTx(
  tx: Tx,
  actor: PublishActor,
  bundleId: string,
  channelName: string,
): Promise<"placed" | "curated_role_required" | "channel_not_found"> {
  const ws = actor.workspaceId;
  // FOR UPDATE: pin the row for the rest of this transaction. A concurrent DELETE would
  // otherwise abort the final transaction on the FK (after custody already published) instead
  // of answering the typed refusal — and a concurrent mode flip (open → curated) or rename
  // must CONFLICT here too, or a member's placement could slip past the curation gate (or into
  // a channel `--to` no longer names).
  const rows = await tx
    .select({ id: channel.id, mode: channel.mode })
    .from(channel)
    .where(and(eq(channel.workspaceId, ws), eq(channel.name, channelName)))
    .limit(1)
    .for("update");
  const row = rows[0];
  if (row === undefined) {
    return "channel_not_found";
  }
  if (row.mode === "curated" && actor.role === "member") {
    return "curated_role_required";
  }
  await tx
    .insert(channelBundle)
    .values({ channelId: row.id, workspaceId: ws, bundleId, addedBy: actor.userId })
    .onConflictDoNothing();
  return "placed";
}

// ── Proposal rows + verdict notices ─────────────────────────────────────────────────────────

/**
 * Open a proposal row for an ingested candidate. Idempotent per (bundle, candidate): an open
 * row for the same candidate is reused (a lost-ack re-propose converges), a resolved one gets
 * a fresh row.
 */
export async function openProposalInTx(
  tx: Tx,
  actor: SessionActor,
  bundleId: string,
  candidateVersionId: string,
): Promise<{ proposalId: string }> {
  const openFilter = and(
    eq(proposal.workspaceId, actor.workspaceId),
    eq(proposal.bundleId, bundleId),
    eq(proposal.candidateVersionId, candidateVersionId),
    eq(proposal.status, "open"),
  );
  const existing = await tx.select({ id: proposal.id }).from(proposal).where(openFilter).limit(1);
  if (existing[0] !== undefined) {
    return { proposalId: existing[0].id };
  }
  // Insert-then-converge: the partial unique index (one open proposal per candidate) is the race
  // arbiter — a concurrent re-propose that lost the select gets ON CONFLICT DO NOTHING here and
  // re-reads the winner's row, so the inbox never carries two identical open proposals.
  const proposalId = mintProposalId();
  const inserted = await tx
    .insert(proposal)
    .values({
      id: proposalId,
      workspaceId: actor.workspaceId,
      bundleId,
      candidateVersionId,
      proposedBy: actor.userId,
    })
    .onConflictDoNothing()
    .returning({ id: proposal.id });
  if (inserted[0] === undefined) {
    const winner = await tx.select({ id: proposal.id }).from(proposal).where(openFilter).limit(1);
    if (winner[0] !== undefined) {
      return { proposalId: winner[0].id };
    }
    // No open row despite the conflict — the concurrent open resolved between the two reads; a
    // fresh propose is legitimate, so fall through to record this one under a new id.
    await tx.insert(proposal).values({
      id: proposalId,
      workspaceId: actor.workspaceId,
      bundleId,
      candidateVersionId,
      proposedBy: actor.userId,
    });
  }
  await auditInTx(tx, {
    workspaceId: actor.workspaceId,
    actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
    kind: "proposal_opened",
    subject: bundleId,
    outcome: "ok",
    details: { versionId: candidateVersionId },
  });
  return { proposalId };
}

export interface OpenProposalRow {
  id: string;
  bundleId: string;
  candidateVersionId: string;
  proposedBy: string | null;
}

/** Lock ONE open proposal row by candidate — the review transaction's FOR UPDATE fence. */
export async function lockOpenProposalInTx(
  tx: Tx,
  ws: string,
  bundleId: string,
  candidateVersionId: string,
): Promise<OpenProposalRow | undefined> {
  const rows = await tx.execute(sql`
    SELECT id, bundle_id, candidate_version_id, proposed_by
    FROM ${proposal}
    WHERE workspace_id = ${ws} AND bundle_id = ${bundleId}
      AND candidate_version_id = ${candidateVersionId} AND status = 'open'
    FOR UPDATE
  `);
  const row = rows.rows[0] as
    | { id: string; bundle_id: string; candidate_version_id: string; proposed_by: string | null }
    | undefined;
  return row === undefined
    ? undefined
    : {
        id: row.id,
        bundleId: row.bundle_id,
        candidateVersionId: row.candidate_version_id,
        proposedBy: row.proposed_by,
      };
}

/** Resolve a locked proposal row + write the author's verdict notice. */
export async function resolveProposalInTx(
  tx: Tx,
  actor:
    | SessionActor
    | { userId: string; display: string; workspaceId: string; sessionId?: string },
  row: OpenProposalRow,
  verdict: "approved" | "rejected" | "withdrawn",
  reason: string | null,
): Promise<void> {
  await tx
    .update(proposal)
    .set({
      status: verdict,
      resolvedBy: actor.userId,
      resolvedReason: reason,
      resolvedAt: new Date(),
    })
    .where(eq(proposal.id, row.id));
  // Withdraw is the author's own act — no verdict notice for telling yourself.
  if (verdict !== "withdrawn" && row.proposedBy !== null && row.proposedBy !== actor.userId) {
    await tx.insert(notice).values({
      userId: row.proposedBy,
      workspaceId: actor.workspaceId,
      kind: "verdict",
      payload: {
        skill_id: row.bundleId,
        version_id: row.candidateVersionId,
        actor: actor.display,
        outcome: verdict === "approved" ? "approve" : "reject",
        ...(reason === null ? {} : { reason }),
      },
    });
  }
  await auditInTx(tx, {
    workspaceId: actor.workspaceId,
    actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
    kind: `proposal_${verdict}`,
    subject: row.bundleId,
    outcome: "ok",
    details: { versionId: row.candidateVersionId, ...(reason === null ? {} : { reason }) },
  });
}

/** Run one final-transaction body (the routes' row-write + receipt step). */
export async function inFinalTx<T>(fn: (tx: Tx) => Promise<T>): Promise<T> {
  return await getDb().transaction(fn);
}

/** The escape a transaction body throws to roll back while carrying an answer out. */
class TxAbort<A> extends Error {
  constructor(readonly answer: A) {
    super("transaction aborted with an answer");
  }
}

/**
 * A final transaction that can END IN A REFUSAL — rolling everything back and still handing the
 * refusal to the caller.
 *
 * A genesis publish needs this. The bundle row must exist before its identity can be claimed
 * (the claim's foreign key points at it), but the claim can REFUSE — and a refused publish must
 * leave no catalog row behind. Returning the refusal normally would commit the registration it
 * is refusing, so the body throws instead and the rollback is what makes "refused" mean nothing
 * was written. The caller gets the answer either way and never sees the throw.
 */
export async function inFinalTxOrRefusal<T, A>(
  fn: (tx: Tx, refuse: (answer: A) => never) => Promise<T>,
): Promise<{ refused: null; value: T } | { refused: A }> {
  const refuse = (answer: A): never => {
    throw new TxAbort(answer);
  };
  try {
    return { refused: null, value: await getDb().transaction((tx) => fn(tx, refuse)) };
  } catch (error) {
    if (error instanceof TxAbort) {
      return { refused: error.answer as A };
    }
    throw error;
  }
}

/**
 * THE EMBEDDED-NAME CLAIM A POINTER MOVE MAKES. An `mcp` bundle carries a SECOND name — the
 * registry name inside the version its `current` points at — and that name is unique across the
 * workspace's active catalog, which is what keeps `…/servers/{name}` single-valued. Publishing,
 * re-publishing and unarchiving all re-claim it; MOVING the pointer is the same claim by another
 * route (an approve promotes a candidate that may rename the server; a revert carries an older
 * name forward), so it asks the same question about the version that is ABOUT to become current
 * and records the answer in the same row.
 *
 * Called inside the transaction that then performs the move: the claim and the move commit
 * together, so a publish racing this one is refused by the identity key rather than slipped
 * between a check and a write.
 *
 * `null` is "claimed". Anything else is the refusal to answer with, in the words every other
 * door uses. A version whose document this tier cannot read end to end refuses too: not provably
 * free is not free.
 */
async function mcpNameClaimRefusalInTx(
  tx: Tx,
  actor: MemberActor | SessionActor,
  bundleId: string,
  versionId: string,
): Promise<McpGateRefusal | null> {
  // A version the vault does not hold at all is not this check's business — it cannot become
  // `current`, and the move that follows answers it in its own words (unknown / purged). The
  // read is the cache-bypassing one: retention moves even though bytes do not. Every OTHER way
  // of being unreadable IS this check's business and fails closed below.
  const meta = await custodyVersionMetaFresh(actor.workspaceId, bundleId, versionId);
  if (!meta.ok && meta.kind === "not_found") {
    return null;
  }
  const document = await serverDocumentOf(actor.workspaceId, bundleId, versionId);
  const serverName = typeof document?.name === "string" ? document.name : null;
  if (serverName === null) {
    return {
      code: "MCP_NAME_TAKEN",
      message:
        "this version's server.json could not be read here, so the registry name it claims cannot be confirmed free",
    };
  }
  const claimed = await claimBundleIdentityInTx(tx, actor.workspaceId, bundleId, "mcp", serverName);
  return claimed.ok || claimed.heldBy === null
    ? null
    : mcpNameTakenRefusal(serverName, claimed.heldBy);
}

/**
 * THE SAME CLAIM, ASKED OF A BUNDLE'S EXISTING `current` — what a RESTORE needs rather than a
 * move. Archiving freed the bundle's registry name, so unarchiving claims it all over again;
 * there is no incoming version to ask about, so the question is put to whatever the bundle
 * already points at. Runs inside the caller's transaction (it is one step of a larger row
 * ceremony), so the claim commits with the restore or not at all.
 *
 * THREE answers, because a caller deciding whether a name can be claimed must tell them apart:
 *
 *  · `claimed`    — free (or nothing to claim); proceed.
 *  · `no_current` — no `current` pointer at all. Nothing is served for this bundle under any
 *                   registry name, so there is no claim to make and nothing to be ambiguous
 *                   with. Absence of a name, not an unreadable one — and NOT a refusal, or a
 *                   never-published bundle would be stranded in the archive by a name it never
 *                   had.
 *  · `unreadable` — a live `current` whose document this tier cannot read end to end. It proves
 *                   nothing about what would be served, so the caller fails closed on it — and
 *                   says THAT, not that some name is taken.
 *  · `taken`      — another active bundle already holds the name.
 */
export type CurrentNameClaim = "claimed" | "no_current" | "unreadable" | "taken";

export async function claimCurrentNameInTx(
  tx: Tx,
  actor: MemberActor | SessionActor,
  bundleId: string,
): Promise<CurrentNameClaim> {
  const pointer = await tx
    .select({ versionId: planeCurrentPointer.versionId })
    .from(planeCurrentPointer)
    .where(
      and(
        eq(planeCurrentPointer.workspaceId, actor.workspaceId),
        eq(planeCurrentPointer.bundleId, bundleId),
      ),
    )
    .limit(1);
  const versionId = pointer[0]?.versionId;
  if (versionId === undefined) {
    return "no_current";
  }
  const document = await serverDocumentOf(actor.workspaceId, bundleId, versionId);
  if (typeof document?.name !== "string") {
    return "unreadable";
  }
  const claimed = await claimBundleIdentityInTx(
    tx,
    actor.workspaceId,
    bundleId,
    "mcp",
    document.name,
  );
  return claimed.ok ? "claimed" : "taken";
}

/** What a guarded move answered: the mover's own result, or the refusal to say instead. */
export type GuardedMove<T> = { refusal: null; moved: T } | { refusal: McpGateRefusal };

/**
 * MOVE A BUNDLE'S `current`, SUBJECT TO WHATEVER ITS KIND REQUIRES FIRST — the one place that
 * pairing is written.
 *
 * Five doors move a pointer onto a version: the session lane's review-approve and revert, the
 * browser's proposal-review and skill-history revert, and (through its own arm) unarchive. Each
 * of them used to carry its own copy of the same three-line dance, which is three lines each to
 * get subtly wrong and five places to forget when a kind arrives with a precondition of its own.
 * They all call THIS instead, and the precondition is looked up from the bundle's kind record
 * (`hasContentGate`) rather than asked about by name.
 *
 * What the precondition IS, for an MCP server: the version about to become current carries a
 * registry name, that name is the workspace's `…/servers/{name}` key, and it must be no other
 * active bundle's. An approve promotes a candidate that may rename the server; a revert carries
 * an older name forward — both are the same claim by another route.
 *
 * The claim and the move ride ONE transaction, which is what makes check-then-move atomic
 * against a publish claiming the name in between. A kind with no precondition moves the pointer
 * with no transaction at all, exactly as it did before.
 *
 * A MOVE THAT DID NOT LAND TAKES THE CLAIM DOWN WITH IT. The custody call answers routine
 * failures with a TYPED outcome rather than a throw — a generation conflict above all, which is
 * what a reviewer meets when someone published while they were deciding. Committing on that path
 * would record a claim for a pointer that never moved: the bundle's OLD name released while it is
 * still being served, and the freed name available to the next publish while the registry lane
 * still answers to it. So anything but `ok` rolls the transaction back and is handed to the
 * caller unchanged — the caller still branches on exactly the outcome custody gave it.
 */
export async function movePointerWithKindPrecondition<T extends { kind: string }>(args: {
  /** The bundle's catalog kind — the authority on what must be checked first. */
  kind: string;
  actor: MemberActor | SessionActor;
  bundleId: string;
  /** The version about to become `current` — what the precondition is asked about. */
  versionId: string;
  /** The custody call that actually moves the pointer. */
  move: () => Promise<T>;
}): Promise<GuardedMove<T>> {
  if (!kindEntry(args.kind).hasContentGate) {
    return { refusal: null, moved: await args.move() };
  }
  const landed = await inFinalTxOrRefusal<GuardedMove<T>, GuardedMove<T>>(async (tx, abort) => {
    const refusal = await mcpNameClaimRefusalInTx(tx, args.actor, args.bundleId, args.versionId);
    if (refusal !== null) {
      abort({ refusal });
    }
    const moved = await args.move();
    if (moved.kind !== "ok") {
      // Nothing moved, so nothing may be claimed — but this is an ANSWER, not a fault, and it
      // travels out intact.
      abort({ refusal: null, moved });
    }
    return { refusal: null, moved };
  });
  return landed.refused !== null ? landed.refused : landed.value;
}
