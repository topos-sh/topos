import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { Form, Link, useActionData, useLoaderData, useNavigation } from "react-router";
import { ConfirmButton } from "@/components/confirm";
import { relativeTime, shortDevice } from "@/components/format";
import { SettingsTabs } from "@/components/settings-tabs";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading, ShortId } from "@/components/ui";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { requireMemberInScope, requireWorkspaceOwner } from "@/lib/auth/guards.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import { approveSession, ownerRemoveSession, rejectSession } from "@/lib/db/identity.server";
import {
  type SessionFreshness,
  type SessionSkillState,
  type SessionSkillStatus,
  type WorkspaceSession,
  type WorkspaceSessions,
  workspaceSessions,
} from "@/lib/db/queries.sessions.server";
import { useWsPath } from "@/lib/ws-path";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Sessions · ${params.ws ?? "Workspace"}` }];
}

/**
 * The workspace Sessions page (the Settings section's Sessions tab) — a session is
 * user × workspace × installation, minted by `topos login`. It enumerates the workspace's
 * sessions, the version each one last reported, and — when the session-approval knob holds
 * them — the PENDING sessions awaiting an owner. Owner arms: approve/reject a pending
 * session, remove any session. Removing a session ends that installation's access to THIS
 * workspace and nothing else; bytes already on the machine stay there. A person's own
 * sessions stay self-service on the account page too.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { actor } = await requireMemberInScope(request, params);
  return { view: await workspaceSessions(actor), isOwner: actor.role === "owner" };
}

/**
 * ONE action, dispatched on the hidden `intent` — all three session arms are OWNER-only
 * guard-gated acts (a loader gate never extends to an action). The data layer lands the
 * audit row of every landed act in its own transaction (`session_approved` /
 * `session_rejected` / `session_ended`); the route records the attempts it never sees —
 * mangled forms, faults.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  // The membership FLOOR, hoisted above the intent dispatch: the unmatched-intent 400 must
  // never answer a non-member (in multi tenancy `:ws` is a guessable public name slug).
  const { workspace } = await requireMemberInScope(request, params);
  const owner = await requireWorkspaceOwner(request, workspace.id);
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  const sessionId = String(formData.get("session_id") ?? "").trim();
  const run =
    intent === "approve-session"
      ? approveSession
      : intent === "reject-session"
        ? rejectSession
        : intent === "remove-session"
          ? ownerRemoveSession
          : null;
  if (run === null || sessionId.length === 0) {
    return { status: "error" as const };
  }
  let outcome: "approved" | "rejected" | "removed" | "unknown_session";
  try {
    outcome = await run(owner, workspace.id, sessionId);
  } catch {
    await recordAdminEvent(owner, {
      kind: intent.replace("-", "_"),
      subject: sessionId,
      outcome: "error",
    });
    return { status: "error" as const };
  }
  // unknown_session — the row vanished between render and submit (a concurrent act): the
  // revalidated page shows the truth; nothing to confirm or refuse.
  return { status: outcome };
}

export default function SessionsPage() {
  const { view, isOwner } = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();
  const wsPath = useWsPath();
  const active = view.sessions.filter((s) => s.status === "active");
  const pending = view.sessions.filter((s) => s.status === "pending");
  return (
    <TooltipProvider>
      <div className="space-y-8">
        <PageHeader
          title="Sessions"
          meta={<SessionsMeta view={view} />}
          actions={
            <>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Link to="/account/sessions" className={buttonClasses("quiet")}>
                    Your sessions
                  </Link>
                </TooltipTrigger>
                <TooltipContent>
                  Sessions are self-service too — each person can end their own from their account
                  page or with topos logout.
                </TooltipContent>
              </Tooltip>
              <Link to={wsPath("")} className={buttonClasses("quiet")}>
                Back to workspace
              </Link>
            </>
          }
        />
        <SettingsTabs active="sessions" />
        {actionData !== undefined && <ActionReceipt status={actionData.status} />}
        <IntroCopy wholeWorkspace={view.wholeWorkspace} />
        <ExpiryPolicyNote sessionMaxAgeMs={view.sessionMaxAgeMs} isOwner={isOwner} />
        {(pending.length > 0 || view.sessionApproval === "on") && (
          <PendingSessions sessions={pending} isOwner={isOwner} />
        )}
        <ActiveSessions
          sessions={active}
          wholeWorkspace={view.wholeWorkspace}
          isOwner={isOwner}
          stalenessWindowMs={view.stalenessWindowMs}
        />
      </div>
    </TooltipProvider>
  );
}

/**
 * The post-action receipt — a calm one-liner after an owner arm lands (the page also revalidates,
 * so the truth is already on screen; this names what happened). The Remove line says plainly that
 * ending a session stops delivery and reporting but leaves the copies already on the machine.
 */
function ActionReceipt({ status }: { status: string }) {
  const line: Record<string, string> = {
    approved: "Session approved — it receives on its next sync.",
    rejected: "Session rejected. That login is over; the person can log in again later.",
    removed:
      "Session removed. Future delivery and reporting stop — the copies already on that machine stay put.",
    unknown_session: "That session was already gone — the page is up to date.",
    error: "That didn't go through. Try again.",
  };
  const text = line[status];
  if (text === undefined) {
    return null;
  }
  return (
    <p role="status" className={status === "error" ? "text-red-700 text-sm" : "text-dim text-sm"}>
      {text}
    </p>
  );
}

function SessionsMeta({ view }: { view: WorkspaceSessions }) {
  const count = view.sessions.filter((s) => s.status === "active").length;
  const pending = view.sessions.length - count;
  const stale = view.sessions.filter(
    (s) => s.status === "active" && s.freshness === "stale",
  ).length;
  return (
    <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
      <span>{count === 1 ? "1 active session" : `${count} active sessions`}</span>
      {pending > 0 && (
        <>
          <span aria-hidden="true">·</span>
          <span>{pending === 1 ? "1 pending" : `${pending} pending`}</span>
        </>
      )}
      {stale > 0 && (
        <>
          <span aria-hidden="true">·</span>
          <span>{stale === 1 ? "1 stale" : `${stale} stale`}</span>
        </>
      )}
    </div>
  );
}

function IntroCopy({ wholeWorkspace }: { wholeWorkspace: boolean }) {
  return (
    <p className="text-dim text-sm leading-relaxed">
      {wholeWorkspace ? (
        <>
          Every session logged into this workspace and the version each one last reported. Use it to
          confirm a change has reached everyone — after a fix lands, watch until every non-stale
          session reads <em className="text-ink not-italic">current</em>.
        </>
      ) : (
        <>
          These are your own sessions in this workspace and the version each one last reported.
          Reviewers and owners see everyone&apos;s.
        </>
      )}
    </p>
  );
}

/**
 * The workspace's session-expiry policy, stated where sessions are read (set on Settings →
 * General, owner-only). An expiry is enforced by the session guard, so an over-age session
 * simply stops working — the machine logs in again.
 */
function ExpiryPolicyNote({
  sessionMaxAgeMs,
  isOwner,
}: {
  sessionMaxAgeMs: number | null;
  isOwner: boolean;
}) {
  const wsPath = useWsPath();
  return (
    <p data-testid="sessions-expiry-policy" className="text-faint text-sm leading-relaxed">
      {sessionMaxAgeMs === null ? (
        <>Sessions here do not expire — a login stands until it is ended from either side.</>
      ) : (
        <>
          Sessions here expire after {formatAge(sessionMaxAgeMs)}: past that age a login stops
          working and the machine logs in again.
        </>
      )}{" "}
      {isOwner && (
        <>
          Set on the{" "}
          <Link to={wsPath("settings")} className="text-ink underline decoration-hairline">
            General settings
          </Link>{" "}
          page.
        </>
      )}
    </p>
  );
}

/** A calm "30 days" / "12 hours" age label for the expiry-policy line. */
function formatAge(ms: number): string {
  const hours = Math.round(ms / 3_600_000);
  if (hours < 24) {
    return hours === 1 ? "1 hour" : `${hours} hours`;
  }
  const days = Math.round((ms / 86_400_000) * 10) / 10;
  return days === 1 ? "1 day" : `${days} days`;
}

/**
 * The pending section: sessions awaiting an owner (the session-approval knob holds a
 * non-owner's new login here). Approve activates delivery; reject deletes the session — the
 * person can log in again later. Owner-only arms; everyone seated sees the queue exists.
 */
function PendingSessions({
  sessions,
  isOwner,
}: {
  sessions: WorkspaceSession[];
  isOwner: boolean;
}) {
  return (
    <section aria-labelledby="pending-sessions-heading" className="space-y-3">
      <SectionHeading>
        <span id="pending-sessions-heading">Pending sessions</span>
      </SectionHeading>
      <p className="text-dim text-sm leading-relaxed">
        Session approval is required here: a login by a non-owner waits until an owner approves it.
        Nothing is delivered over a pending session.
      </p>
      {sessions.length === 0 ? (
        <p className="text-faint text-sm">No sessions awaiting approval.</p>
      ) : (
        <Card className="overflow-hidden">
          <ul>
            {sessions.map((session) => (
              <li
                key={session.sessionId}
                data-testid={`sessions-pending-${session.sessionId}`}
                className="flex flex-wrap items-center gap-x-3 gap-y-2 border-line-soft border-b px-4 py-3 last:border-b-0"
              >
                <span className="text-ink text-sm">{session.displayName}</span>
                <span className="font-mono text-faint text-xs">
                  {shortDevice(session.sessionId)}
                </span>
                <span className="text-dim text-sm">
                  {session.ownerDisplay}{" "}
                  <span className="text-faint text-xs">{session.ownerEmail}</span>
                </span>
                <span className="text-faint text-xs">
                  asked {relativeTime(new Date(session.createdAtMs))}
                </span>
                {isOwner && (
                  <span className="ml-auto flex flex-wrap items-center gap-2">
                    <SessionArm
                      intent="approve-session"
                      sessionId={session.sessionId}
                      label="Approve"
                    />
                    <SessionArm
                      intent="reject-session"
                      sessionId={session.sessionId}
                      label="Reject"
                      tone="danger"
                    />
                  </span>
                )}
              </li>
            ))}
          </ul>
        </Card>
      )}
    </section>
  );
}

function ActiveSessions({
  sessions,
  wholeWorkspace,
  isOwner,
  stalenessWindowMs,
}: {
  sessions: WorkspaceSession[];
  wholeWorkspace: boolean;
  isOwner: boolean;
  stalenessWindowMs: number;
}) {
  if (sessions.length === 0) {
    return (
      <div className="rounded-lg border border-line-soft border-dashed bg-panel px-6 py-12 text-center">
        <h2 className="font-display font-semibold text-base text-ink tracking-[-0.02em]">
          No sessions yet
        </h2>
        <p className="mx-auto mt-2 max-w-md text-dim text-sm leading-relaxed">
          A session appears here when a machine logs into this workspace and reports what it is
          running — run{" "}
          <code className="rounded bg-panel2 px-1.5 py-0.5 font-mono text-[13px]">topos login</code>{" "}
          on it, then let it sync once.
        </p>
      </div>
    );
  }

  // Reviewer/owner: group by person. Member: a single flat "Your sessions" list.
  if (!wholeWorkspace) {
    return (
      <section aria-labelledby="your-sessions-heading" className="space-y-3">
        <SectionHeading>
          <span id="your-sessions-heading">Your sessions</span>
        </SectionHeading>
        <div className="space-y-3">
          {sessions.map((session) => (
            <SessionCard
              key={session.sessionId}
              session={session}
              isOwner={false}
              stalenessWindowMs={stalenessWindowMs}
            />
          ))}
        </div>
      </section>
    );
  }

  const groups = groupByOwner(sessions);
  return (
    <section aria-labelledby="sessions-heading" className="space-y-6">
      <div className="space-y-2">
        <SectionHeading>
          <span id="sessions-heading">Active sessions</span>
        </SectionHeading>
        {isOwner && (
          <p className="text-faint text-sm leading-relaxed">
            Removing a session ends delivery and reporting for exactly this workspace — the copies
            already on the machine stay put and must be chased by hand where it matters. People can
            also end their own sessions from{" "}
            <Link to="/account/sessions" className="text-ink underline decoration-hairline">
              their account page
            </Link>{" "}
            or with <code className="font-mono text-[13px]">topos logout</code>.
          </p>
        )}
      </div>
      {groups.map(([ownerUserId, group]) => (
        <div key={ownerUserId} className="space-y-3">
          <h3 className="font-medium text-ink text-sm">
            {group[0]?.ownerDisplay}{" "}
            <span className="font-normal text-faint text-xs">{group[0]?.ownerEmail}</span>
          </h3>
          <div className="space-y-3">
            {group.map((session) => (
              <SessionCard
                key={session.sessionId}
                session={session}
                isOwner={isOwner}
                stalenessWindowMs={stalenessWindowMs}
              />
            ))}
          </div>
        </div>
      ))}
    </section>
  );
}

/** One session arm — a two-step in-place confirm (people-affecting grade, never type-the-name). */
function SessionArm({
  intent,
  sessionId,
  label,
  tone = "primary",
}: {
  intent: "approve-session" | "reject-session" | "remove-session";
  sessionId: string;
  label: string;
  tone?: "primary" | "quiet" | "danger";
}) {
  const navigation = useNavigation();
  const pending =
    navigation.state !== "idle" &&
    navigation.formData?.get("intent") === intent &&
    navigation.formData?.get("session_id") === sessionId;
  return (
    <Form method="post">
      <input type="hidden" name="intent" value={intent} />
      <input type="hidden" name="session_id" value={sessionId} />
      <ConfirmButton label={label} tone={tone} pending={pending} />
    </Form>
  );
}

function SessionCard({
  session,
  isOwner,
  stalenessWindowMs,
}: {
  session: WorkspaceSession;
  isOwner: boolean;
  stalenessWindowMs: number;
}) {
  return (
    <Card className="overflow-hidden">
      <div data-testid={`sessions-session-${session.sessionId}`} className="space-y-3 px-4 py-3">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
          <span className="text-ink text-sm">{session.displayName}</span>
          <span className="font-mono text-faint text-xs">{shortDevice(session.sessionId)}</span>
          <span className="ml-auto flex flex-wrap items-center gap-1.5">
            <FreshnessChip freshness={session.freshness} />
            {isOwner && (
              <SessionArm
                intent="remove-session"
                sessionId={session.sessionId}
                label="Remove"
                tone="danger"
              />
            )}
          </span>
        </div>
        <div className="text-faint text-xs">
          {session.lastSeenAtMs === null ? (
            <>Has never reported — nothing has synced on it since it logged in.</>
          ) : (
            <>
              last seen {relativeTime(new Date(session.lastSeenAtMs))}
              {session.freshness === "stale" && (
                <> · past the {formatWindow(stalenessWindowMs)} window — chase by hand</>
              )}
            </>
          )}
        </div>
        <SkillStates skills={session.skills} />
      </div>
    </Card>
  );
}

function SkillStates({ skills }: { skills: SessionSkillState[] }) {
  if (skills.length === 0) {
    return <p className="text-faint text-xs">No skills reported for this session.</p>;
  }
  return (
    <ul className="space-y-1.5">
      {skills.map((skill) => (
        <li
          key={skill.skillId}
          className="flex flex-wrap items-center gap-x-2 gap-y-1 border-line-soft border-t pt-1.5 first:border-t-0 first:pt-0"
        >
          <span className="min-w-0 truncate text-ink text-sm">
            {skill.skillName ?? skill.skillId}
          </span>
          <SkillStatusChip status={skill.status} />
          <ShortId value={skill.appliedVersionId} />
          <span className="text-faint text-xs">
            reported {relativeTime(new Date(skill.reportedAtMs))}
          </span>
          {skill.status === "behind" && skill.currentVersionId !== null && (
            <span className="text-faint text-xs">
              current is <ShortId value={skill.currentVersionId} />
            </span>
          )}
        </li>
      ))}
    </ul>
  );
}

/**
 * A status chip that carries its own explainer as a tooltip — the reading legend rides the chips
 * themselves instead of a separate section. The trigger is a REAL focusable control (a button, so
 * it is keyboard-reachable) marked `cursor-help`; the tooltip opens on hover OR focus and never on
 * click, so the chip stays a passive, readable label.
 */
function StatusChip({
  tone,
  text,
  tip,
}: {
  tone: "neutral" | "verified" | "pending";
  text: string;
  tip: string;
}) {
  return (
    <Tooltip>
      <TooltipTrigger
        type="button"
        className="cursor-help rounded-full focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
      >
        <Chip tone={tone}>{text}</Chip>
      </TooltipTrigger>
      <TooltipContent>{tip}</TooltipContent>
    </Tooltip>
  );
}

function FreshnessChip({ freshness }: { freshness: SessionFreshness }) {
  if (freshness === "fresh") {
    return (
      <StatusChip
        tone="verified"
        text="fresh"
        tip="Reported within the staleness window — a recent sync confirmed this."
      />
    );
  }
  if (freshness === "stale") {
    return (
      <StatusChip
        tone="pending"
        text="stale"
        tip="No report within the staleness window. Sessions report when an agent runs, so an idle machine reads stale until its next run — unconfirmed, not wrong."
      />
    );
  }
  return (
    <StatusChip
      tone="neutral"
      text="never reported"
      tip="This session logged in but nothing has synced on it yet, so it has never reported."
    />
  );
}

function SkillStatusChip({ status }: { status: SessionSkillStatus }) {
  if (status === "current") {
    return (
      <StatusChip
        tone="verified"
        text="current"
        tip="This session's copy matches the workspace's current version."
      />
    );
  }
  return (
    <StatusChip
      tone="pending"
      text="behind"
      tip="This session is on an older version — its next update brings it current."
    />
  );
}

/** A group of one person's sessions, keyed by user id, preserving the query's order. */
function groupByOwner(sessions: WorkspaceSession[]): [string, WorkspaceSession[]][] {
  const groups = new Map<string, WorkspaceSession[]>();
  for (const session of sessions) {
    const list = groups.get(session.ownerUserId);
    if (list === undefined) {
      groups.set(session.ownerUserId, [session]);
    } else {
      list.push(session);
    }
  }
  return [...groups.entries()];
}

/** A calm "7 days" / "1 hour" window label for the stale note. */
function formatWindow(ms: number): string {
  const hours = Math.round(ms / 3_600_000);
  if (hours < 24) {
    return hours === 1 ? "1 hour" : `${hours} hour`;
  }
  const days = Math.round(ms / 86_400_000);
  return days === 1 ? "1 day" : `${days} day`;
}
