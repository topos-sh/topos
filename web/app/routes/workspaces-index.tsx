import type { LoaderFunctionArgs } from "react-router";
import { Link, useLoaderData } from "react-router";
import { NoWorkspaces } from "@/components/empty-states";
import { buttonClasses, Chip, PageHeader, SectionHeading } from "@/components/ui";
import { actorFromSession, requireSession } from "@/lib/auth/guards.server";
import { planeMembershipsFor, type WorkspaceMembership } from "@/lib/db/queries.server";

export function meta() {
  return [{ title: "Workspaces · Topos" }];
}

/**
 * The home pane — the always-rendered index, never a bounce (the sole-membership fast-path lives
 * on /app). The left rail is the primary navigation; this pane is the hub: a card per workspace to
 * jump into (and the main way to see them all on mobile), the create action, and — for a seat
 * still on an invite — the join guidance the invite carries until an agent enrolls. Rows come
 * from the directory's own roster (navigable = a confirmed seat).
 */
export async function loader({ request }: LoaderFunctionArgs) {
  const actor = actorFromSession(await requireSession(request));
  // An unverified session sees the empty state — membership is keyed on verified email only.
  const memberships = actor ? await planeMembershipsFor(actor) : [];
  return { memberships };
}

export default function WorkspacesIndex() {
  const { memberships } = useLoaderData<typeof loader>();
  if (memberships.length === 0) {
    return <NoWorkspaces />;
  }
  const channels = memberships.filter((m) => m.navigable);
  const invited = memberships.filter((m) => !m.navigable);
  return (
    <div className="space-y-8">
      <PageHeader
        title="Your workspaces"
        meta={channels.length > 0 ? "Pick a workspace to open it, or create a new one." : undefined}
        actions={
          <Link to="/workspaces/new" className={buttonClasses("quiet")}>
            New workspace
          </Link>
        }
      />

      {channels.length > 0 ? (
        <ul className="grid gap-3 sm:grid-cols-2">
          {channels.map((m) => (
            <li key={m.id}>
              <ChannelCard membership={m} />
            </li>
          ))}
        </ul>
      ) : (
        <p className="text-dim text-sm">
          You&apos;ve been invited but haven&apos;t joined yet. Connect an agent to confirm your
          seat, or create your own workspace.
        </p>
      )}

      {invited.length > 0 && (
        <section aria-labelledby="invited-heading" className="space-y-3">
          <SectionHeading>
            <span id="invited-heading">Invited</span>
          </SectionHeading>
          <ul className="grid gap-3 sm:grid-cols-2">
            {invited.map((m) => (
              <li key={m.id}>
                <InvitedCard membership={m} />
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}

/** A workspace the actor can enter: the whole card is the click target. */
function ChannelCard({ membership: m }: { membership: WorkspaceMembership }) {
  return (
    <Link
      to={`/workspaces/${m.id}`}
      className="group block h-full rounded-lg border border-line-soft bg-panel px-4 py-4 transition-colors hover:border-line hover:bg-panel2 focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
    >
      <div className="flex items-center gap-1.5">
        <span className="text-faint" aria-hidden="true">
          #
        </span>
        <span className="min-w-0 truncate font-medium text-ink text-sm group-hover:text-accent">
          {m.displayName}
        </span>
      </div>
      <p className="mt-2 truncate font-mono text-faint text-xs">{m.address}</p>
      <p className="mt-2 text-faint text-xs capitalize">{m.role}</p>
    </Link>
  );
}

/** An invited-only seat: visible, not enterable — the card IS the join instructions. */
function InvitedCard({ membership: m }: { membership: WorkspaceMembership }) {
  return (
    <div className="h-full rounded-lg border border-line-soft bg-panel px-4 py-4">
      <div className="flex items-center gap-1.5">
        <span className="text-faint" aria-hidden="true">
          #
        </span>
        <span className="min-w-0 truncate font-medium text-ink text-sm">{m.displayName}</span>
        <span className="ml-auto">
          <Chip>invited</Chip>
        </span>
      </div>
      <p className="mt-2 text-dim text-xs leading-relaxed">
        Paste the workspace address from your invite email to your agent; your seat confirms when a
        device enrolls.
      </p>
    </div>
  );
}
