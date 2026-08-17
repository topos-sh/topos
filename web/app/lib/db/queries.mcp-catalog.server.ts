import { and, asc, desc, eq, inArray, isNull, or, sql } from "drizzle-orm";
import type { MemberActor, OwnerActor, SessionActor } from "@/lib/auth/guards.server";
import {
  auditInTx,
  mintBundleId,
  mintMcpRevisionId,
  mintMcpServerId,
} from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import {
  bundleCapRefusalInTx,
  type GenesisDestination,
  type GenesisRegistration,
  inFinalTxOrRefusal,
  registerGenesisBundleInTx,
} from "@/lib/db/queries.custody.server";
import { bundleMcp, mcpServer, mcpServerRevision } from "@/lib/db/schema.app";
import { canonicalServerJson } from "@/lib/mcp/fetch.server";
import type { McpProbeOutcome } from "@/lib/mcp/probe-state";
import type { McpGateRefusal } from "@/lib/mcp/publish-gate.server";
import { validateServerJson } from "@/lib/mcp/validate.server";

/**
 * THE CATALOG'S DATA LAYER — servers, their revisions, and a workspace's connection to one.
 *
 * Everything here obeys one shape: a REVISION IS NEVER WRITTEN ALONE. Adding one and moving the
 * pointer that makes it the thing people receive happen in a single transaction, under a FOR
 * UPDATE lock on the server row — the fenced-invariant pattern this schema uses wherever two
 * writers could otherwise both believe they won (the last-owner check, the claim-code consume,
 * the login-flow approve). The lock is also what makes `seq` monotonic and what makes the
 * "one document per upstream version" rule a decision rather than a race; the partial unique
 * index behind it is the backstop, not the mechanism.
 *
 * WHO MAY WRITE WHAT is the other shape. Global rows (`workspace_id IS NULL`) are staff's: they
 * are the catalog everyone connects to, so no workspace publishes into them. Private rows are
 * their workspace owner's, edited the same way — each save is a new revision and a pointer move,
 * never an edit of bytes somebody already received. Members connect; they never publish.
 *
 * Refusals are the house shape (`McpGateRefusal`): a code a caller may branch on and a sentence a
 * person reads. Nothing here throws to say no — a refusal is an answer, and an exception inside a
 * caller's transaction would take a whole ceremony down with what is really a routine "no".
 */

type Tx = Parameters<Parameters<ReturnType<typeof getDb>["transaction"]>[0]>[0];

/**
 * The `$schema` values this tier knows how to read a document under.
 *
 * FAIL CLOSED: a document declaring a schema that is not on this list is refused at write, even
 * though it would very likely parse. Understanding a new schema means reading what it changed and
 * teaching the extraction below to honor it — which is CODE, and is why this list lives here
 * rather than in a CHECK constraint a migration could widen without anybody reading anything.
 *
 * A document declaring NO `$schema` is not refused: it is a document that made no claim, which is
 * what a hand-written one and this install's own constructions are.
 */
export const KNOWN_MCP_SCHEMA_VERSIONS: readonly string[] = [
  "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json",
];

/** Where a revision's document came from — the fact the version-uniqueness rule reads. */
export type McpRevisionSource = "registry" | "staff" | "owner" | "seed";

/** A server's sign-in tier, as VERIFIED here — never as a vendor claims it. */
export type McpAuthMode = "none" | "oauth" | "manual";

// ── The facts a document is stored WITH ─────────────────────────────────────────────────────

/** What a stored revision carries beside its document, all of it read out of the document. */
export interface McpRevisionFacts {
  /** The embedded registry name — the server's identity upstream and here. */
  registryName: string;
  /** The `version` inside the document. */
  upstreamVersion: string;
  /** The declared `$schema`, or null when the document declared none. */
  schemaVersion: string | null;
  /** The placed remote's transport, null for a document that offers only packages. */
  transport: string | null;
  /** The placed remote's address, null alongside a null transport. */
  url: string | null;
}

export type McpFactsOutcome =
  | { refusal: McpGateRefusal }
  | { refusal: null; facts: McpRevisionFacts };

/**
 * Read a document's storable facts, refusing anything this tier may not store.
 *
 * The extraction is the DOCUMENT GATE's, not a second parse with its own opinions: the same
 * function every publish door already runs decides what the document says and WHICH remote is the
 * placed one (first `streamable-http` wins — the client's own selection order). A document that
 * cannot be shared at all — no credential, no unpinned package, nothing to run — is refused here
 * for exactly the reason it would be refused anywhere else, in the same words.
 */
