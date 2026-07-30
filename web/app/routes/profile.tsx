import { Package, Plus } from "lucide-react";
import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, useFetcher, useLoaderData } from "react-router";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading, ShortId } from "@/components/ui";
import { requireMember, requireMemberInScope } from "@/lib/auth/guards.server";
import {
  addToMine,
  assignChannelToSelf,
  declineBundle,
  feedOf,
  unassignChannelFromSelf,
  undeclineBundle,
  unpickBundle,
} from "@/lib/db/queries.feed.server";
import { deliveryFor, laneChannels } from "@/lib/db/queries.lane.server";
import { skillIndexOf } from "@/lib/db/queries.server";
import { useWsPath } from "@/lib/ws-path";

export function meta() {
  return [{ title: "Your skills" }];
}

/**
 * YOUR SKILLS — the web face of the person's FEED: what the workspace says their agents should
 * have, and the two switches they hold over it. Server-stored, so it roams: every machine this
 * person logs in from delivers the same set.
 *
 * The model is two rows. An ASSIGNMENT is positive — the workspace baseline (the `everyone`
 * channel, assigned to everyone), a curator aiming something at this person, or the person
 * adding it themselves. A DECLINE is the one negative — off for me, whatever assigns it, and
 * it survives new versions and channel reshuffles. Turning something off never removes it from
 * the team library, and turning it back on is one click.
 *
 * Every act here is SELF-scoped (personal rows, nobody else's) — plain one-click toggles, no
 * ceremony.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { actor } = await requireMemberInScope(request, params);
  const [delivery, channels, feed, catalog] = await Promise.all([
    deliveryFor(actor),
    laneChannels(actor),
    feedOf(actor),
    skillIndexOf(actor, actor.workspaceId),
  ]);
  // A bundle the person assigned to THEMSELVES is theirs to take back (an unpick); one aimed
  // at them (or at everyone) is not — declining is the switch for those.
  const picked = new Set(
    feed.assignments.filter((a) => a.kind === "skill" && a.own).map((a) => a.targetId),
  );
  const assignedSkills = new Set(
    feed.assignments.filter((a) => a.kind === "skill").map((a) => a.targetId),
  );
  const declined = new Set(feed.declines.map((d) => d.skillId));
  return {
    delivered: delivery.skills.map((s) => ({
      skillId: s.skill_id,
      name: s.name,
      displayName: s.display_name ?? null,
      versionId: s.version_id,
      viaChannels: s.via.channels,
      direct: s.via.direct,
      picked: picked.has(s.skill_id),
    })),
    channels: channels.map((c) => ({
      channelId: c.channelId,
      name: c.name,
      mode: c.mode,
      builtin: c.builtin,
      assigned: c.included,
      skillCount: c.skills.length,
    })),
    declined: feed.declines,
    // The add picker: active catalog skills the feed neither delivers nor already assigns —
    // a declined skill is offered by its own section's switch, not here.
    addable: catalog
      .filter((row) => !assignedSkills.has(row.skillId))
      .filter((row) => !declined.has(row.skillId))
      .filter((row) => !delivery.skills.some((s) => s.skill_id === row.skillId))
      .map((row) => ({ skillId: row.skillId, name: row.name, displayName: row.displayName })),
  };
}

type ProfileActionData = { intent: string; status: string };

/**
 * The page's self-service acts, dispatched on the hidden `intent` — all personal rows (the feed
 * data layer), naturally idempotent, unconfirmed. Channel intents key on the IMMUTABLE channel
 * id; skill intents on the immutable bundle id.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const { workspace } = await requireMemberInScope(request, params);
  const actor = await requireMember(request, workspace.id);
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  const skillId = String(formData.get("skill_id") ?? "");
  const channelId = String(formData.get("channel_id") ?? "");
  try {
    switch (intent) {
      case "add-skill":
        return data<ProfileActionData>({ intent, status: await addToMine(actor, skillId) });
      case "decline-skill":
        return data<ProfileActionData>({ intent, status: await declineBundle(actor, skillId) });
      case "undecline-skill":
        return data<ProfileActionData>({ intent, status: await undeclineBundle(actor, skillId) });
      case "unpick-skill":
        return data<ProfileActionData>({ intent, status: await unpickBundle(actor, skillId) });
      case "carry-channel":
        return data<ProfileActionData>({
          intent,
          status: await assignChannelToSelf(actor, channelId),
        });
      case "drop-channel":
        return data<ProfileActionData>({
          intent,
          status: await unassignChannelFromSelf(actor, channelId),
        });
      default:
        return data<ProfileActionData>({ intent: "unknown", status: "error" }, { status: 400 });
    }
  } catch {
    return data<ProfileActionData>({ intent, status: "error" }, { status: 500 });
  }
}

export default function ProfilePage() {
  const { delivered, channels, declined, addable } = useLoaderData<typeof loader>();
  return (
    <div className="space-y-8">
      <PageHeader
        title="Your skills"
        meta={
          delivered.length === 1
            ? "1 skill delivered to your agents"
            : `${delivered.length} skills delivered to your agents`
        }
      />
      <p className="max-w-2xl text-dim text-sm leading-relaxed">
        These are the skills your agents receive in this workspace — on every machine you log in
        from. Some are assigned to the whole team (the{" "}
        <span className="font-mono text-[13px]">everyone</span> channel is the workspace baseline),
        some you add yourself. Turn any of them off: it stops arriving for you and stays in the team
        library, and turning it back on is one click.
      </p>
      <DeliveredSection delivered={delivered} />
      <ChannelsSection channels={channels} />
      {declined.length > 0 && <DeclinedSection declined={declined} />}
      <AddSection addable={addable} />
    </div>
  );
}

/**
 * Which ROW of a section is on the wire, by the hidden id its form posts — or null when the
 * section is idle. Each section here drives every one of its rows from ONE fetcher, so
 * `fetcher.state` alone cannot tell the clicked row from its neighbours; without the id the whole
 * list would announce a wait that belongs to one line of it.
 */
