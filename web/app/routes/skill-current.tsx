import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import {
  data,
  Form,
  Link,
  redirect,
  useLoaderData,
  useNavigation,
  useSearchParams,
} from "react-router";
import { VersionFiles } from "@/components/browse/version-files";
import { ConfirmButton } from "@/components/confirm";
import { relativeTime } from "@/components/format";
import { SkillHeader } from "@/components/skill/skill-header";
import { SkillInviteAffordance } from "@/components/skill/skill-invite";
import { SkillTabs } from "@/components/skill/skill-tabs";
import { Card, Chip, SectionHeading, ShortId } from "@/components/ui";
import {
  actorFromSession,
  type MemberActor,
  memberInScope,
  notFound,
  requireMemberInScope,
  requireWorkspaceOwner,
  type ScopedWorkspace,
} from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { loadVersionFilesData } from "@/lib/browse/version-files.server";
import { baseOf, bundleNameOf, bundleNoun, bundlePath, useBundleBase } from "@/lib/bundle-base";
import { requireCanonicalBase } from "@/lib/bundle-base.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import { channelsCarrying } from "@/lib/db/queries.channels.server";
import { assignBundle, assignedToEveryone, unassign } from "@/lib/db/queries.feed.server";
import { mcpProbeFor } from "@/lib/db/queries.mcp.server";
import { createInvitations, foldInviteEmail } from "@/lib/db/queries.roster.server";
import { skillIndexRow } from "@/lib/db/queries.server";
import { type AppliedOnSession, yourSessionsApplying } from "@/lib/db/queries.sessions.server";
import { resolveSkillName } from "@/lib/db/resolve.server";
import { sendInviteEmail } from "@/lib/mail/invite-mail.server";
import { mailDelivery } from "@/lib/mail/transport.server";
import { probeStateLine } from "@/lib/mcp/probe-state";
import { useWsPath } from "@/lib/ws-path";
import { agentDocUrl, inviteUrl, wsPathServer } from "@/lib/ws-url.server";

export function meta({ params }: { params: { skill?: string; server?: string } }) {
  return [{ title: `${params.server ?? params.skill ?? "skill"} · Topos` }];
}

