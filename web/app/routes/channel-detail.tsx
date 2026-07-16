import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, redirect, useFetcher, useLoaderData } from "react-router";
import { relativeTime } from "@/components/format";
import { ResourcePage } from "@/components/resource-page";
import { StepUpFields, StepUpMethodProvider } from "@/components/step-up";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading } from "@/components/ui";
import {
  actorFromSession,
  notFound,
  requireMember,
  requireWorkspaceOwner,
  workspaceInScope,
} from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { requireStepUp, requireTypedName, stepUpMethod } from "@/lib/auth/step-up.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import {
  type ChannelDeleteOutcome,
  type ChannelDetail as ChannelDetailData,
  type ChannelRenameOutcome,
  channelDetail,
  channelRowById,
  deleteChannel,
  optInDefaultChannel,
  optOutDefaultChannel,
  renameChannel,
} from "@/lib/db/queries.channels.server";
import { useWsPath } from "@/lib/ws-path";
import { wsPathServer } from "@/lib/ws-url.server";

export function meta({ params }: { params: { channel?: string } }) {
  return [{ title: `#${params.channel ?? "channel"}` }];
}

/**
 * The channel FACE — resource address and canonical channel page as ONE route. Admission mirrors
 * the other faces: anonymous browser → the constant teaser; a signed-in member → the channel page
 * WITH chrome; a signed-in non-member (or unknown workspace slug) → the house 404.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const session = await getAuth().api.getSession({ headers: request.headers });
  const actor = actorFromSession(session);
  if (actor === null) {
    return { face: "teaser" as const };
  }
  const workspace = await workspaceInScope(params);
  const memberActor = await requireMember(request, workspace.id);
  const channel = params.channel;
  if (!channel) {
    notFound();
  }
  const detail = await channelDetail(memberActor, channel);
  // A miss is the uniform 404 — never a 403, never a "channel exists but…" oracle.
  if (detail === undefined) {
    notFound();
  }
  return {
    face: "page" as const,
    channel,
    detail,
    isOwner: memberActor.role === "owner",
    stepUpMethod: await stepUpMethod(memberActor.userId),
  };
}

/** The typed reply each ceremony returns on a NON-redirect (a landed owner act redirects away). */
type ChannelActionData =
  | { form: "rename"; error: string }
  | { form: "delete"; error: string }
  | { form: "stance"; error: string }
  | { form: "unknown"; error: string };

/**
 * ONE action, dispatched on the hidden `intent`. The owner ceremonies (rename, delete) RE-GUARD
 * with the owner gate and run the step-up ceremony BEFORE any write; the default channel's
 * leave/rejoin is the signed-in member's OWN stance — member-level, step-up-less (it moves
 * nobody's delivery but their own). The data layer lands the audit row of every landed act in
 * its own transaction; the route records the attempts it never sees.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const workspace = await workspaceInScope(params);
  const ws = workspace.id;
  const channel = params.channel;
  if (!channel) {
    notFound();
  }
  // The membership FLOOR, hoisted above the intent dispatch: every intent below requires at
  // least a member (most re-check owner/reviewer themselves), and the unmatched-intent 400 must
  // never answer a non-member — in multi tenancy `:ws` is a guessable public name slug, so a
  // 400-vs-404 split would be a workspace-existence oracle the GET faces deliberately close.
  await requireMember(request, workspace.id);
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  // The owner ceremonies key on the IMMUTABLE channel_id the page was LOADED with (a hidden
  // field) — resolving the URL's mutable name at action time could retarget a freed-and-reused
  // name; the id keeps a stale form acting on the channel the owner was actually looking at.
  const channelId = String(formData.get("channel_id") ?? "");
  if (intent === "rename-channel") {
    return renameChannelIntent(request, ws, workspace.name, channelId, formData);
  }
  if (intent === "delete-channel") {
    return deleteChannelIntent(request, ws, workspace.name, channelId, formData);
  }
  if (intent === "leave-default") {
    return leaveDefaultIntent(request, ws);
  }
  if (intent === "rejoin-default") {
    return rejoinDefaultIntent(request, ws);
  }
  return data<ChannelActionData>({ form: "unknown", error: "Unknown action." }, { status: 400 });
}

const RENAME_GENERIC_ERROR = "That rename didn't go through. Try again.";
const DELETE_GENERIC_ERROR = "That delete didn't go through. Try again.";

/** Map the rename ceremony's outcome codes to honest, member-facing copy. */
function renameErrorCopy(outcome: ChannelRenameOutcome, newName: string): string {
  switch (outcome) {
    case "bad_name":
      return "Channel names use lowercase letters, numbers, and hyphens (up to 64 characters).";
    case "name_taken":
      return `A channel named #${newName} already exists.`;
    case "builtin":
      return "The everyone channel is structural — it can't be renamed.";
    default:
      return "This channel no longer exists.";
  }
}

