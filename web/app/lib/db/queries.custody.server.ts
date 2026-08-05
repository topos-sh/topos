import { and, eq, sql } from "drizzle-orm";
import type { MemberActor, SessionActor } from "@/lib/auth/guards.server";
import { auditInTx, mintProposalId } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import { lockMcpNamesInTx } from "@/lib/db/queries.mcp.server";
import {
  bundle,
  channel,
  channelBundle,
  notice,
  opReceipt,
  proposal,
  workspace,
} from "@/lib/db/schema.app";
import { mcpNameTaken, serverDocumentOf } from "@/lib/mcp/catalog.server";
import { type McpGateRefusal, mcpNameTakenRefusal } from "@/lib/mcp/publish-gate.server";
import { custodyVersionMetaFresh } from "@/lib/plane/reads.server";

/**
 * The custody-op ORCHESTRATION's data half — everything the publish/propose/review/revert
 * routes read and write in the app's own rows: the op-receipt idempotency slots, the genesis
 * registration (bundle row + placement + the author's self-follow), the proposal rows, and the
 * verdict notices. The byte half lives in app/lib/plane/custody.server.ts; the routes sequence
 * the two (vault call first, then the final row transaction carrying the receipt).
 */

type Tx = Parameters<Parameters<ReturnType<typeof getDb>["transaction"]>[0]>[0];

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
 * Catalog names reserved for the CLI's own artifacts (the built-in `topos` skill). A reserved
 * name behaves byte-identically to a taken one: the genesis mint suffixes past it.
 */
const RESERVED_BUNDLE_NAMES = new Set(["topos"]);

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
 * birth-only, never rewritten), an EXCLUSIVE placement, and the author's self-follow stance.
 * Placement is exclusive because `--to` is the targeting mechanism: with NO `--to` the bundle
 * lands in the default `everyone` channel; with a `--to` channel named (`everyone` included — no
 * string-match bypass) it lands in THAT channel alone; with `NO_CHANNEL` it lands in none, which
 * is a destination a caller ASKS for and never a fallback this function picks. EVERY placement is
 * gated by the channel's mode (custody is never curation-blocked; REACH is — including the default
 * channel, including genesis): a curated channel withholds a member's placement with
 * `curated_role_required`, riding the receipt details independent of the version gate. A
 * withheld placement leaves the bundle in NO channel (catalog-only) — the disclosure rides the
 * receipt and the author's self-follow still stands.
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

/**
 * THE EMBEDDED-NAME CLAIM A POINTER MOVE MAKES. An `mcp` bundle carries a SECOND name — the
 * registry name inside the version its `current` points at — and that name is unique across the
 * workspace's active catalog, which is what keeps `…/servers/{name}` single-valued. Publishing,
 * re-publishing and unarchiving all re-claim it under one per-workspace advisory lock; MOVING the
 * pointer is the same claim by another route (an approve promotes a candidate that may rename the
 * server; a revert carries an older name forward), so it takes the same lock and asks the same
 * question about the version that is ABOUT to become current.
 *
 * Called inside the transaction that then performs the move: the lock is what makes check-then-
 * move atomic against a publish claiming the name in between, and a transaction-scoped lock needs
 * no release path. The scan rides THIS transaction's client (see `McpDbClient`).
 *
 * `null` is "claim it". Anything else is the refusal to answer with, in the words every other
 * door uses. A version whose document this tier cannot read end to end refuses too: not
 * provably free is not free, the same way an incomplete catalog scan is not.
 */
export async function mcpNameClaimRefusalInTx(
  tx: Tx,
  actor: MemberActor | SessionActor,
  bundleId: string,
  versionId: string,
): Promise<McpGateRefusal | null> {
  await lockMcpNamesInTx(tx, actor.workspaceId);
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
  // On the HELD client — a pool checkout under the lock is the exhaustion shape.
  const taken = await mcpNameTaken(actor, serverName, bundleId, tx);
  return taken.kind === "free" ? null : mcpNameTakenRefusal(serverName, taken);
}
