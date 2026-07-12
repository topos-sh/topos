import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Form, useActionData, useLoaderData, useNavigation } from "react-router";
import { relativeTime } from "@/components/format";
import { buttonClasses, Card, PageHeader, SectionHeading } from "@/components/ui";
import { actorFromSession, notFound, requireSession } from "@/lib/auth/guards.server";
import type { AdminOutcome } from "@/lib/db/audit.server";
import {
  devicesFor,
  recordSelfDeviceRevoke,
  type SignOutOutcome,
  signOutDevice,
  type WorkspaceDevices,
} from "@/lib/db/queries.devices.server";

export function meta() {
  return [{ title: "Your devices" }];
}

/**
 * The account-level device list — every device enrolled to the signed-in person across every
 * workspace they hold a confirmed seat in. An unverified session is not an actor (membership and
 * every device row key on a verified email), so it gets the house 404, never the page.
 */
export async function loader({ request }: LoaderFunctionArgs) {
  const actor = actorFromSession(await requireSession(request));
  if (!actor) {
    notFound();
  }
  return { workspaces: await devicesFor(actor) };
}

/**
 * Self sign-out — the GitHub-sessions "sign this device out" pattern. It is NOT step-up gated: the
 * destructive-ceremony gates protect OTHER people's access, and this is the person's own escape
 * hatch. The action still RE-GUARDS (session -> actor -> the DAL fn, itself re-gated by the
 * database's owner-or-self matrix) and records one admin_event whatever the outcome.
 */
export async function action({ request }: ActionFunctionArgs) {
  const actor = actorFromSession(await requireSession(request));
  if (!actor) {
    notFound();
  }
  const formData = await request.formData();
  if (String(formData.get("intent") ?? "") !== "sign-out") {
    return data({ status: "error" as const }, { status: 400 });
  }
  const workspaceId = String(formData.get("workspace_id") ?? "");
  const deviceKeyId = String(formData.get("device_key_id") ?? "");

  let status: SignOutOutcome | "error";
  let auditOutcome: AdminOutcome;
  try {
    status = await signOutDevice(actor, workspaceId, deviceKeyId);
    auditOutcome = status === "revoked" ? "ok" : "denied";
  } catch {
    status = "error";
    auditOutcome = "error";
  }
  await recordSelfDeviceRevoke(actor, workspaceId, deviceKeyId, auditOutcome);
  return data({ status });
}

/** The human copy for each non-success sign-out outcome (a self sign-out normally just succeeds). */
const SIGN_OUT_ERROR: Record<string, string> = {
  unknown_device: "That device is no longer enrolled — nothing to sign out.",
  self_required: "You can only sign out your own devices.",
  owner_or_self_required: "You can only sign out your own devices.",
  member_required: "You are no longer a member of that workspace.",
  error: "The server could not sign that device out. Try again.",
};

export default function YourDevices() {
  const { workspaces } = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();
  const error =
    actionData && actionData.status !== "revoked" ? SIGN_OUT_ERROR[actionData.status] : undefined;
  return (
    <div className="space-y-8">
      <PageHeader
        title="Your devices"
        meta="Devices enrolled to your account across every workspace you belong to."
      />
      {error !== undefined && (
        <p role="alert" className="text-red-700 text-sm">
          {error}
        </p>
      )}
      {workspaces.length === 0 ? (
        <NoDevices />
      ) : (
        workspaces.map((ws) => <WorkspaceSection key={ws.workspaceId} group={ws} />)
      )}
    </div>
  );
}

/**
 * Signed in, but no device enrolled in any workspace the person belongs to (or no confirmed seat at
 * all). Honest and instructive: enrolling a device is the agent's `follow` move.
 */
function NoDevices() {
  return (
    <div className="rounded-lg border border-line-soft border-dashed bg-panel px-6 py-12 text-center">
      <h2 className="font-display font-semibold text-base text-ink tracking-[-0.02em]">
        No enrolled devices
      </h2>
      <p className="mx-auto mt-2 max-w-md text-dim text-sm leading-relaxed">
        Enroll one from your agent — run{" "}
        <code className="rounded bg-panel2 px-1.5 py-0.5 font-mono text-[13px]">
          topos follow &lt;workspace address&gt;
        </code>{" "}
        on the device and it appears here.
      </p>
    </div>
  );
}

/** One workspace's devices, headed by its display name and address. */
function WorkspaceSection({ group }: { group: WorkspaceDevices }) {
  return (
    <section aria-labelledby={`devices-${group.workspaceId}`} className="space-y-3">
      <SectionHeading>
        <span id={`devices-${group.workspaceId}`}>
          {group.displayName}{" "}
          <span className="ml-1 font-mono text-faint normal-case">{group.address}</span>
        </span>
      </SectionHeading>
      <Card>
        <ul className="divide-y divide-line-soft">
          {group.devices.map((device) => (
            <DeviceRow
              key={device.deviceKeyId}
              workspaceId={group.workspaceId}
              deviceKeyId={device.deviceKeyId}
              revoked={device.revoked}
              lastReportAtMs={device.lastReportAtMs}
            />
          ))}
        </ul>
      </Card>
    </section>
  );
}

/**
 * One device row. An active device carries its enrolled/last-report line and a "Sign out" button
 * (no step-up — signing out your own device is the escape hatch, not a destructive ceremony over
 * someone else's access). A signed-out device is greyed and carries the re-enroll hint instead.
 */
function DeviceRow({
  workspaceId,
  deviceKeyId,
  revoked,
  lastReportAtMs,
}: {
  workspaceId: string;
  deviceKeyId: string;
  revoked: boolean;
  lastReportAtMs: number | null;
}) {
  const navigation = useNavigation();
  const submittingThis =
    navigation.state !== "idle" &&
    navigation.formData?.get("intent") === "sign-out" &&
    navigation.formData?.get("device_key_id") === deviceKeyId;
  const reported =
    lastReportAtMs === null
      ? "never reported"
      : `last reported ${relativeTime(new Date(lastReportAtMs))}`;
  return (
    <li
      className={`flex flex-wrap items-center justify-between gap-x-4 gap-y-2 px-4 py-3 ${
        revoked ? "opacity-55" : ""
      }`}
    >
      <div className="min-w-0 space-y-1">
        <code className="block break-all font-mono text-ink text-sm">{deviceKeyId}</code>
        {revoked ? (
          <p className="text-dim text-xs">
            signed out — re-enroll to use this device again:{" "}
            <code className="rounded bg-panel2 px-1.5 py-0.5 font-mono">topos auth login</code>
          </p>
        ) : (
          <p className="text-faint text-xs">enrolled · {reported}</p>
        )}
      </div>
      {!revoked && (
        <Form method="post">
          <input type="hidden" name="intent" value="sign-out" />
          <input type="hidden" name="workspace_id" value={workspaceId} />
          <input type="hidden" name="device_key_id" value={deviceKeyId} />
          <button type="submit" className={buttonClasses("danger")} disabled={submittingThis}>
            {submittingThis ? "Signing out…" : "Sign out"}
          </button>
        </Form>
      )}
    </li>
  );
}
