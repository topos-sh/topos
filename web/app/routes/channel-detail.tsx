import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, redirect, useFetcher, useLoaderData } from "react-router";
import { relativeTime } from "@/components/format";
import { StepUpFields } from "@/components/step-up";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading } from "@/components/ui";
import {
  notFound,
  type OwnerActor,
  requireMember,
  requireWorkspaceOwner,
} from "@/lib/auth/guards.server";
import { requireStepUp, requireTypedName } from "@/lib/auth/step-up.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import {
  type ChannelDeleteOutcome,
  type ChannelDetail as ChannelDetailData,
  type ChannelRenameOutcome,
  channelDetail,
  deleteChannel,
  renameChannel,
} from "@/lib/db/queries.channels.server";

export function meta({ params }: { params: { channel?: string } }) {
  return [{ title: `#${params.channel ?? "channel"}` }];
}

export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  const channel = params.channel;
  if (!ws || !channel) {
    notFound();
  }
  const actor = await requireMember(request, ws);
  const detail = await channelDetail(actor, channel);
  // A miss is the uniform 404 — never a 403, never a "channel exists but…" oracle.
  if (detail === undefined) {
    notFound();
  }
  return { ws, channel, detail, isOwner: actor.role === "owner" };
}

/** The typed reply each owner ceremony returns on a NON-redirect (a landed act redirects away). */
type ChannelActionData =
  | { form: "rename"; error: string }
  | { form: "delete"; error: string }
  | { form: "unknown"; error: string };

/**
 * ONE action, dispatched on the hidden `intent`. Each branch RE-GUARDS with the owner gate (a
 * loader gate never extends to an action), then runs the step-up ceremony BEFORE any write. Every
 * attempt lands an `admin_event` row whatever the outcome — a refused step-up is as much a fact as
 * a landed act.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const ws = params.ws;
  const channel = params.channel;
  if (!ws || !channel) {
    notFound();
  }
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  if (intent === "rename-channel") {
    return renameChannelIntent(request, ws, channel, formData);
  }
  if (intent === "delete-channel") {
    return deleteChannelIntent(request, ws, channel, formData);
  }
  return data<ChannelActionData>({ form: "unknown", error: "Unknown action." }, { status: 400 });
}

const RENAME_GENERIC_ERROR = "That rename didn't go through. Try again.";
const DELETE_GENERIC_ERROR = "That delete didn't go through. Try again.";

/** Map the database's rename outcome codes to honest, member-facing copy. */
function renameErrorCopy(outcome: ChannelRenameOutcome, newName: string): string {
  switch (outcome) {
    case "bad_name":
      return "Channel names use lowercase letters, numbers, and hyphens (up to 64 characters).";
    case "name_taken":
      return `A channel named #${newName} already exists.`;
    case "builtin":
      return "The everyone channel is structural — it can't be renamed.";
    case "unknown_channel":
      return "This channel no longer exists.";
    default:
      // owner_role_required / member_required — the web guard already refuses these; defense in depth.
      return "Only a workspace owner can rename a channel.";
  }
}

/** Map the database's delete outcome codes to honest, member-facing copy. */
function deleteErrorCopy(outcome: ChannelDeleteOutcome): string {
  switch (outcome) {
    case "builtin":
      return "The everyone channel is structural — it can't be deleted.";
    case "unknown_channel":
      return "This channel no longer exists.";
    default:
      return "Only a workspace owner can delete a channel.";
  }
}

/**
 * RENAME — owner + step-up. On success redirect to the new channel URL; the database's own
 * outcome codes (bad_name / name_taken / builtin) surface inline otherwise.
 */
