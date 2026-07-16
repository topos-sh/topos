import type { DeviceCandidate } from "@/lib/api/candidate.server";
import { receiptNow } from "@/lib/api/candidate.server";
import {
  buildReceipt,
  conflictEnvelope,
  deniedEnvelope,
  envelopeResponse,
  okPointerEnvelope,
  okReceiptEnvelope,
} from "@/lib/api/receipts.server";
import { badRequest, internalError, uniformNotFound } from "@/lib/api/wire.server";
import type { DeviceActor } from "@/lib/auth/guards.server";
import {
  inFinalTx,
  insertReceiptInTx,
  openProposalInTx,
  placeIntoChannelInTx,
  publishTargetOf,
  registerGenesisBundleInTx,
} from "@/lib/db/queries.custody.server";
import { commitVersion, publishVersion } from "@/lib/plane/custody.server";

/**
 * The shared publish/propose ORCHESTRATION — the one flow behind `POST /v1/publish` (with the
 * protection gate that REROUTES a member's direct publish on an effectively-'reviewed' bundle
 * into a proposal, answering NEEDS_REVIEW with the `downgraded` detail) and `POST
 * /v1/proposals` (the explicit propose — the same flow with the reroute forced).
 *
 * Sequence per op: the caller already authenticated the device and replayed the op receipt;
 * this flow (c) resolves the protection gate, (d) registers a GENESIS bundle (server-minted
 * id + birth name + `everyone`/`--to` placement + the author self-follow), (e) makes the vault
 * call, and (f) lands the final web transaction — registration/placement/proposal/audit writes
 * + the op receipt carrying the terminal envelope verbatim. The vault call carries the ACTOR's
 * display as the attribution (the vault stores display strings, never identities).
 */
export interface PublishFlowArgs {
  actor: DeviceActor;
  /** The raw request body — the receipt slot's identity input (hashed in Postgres). */
  raw: string;
  opId: string;
  skillId: string;
  expected: number;
  candidate: DeviceCandidate;
  displayName: string | null;
  channel: string | null;
  /** The envelope command (`publish` on both arms — the CLI verb). */
  command: string;
  /** True on the explicit `POST /v1/proposals` arm — always commit-only. */
  forceProposal: boolean;
}