/** Map the delete ceremony's outcome codes to honest, member-facing copy. */
function deleteErrorCopy(outcome: ChannelDeleteOutcome): string {
  return outcome === "builtin"
    ? "The everyone channel is structural — it can't be deleted."
    : "This channel no longer exists.";
}

/**
 * RENAME — owner + step-up. On success redirect to the new channel URL; the ceremony's own
 * outcome codes (bad_name / name_taken / builtin) surface inline otherwise.
 */
async function renameChannelIntent(
  request: Request,
  ws: string,
  wsName: string,
  channelId: string,
  formData: FormData,
) {
  const owner = await requireWorkspaceOwner(request, ws);
  const newName = String(formData.get("new_name") ?? "").trim();
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "channel_renamed",
      subject: channelId,
      detail: "step_up",
      outcome: "denied",
    });
    return data<ChannelActionData>({ form: "rename", error: stepUp.error }, { status: 400 });
  }
  let outcome: ChannelRenameOutcome;
  try {
    outcome = await renameChannel(owner, channelId, newName);
  } catch {
    await recordAdminEvent(owner, {
      kind: "channel_renamed",
      subject: channelId,
      outcome: "error",
    });
    return data<ChannelActionData>(
      { form: "rename", error: RENAME_GENERIC_ERROR },
      { status: 500 },
    );
  }
  if (outcome === "renamed") {
    return redirect(wsPathServer(wsName, `channels/${newName}`));
  }
  await recordAdminEvent(owner, {
    kind: "channel_renamed",
    subject: channelId,
    detail: outcome,
    outcome: "denied",
  });
  return data<ChannelActionData>(
    { form: "rename", error: renameErrorCopy(outcome, newName) },
    { status: 400 },
  );
}

/**
 * DELETE — owner + step-up + type-the-channel-name. On success redirect to the channels index.
 * Deleting stops delivery THROUGH this channel only; skills another channel or a direct follow
 * still delivers keep flowing, and devices keep their local copies (an upstream withdrawal).
 */
async function deleteChannelIntent(
  request: Request,
  ws: string,
  wsName: string,
  channelId: string,
  formData: FormData,
) {
  const owner = await requireWorkspaceOwner(request, ws);
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "channel_deleted",
      subject: channelId,
      detail: "step_up",
      outcome: "denied",
    });
    return data<ChannelActionData>({ form: "delete", error: stepUp.error }, { status: 400 });
  }
  const row = await channelRowById(owner, channelId);
  if (row === undefined) {
    return data<ChannelActionData>(
      { form: "delete", error: "This channel no longer exists." },
      { status: 400 },
    );
  }
  // The typed name is anchored to the row's CURRENT name (server state): a channel renamed
  // between page load and submit refuses honestly rather than deleting under a stale name.
  const typed = requireTypedName(formData, row.name);
  if (!typed.ok) {
    await recordAdminEvent(owner, {
      kind: "channel_deleted",
      subject: channelId,
      detail: "confirm_name",
      outcome: "denied",
    });
    return data<ChannelActionData>({ form: "delete", error: typed.error }, { status: 400 });
  }
  let outcome: ChannelDeleteOutcome;
  try {
    outcome = await deleteChannel(owner, channelId);
  } catch {
    await recordAdminEvent(owner, {
      kind: "channel_deleted",
      subject: channelId,
      outcome: "error",
    });
    return data<ChannelActionData>(
      { form: "delete", error: DELETE_GENERIC_ERROR },
      { status: 500 },
    );
  }
  if (outcome === "deleted") {
    return redirect(wsPathServer(wsName, "channels"));
  }
  await recordAdminEvent(owner, {
    kind: "channel_deleted",
    subject: channelId,
    detail: outcome,
    outcome: "denied",
  });
  return data<ChannelActionData>(
    { form: "delete", error: deleteErrorCopy(outcome) },
    { status: 400 },
  );
}

