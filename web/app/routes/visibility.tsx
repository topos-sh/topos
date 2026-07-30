import type { LoaderFunctionArgs } from "react-router";
import { Link, useLoaderData } from "react-router";
import { relativeTime } from "@/components/format";
import { buttonClasses, Card, PageHeader, SectionHeading, ShortId } from "@/components/ui";
import { requireMemberInScope } from "@/lib/auth/guards.server";
import { type VisibleSession, visibleSessionsOf } from "@/lib/db/queries.sessions.server";
import { useWsPath } from "@/lib/ws-path";

export function meta() {
  return [{ title: "What the team can see" }];
}

/**
 * WHAT THE TEAM CAN SEE — the disclosure page, written in the order that matters: the limits
 * first, then the reporting, then the actual rows for the reader's own machines. Somebody
 * deciding whether to log a work machine into a shared workspace is asking "what does this
 * expose", and the honest answer leads with what it does not.
 *
 * Everything below is a statement about the SYSTEM, not a promise of intent: the reporting lane
 * carries bundle ids and version ids, so there is nothing else for a workspace to read. The
 * table at the end is the same four fields the prose names, queried live for the viewer — proof
 * rather than illustration.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { actor } = await requireMemberInScope(request, params);
  return { sessions: await visibleSessionsOf(actor) };
}

export default function VisibilityPage() {
  const { sessions } = useLoaderData<typeof loader>();
  const wsPath = useWsPath();
  return (
    <div className="space-y-8">
      <PageHeader
        title="What the team can see"
        meta="The limits of what a workspace reads from your machines"
        actions={
          <Link to={wsPath("settings/sessions")} className={buttonClasses("quiet")}>
            Sessions
          </Link>
        }
      />
      <NeverSeen />
      <Reported />
      <YourSessions sessions={sessions} />
    </div>
  );
}

/** The limits, first — the four things no report and no lane ever carries. */
function NeverSeen() {
  return (
    <section
      aria-labelledby="never-seen-heading"
      data-testid="visibility-never"
      className="space-y-3"
    >
      <SectionHeading>
        <span id="never-seen-heading">The team can never see</span>
      </SectionHeading>
      <Card className="px-5 py-4">
        <ul className="space-y-3 text-dim text-sm leading-relaxed">
          <li>
            <span className="font-medium text-ink">The contents of your files.</span> Topos moves
            the bundles you publish and nothing else. No other file on the machine is read, listed,
            or uploaded.
          </li>
          <li>
            <span className="font-medium text-ink">Your prompts and conversations.</span> What you
            ask an agent, and what it answers, never leaves the machine through Topos — there is no
            field on the wire that could carry it.
          </li>
          <li>
            <span className="font-medium text-ink">Your repository&apos;s code.</span> A project
            that pins shared skills is still just a folder on your disk; Topos writes skills into it
            and reads nothing back out.
          </li>
          <li>
            <span className="font-medium text-ink">Anything you have not published.</span> A skill
            you keep local is invisible here, and so are your local edits to a shared one — they
            stay yours until you propose them.
          </li>
        </ul>
      </Card>
    </section>
  );
}

/** What the workspace does read — short, and each line maps to a column of the table below. */
function Reported() {
  return (
    <section aria-labelledby="reported-heading" data-testid="visibility-sees" className="space-y-3">
      <SectionHeading>
        <span id="reported-heading">What the team does see</span>
      </SectionHeading>
      <Card className="px-5 py-4">
        <ul className="space-y-3 text-dim text-sm leading-relaxed">
          <li>
            <span className="font-medium text-ink">Each machine you log in,</span> by the name it
            gave itself at login — one session per workspace, per installation.
          </li>
          <li>
            <span className="font-medium text-ink">Which shared skills it holds,</span> by name.
            Names of skills in this workspace; nothing about what they do on your machine.
          </li>
          <li>
            <span className="font-medium text-ink">The version of each one,</span> so a curator can
            tell whether a fix has landed everywhere.
          </li>
          <li>
            <span className="font-medium text-ink">When it last reported,</span> which is how a
            machine reads fresh or stale.
          </li>
        </ul>
        <p className="mt-4 border-line-soft border-t pt-4 text-faint text-sm leading-relaxed">
          Bytes you publish are shared on purpose — a published version is visible to the workspace
          in full, and stays in its history. Acts with reach (publishing, approving, reverting) are
          recorded with your name.
        </p>
      </Card>
    </section>
  );
}

/**
 * The proof: the reader's own sessions with exactly the fields named above. An empty list is
 * the honest answer too — nothing has reported, so there is nothing to show.
 */
function YourSessions({ sessions }: { sessions: VisibleSession[] }) {
  return (
    <section
      aria-labelledby="your-machines-heading"
      data-testid="visibility-your-sessions"
      className="space-y-3"
    >
      <SectionHeading>
        <span id="your-machines-heading">Your machines, as this workspace reads them</span>
      </SectionHeading>
      {sessions.length === 0 ? (
        <p className="text-dim text-sm leading-relaxed">
          No machine of yours is logged into this workspace, so it reads nothing about you at all.
        </p>
      ) : (
        <Card className="overflow-hidden">
          <ul>
            {sessions.map((session) => (
              <li
                key={session.sessionId}
                data-testid={`visibility-session-${session.sessionId}`}
                className="space-y-2 border-line-soft border-b px-4 py-3 last:border-b-0"
              >
                <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
                  <span className="text-ink text-sm">{session.displayName}</span>
                  <span className="text-faint text-xs">
                    {session.lastSeenAtMs === null
                      ? "has never reported"
                      : `last reported ${relativeTime(new Date(session.lastSeenAtMs))}`}
                  </span>
                </div>
                {session.skills.length === 0 ? (
                  <p className="text-faint text-xs">No skills reported for this machine.</p>
                ) : (
                  <ul className="space-y-1">
                    {session.skills.map((skill) => (
                      <li key={skill.name} className="flex flex-wrap items-center gap-x-2 gap-y-1">
                        <span className="min-w-0 truncate text-dim text-sm">{skill.name}</span>
                        <ShortId value={skill.appliedVersionId} />
                      </li>
                    ))}
                  </ul>
                )}
              </li>
            ))}
          </ul>
        </Card>
      )}
    </section>
  );
}
