import { Package, Plus } from "lucide-react";
import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, useFetcher, useLoaderData, useSearchParams } from "react-router";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading, ShortId } from "@/components/ui";
import { requireMember, requireMemberInScope } from "@/lib/auth/guards.server";
import { baseForKind, bundlePath } from "@/lib/bundle-base";
import {
  type AssignedBundle,
  type AssignedChannel,
  addToMine,
  assignedView,
  declineBundle,
  type FeedAttribution,
  unassignChannelFromSelf,
  undeclineBundle,
  unpickBundle,
} from "@/lib/db/queries.feed.server";
import { skillIndexOf } from "@/lib/db/queries.server";
import { cn } from "@/lib/utils";
import { useWsPath } from "@/lib/ws-path";

export function meta() {
  return [{ title: "Your skills" }];
}

/**
 * YOUR SKILLS — the web face of a person's assignments: what this workspace says their agents
 * should have, and the switches they hold over it. Server-stored, so it roams: every machine
 * they log in from resolves the same set.
 *
 * Two views, one page. MINE groups everything effectively theirs by what puts it there — the
 * workspace baseline, each assigned channel, anything aimed straight at them, and their own
 * picks — because "why do I have this" is the question a list of names cannot answer. LIBRARY
 * is the workspace catalog, browsable, with one act on it: add this to mine.
 *
 * The model underneath is two rows. An ASSIGNMENT is positive (a curator's aim, or the
 * person's own click — the row is identical either way). A DECLINE is the one negative: off
 * for me, whatever assigns it, surviving new versions and channel reshuffles. Turning
 * something off never removes it from the team library, which is why a declined row stays on
 * this page, dimmed, with its switch. Every act here is SELF-scoped — plain toggles, no
 * ceremony, nobody else's rows touched.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { actor } = await requireMemberInScope(request, params);
  const [view, catalog] = await Promise.all([
    assignedView(actor),
    skillIndexOf(actor, actor.workspaceId),
  ]);

  // The feed's own predicate, read off the grouped view: assigned (however) minus declined.
  const inFeed = new Set<string>();
  const declined = new Set<string>();
  for (const bundle of [
    ...(view.baseline?.bundles ?? []),
    ...view.channels.flatMap((c) => c.bundles),
    ...view.assigned,
    ...view.picked,
  ]) {
    inFeed.add(bundle.bundleId);
    if (bundle.declined) {
      declined.add(bundle.bundleId);
    }
  }

  return {
    view,
    reaching: [...inFeed].filter((id) => !declined.has(id)).length,
    // The catalog, each entry carrying the state this person is already in with it — the row
    // shows a state OR an act, never both.
    library: catalog.map((row) => ({
      skillId: row.skillId,
      name: row.name,
      displayName: row.displayName,
      kind: row.kind,
      versionId: row.versionId,
      state: declined.has(row.skillId)
        ? ("off" as const)
        : inFeed.has(row.skillId)
          ? ("mine" as const)
          : ("addable" as const),
    })),
  };
}

type FeedActionData = { intent: string; status: string };

/**
 * The page's self-service acts, dispatched on the hidden `intent` — all personal rows (the feed
 * data layer), naturally idempotent, unconfirmed. Skill intents key on the immutable bundle id,
 * the one channel intent on the immutable channel id.
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
        return data<FeedActionData>({ intent, status: await addToMine(actor, skillId) });
      case "decline-skill":
        return data<FeedActionData>({ intent, status: await declineBundle(actor, skillId) });
      case "undecline-skill":
        return data<FeedActionData>({ intent, status: await undeclineBundle(actor, skillId) });
      case "unpick-skill":
        return data<FeedActionData>({ intent, status: await unpickBundle(actor, skillId) });
      case "unpick-channel":
        return data<FeedActionData>({
          intent,
          status: await unassignChannelFromSelf(actor, channelId),
        });
      default:
        return data<FeedActionData>({ intent: "unknown", status: "error" }, { status: 400 });
    }
  } catch {
    return data<FeedActionData>({ intent, status: "error" }, { status: 500 });
  }
}

type LoaderData = ReturnType<typeof useLoaderData<typeof loader>>;

export default function AssignmentsPage() {
  const { view, reaching, library } = useLoaderData<typeof loader>();
  const [searchParams] = useSearchParams();
  const tab = searchParams.get("tab") === "library" ? "library" : "mine";
  return (
    <div className="space-y-8">
      <PageHeader
        title="Your skills"
        meta={
          reaching === 1
            ? "1 skill reaching your agents"
            : `${reaching} skills reaching your agents`
        }
      />
      <ViewTabs active={tab} />
      {tab === "mine" ? <MineView view={view} /> : <LibraryView library={library} />}
    </div>
  );
}

/** The two views, in the settings tab-row idiom: same hairline, same mono label, aria-current
 *  on the one you are reading. The choice rides the URL, so a view is linkable and a reload
 *  keeps it. */
