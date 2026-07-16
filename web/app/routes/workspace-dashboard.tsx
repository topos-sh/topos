import type { LoaderFunctionArgs, MetaFunction } from "react-router";
import { Link, useLoaderData } from "react-router";
import { NoSkills } from "@/components/empty-states";
import { relativeTime } from "@/components/format";
import { LandingPage } from "@/components/landing/landing-page";
import { AddressBlock } from "@/components/members/address-block";
import { ResourcePage } from "@/components/resource-page";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading, ShortId } from "@/components/ui";
import { composition } from "@/composition.server";
import { serverEnv } from "@/env.server";
import { actorFromSession, requireMember, workspaceInScope } from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { theWorkspace } from "@/lib/db/identity.server";
import { rosterOf } from "@/lib/db/queries.roster.server";
import { type SkillIndexRow, skillIndexOf } from "@/lib/db/queries.server";
import { useWsPath } from "@/lib/ws-path";
import { workspaceAddress } from "@/lib/ws-url.server";

export const meta: MetaFunction<typeof loader> = ({ loaderData }) => {
  if (loaderData?.face === "page") {
    return [{ title: `${loaderData.name} · Topos` }];
  }
  if (loaderData?.face === "landing") {
    return [
      { title: "Topos: align the behavior of every agent in your team" },
      {
        name: "description",
        content:
          "Your agents share skills, keep them current, and improve them together: one teammate’s fix upgrades every agent on the team.",
      },
    ];
  }
  return [{ title: "A Topos resource address" }];
};

/**
 * The workspace ROOT face — resource address and canonical dashboard as ONE route (`/` in single
 * tenancy, `/:ws` in multi). Per-request admission, same table as the retired resource-* routes:
 *  - a non-browser DOCUMENT fetch gets the CONSTANT protocol card (the server entry, before this
 *    loader runs — no existence signal leaks);
 *  - an anonymous browser gets the constant teaser — the LANDING page in single tenancy (with the
 *    first-run claim band while unclaimed), the constant resource teaser in multi;
 *  - a signed-in member gets the dashboard WITH the app chrome (face-shell);
 *  - anyone else — a signed-in non-member, an unknown multi slug — the house 404.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const session = await getAuth().api.getSession({ headers: request.headers });
  const actor = actorFromSession(session);
  if (actor === null) {
    // Anonymous browser: the constant teaser. In single tenancy the origin root IS the landing
    // page, with the one sessionless boolean probe — has this install been claimed yet.
    if (composition.tenancy === "multi") {
      return { face: "teaser" as const };
    }
    const workspace = await theWorkspace();
    const awaitingOwner = workspace === null || workspace.claimedAt === null;
    const origin = (serverEnv().TOPOS_PUBLIC_URL ?? new URL(request.url).origin).replace(
      /\/+$/,
      "",
    );
    return { face: "landing" as const, awaitingOwner, setupLine: `${origin}/claim?code=…` };
  }

  // Signed in: resolve the workspace in scope, then the member gate (a non-member 404s here).
  const workspace = await workspaceInScope(params);
  const memberActor = await requireMember(request, workspace.id);
  const [index, roster] = await Promise.all([
    skillIndexOf(memberActor, workspace.id),
    // Direct seat rows: a seat IS membership, so the count is the roster's length.
    rosterOf(memberActor),
  ]);
  return {
    face: "page" as const,
    name: workspace.displayName,
    // The address slug — what the meta line shows; the AddressBlock gets the full shareable address.
    slug: workspace.name,
    shareAddress: workspaceAddress(request, workspace.name),
    index,
    memberCount: roster.length,
  };
}

export default function WorkspaceDashboard() {
  const data = useLoaderData<typeof loader>();
  if (data.face === "landing") {
    return <LandingPage awaitingOwner={data.awaitingOwner} setupLine={data.setupLine} />;
  }
  if (data.face === "teaser") {
    return <ResourcePage />;
  }
  return <DashboardPage {...data} />;
}

/**
 * The workspace dashboard — the content pane. DB-first: the catalog IS the directory's own tables
 * read in place, so a CLI publish lands its rows and the next page load shows them, nothing else
 * required. No per-row vault call: every row renders from the shared database read.
 */