export async function publishFlow(args: PublishFlowArgs): Promise<Response> {
  const { actor, raw, opId, skillId, expected, candidate, displayName, channel, command } = args;
  if (candidate.parents.length > 1) {
    // Two-parent author merges stay unaccepted (the custody lane commits one parent).
    return badRequest("two-parent author merges are not accepted");
  }
  const createdAt = receiptNow();
  const target = await publishTargetOf(actor, skillId);
  const isGenesis = target === undefined;
  const skillName = target?.name;

  if (target !== undefined && target.status !== "active") {
    const receipt = buildReceipt({
      opId,
      command,
      outcome: "DENIED",
      workspaceId: actor.workspaceId,
      skillId: target.bundleId,
      expectedGeneration: expected,
      createdAt,
    });
    const envelope = deniedEnvelope(command, "SKILL_NOT_ACTIVE", target.name, receipt);
    await inFinalTx((tx) => insertReceiptInTx(tx, actor, opId, raw, envelope));
    return envelopeResponse(envelope);
  }

  // I-COMMIT-PARITY: the commit frame's author + message are the WIRE's — the device derived
  // the candidate's version id from `(parents, tree, author, message)` and verifies the served
  // outcome names exactly that version, so the custody lane must record them verbatim (the
  // actor's display string would derive a DIFFERENT id and the client would refuse the OK).
  const laneCandidate = {
    files: candidate.files,
    ...(candidate.parents[0] === undefined ? {} : { parent: candidate.parents[0] }),
    attribution: candidate.author,
    message: candidate.message,
  };

  // Genesis always lands directly (there is no base to review against); the gate reroutes
  // only a REGISTERED bundle's member publish.
  const reroute =
    target !== undefined &&
    (args.forceProposal || (target.protection === "reviewed" && actor.role === "member"));

  if (reroute && target !== undefined) {
    // The propose arm: commit-only ingest, then the proposal row — `current` never moves.
    const committed = await commitVersion(actor.workspaceId, target.bundleId, laneCandidate);
    if (committed.kind === "rejected") {
      return badRequest(committed.message ?? "candidate rejected");
    }
    if (committed.kind !== "ok") {
      return internalError();
    }
    const receipt = buildReceipt({
      opId,
      command,
      outcome: "NEEDS_REVIEW",
      workspaceId: actor.workspaceId,
      skillId: target.bundleId,
      versionId: committed.value.version_id,
      bundleDigest: committed.value.bundle_digest,
      expectedGeneration: expected,
      createdAt,
      ...(args.forceProposal ? {} : { details: { downgraded: true } }),
    });
    const envelope = okReceiptEnvelope(command, receipt);
    await inFinalTx(async (tx) => {
      await openProposalInTx(tx, actor, target.bundleId, committed.value.version_id);
      await insertReceiptInTx(tx, actor, opId, raw, envelope);
    });
    return envelopeResponse(envelope);
  }

  // The direct arm (reviewer+, an open bundle, or genesis). A genesis bundle keeps the
  // CLIENT-SUPPLIED id (WIRE_ID-validated at the door): the author's install keys every
  // subsequent read, publish CAS, and delivery entry on the id it minted at `add` — a
  // server-minted replacement would orphan the author's own copy (their v2 would re-register
  // a duplicate bundle instead of advancing this one).
  const bundleId = isGenesis ? skillId : (target?.bundleId ?? skillId);
  const published = await publishVersion(actor.workspaceId, bundleId, {
    ...laneCandidate,
    ...(isGenesis ? {} : { expected_generation: expected }),
  });
  if (published.kind === "rejected") {
    return badRequest(published.message ?? "candidate rejected");
  }
  if (published.kind === "not_found") {
    return uniformNotFound();
  }
  if (published.kind === "conflict") {
    const receipt = buildReceipt({
      opId,
      command,
      outcome: "CONFLICT",
      workspaceId: actor.workspaceId,
      skillId: bundleId,
      expectedGeneration: expected,
      currentGeneration: published.generation,
      createdAt,
    });
    const envelope = conflictEnvelope({
      command,
      skillName: skillName ?? skillId,
      receipt,
      expectedGeneration: expected,
      currentGeneration: published.generation ?? 0,
    });
    await inFinalTx((tx) => insertReceiptInTx(tx, actor, opId, raw, envelope));
    return envelopeResponse(envelope);
  }
  if (published.kind !== "ok") {
    return internalError();
  }

  const details: Record<string, unknown> = {};
  const envelope = await inFinalTx(async (tx) => {
    if (isGenesis) {
      const registration = await registerGenesisBundleInTx(
        tx,
        actor,
        bundleId,
        displayName,
        channel,
      );
      if (registration.placement !== undefined) {
        details.placement = registration.placement;
      }
    } else if (channel !== null) {
      details.placement = await placeIntoChannelInTx(tx, actor, bundleId, channel);
    }
    const receipt = buildReceipt({
      opId,
      command,
      outcome: "OK",
      workspaceId: actor.workspaceId,
      skillId: bundleId,
      versionId: published.value.version_id,
      bundleDigest: published.value.bundle_digest,
      ...(isGenesis ? {} : { expectedGeneration: expected }),
      currentGeneration: published.value.pointer.generation,
      createdAt,
      ...(Object.keys(details).length > 0 ? { details } : {}),
    });
    // The pointer MOVED — the envelope's data carries the current record (the client's
    // read-your-writes advance requires it and scope-checks it).
    const built = okPointerEnvelope(
      command,
      receipt,
      { workspaceId: actor.workspaceId, skillId: bundleId },
      {
        versionId: published.value.version_id,
        generation: published.value.pointer.generation,
      },
    );
    await insertReceiptInTx(tx, actor, opId, raw, built);
    return built;
  });
  return envelopeResponse(envelope);
}
