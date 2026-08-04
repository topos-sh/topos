import type { WireCandidate } from "@/lib/api/candidate.server";
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
import type { SessionActor } from "@/lib/auth/guards.server";
import {
  bundleIdHeldElsewhere,
  inFinalTx,
  insertReceiptInTx,
  openProposalInTx,
  placeIntoChannelInTx,
  publishTargetOf,
  registerGenesisBundleInTx,
} from "@/lib/db/queries.custody.server";
import { mcpCandidateRefusal } from "@/lib/mcp/publish-gate.server";
import { commitVersion, publishVersion } from "@/lib/plane/custody.server";

/**
 * The shared publish/propose ORCHESTRATION — the one flow behind `POST /v1/publish` (with the
 * protection gate that REROUTES a member's direct publish on an effectively-'reviewed' bundle
 * into a proposal, answering NEEDS_REVIEW with the `downgraded` detail) and `POST
 * /v1/proposals` (the explicit propose — the same flow with the reroute forced).
 *
 * Sequence per op: the caller already authenticated the device and replayed the op receipt;
 * this flow (c) resolves the protection gate, the kind gate, and — for an MCP bundle — the
 * server-document gate (app/lib/mcp/publish-gate.server.ts), (d) registers a GENESIS bundle
 * (server-minted id + birth name + birth KIND + `everyone`/`--to` placement + the author
 * self-follow), (e) makes the vault call, and (f) lands the final web transaction —
 * registration/placement/proposal/audit writes + the op receipt carrying the terminal envelope
 * verbatim. Genesis registration happens on ONE path — the direct arm below, which a forced
 * proposal also takes when no bundle is registered yet (there is no base to review against) —
 * so the declared kind rides publish and propose identically. The vault call carries the ACTOR's
 * display as the attribution (the vault stores display strings, never identities).
 */
export interface PublishFlowArgs {
  actor: SessionActor;
  /** The raw request body — the receipt slot's identity input (hashed in Postgres). */
  raw: string;
  opId: string;
  skillId: string;
  expected: number;
  candidate: WireCandidate;
  displayName: string | null;
  channel: string | null;
  /** The catalog kind a GENESIS publish declares ('mcp' for an MCP-server bundle; null ⇒
   * 'skill'). On an already-registered bundle it is only ever a re-assertion: a value that
   * differs from the stored kind refuses the op before any byte moves. */
  kind?: string | null;
  /** The envelope command (`publish` on both arms — the CLI verb). */
  command: string;
  /** True on the explicit `POST /v1/proposals` arm — always commit-only. */
  forceProposal: boolean;
  /** The upstream provenance the publish carries, when the bytes were imported. */
  upstream?: UpstreamInput | null;
}

/** The parsed upstream-provenance block a publish may carry. */
export interface UpstreamInput {
  host: string;
  repo: string;
  path: string;
  commit: string | null;
  license: string | null;
}

/** Record a publish's upstream provenance inside the final transaction: the bundle's ONE
 * upstream row (upserted — the latest publish's word wins) and, when a commit is named, the
 * per-version record divergence is read from.
 *
 * The upsert's arbiter is the bundle id ALONE (the row's primary key), so its conflict target
 * spans the whole catalog rather than this workspace's slice: the `DO UPDATE` therefore carries
 * a workspace guard, or a caller naming a foreign bundle id would rewrite that workspace's
 * upstream pointer in place — the composite FK stays satisfied, because the update never
 * touches `workspace_id`. The genesis arm refuses a foreign id outright (see
 * `bundleIdHeldElsewhere`); this guard is the belt below that brace. */
