import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Form, useActionData, useLoaderData, useNavigation } from "react-router";
import { ConfirmButton } from "@/components/confirm";
import { relativeTime } from "@/components/format";
import { buttonClasses, Card, Chip, PageHeader } from "@/components/ui";
import { actorFromSession, notFound, requireSession } from "@/lib/auth/guards.server";
import {
  type AccountDevice,
  devicesFor,
  type SignOutOutcome,
  signOutDevice,
  type UnlinkOutcome,
  unlinkOwnDevice,
} from "@/lib/db/queries.devices.server";

export function meta() {
  return [{ title: "Your devices" }];
}

/**
 * The account-level device list — every device enrolled to the signed-in person, one flat list
 * (a device is registered to ONE user; each row carries its per-workspace LINKS). The page
 * needs only a session-minted UserActor: the read is self-scoped by construction.
 */
export async function loader({ request }: LoaderFunctionArgs) {
  const actor = actorFromSession(await requireSession(request));
  if (!actor) {
    notFound();
  }
  return { devices: await devicesFor(actor) };
}

/**
 * TWO self-service acts, dispatched on the hidden `intent`:
 *  - `sign-out` — the global self sign-out (the GitHub-sessions pattern): no confirmation (the
 *    person's own escape hatch), final (the trigger refuses un-revokes), and it severs every
 *    link + reported state in the same transaction.
 *  - `unlink` — sever ONE workspace link on one of the person's own devices; the page arm wears
 *    the in-place confirm. Bytes already on the machine stay there; relinking later is allowed.
 * Both are self-only by the DAL's WHERE clauses (a foreign id answers as an unknown one).
 */
export async function action({ request }: ActionFunctionArgs) {
  const actor = actorFromSession(await requireSession(request));
  if (!actor) {
    notFound();
  }
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  const deviceId = String(formData.get("device_id") ?? "");
  if (intent === "unlink") {
    const workspaceId = String(formData.get("workspace_id") ?? "");
    let status: UnlinkOutcome | "error";
    try {
      status = await unlinkOwnDevice(actor, deviceId, workspaceId);
    } catch {
      status = "error";
    }
    return data({ intent: "unlink" as const, status });
  }
  if (intent !== "sign-out") {
    return data({ intent: "unknown" as const, status: "error" as const }, { status: 400 });
  }
  let status: SignOutOutcome | "error";
  try {
    status = await signOutDevice(actor, deviceId);
  } catch {
    status = "error";
  }
  return data({ intent: "sign-out" as const, status });
}

/** The human copy for each non-success outcome (the self-service acts normally just succeed). */
const ACTION_ERROR: Record<string, string> = {
  unknown_device: "That device is no longer enrolled — nothing to sign out.",
  unknown_link: "That link is already gone — nothing to unlink.",
  error: "The server could not complete that. Try again.",
};

export default function YourDevices() {
  const { devices } = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();
  const error =
    actionData && actionData.status !== "revoked" && actionData.status !== "unlinked"
      ? ACTION_ERROR[actionData.status]
      : undefined;
  const notice =
    actionData?.intent === "unlink" && actionData.status === "unlinked"
      ? "Unlinked. Future delivery and reporting stop — the copies already on the device stay put."
      : undefined;
  return (
    <div className="space-y-8">
      <PageHeader title="Your devices" meta="Devices enrolled to your account." />
      <p className="text-dim text-sm leading-relaxed">
        Signing a device out removes it from your account entirely; unlinking a workspace just stops
        delivery there. Either way, the copies already on the device stay — nothing is deleted from
        the machine.
      </p>
      {error !== undefined && (
        <p role="alert" className="text-red-700 text-sm">
          {error}
        </p>
      )}
      {notice !== undefined && (
        <p role="status" className="text-dim text-sm">
          {notice}
        </p>
      )}
      {devices.length === 0 ? (
        <NoDevices />
      ) : (
        <Card>
          <ul className="divide-y divide-line-soft">
            {devices.map((device) => (
              <DeviceRow key={device.deviceId} device={device} />
            ))}
          </ul>
        </Card>
      )}
    </div>
  );
}

/**
 * Signed in, but no device enrolled yet. Honest and instructive: enrolling a device is the
 * agent's `follow` move.
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

/**
 * One device row: name, enrollment + last-seen line, the linked-workspace list (each link with
 * its status and a SELF unlink arm), and the global "Sign out" button. A signed-out device is
 * greyed and carries the re-enroll hint instead.
 */
function DeviceRow({ device }: { device: AccountDevice }) {
  const navigation = useNavigation();
  const submittingSignOut =
    navigation.state !== "idle" &&
    navigation.formData?.get("intent") === "sign-out" &&
    navigation.formData?.get("device_id") === device.deviceId;
  const seen =
    device.lastSeenAtMs === null
      ? "never seen"
      : `last seen ${relativeTime(new Date(device.lastSeenAtMs))}`;
  return (
    <li className={`space-y-3 px-4 py-3 ${device.revoked ? "opacity-55" : ""}`}>
      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
        <div className="min-w-0 space-y-1">
          <p className="text-ink text-sm">{device.displayName}</p>
          <code className="block break-all font-mono text-faint text-xs">{device.deviceId}</code>
          {device.revoked ? (
            <p className="text-dim text-xs">
              signed out — re-enroll to use this device again:{" "}
              <code className="rounded bg-panel2 px-1.5 py-0.5 font-mono">topos auth login</code>
            </p>
          ) : (
            <p className="text-faint text-xs">
              enrolled {relativeTime(new Date(device.createdAtMs))} · {seen}
            </p>
          )}
        </div>
        {!device.revoked && (
          <Form method="post">
            <input type="hidden" name="intent" value="sign-out" />
            <input type="hidden" name="device_id" value={device.deviceId} />
            <button type="submit" className={buttonClasses("danger")} disabled={submittingSignOut}>
              {submittingSignOut ? "Signing out…" : "Sign out"}
            </button>
          </Form>
        )}
      </div>
      {!device.revoked && <DeviceLinks device={device} />}
    </li>
  );
}

/** The device's linked workspaces — status-chipped, each with its own unlink arm. */
function DeviceLinks({ device }: { device: AccountDevice }) {
  const navigation = useNavigation();
  if (device.links.length === 0) {
    return (
      <p className="text-faint text-xs">Linked to no workspace — delivery reaches it nowhere.</p>
    );
  }
  return (
    <ul className="space-y-1.5">
      {device.links.map((link) => {
        const submitting =
          navigation.state !== "idle" &&
          navigation.formData?.get("intent") === "unlink" &&
          navigation.formData?.get("device_id") === device.deviceId &&
          navigation.formData?.get("workspace_id") === link.workspaceId;
        return (
          <li
            key={link.workspaceId}
            className="flex flex-wrap items-center gap-x-3 gap-y-1 border-line-soft border-t pt-1.5"
          >
            <span className="text-ink text-sm">{link.workspaceDisplayName}</span>
            <span className="font-mono text-faint text-xs">{link.workspaceName}</span>
            {link.status === "pending" ? (
              <Chip tone="pending">awaiting owner approval</Chip>
            ) : (
              <Chip tone="verified">linked</Chip>
            )}
            <span className="ml-auto">
              <Form method="post">
                <input type="hidden" name="intent" value="unlink" />
                <input type="hidden" name="device_id" value={device.deviceId} />
                <input type="hidden" name="workspace_id" value={link.workspaceId} />
                <ConfirmButton label="Unlink" tone="quiet" pending={submitting} />
              </Form>
            </span>
          </li>
        );
      })}
    </ul>
  );
}