/**
 * The bundle FACE — resource address and canonical Current tab as ONE route, mounted under BOTH
 * bases (`skills/:skill` and `mcp/:server`) and fenced to the kind each addresses. A bundle page is
 * MEMBERS-ONLY: an anonymous browser gets the house 404, indistinguishable from a mistyped path, so
 * nothing about a skill (not even that the address shape names one) leaks to a signed-out visitor.
 * (A non-browser document fetch still got the constant protocol card from the server entry — that
 * machine face is existence-blind and teaches `topos login` regardless.) A signed-in member gets
 * the page WITH chrome; a signed-in non-member (or unknown workspace slug) gets the same 404.
 *
 * The Current tab is the DEFAULT view: the current version's files + doc preview inline.
 * Proposals and History are sibling MEMBER-only routes (see SkillTabs). The catalog row this page
 * probes IS the directory's identity surface: the NAME exists the moment a bundle is minted, and the
 * `current` pointer joins in when a publish has landed one. A known name that has NEVER published
 * (`versionId` null) renders honestly; an unknown NAME is the uniform 404 (a rename hint redirects).
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const session = await getAuth().api.getSession({ headers: request.headers });
  const actor = actorFromSession(session);
  if (actor === null) {
    // Signed out: the bundle face is not a public teaser — it is the uniform house 404, so an
    // anonymous probe cannot tell a real skill from a nonexistent one (or from any other path).
    notFound();
  }
  const { workspace, actor: memberActor } = await memberInScope(actor, params);
  const base = baseOf(params);
  const skill = bundleNameOf(params);
  const row = await skillIndexRow(memberActor, skill);
  if (row === undefined) {
    // A rename left an old name behind: follow the resolving hint to the live name; else 404.
    const resolved = await resolveSkillName(memberActor, skill);
    if (resolved !== undefined && resolved.via === "hint" && resolved.status === "active") {
      throw redirect(wsPathServer(workspace.name, bundlePath(base, resolved.name)));
    }
    notFound();
  }
  // A member who addressed it under the other kind's base lands on the canonical page.
  requireCanonicalBase({ wsName: workspace.name, base, kind: row.kind, name: skill });

  const [versionFiles, channels, yourSessions, everyoneAssigned, probe] = await Promise.all([
    row.versionId !== null
      ? loadVersionFilesData(memberActor, row.skillId, row.versionId)
      : Promise.resolve(null),
    // Where the bundle goes and where it landed — the two halves of "who has this", read
    // read-only: the channels are a workspace fact, the sessions are the VIEWER'S OWN machines.
    channelsCarrying(memberActor, row.skillId),
    yourSessionsApplying(memberActor, row.skillId),
    assignedToEveryone(memberActor, { bundleId: row.skillId }),
    // WHAT THE PLANE SAW when it last asked this server. MCP only — a skill has nothing to ask —
    // and advisory throughout: a version nobody probed reads "not checked yet", which says
    // something about this plane and nothing about the server.
    row.kind === "mcp" && row.versionId !== null
      ? mcpProbeFor(memberActor, row.skillId, row.versionId)
      : Promise.resolve(null),
  ]);

  return {
    face: "page" as const,
    wsName: workspace.name,
    skill,
    skillId: row.skillId,
    currentShort: row.versionId !== null ? row.versionId.slice(0, 12) : "—",
    displayName: row.displayName,
    kind: row.kind,
    openProposals: row.openProposals,
    versionId: row.versionId,
    versionFiles,
    channels,
    yourSessions,
    everyoneAssigned,
    /** The probe line, resolved server-side: this kind has one, or it does not. */
    probeLine: row.kind === "mcp" && row.versionId !== null ? probeStateLine(probe) : null,
    // The invite affordance's gates, resolved once here and never re-read client-side: armed mail
    // is the invitation's identity rung, and inviting is owner-only — the same two facts the
    // members page surfaces.
    mailArmed: mailDelivery().canSend,
    isOwner: memberActor.role === "owner",
  };
}

/**
 * The skill face's action. It RE-GUARDS from scratch — a loader's gate never carries into an
 * action — with the member scope the face itself requires (an anonymous request bounces to the
 * constant /login before any skill read; a non-member and an unknown slug land the same uniform
 * 404). Member scope is the FLOOR; each branch re-reads its own gate: `invite` runs the owner
 * check inside createInvitations, and the two ASSIGNMENT arms take requireWorkspaceOwner, whose
 * refusal is the same uniform 404 every other owner-only act answers with. An unmatched intent
 * is a 400 that only a member can ever reach.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const { workspace, actor } = await requireMemberInScope(request, params);
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  if (intent === "invite") {
    return inviteToSkillIntent(request, workspace, actor, bundleNameOf(params), formData);
  }
  if (intent === "assign-everyone" || intent === "unassign-everyone") {
    return assignEveryoneIntent(request, workspace.id, intent, formData);
  }
  return data({ intent: "unknown" as const, status: "error" as const }, { status: 400 });
}

/**
 * The curator arm: put this skill in EVERY member's feed, or withdraw that offer. Owner-only —
 * an assignment reaches people, so the gate is the roster's, not the channel-curation one. The
 * bundle is named by its immutable id (a hidden field the page loaded with), never by the URL's
 * mutable name. Withdrawing takes back the offer alone: a machine that already has the bytes
 * keeps them until it reconciles, and anyone who added the skill themselves still has it.
 */