export function mcpRevisionFacts(document: Record<string, unknown>): McpFactsOutcome {
  const declared = document.$schema;
  if (declared !== undefined && declared !== null) {
    if (typeof declared !== "string" || !KNOWN_MCP_SCHEMA_VERSIONS.includes(declared)) {
      return {
        refusal: {
          code: "MCP_SCHEMA_UNKNOWN",
          message:
            "this document declares a schema this server does not know how to read — supporting it is a code change, not a setting",
        },
      };
    }
  }
  const validated = validateServerJson(new TextEncoder().encode(canonicalServerJson(document)));
  if (!validated.ok) {
    return { refusal: { code: validated.code, message: validated.message } };
  }
  return {
    refusal: null,
    facts: {
      registryName: validated.summary.name,
      upstreamVersion: validated.summary.version,
      schemaVersion: typeof declared === "string" ? declared : null,
      transport: validated.summary.transport,
      url: validated.summary.url,
    },
  };
}

// ── The one write: a revision, and the pointer that may move with it ────────────────────────

/** What one revision write says. */
export interface McpRevisionWrite {
  document: Record<string, unknown>;
  source: McpRevisionSource;
  /** Publish it and make it the server's `current` in this same transaction. */
  publish: boolean;
  /** Display text recorded on a published revision — the catalog outlives every account. */
  attribution: string;
}

export type McpRevisionOutcome =
  | { refusal: McpGateRefusal }
  | { refusal: null; revisionId: string; seq: number; published: boolean };

const SERVER_NOT_FOUND: McpGateRefusal = {
  code: "MCP_SERVER_NOT_FOUND",
  message: "no such server",
};

interface LockedServer {
  id: string;
  workspaceId: string | null;
  status: string;
  authMode: string | null;
  authNote: string | null;
  currentRevisionId: string | null;
}

/**
 * TAKE THE FENCE. Every write below starts here: the server row is pinned for the rest of the
 * transaction, so the seq mint, the version-uniqueness decision and the pointer move all see one
 * consistent server and a concurrent writer waits rather than interleaves.
 */
async function lockServerInTx(tx: Tx, serverId: string): Promise<LockedServer | undefined> {
  const rows = await tx
    .select({
      id: mcpServer.id,
      workspaceId: mcpServer.workspaceId,
      status: mcpServer.status,
      authMode: mcpServer.authMode,
      authNote: mcpServer.authNote,
      currentRevisionId: mcpServer.currentRevisionId,
    })
    .from(mcpServer)
    .where(eq(mcpServer.id, serverId))
    .limit(1)
    .for("update");
  return rows[0];
}

/**
 * A `manual` server with no note is a chore with no instructions — the one thing this catalog
 * refuses to hand anybody. Checked where it MATTERS (the moment a revision becomes something
 * people receive) rather than as a column constraint: a candidate pulled in by a sweep is allowed
 * to be a half-known thing, which is what `candidate` means.
 */
function authNoteRefusal(
  server: Pick<LockedServer, "authMode" | "authNote">,
): McpGateRefusal | null {
  if (server.authMode !== "manual") {
    return null;
  }
  const note = server.authNote ?? "";
  return note.trim().length > 0
    ? null
    : {
        code: "MCP_AUTH_NOTE_REQUIRED",
        message:
          "a server nobody's agent can sign into by itself may only be published with the one line saying what a person has to do first",
      };
}

/**
 * ADD A REVISION, inside the caller's transaction and behind the server's own lock.
 *
 * Publishing is part of this write, not a step after it: a published revision that is not yet
 * anybody's `current` is a version people cannot receive, and a `current` naming a revision that
 * is not published is the pointer promising something the row does not say. Both are states this
 * function refuses to leave behind, so the two writes commit together or not at all.
 *
 * `source = 'registry'` also carries upstream's promise that a version names one document: a
 * second registry-sourced revision of a version already held is refused here (under the lock, so
 * the check is a decision), and the partial unique index stands behind that decision. A staff
 * correction or an owner's edit is a NEW revision of the same version string by design.
 */
