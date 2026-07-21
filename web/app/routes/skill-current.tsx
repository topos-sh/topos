import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, redirect, useLoaderData } from "react-router";
import { VersionFiles } from "@/components/browse/version-files";
import { SkillHeader } from "@/components/skill/skill-header";
import { SkillInviteAffordance } from "@/components/skill/skill-invite";
import { SkillTabs } from "@/components/skill/skill-tabs";
import { Card } from "@/components/ui";
import {
  actorFromSession,
  type MemberActor,
  memberInScope,
  notFound,
  requireMemberInScope,
  type ScopedWorkspace,
} from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { loadVersionFilesData } from "@/lib/browse/version-files.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import { workspacePolicyOf } from "@/lib/db/queries.policy.server";
import { createInvitations, foldInviteEmail } from "@/lib/db/queries.roster.server";
import { skillIndexRow } from "@/lib/db/queries.server";
import { resolveSkillName } from "@/lib/db/resolve.server";
import { sendInviteEmail } from "@/lib/mail/invite-mail.server";
import { mailDelivery } from "@/lib/mail/transport.server";
import { useWsPath } from "@/lib/ws-path";
import { agentDocUrl, inviteUrl, wsPathServer } from "@/lib/ws-url.server";

export function meta({ params }: { params: { skill?: string } }) {
  return [{ title: `${params.skill ?? "skill"} · Topos` }];
}

/**
 * The skill FACE — resource address and canonical Current tab as ONE route. A skill page is
 * MEMBERS-ONLY: an anonymous browser gets the house 404, indistinguishable from a mistyped path, so
 * nothing about a skill (not even that the address shape names one) leaks to a signed-out visitor.
 * (A non-browser document fetch still got the constant protocol card from the server entry — that
 * machine face is existence-blind and teaches `topos follow` regardless.) A signed-in member gets
 * the skill page WITH chrome; a signed-in non-member (or unknown workspace slug) gets the same 404.
 *
 * The Current tab is the DEFAULT skill view: the current version's files + doc preview inline.
 * Proposals and History are sibling MEMBER-only routes (see SkillTabs). The catalog row this page
 * probes IS the directory's identity surface: the NAME exists the moment a skill is minted, and the
 * `current` pointer joins in when a publish has landed one. A known name that has NEVER published
 * (`versionId` null) renders honestly; an unknown NAME is the uniform 404 (a rename hint redirects).
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const session = await getAuth().api.getSession({ headers: request.headers });
  const actor = actorFromSession(session);
  if (actor === null) {
    // Signed out: the skill face is not a public teaser — it is the uniform house 404, so an
    // anonymous probe cannot tell a real skill from a nonexistent one (or from any other path).
    notFound();
  }
  const { workspace, actor: memberActor } = await memberInScope(actor, params);
  const skill = params.skill as string;
  const row = await skillIndexRow(memberActor, skill);
  if (row === undefined) {
    // A rename left an old name behind: follow the resolving hint to the live name; else 404.
    const resolved = await resolveSkillName(memberActor, skill);
    if (resolved !== undefined && resolved.via === "hint" && resolved.status === "active") {
      throw redirect(wsPathServer(workspace.name, `skills/${resolved.name}`));
    }
    notFound();
  }

  const [policy, versionFiles] = await Promise.all([
    workspacePolicyOf(memberActor),
    row.versionId !== null
      ? loadVersionFilesData(memberActor, row.skillId, row.versionId)
      : Promise.resolve(null),
  ]);

  return {
    face: "page" as const,
    wsName: workspace.name,
    skill,
    currentShort: row.versionId !== null ? row.versionId.slice(0, 12) : "—",
    displayName: row.displayName,
    kind: row.kind,
    openProposals: row.openProposals,
    versionId: row.versionId,
    versionFiles,
    // The invite affordance's gates, resolved once here and never re-read client-side: armed mail
    // is the invitation's identity rung, and the invite-policy decides whether a plain member may
    // invite at all — the same two facts the members page surfaces.
    mailArmed: mailDelivery().canSend,
    invitePolicy: policy.invitePolicy,
    isOwner: memberActor.role === "owner",
  };
}

/**
 * The skill face's action — ONE intent today, `invite`: minting an invitation whose FIRST
 * destination is THIS skill (the affordance below). It RE-GUARDS from scratch — a loader's gate
 * never carries into an action — with the member scope the face itself requires (an anonymous
 * request bounces to the constant /login before any skill read; a non-member and an unknown slug
 * land the same uniform 404). Member scope is the FLOOR; the invite branch re-reads the
 * invite-policy against the actor's role itself. An unmatched intent is a 400 that only a member
 * can ever reach.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const { workspace, actor } = await requireMemberInScope(request, params);
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  if (intent === "invite") {
    return inviteToSkillIntent(request, workspace, actor, params.skill as string, formData);
  }
  return data({ intent: "unknown" as const, status: "error" as const }, { status: 400 });
}

/**
 * Invite a teammate to THIS skill. Inviting is a member op the invite-policy gates (createInvitations
 * runs that gate against the actor's role) and it REQUIRES armed mail — the invitation's identity
 * proof is a mailbox round-trip, so an unarmed deployment refuses honestly instead of seating a
 * claim nobody can prove. The skill's catalog row supplies the invitation's first-destination hint
 * (the bundle id stored on the row) AND the display facts the mail's subject/opening line lead with;
 * an unknown skill name is the same uniform 404 the face throws. A send fault never loses the
 * invitation — the row stands and re-inviting mints a fresh link — but the reply says so honestly.
 */
