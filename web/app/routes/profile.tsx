import { Package, Plus } from "lucide-react";
import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, useFetcher, useLoaderData } from "react-router";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading, ShortId } from "@/components/ui";
import { requireMember, requireMemberInScope } from "@/lib/auth/guards.server";
import {
  includeChannelInProfile,
  removeChannelFromProfile,
} from "@/lib/db/queries.channels.server";
import {
  deliveryFor,
  laneChannels,
  profileIncludeBundle,
  profileOf,
  profileRemoveBundle,
} from "@/lib/db/queries.lane.server";
import { skillIndexOf } from "@/lib/db/queries.server";
import { useWsPath } from "@/lib/ws-path";

export function meta() {
  return [{ title: "Your skills" }];
}

/**
 * The PROFILE editor — the web face of the person-side manifest ("Your skills"): the same
 * per-(user, workspace) include/exclude lines `topos add -g` / `remove -g` edit, so a
 * non-technical member can shape what their agents receive without a terminal. Server-stored,
 * so it roams: every machine this person logs into delivers the same set.
 *
 * Three sections: what the profile DELIVERS right now (the resolved set, with why), the
 * CHANNELS (the baseline + curated sets — carry or drop each), and the DIRECT skill lines
 * (add from the catalog; removing something a channel still provides records the one exclude
 * line, disclosed inline). All acts are SELF-scoped (personal lines, nobody else's) — plain
 * one-click toggles, no ceremony.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { actor } = await requireMemberInScope(request, params);
  const [delivery, channels, entries, catalog] = await Promise.all([
    deliveryFor(actor),
    laneChannels(actor),
    profileOf(actor),
    skillIndexOf(actor, actor.workspaceId),
  ]);
  const included = new Set(
    entries.filter((e) => e.mode === "include" && e.kind === "skill").map((e) => e.name),
  );
  const excluded = new Set(
    entries.filter((e) => e.mode === "exclude" && e.kind === "skill").map((e) => e.name),
  );
  const pins = new Map(
    entries
      .filter((e) => e.mode === "include" && e.kind === "skill" && e.pin !== null)
      .map((e) => [e.name, e.pin as string]),
  );
  return {
    delivered: delivery.skills.map((s) => ({
      skillId: s.skill_id,
      name: s.name,
      displayName: s.display_name ?? null,
      versionId: s.version_id,
      viaChannels: s.via.channels,
      direct: s.via.direct,
      pin: pins.get(s.name) ?? null,
    })),
    channels: channels.map((c) => ({
      channelId: c.channelId,
      name: c.name,
      mode: c.mode,
      builtin: c.builtin,
      included: c.included,
      skillCount: c.skills.length,
    })),
    excluded: [...excluded].sort(),
    // The add picker: active catalog skills the profile does not already deliver or include.
    addable: catalog
      .filter((row) => !included.has(row.name))
      .filter((row) => !delivery.skills.some((s) => s.skill_id === row.skillId))
      .map((row) => ({ skillId: row.skillId, name: row.name, displayName: row.displayName })),
  };
}

type ProfileActionData = { intent: string; status: string };

/**
 * The profile's self-service acts, dispatched on the hidden `intent` — all four are personal
 * lines (the data layer's profile ops), naturally idempotent, unconfirmed. Channel intents key
 * on the IMMUTABLE channel id; skill intents on the immutable bundle id.
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
      case "include-skill":
        return data<ProfileActionData>({
          intent,
          status: await profileIncludeBundle(actor, skillId, null),
        });
      case "remove-skill":
        return data<ProfileActionData>({
          intent,
          status: await profileRemoveBundle(actor, skillId),
        });
      case "include-channel":
        return data<ProfileActionData>({
          intent,
          status: await includeChannelInProfile(actor, channelId),
        });
      case "remove-channel":
        return data<ProfileActionData>({
          intent,
          status: await removeChannelFromProfile(actor, channelId),
        });
      default:
        return data<ProfileActionData>({ intent: "unknown", status: "error" }, { status: 400 });
    }
  } catch {
    return data<ProfileActionData>({ intent, status: "error" }, { status: 500 });
  }
}

export default function ProfilePage() {
  const { delivered, channels, excluded, addable } = useLoaderData<typeof loader>();
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
        Your profile is the set of skills YOUR agents receive in this workspace — on every machine
        you log in to. It starts with the workspace baseline (the{" "}
        <span className="font-mono text-[13px]">everyone</span> channel); carry or drop whole sets
        below, add individual skills, or remove one — removing a skill a set still provides records
        a personal exclude, and adding it back clears it. From a terminal,{" "}
        <code className="rounded bg-panel2 px-1.5 py-0.5 font-mono text-[13px]">topos add -g</code>{" "}
        and <code className="rounded bg-panel2 px-1.5 py-0.5 font-mono text-[13px]">remove -g</code>{" "}
        edit the same lines.
      </p>
      <DeliveredSection delivered={delivered} />
      <ChannelsSection channels={channels} />
      {excluded.length > 0 && <ExcludedSection excluded={excluded} />}
      <AddSection addable={addable} />
    </div>
  );
}

function DeliveredSection({
  delivered,
}: {
  delivered: ReturnType<typeof useLoaderData<typeof loader>>["delivered"];
}) {
  const wsPath = useWsPath();
  const fetcher = useFetcher<ProfileActionData>();
  const excludedNote =
    fetcher.data?.intent === "remove-skill" && fetcher.data.status === "excluded"
      ? "Removed — a channel still carries it, so a personal exclude line now holds it back. Adding it again clears the exclude."
      : undefined;
  return (
    <section aria-labelledby="delivered-heading" className="space-y-3">
      <SectionHeading>
        <span id="delivered-heading">Delivered to your agents</span>
      </SectionHeading>
      {excludedNote !== undefined && (
        <p role="status" className="text-dim text-sm">
          {excludedNote}
        </p>
      )}
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
                {skill.pin !== null && <Chip tone="pending">pinned</Chip>}
                <span className="text-faint text-xs">
                  {skill.viaChannels.length > 0 && <>via {skill.viaChannels.join(", ")}</>}
                  {skill.viaChannels.length > 0 && skill.direct && " · "}
                  {skill.direct && "added by you"}
                </span>
                <span className="ml-auto">
                  <fetcher.Form method="post">
                    <input type="hidden" name="intent" value="remove-skill" />
                    <input type="hidden" name="skill_id" value={skill.skillId} />
                    <button
                      type="submit"
                      disabled={fetcher.state !== "idle"}
                      className={buttonClasses("quiet")}
                    >
                      Remove
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
                {channel.included ? (
                  <Chip tone="verified">in your skills</Chip>
                ) : (
                  <Chip tone="neutral">not carried</Chip>
                )}
                <fetcher.Form method="post">
                  <input
                    type="hidden"
                    name="intent"
                    value={channel.included ? "remove-channel" : "include-channel"}
                  />
                  <input type="hidden" name="channel_id" value={channel.channelId} />
                  <ChannelToggle name={channel.name} included={channel.included} />
                </fetcher.Form>
              </span>
            </li>
          ))}
        </ul>
      </Card>
    </section>
  );
}

function ChannelToggle({ name, included }: { name: string; included: boolean }) {
  return (
    <button type="submit" className={buttonClasses("quiet")}>
      {included ? `Drop ${name}` : `Carry ${name}`}
    </button>
  );
}

function ExcludedSection({ excluded }: { excluded: string[] }) {
  return (
    <section aria-labelledby="excluded-heading" className="space-y-3">
      <SectionHeading>
        <span id="excluded-heading">Excluded by you</span>
      </SectionHeading>
      <p className="text-dim text-sm leading-relaxed">
        The one kind of negative line a profile holds: these skills are provided by a channel you
        carry, but your exclude holds them back. Adding one back (below, or{" "}
        <code className="font-mono text-[13px]">topos add -g</code>) clears it.
      </p>
      <div className="flex flex-wrap gap-2">
        {excluded.map((name) => (
          <Chip key={name} tone="neutral">
            {name}
          </Chip>
        ))}
      </div>
    </section>
  );
}

function AddSection({
  addable,
}: {
  addable: ReturnType<typeof useLoaderData<typeof loader>>["addable"];
}) {
  const fetcher = useFetcher<ProfileActionData>();
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
                  <input type="hidden" name="intent" value="include-skill" />
                  <input type="hidden" name="skill_id" value={skill.skillId} />
                  <button
                    type="submit"
                    disabled={fetcher.state !== "idle"}
                    className={buttonClasses("quiet")}
                  >
                    <Plus aria-hidden className="mr-1 inline size-3.5" />
                    Add
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