function ViewTabs({ active }: { active: "mine" | "library" }) {
  const tabs = [
    { id: "mine", label: "Mine", to: "?tab=mine" },
    { id: "library", label: "Library", to: "?tab=library" },
  ] as const;
  return (
    <nav aria-label="Your skills views" className="flex gap-1 border-line-soft border-b">
      {tabs.map((tab) => (
        <Link
          key={tab.id}
          to={tab.to}
          data-testid={`profile-tab-${tab.id}`}
          aria-current={tab.id === active ? "page" : undefined}
          className={cn(
            "-mb-px border-b-2 px-3 py-2 font-mono text-[13px] transition-colors focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2",
            tab.id === active
              ? "border-accent text-ink"
              : "border-transparent text-dim hover:text-ink",
          )}
        >
          {tab.label}
        </Link>
      ))}
    </nav>
  );
}

// ── Mine ─────────────────────────────────────────────────────────────────────────────────────

function MineView({ view }: { view: LoaderData["view"] }) {
  const wsPath = useWsPath();
  const empty =
    view.baseline === null &&
    view.channels.length === 0 &&
    view.assigned.length === 0 &&
    view.picked.length === 0;
  return (
    <div className="space-y-8">
      <p className="max-w-2xl text-dim text-sm leading-relaxed">
        Everything this workspace gives you, grouped by what puts it there. Turn any skill off: it
        stops arriving on your machines and stays in the team library, and turning it back on is one
        click.{" "}
        <Link to={wsPath("visibility")} className="text-ink underline decoration-hairline">
          What the team can and cannot see
        </Link>
        .
      </p>
      {empty ? (
        <EmptyMine />
      ) : (
        <>
          {view.baseline !== null && (
            <ChannelGroup
              channel={view.baseline}
              heading="Baseline"
              testId="profile-group-baseline"
            />
          )}
          {view.channels.length > 0 && (
            <section aria-labelledby="mine-channels-heading" className="space-y-4">
              <SectionHeading>
                <span id="mine-channels-heading">Channels</span>
              </SectionHeading>
              {view.channels.map((channel) => (
                <ChannelGroup key={channel.channelId} channel={channel} />
              ))}
            </section>
          )}
          {view.assigned.length > 0 && (
            <section
              aria-labelledby="mine-assigned-heading"
              data-testid="profile-group-assigned"
              className="space-y-3"
            >
              <SectionHeading>
                <span id="mine-assigned-heading">Assigned to you</span>
              </SectionHeading>
              <BundleRows rows={view.assigned} />
            </section>
          )}
          {view.picked.length > 0 && (
            <section
              aria-labelledby="mine-picked-heading"
              data-testid="profile-group-picked"
              className="space-y-3"
            >
              <SectionHeading>
                <span id="mine-picked-heading">Picked by me</span>
              </SectionHeading>
              <BundleRows
                rows={view.picked.map((b) => ({ ...b, attribution: { by: "you" as const } }))}
                unaddable
              />
            </section>
          )}
        </>
      )}
    </div>
  );
}

function EmptyMine() {
  const wsPath = useWsPath();
  return (
    <div className="rounded-lg border border-line-soft border-dashed bg-panel px-6 py-12 text-center">
      <h2 className="font-display font-semibold text-base text-ink tracking-[-0.02em]">
        Nothing assigned yet
      </h2>
      <p className="mx-auto mt-2 max-w-md text-dim text-sm leading-relaxed">
        When a curator assigns you a skill or a channel it appears here. You can also{" "}
        <Link to="?tab=library" className="text-ink underline decoration-hairline">
          browse the library
        </Link>{" "}
        and add one yourself, or carry a whole set from its{" "}
        <Link to={wsPath("channels")} className="text-ink underline decoration-hairline">
          channel page
        </Link>
        .
      </p>
    </div>
  );
}

