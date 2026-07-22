import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, redirect, useActionData } from "react-router";
import { buttonClasses, Card, PageHeader, SectionHeading } from "@/components/ui";
import { requireMemberInScope } from "@/lib/auth/guards.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import { type ChannelCreateOutcome, createChannel } from "@/lib/db/queries.channels.server";
import { useWsPath } from "@/lib/ws-path";
import { wsPathServer } from "@/lib/ws-url.server";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `New channel · ${params.ws ?? "Workspace"}` }];
}

/**
 * The channel CREATE form as its own Rails-style route (`channels/new`) — the index keeps the
 * list. Member-level and deliberately unconfirmed (creating an empty group destroys nothing); the
 * same grade as the device lane's create-on-first-use, so the browser and the CLI agree on who may
 * mint a group. The loader just guards; the form lives below.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  await requireMemberInScope(request, params);
  return null;
}

/** The create form's typed reply on a NON-redirect (a landed create redirects to the channel). */
interface CreateChannelActionData {
  error: string;
  submittedName?: string;
}

/**
 * CREATE — the one action here. The unique index is the race arbiter, so a create-race loser gets
 * the honest name-taken copy, never a 500. The create lands its audit row in the DAL's own
 * transaction; the route records only the refusals the DAL typed back.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const { workspace, actor } = await requireMemberInScope(request, params);
  const formData = await request.formData();
  const name = String(formData.get("name") ?? "").trim();
  let outcome: ChannelCreateOutcome;
  try {
    outcome = await createChannel(actor, name);
  } catch {
    await recordAdminEvent(actor, { kind: "channel_created", subject: name, outcome: "error" });
    return data<CreateChannelActionData>(
      { error: "That didn't go through. Try again.", submittedName: name },
      { status: 500 },
    );
  }
  if (outcome.outcome === "created") {
    throw redirect(wsPathServer(workspace.name, `channels/${name}`));
  }
  await recordAdminEvent(actor, {
    kind: "channel_created",
    subject: name,
    detail: outcome.outcome,
    outcome: "denied",
  });
  return data<CreateChannelActionData>(
    {
      error:
        outcome.outcome === "name_taken"
          ? `A channel named #${name} already exists.`
          : "Channel names use lowercase letters, numbers, and hyphens (up to 64 characters).",
      submittedName: name,
    },
    { status: 400 },
  );
}

export default function ChannelNew() {
  const state = useActionData<typeof action>();
  const wsPath = useWsPath();
  return (
    <div className="space-y-8">
      <PageHeader
        title="New channel"
        actions={
          <Link to={wsPath("channels")} className={buttonClasses("quiet")}>
            All channels
          </Link>
        }
      />
      <section aria-labelledby="create-channel-heading" className="space-y-3">
        <SectionHeading>
          <span id="create-channel-heading">Create a channel</span>
        </SectionHeading>
        <Card className="space-y-3 px-4 py-3">
          <p className="text-dim text-sm">
            A channel is a named group: skills placed in it are delivered to its members. Any member
            can create one — the CLI creates them on first use the same way.
          </p>
          {/* A full-page POST: a landed create redirects to the new channel; a refusal re-renders
              here with the typed copy. */}
          <form method="post" className="flex flex-wrap items-end gap-2">
            <label className="block flex-1">
              <span className="mb-1 block font-medium text-sm text-dim">Channel name</span>
              <input
                type="text"
                name="name"
                required
                autoComplete="off"
                spellCheck={false}
                placeholder="frontend-guild"
                pattern="[a-z0-9][a-z0-9-]*"
                maxLength={64}
                key={state?.submittedName ?? "initial"}
                defaultValue={state?.submittedName ?? ""}
                className="block h-11 w-full min-w-56 rounded-md border border-line px-3 text-ink text-sm placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25"
              />
            </label>
            <button type="submit" className={`${buttonClasses("quiet")} min-h-11`}>
              Create channel
            </button>
          </form>
          {state !== undefined && state.error.length > 0 && (
            <p className="text-red-600 text-sm" role="alert">
              {state.error}
            </p>
          )}
        </Card>
      </section>
    </div>
  );
}
