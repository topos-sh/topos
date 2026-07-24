import { and, asc, count, desc, eq, sql } from "drizzle-orm";
import type { MemberActor, OwnerActor } from "@/lib/auth/guards.server";
import { auditInTx, mintChannelId } from "@/lib/db/identity.server";
import { type Db, getDb, isUniqueViolation } from "@/lib/db/index.server";
import { bundle, channel, channelBundle, profileEntry, seat } from "@/lib/db/schema.app";

/**
 * The CHANNELS data access layer — reads over the app's own channel tables, the existence
 * ceremonies (create / rename / delete) as plain transactions, and the ONE curation core
 * (place / unplace a bundle reference) that this file's id-keyed web functions AND the
 * session lane's name-keyed ones both run. Every function is actor-first and derives its
 * workspace FROM the actor, so a caller that skipped its guard cannot compile and a
 * wrong-scope read never leaks.
 *
 * A channel is a NAMED, CURATED SET OF BUNDLES — nothing else. It has no membership: people
 * carry a channel by referencing it in their profile, projects by referencing it in
 * `topos.toml`. The DEFAULT channel ('everyone') is the BASELINE — implicit in every member's
 * profile (a profile exclude subtracts it); rename/delete refuse it. `mode` gates who edits
 * the set (open = any member, curated = reviewer+). Channel names use the same charset as
 * bundle names; audit rows ride every existence/mode write (subject = the immutable id).
 */

/** The channel-name rule (the old birth mint's bound, kept). */
const CHANNEL_NAME = /^[a-z0-9][a-z0-9-]*$/;

/**
 * Names a channel can never take: URL segments the channel surface itself claims. `new` is the
 * create form's own route (`channels/new`), which React Router ranks above the dynamic
 * `channels/:channel` face — a channel named `new` would be creatable but its page unreachable.
 */
const CHANNEL_RESERVED = new Set([
  "new",
  // The CLI's built-in skill owns the bare `topos` token in every client-side resolution — a
  // channel under that name would be unreachable from the CLI.
  "topos",
]);
const CHANNEL_NAME_MAX = 64;

/** One channel as the index renders it: identity + mode + the two counts. */
export interface ChannelSummary {
  channelId: string;
  name: string;
  mode: "open" | "curated";
  /** The default channel — the implicit baseline of every member's profile. */
  isDefault: boolean;
  /** Distinct bundle references the channel holds. */
  skillCount: number;
  /** People whose profile carries this set (the baseline: seats − excludes). */
  audienceCount: number;
}

/** The seat count — the default channel's structural audience base. */
async function seatCount(ws: string): Promise<number> {
  const rows = await getDb().select({ n: count() }).from(seat).where(eq(seat.workspaceId, ws));
  return rows[0]?.n ?? 0;
}

async function channelByName(ws: string, name: string) {
  const rows = await getDb()
    .select()
    .from(channel)
    .where(and(eq(channel.workspaceId, ws), eq(channel.name, name)))
    .limit(1);
  return rows[0];
}

/**
 * Every channel in the actor's workspace, the default first (then name order), each with its
 * bundle-reference count and its audience (how many members' profiles carry the set).
 */
export async function channelsOf(actor: MemberActor): Promise<ChannelSummary[]> {
  const ws = actor.workspaceId;
  const [channels, skillCounts, includeCounts, excludeCounts, seats] = await Promise.all([
    getDb()
      .select()
      .from(channel)
      .where(eq(channel.workspaceId, ws))
      .orderBy(desc(channel.isDefault), asc(channel.name)),
    getDb()
      .select({ channelId: channelBundle.channelId, n: count() })
      .from(channelBundle)
      .where(eq(channelBundle.workspaceId, ws))
      .groupBy(channelBundle.channelId),
    getDb()
      .select({ channelId: profileEntry.channelId, n: count() })
      .from(profileEntry)
      .where(and(eq(profileEntry.workspaceId, ws), eq(profileEntry.mode, "include")))
      .groupBy(profileEntry.channelId),
    getDb()
      .select({ channelId: profileEntry.channelId, n: count() })
      .from(profileEntry)
      .where(and(eq(profileEntry.workspaceId, ws), eq(profileEntry.mode, "exclude")))
      .groupBy(profileEntry.channelId),
    seatCount(ws),
  ]);
  const skills = new Map(skillCounts.map((c) => [c.channelId, c.n]));
  const includes = new Map(includeCounts.map((c) => [c.channelId, c.n]));
  const excludes = new Map(excludeCounts.map((c) => [c.channelId, c.n]));
  return channels.map((ch) => ({
    channelId: ch.id,
    name: ch.name,
    mode: ch.mode as ChannelSummary["mode"],
    isDefault: ch.isDefault,
    skillCount: skills.get(ch.id) ?? 0,
    audienceCount: ch.isDefault
      ? Math.max(0, seats - (excludes.get(ch.id) ?? 0))
      : (includes.get(ch.id) ?? 0),
  }));
}