function flyingSkillId(fetcher: { state: string; formData?: FormData }): string | null {
  return fetcher.state === "idle" ? null : (fetcher.formData?.get("skill_id")?.toString() ?? null);
}

/** The channel-row twin of [`flyingSkillId`]. */
function flyingChannelId(fetcher: { state: string; formData?: FormData }): string | null {
  return fetcher.state === "idle"
    ? null
    : (fetcher.formData?.get("channel_id")?.toString() ?? null);
}

function DeliveredSection({
  delivered,
}: {
  delivered: ReturnType<typeof useLoaderData<typeof loader>>["delivered"];
}) {
  const wsPath = useWsPath();
  const fetcher = useFetcher<ProfileActionData>();
  // ONE fetcher serves every row, so every row's button disables together — but only the row that
  // was clicked names its wait. Without the id check the whole list would read "Turning off…".
  const flying = flyingSkillId(fetcher);
  return (
    <section aria-labelledby="delivered-heading" className="space-y-3">
      <SectionHeading>
        <span id="delivered-heading">Delivered to your agents</span>
      </SectionHeading>
      {delivered.length === 0 ? (
        <p className="text-dim text-sm">
          Nothing yet — carry a channel or add a skill below, and your agents pick it up on their
          next update.
        </p>
      ) : (
        <Card className="overflow-hidden">
          <ul>
            {delivered.map((skill) => (
              <li
                key={skill.skillId}
                data-testid={`profile-delivered-${skill.name}`}
                className="flex flex-wrap items-center gap-x-3 gap-y-1 border-line-soft border-b px-4 py-3 last:border-b-0"
              >
                <Link
                  to={wsPath(`skills/${skill.name}`)}
                  className="min-w-0 truncate font-medium text-ink text-sm hover:underline"
                >
                  {skill.displayName ?? skill.name}
                </Link>
                <ShortId value={skill.versionId} />
                <span className="text-faint text-xs">
                  {skill.viaChannels.length > 0 && <>via {skill.viaChannels.join(", ")}</>}
                  {skill.viaChannels.length > 0 && skill.direct && " · "}
                  {skill.direct && (skill.picked ? "added by you" : "assigned to you")}
                </span>
                <span className="ml-auto flex items-center gap-2">
                  {/* Two distinct affordances, never merged: un-adding takes back the person's
                      own act (and leaves anything a channel still carries arriving), while
                      turning off holds the skill back whatever assigns it. */}
                  {skill.picked && (
                    <fetcher.Form method="post">
                      <input type="hidden" name="intent" value="unpick-skill" />
                      <input type="hidden" name="skill_id" value={skill.skillId} />
                      <button
                        type="submit"
                        disabled={fetcher.state !== "idle"}
                        className={buttonClasses("quiet")}
                      >
                        {flying === skill.skillId &&
                        fetcher.formData?.get("intent") === "unpick-skill"
                          ? "Un-adding…"
                          : "Un-add"}
                      </button>
                    </fetcher.Form>
                  )}
                  <fetcher.Form method="post">
                    <input type="hidden" name="intent" value="decline-skill" />
                    <input type="hidden" name="skill_id" value={skill.skillId} />
                    <button
                      type="submit"
                      disabled={fetcher.state !== "idle"}
                      className={buttonClasses("quiet")}
                    >
                      {flying === skill.skillId &&
                      fetcher.formData?.get("intent") === "decline-skill"
                        ? "Turning off…"
                        : "Turn off"}
                    </button>
                  </fetcher.Form>
                </span>
              </li>
            ))}
          </ul>
        </Card>
      )}
    </section>
  );
}

