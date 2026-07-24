import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { parseCandidate, parsePublishHead, receiptNow } from "@/lib/api/candidate.server";
import { publishFlow } from "@/lib/api/publish-flow.server";
import { buildReceipt, deniedEnvelope, envelopeResponse } from "@/lib/api/receipts.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { findReceipt } from "@/lib/db/queries.custody.server";

/**
 * `POST /api/v1/publish` — the direct publish, APP-AUTHORIZED end to end: this tier resolves
 * the acting device, replays the op receipt, runs the protection gate (a member's publish on
 * an effectively-'reviewed' bundle REROUTES to a proposal — NEEDS_REVIEW, `downgraded`), and
 * only then asks the vault to ingest + CAS-move. The wire envelope stays byte-shaped like the
 * committed fixtures; a same-op_id retry replays the stored envelope verbatim.
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
  const raw = await readCappedBody(request, BODY_CAP, "publish body");
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
    return badRequest("malformed publish body");
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
    upstream: parseUpstream(body.upstream),
    command: "publish",
    forceProposal: false,
  });
}

/** The optional upstream-provenance block (`{host, repo, path?, commit?, license?}`) — parsed
 * leniently: a malformed block is DROPPED (provenance is an adjunct, never a publish blocker). */
function parseUpstream(raw: unknown): import("@/lib/api/publish-flow.server").UpstreamInput | null {
  if (typeof raw !== "object" || raw === null) {
    return null;
  }
  const u = raw as Record<string, unknown>;
  if (typeof u.host !== "string" || typeof u.repo !== "string") {
    return null;
  }
  if (u.host !== "github.com" || !/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(u.repo)) {
    return null;
  }
  return {
    host: u.host,
    repo: u.repo,
    path: typeof u.path === "string" ? u.path : "",
    commit: typeof u.commit === "string" ? u.commit : null,
    license: typeof u.license === "string" ? u.license : null,
  };
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
