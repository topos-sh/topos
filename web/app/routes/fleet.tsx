import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Form, Link, useActionData, useLoaderData } from "react-router";
import { relativeTime, shortDevice } from "@/components/format";
import { StepUpFields } from "@/components/step-up";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading, ShortId } from "@/components/ui";
import { notFound, requireMember } from "@/lib/auth/guards.server";
import { requireStepUp } from "@/lib/auth/step-up.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import {
  type Fleet,
  type FleetDevice,
  type FleetFreshness,
  type FleetSkillState,
  type FleetSkillStatus,
  fleetOf,
  revokeDevice,
} from "@/lib/db/queries.fleet.server";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Fleet · ${params.ws ?? "Workspace"}` }];
}

export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const actor = await requireMember(request, ws);
  const fleet = await fleetOf(actor);
  return { ws, fleet };
}

/**
 * ONE action, dispatched on the hidden `intent`. Every branch RE-GUARDS (a loader gate never
 * extends to an action). Revoke re-guards at MEMBER grade — a member may sign their OWN device
 * out — and the guarded `topos_revoke_device` runs the real owner-or-self matrix behind it.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  if (intent === "revoke") {
    return revokeIntent(request, ws, formData);
  }
  return data({ intent: "unknown" as const, status: "error" as const }, { status: 400 });
}

async function revokeIntent(request: Request, ws: string, formData: FormData) {
  const actor = await requireMember(request, ws);
  const deviceKeyId = String(formData.get("device_key_id") ?? "").trim();
  if (deviceKeyId.length === 0) {
    return { intent: "revoke" as const, status: "error" as const, deviceKeyId };
  }
  // Step-up FIRST — a failed re-auth performs nothing (no guarded call), but the attempt is still
  // audited (with detail "step_up" so a refused ceremony reads as one).
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(actor, {
      kind: "device_revoke",
      subject: deviceKeyId,
      detail: "step_up",
      outcome: "denied",
    });
    return {
      intent: "revoke" as const,
      status: "step_up_failed" as const,
      deviceKeyId,
      error: stepUp.error,
    };
  }
  let outcome: Awaited<ReturnType<typeof revokeDevice>>;
  try {
    outcome = await revokeDevice(actor, deviceKeyId);
  } catch {
    await recordAdminEvent(actor, {
      kind: "device_revoke",
      subject: deviceKeyId,
      outcome: "error",
    });
    return { intent: "revoke" as const, status: "error" as const, deviceKeyId };
  }
  await recordAdminEvent(actor, {
    kind: "device_revoke",
    subject: deviceKeyId,
    outcome: outcome === "revoked" ? "ok" : "denied",
  });
  return { intent: "revoke" as const, status: outcome, deviceKeyId };
}

// The hook UNWRAPS the `data(...)` response (and serializes), so derive the client-side shape from
// useActionData rather than the raw action return — otherwise the unknown-intent branch's
// DataWithResponseInit wrapper leaks into the type and mismatches the rendered value.
type FleetActionData = NonNullable<ReturnType<typeof useActionData<typeof action>>>;

/** The per-device result of the last submit, if it targeted this device. */
function resultFor(
  action: FleetActionData | undefined,
  deviceKeyId: string,
): Extract<FleetActionData, { intent: "revoke" }> | undefined {
  if (action === undefined || !("deviceKeyId" in action) || action.intent !== "revoke") {
    return undefined;
  }
  return action.deviceKeyId === deviceKeyId ? action : undefined;
}

export default function FleetPage() {
  const { ws, fleet } = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();
  return (
    <div className="space-y-8">
      <PageHeader
        title="Fleet"
        meta={<FleetMeta fleet={fleet} />}
        actions={
          <Link to={`/workspaces/${ws}`} className={buttonClasses("quiet")}>
            Back to workspace
          </Link>
        }
      />
      <IntroCopy wholeFleet={fleet.wholeFleet} />
      <FleetBody fleet={fleet} actionData={actionData} />
      <BlindSpots />
    </div>
  );
}

function FleetMeta({ fleet }: { fleet: Fleet }) {
  const count = fleet.devices.length;
  const stale = fleet.devices.filter((d) => d.freshness === "stale").length;
  return (
    <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
      <span>{count === 1 ? "1 device" : `${count} devices`}</span>
      {stale > 0 && (
        <>
          <span aria-hidden="true">·</span>
          <span>{stale === 1 ? "1 stale" : `${stale} stale`}</span>
        </>
      )}
    </div>
  );
}

function IntroCopy({ wholeFleet }: { wholeFleet: boolean }) {
  return (
    <p className="text-dim text-sm leading-relaxed">
      {wholeFleet ? (
        <>
          Every enrolled device in this workspace and the version each one last reported. Use it to
          confirm a change has reached the fleet — after a fix lands, watch until every non-stale
          device reads <em className="text-ink not-italic">current</em>.
        </>
      ) : (
        <>
          These are your own devices in this workspace and the version each one last reported.
          Reviewers and owners see the whole fleet.
        </>
      )}
    </p>
  );
}

function FleetBody({
  fleet,
  actionData,
}: {
  fleet: Fleet;
  actionData: FleetActionData | undefined;
}) {
  if (fleet.devices.length === 0) {
    return (
      <div className="rounded-lg border border-line-soft border-dashed bg-panel px-6 py-12 text-center">
        <h2 className="font-display font-semibold text-base text-ink tracking-[-0.02em]">
          No devices enrolled yet
        </h2>
        <p className="mx-auto mt-2 max-w-md text-dim text-sm leading-relaxed">
          A device appears here the first time it enrolls and reports what it is running — run{" "}
          <code className="rounded bg-panel2 px-1.5 py-0.5 font-mono text-[13px]">
            topos follow
          </code>{" "}
          on it, then let it sync once.
        </p>
      </div>
    );
  }

  const present = fleet.devices.filter((d) => !d.removedUpstream);
  const removed = fleet.devices.filter((d) => d.removedUpstream);

  return (
    <div className="space-y-8">
      <PresentDevices
        devices={present}
        wholeFleet={fleet.wholeFleet}
        stalenessWindowMs={fleet.stalenessWindowMs}
        actionData={actionData}
      />
      {removed.length > 0 && (
        <RemovedUpstream
          devices={removed}
          stalenessWindowMs={fleet.stalenessWindowMs}
          actionData={actionData}
        />
      )}
    </div>
  );
}

function PresentDevices({
  devices,
  wholeFleet,
  stalenessWindowMs,
  actionData,
}: {
  devices: FleetDevice[];
  wholeFleet: boolean;
  stalenessWindowMs: number;
  actionData: FleetActionData | undefined;
}) {
  if (devices.length === 0) {
    return null;
  }
  // Reviewer/owner: group by person. Member: a single flat "Your devices" list.
  if (!wholeFleet) {
    return (
      <section aria-labelledby="your-devices-heading" className="space-y-3">
        <SectionHeading>
          <span id="your-devices-heading">Your devices</span>
        </SectionHeading>
        <div className="space-y-3">
          {devices.map((device) => (
            <DeviceCard
              key={device.deviceKeyId}
              device={device}
              showPrincipal={false}
              stalenessWindowMs={stalenessWindowMs}
              actionData={actionData}
            />
          ))}
        </div>
      </section>
    );
  }

  const groups = groupByPrincipal(devices);
  return (
    <section aria-labelledby="fleet-heading" className="space-y-6">
      <SectionHeading>
        <span id="fleet-heading">Enrolled devices</span>
      </SectionHeading>
      {groups.map(([principal, group]) => (
        <div key={principal} className="space-y-3">
          <h3 className="font-medium text-ink text-sm">{principal}</h3>
          <div className="space-y-3">
            {group.map((device) => (
              <DeviceCard
                key={device.deviceKeyId}
                device={device}
                showPrincipal={false}
                stalenessWindowMs={stalenessWindowMs}
                actionData={actionData}
              />
            ))}
          </div>
        </div>
      ))}
    </section>
  );
}

/**
 * The removed-upstream blind spot, named and enumerated: devices whose principal no longer holds a
 * confirmed seat. Removal deleted the seat, never the device — the copies are still out there.
 */
function RemovedUpstream({
  devices,
  stalenessWindowMs,
  actionData,
}: {
  devices: FleetDevice[];
  stalenessWindowMs: number;
  actionData: FleetActionData | undefined;
}) {
  return (
    <section aria-labelledby="removed-upstream-heading" className="space-y-3">
      <SectionHeading>
        <span id="removed-upstream-heading">Removed upstream</span>
      </SectionHeading>
      <p className="text-dim text-sm leading-relaxed">
        The seat is gone, but these copies remain on their devices — this page can no longer move
        them. Revoke a device to stop its credential; the bytes already on disk must be chased by
        hand.
      </p>
      <div className="space-y-3">
        {devices.map((device) => (
          <DeviceCard
            key={device.deviceKeyId}
            device={device}
            showPrincipal={true}
            stalenessWindowMs={stalenessWindowMs}
            actionData={actionData}
          />
        ))}
      </div>
    </section>
  );
}

function DeviceCard({
  device,
  showPrincipal,
  stalenessWindowMs,
  actionData,
}: {
  device: FleetDevice;
  showPrincipal: boolean;
  stalenessWindowMs: number;
  actionData: FleetActionData | undefined;
}) {
  return (
    <Card className="overflow-hidden">
      <div data-testid={`fleet-device-${device.deviceKeyId}`} className="space-y-3 px-4 py-3">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
          <span className="font-mono text-ink text-sm">
            device {shortDevice(device.deviceKeyId)}
          </span>
          {showPrincipal && <span className="text-dim text-sm">{device.principal}</span>}
          <span className="ml-auto flex flex-wrap items-center gap-1.5">
            <FreshnessChip freshness={device.freshness} />
            {device.removedUpstream && <RemovedChip />}
            {device.revoked && <RevokedChip />}
          </span>
        </div>
        <div className="text-faint text-xs">
          {device.lastReportAt === null ? (
            <>Has never reported — no session has run on it since it enrolled.</>
          ) : (
            <>
              last reported {relativeTime(new Date(device.lastReportAt))}
              {device.freshness === "stale" && (
                <> · past the {formatWindow(stalenessWindowMs)} window — chase by hand</>
              )}
            </>
          )}
        </div>
        <SkillStates skills={device.skills} />
        {device.canRevoke && !device.revoked && (
          <RevokeControl deviceKeyId={device.deviceKeyId} actionData={actionData} />
        )}
        {device.revoked && (
          <p className="text-faint text-xs">
            Credential revoked — the device is signed out. Re-enrolling (
            <code className="rounded bg-panel2 px-1 py-0.5 font-mono">topos auth login</code>) is
            the recovery.
          </p>
        )}
      </div>
    </Card>
  );
}

function SkillStates({ skills }: { skills: FleetSkillState[] }) {
  if (skills.length === 0) {
    return <p className="text-faint text-xs">No skills reported for this device.</p>;
  }
  return (
    <ul className="space-y-1.5">
      {skills.map((skill) => (
        <li
          key={skill.skillId}
          className="flex flex-wrap items-center gap-x-2 gap-y-1 border-line-soft border-t pt-1.5 first:border-t-0 first:pt-0"
        >
          <span className="min-w-0 truncate text-ink text-sm">
            {skill.skillName ?? skill.skillId}
          </span>
          <SkillStatusChip status={skill.status} />
          {skill.appliedCommit !== null && <ShortId value={skill.appliedCommit} />}
          {skill.status === "detached" && (
            <span className="text-faint text-xs">
              last known state
              {skill.detachedAt !== null && (
                <> · frozen {relativeTime(new Date(skill.detachedAt))}</>
              )}
            </span>
          )}
        </li>
      ))}
    </ul>
  );
}

/**
 * The revoke ceremony, folded behind a disclosure so it never crowds the row. Step-up is embedded
 * (the password is re-verified before the guarded call); the copy names the boundary — revocation
 * is instant and re-enrollment is the only way back.
 */
function RevokeControl({
  deviceKeyId,
  actionData,
}: {
  deviceKeyId: string;
  actionData: FleetActionData | undefined;
}) {
  const result = resultFor(actionData, deviceKeyId);
  const error = result !== undefined && "error" in result ? result.error : failureCopy(result);
  return (
    <details className="rounded-md border border-line-soft bg-panel2/40 px-3 py-2">
      <summary className="cursor-pointer font-mono text-[13px] text-dim">
        Revoke this device
      </summary>
      <Form method="post" className="mt-3 space-y-3">
        <input type="hidden" name="intent" value="revoke" />
        <input type="hidden" name="device_key_id" value={deviceKeyId} />
        <p className="text-dim text-sm leading-relaxed">
          Revocation is instant — the device&apos;s credential stops working immediately. Fresh work
          on it dies; re-enrolling is the recovery.
        </p>
        <StepUpFields idPrefix={`revoke-${deviceKeyId}`} />
        {error !== undefined && <p className="text-red-700 text-sm">{error}</p>}
        <button type="submit" className={buttonClasses("danger")}>
          Revoke device
        </button>
      </Form>
    </details>
  );
}

/** Map a non-error revoke outcome to its human copy (the step-up error carries its own). */
function failureCopy(
  result: Extract<FleetActionData, { intent: "revoke" }> | undefined,
): string | undefined {
  if (result === undefined) {
    return undefined;
  }
  switch (result.status) {
    case "owner_or_self_required":
      return "You can only revoke your own devices — an owner can revoke any device here.";
    case "unknown_device":
      return "That device is no longer registered.";
    case "member_required":
      return "Your seat in this workspace has changed — reload and try again.";
    case "error":
      return "Something went wrong. Reload and try again.";
    default:
      // "revoked" and "step_up_failed" are handled elsewhere (revalidation / the error field).
      return undefined;
  }
}

function FreshnessChip({ freshness }: { freshness: FleetFreshness }) {
  if (freshness === "fresh") {
    return <Chip tone="verified">fresh</Chip>;
  }
  if (freshness === "stale") {
    return <Chip tone="pending">stale</Chip>;
  }
  return <Chip tone="neutral">never reported</Chip>;
}

function SkillStatusChip({ status }: { status: FleetSkillStatus }) {
  if (status === "current") {
    return <Chip tone="verified">current</Chip>;
  }
  if (status === "behind") {
    return <Chip tone="pending">behind</Chip>;
  }
  return <Chip tone="neutral">detached</Chip>;
}

function RevokedChip() {
  return (
    <span className="inline-flex items-center gap-1 rounded-full bg-red-50 px-2 py-0.5 font-medium text-red-800 text-xs">
      revoked
    </span>
  );
}

function RemovedChip() {
  return (
    <span className="inline-flex items-center gap-1 rounded-full border border-red-200 px-2 py-0.5 font-medium text-red-800 text-xs">
      removed upstream
    </span>
  );
}

/**
 * The legend + the load-bearing footnote: devices report at SESSION START, so a healthy but idle
 * machine reads stale. Naming each state here keeps the blind spots explicit — the page enumerates
 * them, it never hides them.
 */
function BlindSpots() {
  return (
    <section aria-labelledby="reading-heading" className="space-y-3">
      <SectionHeading>
        <span id="reading-heading">Reading this page</span>
      </SectionHeading>
      <Card className="space-y-2 px-4 py-3 text-dim text-sm leading-relaxed">
        <p>
          Devices report at the <strong className="font-medium text-ink">start of a session</strong>
          , so a healthy but idle machine reads <em className="text-ink not-italic">stale</em> until
          its next run — stale means unconfirmed, not wrong.
        </p>
        <p>
          <em className="text-ink not-italic">Detached</em> copies are a device&apos;s last known
          state, frozen when the person unfollowed or left; the bytes stay on their machine and this
          page can no longer move them.
        </p>
        <p>
          <em className="text-ink not-italic">Removed upstream</em> devices belong to a principal
          whose seat is gone; the copies remain on their devices and must be chased by hand.
        </p>
        <p>
          After publishing a scrubbed version, watch until every non-stale device reads{" "}
          <em className="text-ink not-italic">current</em>, then enumerate the stale, detached, and
          removed-upstream copies and chase them directly — none of them are silently omitted here.
        </p>
      </Card>
    </section>
  );
}

/** A group of one person's devices, preserving the query's principal order. */
function groupByPrincipal(devices: FleetDevice[]): [string, FleetDevice[]][] {
  const groups = new Map<string, FleetDevice[]>();
  for (const device of devices) {
    const list = groups.get(device.principal);
    if (list === undefined) {
      groups.set(device.principal, [device]);
    } else {
      list.push(device);
    }
  }
  return [...groups.entries()];
}

/** A calm "7 days" / "1 hour" window label for the stale note. */
function formatWindow(ms: number): string {
  const hours = Math.round(ms / 3_600_000);
  if (hours < 24) {
    return hours === 1 ? "1 hour" : `${hours} hour`;
  }
  const days = Math.round(ms / 86_400_000);
  return days === 1 ? "1 day" : `${days} day`;
}