function ChannelsSection({
  channels,
}: {
  channels: ReturnType<typeof useLoaderData<typeof loader>>["channels"];
}) {
  const fetcher = useFetcher<ProfileActionData>();
  const wsPath = useWsPath();
  // Carrying or dropping a channel changes what every one of this person's machines is owed —
  // a slow answer with no in-flight state reads as a dead button and invites the second click
  // that toggles it straight back.
  const toggling = flyingChannelId(fetcher);
  return (
    <section aria-labelledby="profile-channels-heading" className="space-y-3">
      <SectionHeading>
        <span id="profile-channels-heading">Channels</span>
      </SectionHeading>
      <Card className="overflow-hidden">
        <ul>
          {channels.map((channel) => (
            <li
              key={channel.name}
              data-testid={`profile-channel-${channel.name}`}
              className="flex flex-wrap items-center gap-x-3 gap-y-1 border-line-soft border-b px-4 py-3 last:border-b-0"
            >
              <Link
                to={wsPath(`channels/${channel.name}`)}
                className="font-medium text-ink text-sm hover:underline"
              >
                {channel.name}
              </Link>
              {channel.builtin && <Chip tone="neutral">baseline</Chip>}
              <span className="text-faint text-xs">
                {channel.skillCount === 1 ? "1 skill" : `${channel.skillCount} skills`}
              </span>
              <span className="ml-auto flex items-center gap-2">
                {channel.assigned ? (
                  <Chip tone="verified">in your skills</Chip>
                ) : (
                  <Chip tone="neutral">not carried</Chip>
                )}
                {channel.builtin ? (
                  // The baseline reaches the whole team, so no one person drops it. Its skills
                  // are turned off one at a time, above.
                  <span className="text-faint text-xs">assigned to everyone</span>
                ) : (
                  <fetcher.Form method="post">
                    <input
                      type="hidden"
                      name="intent"
                      value={channel.assigned ? "drop-channel" : "carry-channel"}
                    />
                    <input type="hidden" name="channel_id" value={channel.channelId} />
                    <ChannelToggle
                      name={channel.name}
                      assigned={channel.assigned}
                      busy={fetcher.state !== "idle"}
                      flying={toggling === channel.channelId}
                    />
                  </fetcher.Form>
                )}
              </span>
            </li>
          ))}
        </ul>
      </Card>
    </section>
  );
}

