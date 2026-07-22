import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { deniedCodeEnvelope, okDataEnvelope } from "@/lib/api/row-envelopes.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireDevicePerson } from "@/lib/auth/guards.server";
import { applyDeviceLink, type DeviceLinkOp, describeDeviceLink } from "@/lib/db/identity.server";
import { workspaceAddress } from "@/lib/ws-url.server";

/**
 * The device↔workspace LINK lane — a device is registered once (device ↔ server) and linked
 * per workspace, and this route is the link's own op pair:
 *
 *  - `GET /api/v1/device/link?workspace=<address-slug>` — the DESCRIBE: the caller's standing
 *    in the named workspace (role, the link this device holds NOW, what a link created now
 *    would be born as). Nothing mutates.
 *  - `POST /api/v1/device/link` body `{"workspace": "<slug>"}` — the APPLY: create THIS
 *    device's link, born per the one rule, idempotent (an existing row answers ok with its
 *    current status).
 *
 * Guard: `requireDevicePerson` (credential → un-revoked device → user — NO seat requirement:
 * the describe must be able to answer the refusal). `workspace` resolves by NAME in both
 * tenancies; the empty string is the single-tenant origin-addressed form and a refusal in
 * multi. A seatless caller and an unknown workspace name answer ONE byte-identical typed
 * refusal (`NOT_A_MEMBER`, pointing at the invitation path) — no existence oracle.
 */
const BODY_CAP = 8 * 1024;
const MAX_WORKSPACE = 512;

const NOT_A_MEMBER_MESSAGE =
  "not a member of that workspace — ask a workspace owner to invite you; an invitation link redeems on this device";

/** Serialize one link-op outcome onto the wire (the all-outcome 200 envelope; a device revoked
 * mid-flight folds into the uniform 404 — the same answer its dead credential gets everywhere). */
function linkOpResponse(request: Request, op: DeviceLinkOp, describe: boolean): Response {
  if (op.outcome === "device_revoked") {
    return uniformNotFound();
  }
  if (op.outcome === "not_a_member") {
    return deniedCodeEnvelope("link", "NOT_A_MEMBER", NOT_A_MEMBER_MESSAGE);
  }
  return okDataEnvelope("link", {
    workspace_id: op.workspaceId,
    name: op.name,
    display_name: op.displayName,
    address: workspaceAddress(request, op.name),
    ...(describe ? { role: op.role } : {}),
    link_status: op.linkStatus,
    ...(describe ? { born: op.born } : {}),
  });
}

export async function loader({ request }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const workspace = new URL(request.url).searchParams.get("workspace") ?? "";
  if (workspace.length > MAX_WORKSPACE) {
    return badRequest("malformed link describe: workspace");
  }
  const person = await requireDevicePerson(request);
  return linkOpResponse(request, await describeDeviceLink(person, workspace), true);
}

export async function action({ request }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "POST") {
    return uniformNotFound();
  }
  const raw = await readCappedBody(request, BODY_CAP, "link body");
  if (raw instanceof Response) {
    return raw;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return badRequest("malformed JSON body");
  }
  const workspace = (parsed as { workspace?: unknown }).workspace;
  if (
    typeof parsed !== "object" ||
    parsed === null ||
    typeof workspace !== "string" ||
    workspace.length > MAX_WORKSPACE
  ) {
    return badRequest("malformed link body");
  }
  const person = await requireDevicePerson(request);
  return linkOpResponse(request, await applyDeviceLink(person, workspace), false);
}
