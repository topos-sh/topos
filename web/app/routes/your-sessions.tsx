import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Form, useActionData, useLoaderData, useNavigation } from "react-router";
import { relativeTime } from "@/components/format";
import { buttonClasses, Card, Chip, PageHeader } from "@/components/ui";
import { actorFromSession, notFound, requireSession } from "@/lib/auth/guards.server";
import { revokeOwnSession } from "@/lib/db/identity.server";
import { type AccountSession, sessionsFor } from "@/lib/db/queries.sessions.server";

export function meta() {
  return [{ title: "Your sessions" }];
}

/**
 * The account-level session list — every session the signed-in person holds, across
 * workspaces (a session is user × workspace × installation; `topos login` mints one per
 * workspace per machine). The page needs only a browser-session-minted UserActor: the read
 * is self-scoped by construction.
 */
export async function loader({ request }: LoaderFunctionArgs) {
  const actor = actorFromSession(await requireSession(request));
  if (!actor) {
    notFound();
  }
  return { sessions: await sessionsFor(actor) };
}

/**
 * ONE self-service act: end one of your own sessions (the GitHub-sessions pattern) — no
 * confirmation beyond the button (the person's own escape hatch); the row is deleted, its
 * reported state cascades away, and the machine's credential stops resolving. Self-only by
 * the DAL's WHERE clause (a foreign id answers as an unknown one). Bytes already on the
 * machine stay there.
 */
export async function action({ request }: ActionFunctionArgs) {
  const actor = actorFromSession(await requireSession(request));
  if (!actor) {
    notFound();
  }
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  const sessionId = String(formData.get("session_id") ?? "");
  if (intent !== "end-session") {
    return data({ status: "error" as const }, { status: 400 });
  }
  let status: "revoked" | "unknown_session" | "error";
  try {
    status = await revokeOwnSession(actor, sessionId);
  } catch {
    status = "error";
  }
  return data({ status });
}

/** The human copy for each non-success outcome (the self-service act normally just succeeds). */
const ACTION_ERROR: Record<string, string> = {
  unknown_session: "That session is already gone — nothing to end.",
  error: "The server could not complete that. Try again.",
};

export default function YourSessions() {
  const { sessions } = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();
  const error =
    actionData && actionData.status !== "revoked" ? ACTION_ERROR[actionData.status] : undefined;
  const notice =
    actionData?.status === "revoked"
      ? "Session ended. Future delivery and reporting stop — the copies already on the machine stay put."
      : undefined;
  return (
    <div className="space-y-8">
      <PageHeader title="Your sessions" meta="Everywhere you are logged in with topos." />
      <p className="text-dim text-sm leading-relaxed">
        A session is one machine logged into one workspace. Ending a session stops delivery and
        reporting for exactly that workspace on that machine — the copies already there stay;
        nothing is deleted from the machine. On the machine itself,{" "}
        <code className="rounded bg-panel2 px-1.5 py-0.5 font-mono text-[13px]">topos logout</code>{" "}
        does the same thing.
      </p>
      {error !== undefined && (
        <p role="alert" className="text-red-700 text-sm">
          {error}
        </p>
      )}
      {notice !== undefined && (
        <p role="status" className="text-dim text-sm">
          {notice}
        </p>
      )}
      {sessions.length === 0 ? (
        <NoSessions />
      ) : (
        <Card>
          <ul className="divide-y divide-line-soft">
            {sessions.map((session) => (
              <SessionRow key={session.sessionId} session={session} />
            ))}
          </ul>
        </Card>
      )}
    </div>
  );
}

/**
 * Signed in, but no session anywhere yet. Honest and instructive: logging in is the
 * agent's `topos login` move.
 */
function NoSessions() {
  return (
    <div className="rounded-lg border border-line-soft border-dashed bg-panel px-6 py-12 text-center">
      <h2 className="font-display font-semibold text-base text-ink tracking-[-0.02em]">
        No sessions
      </h2>
      <p className="mx-auto mt-2 max-w-md text-dim text-sm leading-relaxed">
        Log a machine in from your agent — run{" "}
        <code className="rounded bg-panel2 px-1.5 py-0.5 font-mono text-[13px]">
          topos login &lt;workspace address&gt;
        </code>{" "}
        on it and the session appears here.
      </p>
    </div>
  );
}

/**
 * One session row: the installation's name, the workspace it reaches, its status, the
 * created + last-seen line, and the self-service "End session" button.
 */
function SessionRow({ session }: { session: AccountSession }) {
  const navigation = useNavigation();
  const submitting =
    navigation.state !== "idle" &&
    navigation.formData?.get("intent") === "end-session" &&
    navigation.formData?.get("session_id") === session.sessionId;
  const seen =
    session.lastSeenAtMs === null
      ? "never seen"
      : `last seen ${relativeTime(new Date(session.lastSeenAtMs))}`;
  return (
    <li className="space-y-2 px-4 py-3">
      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
        <div className="min-w-0 space-y-1">
          <p className="text-ink text-sm">
            {session.displayName}
            <span className="text-dim"> · {session.workspaceDisplayName}</span>{" "}
            <span className="font-mono text-faint text-xs">{session.workspaceName}</span>
          </p>
          <code className="block break-all font-mono text-faint text-xs">{session.sessionId}</code>
          <p className="text-faint text-xs">
            logged in {relativeTime(new Date(session.createdAtMs))} · {seen}
            {session.expired && (
              <> · past the workspace's session expiry — log in again on that machine</>
            )}
          </p>
        </div>
        <div className="flex items-center gap-2">
          {session.expired ? (
            <Chip tone="pending">expired</Chip>
          ) : session.status === "pending" ? (
            <Chip tone="pending">awaiting owner approval</Chip>
          ) : (
            <Chip tone="verified">active</Chip>
          )}
          <Form method="post">
            <input type="hidden" name="intent" value="end-session" />
            <input type="hidden" name="session_id" value={session.sessionId} />
            <button type="submit" className={buttonClasses("danger")} disabled={submitting}>
              {submitting ? "Ending…" : "End session"}
            </button>
          </Form>
        </div>
      </div>
    </li>
  );
}
