import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Form, useActionData, useLoaderData, useNavigation } from "react-router";
import { relativeTime } from "@/components/format";
import { buttonClasses, Card, PageHeader } from "@/components/ui";
import { actorFromSession, notFound, requireSession } from "@/lib/auth/guards.server";
import {
  type AccountDevice,
  devicesFor,
  type SignOutOutcome,
  signOutDevice,
} from "@/lib/db/queries.devices.server";

export function meta() {
  return [{ title: "Your devices" }];
}

/**
 * The account-level device list — every device enrolled to the signed-in person, one flat list
 * (a device is a possession of ONE user; there is no per-workspace grouping to render). The
 * page needs only a session-minted UserActor: the read is self-scoped by construction.
 */
export async function loader({ request }: LoaderFunctionArgs) {
  const actor = actorFromSession(await requireSession(request));
  if (!actor) {
    notFound();
  }
  return { devices: await devicesFor(actor) };
}

/**
 * Self sign-out — the GitHub-sessions "sign this device out" pattern. It carries no confirmation:
 * the ceremonies over OTHER people's access wear one, and this is the person's own
 * escape hatch — self-only by construction (the DAL's WHERE clause matches only the actor's
 * own device rows, so a foreign id answers the same as an unknown one). Final: the database
 * trigger refuses any un-revoke; re-enrolling is the recovery. The audit row lands inside the
 * DAL's own transaction.
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
  const deviceId = String(formData.get("device_id") ?? "");
  let status: SignOutOutcome | "error";
  try {
    status = await signOutDevice(actor, deviceId);
  } catch {
    status = "error";
  }
  return data({ status });
}

/** The human copy for each non-success sign-out outcome (a self sign-out normally just succeeds). */
const SIGN_OUT_ERROR: Record<string, string> = {
  unknown_device: "That device is no longer enrolled — nothing to sign out.",
  error: "The server could not sign that device out. Try again.",
};

export default function YourDevices() {
  const { devices } = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();
  const error =
    actionData && actionData.status !== "revoked" ? SIGN_OUT_ERROR[actionData.status] : undefined;
  return (
    <div className="space-y-8">
      <PageHeader title="Your devices" meta="Devices enrolled to your account." />
      {error !== undefined && (
        <p role="alert" className="text-red-700 text-sm">
          {error}
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
 * One device row. An active device carries its name, enrollment, and last-seen line plus a
 * "Sign out" button (no confirmation — signing out your own device is the escape hatch, not a
 * ceremony over someone else's access). A signed-out device is greyed and carries
 * the re-enroll hint instead.
 */
function DeviceRow({ device }: { device: AccountDevice }) {
  const navigation = useNavigation();
  const submittingThis =
    navigation.state !== "idle" &&
    navigation.formData?.get("intent") === "sign-out" &&
    navigation.formData?.get("device_id") === device.deviceId;
  const seen =
    device.lastSeenAtMs === null
      ? "never seen"
      : `last seen ${relativeTime(new Date(device.lastSeenAtMs))}`;
  return (
    <li
      className={`flex flex-wrap items-center justify-between gap-x-4 gap-y-2 px-4 py-3 ${
        device.revoked ? "opacity-55" : ""
      }`}
    >
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
          <button type="submit" className={buttonClasses("danger")} disabled={submittingThis}>
            {submittingThis ? "Signing out…" : "Sign out"}
          </button>
        </Form>
      )}
    </li>
  );
}
