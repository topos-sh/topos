import { and, asc, count, desc, eq } from "drizzle-orm";
import type { MemberActor, OwnerActor } from "@/lib/auth/guards.server";
import {
  detachExactInTx,
  entitledIdsInTx,
  healDetachmentsInTx,
  reattachInTx,
} from "@/lib/db/detach.server";
import { auditInTx, mintChannelId } from "@/lib/db/identity.server";
import { type Db, getDb, isUniqueViolation } from "@/lib/db/index.server";
import { personDisplaySql } from "@/lib/db/person-display.server";
import {
  bundle,
  channel,
  channelBundle,
  channelMember,
  channelOptout,
  seat,
} from "@/lib/db/schema.app";
import { user } from "@/lib/db/schema.auth";

/**
 * The CHANNELS data access layer — reads over the app's own channel tables, the existence
 * ceremonies (create / rename / delete) as plain transactions, and the ONE curation core
 * (place / unplace a bundle reference) that this file's id-keyed web functions AND the device
 * lane's name-keyed ones both run. Every function is actor-first and derives its workspace
 * FROM the actor, so a caller that skipped its guard cannot compile and a wrong-scope read
 * never leaks.
 *
 * A channel is plain rows: a named group holding bundle REFERENCES (labels — one bundle,
 * delivered once) and seat-anchored memberships. The DEFAULT channel ('everyone') has IMPLICIT
 * membership: every seat, minus explicit self opt-outs (channel_optout) — it holds no
 * channel_member rows, and rename/delete refuse it. Channel names use the same charset as
 * bundle names; audit rows ride every existence/mode write (subject = the immutable channel id).
 */

/** The channel-name rule (the old birth mint's bound, kept). */
const CHANNEL_NAME = /^[a-z0-9][a-z0-9-]*$/;

/**
 * Names a channel can never take: URL segments the channel surface itself claims. `new` is the
 * create form's own route (`channels/new`), which React Router ranks above the dynamic
 * `channels/:channel` face — a channel named `new` would be creatable but its page unreachable.
 */
const CHANNEL_RESERVED = new Set(["new"]);
const CHANNEL_NAME_MAX = 64;

/** One channel as the index renders it: identity + mode + the two counts. */
export interface ChannelSummary {
  channelId: string;
  name: string;
  mode: "open" | "curated";
  /** The default channel — its membership is the roster minus opt-outs, not rows. */
  isDefault: boolean;
  /** Distinct bundle references the channel holds. */
  skillCount: number;
  /** People the channel reaches (seats − opt-outs for the default; member rows otherwise). */
  memberCount: number;
}

/** The seat count — the default channel's structural base membership. */
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
 * bundle-reference count and its member count.
 */
