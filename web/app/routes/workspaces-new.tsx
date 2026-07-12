import { useState } from "react";
import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { Form, useActionData, useLoaderData, useNavigation } from "react-router";
import { CommandBlock } from "@/components/command-block";
import { buttonClasses, Card } from "@/components/ui";
import {
  actorFromSession,
  normalizeEmail,
  notFound,
  requireSession,
} from "@/lib/auth/guards.server";
import { vaultFetch } from "@/lib/plane/client.server";
import type { CreateWorkspaceBody, CreateWorkspaceOutcome } from "@/lib/plane/wire";

export function meta() {
  return [{ title: "Create a workspace · Topos" }];
}

/** A canonical UUID (what a page-render mints); anything else is a mangled form, not a call. */
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** The vault's static create-denial reason for the per-owner cap (rendered specially). */
const CAP_REASON = "workspace creation limit reached";

/**
 * Door 2: a signed-in human creates a workspace from the web, then pastes ONE command to their
 * agent. The `request_id` is minted CLIENT-side and held stable across retries (useState), so a
 * resubmit of the same form replays the same id — the vault answers with the same workspace
 * instead of minting a second one. Residual, accepted: a full reload mints a new id, so that
 * retry can create a second workspace (duplication is allowed by design, bounded by the cap).
 */
export async function loader({ request }: LoaderFunctionArgs) {
  const session = await requireSession(request);
  if (!session.user.emailVerified) {
    notFound();
  }
  const email = normalizeEmail(session.user.email);
  const localpart = email.split("@")[0] ?? email;
  return { email, defaultName: `${localpart}'s workspace` };
}

interface CreateResult {
  status: "created" | "limit" | "denied" | "error";
  /** The name echoed back so a non-success re-render keeps the user's edit. */
  submittedName?: string;
  /** `created` only: the fields the paste-to-your-agent block renders. */
  displayName?: string;
  address?: string;
  origin?: string;
  replayed?: boolean;
  /** The vault's static denial reason (`denied` only). */
  reason?: string;
}

export async function action({ request }: ActionFunctionArgs): Promise<CreateResult> {
  const formData = await request.formData();
  const requestId = String(formData.get("request_id") ?? "").trim();
  if (!UUID_RE.test(requestId)) {
    return { status: "error" };
  }
  const name = String(formData.get("display_name") ?? "").trim();
  const submittedName = name === "" ? undefined : name;

  // The verified/ASCII-gated actor is the web-tier authorization; its email rides the internal
  // lane's acting header, never a body field.
  const actor = actorFromSession(await requireSession(request));
  if (!actor) {
    return { status: "error", submittedName };
  }

  const body: CreateWorkspaceBody = {
    request_id: requestId,
    ...(submittedName === undefined ? {} : { display_name: submittedName }),
  };
  const response = await vaultFetch({
    method: "POST",
    template: "/internal/v1/workspaces",
    actingEmail: actor.email,
    body,
  });
  if (!response.ok) {
    return { status: "error", submittedName };
  }
  let outcome: CreateWorkspaceOutcome;
  try {
    outcome = (await response.json()) as CreateWorkspaceOutcome;
  } catch {
    return { status: "error", submittedName };
  }
  if (outcome.outcome === "created" || outcome.outcome === "replayed") {
    return {
      status: "created",
      displayName: submittedName,
      address: outcome.address,
      origin: new URL(request.url).origin,
      replayed: outcome.outcome === "replayed",
    };
  }
  if (outcome.outcome === "denied") {
    return outcome.reason === CAP_REASON
      ? { status: "limit", submittedName }
      : { status: "denied", reason: outcome.reason, submittedName };
  }
  return { status: "error", submittedName };
}

export default function WorkspacesNew() {
  const { email, defaultName } = useLoaderData<typeof loader>();
  const state = useActionData<typeof action>();
  const navigation = useNavigation();
  const isSubmitting = navigation.state === "submitting";
  // Minted ONCE per mount and stable across retries — a resubmit replays the same creation.
  const [requestId] = useState(() => crypto.randomUUID());

  return (
    <div className="mx-auto max-w-md space-y-4">
      <div>
        <h1 className="font-display font-semibold text-lg tracking-[-0.02em] text-ink">
          Create a workspace
        </h1>
        <p className="mt-1 text-sm text-dim">
          You become its owner ({email}). Your agent joins with one pasted command.
        </p>
      </div>
      <Card className="p-6">
        {state?.status === "created" ? (
          <CreatedPanel state={state} />
        ) : state?.status === "limit" ? (
          <LimitPanel />
        ) : (
          <Form method="post" className="flex flex-col gap-4">
            <input type="hidden" name="request_id" value={requestId} />
            <label className="block">
              <span className="mb-1 block font-medium text-dim text-sm">Workspace name</span>
              <input
                type="text"
                name="display_name"
                // React resets uncontrolled fields after a submit; the echoed submittedName
                // (keyed, so the node remounts) keeps the user's edit through a denial/error.
                key={state?.submittedName ?? "initial"}
                defaultValue={state?.submittedName ?? defaultName}
                maxLength={120}
                className="block h-11 w-full rounded-md border border-line px-3 text-ink text-sm focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25"
              />
            </label>
            <button
              type="submit"
              disabled={isSubmitting}
              className={`${buttonClasses("primary")} min-h-11 w-full`}
            >
              {isSubmitting ? "Creating…" : "Create workspace"}
            </button>
            {state?.status === "denied" && (
              <p className="text-red-600 text-sm" role="alert">
                The vault declined this creation{state.reason ? `: ${state.reason}` : ""}. Nothing
                was created.
              </p>
            )}
            {state?.status === "error" && (
              <p className="text-red-600 text-sm" role="alert">
                That didn&apos;t go through — nothing was created. Try again (a retry is safe: it
                resumes this same request).
              </p>
            )}
          </Form>
        )}
      </Card>
    </div>
  );
}

/** The success block: the one command that enrolls the creator's agent as this workspace's owner. */
function CreatedPanel({ state }: { state: CreateResult }) {
  const command = `topos follow ${state.origin ?? ""}/${state.address ?? ""}`;
  return (
    <div className="flex flex-col gap-4">
      <div>
        <h2 className="font-display font-semibold text-ink text-lg tracking-[-0.02em]">
          {state.displayName ? `${state.displayName} is ready` : "Your workspace is ready"}
        </h2>
        <p className="mt-1 text-dim text-sm">
          Paste this command to your agent and ask it to follow — it installs topos and joins as
          this workspace&apos;s owner.
        </p>
      </div>
      <CommandBlock command={command} copyLabel="Copy the command" />
      <p className="text-dim text-sm">
        This enrolls one device as you. Skills you publish afterwards reach the whole team
        automatically.
      </p>
    </div>
  );
}

/** The per-owner cap denial — nothing was created. */
function LimitPanel() {
  return (
    <div className="flex flex-col gap-2">
      <h2 className="font-display font-semibold text-ink text-lg tracking-[-0.02em]">
        Workspace creation limit reached
      </h2>
      <p className="text-dim text-sm">
        The vault declined: you&apos;ve reached your workspace limit. Nothing was created — use one
        of your existing workspaces, or contact us.
      </p>
    </div>
  );
}
