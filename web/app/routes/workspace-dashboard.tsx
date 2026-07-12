import type { LoaderFunctionArgs } from "react-router";
import { Link, useLoaderData } from "react-router";
import { NoSkills } from "@/components/empty-states";
import { relativeTime } from "@/components/format";
import { AddressBlock } from "@/components/members/address-block";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading, ShortId } from "@/components/ui";
import { notFound, requireMember } from "@/lib/auth/guards.server";
import {
  planeWorkspaceById,
  rosterOf,
  type SkillIndexRow,
  skillIndexOf,
} from "@/lib/db/queries.server";
import { followBase } from "@/lib/plane/follow-base.server";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `${params.ws ?? "Workspace"} · Topos` }];
}

/**
 * The workspace "channel" — the content pane. DB-first: the catalog IS the directory's own tables
 * read in place, so a CLI publish lands its rows and the next page load shows them, nothing else
 * required. No per-row vault call: every row renders from the shared database read.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const actor = await requireMember(request, ws);
  const [workspace, index, roster] = await Promise.all([
    // The directory's own workspace row — `?? ws` is the honest fallback when it has none.
    planeWorkspaceById(actor, ws),
    skillIndexOf(actor, ws),
    // Direct roster rows (no HTTP call): the confirmed-seat count is a display number.
    rosterOf(actor),
  ]);
  const name = workspace?.displayName ?? ws;
  // The address slug — what joining and sharing speak (`topos follow <origin>/<address>`).
  const address = workspace?.name ?? ws;
  const memberCount = roster.filter((s) => s.status === "confirmed").length;
  return {
    ws,
    name,
    address,
    origin: followBase(request),
    index,
    memberCount,
  };
}

export default function WorkspaceDashboard() {
  const { ws, name, address, origin, index, memberCount } = useLoaderData<typeof loader>();
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
            <code className="font-mono">{address}</code>
            <span aria-hidden="true">·</span>
            <span>{index.length === 1 ? "1 skill" : `${index.length} skills`}</span>
            <span aria-hidden="true">·</span>
            <span>{memberCount === 1 ? "1 member" : `${memberCount} members`}</span>
          </div>
        }
        actions={
          <Link to={`/workspaces/${ws}/settings`} className={buttonClasses("quiet")}>
            Settings
          </Link>
        }
      />

      {index.length === 0 ? (
        <NoSkills />
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
                <CatalogRow key={row.skillId} ws={ws} row={row} />
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
          <AddressBlock address={address} origin={origin} />
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
function CatalogRow({ ws, row }: { ws: string; row: SkillIndexRow }) {
  return (
    <li className="border-line-soft border-b last:border-b-0">
      <Link
        to={`/workspaces/${ws}/skills/${row.name}`}
        className="flex flex-col gap-1 px-4 py-3 hover:bg-panel2 focus-visible:outline-2 focus-visible:outline-accent focus-visible:-outline-offset-2"
      >
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
          <span className="min-w-0 truncate font-medium text-ink text-sm">
            {row.displayName ?? row.name}
          </span>
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
