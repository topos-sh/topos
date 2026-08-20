import { and, eq, isNull } from "drizzle-orm";
import { mintMcpServerId } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import {
  addMcpRevisionInTx,
  lockServerInTx,
  promoteMcpRevisionInTx,
} from "@/lib/db/queries.mcp-catalog.server";
import { mcpServer, mcpServerRevision } from "@/lib/db/schema.app";
import catalogFile from "@/lib/mcp/catalog.json";

/**
 * THE CATALOG SYNC — the committed file reconciled into the public `mcp_server` rows.
 *
 * The generic/public catalog is DECLARED in one committed JSON file (a top-level array of
 * registry-format `server.json` objects, editorial metadata under `_meta["sh.topos/catalog"]`).
 * This is the step that makes those declarations rows: it runs at boot, after the migrations, and
 * for each entry it upserts the public server by NAME and reconciles its revision history.
 *
 * THREE THINGS IT WILL NOT DO. It never deletes a server (a name dropped from the file lingers —
 * removing one is a deliberate panel act). It never clobbers a live `current` (a file update lands
 * as a NEW revision, deduped by content). And it never overrides a staff decision: once a server
 * is `manually_curated` (a staff member edited or promoted it), a file version lands as a
 * NON-CURRENT proposal instead of advancing — a self-hosted install has no panel, so it never
 * sets that bit and always tracks the file.
 *
 * ONE PROMOTION FUNCTION. When the sync does advance `current`, it goes through
 * `promoteMcpRevisionInTx` — the same seam an owner's save and a staff promote use — so the
 * compatibility gate a later increment adds sits in one place.
 *
 * IDEMPOTENT. A settled database finds every file document already present as a revision and does
 * nothing: dedupe is by CONTENT (order-insensitive, so a jsonb round-trip never reads as a change),
 * and a document a staff member has dismissed is left dismissed and never re-proposed.
 */

/** Our editorial block inside a file entry's `_meta`. */
const CATALOG_META_KEY = "sh.topos/catalog";
/** The word a delivered document carries for the client to read (`mcp_render` reads this key). */
const DELIVERED_AUTH_KEY = "sh.topos/auth";
/** Attribution recorded on a revision the sync promotes — the file is the author. */
const CURATED_BY = "Topos";

type Json = Record<string, unknown>;

interface CatalogEditorial {
  authTier?: unknown;
  authNote?: unknown;
  scopeHints?: unknown;
  icon?: unknown;
}

/** What the sync did — the operator's account of one pass. */
export interface McpCatalogSyncReport {
  /** Servers created from the file (new names). */
  created: number;
  /** Servers whose `current` advanced to a file version (auto-advance, untouched by staff). */
  promoted: number;
  /** File versions filed as NON-CURRENT proposals against a manually-curated server. */
  proposed: number;
  /** Entries already reconciled — the document was present and current (or a curated proposal). */
  unchanged: number;
  /** File documents left alone because a staff member had dismissed exactly that document. */
  dismissedSkipped: number;
  /** Entries the document gate refused, named so an operator can look — never forced through. */
  refused: string[];
}

/** A recursive, key-sorted serialization — two documents that a jsonb round-trip reordered still
 *  compare equal, so dedupe is by content and never by key order. */
function stableStringify(value: unknown): string {
  if (Array.isArray(value)) {
    return `[${value.map(stableStringify).join(",")}]`;
  }
  if (value !== null && typeof value === "object") {
    const obj = value as Json;
    const keys = Object.keys(obj).sort();
    return `{${keys.map((k) => `${JSON.stringify(k)}:${stableStringify(obj[k])}`).join(",")}}`;
  }
  return JSON.stringify(value ?? null);
}

function documentsEqual(a: unknown, b: unknown): boolean {
  return stableStringify(a) === stableStringify(b);
}

function asString(value: unknown): string | null {
  return typeof value === "string" && value.length > 0 ? value : null;
}

/** The editorial block a file entry carries under our own `_meta` key. */
function editorialOf(entry: Json): CatalogEditorial {
  const meta = entry._meta;
  if (meta === null || typeof meta !== "object") {
    return {};
  }
  const block = (meta as Json)[CATALOG_META_KEY];
  return block !== null && typeof block === "object" ? (block as CatalogEditorial) : {};
}

/** A reverse-DNS name → a plain display name, for an entry that names no `title`. */
function displayNameFor(entry: Json): string {
  const title = asString(entry.title);
  if (title !== null) {
    return title;
  }
  const name = asString(entry.name) ?? "server";
  const tail = name.slice(name.indexOf("/") + 1);
  return tail.length > 0 ? tail : name;
}

/**
 * THE DELIVERED DOCUMENT — what a machine receives, computed from the file entry. It carries the
 * registry backbone (name, description, remotes, and packages/websiteUrl when the entry has them)
 * and the ONE `_meta` word the client reads for the sign-in tier. The file's own `$schema`, its
 * `title` and its editorial `_meta` block are the FILE's framing, not part of the versionless
 * document people are handed — which keeps the document honest (no `$schema` line over a document
 * that names no version) and the dedupe stable.
 */
