import type { LoaderFunctionArgs } from "react-router";
import { Link, useLoaderData } from "react-router";
import { relativeTime } from "@/components/format";
import { buttonClasses, Card, Chip, PageHeader } from "@/components/ui";
import { notFound, requireMember } from "@/lib/auth/guards.server";
import { type AuditEventRow, auditEventsForSubject } from "@/lib/db/audit.server";
import { channelKeyByName } from "@/lib/db/queries.channels.server";

export function meta({ params }: { params: { channel?: string } }) {
  return [{ title: `History · #${params.channel ?? "channel"}` }];
}

/**
 * The channel's audit trail — the `audit_event` rows whose SUBJECT is this channel's immutable
 * id, newest first, bounded at 100 with an "older events exist" marker past the window. The
 * ledger is append-only and keyed by the id, so the trail survives renames intact.
 *
 * The rows themselves SURVIVE a channel's deletion, but this page resolves the channel by
 * NAME, so it 404s once the row is gone. That is the accepted shape: history is reachable only
 * through a live channel; the deletion trail is retained for an operator's direct audit, not
 * re-surfaced here after the row disappears.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  const channel = params.channel;
  if (!ws || !channel) {
    notFound();
  }
  const actor = await requireMember(request, ws);
  const key = await channelKeyByName(actor, channel);
  if (key === undefined) {
    notFound();
  }
  const window = await auditEventsForSubject(actor, key.channelId);
  return { ws, channel, channelName: key.name, events: window.events, hasMore: window.hasMore };
}

export default function ChannelHistory() {
  const { ws, channel, channelName, events, hasMore } = useLoaderData<typeof loader>();
  return (
    <div className="space-y-8">
      <PageHeader
        title={
          <>
            <span className="text-faint" aria-hidden="true">
              #
            </span>
            {channelName} history
          </>
        }
        actions={
          <Link to={`/workspaces/${ws}/channels/${channel}`} className={buttonClasses("quiet")}>
            Back to channel
          </Link>
        }
      />
      {events.length === 0 ? (
        <p className="text-dim text-sm">No recorded events.</p>
      ) : (
        <Card className="overflow-hidden">
          <ul>
            {events.map((event) => (
              <EventRow key={event.id} event={event} />
            ))}
          </ul>
        </Card>
      )}
      {hasMore && (
        <p className="text-faint text-xs">
          Older events exist beyond this window — the {events.length} most recent are shown.
        </p>
      )}
    </div>
  );
}

/** Humanize the audit-kind vocabulary; unknown codes fall through verbatim. */
const EVENT_LABELS: Record<string, string> = {
  channel_created: "Channel created",
  channel_renamed: "Channel renamed",
  channel_deleted: "Channel deleted",
  mode_open: "Mode set to open",
  mode_curated: "Mode set to curated",
  skill_added: "Skill added",
  skill_removed: "Skill removed",
  member_joined: "Member joined",
  member_left: "Member left",
};

/** Pull the one display-worthy target out of an audit row's details jsonb, when it names one. */
function eventTarget(details: unknown): string | null {
  if (typeof details !== "object" || details === null) {
    return null;
  }
  const d = details as Record<string, unknown>;
  if (typeof d.from === "string" && typeof d.to === "string") {
    return `${d.from} → ${d.to}`;
  }
  for (const key of ["name", "skillId", "userId"]) {
    const value = d[key];
    if (typeof value === "string") {
      return value;
    }
  }
  return null;
}

/**
 * One audit row: the event, the target it names (when the details carry one), who drove it,
 * and when. A non-ok row renders its outcome — a refused attempt is as much a fact as a landed
 * act, and the trail shows both.
 */
function EventRow({ event }: { event: AuditEventRow }) {
  const target = eventTarget(event.details);
  return (
    <li className="flex flex-wrap items-center gap-x-3 gap-y-1 border-line-soft border-b px-4 py-3 last:border-b-0">
      <span className="font-medium text-ink text-sm">{EVENT_LABELS[event.kind] ?? event.kind}</span>
      {event.outcome !== "ok" && <Chip tone="unverified">{event.outcome}</Chip>}
      {target !== null && <code className="font-mono text-dim text-xs">{target}</code>}
      <span className="text-faint text-xs">
        by {event.actorDisplay} · {relativeTime(new Date(event.createdAt))}
      </span>
    </li>
  );
}
