import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { reportApplied } from "@/lib/db/queries.lane.server";

/**
 * `PUT /api/v1/workspaces/{ws}/report` — the device's post-reconcile applied snapshot
 * (`WireAppliedReport`) → 204. Every client-asserted bundle is re-checked against the server's
 * own entitlement predicate, and the reconcile only UPSERTS — a row whose bundle left the
 * install set is the frozen "last known" record the fleet page derives blind spots from.
 * Small-body-capped; a malformed body is a 400 BEFORE the credential resolve.
 */
const BODY_CAP = 64 * 1024;
const HEX_64 = /^[0-9a-f]{64}$/;
// The vault's own id rule, mirrored: lowercase path-safe charset, 1–128 bytes.
const SKILL_ID = /^[a-z0-9_-]{1,128}$/;

export async function action({ request, params }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "PUT") {
    return uniformNotFound();
  }
  const body = await readCappedBody(request, BODY_CAP, "report body");
  if (body instanceof Response) {
    return body;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    return badRequest("malformed JSON body");
  }
  const report = parsed as { schema_version?: unknown; applied?: unknown };
  if (typeof parsed !== "object" || parsed === null || !Array.isArray(report.applied)) {
    return badRequest("malformed report body");
  }
  if (report.schema_version !== 1) {
    return badRequest("malformed report body: schema_version");
  }
  const applied: { skillId: string; versionId: string }[] = [];
  for (const entry of report.applied as unknown[]) {
    const row = entry as { skill_id?: unknown; version_id?: unknown };
    if (typeof row.skill_id !== "string" || !SKILL_ID.test(row.skill_id)) {
      return badRequest("malformed report entry: skill_id");
    }
    if (typeof row.version_id !== "string" || !HEX_64.test(row.version_id)) {
      return badRequest("malformed report entry: version_id must be 64-char lowercase hex");
    }
    applied.push({ skillId: row.skill_id, versionId: row.version_id });
  }
  const actor = await requireDeviceActor(request, params.ws ?? "");
  const outcome = await reportApplied(actor, applied);
  if (outcome === "unlinked") {
    // The link vanished between the guard and the write (an unlink/sever won the race) — the
    // same uniform miss the guard itself would have answered a moment later.
    return uniformNotFound();
  }
  return new Response(null, { status: 204 });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
