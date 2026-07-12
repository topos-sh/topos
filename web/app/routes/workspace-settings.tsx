import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, useLoaderData } from "react-router";
import { AddressBlock } from "@/components/members/address-block";
import { ReviewRequiredPanel } from "@/components/policy/review-required-panel";
import { buttonClasses, Card, PageHeader, SectionHeading } from "@/components/ui";
import { notFound, requireMember, requireWorkspaceOwner } from "@/lib/auth/guards.server";
import {
  lastPolicyEvent,
  type PolicyOutcome,
  planeWorkspaceById,
  recordPolicyEvent,
  setReviewDefault,
  workspacePolicyOf,
} from "@/lib/db/queries.server";
import { followBase } from "@/lib/plane/follow-base.server";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Settings · ${params.ws ?? "Workspace"}` }];
}

export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const actor = await requireMember(request, ws);
  // Management is a confirmed OWNER seat — the actor's role IS the directory's.
  const isOwner = actor.role === "owner";
  const [lastEvent, policy, workspace] = await Promise.all([
    lastPolicyEvent(actor, ws),
    workspacePolicyOf(actor),
    planeWorkspaceById(actor, ws),
  ]);
  return {
    ws,
    isOwner,
    lastEvent,
    address: workspace?.name ?? ws,
    origin: followBase(request),
    // The directory holds the real value now (a bigint 0/1 flag) — the switch reflects it.
    reviewRequired: policy ? policy.reviewRequired === 1 : false,
  };
}

/**
 * ONE action, dispatched on the hidden `intent`. Each branch RE-GUARDS itself (a loader gate
 * never extends to an action). Membership admin lives on its own page (/workspaces/:ws/members);
 * this page is the workspace's policy + address surface.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  if (intent === "set-review-required") {
    return reviewRequiredIntent(request, ws, formData);
  }
  return data({ intent: "unknown" as const, status: "error" as const }, { status: 400 });
}

/**
 * The review-required gate — owner-only. The LOCK is the database's: `topos_set_review_default`
 * re-runs the owner gate inside the function, so this web guard is defense-in-depth, never the
 * only check. Every attempt lands a `policy_event` row whatever the outcome, so the panel's
 * audit line stays honest; the loader revalidates and re-reads it after the action.
 */
async function reviewRequiredIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const value = String(formData.get("review_required") ?? "") === "true";
  let outcome: PolicyOutcome;
  try {
    const set = await setReviewDefault(owner, value);
    outcome = set === "set" ? "ok" : "denied";
  } catch {
    outcome = "error";
  }
  await recordPolicyEvent(owner, value, outcome);
  return { intent: "set-review-required" as const, status: outcome };
}

export default function WorkspaceSettings() {
  const { ws, isOwner, lastEvent, address, origin, reviewRequired } =
    useLoaderData<typeof loader>();
  return (
    <div className="space-y-8">
      <PageHeader
        title="Settings"
        meta={<code className="font-mono">{address}</code>}
        actions={
          <Link to={`/workspaces/${ws}`} className={buttonClasses("quiet")}>
            Back to workspace
          </Link>
        }
      />
      <MembersPointer ws={ws} />
      <AddressSection address={address} origin={origin} />
      {isOwner && (
        <ReviewRequiredPanel
          lastEvent={lastEvent}
          isOwner={isOwner}
          reviewRequired={reviewRequired}
        />
      )}
    </div>
  );
}

/** Membership admin lives on its own page — settings points at it rather than duplicating it. */
function MembersPointer({ ws }: { ws: string }) {
  return (
    <section aria-labelledby="members-pointer-heading" className="space-y-3">
      <SectionHeading>
        <span id="members-pointer-heading">Members</span>
      </SectionHeading>
      <Card className="flex flex-wrap items-center justify-between gap-3 px-4 py-3">
        <p className="text-dim text-sm">
          Invitations, roles, and removals live on the members page.
        </p>
        <Link to={`/workspaces/${ws}/members`} className={buttonClasses("quiet")}>
          Manage members
        </Link>
      </Card>
    </section>
  );
}

/**
 * The workspace address — its own pane section (replacing the old door link). Sharing and joining
 * speak this address: `topos follow <origin>/<address>`.
 */
function AddressSection({ address, origin }: { address: string; origin: string }) {
  return (
    <section aria-labelledby="address-heading" className="space-y-3">
      <SectionHeading>
        <span id="address-heading">Workspace address</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-dim text-sm">
          Hand this to a teammate or another of your own devices — following it joins the workspace.
        </p>
        <AddressBlock address={address} origin={origin} />
      </Card>
    </section>
  );
}
