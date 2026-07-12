import { and, asc, count, desc, eq } from "drizzle-orm";
import type { MemberActor, OwnerActor } from "@/lib/auth/guards.server";
import { getDb, getPool } from "@/lib/db/index.server";
import {
  planeCatalog,
  planeChannelEvents,
  planeChannelMembers,
  planeChannelSkills,
  planeChannels,
  planeWorkspaceMember,
} from "@/lib/db/schema.plane";

/**
 * The CHANNELS data access layer — the sanctioned door to the directory's channel tables
 * (`plane.channels` / `channel_skills` / `channel_members` / `channel_events`, all read-only by
 * grant) and the two guarded existence-admin writes (`topos_channel_rename` /
 * `topos_channel_delete`). Every function is actor-first and derives its workspace FROM the actor
 * (the actor is the scope), so a caller that skipped its guard cannot compile and a wrong-scope
 * read never leaks.
 *
 * A channel is plain rows: a named group holding skill REFERENCES (labels, one skill delivered
 * once) and person-scoped memberships, plus a trigger-emitted audit log. `everyone` is
 * STRUCTURAL — `builtin = 1`, membership derived from the confirmed roster (it holds NO
 * channel_members rows), undeletable and unrenameable (the database's own triggers hold that
 * invariant; the guarded functions relay it as the `builtin` outcome code).
 */

/** One channel as the index renders it: identity + mode + the two counts. */
export interface ChannelSummary {
  channelId: string;
  name: string;
  mode: "open" | "curated";
  /** The structural `everyone` channel — its membership is the roster, not rows. */
  builtin: boolean;
  /** Distinct skill references the channel holds. */
  skillCount: number;
  /**
   * People the channel reaches: for `everyone` the CONFIRMED roster count (structural — there are
   * no membership rows); for any other channel the count of its `channel_members` rows.
   */
  memberCount: number;
}

/** The confirmed-roster size — `everyone`'s structural membership count. */
async function confirmedMemberCount(ws: string): Promise<number> {
  const rows = await getDb()
    .select({ n: count() })
    .from(planeWorkspaceMember)
    .where(
      and(eq(planeWorkspaceMember.workspaceId, ws), eq(planeWorkspaceMember.status, "confirmed")),
    );
  return rows[0]?.n ?? 0;
}

/** One channel row by its user-facing name, or undefined — the detail/history existence probe. */
async function channelByName(ws: string, name: string) {
  const rows = await getDb()
    .select()
    .from(planeChannels)
    .where(and(eq(planeChannels.workspaceId, ws), eq(planeChannels.name, name)))
    .limit(1);
  return rows[0];
}

/**
 * Every channel in the actor's workspace, `everyone` first (builtin, then name order), each with
 * its skill-reference count and its member count — the structural `everyone` counting the
 * confirmed roster, every other channel counting its own membership rows.
 */
export async function channelsOf(actor: MemberActor): Promise<ChannelSummary[]> {
  const ws = actor.workspaceId;
  const [channels, skillCounts, memberCounts, confirmed] = await Promise.all([
    getDb()
      .select()
      .from(planeChannels)
      .where(eq(planeChannels.workspaceId, ws))
      // `everyone` (builtin = 1) leads; the rest in name order.
      .orderBy(desc(planeChannels.builtin), asc(planeChannels.name)),
    getDb()
      .select({ channelId: planeChannelSkills.channelId, n: count() })
      .from(planeChannelSkills)
      .where(eq(planeChannelSkills.workspaceId, ws))
      .groupBy(planeChannelSkills.channelId),
    getDb()
      .select({ channelId: planeChannelMembers.channelId, n: count() })
      .from(planeChannelMembers)
      .where(eq(planeChannelMembers.workspaceId, ws))
      .groupBy(planeChannelMembers.channelId),
    confirmedMemberCount(ws),
  ]);
  const skills = new Map(skillCounts.map((c) => [c.channelId, c.n]));
  const members = new Map(memberCounts.map((c) => [c.channelId, c.n]));
  return channels.map((ch) => ({
    channelId: ch.channelId,
    name: ch.name,
    mode: ch.mode,
    builtin: ch.builtin === 1,
    skillCount: skills.get(ch.channelId) ?? 0,
    // `everyone` reaches the whole confirmed roster structurally; others count their rows.
    memberCount: ch.builtin === 1 ? confirmed : (members.get(ch.channelId) ?? 0),
  }));
}

