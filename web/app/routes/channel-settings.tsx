import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, redirect, useFetcher, useLoaderData } from "react-router";
import { ChannelHeader } from "@/components/channel/channel-header";
import { ChannelTabs } from "@/components/channel/channel-tabs";
import { StepUpFields, StepUpMethodProvider } from "@/components/step-up";
import { buttonClasses, Card, SectionHeading } from "@/components/ui";
import { notFound, requireMemberInScope, requireWorkspaceOwner } from "@/lib/auth/guards.server";
import { requireStepUp, requireTypedName, stepUpMethod } from "@/lib/auth/step-up.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import {
  type ChannelDeleteOutcome,
  type ChannelRenameOutcome,
  channelDetail,
  channelRowById,
  deleteChannel,
  renameChannel,
} from "@/lib/db/queries.channels.server";
import { useWsPath } from "@/lib/ws-path";
import { wsPathServer } from "@/lib/ws-url.server";

export function meta({ params }: { params: { channel?: string } }) {
  return [{ title: `Settings · #${params.channel ?? "channel"}` }];
}

/**
 * The channel's SETTINGS tab — a member-only sibling of the Skills face that hosts the owner
 * existence-ceremonies. What renders depends on the viewer and the channel: an owner on a named
 * channel gets rename + delete (each step-up gated; delete also types the channel name); an owner
 * on the default `everyone` channel gets the read-only structural note; any non-owner gets a quiet
 * read-only note. The page itself is member-visible — the controls, not the page, are owner-gated —
 * so a member finds the tab without an existence oracle.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { actor } = await requireMemberInScope(request, params);
  const channel = params.channel;
  if (!channel) {
    notFound();
  }
  const detail = await channelDetail(actor, channel);
  if (detail === undefined) {
    notFound();
  }
  return {
    detail,
    isOwner: actor.role === "owner",
    stepUpMethod: await stepUpMethod(actor.userId),
  };
}

/** The typed reply each ceremony returns on a NON-redirect (a landed owner act redirects away). */
type ChannelActionData =
  | { form: "rename"; error: string }
  | { form: "delete"; error: string }
  | { form: "unknown"; error: string };

/**
 * The owner ceremonies, dispatched on the hidden `intent`. Rename and delete RE-GUARD with the
 * owner gate and run the step-up ceremony BEFORE any write. The data layer lands the audit row of
 * every landed act in its own transaction; the route records the attempts it never sees.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  // The membership FLOOR, hoisted above the intent dispatch: every intent below re-checks owner,
  // and the unmatched-intent 400 must never answer a non-member — in multi tenancy `:ws` is a
  // guessable public name slug, so a 400-vs-404 split would be a workspace-existence oracle the
  // GET faces deliberately close.
  const { workspace } = await requireMemberInScope(request, params);
  const ws = workspace.id;
  const channel = params.channel;
  if (!channel) {
    notFound();
  }
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
 * RENAME — owner + step-up. On success redirect to the renamed channel's settings; the ceremony's
 * own outcome codes (bad_name / name_taken / builtin) surface inline otherwise.
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
    return redirect(wsPathServer(wsName, `channels/${newName}/settings`));
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

export default function ChannelSettings() {
  const { detail, isOwner, stepUpMethod } = useLoaderData<typeof loader>();
  const wsPath = useWsPath();
  return (
    <StepUpMethodProvider method={stepUpMethod}>
      <div className="space-y-6">
        <ChannelHeader name={detail.name} mode={detail.mode} isDefault={detail.isDefault} />
        <ChannelTabs basePath={wsPath(`channels/${detail.name}`)} active="settings" />
        <SettingsContent
          isOwner={isOwner}
          isDefault={detail.isDefault}
          channelName={detail.name}
          channelId={detail.channelId}
        />
      </div>
    </StepUpMethodProvider>
  );
}

/** The settings body — owner controls for a named channel, else an honest read-only note. */
function SettingsContent({
  isOwner,
  isDefault,
  channelName,
  channelId,
}: {
  isOwner: boolean;
  isDefault: boolean;
  channelName: string;
  channelId: string;
}) {
  if (!isOwner) {
    return <NonOwnerNote />;
  }
  if (isDefault) {
    return <BuiltinAdminNote />;
  }
  return (
    <section aria-labelledby="settings-heading" className="space-y-3">
      <SectionHeading>
        <span id="settings-heading">Owner controls</span>
      </SectionHeading>
      <RenameChannelForm channel={channelName} channelId={channelId} />
      <DeleteChannelForm channel={channelName} channelId={channelId} />
    </section>
  );
}

/** A member who isn't the owner: the controls are theirs, stated plainly and without a control. */
function NonOwnerNote() {
  return (
    <Card className="px-4 py-3">
      <p className="text-dim text-sm">
        Only workspace owners can rename or delete a channel. You can browse this channel&apos;s
        skills, members, and history from the tabs above.
      </p>
    </Card>
  );
}

/** The `everyone` owner note — no controls, an honest statement of why. */
function BuiltinAdminNote() {
  return (
    <section aria-labelledby="settings-heading" className="space-y-3">
      <SectionHeading>
        <span id="settings-heading">Owner controls</span>
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
