/**
 * The custody-op envelope builders — the `JsonEnvelope` + canonical `Receipt` bodies the
 * publish-family routes answer with, byte-shaped like the committed contract fixtures. Pure
 * construction; the op-receipt persistence lives in the DAL (queries.custody.server.ts) and
 * stores the WHOLE envelope, so a replay re-serves these bytes verbatim.
 */

import { type NextAction, nextAction } from "./next-actions.server";

const WIRE_SCHEMA_VERSION = 1;
const JSON_HEADERS = { "content-type": "application/json" } as const;

export interface ReceiptShape {
  schema_version: number;
  op_id: string;
  command: string;
  outcome: string;
  workspace_id: string;
  skill_id?: string;
  version_id?: string;
  bundle_digest?: string;
  expected_generation?: number;
  current_generation?: number;
  created_at: string;
  details?: Record<string, unknown>;
}

export interface ReceiptInput {
  opId: string;
  command: string;
  outcome: string;
  workspaceId: string;
  skillId?: string;
  versionId?: string;
  bundleDigest?: string;
  expectedGeneration?: number;
  currentGeneration?: number;
  createdAt: string;
  details?: Record<string, unknown>;
}

export function buildReceipt(input: ReceiptInput): ReceiptShape {
  return {
    schema_version: WIRE_SCHEMA_VERSION,
    op_id: input.opId,
    command: input.command,
    outcome: input.outcome,
    workspace_id: input.workspaceId,
    ...(input.skillId === undefined ? {} : { skill_id: input.skillId }),
    ...(input.versionId === undefined ? {} : { version_id: input.versionId }),
    ...(input.bundleDigest === undefined ? {} : { bundle_digest: input.bundleDigest }),
    ...(input.expectedGeneration === undefined
      ? {}
      : { expected_generation: input.expectedGeneration }),
    ...(input.currentGeneration === undefined
      ? {}
      : { current_generation: input.currentGeneration }),
    created_at: input.createdAt,
    ...(input.details === undefined ? {} : { details: input.details }),
  };
}

interface ErrorShape {
  code: string;
  outcome: string;
  retryable: boolean;
  affected: Record<string, unknown>;
  expected_generation?: number;
  current_generation?: number;
  context: Record<string, unknown>;
  next_actions: NextAction[];
}

/** An OK envelope carrying its receipt (`data` stays `{}` when no pointer moved). */
export function okReceiptEnvelope(command: string, receipt: ReceiptShape): Record<string, unknown> {
  return {
    schema_version: WIRE_SCHEMA_VERSION,
    command,
    ok: true,
    data: {},
    warnings: [],
    next_actions: [],
    receipt,
  };
}

/**
 * An OK envelope whose `data` carries the moved `current` pointer record (the frozen
 * `WireCurrentRecord` shape — the same body the `/current` read serves). Every write that MOVES
 * the pointer (a direct publish, a revert, a review approve) must answer with it: the client's
 * read-your-writes advance scope-checks this record and refuses an OK that carries none.
 */
export function okPointerEnvelope(
  command: string,
  receipt: ReceiptShape,
  scope: { workspaceId: string; skillId: string },
  record: { versionId: string; generation: number },
): Record<string, unknown> {
  return {
    schema_version: WIRE_SCHEMA_VERSION,
    command,
    ok: true,
    data: {
      schema_version: WIRE_SCHEMA_VERSION,
      scope: { workspace_id: scope.workspaceId, skill_id: scope.skillId },
      record: { version_id: record.versionId, generation: record.generation },
    },
    warnings: [],
    next_actions: [],
    receipt,
  };
}

/** A failed envelope: the receipt (when the op minted one) + the flat error. */
export function errorReceiptEnvelope(
  command: string,
  error: ErrorShape,
  receipt?: ReceiptShape,
): Record<string, unknown> {
  return {
    schema_version: WIRE_SCHEMA_VERSION,
    command,
    ok: false,
    data: {},
    warnings: [],
    next_actions: error.next_actions,
    ...(receipt === undefined ? {} : { receipt }),
    error,
  };
}

/** The stale-CAS CONFLICT envelope (the `REBASE_AND_RETRY` action rides both halves). */
export function conflictEnvelope(args: {
  command: string;
  skillName: string;
  receipt: ReceiptShape;
  expectedGeneration: number;
  currentGeneration: number;
}): Record<string, unknown> {
  const retry: NextAction = nextAction("REBASE_AND_RETRY", ["topos", "publish", args.skillName]);
  return errorReceiptEnvelope(
    args.command,
    {
      code: "STALE_BASE",
      outcome: "CONFLICT",
      retryable: false,
      affected: { skill: args.skillName },
      expected_generation: args.expectedGeneration,
      current_generation: args.currentGeneration,
      context: {},
      next_actions: [retry],
    },
    args.receipt,
  );
}

/** A typed DENIED envelope (role gates, lifecycle refusals, key reuse). The receipt is REQUIRED
 * by the type — the wire contract says every write 200 carries one, and an optional parameter is
 * how a receipt-less DENIED (the op-WAL wedge class) slips back in at a future call site. */
export function deniedEnvelope(
  command: string,
  code: string,
  skillName: string | undefined,
  receipt: ReceiptShape,
): Record<string, unknown> {
  const nextActions: NextAction[] = [
    nextAction("REQUEST_ACCESS", []),
    nextAction("CONTACT_ADMIN", []),
  ];
  return errorReceiptEnvelope(
    command,
    {
      code,
      outcome: "DENIED",
      retryable: false,
      affected: skillName === undefined ? {} : { skill: skillName },
      context: {},
      next_actions: nextActions,
    },
    receipt,
  );
}

/** Serialize a stored/constructed envelope (replays re-serve the stored value verbatim). */
export function envelopeResponse(envelope: unknown, status = 200): Response {
  return new Response(JSON.stringify(envelope), { status, headers: JSON_HEADERS });
}