export async function addMcpRevisionInTx(
  tx: Tx,
  serverId: string,
  write: McpRevisionWrite,
): Promise<McpRevisionOutcome> {
  const server = await lockServerInTx(tx, serverId);
  if (server === undefined) {
    return { refusal: SERVER_NOT_FOUND };
  }
  const read = mcpRevisionFacts(write.document);
  if (read.refusal !== null) {
    return { refusal: read.refusal };
  }
  if (write.publish) {
    const chore = authNoteRefusal(server);
    if (chore !== null) {
      return { refusal: chore };
    }
  }
  if (write.source === "registry") {
    const held = await tx
      .select({ id: mcpServerRevision.id })
      .from(mcpServerRevision)
      .where(
        and(
          eq(mcpServerRevision.serverId, serverId),
          eq(mcpServerRevision.upstreamVersion, read.facts.upstreamVersion),
          eq(mcpServerRevision.source, "registry"),
        ),
      )
      .limit(1);
    if (held[0] !== undefined) {
      return {
        refusal: {
          code: "MCP_VERSION_HELD",
          message: `version ${read.facts.upstreamVersion} of this server is already recorded from upstream`,
        },
      };
    }
  }
  const next = await tx
    .select({ seq: sql<number>`coalesce(max(${mcpServerRevision.seq}), 0) + 1`.mapWith(Number) })
    .from(mcpServerRevision)
    .where(eq(mcpServerRevision.serverId, serverId));
  const seq = next[0]?.seq ?? 1;
  const revisionId = mintMcpRevisionId();
  const now = new Date();
  await tx.insert(mcpServerRevision).values({
    id: revisionId,
    serverId,
    seq,
    status: write.publish ? "published" : "candidate",
    upstreamVersion: read.facts.upstreamVersion,
    schemaVersion: read.facts.schemaVersion,
    document: write.document,
    transport: read.facts.transport,
    url: read.facts.url,
    source: write.source,
    ...(write.publish ? { publishedAt: now, publishedBy: write.attribution } : {}),
  });
  if (write.publish) {
    await tx
      .update(mcpServer)
      .set({
        currentRevisionId: revisionId,
        // A server people can now receive is not a candidate any more. A DELISTED one stays
        // delisted: publishing a correction to something deliberately withdrawn does not put it
        // back on offer — relisting is its own act.
        ...(server.status === "candidate" ? { status: "active" as const } : {}),
      })
      .where(eq(mcpServer.id, serverId));
  }
  return { refusal: null, revisionId, seq, published: write.publish };
}

// ── Private servers: an owner's own, created and edited by the same mechanism ────────────────

/** The editorial half of a server row — what a person reading a catalog entry is told. */
export interface McpServerDetails {
  displayName: string;
  description?: string | null;
  websiteUrl?: string | null;
  /** The brand mark this row flies, by key — never an image, never a remote URL. */
  icon?: string | null;
  /** Null says nobody has established it — never spelled `none`, which is a claim. */
  authMode: McpAuthMode | null;
  authNote?: string | null;
  scopeMenu?: unknown;
}

export type McpServerOutcome =
  | { refusal: McpGateRefusal }
  | { refusal: null; serverId: string; revisionId: string };

/**
 * CREATE A PRIVATE SERVER — a workspace's own, visible to nobody else and exported nowhere.
 *
 * The document is stored exactly as a catalog document is, through the same write: the private
 * half of the table is not a second mechanism, it is the same one with a workspace on the row.
 * The first revision is published immediately, because an owner writing down their own server has
 * nobody to be reviewed by.
 */
export async function createPrivateMcpServer(
  actor: OwnerActor,
  details: McpServerDetails,
  document: Record<string, unknown>,
): Promise<McpServerOutcome> {
  const read = mcpRevisionFacts(document);
  if (read.refusal !== null) {
    return { refusal: read.refusal };
  }
  const landed = await inFinalTxOrRefusal<McpServerOutcome, McpServerOutcome>(
    async (tx, refuse) => {
      const serverId = mintMcpServerId();
      await tx.insert(mcpServer).values({
        id: serverId,
        workspaceId: actor.workspaceId,
        registryName: read.facts.registryName,
        displayName: details.displayName,
        description: details.description ?? null,
        websiteUrl: details.websiteUrl ?? null,
        icon: details.icon ?? null,
        authMode: details.authMode,
        authNote: details.authNote ?? null,
        scopeMenu: details.scopeMenu ?? null,
        status: "active",
      });
      const added = await addMcpRevisionInTx(tx, serverId, {
        document,
        source: "owner",
        publish: true,
        attribution: actor.display,
      });
      if (added.refusal !== null) {
        return refuse({ refusal: added.refusal });
      }
      await auditInTx(tx, {
        workspaceId: actor.workspaceId,
        actor: { userId: actor.userId, display: actor.display },
        kind: "mcp_server_created",
        subject: serverId,
        outcome: "ok",
        details: { registryName: read.facts.registryName, name: details.displayName },
      });
      return { refusal: null, serverId, revisionId: added.revisionId };
    },
  );
  return landed.refused !== null ? landed.refused : landed.value;
}