/**
 * One assigned channel: its name (linked to the set's own page), what put it in this feed, and
 * the bundles it carries today. A channel the person carries THEMSELVES gets the quiet un-add —
 * the inverse of their own click, and the only channel row anyone may take back.
 */
function ChannelGroup({
  channel,
  heading,
  testId,
}: {
  channel: AssignedChannel;
  heading?: string;
  testId?: string;
}) {
  const wsPath = useWsPath();
  const fetcher = useFetcher<FeedActionData>();
  const body = (
    <div data-testid={testId ?? `profile-group-channel-${channel.name}`} className="space-y-3">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <Link
          to={wsPath(`channels/${channel.name}`)}
          className="font-medium text-ink text-sm hover:underline"
        >
          {channel.name}
        </Link>
        {channel.isDefault && <Chip tone="neutral">baseline</Chip>}
        <span className="text-faint text-xs">
          {attributionLabel(channel.attribution)} ·{" "}
          {channel.bundles.length === 1 ? "1 skill" : `${channel.bundles.length} skills`}
        </span>
        {channel.attribution.by === "you" && !channel.isDefault && (
          <fetcher.Form method="post" className="ml-auto">
            <input type="hidden" name="intent" value="unpick-channel" />
            <input type="hidden" name="channel_id" value={channel.channelId} />
            <button
              type="submit"
              disabled={fetcher.state !== "idle"}
              className={buttonClasses("quiet")}
            >
              {fetcher.state === "idle" ? "Un-add" : "Un-adding…"}
            </button>
          </fetcher.Form>
        )}
      </div>
      {channel.bundles.length === 0 ? (
        <p className="text-faint text-sm">This channel carries nothing yet.</p>
      ) : (
        <BundleRows
          rows={channel.bundles.map((b) => ({ ...b, attribution: channel.attribution }))}
        />
      )}
    </div>
  );
  if (heading === undefined) {
    return body;
  }
  return (
    <section aria-labelledby={`mine-${heading.toLowerCase()}-heading`} className="space-y-3">
      <SectionHeading>
        <span id={`mine-${heading.toLowerCase()}-heading`}>{heading}</span>
      </SectionHeading>
      {body}
    </section>
  );
}

type Row = AssignedBundle & { attribution: FeedAttribution };

/**
 * The bundle rows of one group. ONE fetcher serves them all, so every button disables together
 * while a submit is on the wire — but only the row that was clicked names its wait; without the
 * id check the whole list would read "Turning off…".
 */
function BundleRows({ rows, unaddable = false }: { rows: Row[]; unaddable?: boolean }) {
  const wsPath = useWsPath();
  const fetcher = useFetcher<FeedActionData>();
  const busy = fetcher.state !== "idle";
  const flying = busy ? (fetcher.formData?.get("skill_id")?.toString() ?? null) : null;
  const flyingIntent = fetcher.formData?.get("intent")?.toString();
  return (
    <Card className="overflow-hidden">
      <ul>
        {rows.map((row) => (
          <li
            key={row.bundleId}
            data-testid={`profile-row-${row.name}`}
            className={cn(
              "flex flex-wrap items-center gap-x-3 gap-y-1 border-line-soft border-b px-4 py-3 last:border-b-0",
              row.declined && "opacity-60",
            )}
          >
            <Link
              to={wsPath(bundlePath(baseForKind(row.kind), row.name))}
              className={cn(
                "min-w-0 truncate text-sm hover:underline",
                row.declined ? "text-dim" : "font-medium text-ink",
              )}
            >
              {row.displayName ?? row.name}
            </Link>
            {row.versionId !== null && <ShortId value={row.versionId} />}
            <span className="text-faint text-xs">
              {row.declined ? "off — still in the team library" : attributionLabel(row.attribution)}
            </span>
            <span className="ml-auto flex items-center gap-2">
              {/* Two distinct affordances, never merged: un-adding takes back this person's own
                  act (and leaves anything a channel still carries arriving), while the switch
                  holds the skill back whatever assigns it. */}
              {unaddable && (
                <fetcher.Form method="post">
                  <input type="hidden" name="intent" value="unpick-skill" />
                  <input type="hidden" name="skill_id" value={row.bundleId} />
                  <button type="submit" disabled={busy} className={buttonClasses("quiet")}>
                    {flying === row.bundleId && flyingIntent === "unpick-skill"
                      ? "Un-adding…"
                      : "Un-add"}
                  </button>
                </fetcher.Form>
              )}
              <fetcher.Form method="post">
                <input
                  type="hidden"
                  name="intent"
                  value={row.declined ? "undecline-skill" : "decline-skill"}
                />
                <input type="hidden" name="skill_id" value={row.bundleId} />
                <button type="submit" disabled={busy} className={buttonClasses("quiet")}>
                  {flying === row.bundleId && flyingIntent !== "unpick-skill"
                    ? row.declined
                      ? "Turning on…"
                      : "Turning off…"
                    : row.declined
                      ? "Turn on"
                      : "Turn off"}
                </button>
              </fetcher.Form>
            </span>
          </li>
        ))}
      </ul>
    </Card>
  );
}

