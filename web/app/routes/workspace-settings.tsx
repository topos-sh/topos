import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, useLoaderData } from "react-router";
import { AddressBlock } from "@/components/members/address-block";
import type { LastSetLine } from "@/components/policy/last-set-line";
import { RegistrationPanel } from "@/components/policy/registration-panel";
import { ReviewRequiredPanel } from "@/components/policy/review-required-panel";
import { SessionApprovalPanel } from "@/components/policy/session-approval-panel";
import { SessionMaxAgePanel } from "@/components/policy/session-max-age-panel";
import { StalenessWindowPanel } from "@/components/policy/staleness-window-panel";
import { SettingsTabs } from "@/components/settings-tabs";
import { buttonClasses, Card, PageHeader, SectionHeading } from "@/components/ui";
import { composition } from "@/composition.server";
import { requireMemberInScope, requireWorkspaceOwner } from "@/lib/auth/guards.server";
import { type AuditEventRow, lastAuditEventOfKind, recordAdminEvent } from "@/lib/db/audit.server";
import {
  setRegistration,
  setSessionApproval,
  setSessionMaxAge,
  setStalenessWindow,
  workspacePolicyOf,
} from "@/lib/db/queries.policy.server";
import { setReviewDefault } from "@/lib/db/queries.server";
import { useWsPath } from "@/lib/ws-path";
import { workspaceAddress } from "@/lib/ws-url.server";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Settings · ${params.ws ?? "Workspace"}` }];
}

/** Shape one audit row into the panels' "last set by" line (null = never set from here). */
function lastSetOf(row: AuditEventRow | undefined): LastSetLine | null {
  if (row === undefined) {
    return null;
  }
  return { value: row.subject, by: row.actorDisplay, at: row.createdAt };
}

export async function loader({ request, params }: LoaderFunctionArgs) {
  const { workspace, actor } = await requireMemberInScope(request, params);
  // Management is a confirmed OWNER seat — the actor's role IS the seat table's.
  const isOwner = actor.role === "owner";
  // The registration knob governs sign-up only where the install IS the workspace (single
  // tenancy). In multi tenancy account creation is a server-global act the composition owns,
  // so the per-workspace knob governs nothing and its panel does not render.
  const registrationGoverns = composition.tenancy === "single";
  // The knobs are plain columns on the ONE workspace row; the column DEFAULTs are the canonical
  // fallbacks, so a fresh install shows the true defaults, never a blank. The "last set by"
  // lines read the audit ledger — the same rows the setters land in their own transactions.
  const [policy, lastReview, lastStaleness, lastRegistration, lastSessionApproval, lastMaxAge] =
    await Promise.all([
      workspacePolicyOf(actor),
      lastAuditEventOfKind(actor, "policy_review_default"),
      lastAuditEventOfKind(actor, "policy_staleness"),
      registrationGoverns
        ? lastAuditEventOfKind(actor, "policy_registration")
        : Promise.resolve(undefined),
      lastAuditEventOfKind(actor, "policy_session_approval"),
      lastAuditEventOfKind(actor, "policy_session_max_age"),
    ]);
  return {
    isOwner,
    registrationGoverns,
    slug: workspace.name,
    shareAddress: workspaceAddress(request, workspace.name),
    reviewRequired: policy.protectionDefault === "reviewed",
    stalenessWindowMs: policy.stalenessWindowMs,
    registration: policy.registration,
    sessionApproval: policy.sessionApproval,
    sessionMaxAgeMs: policy.sessionMaxAgeMs,
    lastSet: {
      review: lastSetOf(lastReview),
      staleness: lastSetOf(lastStaleness),
      registration: lastSetOf(lastRegistration),
      sessionApproval: lastSetOf(lastSessionApproval),
      sessionMaxAge: lastSetOf(lastMaxAge),
    },
  };
}

/**
 * ONE action, dispatched on the hidden `intent`. Each branch RE-GUARDS itself as an owner (a
 * loader gate never extends to an action), then runs the setter — a settings save is an ordinary
 * owner act, so the owner guard is the whole gate (no re-authentication). A bounds/vocabulary
 * refusal writes NOTHING and records the refused attempt under the knob's own audit kind (outcome
 * `denied` — the "last set by" lines read `ok` rows only, so refusals never pollute them); a
 * passing one writes, and the setter lands the `ok` audit row in its own transaction. Membership
 * admin lives on its own page (the members route).
 */
export async function action({ request, params }: ActionFunctionArgs) {
  // The membership FLOOR, hoisted above the intent dispatch: every intent below requires at
  // least a member (most re-check owner/reviewer themselves), and the unmatched-intent 400 must
  // never answer a non-member — in multi tenancy `:ws` is a guessable public name slug, so a
  // 400-vs-404 split would be a workspace-existence oracle the GET faces deliberately close.
  const { workspace } = await requireMemberInScope(request, params);
  const ws = workspace.id;
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  if (intent === "set-review-required") {
    return reviewRequiredIntent(request, ws, formData);
  }
  if (intent === "set-staleness-window") {
    return stalenessWindowIntent(request, ws, formData);
  }
  // In multi tenancy the per-workspace registration knob governs nothing, so the intent does
  // not exist there: it answers exactly what any unknown intent answers.
  if (intent === "set-registration" && composition.tenancy === "single") {
    return registrationIntent(request, ws, formData);
  }
  if (intent === "set-session-approval") {
    return sessionApprovalIntent(request, ws, formData);
  }
  if (intent === "set-session-max-age") {
    return sessionMaxAgeIntent(request, ws, formData);
  }
  return data({ intent: "unknown" as const, status: "error" as const }, { status: 400 });
}

/** The copy a transient server fault surfaces. */
const SERVER_ERROR = "The server couldn't be reached. Try again.";

type KnobStatus = "ok" | "denied" | "error";

/**
 * The shared knob frame: owner guard → the setter. A bounds/vocabulary refusal ("denied") records
 * under the knob's audit kind and returns its typed error — the setter never saw it or refused it
 * without writing, so the route is the only place that attempt can land in the trail.
 */
async function knobIntent<Outcome extends string>(
  request: Request,
  ws: string,
  args: {
    auditKind: string;
    detail: string;
    run: (owner: Awaited<ReturnType<typeof requireWorkspaceOwner>>) => Promise<Outcome>;
    deniedError: (outcome: Outcome) => string;
  },
): Promise<{ status: KnobStatus; error?: string }> {
  const owner = await requireWorkspaceOwner(request, ws);
  let outcome: Outcome;
  try {
    outcome = await args.run(owner);
  } catch {
    await recordAdminEvent(owner, {
      kind: args.auditKind,
      subject: ws,
      detail: args.detail,
      outcome: "error",
    });
    return { status: "error", error: SERVER_ERROR };
  }
  if (outcome === "set") {
    return { status: "ok" };
  }
  await recordAdminEvent(owner, {
    kind: args.auditKind,
    subject: ws,
    detail: args.detail,
    outcome: "denied",
  });
  return { status: "denied", error: args.deniedError(outcome) };
}

/** The review-required gate — the workspace's protection DEFAULT, as one switch. */
async function reviewRequiredIntent(request: Request, ws: string, formData: FormData) {
  const value = String(formData.get("review_required") ?? "") === "true";
  const result = await knobIntent(request, ws, {
    auditKind: "policy_review_default",
    detail: value ? "reviewed" : "open",
    run: (owner) => setReviewDefault(owner, value),
    deniedError: () => SERVER_ERROR,
  });
  return { intent: "set-review-required" as const, ...result };
}

/** The staleness window — entered in days, converted to milliseconds at hour granularity. */
async function stalenessWindowIntent(request: Request, ws: string, formData: FormData) {
  const days = Number(formData.get("staleness_days") ?? "");
  // Round to the nearest hour, then to milliseconds. A NaN input (empty/garbage) becomes 0,
  // which the setter refuses as bad_window (honest, not a crash).
  const windowMs = Number.isFinite(days) ? Math.round(days * 24) * 3_600_000 : 0;
  const result = await knobIntent(request, ws, {
    auditKind: "policy_staleness",
    detail: String(windowMs),
    run: (owner) => setStalenessWindow(owner, windowMs),
    deniedError: () => "Enter a window between 1 hour and 366 days.",
  });
  return { intent: "set-staleness-window" as const, ...result };
}

/** The registration knob — `open` disables the invitation proof; default invite_only. */
async function registrationIntent(request: Request, ws: string, formData: FormData) {
  const value = String(formData.get("registration") ?? "");
  const result = await knobIntent(request, ws, {
    auditKind: "policy_registration",
    detail: value,
    run: (owner) => setRegistration(owner, value),
    deniedError: () => "Choose invite-only or open.",
  });
  return { intent: "set-registration" as const, ...result };
}

/**
 * The session expiry — entered in days (empty = no expiry), converted to milliseconds at hour
 * granularity like the staleness window. Enforcement is the session guard's, so a landed change
 * takes effect on the very next lane request.
 */
async function sessionMaxAgeIntent(request: Request, ws: string, formData: FormData) {
  const raw = String(formData.get("session_max_age_days") ?? "").trim();
  const days = Number(raw);
  // Empty clears the policy (no expiry); a NaN/zero input becomes 0, which the setter refuses
  // as bad_value (honest, not a crash).
  const maxAgeMs =
    raw === "" ? null : Number.isFinite(days) ? Math.round(days * 24) * 3_600_000 : 0;
  const result = await knobIntent(request, ws, {
    auditKind: "policy_session_max_age",
    detail: maxAgeMs === null ? "off" : String(maxAgeMs),
    run: (owner) => setSessionMaxAge(owner, maxAgeMs),
    deniedError: () => "Enter an expiry between 1 hour and 366 days, or leave empty for none.",
  });
  return { intent: "set-session-max-age" as const, ...result };
}

/** The session-approval knob — `on` bears non-owner logins pending; default off. */
async function sessionApprovalIntent(request: Request, ws: string, formData: FormData) {
  const value = String(formData.get("session_approval") ?? "");
  const result = await knobIntent(request, ws, {
    auditKind: "policy_session_approval",
    detail: value,
    run: (owner) => setSessionApproval(owner, value),
    deniedError: () => "Choose off or required.",
  });
  return { intent: "set-session-approval" as const, ...result };
}

export default function WorkspaceSettings() {
  const {
    isOwner,
    registrationGoverns,
    slug,
    shareAddress,
    reviewRequired,
    stalenessWindowMs,
    registration,
    sessionApproval,
    sessionMaxAgeMs,
    lastSet,
  } = useLoaderData<typeof loader>();
  const wsPath = useWsPath();
  return (
    <div className="space-y-8">
      <PageHeader
        title="Settings"
        meta={<code className="font-mono">{slug}</code>}
        actions={
          <Link to={wsPath("")} className={buttonClasses("quiet")}>
            Back to workspace
          </Link>
        }
      />
      <SettingsTabs active="general" />
      <MembersPointer />
      <AddressSection address={shareAddress} />
      {isOwner && <ExportSection />}
      <ReviewRequiredPanel
        isOwner={isOwner}
        reviewRequired={reviewRequired}
        lastSet={lastSet.review}
      />
      <StalenessWindowPanel
        isOwner={isOwner}
        stalenessWindowMs={stalenessWindowMs}
        lastSet={lastSet.staleness}
      />
      <SessionApprovalPanel
        isOwner={isOwner}
        sessionApproval={sessionApproval}
        lastSet={lastSet.sessionApproval}
      />
      <SessionMaxAgePanel
        isOwner={isOwner}
        sessionMaxAgeMs={sessionMaxAgeMs}
        lastSet={lastSet.sessionMaxAge}
      />
      {registrationGoverns && (
        <RegistrationPanel
          isOwner={isOwner}
          registration={registration}
          lastSet={lastSet.registration}
        />
      )}
    </div>
  );
}

/** Membership admin lives on its own page — settings points at it rather than duplicating it. */
function MembersPointer() {
  const wsPath = useWsPath();
  return (
    <section aria-labelledby="members-pointer-heading" className="space-y-3">
      <SectionHeading>
        <span id="members-pointer-heading">Members</span>
      </SectionHeading>
      <Card className="flex flex-wrap items-center justify-between gap-3 px-4 py-3">
        <p className="text-dim text-sm">
          Invitations, roles, and removals live on the members page.
        </p>
        <Link to={wsPath("members")} className={buttonClasses("quiet")}>
          Manage members
        </Link>
      </Card>
    </section>
  );
}

/**
 * Export — an owner-only action that downloads the WHOLE catalog as one zip (every skill at its
 * current version, one directory each, plus a manifest). A native anchor (not a client `Link`),
 * so the browser makes a document GET the resource-route loader answers with the stream.
 */
function ExportSection() {
  const wsPath = useWsPath();
  return (
    <section aria-labelledby="export-heading" className="space-y-3">
      <SectionHeading>
        <span id="export-heading">Export</span>
      </SectionHeading>
      <Card className="flex flex-wrap items-center justify-between gap-3 px-4 py-3">
        <p className="text-dim text-sm">
          Download every skill in this workspace at its current version, plus a manifest, as a
          single zip archive.
        </p>
        <a href={wsPath("settings/export")} download className={buttonClasses("quiet")}>
          Export skills
        </a>
      </Card>
    </section>
  );
}

/**
 * The workspace address — its own pane section. Sharing and joining speak this address:
 * `topos login <address>`.
 */
function AddressSection({ address }: { address: string }) {
  return (
    <section aria-labelledby="address-heading" className="space-y-3">
      <SectionHeading>
        <span id="address-heading">Workspace address</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-dim text-sm">
          Hand this to a teammate or another of your own machines — logging in joins the workspace.
        </p>
        <AddressBlock address={address} />
      </Card>
    </section>
  );
}
