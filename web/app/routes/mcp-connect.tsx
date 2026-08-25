import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, redirect, useActionData, useLoaderData } from "react-router";
import { McpConnectForm } from "@/components/skill/mcp-gateway";
import { buttonClasses, PageHeader } from "@/components/ui";
import {
  memberPageInScope,
  notFound,
  requireMemberInScope,
  requireWorkspaceOwner,
} from "@/lib/auth/guards.server";
import { baseForKind, bundleNameOf, bundlePath } from "@/lib/bundle-base";
import { recordAdminEvent } from "@/lib/db/audit.server";
import { mcpServerFace } from "@/lib/db/queries.mcp-catalog.server";
import { skillIndexRow } from "@/lib/db/queries.server";
import { gatewayLane } from "@/lib/gateway/client.server";
import { storeManualCredential } from "@/lib/gateway/credentials.server";
import { useSubmittingIntent } from "@/lib/pending";
import { useWsPath } from "@/lib/ws-path";
import { wsPathServer } from "@/lib/ws-url.server";

export function meta({ params }: { params: { server?: string } }) {
  return [{ title: `Connect ${params.server ?? "server"} · Topos` }];
}

/**
 * THE MANUAL TIER'S CONNECT PAGE — `mcp/:server/connect`, its own Rails-style resource because a
 * secret somebody creates by hand is a form, not a redirect. An `oauth` server never lands here:
 * its Connect button leaves for the upstream's own consent screen and comes back, so this page
 * answers the house 404 for one — the same answer a server with no sign-in at all gets, and the
 * same answer every path gets on a deployment that runs no gateway.
 *
 * `?scope=workspace` connects the WORKSPACE service account; the owner gate is re-run inside the
 * action, never trusted from the query. The default is the person's own sign-in.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { workspace, actor } = await memberPageInScope(request, params);
  if (gatewayLane() === null) {
    notFound();
  }
  const name = bundleNameOf(params);
  const row = await skillIndexRow(actor, name);
  if (row === undefined) {
    notFound();
  }
  const server = await mcpServerFace(actor, row.skillId);
  if (server === null || server.authMode !== "manual") {
    notFound();
  }
  const scope: "mine" | "workspace" =
    new URL(request.url).searchParams.get("scope") === "workspace" ? "workspace" : "mine";
  if (scope === "workspace") {
    await requireWorkspaceOwner(request, workspace.id);
  }
  return {
    wsName: workspace.name,
    serverPath: bundlePath(baseForKind(row.kind), name),
    displayName: server.displayName,
    authNote: server.authNote,
    scope,
  };
}

interface ConnectActionData {
  intent: "gateway-manual";
  status: "error";
  message: string;
}

/**
 * SAVE THE SECRET. It goes straight over the internal lane to the gateway, which is the only tier
 * that may hold one — nothing here writes it down, echoes it back, or names it in the audit row.
 * A landed save redirects to the server's page, where the state line now says the sign-in stands.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const { workspace, actor } = await requireMemberInScope(request, params);
  if (gatewayLane() === null) {
    notFound();
  }
  const name = bundleNameOf(params);
  const row = await skillIndexRow(actor, name);
  if (row === undefined) {
    notFound();
  }
  const server = await mcpServerFace(actor, row.skillId);
  if (server === null || server.authMode !== "manual") {
    notFound();
  }
  const formData = await request.formData();
  const scope = formData.get("scope") === "workspace" ? "workspace" : "mine";
  if (scope === "workspace") {
    await requireWorkspaceOwner(request, workspace.id);
  }
  const secret = String(formData.get("secret") ?? "").trim();
  if (secret.length === 0) {
    return data<ConnectActionData>(
      { intent: "gateway-manual", status: "error", message: "Enter the secret to save it." },
      { status: 400 },
    );
  }
  const stored = await storeManualCredential({
    workspaceId: workspace.id,
    serverId: server.serverId,
    userId: scope === "workspace" ? null : actor.userId,
    secret,
    createdByDisplay: actor.display,
  });
  await recordAdminEvent(actor, {
    kind: "mcp_signin_connected",
    subject: server.serverId,
    detail: scope,
    outcome: stored.kind === "stored" ? "ok" : "error",
  });
  if (stored.kind === "stored") {
    throw redirect(wsPathServer(workspace.name, bundlePath(baseForKind(row.kind), name)));
  }
  return data<ConnectActionData>(
    {
      intent: "gateway-manual",
      status: "error",
      message:
        stored.kind === "rejected"
          ? "That secret was not accepted."
          : "That didn't go through. Try again.",
    },
    { status: stored.kind === "rejected" ? 400 : 500 },
  );
}

export default function McpConnectPage() {
  const { serverPath, displayName, authNote, scope } = useLoaderData<typeof loader>();
  const state = useActionData<typeof action>();
  const wsPath = useWsPath();
  const busy = useSubmittingIntent() !== null;
  return (
    <div className="space-y-8">
      <PageHeader
        title={`Connect ${displayName}`}
        actions={
          <Link to={wsPath(serverPath)} className={buttonClasses("quiet")}>
            {displayName}
          </Link>
        }
      />
      {/* The scope rides the FORM, not the query a browser can retype: the action re-runs the
          owner gate against it before a workspace-wide sign-in is stored. */}
      <McpConnectForm
        authNote={authNote}
        scope={scope}
        busy={busy}
        error={state === undefined ? null : state.message}
      />
    </div>
  );
}