/**
 * EDIT A PRIVATE SERVER — which is to say, publish a new revision of it.
 *
 * Nothing already delivered is rewritten: the old revision stays exactly as it was and the
 * pointer moves to the new one, so a machine that took the previous document can still be told
 * which one it holds. The editorial columns are the row's own and are updated in place — they
 * describe the SERVER, not any one version of its document.
 *
 * A global row refuses here, and so does another workspace's private one: both answer as "no such
 * server", because the answer to "may I edit this" must not double as a way to learn it exists.
 */
export async function editPrivateMcpServer(
  actor: OwnerActor,
  serverId: string,
  details: McpServerDetails,
  document: Record<string, unknown>,
): Promise<McpServerOutcome> {
  const landed = await inFinalTxOrRefusal<McpServerOutcome, McpServerOutcome>(
    async (tx, refuse) => {
      const server = await lockServerInTx(tx, serverId);
      if (server === undefined || server.workspaceId !== actor.workspaceId) {
        return refuse({ refusal: SERVER_NOT_FOUND });
      }
      await tx
        .update(mcpServer)
        .set({
          displayName: details.displayName,
          description: details.description ?? null,
          websiteUrl: details.websiteUrl ?? null,
          icon: details.icon ?? null,
          authMode: details.authMode,
          authNote: details.authNote ?? null,
          scopeMenu: details.scopeMenu ?? null,
        })
        .where(eq(mcpServer.id, serverId));
      const added = await addMcpRevisionInTx(tx, serverId, {
        document,
        source: "owner",
        publish: true,
        attribution: actor.display,
      });
      if (added.refusal !== null) {
        return refuse({ refusal: added.refusal });
      }
      await auditInTx(tx, {
        workspaceId: actor.workspaceId,
        actor: { userId: actor.userId, display: actor.display },
        kind: "mcp_server_edited",
        subject: serverId,
        outcome: "ok",
        details: { revisionId: added.revisionId, name: details.displayName },
      });
      return { refusal: null, serverId, revisionId: added.revisionId };
    },
  );
  return landed.refused !== null ? landed.refused : landed.value;
}

// ── The connection: one workspace's use of one server ────────────────────────────────────────

export interface McpConnectRequest {
  serverId: string;
  /** The name the bundle is born with, folded by the same mint every genesis door uses. */
  displayName: string | null;
  /** Where the connection REACHES — a named channel, the workspace default, or nowhere. */
  to: GenesisDestination;
  /** Follow one revision instead of the server's `current`. */
  pinnedRevisionId?: string | null;
}

export type McpConnectOutcome =
  | { refusal: McpGateRefusal }
  | { refusal: null; registration: GenesisRegistration; serverId: string };

/**
 * CONNECT a server to this workspace: the bundle row people curate and assign, plus the row
 * saying which server it is.
 *
 * The bundle half runs through the ordinary genesis registration — the same catalog-name mint
 * with its suffix-on-collision, the same reserved names, the same mode-gated placement, the same
 * audit line — because a connection IS a new bundle and nothing about it deserves a second
 * implementation of naming.
 *
 * WHAT MAY BE CONNECTED: an `active` server from the global catalog, or this workspace's own
 * private one. A candidate, a delisted row and another workspace's private server all answer
 * "no such server", identically — the catalog states what it offers, and refuses to be an oracle
 * about anything else.
 *
 * ONE CONNECTION PER SERVER PER WORKSPACE, and the unique index is the arbiter rather than a
 * preceding read: two members connecting the same server at once must not both succeed, and the
 * loser's whole transaction rolls back, so the second one leaves no half-born bundle behind.
 */
