import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { HEX_64, parsePublishHead, receiptNow } from "@/lib/api/candidate.server";
import {
  buildReceipt,
  conflictEnvelope,
  deniedEnvelope,
  envelopeResponse,
  okPointerEnvelope,
  okReceiptEnvelope,
} from "@/lib/api/receipts.server";
import { badRequest, internalError, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import {
  findReceipt,
  inFinalTx,
  insertReceiptInTx,
  lockOpenProposalInTx,
  publishTargetOf,
  resolveProposalInTx,
} from "@/lib/db/queries.custody.server";
import { proposalByCandidate } from "@/lib/db/queries.server";
import { movePointer, purgeVersionBytes } from "@/lib/plane/custody.server";

/**
 * `POST /api/v1/reviews` — a governance decision on an open proposal, app-authorized:
 * `approve` (reviewer+; four-eyes under review-required — the proposer may not self-approve)
 * CAS-moves the pointer onto the candidate, resolves the row, and notifies the proposer;
 * `reject` (reviewer+, reason mandatory) resolves the row + the verdict notice and best-effort
 * purges the candidate's bytes; `withdraw` is the AUTHOR retracting their own open proposal
 * (idempotent; no notice — the author did it).
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
  const raw = await readCappedBody(request, BODY_CAP, "review body");
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
    return badRequest("malformed review body");
  }
  const body = parsed as Record<string, unknown>;
  const head = parsePublishHead(body);
  if (typeof head === "string") {
    return badRequest(head);
  }
  const proposalId = body.proposal;
  const decision = body.decision;
  const reason = typeof body.reason === "string" ? body.reason.trim() : "";
  if (typeof proposalId !== "string" || !HEX_64.test(proposalId)) {
    return badRequest("malformed proposal id");
  }
  if (decision !== "approve" && decision !== "reject" && decision !== "withdraw") {
    return badRequest("malformed review decision");
  }
  if (decision === "reject" && reason.length === 0) {
    return badRequest("a reject carries its reason back to the author");
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
      command: "review",
      outcome: "DENIED",
      workspaceId: actor.workspaceId,
      createdAt: receiptNow(),
    });
    return envelopeResponse(deniedEnvelope("review", "OP_ID_REUSED", undefined, receipt));
  }

  const createdAt = receiptNow();
  const target = await publishTargetOf(actor, head.skillId);
  if (target === undefined) {
    return uniformNotFound();
  }
  const receiptBase = {
    opId: head.opId,
    command: "review",
    workspaceId: actor.workspaceId,
    skillId: target.bundleId,
    versionId: proposalId,
    expectedGeneration: head.expected,
    createdAt,
  };

  if (decision !== "withdraw" && actor.role === "member") {
    const envelope = deniedEnvelope(
      "review",
      "REVIEWER_ROLE_REQUIRED",
      target.name,
      buildReceipt({ ...receiptBase, outcome: "DENIED" }),
    );
    await inFinalTx((tx) => insertReceiptInTx(tx, actor, head.opId, raw, envelope));
    return envelopeResponse(envelope);
  }

  if (decision === "approve") {
    // Four-eyes under review-required: the proposer may not approve their own proposal.
    const row = await proposalByCandidate(actor, target.bundleId, proposalId);
    if (row === undefined || row.status !== "open") {
      const envelope = deniedEnvelope(
        "review",
        "NO_OPEN_PROPOSAL",
        target.name,
        buildReceipt({ ...receiptBase, outcome: "DENIED" }),
      );
      await inFinalTx((tx) => insertReceiptInTx(tx, actor, head.opId, raw, envelope));
      return envelopeResponse(envelope);
    }
    if (target.protection === "reviewed" && row.proposedBy === actor.userId) {
      const envelope = deniedEnvelope(
        "review",
        "FOUR_EYES_REQUIRED",
        target.name,
        buildReceipt({ ...receiptBase, outcome: "DENIED" }),
      );
      await inFinalTx((tx) => insertReceiptInTx(tx, actor, head.opId, raw, envelope));
      return envelopeResponse(envelope);
    }
    const moved = await movePointer(actor.workspaceId, target.bundleId, {
      version_id: proposalId,
      expected_generation: head.expected,
      attribution: actor.display,
    });
    if (moved.kind === "not_found") {
      // The candidate's bytes are gone (purged/reclaimed) — the proposal cannot promote.
      const envelope = deniedEnvelope(
        "review",
        "UNKNOWN_VERSION",
        target.name,
        buildReceipt({ ...receiptBase, outcome: "DENIED" }),
      );
      await inFinalTx((tx) => insertReceiptInTx(tx, actor, head.opId, raw, envelope));
      return envelopeResponse(envelope);
    }
    if (moved.kind === "conflict") {
      const receipt = buildReceipt({
        ...receiptBase,
        outcome: "CONFLICT",
        currentGeneration: moved.generation,
      });
      const envelope = conflictEnvelope({
        command: "review",
        skillName: target.name,
        receipt,
        expectedGeneration: head.expected,
        currentGeneration: moved.generation ?? 0,
      });
      await inFinalTx((tx) => insertReceiptInTx(tx, actor, head.opId, raw, envelope));
      return envelopeResponse(envelope);
    }
    if (moved.kind !== "ok") {
      return internalError();
    }
    const envelope = await inFinalTx(async (tx) => {
      const locked = await lockOpenProposalInTx(tx, actor.workspaceId, target.bundleId, proposalId);
      if (locked !== undefined) {
        await resolveProposalInTx(tx, actor, locked, "approved", null);
      }
      const receipt = buildReceipt({
        ...receiptBase,
        outcome: "OK",
        currentGeneration: moved.value.generation,
      });
      // An approve MOVED the pointer — the envelope's data carries the current record (the
      // client's read-your-writes advance requires it and scope-checks it).
      const built = okPointerEnvelope(
        "review",
        receipt,
        { workspaceId: actor.workspaceId, skillId: target.bundleId },
        { versionId: moved.value.version_id, generation: moved.value.generation },
      );
      await insertReceiptInTx(tx, actor, head.opId, raw, built);
      return built;
    });
    return envelopeResponse(envelope);
  }

  // reject | withdraw — standalone status flips, no pointer move.
  const envelope = await inFinalTx(async (tx) => {
    const locked = await lockOpenProposalInTx(tx, actor.workspaceId, target.bundleId, proposalId);
    if (locked === undefined) {
      if (decision === "withdraw") {
        // Idempotent re-withdraw: an already-withdrawn candidate answers OK again.
        const resolved = await proposalByCandidate(actor, target.bundleId, proposalId);
        if (resolved !== undefined && resolved.status === "withdrawn") {
          const receipt = buildReceipt({ ...receiptBase, outcome: "OK" });
          const built = okReceiptEnvelope("review", receipt);
          await insertReceiptInTx(tx, actor, head.opId, raw, built);
          return built;
        }
      }
      const built = deniedEnvelope(
        "review",
        "NO_OPEN_PROPOSAL",
        target.name,
        buildReceipt({ ...receiptBase, outcome: "DENIED" }),
      );
      await insertReceiptInTx(tx, actor, head.opId, raw, built);
      return built;
    }
    if (decision === "withdraw" && locked.proposedBy !== actor.userId) {
      const built = deniedEnvelope(
        "review",
        "AUTHOR_ONLY",
        target.name,
        buildReceipt({ ...receiptBase, outcome: "DENIED" }),
      );
      await insertReceiptInTx(tx, actor, head.opId, raw, built);
      return built;
    }
    await resolveProposalInTx(
      tx,
      actor,
      locked,
      decision === "withdraw" ? "withdrawn" : "rejected",
      decision === "reject" ? reason : null,
    );
    const receipt = buildReceipt({ ...receiptBase, outcome: "OK" });
    const built = okReceiptEnvelope("review", receipt);
    await insertReceiptInTx(tx, actor, head.opId, raw, built);
    return built;
  });
  if (decision === "reject") {
    // Best-effort byte reclaim of the rejected candidate — the record (the row + notice)
    // already stands; a custody fault here changes nothing the reviewer decided.
    void purgeVersionBytes(actor.workspaceId, target.bundleId, proposalId, actor.display).catch(
      () => {},
    );
  }
  return envelopeResponse(envelope);
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
