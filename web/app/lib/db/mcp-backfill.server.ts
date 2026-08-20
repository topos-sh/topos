import { sql } from "drizzle-orm";
import { mintMcpServerId } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import { inFinalTxOrRefusal } from "@/lib/db/queries.custody.server";
import {
  addMcpRevisionInTx,
  lockServerInTx,
  promoteMcpRevisionInTx,
} from "@/lib/db/queries.mcp-catalog.server";
import { bundleMcp, mcpServer } from "@/lib/db/schema.app";
import { serverDocumentOf } from "@/lib/mcp/catalog.server";

/**
 * WHAT AN EXISTING MCP BUNDLE IS CONNECTED TO — the one step the migrations beside this file
 * cannot take themselves.
 *
 * Until now an MCP bundle WAS its bytes: a `server.json` in the vault, addressed by a content
 * hash. The catalog turns that inside out — the bundle names a server, and the server holds the
 * document — so every bundle that already exists needs the row saying which server it means. The
 * name that answers that question lives INSIDE those bytes, and the vault stores bytes behind an
 * HTTP lane that no SQL statement can reach. So the step runs here, in the tier where the
 * documents are readable, immediately after the DDL lands and before the first request.
 *
 * WHERE A BUNDLE LANDS:
 *  · its embedded registry name matches a server in the global catalog — the bundle connects to
 *    that one, and from then on receives what the catalog publishes;
 *  · it matches nothing — the workspace gets a PRIVATE server of its own holding exactly the
 *    document it was already serving, and connects to that. Nothing is exported, nothing is
 *    shared, and nobody's server quietly becomes somebody else's.
 *
 * The old bytes are left where they are. They are not referenced any more, and deleting somebody's
 * data to tidy up after a schema change is not a trade this makes.
 *
 * Self-healing rather than once-and-done: it considers only bundles with no connection row yet, so
 * a settled database does one indexed read and stops. A bundle whose document cannot be read right
 * now is NAMED in the report and reconsidered on the next boot — unreadable today (a vault still
 * starting up) is not unreadable forever — and it never throws, because a vault that is down must
 * not stop the app from booting.
 */

/** How long one document read may take, and how long the whole sweep may run. */
const DOCUMENT_READ_TIMEOUT_MS = 3_000;
const SWEEP_BUDGET_MS = 10_000;

/** What one pass did — the operator's account of it. */
export interface McpBackfillReport {
  /** Bundles connected to a server already in the global catalog. */
  connected: number;
  /** Bundles that got their workspace a private server holding their own document. */
  privatized: number;
  /** Bundles whose document this tier could not read, named so an operator can look. */
  unreadable: string[];
  /** Bundles whose document today's rules refuse — recorded, never forced through. */
  refused: string[];
  /** Bundles whose workspace is already connected to that server through another bundle. */
  contested: string[];
  /** Bundles the sweep never reached before its budget ran out; left for the next boot. */
  deferred: string[];
}

interface PendingBundle {
  workspace_id: string;
  bundle_id: string;
  name: string;
  display_name: string | null;
  version_id: string;
}

/**
 * Wait for `work`, but not forever — `null` on expiry. A hung `fetch` has nothing to cancel it
 * with, so the promise is abandoned rather than cancelled; this runs once at boot and its whole
 * job is to stop blocking it.
 */
async function withDeadline<T>(work: Promise<T>, ms: number): Promise<T | null> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const expiry = new Promise<null>((resolve) => {
    timer = setTimeout(() => resolve(null), ms);
  });
  try {
    return await Promise.race([work, expiry]);
  } finally {
    if (timer !== undefined) {
      clearTimeout(timer);
    }
  }
}

/** The whole work list in one indexed read: every MCP bundle with a `current` and no connection. */
async function pendingBundles(): Promise<PendingBundle[]> {
  const rows = await getDb().execute(sql`
    SELECT b.workspace_id, b.id AS bundle_id, b.name, b.display_name, cp.version_id
    FROM web.bundle b
    JOIN plane.current_pointer cp
      ON cp.workspace_id = b.workspace_id AND cp.bundle_id = b.id
    WHERE b.kind = 'mcp' AND b.status IN ('active', 'archived')
      AND NOT EXISTS (SELECT 1 FROM web.bundle_mcp m WHERE m.bundle_id = b.id)
    -- Active first: two bundles of one workspace serving one server can hold only ONE connection
    -- between them, and the live one is the one that should have it.
    ORDER BY (b.status = 'active') DESC, b.workspace_id, b.name
  `);
  return rows.rows as unknown as PendingBundle[];
}

/** The public catalog row serving this name, or null. */
async function globalServerNamed(name: string): Promise<string | null> {
  const rows = await getDb().execute(sql`
    SELECT id FROM web.mcp_server
    WHERE workspace_id IS NULL AND status = 'active' AND name = ${name}
    LIMIT 1
  `);
  const row = rows.rows[0] as { id: string } | undefined;
  return row?.id ?? null;
}

/** Connect a bundle to a server that already exists. `false` = its workspace already is. */
async function connectExisting(row: PendingBundle, serverId: string): Promise<boolean> {
  const written = await getDb()
    .insert(bundleMcp)
    .values({
      bundleId: row.bundle_id,
      workspaceId: row.workspace_id,
      serverId,
    })
    .onConflictDoNothing()
    .returning({ bundleId: bundleMcp.bundleId });
  return written[0] !== undefined;
}

