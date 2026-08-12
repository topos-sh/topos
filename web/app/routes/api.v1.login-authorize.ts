import type { ActionFunctionArgs } from "react-router";
import { laneGate } from "@/lib/api/compat.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import {
  LOGIN_FLOW_POLL_INTERVAL_SECS,
  type LoginBinding,
  startLoginFlow,
} from "@/lib/db/identity.server";
import { publicOrigin } from "@/lib/plane/public-base.server";
import { isWorkspaceNameShape } from "@/lib/workspace-name";

/**
 * `POST /api/v1/login/authorize` — begin the gh-style login flow toward THIS SERVER
 * (`DeviceAuthStartRequest` → `DeviceAuthStartResponse`). The flow starts WORKSPACE-LESS: the
 * workspace is chosen (or created) at the browser approval, where the signed-in approver's
 * seats are known — so this route mints the flow row on BOTH tenancies, always, with no
 * workspace read at all. Deliberate and load-bearing: the start is unauthenticated, so it must
 * disclose NOTHING about workspaces or accounts, and every refusal is constant.
 *
 * `preselect` is the ADDRESS SLUG a `login <workspace>` shortcut named — recorded
 * shape-checked but UNRESOLVED (never an existence check), display-only: it preselects the
 * chooser's matching option and decides nothing. A shape-invalid value is the uniform 404
 * (such a name can never exist). A login mints ONE workspace-scoped session; further
 * workspaces are further logins (or the lane-side `login/connect`).
 *
 * No credential yet: this is the flow's unauthenticated start (the lane gate is its only gate).
 * The response's `device_code` (the RFC 8628 field name — the gh-proven device-authorization
 * grant shape) is the polling secret — and, once approved, the session's ONE bearer credential
 * (promoted at the exchange; the poll echoes it back from the field the client already holds).
 */
const BODY_CAP = 8 * 1024;
const MAX_REQUESTED_NAME = 200;
const MAX_INVITE_TOKEN = 512;

export async function action({ request }: ActionFunctionArgs): Promise<Response> {
  const gated = laneGate(request);
  if (gated !== null) {
    return gated;
  }
  if (request.method !== "POST") {
    return uniformNotFound();
  }
  const raw = await readCappedBody(request, BODY_CAP, "login authorize body");
  if (raw instanceof Response) {
    return raw;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return badRequest("malformed JSON body");
  }
  const body = parsed as {
    requested_name?: unknown;
    preselect?: unknown;
    invite_token?: unknown;
    redirect?: unknown;
  };
  if (
    typeof parsed !== "object" ||
    parsed === null ||
    typeof body.requested_name !== "string" ||
    body.requested_name.trim().length === 0 ||
    body.requested_name.length > MAX_REQUESTED_NAME
  ) {
    return badRequest("malformed login authorize body");
  }
  // The optional preselect: a wrong TYPE is a malformed body; a well-typed slug that can never
  // exist (shape-invalid per the one workspace-name rule) is the uniform 404 — the same answer
  // any impossible name gets, existence never consulted.
  if (body.preselect !== undefined && typeof body.preselect !== "string") {
    return badRequest("malformed login authorize body: preselect");
  }
  if (body.preselect !== undefined && !isWorkspaceNameShape(body.preselect)) {
    return uniformNotFound();
  }
  // The optional invitation token a `topos login <invite-url>` carries: recorded (as its
  // hash) UNVALIDATED — this start is unauthenticated and must not be a token oracle. The
  // approval resolves it under its own fence.
  if (
    body.invite_token !== undefined &&
    (typeof body.invite_token !== "string" ||
      body.invite_token.length === 0 ||
      body.invite_token.length > MAX_INVITE_TOKEN)
  ) {
    return badRequest("malformed login authorize body: invite_token");
  }
  // How the approval outcome is ACCELERATED back — the CLI's own declaration, made because it
  // has just bound a 127.0.0.1 listener and can open a browser on this machine. WRITE-ONCE
  // from here: the binding is what gates the /verify card's URL pre-arm, so nothing downstream
  // may re-read it from a request. The CLI sends the field ONLY when it bound a listener, so an
  // absent value is a positive statement — this start wants the typed-code flow. Either way the
  // POLL is the one completion mechanism; a loopback redirect carries state + outcome only,
  // never a secret.
  if (body.redirect !== undefined && body.redirect !== "loopback" && body.redirect !== "device") {
    return badRequest("malformed login authorize body: redirect");
  }
  const binding: LoginBinding = body.redirect === "loopback" ? "loopback" : "device";

  const flow = await startLoginFlow(
    body.requested_name.trim(),
    (body.preselect as string | undefined) ?? null,
    body.invite_token as string | undefined,
    binding,
  );
  const origin = publicOrigin(request);
  // The code never enters ANY URL: the CLI prints the bare /verify address and the short code
  // on separate lines, and the human types the code into the page's POST form.
  return Response.json({
    device_code: flow.flowCode,
    user_code: flow.userCode,
    verification_uri: `${origin}/verify`,
    expires_in_secs: flow.expiresInSecs,
    interval_secs: LOGIN_FLOW_POLL_INTERVAL_SECS,
  });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
