import type { LoaderFunctionArgs } from "react-router";
import { Link, useLoaderData } from "react-router";
import { relativeTime } from "@/components/format";
import { buttonClasses, Card, PageHeader } from "@/components/ui";
import { notFound, requireMember } from "@/lib/auth/guards.server";
import { type ChannelEvent, channelHistory } from "@/lib/db/queries.channels.server";

export function meta({ params }: { params: { channel?: string } }) {
  return [{ title: `History · #${params.channel ?? "channel"}` }];
}

/**
 * The channel audit trail — the trigger-emitted `channel_events`, newest first, bounded at 100
 * with an "older events exist" marker past the window.
 *
 * The `channel_events` rows SURVIVE a channel's deletion (the log is append-only), but this page
 * resolves the channel by NAME, so it 404s once the channel row is gone. That is the accepted
 * shape: the history is reachable only through a live channel; the deletion trail is retained for
 * an operator's direct audit, not re-surfaced here after the row disappears.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  const channel = params.channel;
  if (!ws || !channel) {
    notFound();
  }
  const actor = await requireMember(request, ws);
  const history = await channelHistory(actor, channel, { limit: 100 });
  if (history === undefined) {
    notFound();
  }
  return { ws, channel, history };
}

export default function ChannelHistory() {
  const { ws, channel, history } = useLoaderData<typeof loader>();
  return (
    <div className="space-y-8">
      <PageHeader
        title={
          <>
            <span className="text-faint" aria-hidden="true">
              #
            </span>
            {history.channelName} history
          </>
        }
        actions={
          <Link to={`/workspaces/${ws}/channels/${channel}`} className={buttonClasses("quiet")}>
            Back to channel
          </Link>
        }
      />
      {history.events.length === 0 ? (
        <p className="text-dim text-sm">No recorded events.</p>
      ) : (
        <Card className="overflow-hidden">
          <ul>
            {history.events.map((event) => (
              <EventRow key={event.id} event={event} />
            ))}
          </ul>
        </Card>
      )}
      {history.hasMore && (
        <p className="text-faint text-xs">
          Older events exist beyond this window — the {history.events.length} most recent are shown.
        </p>
      )}
    </div>
  );
}

/** Humanize the trigger's event vocabulary; unknown codes fall through verbatim. */
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

/**
 * One audit row: the event, the skill or person it touched (when the event names one), who drove
 * it, and when.
 */
function EventRow({ event }: { event: ChannelEvent }) {
  const target = event.skillId ?? event.principal;
  return (
    <li className="flex flex-wrap items-center gap-x-3 gap-y-1 border-line-soft border-b px-4 py-3 last:border-b-0">
      <span className="font-medium text-ink text-sm">
        {EVENT_LABELS[event.event] ?? event.event}
      </span>
      {target !== null && <code className="font-mono text-dim text-xs">{target}</code>}
      <span className="text-faint text-xs">
        by {event.actor} · {relativeTime(new Date(event.createdAt))}
      </span>
    </li>
  );
}
