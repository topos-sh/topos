import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, useFetcher, useLoaderData } from "react-router";
import { ChannelHeader } from "@/components/channel/channel-header";
import { ChannelTabs } from "@/components/channel/channel-tabs";
import { ResourcePage } from "@/components/resource-page";
import { buttonClasses, Card, Chip, SectionHeading } from "@/components/ui";
import {
  actorFromSession,
  notFound,
  requireMember,
  workspaceInScope,
} from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import {
  type ChannelDetail as ChannelDetailData,
  type ChannelPlaceOutcome,
  type ChannelUnplaceOutcome,
  channelDetail,
  placeBundleInChannel,
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
 * The channel FACE — resource address and the channel's default SKILLS tab as ONE route. Admission
 * mirrors the other faces: anonymous browser → the constant teaser; a signed-in member → the
 * channel page WITH chrome; a signed-in non-member (or unknown workspace slug) → the house 404.
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
  | { form: "unknown"; error: string };

/**
 * The channel-face curation, dispatched on the hidden `intent`: add or remove a bundle reference.
 * Both are MEMBER-level and step-up-less — the same grade as the CLI's create-on-first-use
 * placement — with the curated-channel role gate enforced by the data layer, not the route. The
 * DAL lands the skill_added / skill_removed audit row of every landed act in its own transaction;
 * the route records only the faults it never sees.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const workspace = await workspaceInScope(params);
  const ws = workspace.id;
  const channel = params.channel;
  if (!channel) {
    notFound();
  }
  // The membership FLOOR, hoisted above the intent dispatch: every intent re-reads its member
  // actor, and the unmatched-intent 400 must never answer a non-member — in multi tenancy `:ws`
  // is a guessable public name slug, so a 400-vs-404 split would be a workspace-existence oracle
  // the GET faces deliberately close.
  await requireMember(request, workspace.id);
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
  if (data.face === "teaser") {
    return <ResourcePage />;
  }
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
      <SkillsSection detail={detail} addable={addable} canCurate={canCurate} />
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
function AddSkillForm({ channelId, addable }: { channelId: string; addable: AddableSkill[] }) {
  const fetcher = useFetcher<SkillCurationActionData>();
  const pending = fetcher.state !== "idle";
  const error =
    fetcher.data?.form === "add" && fetcher.data.error.length > 0 ? fetcher.data.error : undefined;
  return (
    <div className="space-y-2">
      <fetcher.Form method="post" className="flex flex-wrap items-end gap-2">
        <input type="hidden" name="intent" value="add-skill" />
        <input type="hidden" name="channel_id" value={channelId} />
        <label className="block">
          <span className="mb-1 block font-medium text-sm text-dim">Add a skill</span>
          <select
            name="skill_id"
            className="block h-11 min-w-56 rounded-md border border-line bg-panel px-3 text-ink text-sm focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25"
          >
            {addable.map((skill) => (
              <option key={skill.skillId} value={skill.skillId}>
                {skill.displayName ?? skill.name}
              </option>
            ))}
          </select>
        </label>
        <button type="submit" disabled={pending} className={`${buttonClasses("quiet")} min-h-11`}>
          {pending ? "Adding…" : "Add"}
        </button>
      </fetcher.Form>
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