/** The immutable-key probe the owner ceremonies re-read their target through: a stale form
 * whose channel was renamed still acts on THE CHANNEL THE OWNER WAS LOOKING AT (the id never
 * moves), and the delete's typed-name compares against the row's CURRENT name — server state,
 * never a form echo. A missing row (deleted meanwhile) is the caller's honest refusal. */
export interface ChannelKey {
  channelId: string;
  name: string;
  isDefault: boolean;
}

export async function channelRowById(
  actor: MemberActor,
  channelId: string,
): Promise<ChannelKey | undefined> {
  const rows = await getDb()
    .select({ channelId: channel.id, name: channel.name, isDefault: channel.isDefault })
    .from(channel)
    .where(and(eq(channel.workspaceId, actor.workspaceId), eq(channel.id, channelId)))
    .limit(1);
  return rows[0];
}

/** The name → immutable-key resolve for pages that need the id alone (the history page's
 * anchor: audit rows subject the channel ID, which outlives renames). */
export async function channelKeyByName(
  actor: MemberActor,
  name: string,
): Promise<ChannelKey | undefined> {
  const row = await channelByName(actor.workspaceId, name);
  return row === undefined
    ? undefined
    : { channelId: row.id, name: row.name, isDefault: row.isDefault };
}

/** One bundle reference a channel holds — joined to the catalog for its name/display/status. */
export interface ChannelSkillRef {
  skillId: string;
  name: string;
  displayName: string | null;
  /** Defensive: archive UNPLACES, so a placed reference should be active — render honestly if not. */
  status: "active" | "archived" | "deleted";
}

export interface ChannelDetail {
  channelId: string;
  name: string;
  mode: "open" | "curated";
  isDefault: boolean;
  createdBy: string | null;
  createdAt: Date;
  /** The bundle references, catalog-name order. */
  skills: ChannelSkillRef[];
  /** People whose profile carries this set (the baseline: seats − excludes). */
  audienceCount: number;
  /** THIS member's stance: the set is in their profile (default: not excluded). */
  viewerIncluded: boolean;
}

/**
 * One channel's full read: the row, its bundle references (joined to the catalog), its
 * audience, and the VIEWER's own stance (the page's add-to/remove-from-my-skills arm renders
 * from it). Undefined when the channel does not exist (the route renders the 404).
 */
export async function channelDetail(
  actor: MemberActor,
  name: string,
): Promise<ChannelDetail | undefined> {
  const ws = actor.workspaceId;
  const row = await channelByName(ws, name);
  if (row === undefined) {
    return undefined;
  }
  const [skills, stanceRows, audience] = await Promise.all([
    getDb()
      .select({
        skillId: channelBundle.bundleId,
        name: bundle.name,
        displayName: bundle.displayName,
        status: bundle.status,
      })
      .from(channelBundle)
      .innerJoin(
        bundle,
        and(
          eq(bundle.workspaceId, channelBundle.workspaceId),
          eq(bundle.id, channelBundle.bundleId),
        ),
      )
      .where(and(eq(channelBundle.workspaceId, ws), eq(channelBundle.channelId, row.id)))
      .orderBy(asc(bundle.name)),
    getDb()
      .select({ mode: profileEntry.mode })
      .from(profileEntry)
      .where(and(eq(profileEntry.channelId, row.id), eq(profileEntry.userId, actor.userId)))
      .limit(1),
    channelAudienceCount(ws, row.id, row.isDefault),
  ]);
  const stance = stanceRows[0]?.mode;
  return {
    channelId: row.id,
    name: row.name,
    mode: row.mode as ChannelDetail["mode"],
    isDefault: row.isDefault,
    createdBy: row.createdBy,
    createdAt: row.createdAt,
    skills: skills.map((s) => ({ ...s, status: s.status as ChannelSkillRef["status"] })),
    audienceCount: audience,
    viewerIncluded: row.isDefault ? stance !== "exclude" : stance === "include",
  };
}