/** One skill reference a channel holds — joined to the catalog for its name/display/status. */
export interface ChannelSkillRef {
  skillId: string;
  /** The catalog name the skill page routes on. */
  name: string;
  displayName: string | null;
  /** Defensive: archive UNPLACES, so a placed reference should be active — render honestly if not. */
  status: "active" | "archived" | "deleted";
}

/** One person-scoped membership row (empty for the structural `everyone`). */
export interface ChannelMemberRef {
  principal: string;
  addedBy: string | null;
  addedAt: string;
}

export interface ChannelDetail {
  channelId: string;
  name: string;
  mode: "open" | "curated";
  builtin: boolean;
  createdBy: string | null;
  createdAt: string;
  /** The skill references, catalog-name order. */
  skills: ChannelSkillRef[];
  /** The person-scoped memberships (always [] for `everyone` — it is roster-derived). */
  members: ChannelMemberRef[];
  /** The confirmed roster size — what `everyone` reaches structurally. */
  confirmedMemberCount: number;
}

/**
 * One channel's full read: the row, its skill references (joined to the catalog), and its
 * members — or the structural note for `everyone` (an empty member list plus the confirmed roster
 * count). Undefined when the channel does not exist (the route renders the uniform 404).
 */
export async function channelDetail(
  actor: MemberActor,
  name: string,
): Promise<ChannelDetail | undefined> {
  const ws = actor.workspaceId;
  const channel = await channelByName(ws, name);
  if (channel === undefined) {
    return undefined;
  }
  const [skills, members, confirmed] = await Promise.all([
    getDb()
      .select({
        skillId: planeChannelSkills.skillId,
        name: planeCatalog.name,
        displayName: planeCatalog.displayName,
        status: planeCatalog.status,
      })
      .from(planeChannelSkills)
      // channel_skills FKs onto the catalog, so every reference has a catalog row.
      .innerJoin(
        planeCatalog,
        and(
          eq(planeCatalog.workspaceId, planeChannelSkills.workspaceId),
          eq(planeCatalog.skillId, planeChannelSkills.skillId),
        ),
      )
      .where(
        and(
          eq(planeChannelSkills.workspaceId, ws),
          eq(planeChannelSkills.channelId, channel.channelId),
        ),
      )
      .orderBy(asc(planeCatalog.name)),
    channel.builtin === 1
      ? Promise.resolve<ChannelMemberRef[]>([])
      : getDb()
          .select({
            principal: planeChannelMembers.principal,
            addedBy: planeChannelMembers.addedBy,
            addedAt: planeChannelMembers.addedAt,
          })
          .from(planeChannelMembers)
          .where(
            and(
              eq(planeChannelMembers.workspaceId, ws),
              eq(planeChannelMembers.channelId, channel.channelId),
            ),
          )
          .orderBy(asc(planeChannelMembers.addedAt), asc(planeChannelMembers.principal)),
    confirmedMemberCount(ws),
  ]);
  return {
    channelId: channel.channelId,
    name: channel.name,
    mode: channel.mode,
    builtin: channel.builtin === 1,
    createdBy: channel.createdBy,
    createdAt: channel.createdAt,
    skills,
    members,
    confirmedMemberCount: confirmed,
  };
}

/** One audit row from the trigger-emitted channel log. */
export interface ChannelEvent {
  id: number;
  event: string;
  /** The skill (immutable id) a placement/removal touched, when the event names one. */
  skillId: string | null;
  /** The person a join/leave touched, when the event names one. */
  principal: string | null;
  /** Who drove the write (`unattributed` for a bypassing write). */
  actor: string;
  /** TEXT ISO-8601 — parse at the display edge. */
  createdAt: string;
}

export interface ChannelHistory {
  channelId: string;
  channelName: string;
  events: ChannelEvent[];
  /** True when older events exist beyond the window — retained, just not shown. */
  hasMore: boolean;
}