/** Leave the default channel — the member's own stance; the DAL writes the detach records. */
async function leaveDefaultIntent(request: Request, ws: string) {
  const actor = await requireMember(request, ws);
  try {
    await optOutDefaultChannel(actor);
  } catch {
    await recordAdminEvent(actor, { kind: "member_left", subject: ws, outcome: "error" });
    return data<ChannelActionData>(
      { form: "stance", error: "That didn't go through. Try again." },
      { status: 500 },
    );
  }
  return data<ChannelActionData>({ form: "stance", error: "" });
}

/** Rejoin the default channel — deletes the opt-out; re-entitled detach records heal. */
async function rejoinDefaultIntent(request: Request, ws: string) {
  const actor = await requireMember(request, ws);
  try {
    await optInDefaultChannel(actor);
  } catch {
    await recordAdminEvent(actor, { kind: "member_joined", subject: ws, outcome: "error" });
    return data<ChannelActionData>(
      { form: "stance", error: "That didn't go through. Try again." },
      { status: 500 },
    );
  }
  return data<ChannelActionData>({ form: "stance", error: "" });
}

export default function ChannelDetail() {
  const data = useLoaderData<typeof loader>();
  if (data.face === "teaser") {
    return <ResourcePage />;
  }
  return (
    <StepUpMethodProvider method={data.stepUpMethod}>
      <ChannelDetailPage {...data} />
    </StepUpMethodProvider>
  );
}

function ChannelDetailPage({
  channel,
  detail,
  isOwner,
}: Extract<Awaited<ReturnType<typeof loader>>, { face: "page" }>) {
  const wsPath = useWsPath();
  return (
    <div className="space-y-8">
      <PageHeader
        title={
          <>
            <span className="text-faint" aria-hidden="true">
              #
            </span>
            {detail.name}
          </>
        }
        meta={
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <Chip tone={detail.mode === "curated" ? "pending" : "neutral"}>{detail.mode}</Chip>
            {detail.isDefault && <span>every member, minus opt-outs</span>}
          </div>
        }
        actions={
          <>
            <Link to={wsPath(`channels/${detail.name}/history`)} className={buttonClasses("quiet")}>
              History
            </Link>
            <Link to={wsPath("channels")} className={buttonClasses("quiet")}>
              All channels
            </Link>
          </>
        }
      />

      <SkillsSection skills={detail.skills} />
      <MembersSection detail={detail} />

      {isOwner &&
        (detail.isDefault ? (
          <BuiltinAdminNote />
        ) : (
          <section aria-labelledby="admin-heading" className="space-y-3">
            <SectionHeading>
              <span id="admin-heading">Owner controls</span>
            </SectionHeading>
            <RenameChannelForm channel={channel} channelId={detail.channelId} />
            <DeleteChannelForm channel={channel} channelId={detail.channelId} />
          </section>
        ))}
    </div>
  );
}

