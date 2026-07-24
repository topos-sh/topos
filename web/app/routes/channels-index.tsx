import type { LoaderFunctionArgs } from "react-router";
import { Link, useLoaderData } from "react-router";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading } from "@/components/ui";
import { requireMemberInScope } from "@/lib/auth/guards.server";
import { type ChannelSummary, channelsOf } from "@/lib/db/queries.channels.server";
import { useWsPath } from "@/lib/ws-path";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Channels · ${params.ws ?? "Workspace"}` }];
}

/**
 * The channel list — every named group in the workspace, `everyone` first. Plain rows read
 * straight from the app's own tables: mode, the default marker, and the two reach counts
 * (skill references and members). Each row links to the channel's detail page; the create form is
 * its own Rails-style `channels/new` route (channel-new.tsx).
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { actor } = await requireMemberInScope(request, params);
  return { channels: await channelsOf(actor) };
}

export default function ChannelsIndex() {
  const { channels } = useLoaderData<typeof loader>();
  const wsPath = useWsPath();
  return (
    <div className="space-y-8">
      <PageHeader
        title="Channels"
        meta={<span>{channels.length === 1 ? "1 channel" : `${channels.length} channels`}</span>}
        actions={
          <Link to={wsPath("channels/new")} className={buttonClasses("quiet")}>
            New channel
          </Link>
        }
      />
      {/* No empty branch: a workspace is born with #everyone, so the list is never empty. */}
      <section aria-labelledby="channels-heading" className="space-y-3">
        <SectionHeading>
          <span id="channels-heading">Channels</span>
        </SectionHeading>
        <Card className="overflow-hidden">
          <ul>
            {channels.map((channel) => (
              <ChannelRow key={channel.channelId} channel={channel} />
            ))}
          </ul>
        </Card>
        {channels.length === 1 && channels[0]?.isDefault && (
          <p className="text-faint text-sm">
            Every workspace starts with <span className="font-mono">#everyone</span>. Create a
            channel to share a set of skills with just the people who follow it.
          </p>
        )}
      </section>
    </div>
  );
}

/**
 * One channel row — the `#`-prefixed name (the CLI's channel vocabulary), the mode chip, the
 * default marker on `everyone`, and the skill + member counts. The whole row is the click
 * target, keyed on the channel NAME (the user-facing key the detail page routes on).
 */
function ChannelRow({ channel }: { channel: ChannelSummary }) {
  const wsPath = useWsPath();
  return (
    <li className="border-line-soft border-b last:border-b-0">
      <Link
        to={wsPath(`channels/${channel.name}`)}
        className="flex flex-col gap-1 px-4 py-3 hover:bg-panel2 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent"
      >
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
          <span className="min-w-0 truncate font-medium text-ink text-sm">
            <span className="text-faint" aria-hidden="true">
              #
            </span>
            {channel.name}
          </span>
          <Chip tone={channel.mode === "curated" ? "pending" : "neutral"}>{channel.mode}</Chip>
          {channel.isDefault && (
            <span className="text-faint text-xs">every member, minus opt-outs</span>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-faint text-xs">
          <span>{channel.skillCount === 1 ? "1 skill" : `${channel.skillCount} skills`}</span>
          <span aria-hidden="true">·</span>
          <span>
            {channel.audienceCount === 1 ? "1 person" : `${channel.audienceCount} people`}
          </span>
        </div>
      </Link>
    </li>
  );
}