/** The audit window ceiling. */
const HISTORY_MAX_LIMIT = 100;

/**
 * One channel's audit trail, newest first, bounded by `limit` (default + cap 100) with a +1 probe
 * so a full window can honestly say older events exist. The channel is resolved by NAME, so this
 * read 404s once the channel row is deleted even though its `channel_events` rows SURVIVE the
 * deletion (the append-only audit outlives the row) — the accepted shape: history is reachable
 * only through a live channel.
 */
export async function channelHistory(
  actor: MemberActor,
  name: string,
  opts: { limit?: number } = {},
): Promise<ChannelHistory | undefined> {
  const ws = actor.workspaceId;
  const channel = await channelByName(ws, name);
  if (channel === undefined) {
    return undefined;
  }
  const limit = Math.min(Math.max(opts.limit ?? HISTORY_MAX_LIMIT, 1), HISTORY_MAX_LIMIT);
  const rows = await getDb()
    .select({
      id: planeChannelEvents.id,
      event: planeChannelEvents.event,
      skillId: planeChannelEvents.skillId,
      principal: planeChannelEvents.principal,
      actor: planeChannelEvents.actor,
      createdAt: planeChannelEvents.createdAt,
    })
    .from(planeChannelEvents)
    .where(
      and(
        eq(planeChannelEvents.workspaceId, ws),
        eq(planeChannelEvents.channelId, channel.channelId),
      ),
    )
    // The IDENTITY column is a monotone total order — newest first without a tie-break.
    .orderBy(desc(planeChannelEvents.id))
    .limit(limit + 1);
  const hasMore = rows.length > limit;
  return {
    channelId: channel.channelId,
    channelName: channel.name,
    events: hasMore ? rows.slice(0, limit) : rows,
    hasMore,
  };
}

/** The outcome codes `topos_channel_rename` speaks (relayed verbatim). */
export type ChannelRenameOutcome =
  | "renamed"
  | "name_taken"
  | "bad_name"
  | "builtin"
  | "unknown_channel"
  | "owner_role_required"
  | "member_required";

/**
 * Rename a channel: ONE call to the guarded `topos_channel_rename`, which re-runs the owner gate,
 * refuses the structural `everyone` (`builtin`), and validates the new name — this tier adds
 * nothing to the decision. The channel_id is immutable, so references, memberships, and the audit
 * trail survive; only the display name moves.
 */
export async function renameChannel(
  actor: OwnerActor,
  name: string,
  newName: string,
): Promise<ChannelRenameOutcome> {
  const createdAt = new Date().toISOString();
  const result = await getPool().query<{ outcome: ChannelRenameOutcome }>(
    "select topos_channel_rename($1, $2, $3, $4, $5) as outcome",
    [actor.workspaceId, name, newName, actor.email, createdAt],
  );
  const outcome = result.rows[0]?.outcome;
  if (outcome === undefined) {
    throw new Error("topos_channel_rename returned no outcome");
  }
  return outcome;
}

/** The outcome codes `topos_channel_delete` speaks (relayed verbatim). */
export type ChannelDeleteOutcome =
  | "deleted"
  | "builtin"
  | "unknown_channel"
  | "owner_role_required"
  | "member_required";

/**
 * Delete a channel: ONE call to the guarded `topos_channel_delete`, which re-runs the owner gate,
 * refuses `everyone`, and CASCADE-deletes the references and memberships (each delete rides the
 * audit trigger, so the history records exactly what the deletion unplaced). Deliberately writes
 * NO person-detach records — a channel deletion is an upstream withdrawal, never a person's own
 * detach; skills another channel or a direct follow still delivers keep flowing.
 */
export async function deleteChannel(
  actor: OwnerActor,
  name: string,
): Promise<ChannelDeleteOutcome> {
  const createdAt = new Date().toISOString();
  const result = await getPool().query<{ outcome: ChannelDeleteOutcome }>(
    "select topos_channel_delete($1, $2, $3, $4) as outcome",
    [actor.workspaceId, name, actor.email, createdAt],
  );
  const outcome = result.rows[0]?.outcome;
  if (outcome === undefined) {
    throw new Error("topos_channel_delete returned no outcome");
  }
  return outcome;
}