export async function connectMcpServer(
  actor: MemberActor | SessionActor,
  request: McpConnectRequest,
): Promise<McpConnectOutcome> {
  const landed = await inFinalTxOrRefusal<McpConnectOutcome, McpConnectOutcome>(
    async (tx, refuse) => {
      const rows = await tx
        .select({
          id: mcpServer.id,
          workspaceId: mcpServer.workspaceId,
          currentRevisionId: mcpServer.currentRevisionId,
        })
        .from(mcpServer)
        .where(
          and(
            eq(mcpServer.id, request.serverId),
            eq(mcpServer.status, "active"),
            or(isNull(mcpServer.workspaceId), eq(mcpServer.workspaceId, actor.workspaceId)),
          ),
        )
        .limit(1);
      const server = rows[0];
      if (server === undefined) {
        return refuse({ refusal: SERVER_NOT_FOUND });
      }
      const pinnedRevisionId = request.pinnedRevisionId ?? null;
      if (pinnedRevisionId !== null) {
        const pinned = await tx
          .select({ id: mcpServerRevision.id })
          .from(mcpServerRevision)
          .where(
            and(
              eq(mcpServerRevision.id, pinnedRevisionId),
              eq(mcpServerRevision.serverId, server.id),
              eq(mcpServerRevision.status, "published"),
            ),
          )
          .limit(1);
        if (pinned[0] === undefined) {
          return refuse({
            refusal: {
              code: "MCP_REVISION_NOT_FOUND",
              message: "that version of this server is not one anybody can be pinned to",
            },
          });
        }
      } else if (server.currentRevisionId === null) {
        return refuse({
          refusal: {
            code: "MCP_NOTHING_PUBLISHED",
            message: "this server has no published version yet, so there is nothing to deliver",
          },
        });
      }
      const bundleId = mintBundleId();
      const capped = await bundleCapRefusalInTx(tx, actor, bundleId);
      if (capped !== null) {
        return refuse({ refusal: capped });
      }
      const registration = await registerGenesisBundleInTx(
        tx,
        actor,
        bundleId,
        request.displayName,
        request.to,
        "mcp",
      );
      // The conflict IS the refusal: a second connection to the same server in the same workspace
      // returns nothing here, and the rollback takes the bundle row it would have hung off with it.
      const connected = await tx
        .insert(bundleMcp)
        .values({
          bundleId,
          workspaceId: actor.workspaceId,
          serverId: server.id,
          pinnedRevisionId,
          createdBy: actor.userId,
        })
        .onConflictDoNothing()
        .returning({ bundleId: bundleMcp.bundleId });
      if (connected[0] === undefined) {
        return refuse({
          refusal: {
            code: "MCP_ALREADY_CONNECTED",
            message: "this workspace is already connected to that server",
          },
        });
      }
      await auditInTx(tx, {
        workspaceId: actor.workspaceId,
        actor: {
          userId: actor.userId,
          sessionId: "sessionId" in actor ? actor.sessionId : undefined,
          display: actor.display,
        },
        kind: "mcp_server_connected",
        subject: bundleId,
        outcome: "ok",
        details: {
          serverId: server.id,
          ...(pinnedRevisionId === null ? {} : { pinnedRevisionId }),
        },
      });
      return { refusal: null, registration, serverId: server.id };
    },
  );
  return landed.refused !== null ? landed.refused : landed.value;
}

// ── Delivery: which document a connected bundle actually hands a machine ────────────────────

/** One connected server, resolved to the exact document a machine receives. */
export interface McpDeliveredServer {
  bundleId: string;
  serverId: string;
  revisionId: string;
  /** The connection follows ONE revision rather than the server's `current`. */
  pinned: boolean;
  /** The resolved revision has been pulled back — say so; never move somebody off a pin. */
  revoked: boolean;
  document: Record<string, unknown>;
}

/**
 * THE DELIVERY READ — for a set of connected bundles, the document each one resolves to.
 *
 * THREE WAYS a document is reached, and the row says which:
 *  · the workspace's OWN server — a private row, whose current revision is whatever its owner
 *    last saved;
 *  · a PIN — this connection follows one revision and keeps following it, revoked or not (a pin
 *    is a promise, and quietly moving somebody off one is the opposite of keeping it);
 *  · the catalog's CURRENT — the ordinary case, where corrections arrive as they are published.
 *
 * A bundle whose server has nothing published resolves to nothing and is simply absent: delivery
 * serves what exists, and there is no half-answer to give for a server with no document.
 */
