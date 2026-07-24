import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { parseCandidate, parsePublishHead, receiptNow } from "@/lib/api/candidate.server";
import { publishFlow } from "@/lib/api/publish-flow.server";
import { buildReceipt, deniedEnvelope, envelopeResponse } from "@/lib/api/receipts.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { findReceipt } from "@/lib/db/queries.custody.server";

/**
 * `POST /api/v1/proposals` — open a proposal explicitly: the SAME flow as publish with the
 * propose arm forced (a commit-only ingest + the proposal row; `current` never moves;
 * NEEDS_REVIEW). Genesis still lands directly — there is no base to review against.
 */
const BODY_CAP = 160 * 1024 * 1024;

export async function action({ request }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "POST") {
    return uniformNotFound();
  }
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
      command: "publish",
      outcome: "DENIED",
      workspaceId: actor.workspaceId,
      createdAt: receiptNow(),
    });
    return envelopeResponse(deniedEnvelope("publish", "OP_ID_REUSED", undefined, receipt));
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
