import type { LoaderFunctionArgs } from "react-router";
import { Link, useLoaderData } from "react-router";
import { relativeTime, shortDevice } from "@/components/format";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading, ShortId } from "@/components/ui";
import { notFound, requireMember } from "@/lib/auth/guards.server";
import {
  type DetachedCopyRow,
  detachedCopiesOf,
  type Fleet,
  type FleetDevice,
  type FleetFreshness,
  type FleetSkillState,
  type FleetSkillStatus,
  fleetOf,
} from "@/lib/db/queries.fleet.server";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Fleet · ${params.ws ?? "Workspace"}` }];
}

/**
 * The fleet page is a visibility surface, read-only by design: it enumerates every device that
 * touches the workspace and the version each one last reported, and it NAMES its blind spots
 * (stale devices, detached copies, per-device exclusions, removed members' devices) instead of
 * omitting them. It carries NO revoke arm — a device is a possession, revocation is self-only,
 * and the one place a device signs out is the owner's own /settings/devices page.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const actor = await requireMember(request, ws);
  const [fleet, detached] = await Promise.all([fleetOf(actor), detachedCopiesOf(actor)]);
  return { ws, fleet, detached };
}

export default function FleetPage() {
  const { ws, fleet, detached } = useLoaderData<typeof loader>();
  return (
    <div className="space-y-8">
      <PageHeader
        title="Fleet"
        meta={<FleetMeta fleet={fleet} />}
        actions={
          <>
            <Link to="/settings/devices" className={buttonClasses("quiet")}>
              Your devices
            </Link>
            <Link to={`/workspaces/${ws}`} className={buttonClasses("quiet")}>
              Back to workspace
            </Link>
          </>
        }
      />
      <IntroCopy wholeFleet={fleet.wholeFleet} />
      <FleetBody fleet={fleet} />
      {detached.length > 0 && <DetachedCopies rows={detached} />}
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
          Every enrolled device that touches this workspace and the version each one last reported.
          Use it to confirm a change has reached the fleet — after a fix lands, watch until every
          non-stale device reads <em className="text-ink not-italic">current</em>.
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

function FleetBody({ fleet }: { fleet: Fleet }) {
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
      />
      {removed.length > 0 && (
        <RemovedUpstream devices={removed} stalenessWindowMs={fleet.stalenessWindowMs} />
      )}
    </div>
  );
}

function PresentDevices({
  devices,
  wholeFleet,
  stalenessWindowMs,
}: {
  devices: FleetDevice[];
  wholeFleet: boolean;
  stalenessWindowMs: number;
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
              key={device.deviceId}
              device={device}
              showOwner={false}
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
      <SectionHeading>
        <span id="fleet-heading">Enrolled devices</span>
      </SectionHeading>
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
                stalenessWindowMs={stalenessWindowMs}
              />
            ))}
          </div>
        </div>
      ))}
    </section>
  );
}

/**
 * The removed-upstream blind spot, named and enumerated: devices whose owner no longer holds a
 * seat. Removal deleted the seat, never the device — the copies are still out there, and only
 * the person themselves can sign the device out.
 */
function RemovedUpstream({
  devices,
  stalenessWindowMs,
}: {
  devices: FleetDevice[];
  stalenessWindowMs: number;
}) {
  return (
    <section aria-labelledby="removed-upstream-heading" className="space-y-3">
      <SectionHeading>
        <span id="removed-upstream-heading">Removed upstream</span>
      </SectionHeading>
      <p className="text-dim text-sm leading-relaxed">
        The seat is gone, but these copies remain on their devices — this page can no longer move
        them, and the bytes already on disk must be chased by hand.
      </p>
      <div className="space-y-3">
        {devices.map((device) => (
          <DeviceCard
            key={device.deviceId}
            device={device}
            showOwner={true}
            stalenessWindowMs={stalenessWindowMs}
          />
        ))}
      </div>
    </section>
  );
}

function DeviceCard({
  device,
  showOwner,
  stalenessWindowMs,
}: {
  device: FleetDevice;
  showOwner: boolean;
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
            {device.removedUpstream && <RemovedChip />}
            {device.revoked && <RevokedChip />}
          </span>
        </div>
        <div className="text-faint text-xs">
          {device.lastSeenAtMs === null ? (
            <>Has never reported — no session has run on it since it enrolled.</>
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
 * The standing detach records, person-joined — the chase list that survives even when the
 * device rows themselves are revoked or quiet: whose copies froze, of what, and why.
 */
function DetachedCopies({ rows }: { rows: DetachedCopyRow[] }) {
  return (
    <section aria-labelledby="detached-copies-heading" className="space-y-3">
      <SectionHeading>
        <span id="detached-copies-heading">Detached copies</span>
      </SectionHeading>
      <p className="text-dim text-sm leading-relaxed">
        Copies delivery no longer reaches — frozen where their devices last applied them. Sync
        cannot move or recall them; chase them by hand where it matters.
      </p>
      <Card className="overflow-hidden">
        <ul>
          {rows.map((row) => (
            <li
              key={`${row.userId}:${row.bundleId}`}
              className="flex flex-wrap items-center gap-x-3 gap-y-1 border-line-soft border-b px-4 py-3 last:border-b-0"
            >
              <span className="text-ink text-sm">{row.display}</span>
              <span className="font-mono text-dim text-xs">{row.bundleName ?? row.bundleId}</span>
              <span className="text-faint text-xs">
                {detachCauseLabel(row.cause)} · {relativeTime(new Date(row.createdAt))}
              </span>
            </li>
          ))}
        </ul>
      </Card>
    </section>
  );
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
  if (status === "excluded") {
    return <Chip tone="neutral">excluded</Chip>;
  }
  if (status === "removed_upstream") {
    return <Chip tone="neutral">removed upstream</Chip>;
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
 * machine reads stale. Naming each state here keeps the blind spots explicit — the page
 * enumerates them, it never hides them.
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
          state, frozen when delivery stopped reaching the person; the bytes stay on their machine
          and this page can no longer move them.
        </p>
        <p>
          <em className="text-ink not-italic">Excluded</em> copies were opted out on that one device
          — the person still follows the skill elsewhere; this device holds what it held.
        </p>
        <p>
          <em className="text-ink not-italic">Removed upstream</em> devices belong to a person whose
          seat is gone; the copies remain on their devices and must be chased by hand.
        </p>
        <p>
          Signing a device out is SELF-service — a device is a possession, and its owner does it
          from{" "}
          <Link to="/settings/devices" className="text-ink underline decoration-hairline">
            your devices
          </Link>
          . This page watches; it doesn&apos;t reach into pockets.
        </p>
        <p>
          After publishing a scrubbed version, watch until every non-stale device reads{" "}
          <em className="text-ink not-italic">current</em>, then enumerate the stale, detached,
          excluded, and removed-upstream copies and chase them directly — none of them are silently
          omitted here.
        </p>
      </Card>
    </section>
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
