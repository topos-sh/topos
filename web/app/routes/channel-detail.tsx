import { Check, ChevronsUpDown, Package, Plus } from "lucide-react";
import { useState } from "react";
import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, useFetcher, useLoaderData } from "react-router";
import { ChannelHeader } from "@/components/channel/channel-header";
import { ChannelTabs } from "@/components/channel/channel-tabs";
import { buttonClasses, Card, Chip, SectionHeading } from "@/components/ui";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { actorFromSession, memberInScope, notFound, requireMember } from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import {
  type ChannelDetail as ChannelDetailData,
  type ChannelPlaceOutcome,
  type ChannelUnplaceOutcome,
  channelDetail,
  includeChannelInProfile,
  placeBundleInChannel,
  removeChannelFromProfile,
  unplaceBundleFromChannel,
} from "@/lib/db/queries.channels.server";
import { skillIndexOf } from "@/lib/db/queries.server";
import { useWsPath } from "@/lib/ws-path";

export function meta({ params }: { params: { channel?: string } }) {
  return [{ title: `#${params.channel ?? "channel"}` }];
}

/** One catalog entry the add picker can place — slimmed to what the option needs. */
type AddableSkill = { skillId: string; name: string; displayName: string | null };

/**
 * The channel FACE — resource address and the channel's default SKILLS tab as ONE route. A channel
 * page is MEMBERS-ONLY: an anonymous browser gets the house 404, indistinguishable from a mistyped
 * path, so nothing about a channel leaks to a signed-out visitor. (A non-browser document fetch
 * still got the constant, existence-blind protocol card from the server entry.) A signed-in member
 * gets the channel page WITH chrome; a signed-in non-member (or unknown workspace slug) → the same
 * house 404.
 *
 * The Skills tab is the DEFAULT channel view (the bundle references the channel delivers), and it
 * hosts the curation controls: whoever may curate this channel (any member of an open channel, a
 * reviewer-or-owner of a curated one) gets the add picker and the per-row remove. Members, History,
 * and Settings are sibling MEMBER-only routes (see ChannelTabs), each rendering the same header +
 * tabs itself.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const session = await getAuth().api.getSession({ headers: request.headers });
  const actor = actorFromSession(session);
  if (actor === null) {
    // Signed out: the channel face is not a public teaser — it is the uniform house 404, so an
    // anonymous probe cannot tell a real channel from a nonexistent one (or from any other path).
    notFound();
  }
  const { workspace, actor: memberActor } = await memberInScope(actor, params);
  const channel = params.channel;
  if (!channel) {
    notFound();
  }
  const detail = await channelDetail(memberActor, channel);
  // A miss is the uniform 404 — never a 403, never a "channel exists but…" oracle.
  if (detail === undefined) {
    notFound();
  }
  // The curation gate, computed the same way the data layer enforces it: an open channel takes any
  // member, a curated one reviewer-or-owner. The add picker offers the active catalog minus what
  // the channel already references; a non-curator loads none of it (nothing to offer).
  const canCurate = detail.mode === "open" || memberActor.role !== "member";
  const placed = new Set(detail.skills.map((skill) => skill.skillId));
  const addable: AddableSkill[] = canCurate
    ? (await skillIndexOf(memberActor, workspace.id))
        .filter((row) => !placed.has(row.skillId))
        .map((row) => ({ skillId: row.skillId, name: row.name, displayName: row.displayName }))
    : [];
  return { face: "page" as const, detail, addable, canCurate };
}

/** The typed reply each curation fetcher reads back — empty error string on a landed act. */
type SkillCurationActionData =
  | { form: "add"; error: string }
  | { form: "remove"; error: string }
  | { form: "stance"; error: string }
  | { form: "unknown"; error: string };