/** The one attribution line: the assignment row read out loud. */
function attributionLabel(attribution: FeedAttribution): string {
  if (attribution.by === "everyone") {
    return "assigned to everyone";
  }
  if (attribution.by === "you") {
    return "picked by you";
  }
  return `assigned by ${attribution.display}`;
}

// ── Library ──────────────────────────────────────────────────────────────────────────────────

/**
 * The workspace catalog, browsable. A row shows the state this person is already in — mine, or
 * turned off — or the one act available: add it. Adding also clears a standing decline, because
 * asking for a thing and holding it back are contradictory stances and the newer one is the
 * real intent.
 */
function LibraryView({ library }: { library: LoaderData["library"] }) {
  const wsPath = useWsPath();
  const fetcher = useFetcher<FeedActionData>();
  const busy = fetcher.state !== "idle";
  const flying = busy ? (fetcher.formData?.get("skill_id")?.toString() ?? null) : null;
  if (library.length === 0) {
    return (
      <div className="rounded-lg border border-line-soft border-dashed bg-panel px-6 py-12 text-center">
        <h2 className="font-display font-semibold text-base text-ink tracking-[-0.02em]">
          The library is empty
        </h2>
        <p className="mx-auto mt-2 max-w-md text-dim text-sm leading-relaxed">
          Nothing has been published to this workspace yet. Publish a skill from your agent and it
          appears here for everyone.
        </p>
      </div>
    );
  }
  return (
    <div className="space-y-4">
      <p className="max-w-2xl text-dim text-sm leading-relaxed">
        Every skill and MCP server this workspace holds. Adding one puts it in your feed on every
        machine you log in from; it stays in the library either way.
      </p>
      <Card className="overflow-hidden">
        <ul>
          {library.map((entry) => (
            <li
              key={entry.skillId}
              data-testid={`profile-library-${entry.name}`}
              className="flex flex-wrap items-center gap-x-3 gap-y-1 border-line-soft border-b px-4 py-3 last:border-b-0"
            >
              <Package aria-hidden className="size-4 shrink-0 text-faint" />
              <Link
                to={wsPath(bundlePath(baseForKind(entry.kind), entry.name))}
                className="min-w-0 truncate font-medium text-ink text-sm hover:underline"
              >
                {entry.displayName ?? entry.name}
              </Link>
              {entry.versionId !== null && <ShortId value={entry.versionId} />}
              <span className="ml-auto flex items-center gap-2">
                {entry.state === "mine" && <Chip tone="verified">in your skills</Chip>}
                {entry.state === "off" && <Chip tone="neutral">off for you</Chip>}
                {entry.state === "addable" && (
                  <fetcher.Form method="post">
                    <input type="hidden" name="intent" value="add-skill" />
                    <input type="hidden" name="skill_id" value={entry.skillId} />
                    <button type="submit" disabled={busy} className={buttonClasses("quiet")}>
                      <Plus aria-hidden className="mr-1 inline size-3.5" />
                      {flying === entry.skillId ? "Adding…" : "Add to mine"}
                    </button>
                  </fetcher.Form>
                )}
              </span>
            </li>
          ))}
        </ul>
      </Card>
    </div>
  );
}
