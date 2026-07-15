import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, redirect, useFetcher, useLoaderData } from "react-router";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading } from "@/components/ui";
import { notFound, requireMember } from "@/lib/auth/guards.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import {
  type ChannelCreateOutcome,
  type ChannelSummary,
  channelsOf,
  createChannel,
} from "@/lib/db/queries.channels.server";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Channels · ${params.ws ?? "Workspace"}` }];
}

/**
 * The channel list — every named group in the workspace, `everyone` first. Plain rows read
 * straight from the app's own tables: mode, the default marker, and the two reach counts
 * (skill references and members). Each row links to the channel's detail page; the create
 * form below is MEMBER-level — the same grade as the device lane's create-on-first-use
 * placement, so the browser and the CLI agree on who may mint a group.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const actor = await requireMember(request, ws);
  return { ws, channels: await channelsOf(actor) };
}

/** The create form's typed reply on a NON-redirect (a landed create redirects to the channel). */
interface CreateChannelActionData {
  intent: "create-channel";
  error: string;
  submittedName?: string;
}

/**
 * CREATE — the one action here. Member-level and deliberately step-up-less (creating an empty
 * group destroys nothing); the unique index is the race arbiter, so a create-race loser gets
 * the honest name-taken copy, never a 500. The create lands its audit row in the DAL's own
 * transaction; the route records only the refusals the DAL typed back.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const formData = await request.formData();
  if (String(formData.get("intent") ?? "") !== "create-channel") {
    return data<CreateChannelActionData>(
      { intent: "create-channel", error: "Unknown action." },
      { status: 400 },
    );
  }
  const actor = await requireMember(request, ws);
  const name = String(formData.get("name") ?? "").trim();
  let outcome: ChannelCreateOutcome;
  try {
    outcome = await createChannel(actor, name);
  } catch {
    await recordAdminEvent(actor, { kind: "channel_created", subject: name, outcome: "error" });
    return data<CreateChannelActionData>(
      {
        intent: "create-channel",
        error: "That didn't go through. Try again.",
        submittedName: name,
      },
      { status: 500 },
    );
  }
  if (outcome.outcome === "created") {
    throw redirect(`/workspaces/${ws}/channels/${name}`);
  }
  await recordAdminEvent(actor, {
    kind: "channel_created",
    subject: name,
    detail: outcome.outcome,
    outcome: "denied",
  });
  return data<CreateChannelActionData>(
    {
      intent: "create-channel",
      error:
        outcome.outcome === "name_taken"
          ? `A channel named #${name} already exists.`
          : "Channel names use lowercase letters, numbers, and hyphens (up to 64 characters).",
      submittedName: name,
    },
    { status: 400 },
  );
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
      <CreateChannelSection />
    </div>
  );
}

/**
 * One channel row — the `#`-prefixed name (the CLI's channel vocabulary), the mode chip, the
 * default marker on `everyone`, and the skill + member counts. The whole row is the click
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
          {channel.isDefault && (
            <span className="text-faint text-xs">every member, minus opt-outs</span>
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

/** The member-level create form — name in, honest refusal copy back, redirect on success. */
function CreateChannelSection() {
  const fetcher = useFetcher<CreateChannelActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;
  return (
    <section aria-labelledby="create-channel-heading" className="space-y-3">
      <SectionHeading>
        <span id="create-channel-heading">New channel</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-dim text-sm">
          A channel is a named group: skills placed in it are delivered to its members. Any member
          can create one — the CLI creates them on first use the same way.
        </p>
        <fetcher.Form method="post" className="flex flex-wrap items-end gap-2">
          <input type="hidden" name="intent" value="create-channel" />
          <label className="block flex-1">
            <span className="mb-1 block font-medium text-sm text-dim">Channel name</span>
            <input
              type="text"
              name="name"
              required
              autoComplete="off"
              spellCheck={false}
              placeholder="frontend-guild"
              pattern="[a-z0-9][a-z0-9-]*"
              maxLength={64}
              key={state?.submittedName ?? "initial"}
              defaultValue={state?.submittedName ?? ""}
              className="block h-11 w-full min-w-56 rounded-md border border-line px-3 text-ink text-sm placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25"
            />
          </label>
          <button type="submit" disabled={pending} className={`${buttonClasses("quiet")} min-h-11`}>
            {pending ? "Creating…" : "Create channel"}
          </button>
        </fetcher.Form>
        {state !== undefined && state.error.length > 0 && (
          <p className="text-red-600 text-sm" role="alert">
            {state.error}
          </p>
        )}
      </Card>
    </section>
  );
}
