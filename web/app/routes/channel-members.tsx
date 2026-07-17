import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, useFetcher, useLoaderData } from "react-router";
import { ChannelHeader } from "@/components/channel/channel-header";
import { ChannelTabs } from "@/components/channel/channel-tabs";
import { relativeTime } from "@/components/format";
import { buttonClasses, Card, SectionHeading } from "@/components/ui";
import { notFound, requireMember, workspaceInScope } from "@/lib/auth/guards.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import {
  type ChannelDetail as ChannelDetailData,
  channelDetail,
  optInDefaultChannel,
  optOutDefaultChannel,
} from "@/lib/db/queries.channels.server";
import { useWsPath } from "@/lib/ws-path";

export function meta({ params }: { params: { channel?: string } }) {
  return [{ title: `Members · #${params.channel ?? "channel"}` }];
}

/**
 * The channel's MEMBERS tab — a member-only sibling of the Skills face. Named channels render their
 * explicit membership rows; the default `everyone` channel renders the structural note plus the
 * viewer's OWN leave/rejoin stance (the self-service opt-out, nobody else's). Same guard-then-read
 * order as every channel page: requireMember before any data, then the DB read as the uniform 404.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const workspace = await workspaceInScope(params);
  const channel = params.channel;
  if (!channel) {
    notFound();
  }
  const actor = await requireMember(request, workspace.id);
  const detail = await channelDetail(actor, channel);
  if (detail === undefined) {
    notFound();
  }
  return { detail };
}

/** The typed reply the default channel's leave/rejoin fetcher reads back. */
type StanceActionData = { form: "stance"; error: string } | { form: "unknown"; error: string };

/**
 * The default channel's self-service stance — the signed-in member's OWN leave/rejoin, member-level
 * and deliberately step-up-less (it moves nobody's delivery but their own). The data layer lands the
 * audit row of every landed act in its own transaction; the route records the faults it never sees.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const workspace = await workspaceInScope(params);
  const ws = workspace.id;
  const channel = params.channel;
  if (!channel) {
    notFound();
  }
  // The membership FLOOR, hoisted above the intent dispatch: the unmatched-intent 400 must never
  // answer a non-member — in multi tenancy `:ws` is a guessable public name slug, so a 400-vs-404
  // split would be a workspace-existence oracle the GET faces deliberately close.
  await requireMember(request, workspace.id);
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  if (intent === "leave-default") {
    return leaveDefaultIntent(request, ws);
  }
  if (intent === "rejoin-default") {
    return rejoinDefaultIntent(request, ws);
  }
  return data<StanceActionData>({ form: "unknown", error: "Unknown action." }, { status: 400 });
}

/** Leave the default channel — the member's own stance; the DAL writes the detach records. */
async function leaveDefaultIntent(request: Request, ws: string) {
  const actor = await requireMember(request, ws);
  try {
    await optOutDefaultChannel(actor);
  } catch {
    await recordAdminEvent(actor, { kind: "member_left", subject: ws, outcome: "error" });
    return data<StanceActionData>(
      { form: "stance", error: "That didn't go through. Try again." },
      { status: 500 },
    );
  }
  return data<StanceActionData>({ form: "stance", error: "" });
}

/** Rejoin the default channel — deletes the opt-out; re-entitled detach records heal. */
async function rejoinDefaultIntent(request: Request, ws: string) {
  const actor = await requireMember(request, ws);
  try {
    await optInDefaultChannel(actor);
  } catch {
    await recordAdminEvent(actor, { kind: "member_joined", subject: ws, outcome: "error" });
    return data<StanceActionData>(
      { form: "stance", error: "That didn't go through. Try again." },
      { status: 500 },
    );
  }
  return data<StanceActionData>({ form: "stance", error: "" });
}

export default function ChannelMembers() {
  const { detail } = useLoaderData<typeof loader>();
  const wsPath = useWsPath();
  return (
    <div className="space-y-6">
      <ChannelHeader name={detail.name} mode={detail.mode} isDefault={detail.isDefault} />
      <ChannelTabs basePath={wsPath(`channels/${detail.name}`)} active="members" />
      <MembersSection detail={detail} />
    </div>
  );
}

/** The channel's people — the structural note + the viewer's own stance for the default,
 * else the explicit membership list. */
function MembersSection({ detail }: { detail: ChannelDetailData }) {
  return (
    <section aria-labelledby="members-heading" className="space-y-3">
      <SectionHeading>
        <span id="members-heading">Members</span>
      </SectionHeading>
      {detail.isDefault ? (
        <Card className="space-y-3 px-4 py-3">
          <p className="text-dim text-sm">
            <span className="font-medium text-ink">everyone</span> reaches every member of the
            workspace who hasn&apos;t opted out —{" "}
            {detail.defaultMemberCount === 1 ? "1 member" : `${detail.defaultMemberCount} members`}{" "}
            right now. Its membership is the roster minus self opt-outs, so there are no rows to add
            or remove.
          </p>
          <DefaultStanceForm viewerIsMember={detail.viewerIsMember} />
        </Card>
      ) : detail.members.length === 0 ? (
        <p className="text-dim text-sm">No members yet.</p>
      ) : (
        <Card className="overflow-hidden">
          <ul>
            {detail.members.map((member) => (
              <li
                key={member.userId}
                className="flex flex-wrap items-center gap-x-3 gap-y-1 border-line-soft border-b px-4 py-3 last:border-b-0"
              >
                <span className="min-w-0 truncate font-medium text-ink text-sm">
                  {member.display}
                </span>
                <span className="text-faint text-xs">
                  joined {relativeTime(new Date(member.addedAt))}
                </span>
              </li>
            ))}
          </ul>
        </Card>
      )}
    </section>
  );
}

/**
 * The default channel's self-service stance — the viewer's own leave/rejoin, nobody else's.
 * Leaving stops delivery of everyone-channel skills to YOUR devices; the copies they already
 * hold freeze in place (the honest boundary — sync never deletes local work). Rejoining
 * resumes delivery on the next update.
 */
function DefaultStanceForm({ viewerIsMember }: { viewerIsMember: boolean }) {
  const fetcher = useFetcher<StanceActionData>();
  const pending = fetcher.state !== "idle";
  const error =
    fetcher.data?.form === "stance" && fetcher.data.error.length > 0
      ? fetcher.data.error
      : undefined;
  return (
    <div className="space-y-2 border-line-soft border-t pt-3">
      {viewerIsMember ? (
        <>
          <p className="text-dim text-sm">
            You&apos;re in. Leaving stops delivery of this channel&apos;s skills to your devices —
            the copies they already hold freeze in place; nothing is deleted.
          </p>
          <fetcher.Form method="post">
            <input type="hidden" name="intent" value="leave-default" />
            <button type="submit" disabled={pending} className={buttonClasses("quiet")}>
              {pending ? "Leaving…" : "Leave everyone"}
            </button>
          </fetcher.Form>
        </>
      ) : (
        <>
          <p className="text-dim text-sm">
            You&apos;ve opted out — this channel&apos;s skills aren&apos;t delivered to your
            devices. Rejoining resumes delivery on your next update.
          </p>
          <fetcher.Form method="post">
            <input type="hidden" name="intent" value="rejoin-default" />
            <button type="submit" disabled={pending} className={buttonClasses("quiet")}>
              {pending ? "Rejoining…" : "Rejoin everyone"}
            </button>
          </fetcher.Form>
        </>
      )}
      {error !== undefined && (
        <p className="text-red-600 text-sm" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