function DashboardPage({
  name,
  slug,
  shareAddress,
  index,
  memberCount,
}: {
  name: string;
  slug: string;
  shareAddress: string;
  index: SkillIndexRow[];
  memberCount: number;
}) {
  const wsPath = useWsPath();
  return (
    <div className="space-y-8">
      <PageHeader
        title={
          <>
            <span className="text-faint" aria-hidden="true">
              #{" "}
            </span>
            {name}
          </>
        }
        meta={
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <code className="font-mono">{slug}</code>
            <span aria-hidden="true">·</span>
            <span>{index.length === 1 ? "1 skill" : `${index.length} skills`}</span>
            <span aria-hidden="true">·</span>
            <span>{memberCount === 1 ? "1 member" : `${memberCount} members`}</span>
          </div>
        }
        actions={
          <Link to={wsPath("settings")} className={buttonClasses("quiet")}>
            Settings
          </Link>
        }
      />

      {index.length === 0 ? (
        <NoSkills shareAddress={shareAddress} />
      ) : (
        <section aria-labelledby="skill-index-heading" className="space-y-3">
          <div className="flex items-center justify-between gap-3">
            <SectionHeading>
              <span id="skill-index-heading">Skills</span>
            </SectionHeading>
            <span className="text-faint text-xs">Published skills appear here automatically.</span>
          </div>
          <Card className="overflow-hidden">
            <ul>
              {index.map((row) => (
                <CatalogRow key={row.skillId} row={row} />
              ))}
            </ul>
          </Card>
        </section>
      )}

      <section aria-labelledby="address-heading" className="space-y-2">
        <SectionHeading>
          <span id="address-heading">Add my device</span>
        </SectionHeading>
        <Card className="space-y-2 px-4 py-3">
          <p className="text-dim text-sm">
            Enroll another of your own devices — or hand an invited teammate the workspace address.
          </p>
          <AddressBlock address={shareAddress} />
        </Card>
      </section>
    </div>
  );
}

/**
 * One catalog row, rendered ENTIRELY from the DB read (no per-row vault call): the skill's name,
 * the current pointer's short hash, when the pointer last moved (`updatedAtMs` is epoch-ms —
 * `new Date(ms)` here at the display edge), the open-proposal badge, and the recorded
 * `bundle_digest`. When nothing is published yet the pointer is absent — render that honestly
 * rather than assume one. The whole row is the click target, keyed on the catalog NAME.
 */
function CatalogRow({ row }: { row: SkillIndexRow }) {
  const wsPath = useWsPath();
  return (
    <li className="border-line-soft border-b last:border-b-0">
      <Link
        to={wsPath(`skills/${row.name}`)}
        className="flex flex-col gap-1 px-4 py-3 hover:bg-panel2 focus-visible:outline-2 focus-visible:outline-accent focus-visible:-outline-offset-2"
      >
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
          <span className="min-w-0 truncate font-medium text-ink text-sm">
            {row.displayName ?? row.name}
          </span>
          <span className="text-faint text-xs">{row.kind}</span>
          {row.openProposals > 0 && (
            <Chip tone="accent">
              {row.openProposals === 1
                ? "1 proposal awaiting review"
                : `${row.openProposals} proposals awaiting review`}
            </Chip>
          )}
        </div>
        {row.versionId === null ? (
          <div className="text-faint text-xs">Nothing published yet</div>
        ) : (
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-faint text-xs">
            <ShortId value={row.versionId} />
            <span aria-hidden="true">·</span>
            <span>
              updated {row.updatedAtMs === null ? "—" : relativeTime(new Date(row.updatedAtMs))}
            </span>
            <span aria-hidden="true">·</span>
            <span className="font-mono">
              {row.bundleDigest === null ? "—" : `sha-256:${row.bundleDigest.slice(0, 12)}…`}
            </span>
          </div>
        )}
      </Link>
    </li>
  );
}