/**
 * The channel-face curation, dispatched on the hidden `intent`: add or remove a bundle reference.
 * Both are MEMBER-level and unconfirmed — the same grade as the CLI's create-on-first-use
 * placement — with the curated-channel role gate enforced by the data layer, not the route. The
 * DAL lands the skill_added / skill_removed audit row of every landed act in its own transaction;
 * the route records only the faults it never sees.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  // The face posture, POST included: an anonymous submit gets the same uniform 404 the GET face
  // answers (face-shell mounts no login-bounce middleware), and the membership FLOOR is hoisted
  // above the intent dispatch — every intent re-reads its member actor, and the unmatched-intent
  // 400 must never answer a non-member — in multi tenancy `:ws` is a guessable public name slug,
  // so a 400-vs-404 split would be a workspace-existence oracle the GET faces deliberately close.
  const session = await getAuth().api.getSession({ headers: request.headers });
  const sessionActor = actorFromSession(session);
  if (sessionActor === null) {
    notFound();
  }
  const { workspace } = await memberInScope(sessionActor, params);
  const ws = workspace.id;
  const channel = params.channel;
  if (!channel) {
    notFound();
  }
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  // Both intents key on the IMMUTABLE channel_id the page was LOADED with (a hidden field), like
  // every channel ceremony — resolving the URL's mutable name at action time could retarget a
  // freed-and-reused name.
  const channelId = String(formData.get("channel_id") ?? "");
  const skillId = String(formData.get("skill_id") ?? "");
  if (intent === "add-skill") {
    return addSkillIntent(request, ws, channelId, skillId);
  }
  if (intent === "remove-skill") {
    return removeSkillIntent(request, ws, channelId, skillId);
  }
  // The viewer's OWN stance: put this set in (or take it out of) their profile — a personal,
  // self-scoped act (the default channel's remove records the one exclude line).
  if (intent === "profile-add" || intent === "profile-remove") {
    const actor = await requireMember(request, ws);
    try {
      const outcome =
        intent === "profile-add"
          ? await includeChannelInProfile(actor, channelId)
          : await removeChannelFromProfile(actor, channelId);
      if (outcome === "unknown_channel") {
        return data<SkillCurationActionData>(
          { form: "stance", error: "This channel no longer exists." },
          { status: 400 },
        );
      }
      return data<SkillCurationActionData>({ form: "stance", error: "" });
    } catch {
      return data<SkillCurationActionData>(
        { form: "stance", error: "That didn't go through. Try again." },
        { status: 500 },
      );
    }
  }
  return data<SkillCurationActionData>(
    { form: "unknown", error: "Unknown action." },
    { status: 400 },
  );
}

const ADD_GENERIC_ERROR = "That add didn't go through. Try again.";
const REMOVE_GENERIC_ERROR = "That remove didn't go through. Try again.";

/** Map the place outcome's codes to honest, member-facing copy. */
function placeErrorCopy(outcome: ChannelPlaceOutcome): string {
  switch (outcome) {
    case "unknown_skill":
      return "That skill no longer exists in this workspace.";
    case "skill_not_active":
      return "That skill isn't active, so it can't be added.";
    case "curated_role_required":
      return "Reviewers and owners manage a curated channel's skills.";
    default:
      return "This channel no longer exists.";
  }
}

/** Map the unplace outcome's codes to honest, member-facing copy. */
function unplaceErrorCopy(outcome: ChannelUnplaceOutcome): string {
  switch (outcome) {
    case "not_placed":
      return "That skill isn't in this channel.";
    case "curated_role_required":
      return "Reviewers and owners manage a curated channel's skills.";
    default:
      return "This channel no longer exists.";
  }
}

/** ADD — member-level; the place core's outcome codes surface inline, a landed act revalidates. */
async function addSkillIntent(request: Request, ws: string, channelId: string, skillId: string) {
  const actor = await requireMember(request, ws);
  let outcome: ChannelPlaceOutcome;
  try {
    outcome = await placeBundleInChannel(actor, channelId, skillId);
  } catch {
    await recordAdminEvent(actor, { kind: "skill_added", subject: channelId, outcome: "error" });
    return data<SkillCurationActionData>(
      { form: "add", error: ADD_GENERIC_ERROR },
      { status: 500 },
    );
  }
  if (outcome === "placed") {
    return data<SkillCurationActionData>({ form: "add", error: "" });
  }
  return data<SkillCurationActionData>(
    { form: "add", error: placeErrorCopy(outcome) },
    { status: 400 },
  );
}

/** REMOVE — the symmetric member-level act; the unplace core's codes surface inline. */
async function removeSkillIntent(request: Request, ws: string, channelId: string, skillId: string) {
  const actor = await requireMember(request, ws);
  let outcome: ChannelUnplaceOutcome;
  try {
    outcome = await unplaceBundleFromChannel(actor, channelId, skillId);
  } catch {
    await recordAdminEvent(actor, { kind: "skill_removed", subject: channelId, outcome: "error" });
    return data<SkillCurationActionData>(
      { form: "remove", error: REMOVE_GENERIC_ERROR },
      { status: 500 },
    );
  }
  if (outcome === "removed") {
    return data<SkillCurationActionData>({ form: "remove", error: "" });
  }
  return data<SkillCurationActionData>(
    { form: "remove", error: unplaceErrorCopy(outcome) },
    { status: 400 },
  );
}

export default function ChannelDetail() {
  const data = useLoaderData<typeof loader>();
  return (
    <ChannelSkillsPage detail={data.detail} addable={data.addable} canCurate={data.canCurate} />
  );
}

