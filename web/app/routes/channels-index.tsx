import type { LoaderFunctionArgs } from "react-router";
import { Link, useLoaderData } from "react-router";
import { Card, Chip, PageHeader, SectionHeading } from "@/components/ui";
import { notFound, requireMember } from "@/lib/auth/guards.server";
import { type ChannelSummary, channelsOf } from "@/lib/db/queries.channels.server";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Channels · ${params.ws ?? "Workspace"}` }];
}

/**
 * The channel list — every named group in the workspace, `everyone` first. Plain rows read
 * straight from the directory: mode, the structural `everyone` marker, and the two reach counts
 * (skill references and members). Each row links to the channel's detail page.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const actor = await requireMember(request, ws);
  return { ws, channels: await channelsOf(actor) };
}

export default function ChannelsIndex() {
  const { ws, channels } = useLoaderData<typeof loader>();
  return (
    <div className="space-y-8">
      <PageHeader
        title="Channels"
        meta={<span>{channels.length === 1 ? "1 channel" : `${channels.length} channels`}</span>}
      />
      {channels.length === 0 ? (
        <p className="text-dim text-sm">No channels yet.</p>
      ) : (
        <section aria-labelledby="channels-heading" className="space-y-3">
          <SectionHeading>
            <span id="channels-heading">Channels</span>
          </SectionHeading>
          <Card className="overflow-hidden">
            <ul>
              {channels.map((channel) => (
                <ChannelRow key={channel.channelId} ws={ws} channel={channel} />
              ))}
            </ul>
          </Card>
        </section>
      )}
    </div>
  );
}

/**
 * One channel row — the `#`-prefixed name (the CLI's channel vocabulary), the mode chip, the
 * structural marker on `everyone`, and the skill + member counts. The whole row is the click
 * target, keyed on the channel NAME (the user-facing key the detail page routes on).
 */
function ChannelRow({ ws, channel }: { ws: string; channel: ChannelSummary }) {
  return (
    <li className="border-line-soft border-b last:border-b-0">
      <Link
        to={`/workspaces/${ws}/channels/${channel.name}`}
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
          {channel.builtin && (
            <span className="text-faint text-xs">every confirmed member, structural</span>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-faint text-xs">
          <span>{channel.skillCount === 1 ? "1 skill" : `${channel.skillCount} skills`}</span>
          <span aria-hidden="true">·</span>
          <span>{channel.memberCount === 1 ? "1 member" : `${channel.memberCount} members`}</span>
        </div>
      </Link>
    </li>
  );
}