/** The skill references the channel delivers — each a link to its skill page (by catalog name). */
function SkillsSection({ skills }: { skills: ChannelDetailData["skills"] }) {
  const wsPath = useWsPath();
  return (
    <section aria-labelledby="skills-heading" className="space-y-3">
      <SectionHeading>
        <span id="skills-heading">Skills</span>
      </SectionHeading>
      {skills.length === 0 ? (
        <p className="text-dim text-sm">This channel references no skills yet.</p>
      ) : (
        <Card className="overflow-hidden">
          <ul>
            {skills.map((skill) => (
              <li key={skill.skillId} className="border-line-soft border-b last:border-b-0">
                {skill.status === "active" ? (
                  <Link
                    to={wsPath(`skills/${skill.name}`)}
                    className="flex items-center gap-2 px-4 py-3 hover:bg-panel2 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent"
                  >
                    <span className="min-w-0 truncate font-medium text-ink text-sm">
                      {skill.displayName ?? skill.name}
                    </span>
                  </Link>
                ) : (
                  <div className="flex items-center gap-2 px-4 py-3">
                    <span className="min-w-0 truncate text-dim text-sm">
                      {skill.displayName ?? skill.name}
                    </span>
                    <Chip tone="unverified">{skill.status}</Chip>
                  </div>
                )}
              </li>
            ))}
          </ul>
        </Card>
      )}
    </section>
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
  const fetcher = useFetcher<ChannelActionData>();
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

/** The `everyone` owner note — no controls, an honest statement of why. */
function BuiltinAdminNote() {
  return (
    <section aria-labelledby="admin-heading" className="space-y-3">
      <SectionHeading>
        <span id="admin-heading">Owner controls</span>
      </SectionHeading>
      <Card className="px-4 py-3">
        <p className="text-dim text-sm">
          The everyone channel is structural — it can't be renamed or deleted. Its membership is the
          roster, minus each person's own opt-out.
        </p>
      </Card>
    </section>
  );
}

/** The rename ceremony — step-up + the new name; the ceremony's outcome codes surface inline. */
function RenameChannelForm({ channel, channelId }: { channel: string; channelId: string }) {
  const fetcher = useFetcher<ChannelActionData>();
  const pending = fetcher.state !== "idle";
  const error = fetcher.data?.form === "rename" ? fetcher.data.error : undefined;
  return (
    <Card className="space-y-3 px-4 py-3">
      <div>
        <h3 className="font-medium text-ink text-sm">Rename this channel</h3>
        <p className="mt-1 text-dim text-sm">
          The channel keeps its skills, members, and history — only the name changes.
        </p>
      </div>
      <fetcher.Form method="post" className="space-y-3">
        <input type="hidden" name="intent" value="rename-channel" />
        <input type="hidden" name="channel_id" value={channelId} />
        <label className="block" htmlFor="rename-new-name">
          <span className="mb-1 block font-medium text-sm text-dim">New name</span>
          <input
            id="rename-new-name"
            type="text"
            name="new_name"
            required
            autoComplete="off"
            spellCheck={false}
            placeholder="new-channel-name"
            className="block h-11 w-full min-w-56 rounded-md border border-line px-3 text-ink text-sm placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25"
          />
        </label>
        <StepUpFields idPrefix={`rename-${channel}`} />
        {error && (
          <p className="text-red-600 text-sm" role="alert">
            {error}
          </p>
        )}
        <button type="submit" disabled={pending} className={`${buttonClasses("quiet")} min-h-11`}>
          {pending ? "Renaming…" : "Rename channel"}
        </button>
      </fetcher.Form>
    </Card>
  );
}

/** The delete ceremony — step-up + type-the-name; the copy states the semantics honestly. */
function DeleteChannelForm({ channel, channelId }: { channel: string; channelId: string }) {
  const fetcher = useFetcher<ChannelActionData>();
  const pending = fetcher.state !== "idle";
  const error = fetcher.data?.form === "delete" ? fetcher.data.error : undefined;
  return (
    <Card className="space-y-3 border-red-200 px-4 py-3">
      <div>
        <h3 className="font-medium text-ink text-sm">Delete this channel</h3>
        <p className="mt-1 text-dim text-sm">
          Deleting <span className="font-mono">#{channel}</span> stops delivery through it. Skills
          another channel or a direct follow still delivers keep flowing; devices treat what lapses
          as an upstream withdrawal and keep their local copies. This can't be undone from here.
        </p>
      </div>
      <fetcher.Form method="post" className="space-y-3">
        <input type="hidden" name="intent" value="delete-channel" />
        <input type="hidden" name="channel_id" value={channelId} />
        <StepUpFields idPrefix={`delete-${channel}`} typedName={channel} />
        {error && (
          <p className="text-red-600 text-sm" role="alert">
            {error}
          </p>
        )}
        <button type="submit" disabled={pending} className={`${buttonClasses("danger")} min-h-11`}>
          {pending ? "Deleting…" : "Delete channel"}
        </button>
      </fetcher.Form>
    </Card>
  );
}