function ChannelSkillsPage({
  detail,
  addable,
  canCurate,
}: {
  detail: ChannelDetailData;
  addable: AddableSkill[];
  canCurate: boolean;
}) {
  const wsPath = useWsPath();
  return (
    <div className="space-y-6">
      <ChannelHeader name={detail.name} mode={detail.mode} isDefault={detail.isDefault} />
      <ChannelTabs basePath={wsPath(`channels/${detail.name}`)} active="skills" />
      <StanceSection detail={detail} />
      <SkillsSection detail={detail} addable={addable} canCurate={canCurate} />
    </div>
  );
}

/**
 * The viewer's own stance on this SET: whether their profile carries it, and the one-click
 * toggle (self-scoped — a personal profile line, never anyone else's). A channel has no
 * membership: people carry it by referencing it; the default channel is the implicit baseline
 * and its remove records the one exclude line.
 */
function StanceSection({ detail }: { detail: ChannelDetailData }) {
  const fetcher = useFetcher<SkillCurationActionData>();
  const pending = fetcher.state !== "idle";
  const error =
    fetcher.data?.form === "stance" && fetcher.data.error.length > 0
      ? fetcher.data.error
      : undefined;
  const audience =
    detail.audienceCount === 1 ? "1 person's profile" : `${detail.audienceCount} people's profiles`;
  return (
    <div
      data-testid="channel-stance"
      className="flex flex-wrap items-center gap-x-3 gap-y-2 rounded-md border border-line-soft bg-panel px-4 py-3"
    >
      <span className="text-dim text-sm">
        {detail.isDefault ? (
          <>The baseline set — carried by {audience}.</>
        ) : (
          <>A curated set — carried by {audience}.</>
        )}
      </span>
      {detail.viewerIncluded ? (
        <Chip tone="verified">in your skills</Chip>
      ) : (
        <Chip tone="neutral">not in your skills</Chip>
      )}
      <fetcher.Form method="post" className="ml-auto">
        <input
          type="hidden"
          name="intent"
          value={detail.viewerIncluded ? "profile-remove" : "profile-add"}
        />
        <input type="hidden" name="channel_id" value={detail.channelId} />
        <button type="submit" disabled={pending} className={buttonClasses("quiet")}>
          {detail.viewerIncluded ? "Remove from my skills" : "Add to my skills"}
        </button>
      </fetcher.Form>
      {error !== undefined && (
        <p role="alert" className="w-full text-red-700 text-xs">
          {error}
        </p>
      )}
    </div>
  );
}

/**
 * The skill references the channel delivers — each a link to its skill page (by catalog name) —
 * plus the curation controls the viewer is allowed: a quiet Remove on each row and an add picker
 * beneath, or (for a member on a curated channel) an honest read-only note.
 */
function SkillsSection({
  detail,
  addable,
  canCurate,
}: {
  detail: ChannelDetailData;
  addable: AddableSkill[];
  canCurate: boolean;
}) {
  const wsPath = useWsPath();
  return (
    <section aria-labelledby="skills-heading" className="space-y-3">
      <SectionHeading>
        <span id="skills-heading">Skills</span>
      </SectionHeading>
      {detail.skills.length === 0 ? (
        <p className="text-dim text-sm">This channel references no skills yet.</p>
      ) : (
        <Card className="overflow-hidden">
          <ul>
            {detail.skills.map((skill) => (
              <SkillRow
                key={skill.skillId}
                skill={skill}
                channelId={detail.channelId}
                canCurate={canCurate}
                to={wsPath(`skills/${skill.name}`)}
              />
            ))}
          </ul>
        </Card>
      )}
      {canCurate && addable.length > 0 && (
        <AddSkillForm channelId={detail.channelId} addable={addable} />
      )}
      {!canCurate && <CuratedNote />}
    </section>
  );
}

/** One reference row — the link (or a quiet non-active label) plus, for a curator, a Remove. */
function SkillRow({
  skill,
  channelId,
  canCurate,
  to,
}: {
  skill: ChannelDetailData["skills"][number];
  channelId: string;
  canCurate: boolean;
  to: string;
}) {
  return (
    <li className="flex items-center gap-2 border-line-soft border-b last:border-b-0">
      {skill.status === "active" ? (
        <Link
          to={to}
          className="flex min-w-0 flex-1 items-center gap-2 px-4 py-3 hover:bg-panel2 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent"
        >
          <span className="min-w-0 truncate font-medium text-ink text-sm">
            {skill.displayName ?? skill.name}
          </span>
        </Link>
      ) : (
        <div className="flex min-w-0 flex-1 items-center gap-2 px-4 py-3">
          <span className="min-w-0 truncate text-dim text-sm">
            {skill.displayName ?? skill.name}
          </span>
          <Chip tone="unverified">{skill.status}</Chip>
        </div>
      )}
      {canCurate && <RemoveSkillControl channelId={channelId} skillId={skill.skillId} />}
    </li>
  );
}

