import { composition } from "@/composition.server";
import { receiptNow } from "@/lib/api/candidate.server";
import { buildReceipt, deniedEnvelope, envelopeResponse } from "@/lib/api/receipts.server";
import type { SessionActor } from "@/lib/auth/guards.server";
import type { McpGateRefusal } from "@/lib/mcp/publish-gate.server";
import { workspaceStoredBytes } from "@/lib/plane/storage.server";

/**
 * The per-workspace STORAGE quota (`storage-bytes`) — ONE admission check for EVERY custody
 * ingest door: the session lane's publish/propose routes, the shared genesis path all three
 * creation doors run (so add-from-GitHub and add-an-MCP-server cannot ingest around it), and
 * the upstream checker's imports. A no-op without a limit (the OSS default — the stat is not
 * even read). With one: stored + the candidate's decoded bytes over the limit refuses; a
 * failed stat read ALLOWS (fail-open — the ingest shares the same backend and fails on a real
 * outage; the skip is logged in storage.server.ts).
 *
 * ADMISSION, not accounting: the stat is a pre-ingest read, so concurrent ingests that all
 * pass it can overshoot the limit by at most the sum of their in-flight candidates — bounded
 * by the body cap per request — and the very next stat read enforces again. An exact
 * reservation would have to live inside the vault's ingest itself; the vault is deliberately
 * policy-free, so this tier holds the line at bounded-overshoot admission.
 */

/** The one refusal every non-route door renders (the same code the route envelope carries). */
const STORAGE_LIMIT_REFUSAL: McpGateRefusal = {
  code: "STORAGE_LIMIT_REACHED",
  message: "Storage limit reached for this workspace.",
};

/**
 * What a candidate would add to custody AT MOST: the decoded size of every file it carries,
 * computed arithmetically off the base64 lengths (never by materializing the bytes — the
 * publish cap is large). This is the honest admission charge: the quota is defined over
 * STORED custody bytes, so the wire's base64 expansion and JSON framing must not count
 * against it. Still conservative — content-addressed dedup only ever stores less than this
 * (an unchanged file adds nothing), and only the vault could price that exactly.
 */
export function candidateStoredBytes(candidate: {
  files: readonly { content_base64: string }[];
}): number {
  let total = 0;
  for (const file of candidate.files) {
    const b64 = file.content_base64;
    const padding = b64.endsWith("==") ? 2 : b64.endsWith("=") ? 1 : 0;
    total += Math.max(0, Math.floor((b64.length / 4) * 3) - padding);
  }
  return total;
}

/** The admission decision itself — shared by every door below. */
export async function storageCapExceeded(workspaceId: string, addBytes: number): Promise<boolean> {
  const entitlements = await composition.entitlements.forWorkspace(workspaceId);
  const limit = entitlements.limit("storage-bytes");
  if (limit === null) {
    return false;
  }
  const stored = await workspaceStoredBytes(workspaceId);
  return stored !== null && stored + addBytes > limit;
}

/**
 * The quota as the GENESIS path's refusal shape — the same typed answer every genesis door
 * already renders, decided before any custody call so a refusal leaves no ingested bytes.
 */
export async function storageCapRefusalForIngest(
  workspaceId: string,
  addBytes: number,
): Promise<McpGateRefusal | null> {
  return (await storageCapExceeded(workspaceId, addBytes)) ? STORAGE_LIMIT_REFUSAL : null;
}

/**
 * The quota as the SESSION LANE's wire answer (publish/propose routes), consulted after auth
 * and the replay check. Deliberately NOT persisted as an op receipt: the refusal reflects the
 * workspace's standing limit, and the same op retried under a wider limit must re-evaluate,
 * not replay.
 */
export async function storageQuotaRefusal(
  actor: SessionActor,
  opId: string,
  command: string,
  requestBytes: number,
): Promise<Response | null> {
  if (!(await storageCapExceeded(actor.workspaceId, requestBytes))) {
    return null;
  }
  const receipt = buildReceipt({
    opId,
    command,
    outcome: "DENIED",
    workspaceId: actor.workspaceId,
    createdAt: receiptNow(),
  });
  return envelopeResponse(
    deniedEnvelope(command, STORAGE_LIMIT_REFUSAL.code, undefined, receipt, {
      message: STORAGE_LIMIT_REFUSAL.message,
    }),
  );
}