export async function channelsOf(actor: MemberActor): Promise<ChannelSummary[]> {
  const ws = actor.workspaceId;
  const [channels, skillCounts, memberCounts, optoutCounts, seats] = await Promise.all([
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
      .select({ channelId: channelMember.channelId, n: count() })
      .from(channelMember)
      .where(eq(channelMember.workspaceId, ws))
      .groupBy(channelMember.channelId),
    getDb()
      .select({ channelId: channelOptout.channelId, n: count() })
      .from(channelOptout)
      .where(eq(channelOptout.workspaceId, ws))
      .groupBy(channelOptout.channelId),
    seatCount(ws),
  ]);
  const skills = new Map(skillCounts.map((c) => [c.channelId, c.n]));
  const members = new Map(memberCounts.map((c) => [c.channelId, c.n]));
  const optouts = new Map(optoutCounts.map((c) => [c.channelId, c.n]));
  return channels.map((ch) => ({
    channelId: ch.id,
    name: ch.name,
    mode: ch.mode as ChannelSummary["mode"],
    isDefault: ch.isDefault,
    skillCount: skills.get(ch.id) ?? 0,
    memberCount: ch.isDefault
      ? Math.max(0, seats - (optouts.get(ch.id) ?? 0))
      : (members.get(ch.id) ?? 0),
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

/** One membership row of a NAMED channel (the default channel derives instead). */
export interface ChannelMemberRef {
  userId: string;
  display: string;
  addedBy: string | null;
  addedAt: Date;
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
  /** The explicit membership rows (always [] for the default channel — it derives). */
  members: ChannelMemberRef[];
  /** What the default channel reaches: seats − opt-outs. */
  defaultMemberCount: number;
  /** THIS member's stance: in a named channel, a member row; in the default, no opt-out row. */
  viewerIsMember: boolean;
}

/**
 * One channel's full read: the row, its bundle references (joined to the catalog), its
 * members, and the VIEWER's own stance (the default channel's self-service leave/rejoin arm
 * renders from it). Undefined when the channel does not exist (the route renders the 404).
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
  const [skills, members, optoutRows, seats, viewerRows] = await Promise.all([
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
    row.isDefault
      ? Promise.resolve([])
      : getDb()
          .select({
            userId: channelMember.userId,
            display: personDisplaySql(user),
            addedBy: channelMember.addedBy,
            addedAt: channelMember.createdAt,
          })
          .from(channelMember)
          .innerJoin(user, eq(user.id, channelMember.userId))
          .where(and(eq(channelMember.workspaceId, ws), eq(channelMember.channelId, row.id)))
          .orderBy(asc(channelMember.createdAt), asc(channelMember.userId)),
    getDb()
      .select({ userId: channelOptout.userId })
      .from(channelOptout)
      .where(and(eq(channelOptout.workspaceId, ws), eq(channelOptout.channelId, row.id))),
    seatCount(ws),
    row.isDefault
      ? Promise.resolve([])
      : getDb()
          .select({ userId: channelMember.userId })
          .from(channelMember)
          .where(
            and(
              eq(channelMember.workspaceId, ws),
              eq(channelMember.channelId, row.id),
              eq(channelMember.userId, actor.userId),
            ),
          )
          .limit(1),
  ]);
  const viewerOptedOut = optoutRows.some((o) => o.userId === actor.userId);
  return {
    channelId: row.id,
    name: row.name,
    mode: row.mode as ChannelDetail["mode"],
    isDefault: row.isDefault,
    createdBy: row.createdBy,
    createdAt: row.createdAt,
    skills: skills.map((s) => ({ ...s, status: s.status as ChannelSkillRef["status"] })),
    members,
    defaultMemberCount: Math.max(0, seats - optoutRows.length),
    viewerIsMember: row.isDefault ? !viewerOptedOut : viewerRows.length > 0,
  };
}

// ── Existence admin (create / rename / delete) ──────────────────────────────────────────────

export type ChannelCreateOutcome =
  | { outcome: "created"; channelId: string }
  | { outcome: "name_taken" }
  | { outcome: "bad_name" };

/**
 * Create a named channel. The unique index is the race arbiter: a create-race loser maps to
 * the honest `name_taken`, never a 500. Member-level — the same grade as the device lane's
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
 * channel typed. References, memberships, and the audit trail survive; only the display name
 * moves (no hint table for channels — a channel name is a grouping label, not a distribution
 * address a device pins).
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
 * References and memberships CASCADE with the row; deliberately NO person-detach records — a
 * channel deletion is an upstream withdrawal, never a person's own detach; bundles another
 * channel or a direct follow still delivers keep flowing. The audit row keeps the channel id:
 * history is append-only and survives the row.
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
 * The actor shape BOTH curation doors satisfy: the web page's MemberActor and the device
 * lane's DeviceActor (whose deviceId rides into the audit row when present). The policy —
 * gates, idempotence, healing, audit — is written ONCE against this shape so the two lanes
 * cannot drift; the branded outer functions stay the only entry points.
 */
interface CurationActor {
  readonly userId: string;
  readonly display: string;
  readonly workspaceId: string;
  readonly role: "owner" | "reviewer" | "member";
  readonly deviceId?: string;
}

/** The catalog probe every curation door gates on FIRST: NULL = no such bundle in this
 * workspace (checked before any channel resolution, so a bad skill never mints a channel
 * on the device lane and the two doors refuse in the same order). */
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
 * device lane by NAME with create-on-first-use; the web page by ID): the curated-mode role
 * gate (reviewer+), the idempotent reference insert (ON CONFLICT — a re-place answers
 * 'placed' again), the bundle-scoped detachment heal (anyone re-entitled through this
 * placement self-heals), and the `skill_added` audit row — emitted for the ACT, a re-place of
 * a standing row included.
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
  await healDetachmentsInTx(tx, actor.workspaceId, bundleId);
  await auditInTx(tx, {
    workspaceId: actor.workspaceId,
    actor: { userId: actor.userId, deviceId: actor.deviceId, display: actor.display },
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
    actor: { userId: actor.userId, deviceId: actor.deviceId, display: actor.display },
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
 * the device lane shares: same gates (the bundle must exist in-workspace and be active; a
 * CURATED channel takes reviewer+), same idempotence, same detachment healing, same audit
 * row. Keyed on the IMMUTABLE channel id like every web ceremony — the page operates on an
 * existing channel, so there is NO create-on-first-use here: an id that does not resolve in
 * the actor's workspace is the honest unknown_channel, never a mint.
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

// ── The default channel's self-service opt-out (the ONE negative membership row) ───────────

/**
 * Leave the DEFAULT channel — a personal act: insert the opt-out row, then write detach
 * records (cause 'channel_leave') for exactly the bundles this leave lapsed (before − after
 * over the entitlement union, computed inside the one transaction).
 */
export async function optOutDefaultChannel(actor: MemberActor): Promise<"left" | "not_member"> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const rows = await tx
      .select({ id: channel.id })
      .from(channel)
      .where(and(eq(channel.workspaceId, ws), eq(channel.isDefault, true)))
      .limit(1);
    const def = rows[0];
    if (def === undefined) {
      return "not_member";
    }
    const before = await entitledIdsInTx(tx, ws, actor.userId);
    const inserted = await tx
      .insert(channelOptout)
      .values({ channelId: def.id, workspaceId: ws, userId: actor.userId })
      .onConflictDoNothing()
      .returning({ userId: channelOptout.userId });
    if (inserted.length === 0) {
      return "not_member";
    }
    const after = new Set(await entitledIdsInTx(tx, ws, actor.userId));
    await detachExactInTx(
      tx,
      ws,
      actor.userId,
      before.filter((id) => !after.has(id)),
      "channel_leave",
    );
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, display: actor.display },
      kind: "member_left",
      subject: def.id,
      outcome: "ok",
      details: { userId: actor.userId },
    });
    return "left";
  });
}

/** Rejoin the DEFAULT channel: delete the opt-out + clear the re-entitled detach records. */
export async function optInDefaultChannel(actor: MemberActor): Promise<"joined"> {
  const ws = actor.workspaceId;
  await getDb().transaction(async (tx) => {
    const rows = await tx
      .select({ id: channel.id })
      .from(channel)
      .where(and(eq(channel.workspaceId, ws), eq(channel.isDefault, true)))
      .limit(1);
    const def = rows[0];
    if (def === undefined) {
      return;
    }
    const deleted = await tx
      .delete(channelOptout)
      .where(
        and(
          eq(channelOptout.workspaceId, ws),
          eq(channelOptout.channelId, def.id),
          eq(channelOptout.userId, actor.userId),
        ),
      )
      .returning({ userId: channelOptout.userId });
    if (deleted.length === 0) {
      return;
    }
    await reattachInTx(tx, ws, actor.userId);
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, display: actor.display },
      kind: "member_joined",
      subject: def.id,
      outcome: "ok",
      details: { userId: actor.userId },
    });
  });
  return "joined";
}