async function recordUpstreamInTx(
  tx: Parameters<Parameters<typeof inFinalTx>[0]>[0],
  ws: string,
  bundleId: string,
  versionId: string,
  upstream: UpstreamInput,
): Promise<void> {
  const { sql } = await import("drizzle-orm");
  await tx.execute(sql`
    INSERT INTO web.bundle_upstream (bundle_id, workspace_id, host, repo, path, license)
    VALUES (${bundleId}, ${ws}, ${upstream.host}, ${upstream.repo}, ${upstream.path},
            ${upstream.license})
    ON CONFLICT (bundle_id) DO UPDATE
      SET host = excluded.host, repo = excluded.repo, path = excluded.path,
          license = COALESCE(excluded.license, web.bundle_upstream.license)
      WHERE web.bundle_upstream.workspace_id = excluded.workspace_id
  `);
  if (upstream.commit !== null) {
    await tx.execute(sql`
      INSERT INTO web.version_upstream (workspace_id, bundle_id, version_id, commit)
      VALUES (${ws}, ${bundleId}, ${versionId}, ${upstream.commit})
      ON CONFLICT (bundle_id, version_id) DO NOTHING
    `);
  }
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

  // A genesis publish keeps the client-supplied id, and bundle ids are unique catalog-wide — so
  // an id already held by ANOTHER workspace is refused before anything is written or ingested.
  // The uniform 404 is byte-identical to an unknown workspace, so this is no existence oracle.
  if (isGenesis && (await bundleIdHeldElsewhere(actor, skillId))) {
    return uniformNotFound();
  }

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

  // KIND IS BIRTH METADATA: a bundle's kind is fixed at genesis, so a publish that names a
  // DIFFERENT one is describing a different thing under an existing identity — refused here,
  // BEFORE any custody call, so no bytes are ingested against the mismatch (a silent accept
  // would leave the catalog saying one thing and the bundle being another). Naming the SAME
  // kind, or naming none, is a no-op.
  if (target !== undefined && args.kind != null && args.kind !== target.kind) {
    const receipt = buildReceipt({
      opId,
      command,
      outcome: "DENIED",
      workspaceId: actor.workspaceId,
      skillId: target.bundleId,
      expectedGeneration: expected,
      createdAt,
    });
    const envelope = deniedEnvelope(command, "KIND_MISMATCH", target.name, receipt);
    await inFinalTx((tx) => insertReceiptInTx(tx, actor, opId, raw, envelope));
    return envelopeResponse(envelope);
  }

  // THE MCP GATE: when the bundle IS (or is being born as) an MCP server, the candidate's
  // `server.json` decides whether it may land at all — a remote https endpoint, no
  // per-installation template, no credential, and an embedded registry name no other bundle
  // here already claims. Placed beside the kind gate and for the same reason: a refusal must
  // leave no ingested bytes behind, so it answers BEFORE any custody call. Every other kind
  // passes straight through — this is the one branch on the catalog tag.
  const effectiveKind = target?.kind ?? args.kind ?? "skill";
  if (effectiveKind === "mcp") {
    const refusal = await mcpCandidateRefusal(actor, candidate.files, target?.bundleId ?? skillId);
    if (refusal !== null) {
      const receipt = buildReceipt({
        opId,
        command,
        outcome: "DENIED",
        workspaceId: actor.workspaceId,
        skillId: target?.bundleId ?? skillId,
        expectedGeneration: expected,
        createdAt,
      });
      const envelope = deniedEnvelope(command, refusal.code, skillName, receipt, {
        message: refusal.message,
      });
      await inFinalTx((tx) => insertReceiptInTx(tx, actor, opId, raw, envelope));
      return envelopeResponse(envelope);
    }
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
    // The propose arm: commit-only ingest, then the proposal row — `current` never moves. A
    // `--to` placement applies HERE too (the bundle is registered; a channel reference is
    // reach, and delivery starts only when a `current` exists) — mode-gated and
    // existing-channels-only, exactly like the direct arm, its outcome riding the receipt.
    const committed = await commitVersion(actor.workspaceId, target.bundleId, laneCandidate);
    if (committed.kind === "rejected") {
      return badRequest(committed.message ?? "candidate rejected");
    }
    if (committed.kind !== "ok") {
      return internalError();
    }
    const envelope = await inFinalTx(async (tx) => {
      await openProposalInTx(tx, actor, target.bundleId, committed.value.version_id);
      const details: Record<string, unknown> = args.forceProposal ? {} : { downgraded: true };
      if (channel !== null) {
        details.placement = await placeIntoChannelInTx(tx, actor, target.bundleId, channel);
      }
      if (args.upstream != null) {
        await recordUpstreamInTx(
          tx,
          actor.workspaceId,
          target.bundleId,
          committed.value.version_id,
          args.upstream,
        );
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
        ...(Object.keys(details).length === 0 ? {} : { details }),
      });
      const env = okReceiptEnvelope(command, receipt);
      await insertReceiptInTx(tx, actor, opId, raw, env);
      return env;
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
        args.kind ?? null,
      );
      if (registration.placement !== undefined) {
        details.placement = registration.placement;
      }
    } else if (channel !== null) {
      details.placement = await placeIntoChannelInTx(tx, actor, bundleId, channel);
    }
    if (args.upstream != null) {
      await recordUpstreamInTx(
        tx,
        actor.workspaceId,
        bundleId,
        published.value.version_id,
        args.upstream,
      );
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