export async function mcpServersForDelivery(
  actor: MemberActor | SessionActor,
  bundleIds: string[],
): Promise<Map<string, McpDeliveredServer>> {
  if (bundleIds.length === 0) {
    return new Map();
  }
  const rows = await getDb()
    .select({
      bundleId: bundleMcp.bundleId,
      serverId: bundleMcp.serverId,
      pinnedRevisionId: bundleMcp.pinnedRevisionId,
      revisionId: mcpServerRevision.id,
      status: mcpServerRevision.status,
      document: mcpServerRevision.document,
    })
    .from(bundleMcp)
    .innerJoin(mcpServer, eq(mcpServer.id, bundleMcp.serverId))
    .innerJoin(
      mcpServerRevision,
      eq(
        mcpServerRevision.id,
        sql`coalesce(${bundleMcp.pinnedRevisionId}, ${mcpServer.currentRevisionId})`,
      ),
    )
    .where(
      and(eq(bundleMcp.workspaceId, actor.workspaceId), inArray(bundleMcp.bundleId, bundleIds)),
    );
  return new Map(
    rows.map((row) => [
      row.bundleId,
      {
        bundleId: row.bundleId,
        serverId: row.serverId,
        revisionId: row.revisionId,
        pinned: row.pinnedRevisionId !== null,
        revoked: row.status === "revoked",
        document: row.document as Record<string, unknown>,
      },
    ]),
  );
}

/** The same resolution for ONE bundle — null when it connects nothing a machine could receive. */
export async function mcpServerForDelivery(
  actor: MemberActor | SessionActor,
  bundleId: string,
): Promise<McpDeliveredServer | null> {
  const resolved = await mcpServersForDelivery(actor, [bundleId]);
  return resolved.get(bundleId) ?? null;
}

// ── Staff decisions on the global catalog ────────────────────────────────────────────────────

/**
 * WHO ACTED, when the act is staff's.
 *
 * NOT a branded actor, and it cannot be: the guards mint proof of a SEAT, and the global catalog
 * has no workspace to hold a seat in. This tier holds no staff identity at all — the back-office
 * that does gates these calls and hands down the attribution, which is recorded as display text
 * on the revision and on a workspace-less audit row (the ledger already carries those: a login
 * denial lands before any workspace is chosen).
 */
export interface McpCatalogStaff {
  readonly display: string;
}

export type McpDecisionOutcome =
  | { refusal: McpGateRefusal }
  | { refusal: null; revisionId: string; serverId: string };

const REVISION_NOT_FOUND: McpGateRefusal = {
  code: "MCP_REVISION_NOT_FOUND",
  message: "no such revision",
};

interface LockedRevision {
  id: string;
  serverId: string;
  status: string;
  seq: number;
}

/** The revision, read under its server's lock — so a decision and a pointer move are one act. */
async function revisionUnderLock(
  tx: Tx,
  revisionId: string,
): Promise<{ revision: LockedRevision; server: LockedServer } | undefined> {
  const rows = await tx
    .select({
      id: mcpServerRevision.id,
      serverId: mcpServerRevision.serverId,
      status: mcpServerRevision.status,
      seq: mcpServerRevision.seq,
    })
    .from(mcpServerRevision)
    .where(eq(mcpServerRevision.id, revisionId))
    .limit(1);
  const revision = rows[0];
  if (revision === undefined) {
    return undefined;
  }
  const server = await lockServerInTx(tx, revision.serverId);
  if (server === undefined) {
    return undefined;
  }
  // Re-read after the lock: a concurrent decision on the same revision waited on that lock, and
  // what it did is what this one must see.
  const fresh = await tx
    .select({
      id: mcpServerRevision.id,
      serverId: mcpServerRevision.serverId,
      status: mcpServerRevision.status,
      seq: mcpServerRevision.seq,
    })
    .from(mcpServerRevision)
    .where(eq(mcpServerRevision.id, revisionId))
    .limit(1);
  return fresh[0] === undefined ? undefined : { revision: fresh[0], server };
}

/** Staff acts on the global catalog only — a workspace's private server is its owner's. */
function globalOnlyRefusal(server: LockedServer): McpGateRefusal | null {
  return server.workspaceId === null ? null : REVISION_NOT_FOUND;
}

async function auditCatalogInTx(
  tx: Tx,
  staff: McpCatalogStaff,
  kind: string,
  subject: string,
  details: Record<string, unknown>,
): Promise<void> {
  await auditInTx(tx, {
    // SERVER-scoped, not workspace-scoped: the catalog belongs to the install, and every
    // workspace-scoped reader queries by equality, so a null row never surfaces in one.
    workspaceId: null,
    actor: { display: staff.display },
    kind,
    subject,
    outcome: "ok",
    details,
  });
}

/**
 * ACCEPT a candidate: it becomes published and the server's `current` in one transaction, and the
 * server itself stops being a candidate. This is the only way a global revision becomes something
 * people receive — there is no workspace review of a catalog server, by design.
 */