async function renameChannelIntent(
  request: Request,
  ws: string,
  channel: string,
  formData: FormData,
) {
  const owner = await requireWorkspaceOwner(request, ws);
  const newName = String(formData.get("new_name") ?? "").trim();
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "channel_rename",
      subject: channel,
      detail: "step_up",
      outcome: "denied",
    });
    return data<ChannelActionData>({ form: "rename", error: stepUp.error }, { status: 400 });
  }
  let outcome: ChannelRenameOutcome;
  try {
    outcome = await renameChannel(owner as OwnerActor, channel, newName);
  } catch {
    await recordAdminEvent(owner, {
      kind: "channel_rename",
      subject: channel,
      detail: "error",
      outcome: "error",
    });
    return data<ChannelActionData>(
      { form: "rename", error: RENAME_GENERIC_ERROR },
      { status: 500 },
    );
  }
  const ok = outcome === "renamed";
  await recordAdminEvent(owner, {
    kind: "channel_rename",
    subject: channel,
    detail: ok ? newName : outcome,
    outcome: ok ? "ok" : "denied",
  });
  if (ok) {
    return redirect(`/workspaces/${ws}/channels/${newName}`);
  }
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
  channel: string,
  formData: FormData,
) {
  const owner = await requireWorkspaceOwner(request, ws);
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "channel_delete",
      subject: channel,
      detail: "step_up",
      outcome: "denied",
    });
    return data<ChannelActionData>({ form: "delete", error: stepUp.error }, { status: 400 });
  }
  const typed = requireTypedName(formData, channel);
  if (!typed.ok) {
    await recordAdminEvent(owner, {
      kind: "channel_delete",
      subject: channel,
      detail: "confirm_name",
      outcome: "denied",
    });
    return data<ChannelActionData>({ form: "delete", error: typed.error }, { status: 400 });
  }
  let outcome: ChannelDeleteOutcome;
  try {
    outcome = await deleteChannel(owner as OwnerActor, channel);
  } catch {
    await recordAdminEvent(owner, {
      kind: "channel_delete",
      subject: channel,
      detail: "error",
      outcome: "error",
    });
    return data<ChannelActionData>(
      { form: "delete", error: DELETE_GENERIC_ERROR },
      { status: 500 },
    );
  }
  const ok = outcome === "deleted";
  await recordAdminEvent(owner, {
    kind: "channel_delete",
    subject: channel,
    detail: ok ? undefined : outcome,
    outcome: ok ? "ok" : "denied",
  });
  if (ok) {
    return redirect(`/workspaces/${ws}/channels`);
  }
  return data<ChannelActionData>(
    { form: "delete", error: deleteErrorCopy(outcome) },
    { status: 400 },
  );
}

export default function ChannelDetail() {
  const { ws, channel, detail, isOwner } = useLoaderData<typeof loader>();
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
            {detail.builtin && <span>every confirmed member, structural</span>}
          </div>
        }
        actions={
          <>
            <Link
              to={`/workspaces/${ws}/channels/${detail.name}/history`}
              className={buttonClasses("quiet")}
            >
              History
            </Link>
            <Link to={`/workspaces/${ws}/channels`} className={buttonClasses("quiet")}>
              All channels
            </Link>
          </>
        }
      />

      <SkillsSection ws={ws} skills={detail.skills} />
      <MembersSection detail={detail} />

      {isOwner &&
        (detail.builtin ? (
          <BuiltinAdminNote />
        ) : (
          <section aria-labelledby="admin-heading" className="space-y-3">
            <SectionHeading>
              <span id="admin-heading">Owner controls</span>
            </SectionHeading>
            <RenameChannelForm channel={channel} />
            <DeleteChannelForm channel={channel} />
          </section>
        ))}
    </div>
  );
}

/** The skill references the channel delivers — each a link to its skill page (by catalog name). */
function SkillsSection({ ws, skills }: { ws: string; skills: ChannelDetailData["skills"] }) {
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
                    to={`/workspaces/${ws}/skills/${skill.name}`}
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

/** The channel's people — the structural note for `everyone`, else the membership list. */
function MembersSection({ detail }: { detail: ChannelDetailData }) {
  return (
    <section aria-labelledby="members-heading" className="space-y-3">
      <SectionHeading>
        <span id="members-heading">Members</span>
      </SectionHeading>
      {detail.builtin ? (
        <Card className="px-4 py-3">
          <p className="text-dim text-sm">
            <span className="font-medium text-ink">everyone</span> reaches every confirmed member of
            the workspace —{" "}
            {detail.confirmedMemberCount === 1
              ? "1 member"
              : `${detail.confirmedMemberCount} members`}{" "}
            right now. Its membership is the roster itself, so there are no rows to add or remove.
          </p>
        </Card>
      ) : detail.members.length === 0 ? (
        <p className="text-dim text-sm">No members yet.</p>
      ) : (
        <Card className="overflow-hidden">
          <ul>
            {detail.members.map((member) => (
              <li
                key={member.principal}
                className="flex flex-wrap items-center gap-x-3 gap-y-1 border-line-soft border-b px-4 py-3 last:border-b-0"
              >
                <span className="min-w-0 truncate font-medium text-ink text-sm">
                  {member.principal}
                </span>
                <span className="text-faint text-xs">
                  joined {relativeTime(new Date(member.addedAt))}
                  {member.addedBy ? ` · added by ${member.addedBy}` : ""}
                </span>
              </li>
            ))}
          </ul>
        </Card>
      )}
    </section>
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
          confirmed roster.
        </p>
      </Card>
    </section>
  );
}

/** The rename ceremony — step-up + the new name; the database's outcome codes surface inline. */
function RenameChannelForm({ channel }: { channel: string }) {
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
function DeleteChannelForm({ channel }: { channel: string }) {
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