/**
 * Give this bundle's workspace a private server holding the document it already serves, and
 * connect it. One transaction: a server with no revision would be a row nobody can receive, and a
 * document today's rules refuse leaves nothing behind at all.
 */
async function privatizeDocument(
  row: PendingBundle,
  document: Record<string, unknown>,
  name: string,
): Promise<"connected" | "refused" | "contested"> {
  const landed = await inFinalTxOrRefusal<"connected", "refused" | "contested">(
    async (tx, stop) => {
      const serverId = mintMcpServerId();
      await tx.insert(mcpServer).values({
        id: serverId,
        workspaceId: row.workspace_id,
        name,
        displayName: row.display_name ?? row.name,
        description: typeof document.description === "string" ? document.description : null,
        // Nothing was ever established about this server's sign-in here, and a migration is not
        // the place to start claiming things about somebody else's endpoint.
        authMode: null,
        status: "active",
      });
      const added = await addMcpRevisionInTx(tx, serverId, { document });
      if (added.refusal !== null) {
        return stop("refused");
      }
      const server = await lockServerInTx(tx, serverId);
      if (server === undefined) {
        return stop("refused");
      }
      // Nobody did this: the same word the birth rows of a workspace use.
      const promoted = await promoteMcpRevisionInTx(tx, server, added.revisionId, "system");
      if (promoted !== null) {
        return stop("refused");
      }
      const written = await tx
        .insert(bundleMcp)
        .values({ bundleId: row.bundle_id, workspaceId: row.workspace_id, serverId })
        .onConflictDoNothing()
        .returning({ bundleId: bundleMcp.bundleId });
      if (written[0] === undefined) {
        return stop("contested");
      }
      return "connected";
    },
  );
  return landed.refused !== null ? landed.refused : landed.value;
}

export async function backfillMcpConnections(): Promise<McpBackfillReport> {
  const report: McpBackfillReport = {
    connected: 0,
    privatized: 0,
    unreadable: [],
    refused: [],
    contested: [],
    deferred: [],
  };
  const deadline = Date.now() + SWEEP_BUDGET_MS;
  for (const row of await pendingBundles()) {
    const where = `${row.workspace_id}/${row.name}`;
    if (Date.now() >= deadline) {
      report.deferred.push(where);
      continue;
    }
    const document = await withDeadline(
      serverDocumentOf(row.workspace_id, row.bundle_id, row.version_id),
      DOCUMENT_READ_TIMEOUT_MS,
    );
    const serverName = typeof document?.name === "string" ? document.name : null;
    if (document === null || serverName === null) {
      report.unreadable.push(where);
      continue;
    }
    const global = await globalServerNamed(serverName);
    if (global !== null) {
      if (await connectExisting(row, global)) {
        report.connected += 1;
      } else {
        report.contested.push(where);
      }
      continue;
    }
    const landed = await privatizeDocument(row, document, serverName);
    if (landed === "connected") {
      report.privatized += 1;
    } else if (landed === "contested") {
      report.contested.push(where);
    } else {
      report.refused.push(where);
    }
  }
  return report;
}

/**
 * Run the backfill and SAY what it could not do.
 *
 * SILENT WHEN IT WORKED, loud and naming names when a bundle was left out: a connection recorded
 * is bookkeeping nobody needs told, while a bundle without one is a server a machine will stop
 * being handed, which somebody does.
 */
export async function backfillMcpConnectionsAtBoot(): Promise<void> {
  let report: McpBackfillReport;
  try {
    report = await backfillMcpConnections();
  } catch (error) {
    // ONE SENTENCE, and the cause on its own line: a thrown query error carries its whole
    // statement and parameters, which turns a boot log into SQL where a description belongs.
    console.warn(
      "[mcp-backfill] the connection backfill did not complete; it runs again on the next boot, " +
        "and until it does the servers it has not reached deliver nothing.",
    );
    console.warn(`[mcp-backfill] cause: ${firstLine(error)}`);
    return;
  }
  for (const name of report.unreadable) {
    console.warn(
      `[mcp-backfill] ${name}: its published server.json could not be read here, so the server it ` +
        "names is NOT recorded and it delivers nothing. Retried on the next boot.",
    );
  }
  for (const name of report.refused) {
    console.warn(
      `[mcp-backfill] ${name}: its document is not one this build can store, so no server was ` +
        "recorded for it. It delivers nothing until somebody republishes it.",
    );
  }
  for (const name of report.contested) {
    console.warn(
      `[mcp-backfill] ${name}: its workspace already reaches that server through another bundle, ` +
        "so this one was left unconnected. Delete it, or point it at a server of its own.",
    );
  }
  for (const name of report.deferred) {
    console.warn(
      `[mcp-backfill] ${name}: the backfill ran out of time before reaching it (the vault was slow ` +
        "or unreachable). Retried on the next boot.",
    );
  }
}

/** An error boiled down to something that belongs on one log line: its first line, bounded. */
function firstLine(error: unknown): string {
  const text = error instanceof Error ? error.message : String(error);
  const line = text.split("\n", 1)[0] ?? "";
  return line.length > 200 ? `${line.slice(0, 200)}…` : line;
}