async function channelAudienceCount(
  ws: string,
  channelId: string,
  isDefault: boolean,
): Promise<number> {
  if (isDefault) {
    const [seats, excludes] = await Promise.all([
      seatCount(ws),
      getDb()
        .select({ n: count() })
        .from(profileEntry)
        .where(and(eq(profileEntry.channelId, channelId), eq(profileEntry.mode, "exclude"))),
    ]);
    return Math.max(0, seats - (excludes[0]?.n ?? 0));
  }
  const includes = await getDb()
    .select({ n: count() })
    .from(profileEntry)
    .where(and(eq(profileEntry.channelId, channelId), eq(profileEntry.mode, "include")));
  return includes[0]?.n ?? 0;
}

// ── The viewer's own profile stance (the channel page's self-service arm) ───────────────────

/**
 * Add this channel to the viewer's profile — for the default channel, clear any exclude (the
 * baseline needs no include line). Mirrors the session lane's profile ops; a personal act.
 */
export async function includeChannelInProfile(
  actor: MemberActor,
  channelId: string,
): Promise<"included" | "unknown_channel"> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const rows = await tx
      .select({ id: channel.id, isDefault: channel.isDefault })
      .from(channel)
      .where(and(eq(channel.workspaceId, ws), eq(channel.id, channelId)))
      .limit(1);
    const row = rows[0];
    if (row === undefined) {
      return "unknown_channel";
    }
    if (row.isDefault) {
      await tx.execute(sql`
        DELETE FROM web.profile_entry
        WHERE user_id = ${actor.userId} AND channel_id = ${row.id} AND mode = 'exclude'
      `);
      return "included";
    }
    await tx.execute(sql`
      INSERT INTO web.profile_entry (workspace_id, user_id, mode, channel_id)
      VALUES (${ws}, ${actor.userId}, 'include', ${row.id})
      ON CONFLICT (user_id, channel_id) WHERE channel_id is not null
      DO UPDATE SET mode = 'include', updated_at = now()
    `);
    return "included";
  });
}

/**
 * Take this channel out of the viewer's profile — the default channel, being implicit, takes
 * an EXCLUDE line (the one negative state) instead of a deletion.
 */
export async function removeChannelFromProfile(
  actor: MemberActor,
  channelId: string,
): Promise<"removed" | "unknown_channel"> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const rows = await tx
      .select({ id: channel.id, isDefault: channel.isDefault })
      .from(channel)
      .where(and(eq(channel.workspaceId, ws), eq(channel.id, channelId)))
      .limit(1);
    const row = rows[0];
    if (row === undefined) {
      return "unknown_channel";
    }
    if (row.isDefault) {
      await tx.execute(sql`
        INSERT INTO web.profile_entry (workspace_id, user_id, mode, channel_id)
        VALUES (${ws}, ${actor.userId}, 'exclude', ${row.id})
        ON CONFLICT (user_id, channel_id) WHERE channel_id is not null
        DO UPDATE SET mode = 'exclude', updated_at = now()
      `);
      return "removed";
    }
    await tx.execute(sql`
      DELETE FROM web.profile_entry
      WHERE user_id = ${actor.userId} AND channel_id = ${row.id} AND mode = 'include'
    `);
    return "removed";
  });
}

// ── Existence admin (create / rename / delete) ──────────────────────────────────────────────

export type ChannelCreateOutcome =
  | { outcome: "created"; channelId: string }
  | { outcome: "name_taken" }
  | { outcome: "bad_name" };

/**
 * Create a named channel. The unique index is the race arbiter: a create-race loser maps to
 * the honest `name_taken`, never a 500. Member-level — the same grade as the session lane's
 * create-on-first-use placement.
 */
export async function createChannel(
  actor: MemberActor,
  name: string,
): Promise<ChannelCreateOutcome> {
  if (!CHANNEL_NAME.test(name) || name.length > CHANNEL_NAME_MAX || CHANNEL_RESERVED.has(name)) {
    return { outcome: "bad_name" };
  }
  const channelId = mintChannelId();
  try {
    await getDb().transaction(async (tx) => {
      await tx.insert(channel).values({
        id: channelId,
        workspaceId: actor.workspaceId,
        name,
        createdBy: actor.userId,
      });
      await auditInTx(tx, {
        workspaceId: actor.workspaceId,
        actor: { userId: actor.userId, display: actor.display },
        kind: "channel_created",
        subject: channelId,
        outcome: "ok",
        details: { name },
      });
    });
  } catch (error) {
    if (isUniqueViolation(error)) {
      return { outcome: "name_taken" };
    }
    throw error;
  }
  return { outcome: "created", channelId };
}