async function assignEveryoneIntent(
  request: Request,
  ws: string,
  intent: "assign-everyone" | "unassign-everyone",
  formData: FormData,
) {
  const owner = await requireWorkspaceOwner(request, ws);
  const bundleId = String(formData.get("skill_id") ?? "");
  try {
    const outcome =
      intent === "assign-everyone"
        ? await assignBundle(owner, bundleId, { everyone: true })
        : await unassign(owner, { bundleId }, { everyone: true });
    return data(
      { intent, status: outcome },
      outcome === "assigned" || outcome === "unassigned" ? undefined : { status: 400 },
    );
  } catch {
    await recordAdminEvent(owner, { kind: "assigned", subject: bundleId, outcome: "error" });
    return data({ intent, status: "error" as const }, { status: 500 });
  }
}

/**
 * Invite a teammate to THIS skill. Inviting is owner-only (createInvitations runs that gate
 * against the actor's role) and it REQUIRES armed mail — the invitation's identity
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

  const outcome = await createInvitations(actor, [folded], {
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
  skillId,
  currentShort,
  displayName,
  kind,
  openProposals,
  versionId,
  versionFiles,
  channels,
  yourSessions,
  everyoneAssigned,
  probeLine,
  mailArmed,
  isOwner,
}: Extract<Awaited<ReturnType<typeof loader>>, { face: "page" }>) {
  const wsPath = useWsPath();
  const base = useBundleBase();
  const noun = bundleNoun(base);
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
        basePath={wsPath(bundlePath(base, skill))}
        active="current"
        openProposals={openProposals}
        showSettings={isOwner}
      />
      <PlacementNote />
      {probeLine !== null && (
        <p data-testid="mcp-probe" className="text-dim text-sm">
          {probeLine}
        </p>
      )}
      {versionId !== null && versionFiles !== null ? (
        <VersionFiles skill={skill} versionId={versionId} currentChip {...versionFiles} />
      ) : (
        <Card className="px-4 py-3">
          <p className="text-dim text-sm">
            Nothing published yet — this {noun} has a name in the catalog, but no version has been
            published to it. Publish one with the topos CLI and it appears here.
          </p>
        </Card>
      )}
      <DeliverySection
        skillId={skillId}
        noun={noun}
        channels={channels}
        yourSessions={yourSessions}
        everyoneAssigned={everyoneAssigned}
        isOwner={isOwner}
      />
      <SkillInviteAffordance mailArmed={mailArmed} isOwner={isOwner} noun={noun} />
    </div>
  );
}

/** A channel name as this app mints them — the only spelling the note below will echo back. */
const CHANNEL_NAME = /^[a-z0-9][a-z0-9-]{0,63}$/;

/**
 * WHAT THE PUBLISH DID NOT DO. A bundle can land in the catalog while its PLACEMENT is withheld:
 * a curated channel takes a member's placement (the default `everyone` included), and a channel
 * deleted mid-publish has nothing to place into. The act said so on the receipt; the page the
 * publisher lands on has to say it too, or the bundle reads as delivered when it reaches nobody.
 *
 * The two facts arrive as query parameters, which makes them FORGEABLE — so nothing here trusts
 * them with anything: an unknown outcome renders nothing at all, and the channel name is echoed
 * only when it is spelled the way this app mints channel names.
 *
 * Exported for the unit suite: the sentence each outcome earns is the disclosure.
 */
export function placementNote(placement: string | null, channel: string | null): string | null {
  const named = channel !== null && CHANNEL_NAME.test(channel) ? `#${channel}` : "that channel";
  if (placement === "curated_role_required") {
    return `Published to the catalog — placing it into ${named} takes a reviewer or owner.`;
  }
  if (placement === "channel_not_found") {
    return `Published to the catalog — ${named} was not there to place it into.`;
  }
  return null;
}

function PlacementNote() {
  const [params] = useSearchParams();
  const note = placementNote(params.get("placement"), params.get("channel"));
  if (note === null) {
    return null;
  }
  return (
    <Card className="px-4 py-3">
      <p role="status" data-testid="placement-note" className="text-dim text-sm">
        {note}
      </p>
    </Card>
  );
}