export async function acceptMcpRevision(
  staff: McpCatalogStaff,
  revisionId: string,
): Promise<McpDecisionOutcome> {
  const landed = await inFinalTxOrRefusal<McpDecisionOutcome, McpDecisionOutcome>(
    async (tx, refuse) => {
      const found = await revisionUnderLock(tx, revisionId);
      if (found === undefined) {
        return refuse({ refusal: REVISION_NOT_FOUND });
      }
      const foreign = globalOnlyRefusal(found.server);
      if (foreign !== null) {
        return refuse({ refusal: foreign });
      }
      if (found.revision.status !== "candidate") {
        return refuse({
          refusal: {
            code: "MCP_REVISION_DECIDED",
            message: `this revision is already ${found.revision.status}`,
          },
        });
      }
      // A catalog row states how a person gets in. Publishing one where nobody established that
      // yet would put the catalog's name behind a fact it does not have.
      if (found.server.authMode === null) {
        return refuse({
          refusal: {
            code: "MCP_AUTH_MODE_REQUIRED",
            message:
              "this server's sign-in has not been established yet, and the catalog does not publish what it has not checked",
          },
        });
      }
      const chore = authNoteRefusal(found.server);
      if (chore !== null) {
        return refuse({ refusal: chore });
      }
      await tx
        .update(mcpServerRevision)
        .set({ status: "published", publishedAt: new Date(), publishedBy: staff.display })
        .where(eq(mcpServerRevision.id, revisionId));
      await tx
        .update(mcpServer)
        .set({
          currentRevisionId: revisionId,
          ...(found.server.status === "candidate" ? { status: "active" as const } : {}),
        })
        .where(eq(mcpServer.id, found.server.id));
      await auditCatalogInTx(tx, staff, "mcp_revision_published", revisionId, {
        serverId: found.server.id,
        seq: found.revision.seq,
      });
      return { refusal: null, revisionId, serverId: found.server.id };
    },
  );
  return landed.refused !== null ? landed.refused : landed.value;
}

/** REJECT a candidate: the decision is recorded, the row stays, nothing is ever pointed at it. */
export async function rejectMcpRevision(
  staff: McpCatalogStaff,
  revisionId: string,
  reason: string | null = null,
): Promise<McpDecisionOutcome> {
  const landed = await inFinalTxOrRefusal<McpDecisionOutcome, McpDecisionOutcome>(
    async (tx, refuse) => {
      const found = await revisionUnderLock(tx, revisionId);
      if (found === undefined) {
        return refuse({ refusal: REVISION_NOT_FOUND });
      }
      const foreign = globalOnlyRefusal(found.server);
      if (foreign !== null) {
        return refuse({ refusal: foreign });
      }
      if (found.revision.status !== "candidate") {
        return refuse({
          refusal: {
            code: "MCP_REVISION_DECIDED",
            message: `this revision is already ${found.revision.status}`,
          },
        });
      }
      await tx
        .update(mcpServerRevision)
        .set({ status: "rejected", decidedAt: new Date(), decidedBy: staff.display })
        .where(eq(mcpServerRevision.id, revisionId));
      await auditCatalogInTx(tx, staff, "mcp_revision_rejected", revisionId, {
        serverId: found.server.id,
        ...(reason === null ? {} : { reason }),
      });
      return { refusal: null, revisionId, serverId: found.server.id };
    },
  );
  return landed.refused !== null ? landed.refused : landed.value;
}

/**
 * REVOKE a published revision — the pull-back.
 *
 * Two things happen and neither is a deletion. The revision is marked revoked, keeping the
 * `published_at` that says it once was served. And if it was the server's `current`, the pointer
 * falls back to the newest revision still published, or to nothing when none is left: the point
 * of a revocation is that the thing stops being handed out, so leaving the pointer on it would
 * make the act cosmetic.
 *
 * A connection PINNED to the revoked revision is not moved. It asked for exactly that document,
 * and the honest answer is to keep serving it and say it was pulled back.
 */
