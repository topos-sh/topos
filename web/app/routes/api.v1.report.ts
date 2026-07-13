import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { deviceReportApplied } from "@/lib/db/queries.device.server";

/**
 * `PUT /api/v1/workspaces/{ws}/report` — the device's post-reconcile applied snapshot
 * (`WireAppliedReport`) → 204. Served by this tier: `topos_report_applied` re-checks every
 * client-asserted skill against the server's own entitlement predicate, stamps the ONE
 * staleness clock, and fences itself (its caller here is READ COMMITTED). Small-body-capped
 * like the vault's enrollment belt; a malformed body is a 400 BEFORE the credential resolve
 * (the vault's extractor ordering, mirrored).
 */
const BODY_CAP = 64 * 1024;
const HEX_64 = /^[0-9a-f]{64}$/;
// The vault's own SkillId rule, mirrored: lowercase path-safe charset, 1–128 bytes.
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
  // `schema_version` is a required field on the wire (the vault's `WireAppliedReport` deserializer
  // rejected a body without it); mirror that presence check so the door posture matches.
  if (report.schema_version !== 1) {
    return badRequest("malformed report body: schema_version");
  }
  const skillIds: string[] = [];
  const commits: Buffer[] = [];
  for (const entry of report.applied as unknown[]) {
    const row = entry as { skill_id?: unknown; version_id?: unknown };
    if (typeof row.skill_id !== "string" || !SKILL_ID.test(row.skill_id)) {
      return badRequest("malformed report entry: skill_id");
    }
    if (typeof row.version_id !== "string" || !HEX_64.test(row.version_id)) {
      return badRequest("malformed report entry: version_id must be 64-char lowercase hex");
    }
    skillIds.push(row.skill_id);
    commits.push(Buffer.from(row.version_id, "hex"));
  }
  const actor = await requireDeviceActor(request, params.ws ?? "");
  const outcome = await deviceReportApplied(actor, Date.now(), skillIds, commits);
  if (outcome === null) {
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