export type ChannelRenameOutcome =
  | "renamed"
  | "name_taken"
  | "bad_name"
  | "builtin"
  | "unknown_channel";

/**
 * Rename a channel — an owner act keyed on the IMMUTABLE channel id, refusing the default
 * channel typed. References, profile lines, and the audit trail survive; only the display
 * name moves (no hint table for channels — a channel name is a grouping label, not a
 * distribution address a session pins).
 */
export async function renameChannel(
  actor: OwnerActor,
  channelId: string,
  newName: string,
): Promise<ChannelRenameOutcome> {
  if (
    !CHANNEL_NAME.test(newName) ||
    newName.length > CHANNEL_NAME_MAX ||
    CHANNEL_RESERVED.has(newName)
  ) {
    return "bad_name";
  }
  try {
    return await getDb().transaction(async (tx) => {
      const rows = await tx
        .select({ name: channel.name, isDefault: channel.isDefault })
        .from(channel)
        .where(and(eq(channel.workspaceId, actor.workspaceId), eq(channel.id, channelId)))
        .limit(1);
      const row = rows[0];
      if (row === undefined) {
        return "unknown_channel";
      }
      if (row.isDefault) {
        return "builtin";
      }
      if (row.name !== newName) {
        await tx
          .update(channel)
          .set({ name: newName })
          .where(and(eq(channel.workspaceId, actor.workspaceId), eq(channel.id, channelId)));
        await auditInTx(tx, {
          workspaceId: actor.workspaceId,
          actor: { userId: actor.userId, display: actor.display },
          kind: "channel_renamed",
          subject: channelId,
          outcome: "ok",
          details: { from: row.name, to: newName },
        });
      }
      return "renamed";
    });
  } catch (error) {
    if (isUniqueViolation(error)) {
      return "name_taken";
    }
    throw error;
  }
}

export type ChannelDeleteOutcome = "deleted" | "builtin" | "unknown_channel";

/**
 * Delete a channel — an owner act keyed on the immutable id, refusing the default channel.
 * References and profile lines CASCADE with the row; a channel deletion is an upstream
 * withdrawal — bundles another channel or a direct include still provides keep flowing. The
 * audit row keeps the channel id: history is append-only and survives the row.
 */
export async function deleteChannel(
  actor: OwnerActor,
  channelId: string,
): Promise<ChannelDeleteOutcome> {
  return await getDb().transaction(async (tx) => {
    const rows = await tx
      .select({ name: channel.name, isDefault: channel.isDefault })
      .from(channel)
      .where(and(eq(channel.workspaceId, actor.workspaceId), eq(channel.id, channelId)))
      .limit(1);
    const row = rows[0];
    if (row === undefined) {
      return "unknown_channel";
    }
    if (row.isDefault) {
      return "builtin";
    }
    await tx
      .delete(channel)
      .where(and(eq(channel.workspaceId, actor.workspaceId), eq(channel.id, channelId)));
    await auditInTx(tx, {
      workspaceId: actor.workspaceId,
      actor: { userId: actor.userId, display: actor.display },
      kind: "channel_deleted",
      subject: channelId,
      outcome: "ok",
      details: { name: row.name },
    });
    return "deleted";
  });
}

// ── Curation (place / unplace a bundle reference) — the ONE core both doors run ─────────────

type Tx = Parameters<Parameters<Db["transaction"]>[0]>[0];

/**
 * The actor shape BOTH curation doors satisfy: the web page's MemberActor and the session
 * lane's SessionActor (whose sessionId rides into the audit row when present). The policy —
 * gates, idempotence, audit — is written ONCE against this shape so the two lanes cannot
 * drift; the branded outer functions stay the only entry points.
 */
interface CurationActor {
  readonly userId: string;
  readonly display: string;
  readonly workspaceId: string;
  readonly role: "owner" | "reviewer" | "member";
  readonly sessionId?: string;
}

/** The catalog probe every curation door gates on FIRST: NULL = no such bundle in this
 * workspace (checked before any channel resolution, so a bad skill never mints a channel
 * on the session lane and the two doors refuse in the same order). */
export async function bundleStatusInTx(
  tx: Tx,
  ws: string,
  bundleId: string,
): Promise<string | null> {
  const rows = await tx
    .select({ status: bundle.status })
    .from(bundle)
    .where(and(eq(bundle.workspaceId, ws), eq(bundle.id, bundleId)))
    .limit(1);
  return rows[0]?.status ?? null;
}

