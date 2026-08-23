import { and, eq, isNotNull, isNull, or, sql } from "drizzle-orm";
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
import type { McpProbeOutcome, McpProbeRecord } from "@/lib/mcp/probe-state";
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
 * WHO MAY WRITE WHAT is the other shape. Public rows (`workspace_id IS NULL`) are the generic
 * catalog: the boot sync reconciles the committed file into them, and staff may add or curate
 * them — no workspace publishes into them. Private rows are their workspace owner's, edited the
 * same way — each save is a new revision and a pointer move, never an edit of bytes somebody
 * already received. Members connect; they never publish.
 *
 * PROMOTION — moving `current` — is written ONCE, in `promoteMcpRevisionInTx`. Every path that
 * makes a revision the thing people receive (an owner's save, the file sync's auto-advance, a
 * staff promote) goes through it, so the compatibility gate a later increment adds has one seam
 * to sit behind rather than three.
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
 *
 * THE SCHEMA IS A METADATA FORMAT, NOT A FRESHNESS SIGNAL. Every one of these revisions is read
 * against the extraction below and changes nothing it reads: the same `name`, `description`,
 * `version`, remotes and packages, at the same paths. An older schema on this list is not an older
 * or a worse document — a hand-authored or a self-maintained document may declare any of them. It
 * is an explicit allowlist of the revisions this build knows how to read, not "any official host".
 */
export const KNOWN_MCP_SCHEMA_VERSIONS: readonly string[] = [
  "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json",
  "https://static.modelcontextprotocol.io/schemas/2025-10-17/server.schema.json",
  "https://static.modelcontextprotocol.io/schemas/2025-09-29/server.schema.json",
  "https://static.modelcontextprotocol.io/schemas/2025-09-16/server.schema.json",
];

/** A server's sign-in tier, as VERIFIED here — never as a vendor claims it. */
export type McpAuthMode = "none" | "oauth" | "manual";

// ── The facts a document is stored WITH ─────────────────────────────────────────────────────

