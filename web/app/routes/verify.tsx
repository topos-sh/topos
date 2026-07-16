import type { ReactNode } from "react";
import {
  type ActionFunctionArgs,
  data,
  Form,
  type LoaderFunctionArgs,
  type MetaFunction,
  redirect,
  useActionData,
  useLoaderData,
  useNavigation,
} from "react-router";
import { buttonClasses } from "@/components/ui";
import { actorFromSession, notFound, requireSession } from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import {
  approveDeviceAuth,
  denyDeviceAuth,
  pendingDeviceAuth,
  theWorkspace,
} from "@/lib/db/identity.server";

export const meta: MetaFunction = () => [{ title: "Approve a device · Topos" }];

/**
 * The ONE device-approve ceremony (the gh-style device flow's browser half). A device that
 * wants to act as you shows a short code and points here; a SIGNED-IN person types (or arrives
 * with) that code, sees what is asking, and approves or denies. A LIVE SESSION plus the explicit
 * approve click IS the whole ceremony — approval mints the device's bearer credential (a
 * credential that acts as you), and being signed in and choosing Approve is the proof of who is
 * acting. Denying destroys a pending request and mints nothing.
 *
 * Signed out, the page bounces to /login carrying itself (code included) as the `next` path —
 * which a password OR a magic-link sign-in both honor, returning here to finish the approval.
 * An unknown or expired code is an honest in-page state, never a 404 — the person may have
 * mistyped and needs the form back.
 */

/**
 * The loader's own sign-in bounce (not requireSession, whose bounce drops the query): the
 * `next` path carries the code, so a person landing here from a mailed/printed link signs in
 * and returns to the resolved request.
 */
export async function loader({ request }: LoaderFunctionArgs) {
  const code = (new URL(request.url).searchParams.get("code") ?? "").trim();
  const actor = actorFromSession(await getAuth().api.getSession({ headers: request.headers }));
  if (actor === null) {
    const next = `/verify${code === "" ? "" : `?code=${encodeURIComponent(code)}`}`;
    throw redirect(`/login?next=${encodeURIComponent(next)}`);
  }
  const pending = code === "" ? null : await pendingDeviceAuth(code);
  return { code, pending };
}

const REQUEST_GONE =
  "That request expired or was already handled — nothing was approved. Ask the device to start again.";

export async function action({ request }: ActionFunctionArgs) {
  // A POST has no query to preserve — the plain guard's /login bounce is right here.
  const actor = actorFromSession(await requireSession(request));
  if (actor === null) {
    notFound();
  }
  const form = await request.formData();
  const intent = String(form.get("intent") ?? "");
  const userCode = String(form.get("code") ?? "").trim();
  if (userCode === "" || (intent !== "approve" && intent !== "deny")) {
    throw data(null, { status: 400 });
  }
  // The single-tenant resolve: the one workspace scopes the ceremony's audit row.
  const workspace = await theWorkspace();
  if (workspace === null) {
    notFound();
  }

  if (intent === "approve") {
    // No step-up: the live session + this explicit approve click is the whole ceremony.
    const approved = await approveDeviceAuth(
      userCode,
      { userId: actor.userId, display: actor.display },
      workspace.id,
    );
    if (approved === null) {
      return data({ kind: "refused" as const, error: REQUEST_GONE }, { status: 400 });
    }
    return { kind: "approved" as const, name: approved.requestedName };
  }

  const denied = await denyDeviceAuth(
    userCode,
    { userId: actor.userId, display: actor.display },
    workspace.id,
  );
  if (!denied) {
    return data({ kind: "refused" as const, error: REQUEST_GONE }, { status: 400 });
  }
  return { kind: "denied" as const };
}

const INPUT =
  "block h-11 w-full rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";