function ChannelToggle({
  name,
  assigned,
  busy,
  flying,
}: {
  name: string;
  assigned: boolean;
  /** Any row of this section is on the wire — every toggle disables together. */
  busy: boolean;
  /** THIS row is the one on the wire — only it names the wait. */
  flying: boolean;
}) {
  return (
    <button type="submit" disabled={busy} className={buttonClasses("quiet")}>
      {flying
        ? assigned
          ? `Dropping ${name}…`
          : `Carrying ${name}…`
        : assigned
          ? `Drop ${name}`
          : `Carry ${name}`}
    </button>
  );
}

function DeclinedSection({
  declined,
}: {
  declined: ReturnType<typeof useLoaderData<typeof loader>>["declined"];
}) {
  const fetcher = useFetcher<ProfileActionData>();
  const flying = flyingSkillId(fetcher);
  return (
    <section aria-labelledby="declined-heading" className="space-y-3">
      <SectionHeading>
        <span id="declined-heading">Turned off</span>
      </SectionHeading>
      <p className="text-dim text-sm leading-relaxed">
        Off for you, whatever assigns them — the team still has them, and the switch survives new
        versions and changes to the channels that carry them.
      </p>
      <Card className="overflow-hidden">
        <ul>
          {declined.map((skill) => (
            <li
              key={skill.skillId}
              data-testid={`profile-declined-${skill.name}`}
              className="flex items-center gap-3 border-line-soft border-b px-4 py-3 last:border-b-0 opacity-60"
            >
              <span className="min-w-0 truncate text-dim text-sm">{skill.name}</span>
              <span className="text-faint text-xs">off — still in the team library</span>
              <span className="ml-auto">
                <fetcher.Form method="post">
                  <input type="hidden" name="intent" value="undecline-skill" />
                  <input type="hidden" name="skill_id" value={skill.skillId} />
                  <button
                    type="submit"
                    disabled={fetcher.state !== "idle"}
                    className={buttonClasses("quiet")}
                  >
                    {flying === skill.skillId ? "Turning on…" : "Turn on"}
                  </button>
                </fetcher.Form>
              </span>
            </li>
          ))}
        </ul>
      </Card>
    </section>
  );
}

function AddSection({
  addable,
}: {
  addable: ReturnType<typeof useLoaderData<typeof loader>>["addable"];
}) {
  const fetcher = useFetcher<ProfileActionData>();
  const adding = flyingSkillId(fetcher);
  if (addable.length === 0) {
    return null;
  }
  return (
    <section aria-labelledby="profile-add-heading" className="space-y-3">
      <SectionHeading>
        <span id="profile-add-heading">Add from the catalog</span>
      </SectionHeading>
      <Card className="overflow-hidden">
        <ul>
          {addable.map((skill) => (
            <li
              key={skill.skillId}
              data-testid={`profile-addable-${skill.name}`}
              className="flex items-center gap-3 border-line-soft border-b px-4 py-3 last:border-b-0"
            >
              <Package aria-hidden className="size-4 shrink-0 text-faint" />
              <span className="min-w-0 truncate text-ink text-sm">
                {skill.displayName ?? skill.name}
              </span>
              <span className="ml-auto">
                <fetcher.Form method="post">
                  <input type="hidden" name="intent" value="add-skill" />
                  <input type="hidden" name="skill_id" value={skill.skillId} />
                  <button
                    type="submit"
                    disabled={fetcher.state !== "idle"}
                    className={buttonClasses("quiet")}
                  >
                    <Plus aria-hidden className="mr-1 inline size-3.5" />
                    {adding === skill.skillId ? "Adding…" : "Add"}
                  </button>
                </fetcher.Form>
              </span>
            </li>
          ))}
        </ul>
      </Card>
    </section>
  );
}