async function inviteToSkillIntent(
  request: Request,
  workspace: ScopedWorkspace,
  actor: MemberActor,
  skillName: string,
  formData: FormData,
) {
  if (!mailDelivery().canSend) {
    return { intent: "invite" as const, status: "mail_unarmed" as const };
  }
  const row = await skillIndexRow(actor, skillName);
  if (row === undefined) {
    notFound();
  }
  const raw = String(formData.get("email") ?? "");
  const folded = foldInviteEmail(raw);
  if (folded === null || !folded.includes("@")) {
    return { intent: "invite" as const, status: "error" as const, submittedEmail: raw };
  }

  const policy = await workspacePolicyOf(actor);
  const outcome = await createInvitations(actor, [folded], policy.invitePolicy, {
    bundleId: row.skillId,
  });
  if (outcome.outcome === "owner_role_required") {
    await recordAdminEvent(actor, {
      kind: "invitation_created",
      subject: folded,
      detail: "owner_role_required",
      outcome: "denied",
    });
    return { intent: "invite" as const, status: "owner_required" as const, submittedEmail: raw };
  }
  if (outcome.outcome !== "invited") {
    return { intent: "invite" as const, status: "error" as const, submittedEmail: raw };
  }

  let emailSent = true;
  try {
    for (const one of outcome.minted) {
      await sendInviteEmail({
        to: one.email,
        inviteUrl: inviteUrl(request, workspace.name, one.token),
        agentUrl: agentDocUrl(request),
        workspaceDisplayName: workspace.displayName,
        invitedBy: actor.display,
        hint: { kind: row.kind, name: row.name },
      });
    }
  } catch {
    emailSent = false;
  }
  return { intent: "invite" as const, status: "invited" as const, invited: folded, emailSent };
}

export default function SkillCurrentPage() {
  const data = useLoaderData<typeof loader>();
  return <SkillCurrentContent {...data} />;
}

function SkillCurrentContent({
  wsName,
  skill,
  currentShort,
  displayName,
  kind,
  openProposals,
  versionId,
  versionFiles,
  mailArmed,
  invitePolicy,
  isOwner,
}: Extract<Awaited<ReturnType<typeof loader>>, { face: "page" }>) {
  const wsPath = useWsPath();
  return (
    <div className="space-y-6">
      <SkillHeader
        ws={wsName}
        skill={skill}
        currentShort={currentShort}
        displayName={displayName}
        kind={kind}
      />
      <SkillTabs
        basePath={wsPath(`skills/${skill}`)}
        active="current"
        openProposals={openProposals}
      />
      {versionId !== null && versionFiles !== null ? (
        <VersionFiles skill={skill} versionId={versionId} currentChip {...versionFiles} />
      ) : (
        <Card className="px-4 py-3">
          <p className="text-dim text-sm">
            Nothing published yet — this skill has a name in the catalog, but no version has been
            published to it. Publish one with the topos CLI and it appears here.
          </p>
        </Card>
      )}
      <SkillInviteAffordance mailArmed={mailArmed} invitePolicy={invitePolicy} isOwner={isOwner} />
    </div>
  );
}