export default function VerifyPage() {
  const { code, pending } = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();

  if (actionData?.kind === "approved") {
    return (
      <Shell>
        <PlainState heading="Device connected">
          {actionData.name} is connected — you can close this tab; the device picks the approval up
          on its next poll.
        </PlainState>
      </Shell>
    );
  }
  if (actionData?.kind === "denied") {
    return (
      <Shell>
        <PlainState heading="Request denied">
          Nothing was connected — the device is told on its next poll.
        </PlainState>
      </Shell>
    );
  }

  return (
    <Shell>
      <div className="flex flex-col gap-6">
        <div className="flex flex-col gap-2 text-center">
          <p className="font-medium text-faint text-xs uppercase tracking-wide">Device approval</p>
          <h1 className="font-display font-semibold text-ink text-lg tracking-[-0.02em]">
            Approve a device
          </h1>
          <p className="text-dim text-sm">
            Enter the code your device shows — it looks like{" "}
            <code className="font-mono text-ink">AB29-CD34</code>.
          </p>
        </div>
        {actionData?.kind === "refused" && (
          <p className="text-center text-red-600 text-sm" role="alert">
            {actionData.error}
          </p>
        )}
        <CodeLookup code={code} />
        {pending !== null ? (
          <PendingRequest code={code} requestedName={pending.requestedName} />
        ) : (
          code !== "" && (
            <p className="text-center text-dim text-sm" role="status">
              No pending request for this code — it may have expired, or a character is off. Check
              your terminal and try again.
            </p>
          )
        )}
      </div>
    </Shell>
  );
}

/** The code form, a GET: submitting re-resolves the pending request server-side. */
function CodeLookup({ code }: { code: string }) {
  return (
    <Form method="get" className="flex items-end gap-2">
      <label className="block flex-1">
        <span className="mb-1 block font-medium text-dim text-sm">Code</span>
        <input
          type="text"
          name="code"
          defaultValue={code}
          required
          autoComplete="off"
          spellCheck={false}
          className={`${INPUT} font-mono uppercase`}
          placeholder="AB29-CD34"
        />
      </label>
      <button type="submit" className={`${buttonClasses("quiet")} min-h-11`}>
        Look up
      </button>
    </Form>
  );
}

/**
 * The resolved request: what is asking, then the two arms. The approve form posts the RESOLVED
 * code as a hidden field — the approval applies to exactly the request shown, never to whatever
 * the lookup input holds at submit time.
 */
function PendingRequest({ code, requestedName }: { code: string; requestedName: string }) {
  const navigation = useNavigation();
  const submitting = navigation.state !== "idle";
  return (
    <div className="flex flex-col gap-4 rounded-md border border-line-soft bg-ground p-4">
      <p className="text-ink text-sm">
        Device <span className="font-medium">“{requestedName}”</span> wants to act as you. Approving
        gives it a credential that publishes, follows, and reads with your seat until you sign it
        out from your devices.
      </p>
      <Form method="post" className="flex flex-col gap-3">
        <input type="hidden" name="intent" value="approve" />
        <input type="hidden" name="code" value={code} />
        <button
          type="submit"
          disabled={submitting}
          className={`${buttonClasses("primary")} min-h-11 w-full`}
        >
          {submitting ? "Working…" : `Approve “${requestedName}”`}
        </button>
      </Form>
      <Form method="post">
        <input type="hidden" name="intent" value="deny" />
        <input type="hidden" name="code" value={code} />
        <button
          type="submit"
          disabled={submitting}
          className={`${buttonClasses("danger")} min-h-11 w-full`}
        >
          Deny — this isn’t my device
        </button>
      </Form>
    </div>
  );
}

function PlainState({ heading, children }: { heading: string; children: ReactNode }) {
  return (
    <div className="flex flex-col items-center gap-2 text-center">
      <p className="font-medium text-faint text-xs uppercase tracking-wide">Device approval</p>
      <h1 className="font-display font-semibold text-ink text-lg tracking-[-0.02em]">{heading}</h1>
      <p className="text-dim text-sm">{children}</p>
    </div>
  );
}

function Shell({ children }: { children: ReactNode }) {
  return (
    <main className="mx-auto flex min-h-dvh w-full max-w-md flex-col justify-center px-4 py-10">
      <div className="rounded-lg border border-line-soft bg-panel p-6 shadow-sm sm:p-8">
        {children}
      </div>
    </main>
  );
}