export function deliveredDocumentOf(entry: Json): Json {
  const editorial = editorialOf(entry);
  const doc: Json = {
    name: entry.name,
    description: entry.description,
    remotes: entry.remotes,
  };
  if (Array.isArray(entry.packages)) {
    doc.packages = entry.packages;
  }
  const websiteUrl = asString(entry.websiteUrl);
  if (websiteUrl !== null) {
    doc.websiteUrl = websiteUrl;
  }
  const authTier = asString(editorial.authTier);
  if (authTier !== null) {
    doc._meta = { [DELIVERED_AUTH_KEY]: authTier };
  }
  return doc;
}

/**
 * RECONCILE THE FILE INTO THE PUBLIC CATALOG. Pass `entries` to sync a specific set (tests do);
 * omit it to read the committed file. One transaction per entry, so a single bad document is named
 * and skipped rather than failing the whole pass.
 */
export async function syncMcpCatalog(
  entries: Json[] = catalogFile as unknown as Json[],
): Promise<McpCatalogSyncReport> {
  const report: McpCatalogSyncReport = {
    created: 0,
    promoted: 0,
    proposed: 0,
    unchanged: 0,
    dismissedSkipped: 0,
    refused: [],
  };
  for (const entry of entries) {
    const name = asString(entry.name);
    if (name === null) {
      report.refused.push("(entry with no name)");
      continue;
    }
    await syncOneEntry(entry, name, report);
  }
  return report;
}

async function syncOneEntry(
  entry: Json,
  name: string,
  report: McpCatalogSyncReport,
): Promise<void> {
  const editorial = editorialOf(entry);
  const doc = deliveredDocumentOf(entry);
  const columns = {
    displayName: displayNameFor(entry),
    description: asString(entry.description),
    websiteUrl: asString(entry.websiteUrl),
    icon: asString(editorial.icon),
    authMode: asString(editorial.authTier),
    authNote: asString(editorial.authNote),
    scopeMenu: editorial.scopeHints ?? null,
  };

  await getDb().transaction(async (tx) => {
    const found = await tx
      .select({ id: mcpServer.id })
      .from(mcpServer)
      .where(and(isNull(mcpServer.workspaceId), eq(mcpServer.name, name)))
      .limit(1)
      .for("update");

    let serverId: string;
    if (found[0] === undefined) {
      serverId = mintMcpServerId();
      await tx.insert(mcpServer).values({
        id: serverId,
        workspaceId: null,
        name,
        ...columns,
        status: "active",
        manuallyCurated: false,
      });
      report.created += 1;
    } else {
      serverId = found[0].id;
    }

    let server = await lockServerInTx(tx, serverId);
    if (server === undefined) {
      return;
    }

    // Editorial rows describe the SERVER, not a version — refreshed from the file while a server
    // still tracks it, but LEFT ALONE once a staff member has taken it in hand (curated), so
    // curation is never quietly undone by a boot.
    if (!server.manuallyCurated && found[0] !== undefined) {
      await tx.update(mcpServer).set(columns).where(eq(mcpServer.id, serverId));
      server = await lockServerInTx(tx, serverId);
      if (server === undefined) {
        return;
      }
    }

    const revisions = await tx
      .select({
        id: mcpServerRevision.id,
        document: mcpServerRevision.document,
        dismissedAt: mcpServerRevision.dismissedAt,
      })
      .from(mcpServerRevision)
      .where(eq(mcpServerRevision.serverId, serverId));
    const match = revisions.find((r) => documentsEqual(r.document, doc));

    // A document a staff member already dismissed stays dismissed and is never re-offered.
    if (match !== undefined && match.dismissedAt !== null) {
      report.dismissedSkipped += 1;
      return;
    }

    let targetId = match?.id ?? null;
    let appended = false;
    if (targetId === null) {
      const added = await addMcpRevisionInTx(tx, serverId, { document: doc });
      if (added.refusal !== null) {
        report.refused.push(name);
        return;
      }
      targetId = added.revisionId;
      appended = true;
    }

    if (server.manuallyCurated) {
      // The file proposes; a staff member promotes. A new document is a proposal, an existing one
      // is nothing to do.
      if (appended) {
        report.proposed += 1;
      } else {
        report.unchanged += 1;
      }
      return;
    }

    // Untouched by staff → track the file: advance `current` to this document.
    if (targetId === server.currentRevisionId) {
      report.unchanged += 1;
      return;
    }
    const refusal = await promoteMcpRevisionInTx(tx, server, targetId, CURATED_BY);
    if (refusal !== null) {
      report.refused.push(name);
      return;
    }
    report.promoted += 1;
  });
}

/**
 * Run the sync at boot and SAY what it refused. Silent when it worked (a settled catalog is
 * bookkeeping nobody needs told); loud and naming names when a document could not be stored, and
 * never throwing — a catalog that will not reconcile must not stop the app from booting.
 */
export async function syncMcpCatalogAtBoot(): Promise<void> {
  let report: McpCatalogSyncReport;
  try {
    report = await syncMcpCatalog();
  } catch (error) {
    const line = error instanceof Error ? error.message.split("\n", 1)[0] : String(error);
    console.warn(
      "[mcp-catalog-sync] the catalog sync did not complete; it runs again on the next boot.",
    );
    console.warn(`[mcp-catalog-sync] cause: ${line}`);
    return;
  }
  for (const name of report.refused) {
    console.warn(
      `[mcp-catalog-sync] ${name}: its document is not one this build can store, so it was left ` +
        "as the file declared it and delivers whatever it already had.",
    );
  }
}