export async function revokeMcpRevision(
  staff: McpCatalogStaff,
  revisionId: string,
  reason: string | null = null,
): Promise<McpDecisionOutcome> {
  const landed = await inFinalTxOrRefusal<McpDecisionOutcome, McpDecisionOutcome>(
    async (tx, refuse) => {
      const found = await revisionUnderLock(tx, revisionId);
      if (found === undefined) {
        return refuse({ refusal: REVISION_NOT_FOUND });
      }
      const foreign = globalOnlyRefusal(found.server);
      if (foreign !== null) {
        return refuse({ refusal: foreign });
      }
      if (found.revision.status !== "published") {
        return refuse({
          refusal: {
            code: "MCP_REVISION_NOT_PUBLISHED",
            message: "only a published revision can be pulled back",
          },
        });
      }
      await tx
        .update(mcpServerRevision)
        .set({ status: "revoked", revokedAt: new Date() })
        .where(eq(mcpServerRevision.id, revisionId));
      if (found.server.currentRevisionId === revisionId) {
        const fallback = await tx
          .select({ id: mcpServerRevision.id })
          .from(mcpServerRevision)
          .where(
            and(
              eq(mcpServerRevision.serverId, found.server.id),
              eq(mcpServerRevision.status, "published"),
            ),
          )
          .orderBy(desc(mcpServerRevision.seq))
          .limit(1);
        await tx
          .update(mcpServer)
          .set({ currentRevisionId: fallback[0]?.id ?? null })
          .where(eq(mcpServer.id, found.server.id));
      }
      await auditCatalogInTx(tx, staff, "mcp_revision_revoked", revisionId, {
        serverId: found.server.id,
        ...(reason === null ? {} : { reason }),
      });
      return { refusal: null, revisionId, serverId: found.server.id };
    },
  );
  return landed.refused !== null ? landed.refused : landed.value;
}

// ── What the plane saw when it asked ─────────────────────────────────────────────────────────

/** One probe's findings, written onto the revision it was asked about. */
export interface McpRevisionProbeWrite {
  outcome: McpProbeOutcome;
  /** The protocol versions the endpoint answered with — an internal fact, never a surface. */
  protocolVersions?: unknown;
  /** What automatic verification concluded, including the discovery-chain walk behind `oauth`. */
  verification?: unknown;
}

/**
 * Record a probe against one revision. Idempotent by construction — a re-probe REPLACES the older
 * answer rather than accumulating a history nobody reads, because the question is "does this work
 * now" and the row carries its own timestamp.
 *
 * NO ACTOR, deliberately: an audit line records what a PERSON did, and nobody did this. It is
 * this plane's own observation of somebody else's server — the same kind of write as the mail
 * transport's metadata-only send log.
 */
export async function recordMcpRevisionProbe(
  revisionId: string,
  write: McpRevisionProbeWrite,
): Promise<void> {
  await getDb()
    .update(mcpServerRevision)
    .set({
      probeOutcome: write.outcome,
      probedAt: new Date(),
      protocolVersions: write.protocolVersions ?? null,
      ...(write.verification === undefined ? {} : { verification: write.verification }),
    })
    .where(eq(mcpServerRevision.id, revisionId));
}

// ── Reads the surfaces share ─────────────────────────────────────────────────────────────────

/** A catalog row as a surface renders it, with the version it currently offers. */
export interface McpCatalogRow {
  serverId: string;
  registryName: string | null;
  displayName: string;
  description: string | null;
  icon: string | null;
  authMode: string | null;
  authNote: string | null;
  url: string | null;
  transport: string | null;
  revisionId: string;
  upstreamVersion: string;
}

/**
 * WHAT A WORKSPACE MAY CONNECT: the global catalog's active servers plus its own private ones,
 * each with the document its `current` names. The order is the one a person scans — display name,
 * case-insensitively — and it is the same list for every member, because a seat reads the whole
 * catalog.
 */
export async function connectableMcpServers(
  actor: MemberActor | SessionActor,
): Promise<McpCatalogRow[]> {
  const rows = await getDb()
    .select({
      serverId: mcpServer.id,
      registryName: mcpServer.registryName,
      displayName: mcpServer.displayName,
      description: mcpServer.description,
      icon: mcpServer.icon,
      authMode: mcpServer.authMode,
      authNote: mcpServer.authNote,
      url: mcpServerRevision.url,
      transport: mcpServerRevision.transport,
      revisionId: mcpServerRevision.id,
      upstreamVersion: mcpServerRevision.upstreamVersion,
    })
    .from(mcpServer)
    .innerJoin(mcpServerRevision, eq(mcpServerRevision.id, mcpServer.currentRevisionId))
    .where(
      and(
        eq(mcpServer.status, "active"),
        or(isNull(mcpServer.workspaceId), eq(mcpServer.workspaceId, actor.workspaceId)),
      ),
    )
    .orderBy(asc(sql`lower(${mcpServer.displayName})`));
  return rows;
}