/** What a stored revision carries beside its document, all of it read out of the document. */
export interface McpRevisionFacts {
  /** The embedded reverse-DNS name — the server's identity. */
  name: string;
  /** The `version` inside the document, or NULL when it names none (an editorial/self-maintained
   *  server we never stamped a fake version on). Registry documents always carry a real one. */
  upstreamVersion: string | null;
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
 *
 * `requireVersion` carries the ONE rule that differs by provenance: a REGISTRY document must name a
 * version (the official registry mandates it, and we ingest what it gives), while an editorial or
 * self-maintained document may carry none — in which case `upstreamVersion` comes back NULL, and
 * we never fabricate a number to fill it. A document that DECLARES one of the known official
 * `$schema`s still needs a version whatever its provenance: every one of those schemas requires the
 * field, so a document claiming a schema while omitting it is self-contradictory, and we would be
 * storing (and serving verbatim) something no schema-aware reader would accept. The honest shape of
 * a version-less server is to make NO schema claim — exactly what a hand-written document does.
 */
export function mcpRevisionFacts(
  document: Record<string, unknown>,
  options: { requireVersion: boolean },
): McpFactsOutcome {
  const declared = document.$schema;
  const declaresKnownSchema = typeof declared === "string";
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
  const validated = validateServerJson(new TextEncoder().encode(canonicalServerJson(document)), {
    requireVersion: options.requireVersion || declaresKnownSchema,
  });
  if (!validated.ok) {
    return { refusal: { code: validated.code, message: validated.message } };
  }
  return {
    refusal: null,
    facts: {
      name: validated.summary.name,
      upstreamVersion: validated.summary.version,
      schemaVersion: typeof declared === "string" ? declared : null,
      transport: validated.summary.transport,
      url: validated.summary.url,
    },
  };
}

// ── The two writes: append a revision, and (separately) promote one to current ──────────────

/** What one revision write says. */
export interface McpRevisionWrite {
  document: Record<string, unknown>;
}

export type McpRevisionOutcome =
  | { refusal: McpGateRefusal }
  | { refusal: null; revisionId: string; seq: number };

const SERVER_NOT_FOUND: McpGateRefusal = {
  code: "MCP_SERVER_NOT_FOUND",
  message: "no such server",
};

export interface LockedServer {
  id: string;
  workspaceId: string | null;
  name: string | null;
  status: string;
  authMode: string | null;
  authNote: string | null;
  manuallyCurated: boolean;
  currentRevisionId: string | null;
  /** Bumped on every row write (`$onUpdate`) — the optimistic-concurrency stamp a staff edit
   *  carries back so a save built on a stale view (any edit OR a promote since) refuses. */
  updatedAt: Date;
}

/**
 * TAKE THE FENCE. Every write below starts here: the server row is pinned for the rest of the
 * transaction, so the seq mint and the pointer move see one consistent server and a concurrent
 * writer waits rather than interleaves.
 */
export async function lockServerInTx(tx: Tx, serverId: string): Promise<LockedServer | undefined> {
  const rows = await tx
    .select({
      id: mcpServer.id,
      workspaceId: mcpServer.workspaceId,
      name: mcpServer.name,
      status: mcpServer.status,
      authMode: mcpServer.authMode,
      authNote: mcpServer.authNote,
      manuallyCurated: mcpServer.manuallyCurated,
      currentRevisionId: mcpServer.currentRevisionId,
      updatedAt: mcpServer.updatedAt,
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
 * people receive, i.e. promotion) rather than as a column constraint.
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
 * ADD A REVISION — append one, non-current, inside the caller's transaction behind the server's
 * lock. It never moves the pointer: promotion is a separate, single decision
 * (`promoteMcpRevisionInTx`), because a file version is a proposal until something says otherwise.
 *
 * The document's name must match the server's — a revision is a version OF THIS server, and one
 * that renamed it would leave the catalog keyed by one name while handing out another.
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
  // We author every catalog document ourselves now (the file, an owner, a staff edit), so a
  // missing version is honest rather than a shortfall — never fabricated.
  const read = mcpRevisionFacts(write.document, { requireVersion: false });
  if (read.refusal !== null) {
    return { refusal: read.refusal };
  }
  if (server.name !== null && server.name !== read.facts.name) {
    return {
      refusal: {
        code: "MCP_NAME_MISMATCH",
        message: `this server is ${server.name}; a version of it cannot call itself ${read.facts.name}`,
      },
    };
  }
  // ONE DOCUMENT PER VERSION, when a document names one — the clean refusal the partial unique
  // index stands behind. A versionless document is exempt (null is distinct), which is why the
  // file's versionless revisions append freely.
  const version = read.facts.upstreamVersion;
  if (version !== null) {
    const held = await tx
      .select({ id: mcpServerRevision.id })
      .from(mcpServerRevision)
      .where(
        and(
          eq(mcpServerRevision.serverId, serverId),
          eq(mcpServerRevision.upstreamVersion, version),
        ),
      )
      .limit(1);
    if (held[0] !== undefined) {
      return {
        refusal: {
          code: "MCP_VERSION_HELD",
          message: `version ${version} of this server is already recorded`,
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
  await tx.insert(mcpServerRevision).values({
    id: revisionId,
    serverId,
    seq,
    upstreamVersion: read.facts.upstreamVersion,
    schemaVersion: read.facts.schemaVersion,
    document: write.document,
    transport: read.facts.transport,
    url: read.facts.url,
  });
  return { refusal: null, revisionId, seq };
}

/**
 * PROMOTE ONE REVISION TO `current` — the ONE place the pointer moves, whichever path asked (an
 * owner's save, the file sync's auto-advance, a staff promote). Routing every promotion here is
 * what lets a later increment add a gateway-compatibility gate in one seam: a version the client
 * cannot place would be held as a proposal instead of promoted, so a live workspace is never
 * broken by an incompatible update. THE GATE IS NOT BUILT YET (there is no gateway) — this is the
 * seam it will sit in.
 *
 * The caller holds the server's lock (from `lockServerInTx` / `addMcpRevisionInTx`) and passes the
 * `server` it read under it. A `manual` server still needs its one line before anyone receives it.
 * The `published_at`/`published_by` stamp records WHEN this revision last became current and by
 * whom — history, not a status.
 */
export async function promoteMcpRevisionInTx(
  tx: Tx,
  server: LockedServer,
  revisionId: string,
  by: string,
): Promise<McpGateRefusal | null> {
  const chore = authNoteRefusal(server);
  if (chore !== null) {
    return chore;
  }
  await tx
    .update(mcpServerRevision)
    .set({ publishedAt: new Date(), publishedBy: by })
    .where(and(eq(mcpServerRevision.id, revisionId), eq(mcpServerRevision.serverId, server.id)));
  await tx
    .update(mcpServer)
    .set({ currentRevisionId: revisionId })
    .where(eq(mcpServer.id, server.id));
  return null;
}

// ── Private servers: an owner's own, created and edited by the same mechanism ────────────────

/** The editorial half of a server row — what a person reading a catalog entry is told. */
/**
 * WHO MAY WRITE A WORKSPACE'S OWN SERVER DOWN: an owner, whichever surface they came through.
 * The page's cookie-side `OwnerActor` and the session lane's bearer-side actor are the same seat
 * read two ways, and the type says so rather than making the lane widen the parameter to any
 * member and prove the role in prose.
 */
export type McpServerAuthor = OwnerActor | (SessionActor & { readonly role: "owner" });

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
 * The document is stored exactly as a catalog document is, through the same write. The first
 * revision is promoted immediately, because an owner writing down their own server has nobody to
 * be reviewed by.
 */
export async function createPrivateMcpServer(
  actor: McpServerAuthor,
  details: McpServerDetails,
  document: Record<string, unknown>,
): Promise<McpServerOutcome> {
  // A workspace's OWN server is ours to author — its document may name no version, and we store
  // that truthfully rather than stamp a placeholder on it.
  const read = mcpRevisionFacts(document, { requireVersion: false });
  if (read.refusal !== null) {
    return { refusal: read.refusal };
  }
  const landed = await inFinalTxOrRefusal<McpServerOutcome, McpServerOutcome>(
    async (tx, refuse) => {
      const serverId = mintMcpServerId();
      // ONE NAME, ONE SERVER, inside this workspace: two private rows under one name would make a
      // lookup a coin flip. The partial unique index is the arbiter — the conflict IS the refusal,
      // so two owners doing this at once cannot both win, and the second gets an answer.
      const born = await tx
        .insert(mcpServer)
        .values({
          id: serverId,
          workspaceId: actor.workspaceId,
          name: read.facts.name,
          displayName: details.displayName,
          description: details.description ?? null,
          websiteUrl: details.websiteUrl ?? null,
          icon: details.icon ?? null,
          authMode: details.authMode,
          authNote: details.authNote ?? null,
          scopeMenu: details.scopeMenu ?? null,
          status: "active",
        })
        .onConflictDoNothing()
        .returning({ id: mcpServer.id });
      if (born[0] === undefined) {
        return refuse({
          refusal: {
            code: "MCP_NAME_TAKEN",
            message: `this workspace already has a server called ${read.facts.name}`,
          },
        });
      }
      const added = await addMcpRevisionInTx(tx, serverId, { document });
      if (added.refusal !== null) {
        return refuse({ refusal: added.refusal });
      }
      // The lock the add took is this transaction's; re-read the server under it for promotion.
      const server = await lockServerInTx(tx, serverId);
      if (server === undefined) {
        return refuse({ refusal: SERVER_NOT_FOUND });
      }
      const promoted = await promoteMcpRevisionInTx(tx, server, added.revisionId, actor.display);
      if (promoted !== null) {
        return refuse({ refusal: promoted });
      }
      await auditInTx(tx, {
        workspaceId: actor.workspaceId,
        actor: { userId: actor.userId, display: actor.display },
        kind: "mcp_server_created",
        subject: serverId,
        outcome: "ok",
        details: { name: read.facts.name, displayName: details.displayName },
      });
      return { refusal: null, serverId, revisionId: added.revisionId };
    },
  );
  return landed.refused !== null ? landed.refused : landed.value;
}

/**
 * EDIT A PRIVATE SERVER — which is to say, promote a new revision of it.
 *
 * Nothing already delivered is rewritten: the old revision stays exactly as it was and the
 * pointer moves to the new one, so a machine that took the previous document can still be told
 * which one it holds. The editorial columns are the row's own and are updated in place — they
 * describe the SERVER, not any one version of its document.
 *
 * A public row refuses here, and so does another workspace's private one: both answer as "no such
 * server", because the answer to "may I edit this" must not double as a way to learn it exists.
 */
export async function editPrivateMcpServer(
  actor: McpServerAuthor,
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
      const added = await addMcpRevisionInTx(tx, serverId, { document });
      if (added.refusal !== null) {
        return refuse({ refusal: added.refusal });
      }
      // Promote against the server as edited — the auth note just written is what the manual-note
      // gate must see, so re-read it under the lock this transaction already holds.
      const edited = await lockServerInTx(tx, serverId);
      if (edited === undefined) {
        return refuse({ refusal: SERVER_NOT_FOUND });
      }
      const promoted = await promoteMcpRevisionInTx(tx, edited, added.revisionId, actor.display);
      if (promoted !== null) {
        return refuse({ refusal: promoted });
      }
      await auditInTx(tx, {
        workspaceId: actor.workspaceId,
        actor: { userId: actor.userId, display: actor.display },
        kind: "mcp_server_edited",
        subject: serverId,
        outcome: "ok",
        details: { revisionId: added.revisionId, displayName: details.displayName },
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
        // A pin names a revision OF THIS SERVER that HAS BEEN ON OFFER — it carries the promotion
        // stamp (the current, or one that was current before) and is not dismissed. It need not be
        // the current one, but it may never be a file proposal nobody promoted: pinning to one
        // would deliver a document the catalog never put on offer, bypassing curation.
        const pinned = await tx
          .select({ id: mcpServerRevision.id })
          .from(mcpServerRevision)
          .where(
            and(
              eq(mcpServerRevision.id, pinnedRevisionId),
              eq(mcpServerRevision.serverId, server.id),
              isNotNull(mcpServerRevision.publishedAt),
              isNull(mcpServerRevision.dismissedAt),
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
            message: "this server has no current version yet, so there is nothing to deliver",
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

/**
 * THE RESOLUTION, WRITTEN ONCE — a connection's pin exactly, else the server's `current`.
 *
 * THREE WAYS a document is reached, and one expression decides between them:
 *  · the workspace's OWN server — a private row, whose current revision is whatever its owner
 *    last saved;
 *  · a PIN — this connection follows one revision and keeps following it, revoked or not (a pin
 *    is a promise, and quietly moving somebody off one is the opposite of keeping it);
 *  · the catalog's CURRENT — the ordinary case, where corrections arrive as they are published.
 *
 * Every read that has to answer "which revision does this workspace receive" spells THIS, which
 * is why they all alias the connection row `bm` and the server row `ms`. A second spelling
 * somewhere would be a second answer, and delivery, the catalog index, the registry lane and the
 * server's own page would drift apart one query at a time.
 */
export const MCP_RESOLVED_REVISION = sql`COALESCE(bm.pinned_revision_id, ms.current_revision_id)`;

// ── Staff decisions on the public catalog (promote a proposal, dismiss one) ──────────────────

/**
 * WHO ACTED, when the act is staff's.
 *
 * NOT a branded actor, and it cannot be: the guards mint proof of a SEAT, and the public catalog
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
  seq: number;
  dismissedAt: Date | null;
}

/** The revision, read under its server's lock — so a decision and a pointer move are one act. */
async function revisionUnderLock(
  tx: Tx,
  revisionId: string,
): Promise<{ revision: LockedRevision; server: LockedServer } | undefined> {
  const first = await tx
    .select({ serverId: mcpServerRevision.serverId })
    .from(mcpServerRevision)
    .where(eq(mcpServerRevision.id, revisionId))
    .limit(1);
  if (first[0] === undefined) {
    return undefined;
  }
  const server = await lockServerInTx(tx, first[0].serverId);
  if (server === undefined) {
    return undefined;
  }
  // Re-read after the lock: a concurrent decision on the same revision waited on that lock, and
  // what it did is what this one must see.
  const fresh = await tx
    .select({
      id: mcpServerRevision.id,
      serverId: mcpServerRevision.serverId,
      seq: mcpServerRevision.seq,
      dismissedAt: mcpServerRevision.dismissedAt,
    })
    .from(mcpServerRevision)
    .where(eq(mcpServerRevision.id, revisionId))
    .limit(1);
  return fresh[0] === undefined ? undefined : { revision: fresh[0], server };
}

/** Staff acts on the public catalog only — a workspace's private server is its owner's. */
function publicOnlyRefusal(server: LockedServer): McpGateRefusal | null {
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
 * MARK A PUBLIC SERVER MANUALLY CURATED — a staff member has taken it in hand, so it stops
 * tracking the file automatically and future file versions land as proposals. Called by every
 * staff act that touches a public row (an edit, a promote, a dismiss).
 */
async function markManuallyCuratedInTx(tx: Tx, serverId: string): Promise<void> {
  await tx.update(mcpServer).set({ manuallyCurated: true }).where(eq(mcpServer.id, serverId));
}

/**
 * PROMOTE a proposal to `current` — a staff act, and the moment a public server becomes manually
 * curated (it now tracks staff's hand, not the file's). The pointer move itself goes through the
 * ONE promotion function, so the gateway gate a later increment adds covers a staff promote too.
 */
export async function promoteMcpRevision(
  staff: McpCatalogStaff,
  revisionId: string,
): Promise<McpDecisionOutcome> {
  const landed = await inFinalTxOrRefusal<McpDecisionOutcome, McpDecisionOutcome>(
    async (tx, refuse) => {
      const found = await revisionUnderLock(tx, revisionId);
      if (found === undefined) {
        return refuse({ refusal: REVISION_NOT_FOUND });
      }
      const foreign = publicOnlyRefusal(found.server);
      if (foreign !== null) {
        return refuse({ refusal: foreign });
      }
      if (found.revision.dismissedAt !== null) {
        return refuse({
          refusal: {
            code: "MCP_REVISION_DISMISSED",
            message: "this proposal was dismissed, so it cannot be promoted",
          },
        });
      }
      if (found.server.currentRevisionId === revisionId) {
        return refuse({
          refusal: {
            code: "MCP_REVISION_CURRENT",
            message: "this revision is already the one on offer",
          },
        });
      }
      // ONLY FORWARD. A pending proposal is a revision NEWER than the current; a stale panel (the
      // pointer advanced since it rendered — another operator promoted a later file version) must
      // not roll the global offer back onto history and change every unpinned workspace's document.
      // Re-read the current's seq under this lock and refuse anything not above it.
      if (found.server.currentRevisionId !== null) {
        const cur = await tx
          .select({ seq: mcpServerRevision.seq })
          .from(mcpServerRevision)
          .where(eq(mcpServerRevision.id, found.server.currentRevisionId))
          .limit(1);
        const currentSeq = cur[0]?.seq;
        if (currentSeq !== undefined && found.revision.seq <= currentSeq) {
          return refuse({
            refusal: {
              code: "MCP_REVISION_NOT_PROPOSAL",
              message:
                "this revision is not newer than the one on offer, so it is no longer a pending proposal",
            },
          });
        }
      }
      // A catalog row states how a person gets in. Promoting one where nobody established that yet
      // would put the catalog's name behind a fact it does not have.
      if (found.server.authMode === null) {
        return refuse({
          refusal: {
            code: "MCP_AUTH_MODE_REQUIRED",
            message:
              "this server's sign-in has not been established yet, and the catalog does not offer what it has not checked",
          },
        });
      }
      // THE VERIFIED TIER IS THE ROW's (`auth_mode`), never a document's word. A revision delivers
      // its own tier in `sh.topos/auth`; promoting one whose delivered tier DIFFERS from the
      // established one would split what surfaces show (the column) from what a machine reads (the
      // document). Rather than let a document overwrite verified truth, refuse and send staff to
      // EDIT — which re-authors the document at the verified tier — to reconcile a genuine change.
      const promotedRows = await tx
        .select({ document: mcpServerRevision.document })
        .from(mcpServerRevision)
        .where(eq(mcpServerRevision.id, revisionId))
        .limit(1);
      // The delivered tier must MATCH the verified one exactly — a document that names a DIFFERENT
      // tier would overwrite verified truth, and one that names NONE (null) would deliver a machine
      // no tier at all (delivery hands over only the document, and the client reads its `sh.topos/auth`).
      // Either way, refuse and send staff to EDIT, which re-authors the document at the verified tier.
      const docTier = deliveredAuthOf(promotedRows[0]?.document);
      if (docTier !== found.server.authMode) {
        return refuse({
          refusal: {
            code: "MCP_AUTH_TIER_MISMATCH",
            message:
              "this version does not deliver the sign-in tier established for this server — edit the server to reconcile it before promoting",
          },
        });
      }
      const promoted = await promoteMcpRevisionInTx(tx, found.server, revisionId, staff.display);
      if (promoted !== null) {
        return refuse({ refusal: promoted });
      }
      await markManuallyCuratedInTx(tx, found.server.id);
      // A staff promote OFFERS this version, so a row a migration left `delisted` (a former,
      // never-accepted candidate) is relisted as part of the act — otherwise the catalog would
      // report it offered while workspace queries still exclude it.
      if (found.server.status !== "active") {
        await tx
          .update(mcpServer)
          .set({ status: "active" })
          .where(eq(mcpServer.id, found.server.id));
      }
      await auditCatalogInTx(tx, staff, "mcp_revision_promoted", revisionId, {
        serverId: found.server.id,
        seq: found.revision.seq,
      });
      return { refusal: null, revisionId, serverId: found.server.id };
    },
  );
  return landed.refused !== null ? landed.refused : landed.value;
}

/**
 * DISMISS a proposal — a terminal decline. The revision stays as history, but `dismissed_at` marks
 * it so it stops showing as pending and the file re-offering the same document never re-proposes
 * it. The current revision is never touched (you cannot dismiss what is on offer). Dismissing also
 * makes the server manually curated: a staff member has ruled on it.
 */
export async function dismissMcpRevision(
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
      const foreign = publicOnlyRefusal(found.server);
      if (foreign !== null) {
        return refuse({ refusal: foreign });
      }
      if (found.server.currentRevisionId === revisionId) {
        return refuse({
          refusal: {
            code: "MCP_REVISION_CURRENT",
            message: "the version on offer cannot be dismissed — promote another first",
          },
        });
      }
      if (found.revision.dismissedAt !== null) {
        return refuse({
          refusal: {
            code: "MCP_REVISION_DISMISSED",
            message: "this proposal is already dismissed",
          },
        });
      }
      // ONLY A PENDING PROPOSAL. Dismiss marks a revision terminal so it never re-offers or is
      // pinnable; a stale page could otherwise dismiss a revision that has since been superseded and
      // is now HISTORY (seq below the current), quietly burying a version that was once on offer.
      // A pending proposal is newer than the current — re-read the current's seq under this lock.
      if (found.server.currentRevisionId !== null) {
        const cur = await tx
          .select({ seq: mcpServerRevision.seq })
          .from(mcpServerRevision)
          .where(eq(mcpServerRevision.id, found.server.currentRevisionId))
          .limit(1);
        const currentSeq = cur[0]?.seq;
        if (currentSeq !== undefined && found.revision.seq <= currentSeq) {
          return refuse({
            refusal: {
              code: "MCP_REVISION_NOT_PROPOSAL",
              message:
                "this revision is history, not a pending proposal, so it cannot be dismissed",
            },
          });
        }
      }
      await tx
        .update(mcpServerRevision)
        .set({ dismissedAt: new Date(), dismissedBy: staff.display })
        .where(eq(mcpServerRevision.id, revisionId));
      await markManuallyCuratedInTx(tx, found.server.id);
      await auditCatalogInTx(tx, staff, "mcp_revision_dismissed", revisionId, {
        serverId: found.server.id,
        ...(reason === null ? {} : { reason }),
      });
      return { refusal: null, revisionId, serverId: found.server.id };
    },
  );
  return landed.refused !== null ? landed.refused : landed.value;
}

// ── Staff acts on the PUBLIC catalog: list it, add a custom server, edit one ─────────────────

/**
 * A pending file PROPOSAL on a public server — a non-current, non-dismissed revision newer than
 * the current, awaiting a staff decision. When a manually-curated server has fallen several file
 * versions behind, this is the NEWEST such (highest seq): promoting it supersedes the rest.
 */
export interface McpPublicProposal {
  revisionId: string;
  seq: number;
  upstreamVersion: string | null;
  createdAt: Date;
}

/** One public catalog server as the staff panel lists it: what it offers now, and any pending
 *  proposal the file has filed against it. */
export interface McpPublicServerRow {
  serverId: string;
  name: string | null;
  displayName: string;
  description: string | null;
  /** The brand-mark key the row flies — carried so an edit preserves it rather than clearing it. */
  icon: string | null;
  status: string;
  authMode: string | null;
  authNote: string | null;
  scopeMenu: unknown;
  /** A staff edit or promote has taken this server in hand, so the file proposes rather than
   *  advances. */
  manuallyCurated: boolean;
  currentRevisionId: string | null;
  /** The version the current revision offers, or NULL when it names none / there is no current. */
  currentVersion: string | null;
  /** The row's `updated_at` in epoch millis — the optimistic-concurrency stamp an edit carries back. */
  updatedAt: number;
  /** How many workspaces connect this server. */
  connections: number;
  /** The newest pending file proposal, or null when the server is settled. */
  proposal: McpPublicProposal | null;
}

/**
 * THE PUBLIC CATALOG, for the operator: every public server, its current version, and its newest
 * pending proposal, display-name ordered. A private server is its workspace owner's and never here.
 * The proposal is derived exactly as a surface derives one — a non-current, non-dismissed revision
 * whose seq is above the current's — so the panel and the delivery lane agree on what is pending.
 */
export async function listPublicMcpServers(): Promise<McpPublicServerRow[]> {
  const rows = await getDb().execute(sql`
    SELECT ms.id AS server_id, ms.name, ms.display_name, ms.description, ms.icon, ms.status,
           ms.auth_mode, ms.auth_note, ms.scope_menu, ms.manually_curated,
           ms.current_revision_id, ms.updated_at, cur.upstream_version AS current_version,
           (SELECT count(*) FROM web.bundle_mcp bm WHERE bm.server_id = ms.id) AS connections,
           prop.id AS proposal_id, prop.seq AS proposal_seq,
           prop.upstream_version AS proposal_version, prop.created_at AS proposal_created_at
    FROM web.mcp_server ms
    LEFT JOIN web.mcp_server_revision cur ON cur.id = ms.current_revision_id
    LEFT JOIN LATERAL (
      SELECT r.id, r.seq, r.upstream_version, r.created_at
      FROM web.mcp_server_revision r
      WHERE r.server_id = ms.id
        AND r.dismissed_at IS NULL
        AND (ms.current_revision_id IS NULL OR r.id <> ms.current_revision_id)
        AND (cur.seq IS NULL OR r.seq > cur.seq)
      ORDER BY r.seq DESC
      LIMIT 1
    ) prop ON TRUE
    WHERE ms.workspace_id IS NULL
    ORDER BY lower(ms.display_name)
  `);
  return (rows.rows as Record<string, unknown>[]).map((row) => ({
    serverId: row.server_id as string,
    name: (row.name as string | null) ?? null,
    displayName: row.display_name as string,
    description: (row.description as string | null) ?? null,
    icon: (row.icon as string | null) ?? null,
    status: row.status as string,
    authMode: (row.auth_mode as string | null) ?? null,
    authNote: (row.auth_note as string | null) ?? null,
    scopeMenu: row.scope_menu ?? null,
    manuallyCurated: row.manually_curated === true,
    currentRevisionId: (row.current_revision_id as string | null) ?? null,
    currentVersion: (row.current_version as string | null) ?? null,
    updatedAt: new Date(row.updated_at as string).getTime(),
    connections: Number(row.connections ?? 0),
    proposal:
      row.proposal_id === null || row.proposal_id === undefined
        ? null
        : {
            revisionId: row.proposal_id as string,
            seq: Number(row.proposal_seq),
            upstreamVersion: (row.proposal_version as string | null) ?? null,
            createdAt: new Date(row.proposal_created_at as string),
          },
  }));
}

/** A public server the catalog OFFERS must state a verified sign-in tier — the one fact a vendor's
 *  own word never settles. Required at add and edit, exactly as the file sync requires it of every
 *  entry, so a public row is never on offer while saying nobody checked. */
const PUBLIC_TIER_REQUIRED: McpGateRefusal = {
  code: "MCP_AUTH_MODE_REQUIRED",
  message:
    "a public server the catalog offers must state a verified sign-in tier before it goes on offer",
};

/**
 * The key a DELIVERED document carries so a machine reads the verified sign-in tier — the same
 * word the file sync injects, and the one `mcp_render` reads. The column is the catalog's own
 * record of the tier; this is what the client is handed, so a public document staff author must
 * carry it too, or a machine would see no tier (or a vendor's stale claim) and sign in wrongly.
 */
export const DELIVERED_AUTH_KEY = "sh.topos/auth";

/**
 * THE DELIVERED PUBLIC DOCUMENT — what a public catalog row stores and hands a machine, built from
 * an authored one exactly as the file sync's `deliveredDocumentOf` builds it from a file entry: the
 * registry backbone (name, description, remotes/packages, websiteUrl) plus the ONE `_meta` word the
 * client reads for the tier, and NOTHING ELSE.
 *
 * VERSIONLESS, deliberately. A public catalog server carries no version (the no-fake-version rule,
 * and every file-sourced row is versionless), so the authored `version` — and the `$schema` claim
 * that would require one — are dropped. This is also what lets a tier be corrected: the tier lives
 * in the delivered document, and were the document versioned, changing the tier would need a second
 * document under the same version, which "one document per version" forbids.
 */
function deliveredPublicDocument(
  document: Record<string, unknown>,
  authMode: McpAuthMode,
): Record<string, unknown> {
  const doc: Record<string, unknown> = { name: document.name, description: document.description };
  if (Array.isArray(document.remotes)) {
    doc.remotes = document.remotes;
  }
  if (Array.isArray(document.packages)) {
    doc.packages = document.packages;
  }
  if (typeof document.websiteUrl === "string" && document.websiteUrl.length > 0) {
    doc.websiteUrl = document.websiteUrl;
  }
  doc._meta = { [DELIVERED_AUTH_KEY]: authMode };
  return doc;
}

/** The verified tier a delivered document carries, or null when it names none or an unknown one —
 *  what `mcp_render` reads, and the value the row's column must agree with. */
function deliveredAuthOf(document: unknown): McpAuthMode | null {
  if (typeof document !== "object" || document === null) {
    return null;
  }
  const meta = (document as Record<string, unknown>)._meta;
  if (typeof meta !== "object" || meta === null) {
    return null;
  }
  const tier = (meta as Record<string, unknown>)[DELIVERED_AUTH_KEY];
  return tier === "none" || tier === "oauth" || tier === "manual" ? tier : null;
}

/**
 * Order-insensitive document content, so two documents that say the same thing compare equal
 * regardless of key order — a jsonb round-trip reorders keys, so the stored current and a freshly
 * built one must be compared this way, not by `JSON.stringify` (this mirrors the file sync's dedupe).
 */
function stableStringify(value: unknown): string {
  if (Array.isArray(value)) {
    return `[${value.map(stableStringify).join(",")}]`;
  }
  if (value !== null && typeof value === "object") {
    const obj = value as Record<string, unknown>;
    return `{${Object.keys(obj)
      .sort()
      .map((k) => `${JSON.stringify(k)}:${stableStringify(obj[k])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value ?? null);
}

/**
 * ADD A CUSTOM PUBLIC SERVER — one the file does not carry, written down by staff. It is born
 * `active`, MANUALLY CURATED (staff authored it, so the file never advances over it — a later file
 * entry under the same name proposes instead), and its first revision is promoted at once, because
 * a public row the catalog offers must have something to deliver.
 *
 * The document is normalized to the DELIVERED public shape (`deliveredPublicDocument`) — versionless,
 * with the verified tier folded in so a machine reads what a person established, not a vendor's word;
 * the column keeps the same tier for the surfaces. The public-name partial unique is the arbiter for
 * two operators racing the same name — the conflict IS the refusal.
 */
export async function createPublicMcpServer(
  staff: McpCatalogStaff,
  details: McpServerDetails,
  document: Record<string, unknown>,
): Promise<McpServerOutcome> {
  if (details.authMode === null) {
    return { refusal: PUBLIC_TIER_REQUIRED };
  }
  // A `manual` server needs its one line UP FRONT — refused here, not only at promotion, so a save
  // never half-writes and learns at the end that it cannot be offered.
  const noteMissing = authNoteRefusal({
    authMode: details.authMode,
    authNote: details.authNote ?? null,
  });
  if (noteMissing !== null) {
    return { refusal: noteMissing };
  }
  const stored = deliveredPublicDocument(document, details.authMode);
  const read = mcpRevisionFacts(stored, { requireVersion: false });
  if (read.refusal !== null) {
    return { refusal: read.refusal };
  }
  const landed = await inFinalTxOrRefusal<McpServerOutcome, McpServerOutcome>(
    async (tx, refuse) => {
      // SERIALIZE THE NAME CLAIM. The partial-unique index arbitrates named rows, but the null-name
      // shadow-check below is a plain read, so two transactions claiming one embedded name (a create
      // racing a null-name activation) could both pass it. A transaction advisory lock on the name —
      // taken by both this door and the edit door — makes them serialize, so the second sees the
      // first's committed row and refuses.
      await tx.execute(sql`SELECT pg_advisory_xact_lock(hashtextextended(${read.facts.name}, 0))`);
      // A self-maintained public row (name IS NULL) already serving this name by the name inside its
      // document reserves it exactly as a named row does — every name lane reads it by its embedded
      // name. The public partial-unique index constrains only `name`, so it cannot see a null-name
      // row; this is the check that keeps the name one server's either way.
      const shadowed = await tx.execute(sql`
        SELECT 1
        FROM web.mcp_server ms
        JOIN web.mcp_server_revision cur ON cur.id = ms.current_revision_id
        WHERE ms.workspace_id IS NULL AND ms.name IS NULL
          AND cur.document->>'name' = ${read.facts.name}
        LIMIT 1
      `);
      if ((shadowed.rows as unknown[]).length > 0) {
        return refuse({
          refusal: {
            code: "MCP_NAME_TAKEN",
            message: `the catalog already carries a self-maintained ${read.facts.name}`,
          },
        });
      }
      const serverId = mintMcpServerId();
      const born = await tx
        .insert(mcpServer)
        .values({
          id: serverId,
          workspaceId: null,
          name: read.facts.name,
          displayName: details.displayName,
          description: details.description ?? null,
          websiteUrl: details.websiteUrl ?? null,
          icon: details.icon ?? null,
          authMode: details.authMode,
          authNote: details.authNote ?? null,
          scopeMenu: details.scopeMenu ?? null,
          status: "active",
          manuallyCurated: true,
        })
        .onConflictDoNothing()
        .returning({ id: mcpServer.id });
      if (born[0] === undefined) {
        return refuse({
          refusal: {
            code: "MCP_NAME_TAKEN",
            message: `the catalog already carries ${read.facts.name}`,
          },
        });
      }
      const added = await addMcpRevisionInTx(tx, serverId, { document: stored });
      if (added.refusal !== null) {
        return refuse({ refusal: added.refusal });
      }
      const server = await lockServerInTx(tx, serverId);
      if (server === undefined) {
        return refuse({ refusal: SERVER_NOT_FOUND });
      }
      const promoted = await promoteMcpRevisionInTx(tx, server, added.revisionId, staff.display);
      if (promoted !== null) {
        return refuse({ refusal: promoted });
      }
      await auditCatalogInTx(tx, staff, "mcp_public_server_created", serverId, {
        name: read.facts.name,
        displayName: details.displayName,
      });
      return { refusal: null, serverId, revisionId: added.revisionId };
    },
  );
  return landed.refused !== null ? landed.refused : landed.value;
}

/**
 * EDIT A PUBLIC SERVER — update its editorial columns and, when what a machine RECEIVES changes,
 * promote a new revision of it; either way MARK IT MANUALLY CURATED so the file proposes future
 * versions rather than advancing over this hand. A private row, or one that does not exist, answers
 * "no such server" — staff edit the public catalog only, and the answer to "may I edit this" must
 * not double as a way to learn a private server exists.
 *
 * EDITORIAL-ONLY IS NOT A NEW VERSION. The display name, the scope menu and the manual note are the
 * ROW's own columns, not part of the delivered document; changing only those must not append a
 * duplicate revision — which a versioned document could not even do (one document per version), so
 * editing such a server's metadata would otherwise refuse `MCP_VERSION_HELD` and roll back. So the
 * delivered document is materialized (the tier folded in), compared by content to the current, and
 * a new revision is appended only when it actually differs.
 *
 * OPTIMISTIC CONCURRENCY. A staff view is a snapshot; a save built on it can overwrite another
 * operator's work — a promotion that moved the pointer, or even a bare editorial change that moved
 * no revision. Passing `expectedRowStamp` (the row's `updated_at` in epoch millis, as the edit view
 * loaded it) makes this refuse when the row has been written since — every edit and every promote
 * bumps `updated_at`, so it catches a stale save the revision id alone would miss. Omit it
 * (undefined) to skip the check.
 */
export async function editPublicMcpServer(
  staff: McpCatalogStaff,
  serverId: string,
  details: McpServerDetails,
  document: Record<string, unknown>,
  expectedRowStamp?: number,
): Promise<McpServerOutcome> {
  if (details.authMode === null) {
    return { refusal: PUBLIC_TIER_REQUIRED };
  }
  // UP FRONT, and it matters here more than anywhere: the editorial-only fast path below never
  // reaches the promotion-time note gate, so clearing a `manual` server's note without touching its
  // document would otherwise leave it offered with no instructions.
  const noteMissing = authNoteRefusal({
    authMode: details.authMode,
    authNote: details.authNote ?? null,
  });
  if (noteMissing !== null) {
    return { refusal: noteMissing };
  }
  const stored = deliveredPublicDocument(document, details.authMode);
  const landed = await inFinalTxOrRefusal<McpServerOutcome, McpServerOutcome>(
    async (tx, refuse) => {
      const server = await lockServerInTx(tx, serverId);
      if (server === undefined || server.workspaceId !== null) {
        return refuse({ refusal: SERVER_NOT_FOUND });
      }
      // Refuse a save built on a stale view: if the caller named the stamp it loaded and the row has
      // been written under this lock since (an edit or a promote), this would overwrite that newer
      // work — the row fields, or the pointer every workspace follows. Reload and edit again.
      if (expectedRowStamp !== undefined && server.updatedAt.getTime() !== expectedRowStamp) {
        return refuse({
          refusal: {
            code: "MCP_REVISION_STALE",
            message: "this server changed since you loaded it — reload and edit it again",
          },
        });
      }
      // The current document, read once — for the identity fence and the unchanged-delivery check.
      const currentRows =
        server.currentRevisionId === null
          ? []
          : await tx
              .select({ document: mcpServerRevision.document })
              .from(mcpServerRevision)
              .where(eq(mcpServerRevision.id, server.currentRevisionId))
              .limit(1);
      const currentDoc = (currentRows[0]?.document as Record<string, unknown> | undefined) ?? null;
      // FENCE THE IDENTITY of a null-name row. `addMcpRevisionInTx` refuses a document that renames a
      // NAMED server, but a self-maintained row carries no `name` — its identity is the name inside
      // its current document, and the partial-unique index cannot see it. So for a null-name row an
      // edit must neither (a) change an existing embedded name, nor (b) activate one under a name
      // some OTHER public row already carries — either would make a name-based read a coin flip.
      // (A named server is fenced by `addMcpRevisionInTx` below.)
      let nameClaim: string | undefined;
      if (server.name === null) {
        const read = mcpRevisionFacts(stored, { requireVersion: false });
        if (read.refusal !== null) {
          return refuse({ refusal: read.refusal });
        }
        const embeddedName = read.facts.name;
        const currentName = typeof currentDoc?.name === "string" ? currentDoc.name : null;
        if (currentName !== null && currentName !== embeddedName) {
          return refuse({
            refusal: {
              code: "MCP_NAME_MISMATCH",
              message: `this server is ${currentName}; a version of it cannot call itself ${embeddedName}`,
            },
          });
        }
        // The same advisory lock the create door takes, so a null-name activation and a create
        // claiming one embedded name serialize instead of both passing a plain read.
        await tx.execute(sql`SELECT pg_advisory_xact_lock(hashtextextended(${embeddedName}, 0))`);
        const shadowed = await tx.execute(sql`
          SELECT 1
          FROM web.mcp_server ms
          LEFT JOIN web.mcp_server_revision cur ON cur.id = ms.current_revision_id
          WHERE ms.workspace_id IS NULL AND ms.id <> ${serverId}
            AND COALESCE(ms.name, cur.document->>'name') = ${embeddedName}
          LIMIT 1
        `);
        if ((shadowed.rows as unknown[]).length > 0) {
          return refuse({
            refusal: {
              code: "MCP_NAME_TAKEN",
              message: `the catalog already carries ${embeddedName}`,
            },
          });
        }
        // CLAIM THE NAME into the column. A staff-curated row is a proper named row: writing the
        // embedded name in lets the partial-unique index arbitrate it and lets the authoritative
        // file sync (which searches `mcp_server.name`) FIND this row rather than inserting a second
        // public server under the same name when a later `catalog.json` release adds it.
        nameClaim = embeddedName;
      }
      await tx
        .update(mcpServer)
        .set({
          ...(nameClaim === undefined ? {} : { name: nameClaim }),
          displayName: details.displayName,
          description: details.description ?? null,
          websiteUrl: details.websiteUrl ?? null,
          icon: details.icon ?? null,
          authMode: details.authMode,
          authNote: details.authNote ?? null,
          scopeMenu: details.scopeMenu ?? null,
          manuallyCurated: true,
          // An edit CURATES the server into the offered catalog — relist a row a migration left
          // `delisted`, so an edited server is one workspaces can actually connect.
          status: "active",
        })
        .where(eq(mcpServer.id, serverId));
      // When the delivered document is unchanged, this is a column-only edit: keep the current
      // revision and change nothing a machine receives. (Compared order-insensitively — the stored
      // current came back through jsonb.)
      if (currentDoc !== null && stableStringify(currentDoc) === stableStringify(stored)) {
        await auditCatalogInTx(tx, staff, "mcp_public_server_edited", serverId, {
          revisionId: server.currentRevisionId,
          displayName: details.displayName,
        });
        return { refusal: null, serverId, revisionId: server.currentRevisionId as string };
      }
      const added = await addMcpRevisionInTx(tx, serverId, { document: stored });
      if (added.refusal !== null) {
        return refuse({ refusal: added.refusal });
      }
      // Promote against the server as edited — the auth note just written is what the manual-note
      // gate must see, so re-read it under the lock this transaction already holds.
      const edited = await lockServerInTx(tx, serverId);
      if (edited === undefined) {
        return refuse({ refusal: SERVER_NOT_FOUND });
      }
      const promoted = await promoteMcpRevisionInTx(tx, edited, added.revisionId, staff.display);
      if (promoted !== null) {
        return refuse({ refusal: promoted });
      }
      await auditCatalogInTx(tx, staff, "mcp_public_server_edited", serverId, {
        revisionId: added.revisionId,
        displayName: details.displayName,
      });
      return { refusal: null, serverId, revisionId: added.revisionId };
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
  name: string | null;
  displayName: string;
  description: string | null;
  icon: string | null;
  authMode: string | null;
  authNote: string | null;
  url: string | null;
  transport: string | null;
  revisionId: string;
  /** The version the current revision offers, or NULL when it names none. */
  upstreamVersion: string | null;
  /** The catalog name of the ACTIVE bundle this workspace connected it as, or null. One
   *  connection per server per workspace, so this is the answer, not one of several. */
  connectedAs: string | null;
  /** The connection exists but its bundle is ARCHIVED — the server cannot be added again (the
   *  one connection is still spoken for) and restoring the archived bundle is what brings it
   *  back. Never true alongside `connectedAs`. */
  inArchive: boolean;
}

// ── The server a bundle connects to, as its page reads it ───────────────────────────────────

/**
 * Where a revision stands relative to the pointer — derived, never stored:
 *  · `current` — the one the server offers right now;
 *  · `proposal` — a later revision (a file update, or a staff draft) awaiting promotion;
 *  · `history` — a revision that was current before, or an older one behind the current;
 *  · `dismissed` — a proposal a staff member declined (terminal).
 */
export type McpRevisionState = "current" | "proposal" | "history" | "dismissed";

/** One revision on the history a server's page lists. */
export interface McpRevisionRow {
  revisionId: string;
  seq: number;
  /** The `version` this revision stored, or NULL when it named none. */
  upstreamVersion: string | null;
  state: McpRevisionState;
  publishedAt: Date | null;
  publishedBy: string | null;
}

/** Everything a connected server's page shows, in one read. */
export interface McpServerFace {
  bundleId: string;
  serverId: string;
  /** The workspace's OWN server — the one an owner may edit. */
  isPrivate: boolean;
  name: string | null;
  displayName: string;
  description: string | null;
  websiteUrl: string | null;
  icon: string | null;
  authMode: string | null;
  authNote: string | null;
  /** The connection follows ONE revision rather than the server's current. */
  pinnedRevisionId: string | null;
  currentRevisionId: string | null;
  /** The owner's routing mandate for this connection — null is Auto, the ordinary state. */
  gatewayPolicy: "direct" | "required" | null;
  /** THIS viewer chose direct for their own machines (meaningful only under Auto). */
  gatewayOptedOut: boolean;
  /** What this workspace's machines receive — null when the server has nothing current. */
  resolved: {
    revisionId: string;
    /** The version this workspace's machines receive, or NULL when the revision names none. */
    upstreamVersion: string | null;
    state: McpRevisionState;
    url: string | null;
    transport: string | null;
    document: Record<string, unknown>;
    probe: McpProbeRecord | null;
  } | null;
  /** The version history, newest first. A private server's is its owner's edit trail. */
  revisions: McpRevisionRow[];
}

/** The probe columns as the shared line reader takes them — absent is "not checked yet". */
function probeRecordOf(row: {
  probe_outcome: string | null;
  probed_at: string | Date | null;
  verification: unknown;
}): McpProbeRecord | null {
  if (row.probe_outcome === null || row.probed_at === null) {
    return null;
  }
  const detail = (row.verification as { probeDetail?: unknown } | null)?.probeDetail;
  return {
    outcome: row.probe_outcome as McpProbeOutcome,
    probedAt: new Date(row.probed_at).toISOString(),
    detail: typeof detail === "string" ? detail : null,
  };
}

/** Where one revision stands, given the current pointer and the current's seq. */
function revisionStateOf(
  revisionId: string,
  seq: number,
  dismissedAt: unknown,
  currentRevisionId: string | null,
  currentSeq: number | null,
): McpRevisionState {
  if (dismissedAt !== null && dismissedAt !== undefined) {
    return "dismissed";
  }
  if (revisionId === currentRevisionId) {
    return "current";
  }
  return currentSeq !== null && seq > currentSeq ? "proposal" : "history";
}

/**
 * THE SERVER BEHIND ONE BUNDLE. Workspace-scoped through the connection row, so a bundle id from
 * another workspace resolves to nothing — the same answer an id that names no server gets.
 *
 * A PRIVATE server's history is all of it; a PUBLIC server's is the part that has been on offer
 * (a revision with a promotion stamp), because a file proposal awaiting a staff decision is a
 * staff concern, not something the workspace is being offered.
 */
export async function mcpServerFace(
  actor: MemberActor | SessionActor,
  bundleId: string,
): Promise<McpServerFace | null> {
  const rows = await getDb().execute(sql`
    SELECT bm.bundle_id, bm.server_id, bm.pinned_revision_id, bm.gateway_policy,
           EXISTS (
             SELECT 1 FROM web.mcp_gateway_optout go
             WHERE go.workspace_id = bm.workspace_id AND go.server_id = bm.server_id
               AND go.user_id = ${actor.userId}
           ) AS gateway_optout,
           ms.workspace_id, ms.name, ms.display_name, ms.description, ms.website_url,
           ms.icon, ms.auth_mode, ms.auth_note, ms.current_revision_id,
           curr.seq AS current_seq,
           r.id AS revision_id, r.seq, r.upstream_version, r.dismissed_at, r.url, r.transport,
           r.document, r.probe_outcome, r.probed_at, r.verification
    FROM web.bundle_mcp bm
    JOIN web.mcp_server ms ON ms.id = bm.server_id
    LEFT JOIN web.mcp_server_revision curr ON curr.id = ms.current_revision_id
    LEFT JOIN web.mcp_server_revision r ON r.id = ${MCP_RESOLVED_REVISION}
    WHERE bm.workspace_id = ${actor.workspaceId} AND bm.bundle_id = ${bundleId}
    LIMIT 1
  `);
  const row = rows.rows[0] as Record<string, unknown> | undefined;
  if (row === undefined) {
    return null;
  }
  const isPrivate = row.workspace_id !== null;
  const currentRevisionId = (row.current_revision_id as string | null) ?? null;
  const currentSeq = row.current_seq === null ? null : Number(row.current_seq);
  const history = await getDb().execute(sql`
    SELECT r.id AS revision_id, r.seq, r.upstream_version, r.dismissed_at,
           r.published_at, r.published_by
    FROM web.mcp_server_revision r
    WHERE r.server_id = ${row.server_id as string}
      AND (${isPrivate} OR r.published_at IS NOT NULL)
    ORDER BY r.seq DESC
  `);
  return {
    bundleId: row.bundle_id as string,
    serverId: row.server_id as string,
    isPrivate,
    name: (row.name as string | null) ?? null,
    displayName: row.display_name as string,
    description: (row.description as string | null) ?? null,
    websiteUrl: (row.website_url as string | null) ?? null,
    icon: (row.icon as string | null) ?? null,
    authMode: (row.auth_mode as string | null) ?? null,
    authNote: (row.auth_note as string | null) ?? null,
    pinnedRevisionId: (row.pinned_revision_id as string | null) ?? null,
    currentRevisionId,
    gatewayPolicy: (row.gateway_policy as "direct" | "required" | null) ?? null,
    gatewayOptedOut: row.gateway_optout === true,
    resolved:
      row.revision_id === null || row.revision_id === undefined
        ? null
        : {
            revisionId: row.revision_id as string,
            upstreamVersion: (row.upstream_version as string | null) ?? null,
            state: revisionStateOf(
              row.revision_id as string,
              Number(row.seq),
              row.dismissed_at,
              currentRevisionId,
              currentSeq,
            ),
            url: (row.url as string | null) ?? null,
            transport: (row.transport as string | null) ?? null,
            document: row.document as Record<string, unknown>,
            probe: probeRecordOf(
              row as unknown as {
                probe_outcome: string | null;
                probed_at: string | null;
                verification: unknown;
              },
            ),
          },
    revisions: (history.rows as Record<string, unknown>[]).map((rev) => ({
      revisionId: rev.revision_id as string,
      seq: Number(rev.seq),
      upstreamVersion: (rev.upstream_version as string | null) ?? null,
      state: revisionStateOf(
        rev.revision_id as string,
        Number(rev.seq),
        rev.dismissed_at,
        currentRevisionId,
        currentSeq,
      ),
      publishedAt: rev.published_at === null ? null : new Date(rev.published_at as string),
      publishedBy: (rev.published_by as string | null) ?? null,
    })),
  };
}

// ── The workspace registry read lane: what THIS WORKSPACE runs, in the official read shape ──

/**
 * One server as a REGISTRY ANSWER: the document, plus the facts this catalog adds under its own
 * `_meta` key. The lane serves what a workspace RUNS — the revision its connection resolves to.
 */
export interface McpRegistryRow {
  serverId: string;
  /** The reverse-DNS name, or null for a row addressed by the name inside its document. Absent
   *  from the wire — the serializer reads the name off the document. */
  name: string | null;
  authMode: string | null;
  authNote: string | null;
  scopeMenu: unknown;
  revisionId: string;
  document: Record<string, unknown>;
  /** Whether this revision is the one the server currently offers. */
  isLatest: boolean;
}

/** One page of registry rows: what was asked for, plus the cursor to ask again with. */
export interface McpRegistryPage {
  rows: McpRegistryRow[];
  nextCursor: string | null;
}

/** How many rows one page carries when the caller names no size, and the ceiling on asking. */
export const MCP_REGISTRY_PAGE_DEFAULT = 50;
export const MCP_REGISTRY_PAGE_MAX = 100;

/** The page size a caller's `limit` really gets — a garbage value reads as none given. */
export function mcpRegistryLimit(raw: string | null): number {
  const asked = Number(raw);
  if (!Number.isInteger(asked) || asked < 1) {
    return MCP_REGISTRY_PAGE_DEFAULT;
  }
  return Math.min(asked, MCP_REGISTRY_PAGE_MAX);
}

function registryRowsOf(rows: Record<string, unknown>[]): McpRegistryRow[] {
  return rows.map((row) => ({
    serverId: row.server_id as string,
    name: (row.name as string | null) ?? null,
    authMode: (row.auth_mode as string | null) ?? null,
    authNote: (row.auth_note as string | null) ?? null,
    scopeMenu: row.scope_menu ?? null,
    revisionId: row.revision_id as string,
    document: row.document as Record<string, unknown>,
    isLatest: row.is_latest === true,
  }));
}

/** Cut one over-read page down to size and say whether there is more. */
function paged(rows: McpRegistryRow[], limit: number): McpRegistryPage {
  const more = rows.length > limit;
  const page = more ? rows.slice(0, limit) : rows;
  return { rows: page, nextCursor: more ? (page.at(-1)?.serverId ?? null) : null };
}

/** The workspace lane's one FROM/WHERE, written once for the list and the single read. */
function workspaceRegistrySelect(workspaceId: string) {
  return sql`
    SELECT ms.id AS server_id, ms.name, ms.auth_mode, ms.auth_note, ms.scope_menu,
           r.id AS revision_id, r.document,
           (r.id = ms.current_revision_id) AS is_latest
    FROM web.mcp_server ms
    LEFT JOIN (
      SELECT c.server_id, c.pinned_revision_id
      FROM web.bundle_mcp c
      JOIN web.bundle b
        ON b.id = c.bundle_id AND b.workspace_id = c.workspace_id AND b.status = 'active'
      WHERE c.workspace_id = ${workspaceId}
    ) bm ON bm.server_id = ms.id
    JOIN web.mcp_server_revision r ON r.id = ${MCP_RESOLVED_REVISION}
    WHERE (bm.server_id IS NOT NULL
           OR (ms.workspace_id = ${workspaceId} AND ms.status = 'active'))
  `;
}

/**
 * WHAT ONE WORKSPACE RUNS — its live connections, each resolved exactly as delivery resolves it
 * (a pin, else the server's current), plus its own private servers.
 */
export async function workspaceRegistryServers(
  actor: MemberActor | SessionActor,
  page: { cursor?: string | null; limit?: number },
): Promise<McpRegistryPage> {
  const limit = page.limit ?? MCP_REGISTRY_PAGE_DEFAULT;
  const cursor = page.cursor ?? null;
  const rows = await getDb().execute(sql`
    ${workspaceRegistrySelect(actor.workspaceId)}
      AND (${cursor}::text IS NULL OR ms.id > ${cursor})
    ORDER BY ms.id
    LIMIT ${limit + 1}
  `);
  return paged(registryRowsOf(rows.rows as Record<string, unknown>[]), limit);
}

/**
 * The same read for ONE embedded name. A workspace's OWN server outranks a public one carrying the
 * same name: inside a workspace, the workspace's answer is the answer.
 */
export async function workspaceRegistryServer(
  actor: MemberActor | SessionActor,
  serverName: string,
): Promise<McpRegistryRow | null> {
  const rows = await getDb().execute(sql`
    ${workspaceRegistrySelect(actor.workspaceId)}
      AND COALESCE(ms.name, r.document->>'name') = ${serverName}
    ORDER BY (ms.workspace_id IS NULL), ms.id
    LIMIT 1
  `);
  return registryRowsOf(rows.rows as Record<string, unknown>[])[0] ?? null;
}

/**
 * WHICH SERVER a name names for this workspace, when one is connectable: the workspace's OWN row
 * first, then the public catalog's — because inside a workspace the workspace's answer is the
 * answer. A name that resolves to two rows must never be a coin flip between two documents.
 *
 * A server with no `name` on the row is addressed by the name inside its current document, so a
 * catalog server stays connectable by the name a person knows it by.
 */
export async function connectableMcpServerByName(
  actor: MemberActor | SessionActor,
  serverName: string,
): Promise<{ serverId: string; displayName: string } | null> {
  const rows = await getDb().execute(sql`
    SELECT ms.id AS server_id, ms.display_name
    FROM web.mcp_server ms
    LEFT JOIN web.mcp_server_revision cur ON cur.id = ms.current_revision_id
    WHERE COALESCE(ms.name, cur.document->>'name') = ${serverName}
      AND ms.status = 'active'
      AND (ms.workspace_id IS NULL OR ms.workspace_id = ${actor.workspaceId})
    ORDER BY (ms.workspace_id IS NULL), ms.id
    LIMIT 1
  `);
  const row = (rows.rows as Record<string, unknown>[])[0];
  return row === undefined
    ? null
    : { serverId: row.server_id as string, displayName: row.display_name as string };
}

/**
 * The bundle a workspace ALREADY connects a catalog server through, by the server's name — `null`
 * when it connects none. The idempotent half of the lane's connect: asking twice for the same
 * server is not a mistake to refuse, it is a question with an answer.
 */
export async function connectedMcpBundle(
  actor: MemberActor | SessionActor,
  serverName: string,
): Promise<{ bundleId: string; name: string } | null> {
  const rows = await getDb().execute(sql`
    SELECT b.id AS bundle_id, b.name
    FROM web.mcp_server ms
    JOIN web.bundle_mcp bm ON bm.server_id = ms.id AND bm.workspace_id = ${actor.workspaceId}
    JOIN web.bundle b ON b.id = bm.bundle_id AND b.workspace_id = bm.workspace_id
    LEFT JOIN web.mcp_server_revision cur ON cur.id = ms.current_revision_id
    WHERE COALESCE(ms.name, cur.document->>'name') = ${serverName}
      AND (ms.workspace_id IS NULL OR ms.workspace_id = ${actor.workspaceId})
      AND b.status = 'active'
    ORDER BY (ms.workspace_id IS NULL), ms.id
    LIMIT 1
  `);
  const row = (rows.rows as Record<string, unknown>[])[0];
  return row === undefined ? null : { bundleId: row.bundle_id as string, name: row.name as string };
}

/**
 * WHAT A WORKSPACE MAY CONNECT: the public catalog's active servers plus its own private ones,
 * each with the document its `current` names. The order is the one a person scans — display name,
 * case-insensitively — and it is the same list for every member, because a seat reads the whole
 * catalog.
 */
export async function connectableMcpServers(
  actor: MemberActor | SessionActor,
): Promise<McpCatalogRow[]> {
  const rows = await getDb().execute(sql`
    SELECT ms.id AS server_id, ms.name, ms.display_name, ms.description, ms.icon,
           ms.auth_mode, ms.auth_note, r.url, r.transport,
           r.id AS revision_id, r.upstream_version,
           CASE WHEN b.status = 'active' THEN b.name END AS connected_as,
           (b.status = 'archived') AS in_archive
    FROM web.mcp_server ms
    JOIN web.mcp_server_revision r ON r.id = ms.current_revision_id
    LEFT JOIN web.bundle_mcp bm ON bm.server_id = ms.id AND bm.workspace_id = ${actor.workspaceId}
    LEFT JOIN web.bundle b ON b.id = bm.bundle_id AND b.workspace_id = bm.workspace_id
    WHERE ms.status = 'active'
      AND (ms.workspace_id IS NULL OR ms.workspace_id = ${actor.workspaceId})
    ORDER BY lower(ms.display_name)
  `);
  return (rows.rows as Record<string, unknown>[]).map((row) => ({
    serverId: row.server_id as string,
    name: (row.name as string | null) ?? null,
    displayName: row.display_name as string,
    description: (row.description as string | null) ?? null,
    icon: (row.icon as string | null) ?? null,
    authMode: (row.auth_mode as string | null) ?? null,
    authNote: (row.auth_note as string | null) ?? null,
    url: (row.url as string | null) ?? null,
    transport: (row.transport as string | null) ?? null,
    revisionId: row.revision_id as string,
    upstreamVersion: (row.upstream_version as string | null) ?? null,
    connectedAs: (row.connected_as as string | null) ?? null,
    inArchive: row.in_archive === true,
  }));
}