/**
 * Where this bundle goes and where it landed — read-only and quiet. The channels are a workspace
 * fact (which sets carry the reference); the sessions are the READER'S OWN machines and nobody
 * else's, because this page answers "do I have this, and at which version" — the
 * workspace-wide answer is the Sessions page, which has its own role scoping. `noun` is what the
 * copy calls it: a skill is held, an MCP server is held the same way and says so.
 */
function DeliverySection({
  skillId,
  noun,
  channels,
  yourSessions,
  everyoneAssigned,
  isOwner,
}: {
  skillId: string;
  noun: string;
  channels: { channelId: string; name: string; isDefault: boolean }[];
  yourSessions: AppliedOnSession[];
  everyoneAssigned: boolean;
  isOwner: boolean;
}) {
  const wsPath = useWsPath();
  return (
    <section aria-labelledby="skill-delivery-heading" className="space-y-3">
      <SectionHeading>
        <span id="skill-delivery-heading">Delivery</span>
      </SectionHeading>
      <Card className="space-y-4 px-4 py-3">
        <div data-testid="skill-channels" className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <span className="text-dim text-sm">
            {channels.length === 0 ? "In no channel." : "Carried by"}
          </span>
          {channels.map((channel) => (
            <Link
              key={channel.channelId}
              to={wsPath(`channels/${channel.name}`)}
              className="text-ink text-sm underline decoration-hairline"
            >
              {channel.name}
            </Link>
          ))}
          {everyoneAssigned && <Chip tone="accent">assigned to everyone</Chip>}
        </div>
        <div data-testid="skill-your-sessions" className="space-y-1.5">
          {yourSessions.length === 0 ? (
            <p className="text-faint text-sm">
              None of your machines has reported holding this {noun}.
            </p>
          ) : (
            <>
              <p className="text-dim text-sm">On your machines</p>
              <ul className="space-y-1">
                {yourSessions.map((session) => (
                  <li
                    key={session.sessionId}
                    className="flex flex-wrap items-center gap-x-2 gap-y-1"
                  >
                    <span className="min-w-0 truncate text-ink text-sm">{session.displayName}</span>
                    <ShortId value={session.appliedVersionId} />
                    <Chip tone={session.current ? "verified" : "pending"}>
                      {session.current ? "current" : "behind"}
                    </Chip>
                    <span className="text-faint text-xs">
                      reported {relativeTime(new Date(session.reportedAtMs))}
                    </span>
                  </li>
                ))}
              </ul>
            </>
          )}
        </div>
        {isOwner && <EveryoneArm skillId={skillId} noun={noun} assigned={everyoneAssigned} />}
      </Card>
    </section>
  );
}

/**
 * The owner's one assignment arm on a bundle: give it to everyone here, or withdraw that. It
 * reaches people, so it wears the in-place two-step confirm every people-affecting act wears —
 * arming performs nothing, and only the armed submit posts.
 */
function EveryoneArm({
  skillId,
  noun,
  assigned,
}: {
  skillId: string;
  noun: string;
  assigned: boolean;
}) {
  const navigation = useNavigation();
  const intent = assigned ? "unassign-everyone" : "assign-everyone";
  const pending = navigation.state !== "idle" && navigation.formData?.get("intent") === intent;
  return (
    <div
      data-testid="skill-everyone-arm"
      className="flex flex-wrap items-center gap-x-3 gap-y-2 border-line-soft border-t pt-3"
    >
      <span className="text-faint text-xs">
        {assigned
          ? `Every member of this workspace is assigned this ${noun}. Withdrawing takes back the offer; copies already on a machine stay until it updates.`
          : `Assigning to everyone puts this ${noun} in every member's feed. Each person can still turn it off for themselves.`}
      </span>
      <Form method="post" className="ml-auto">
        <input type="hidden" name="intent" value={intent} />
        <input type="hidden" name="skill_id" value={skillId} />
        <ConfirmButton
          label={assigned ? "Remove from everyone" : "Assign to everyone"}
          tone="quiet"
          pending={pending}
        />
      </Form>
    </div>
  );
}