/**
 * The place core, INSIDE the caller's transaction, after the caller resolved its channel (the
 * session lane by NAME with create-on-first-use; the web page by ID): the curated-mode role
 * gate (reviewer+), the idempotent reference insert (ON CONFLICT — a re-place answers
 * 'placed' again), and the `skill_added` audit row — emitted for the ACT, a re-place of a
 * standing row included.
 */
export async function placeBundleRefInTx(
  tx: Tx,
  actor: CurationActor,
  target: { id: string; mode: string },
  bundleId: string,
): Promise<"placed" | "curated_role_required"> {
  if (target.mode === "curated" && actor.role === "member") {
    return "curated_role_required";
  }
  await tx
    .insert(channelBundle)
    .values({
      channelId: target.id,
      workspaceId: actor.workspaceId,
      bundleId,
      addedBy: actor.userId,
    })
    .onConflictDoNothing();
  await auditInTx(tx, {
    workspaceId: actor.workspaceId,
    actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
    kind: "skill_added",
    subject: target.id,
    outcome: "ok",
    details: { skillId: bundleId },
  });
  return "placed";
}

/** The unplace core — symmetric gate with place; the delete's row count answers not_placed. */
export async function unplaceBundleRefInTx(
  tx: Tx,
  actor: CurationActor,
  target: { id: string; mode: string },
  bundleId: string,
): Promise<"removed" | "not_placed" | "curated_role_required"> {
  if (target.mode === "curated" && actor.role === "member") {
    return "curated_role_required";
  }
  const deleted = await tx
    .delete(channelBundle)
    .where(
      and(
        eq(channelBundle.workspaceId, actor.workspaceId),
        eq(channelBundle.channelId, target.id),
        eq(channelBundle.bundleId, bundleId),
      ),
    )
    .returning({ bundleId: channelBundle.bundleId });
  if (deleted.length === 0) {
    return "not_placed";
  }
  await auditInTx(tx, {
    workspaceId: actor.workspaceId,
    actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
    kind: "skill_removed",
    subject: target.id,
    outcome: "ok",
    details: { skillId: bundleId },
  });
  return "removed";
}

/** The id-keyed channel resolve the web curation functions share — workspace-scoped, mode
 * included (the gate needs it). */
async function channelCurationTargetInTx(tx: Tx, ws: string, channelId: string) {
  const rows = await tx
    .select({ id: channel.id, mode: channel.mode })
    .from(channel)
    .where(and(eq(channel.workspaceId, ws), eq(channel.id, channelId)))
    .limit(1);
  return rows[0];
}

export type ChannelPlaceOutcome =
  | "placed"
  | "unknown_channel"
  | "unknown_skill"
  | "skill_not_active"
  | "curated_role_required";

/**
 * Place a bundle reference into a channel — the web page's door onto the one curation core
 * the session lane shares: same gates (the bundle must exist in-workspace and be active; a
 * CURATED channel takes reviewer+), same idempotence, same audit row. Keyed on the IMMUTABLE
 * channel id like every web ceremony — the page operates on an existing channel, so there is
 * NO create-on-first-use here: an id that does not resolve in the actor's workspace is the
 * honest unknown_channel, never a mint.
 */
export async function placeBundleInChannel(
  actor: MemberActor,
  channelId: string,
  bundleId: string,
): Promise<ChannelPlaceOutcome> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const status = await bundleStatusInTx(tx, ws, bundleId);
    if (status === null) {
      return "unknown_skill";
    }
    if (status !== "active") {
      return "skill_not_active";
    }
    const target = await channelCurationTargetInTx(tx, ws, channelId);
    if (target === undefined) {
      return "unknown_channel";
    }
    return await placeBundleRefInTx(tx, actor, target, bundleId);
  });
}

export type ChannelUnplaceOutcome =
  | "removed"
  | "not_placed"
  | "unknown_channel"
  | "curated_role_required";

/** Remove a bundle reference from a channel — id-keyed, symmetric gate with place. */
export async function unplaceBundleFromChannel(
  actor: MemberActor,
  channelId: string,
  bundleId: string,
): Promise<ChannelUnplaceOutcome> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const target = await channelCurationTargetInTx(tx, ws, channelId);
    if (target === undefined) {
      return "unknown_channel";
    }
    return await unplaceBundleRefInTx(tx, actor, target, bundleId);
  });
}