/** The per-row Remove — its own small fetcher form; a fault surfaces beside the button. */
function RemoveSkillControl({ channelId, skillId }: { channelId: string; skillId: string }) {
  const fetcher = useFetcher<SkillCurationActionData>();
  const pending = fetcher.state !== "idle";
  const error =
    fetcher.data?.form === "remove" && fetcher.data.error.length > 0
      ? fetcher.data.error
      : undefined;
  return (
    <div className="flex shrink-0 items-center gap-2 pr-3">
      {error !== undefined && (
        <span className="text-red-600 text-xs" role="alert">
          {error}
        </span>
      )}
      <fetcher.Form method="post">
        <input type="hidden" name="intent" value="remove-skill" />
        <input type="hidden" name="channel_id" value={channelId} />
        <input type="hidden" name="skill_id" value={skillId} />
        <button type="submit" disabled={pending} className={buttonClasses("quiet")}>
          {pending ? "Removing…" : "Remove"}
        </button>
      </fetcher.Form>
    </div>
  );
}

/** The add picker — a native select of the addable catalog + an Add submit, quiet under the list. */
/**
 * The add picker, in the workspace selector's design language: one dropdown trigger (a leading
 * glyph, a truncating label, the ChevronsUpDown tail) over a menu of the addable catalog with
 * the staged choice ticked. Choosing only STAGES the skill on the trigger — the explicit Add
 * button beside it performs the act (a surprise-free two-step, like every other form here). The
 * staged choice is validated against the CURRENT addable list, so a successful add — which
 * revalidates the skill out of the catalog — resets the trigger to its placeholder by itself.
 */
function AddSkillForm({ channelId, addable }: { channelId: string; addable: AddableSkill[] }) {
  const fetcher = useFetcher<SkillCurationActionData>();
  const [stagedId, setStagedId] = useState<string | null>(null);
  const staged = addable.find((skill) => skill.skillId === stagedId);
  const pending = fetcher.state !== "idle";
  const error =
    fetcher.data?.form === "add" && fetcher.data.error.length > 0 ? fetcher.data.error : undefined;
  return (
    <div className="space-y-2">
      <span className="block font-medium text-dim text-sm">Add a skill</span>
      <div className="flex flex-wrap items-center gap-2">
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <button
              type="button"
              disabled={pending}
              className="flex h-11 min-w-56 items-center gap-2 rounded-md border border-line bg-panel px-3 text-ink text-sm transition-colors hover:bg-panel2 focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2 disabled:cursor-not-allowed disabled:opacity-50 data-[state=open]:bg-panel2"
            >
              {staged === undefined ? (
                <Plus className="size-4 shrink-0 text-faint" aria-hidden="true" />
              ) : (
                <Package className="size-4 shrink-0 text-faint" aria-hidden="true" />
              )}
              <span className="min-w-0 flex-1 truncate text-left font-medium">
                {staged === undefined ? "Choose a skill" : (staged.displayName ?? staged.name)}
              </span>
              <ChevronsUpDown className="size-4 shrink-0 text-faint" aria-hidden="true" />
            </button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="start" className="min-w-56">
            <DropdownMenuLabel className="font-normal text-faint text-xs">
              Workspace skills
            </DropdownMenuLabel>
            {addable.map((skill) => (
              <DropdownMenuItem key={skill.skillId} onSelect={() => setStagedId(skill.skillId)}>
                <Package />
                <span className="min-w-0 flex-1 truncate">{skill.displayName ?? skill.name}</span>
                {skill.skillId === stagedId && <Check className="ml-auto size-4 text-accent" />}
              </DropdownMenuItem>
            ))}
          </DropdownMenuContent>
        </DropdownMenu>
        <fetcher.Form method="post">
          <input type="hidden" name="intent" value="add-skill" />
          <input type="hidden" name="channel_id" value={channelId} />
          <input type="hidden" name="skill_id" value={staged?.skillId ?? ""} />
          <button
            type="submit"
            disabled={pending || staged === undefined}
            className={`${buttonClasses("quiet")} min-h-11`}
          >
            {pending ? "Adding…" : "Add"}
          </button>
        </fetcher.Form>
      </div>
      {error !== undefined && (
        <p className="text-red-600 text-sm" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}

/** A member on a CURATED channel: the controls are a reviewer's/owner's, stated plainly. */
function CuratedNote() {
  return (
    <p className="text-dim text-sm">Reviewers and owners manage this channel&apos;s skills.</p>
  );
}
