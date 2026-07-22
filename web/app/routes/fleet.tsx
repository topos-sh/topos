import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { Form, Link, useActionData, useLoaderData, useNavigation } from "react-router";
import { ConfirmButton } from "@/components/confirm";
import { relativeTime, shortDevice } from "@/components/format";
import { SettingsTabs } from "@/components/settings-tabs";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading, ShortId } from "@/components/ui";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { requireMemberInScope, requireWorkspaceOwner } from "@/lib/auth/guards.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import {
  approveDeviceLink,
  ownerRemoveDeviceLink,
  rejectDeviceLink,
} from "@/lib/db/identity.server";
import {
  type Fleet,
  type FleetDevice,
  type FleetFreshness,
  type FleetSkillState,
  type FleetSkillStatus,
  fleetOf,
} from "@/lib/db/queries.fleet.server";
import { useWsPath } from "@/lib/ws-path";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Linked devices · ${params.ws ?? "Workspace"}` }];
}

/**
 * The workspace Devices page (the Settings section's Devices tab) — DEVICE-LINK-driven: it
 * enumerates the workspace's linked devices (a device is registered once, linked per
 * workspace) and the version each one last reported, and — when the device-approval knob
 * holds them — the PENDING links awaiting an owner. Owner arms: approve/reject a pending
 * link, remove any link. Removing a link ends delivery to that device here; bytes already on
 * the machine stay there. Signing a device out whole stays SELF-only on the account page.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { actor } = await requireMemberInScope(request, params);
  return { fleet: await fleetOf(actor), isOwner: actor.role === "owner" };
}

/**
 * ONE action, dispatched on the hidden `intent` — all three link arms are OWNER-only
 * guard-gated acts (a loader gate never extends to an action). The data layer lands the
 * audit row of every landed act in its own transaction (`link_approved` / `link_rejected` /
 * `device_unlinked`); the route records the attempts it never sees — mangled forms, faults.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  // The membership FLOOR, hoisted above the intent dispatch: the unmatched-intent 400 must
  // never answer a non-member (in multi tenancy `:ws` is a guessable public name slug).
  const { workspace } = await requireMemberInScope(request, params);
  const owner = await requireWorkspaceOwner(request, workspace.id);
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  const deviceId = String(formData.get("device_id") ?? "").trim();
  const run =
    intent === "approve-link"
      ? approveDeviceLink
      : intent === "reject-link"
        ? rejectDeviceLink
        : intent === "remove-link"
          ? ownerRemoveDeviceLink
          : null;
  if (run === null || deviceId.length === 0) {
    return { status: "error" as const };
  }
  let outcome: "approved" | "rejected" | "removed" | "unknown_link";
  try {
    outcome = await run(owner, workspace.id, deviceId);
  } catch {
    await recordAdminEvent(owner, {
      kind: intent.replace("-", "_"),
      subject: deviceId,
      outcome: "error",
    });
    return { status: "error" as const };
  }
  // unknown_link — the row vanished between render and submit (a concurrent act): the
  // revalidated page shows the truth; nothing to confirm or refuse.
  return { status: outcome };
}

export default function FleetPage() {
  const { fleet, isOwner } = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();
  const wsPath = useWsPath();
  const active = fleet.devices.filter((d) => d.linkStatus === "active");
  const pending = fleet.devices.filter((d) => d.linkStatus === "pending");
  return (
    <TooltipProvider>
      <div className="space-y-8">
        <PageHeader
          title="Linked devices"
          meta={<FleetMeta fleet={fleet} />}
          actions={
            <>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Link to="/account/devices" className={buttonClasses("quiet")}>
                    Your devices
                  </Link>
                </TooltipTrigger>
                <TooltipContent>
                  Devices are self-service — each person manages their own from their account page.
                </TooltipContent>
              </Tooltip>
              <Link to={wsPath("")} className={buttonClasses("quiet")}>
                Back to workspace
              </Link>
            </>
          }
        />
        <SettingsTabs active="devices" />
        {actionData !== undefined && <ActionReceipt status={actionData.status} />}
        <IntroCopy wholeFleet={fleet.wholeFleet} />
        {(pending.length > 0 || fleet.deviceApproval === "on") && (
          <PendingLinks devices={pending} isOwner={isOwner} />
        )}
        <LinkedDevices
          devices={active}
          wholeFleet={fleet.wholeFleet}
          isOwner={isOwner}
          stalenessWindowMs={fleet.stalenessWindowMs}
        />
      </div>
    </TooltipProvider>
  );
}

/**
 * The post-action receipt — a calm one-liner after an owner arm lands (the page also revalidates,
 * so the truth is already on screen; this names what happened). The Remove line says plainly that
 * severing a link ends delivery and reporting but leaves the copies already on the device in place.
 */
function ActionReceipt({ status }: { status: string }) {
  const line: Record<string, string> = {
    approved: "Link approved — the device receives on its next sync.",
    rejected: "Link rejected. The device isn't linked here; it can ask again later.",
    removed:
      "Link removed. Future delivery and reporting stop — the copies already on that device stay put.",
    unknown_link: "That link was already gone — the page is up to date.",
    error: "That didn't go through. Try again.",
  };
  const text = line[status];
  if (text === undefined) {
    return null;
  }
  return (
    <p role="status" className={status === "error" ? "text-red-700 text-sm" : "text-dim text-sm"}>
      {text}
    </p>
  );
}

function FleetMeta({ fleet }: { fleet: Fleet }) {
  const count = fleet.devices.filter((d) => d.linkStatus === "active").length;
  const pending = fleet.devices.length - count;
  const stale = fleet.devices.filter(
    (d) => d.linkStatus === "active" && d.freshness === "stale",
  ).length;
  return (
    <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
      <span>{count === 1 ? "1 linked device" : `${count} linked devices`}</span>
      {pending > 0 && (
        <>
          <span aria-hidden="true">·</span>
          <span>{pending === 1 ? "1 pending" : `${pending} pending`}</span>
        </>
      )}
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
          Every device linked to this workspace and the version each one last reported. Use it to
          confirm a change has reached the fleet — after a fix lands, watch until every non-stale
          device reads <em className="text-ink not-italic">current</em>.
        </>
      ) : (
        <>
          These are your own devices linked to this workspace and the version each one last
          reported. Reviewers and owners see the whole fleet.
        </>
      )}
    </p>
  );
}

/**
 * The pending section: links awaiting an owner (the device-approval knob holds a non-owner's
 * new link here). Approve activates delivery; reject deletes the link — the device can ask
 * again later. Owner-only arms; everyone seated sees the queue exists.
 */
function PendingLinks({ devices, isOwner }: { devices: FleetDevice[]; isOwner: boolean }) {
  return (
    <section aria-labelledby="pending-links-heading" className="space-y-3">
      <SectionHeading>
        <span id="pending-links-heading">Pending links</span>
      </SectionHeading>
      <p className="text-dim text-sm leading-relaxed">
        Device approval is required here: a link asked for by a non-owner waits until an owner
        approves it. Nothing is delivered over a pending link.
      </p>
      {devices.length === 0 ? (
        <p className="text-faint text-sm">No links awaiting approval.</p>
      ) : (
        <Card className="overflow-hidden">
          <ul>
            {devices.map((device) => (
              <li
                key={device.deviceId}
                data-testid={`fleet-pending-${device.deviceId}`}
                className="flex flex-wrap items-center gap-x-3 gap-y-2 border-line-soft border-b px-4 py-3 last:border-b-0"
              >
                <span className="text-ink text-sm">{device.displayName}</span>
                <span className="font-mono text-faint text-xs">{shortDevice(device.deviceId)}</span>
                <span className="text-dim text-sm">
                  {device.ownerDisplay}{" "}
                  <span className="text-faint text-xs">{device.ownerEmail}</span>
                </span>
                <span className="text-faint text-xs">
                  asked {relativeTime(new Date(device.linkedAtMs))}
                </span>
                {isOwner && (
                  <span className="ml-auto flex flex-wrap items-center gap-2">
                    <LinkArm intent="approve-link" deviceId={device.deviceId} label="Approve" />
                    <LinkArm
                      intent="reject-link"
                      deviceId={device.deviceId}
                      label="Reject"
                      tone="danger"
                    />
                  </span>
                )}
              </li>
            ))}
          </ul>
        </Card>
      )}
    </section>
  );
}

function LinkedDevices({
  devices,
  wholeFleet,
  isOwner,
  stalenessWindowMs,
}: {
  devices: FleetDevice[];
  wholeFleet: boolean;
  isOwner: boolean;
  stalenessWindowMs: number;
}) {
  if (devices.length === 0) {
    return (
      <div className="rounded-lg border border-line-soft border-dashed bg-panel px-6 py-12 text-center">
        <h2 className="font-display font-semibold text-base text-ink tracking-[-0.02em]">
          No devices linked yet
        </h2>
        <p className="mx-auto mt-2 max-w-md text-dim text-sm leading-relaxed">
          A device appears here when it links to this workspace and reports what it is running — run{" "}
          <code className="rounded bg-panel2 px-1.5 py-0.5 font-mono text-[13px]">
            topos follow
          </code>{" "}
          on it, then let it sync once.
        </p>
      </div>
    );
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
              key={device.deviceId}
              device={device}
              showOwner={false}
              isOwner={false}
              stalenessWindowMs={stalenessWindowMs}
            />
          ))}
        </div>
      </section>
    );
  }

  const groups = groupByOwner(devices);
  return (
    <section aria-labelledby="fleet-heading" className="space-y-6">
      <div className="space-y-2">
        <SectionHeading>
          <span id="fleet-heading">Linked devices</span>
        </SectionHeading>
        {isOwner && (
          <p className="text-faint text-sm leading-relaxed">
            Removing a device&apos;s link ends delivery and reporting here — the copies already on
            the machine stay put and must be chased by hand where it matters. Signing a device out
            whole is self-service, from{" "}
            <Link to="/account/devices" className="text-ink underline decoration-hairline">
              your devices
            </Link>
            .
          </p>
        )}
      </div>
      {groups.map(([ownerUserId, group]) => (
        <div key={ownerUserId} className="space-y-3">
          <h3 className="font-medium text-ink text-sm">
            {group[0]?.ownerDisplay}{" "}
            <span className="font-normal text-faint text-xs">{group[0]?.ownerEmail}</span>
          </h3>
          <div className="space-y-3">
            {group.map((device) => (
              <DeviceCard
                key={device.deviceId}
                device={device}
                showOwner={false}
                isOwner={isOwner}
                stalenessWindowMs={stalenessWindowMs}
              />
            ))}
          </div>
        </div>
      ))}
    </section>
  );
}

/** One link arm — a two-step in-place confirm (people-affecting grade, never type-the-name). */
function LinkArm({
  intent,
  deviceId,
  label,
  tone = "primary",
}: {
  intent: "approve-link" | "reject-link" | "remove-link";
  deviceId: string;
  label: string;
  tone?: "primary" | "quiet" | "danger";
}) {
  const navigation = useNavigation();
  const pending =
    navigation.state !== "idle" &&
    navigation.formData?.get("intent") === intent &&
    navigation.formData?.get("device_id") === deviceId;
  return (
    <Form method="post">
      <input type="hidden" name="intent" value={intent} />
      <input type="hidden" name="device_id" value={deviceId} />
      <ConfirmButton label={label} tone={tone} pending={pending} />
    </Form>
  );
}

function DeviceCard({
  device,
  showOwner,
  isOwner,
  stalenessWindowMs,
}: {
  device: FleetDevice;
  showOwner: boolean;
  isOwner: boolean;
  stalenessWindowMs: number;
}) {
  return (
    <Card className="overflow-hidden">
      <div data-testid={`fleet-device-${device.deviceId}`} className="space-y-3 px-4 py-3">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
          <span className="text-ink text-sm">{device.displayName}</span>
          <span className="font-mono text-faint text-xs">{shortDevice(device.deviceId)}</span>
          {showOwner && (
            <span className="text-dim text-sm">
              {device.ownerDisplay} <span className="text-faint text-xs">{device.ownerEmail}</span>
            </span>
          )}
          <span className="ml-auto flex flex-wrap items-center gap-1.5">
            <FreshnessChip freshness={device.freshness} />
            {isOwner && (
              <LinkArm
                intent="remove-link"
                deviceId={device.deviceId}
                label="Remove"
                tone="danger"
              />
            )}
          </span>
        </div>
        <div className="text-faint text-xs">
          {device.lastSeenAtMs === null ? (
            <>Has never reported — no session has run on it since it linked.</>
          ) : (
            <>
              last seen {relativeTime(new Date(device.lastSeenAtMs))}
              {device.freshness === "stale" && (
                <> · past the {formatWindow(stalenessWindowMs)} window — chase by hand</>
              )}
            </>
          )}
        </div>
        <SkillStates skills={device.skills} />
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
          <ShortId value={skill.appliedVersionId} />
          <span className="text-faint text-xs">
            reported {relativeTime(new Date(skill.reportedAtMs))}
          </span>
          {skill.status === "detached" && (
            <span className="text-faint text-xs">
              last known state
              {skill.detachCause !== null && <> · {detachCauseLabel(skill.detachCause)}</>}
            </span>
          )}
          {skill.status === "behind" && skill.currentVersionId !== null && (
            <span className="text-faint text-xs">
              current is <ShortId value={skill.currentVersionId} />
            </span>
          )}
        </li>
      ))}
    </ul>
  );
}

/** The detach records' cause vocabulary, humanized; unknown codes fall through verbatim. */
function detachCauseLabel(cause: string): string {
  if (cause === "membership_removed") {
    return "the seat was removed";
  }
  if (cause === "channel_leave") {
    return "they left the channel";
  }
  return cause;
}

/**
 * A status chip that carries its own explainer as a tooltip — the reading legend rides the chips
 * themselves instead of a separate section. The trigger is a REAL focusable control (a button, so
 * it is keyboard-reachable) marked `cursor-help`; the tooltip opens on hover OR focus and never on
 * click, so the chip stays a passive, readable label.
 */
function StatusChip({
  tone,
  text,
  tip,
}: {
  tone: "neutral" | "verified" | "pending";
  text: string;
  tip: string;
}) {
  return (
    <Tooltip>
      <TooltipTrigger
        type="button"
        className="cursor-help rounded-full focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
      >
        <Chip tone={tone}>{text}</Chip>
      </TooltipTrigger>
      <TooltipContent>{tip}</TooltipContent>
    </Tooltip>
  );
}

function FreshnessChip({ freshness }: { freshness: FleetFreshness }) {
  if (freshness === "fresh") {
    return (
      <StatusChip
        tone="verified"
        text="fresh"
        tip="Reported within the staleness window — a recent session confirmed this."
      />
    );
  }
  if (freshness === "stale") {
    return (
      <StatusChip
        tone="pending"
        text="stale"
        tip="No report within the staleness window. Devices report at the start of a session, so an idle machine reads stale until its next run — unconfirmed, not wrong."
      />
    );
  }
  return (
    <StatusChip
      tone="neutral"
      text="never reported"
      tip="This device has linked but no session has run on it yet, so it has never reported."
    />
  );
}

function SkillStatusChip({ status }: { status: FleetSkillStatus }) {
  if (status === "current") {
    return (
      <StatusChip
        tone="verified"
        text="current"
        tip="This device's copy matches the workspace's current version."
      />
    );
  }
  if (status === "behind") {
    return (
      <StatusChip
        tone="pending"
        text="behind"
        tip="This device is on an older version — its next update brings it current."
      />
    );
  }
  if (status === "excluded") {
    return (
      <StatusChip
        tone="neutral"
        text="excluded"
        tip="Opted out on this one device — it won't receive this skill. The copies already on the machine stay."
      />
    );
  }
  return (
    <StatusChip
      tone="neutral"
      text="detached"
      tip="A last-known state, frozen when delivery stopped reaching this person. The copies already on the machine stay."
    />
  );
}

/** A group of one person's devices, keyed by user id, preserving the query's order. */
function groupByOwner(devices: FleetDevice[]): [string, FleetDevice[]][] {
  const groups = new Map<string, FleetDevice[]>();
  for (const device of devices) {
    const list = groups.get(device.ownerUserId);
    if (list === undefined) {
      groups.set(device.ownerUserId, [device]);
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
