import type { ActionFunctionArgs } from "react-router";
import {
  parseBundleKind,
  parseCandidate,
  parsePublishHead,
  receiptNow,
} from "@/lib/api/candidate.server";
import { laneGate } from "@/lib/api/compat.server";
import { publishFlow } from "@/lib/api/publish-flow.server";
import { buildReceipt, deniedEnvelope, envelopeResponse } from "@/lib/api/receipts.server";
import { candidateStoredBytes, storageQuotaRefusal } from "@/lib/api/storage-quota.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActorPreBody } from "@/lib/auth/guards.server";
import { findReceipt } from "@/lib/db/queries.custody.server";

/**
 * `POST /api/v1/proposals` — open a proposal explicitly: the SAME flow as publish with the
 * propose arm forced (a commit-only ingest + the proposal row; `current` never moves;
 * NEEDS_REVIEW). Genesis still lands directly — there is no base to review against.
 */
const BODY_CAP = 160 * 1024 * 1024;

export async function action({ request }: ActionFunctionArgs): Promise<Response> {
  const gated = laneGate(request);
  if (gated !== null) {
    return gated;
  }
  if (request.method !== "POST") {
    return uniformNotFound();
  }
  // AUTH BEFORE THE BODY: the credential resolves first (it is workspace-scoped, so the live
  // session names the one workspace it may act in), so an unauthenticated caller is refused
  // before this tier buffers anything against the large propose cap. The body's workspace is
  // then HELD against the session's below — a mismatch is the same uniform 404 the old
  // workspace-keyed resolve answered.
  const actor = await requireSessionActorPreBody(request);
  const raw = await readCappedBody(request, BODY_CAP, "propose body");
  if (raw instanceof Response) {
    return raw;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return badRequest("malformed JSON body");
  }
  if (typeof parsed !== "object" || parsed === null) {
    return badRequest("malformed propose body");
  }
  const body = parsed as Record<string, unknown>;
  const head = parsePublishHead(body);
  if (typeof head === "string") {
    return badRequest(head);
  }
  const candidate = parseCandidate(body.candidate);
  if (typeof candidate === "string") {
    return badRequest(candidate);
  }
  const kind = parseBundleKind(body.kind);
  if (typeof kind === "string") {
    return badRequest(kind);
  }
  if (head.workspaceId !== actor.workspaceId) {
    return uniformNotFound();
  }

  const replay = await findReceipt(actor, head.opId, raw);
  if (replay.kind === "replay") {
    return envelopeResponse(replay.outcome);
  }
  if (replay.kind === "key_reuse") {
    // Before target resolution: workspace is the actor's, the skill unknown — omit what we
    // cannot honestly name. The write 200 still carries a receipt so the CLI's op-WAL clears.
    const receipt = buildReceipt({
      opId: head.opId,
      command: "publish",
      outcome: "DENIED",
      workspaceId: actor.workspaceId,
      createdAt: receiptNow(),
    });
    return envelopeResponse(deniedEnvelope("publish", "OP_ID_REUSED", undefined, receipt));
  }

  // The per-workspace storage quota — after auth and the replay check (a replayed op re-serves
  // its stored envelope), before any vault ingest; charged at the candidate's DECODED bytes
  // (what custody could at most grow by), never the wire's base64/JSON framing. A no-op
  // without a `storage-bytes` limit.
  const quota = await storageQuotaRefusal(
    actor,
    head.opId,
    "publish",
    candidateStoredBytes(candidate),
  );
  if (quota !== null) {
    return quota;
  }

  return publishFlow({
    actor,
    raw,
    opId: head.opId,
    skillId: head.skillId,
    expected: head.expected,
    candidate,
    displayName: typeof body.display_name === "string" ? body.display_name : null,
    channel: typeof body.channel === "string" ? body.channel : null,
    // Genesis by proposal still registers the bundle (there is no base to review against), so
    // the declared kind rides this arm exactly as it rides a direct publish.
    kind: kind.kind,
    command: "publish",
    forceProposal: true,
  });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
