import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { HEX_64, parsePublishHead, receiptNow } from "@/lib/api/candidate.server";
import {
  buildReceipt,
  conflictEnvelope,
  deniedEnvelope,
  envelopeResponse,
  okPointerEnvelope,
} from "@/lib/api/receipts.server";
import { badRequest, internalError, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { auditInTx } from "@/lib/db/identity.server";
import {
  findReceipt,
  inFinalTx,
  insertReceiptInTx,
  publishTargetOf,
} from "@/lib/db/queries.custody.server";
import { revertPointer } from "@/lib/plane/custody.server";

/**
 * `POST /api/v1/reverts` — the FORWARD revert: the vault constructs a new one-parent commit
 * carrying the `good` version's tree on top of `current` (the generation advances; the pointer
 * never moves backward), CAS-fenced. Reviewer+ — the same decision grade the web's roll-back
 * ceremony earns; a purged target refuses typed.
 */
const BODY_CAP = 64 * 1024;

export async function action({ request }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "POST") {
    return uniformNotFound();
  }
  const raw = await readCappedBody(request, BODY_CAP, "revert body");
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
    return badRequest("malformed revert body");
  }
  const body = parsed as Record<string, unknown>;
  const head = parsePublishHead(body);
  if (typeof head === "string") {
    return badRequest(head);
  }
  const good = body.good;
  const message = typeof body.message === "string" ? body.message : "";
  const author = typeof body.author === "string" ? body.author : "";
  if (typeof good !== "string" || !HEX_64.test(good)) {
    return badRequest("malformed good version id");
  }
  if (author.length === 0) {
    return badRequest("malformed revert author");
  }

  const actor = await requireSessionActor(request, head.workspaceId);
  const replay = await findReceipt(actor, head.opId, raw);
  if (replay.kind === "replay") {
    return envelopeResponse(replay.outcome);
  }
  if (replay.kind === "key_reuse") {
    // Before target resolution: workspace is the actor's, the skill unknown — omit what we
    // cannot honestly name. The write 200 still carries a receipt so the CLI's op-WAL clears.
    const receipt = buildReceipt({
      opId: head.opId,
      command: "revert",
      outcome: "DENIED",
      workspaceId: actor.workspaceId,
      createdAt: receiptNow(),
    });
    return envelopeResponse(deniedEnvelope("revert", "OP_ID_REUSED", undefined, receipt));
  }

  const createdAt = receiptNow();
  const target = await publishTargetOf(actor, head.skillId);
  if (target === undefined) {
    return uniformNotFound();
  }
  const receiptBase = {
    opId: head.opId,
    command: "revert",
    workspaceId: actor.workspaceId,
    skillId: target.bundleId,
    expectedGeneration: head.expected,
    createdAt,
  };
  if (target.status !== "active") {
    const envelope = deniedEnvelope(
      "revert",
      "SKILL_NOT_ACTIVE",
      target.name,
      buildReceipt({ ...receiptBase, outcome: "DENIED" }),
    );
    await inFinalTx((tx) => insertReceiptInTx(tx, actor, head.opId, raw, envelope));
    return envelopeResponse(envelope);
  }
  if (actor.role === "member") {
    const envelope = deniedEnvelope(
      "revert",
      "REVIEWER_ROLE_REQUIRED",
      target.name,
      buildReceipt({ ...receiptBase, outcome: "DENIED" }),
    );
    await inFinalTx((tx) => insertReceiptInTx(tx, actor, head.opId, raw, envelope));
    return envelopeResponse(envelope);
  }

  // I-COMMIT-PARITY: the forward-commit frame's inputs are the WIRE's — the device pre-derived
  // the forward id from `(parents, tree, author, message)` and will verify the served pointer
  // names exactly that version, so the custody lane records the wire's author + message
  // verbatim (a substituted display string would derive a different id and the client would
  // refuse the OK).
  const reverted = await revertPointer(actor.workspaceId, target.bundleId, {
    to_version_id: good,
    expected_generation: head.expected,
    attribution: author,
    message,
  });
  if (reverted.kind === "conflict") {
    const receipt = buildReceipt({
      ...receiptBase,
      outcome: "CONFLICT",
      currentGeneration: reverted.generation,
    });
    const envelope = conflictEnvelope({
      command: "revert",
      skillName: target.name,
      receipt,
      expectedGeneration: head.expected,
      currentGeneration: reverted.generation ?? 0,
    });
    await inFinalTx((tx) => insertReceiptInTx(tx, actor, head.opId, raw, envelope));
    return envelopeResponse(envelope);
  }
  if (reverted.kind === "not_found" || reverted.kind === "target_purged") {
    const code = reverted.kind === "target_purged" ? "TARGET_PURGED" : "UNKNOWN_VERSION";
    const envelope = deniedEnvelope(
      "revert",
      code,
      target.name,
      buildReceipt({ ...receiptBase, outcome: "DENIED" }),
    );
    await inFinalTx((tx) => insertReceiptInTx(tx, actor, head.opId, raw, envelope));
    return envelopeResponse(envelope);
  }
  if (reverted.kind !== "ok") {
    return internalError();
  }
  const envelope = await inFinalTx(async (tx) => {
    await auditInTx(tx, {
      workspaceId: actor.workspaceId,
      actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
      kind: "revert",
      subject: target.bundleId,
      outcome: "ok",
      details: { good, versionId: reverted.value.version_id },
    });
    const receipt = buildReceipt({
      ...receiptBase,
      outcome: "OK",
      versionId: reverted.value.version_id,
      bundleDigest: reverted.value.bundle_digest,
      currentGeneration: reverted.value.pointer.generation,
    });
    // The pointer MOVED — the envelope's data carries the current record (the client's
    // read-your-writes advance requires it and scope-checks it).
    const built = okPointerEnvelope(
      "revert",
      receipt,
      { workspaceId: actor.workspaceId, skillId: target.bundleId },
      { versionId: reverted.value.version_id, generation: reverted.value.pointer.generation },
    );
    await insertReceiptInTx(tx, actor, head.opId, raw, built);
    return built;
  });
  return envelopeResponse(envelope);
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
