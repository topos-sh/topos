import { and, asc, count, eq, sql } from "drizzle-orm";
import { alias } from "drizzle-orm/pg-core";
import type { DeviceActor } from "@/lib/auth/guards.server";
import {
  detachExactInTx,
  entitledIdsInTx,
  pgTextArray,
  reattachInTx,
} from "@/lib/db/detach.server";
import {
  auditInTx,
  entitledBundlesSql,
  mintChannelId,
  mintInvitationId,
  mintInviteToken,
  supersedeDeclinedInvitationTx,
} from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import { personDisplayLeftSql } from "@/lib/db/person-display.server";
import {
  bundleStatusInTx,
  placeBundleRefInTx,
  unplaceBundleRefInTx,
} from "@/lib/db/queries.channels.server";
import { foldInviteEmail, INVITATION_TTL_MS } from "@/lib/db/queries.roster.server";
import {
  bundle,
  bundleSubscription,
  channel,
  channelMember,
  channelOptout,
  deviceExclusion,
  invitation,
  notice,
  proposal,
  seat,
  workspace,
} from "@/lib/db/schema.app";
import { user } from "@/lib/db/schema.auth";
import { planeCurrentPointer, planeVersionDigest } from "@/lib/db/schema.custody";

/**
 * The DEVICE lane's data access — the row-op half of `/api/v1`, served entirely by this tier
 * since the identity unification (the guarded SQL functions are gone; the policy logic lives
 * HERE, once). Every op takes the branded `DeviceActor` (minted only by requireDeviceActor)
 * and passes the actor's server-resolved person/device — never anything client-asserted beyond
 * the credential itself. Role gates read the actor's seat role; existence misses answer the
 * same status vocabulary the old functions spoke so the wire mapping stays byte-shaped.
 *
 * Multi-read answers (delivery, the channels index) run inside ONE REPEATABLE READ transaction
 * — one snapshot, so the entitled/detached/notices sets can never straddle a subscription
 * change (a bundle in NEITHER list would read as an upstream withdrawal and get cleaned).
 */

const CHANNEL_NAME = /^[a-z0-9][a-z0-9-]*$/;
const CHANNEL_NAME_MAX = 64;

type Tx = Parameters<Parameters<ReturnType<typeof getDb>["transaction"]>[0]>[0];

// ── Delivery ─────────────────────────────────────────────────────────────────────────────────

export interface DeliverySkill {
  skill_id: string;
  name: string;
  kind: string;
  display_name?: string;
  protection: string;
  version_id: string;
  bundle_digest: string;
  generation: number;
  updated_at: number;
  via: { channels: string[]; direct: boolean };
}

export interface DeliveryNotice {
  id: string;
  kind: string;
  skill_id?: string;
  skill_name?: string;
  version_id?: string;
  actor?: string;
  outcome?: string;
  reason?: string;
  message?: string;
  created_at: string;
}

/** The complete `WireDelivery` body (the route serializes it verbatim). */
export interface DeliveryBody {
  schema_version: 1;
  workspace_id: string;
  /** The device↔workspace link's status; "pending" delivers NOTHING (the empty body below). */
  link_status: "active" | "pending";
  skills: DeliverySkill[];
  detached: string[];
  excluded?: string[];
  notices: DeliveryNotice[];
  proposals_awaiting: number;
  staleness_window_ms: number;
}

/**
 * The PENDING link's delivery: shape-complete and EMPTY — no data flows over a pending link
 * (skills/detached/excluded/notices empty, zero proposals), but the staleness clock still
 * serves so the client's freshness bookkeeping stays honest while it waits for approval.
 */
export async function emptyDeliveryFor(actor: DeviceActor): Promise<DeliveryBody> {
  const wsRows = await getDb()
    .select({ stalenessWindowMs: workspace.stalenessWindowMs })
    .from(workspace)
    .where(eq(workspace.id, actor.workspaceId))
    .limit(1);
  return {
    schema_version: 1,
    workspace_id: actor.workspaceId,
    link_status: "pending",
    skills: [],
    detached: [],
    notices: [],
    proposals_awaiting: 0,
    staleness_window_ms: wsRows[0]?.stalenessWindowMs ?? 604800000,
  };
}

/** RFC-3339 seconds + Z (the wire's timestamp spelling). */
function isoSeconds(date: Date): string {
  return date.toISOString().replace(/\.\d{3}Z$/, "Z");
}

/**
 * The currency answer for ONE enrolled device: the entitled skills (channel union ∪ direct
 * follows − unfollows − this device's exclusions, active + current-holding only, with `via`
 * attribution and the resolved protection), the person's detached set, this device's
 * exclusions, the unacked notices, the open-proposal count over the entitled set, and the ONE
 * staleness clock.
 */
export async function deliveryFor(actor: DeviceActor): Promise<DeliveryBody> {
  const ws = actor.workspaceId;
  return await getDb().transaction(
    async (tx) => {
      const skillRows = await tx.execute(sql`
        SELECT b.id AS skill_id, b.name, b.kind, b.display_name,
               COALESCE(b.protection, w.protection_default, 'open') AS protection,
               cp.version_id, cp.generation,
               (extract(epoch from cp.moved_at) * 1000)::bigint AS updated_at,
               vd.bundle_digest,
               COALESCE((
                 SELECT array_agg(ch.name ORDER BY ch.name)
                 FROM web.channel_bundle cb
                 JOIN web.channel ch ON ch.id = cb.channel_id
                 WHERE cb.workspace_id = ${ws} AND cb.bundle_id = b.id
                   AND (
                     (ch.is_default AND NOT EXISTS (
                        SELECT 1 FROM web.channel_optout o
                        WHERE o.channel_id = ch.id AND o.user_id = ${actor.userId}))
                     OR EXISTS (
                        SELECT 1 FROM web.channel_member cm
                        WHERE cm.channel_id = ch.id AND cm.user_id = ${actor.userId})
                   )
               ), '{}') AS via_channels,
               EXISTS (
                 SELECT 1 FROM web.bundle_subscription bs
                 WHERE bs.user_id = ${actor.userId} AND bs.bundle_id = b.id
                   AND bs.state = 'following'
               ) AS direct
        FROM (${entitledBundlesSql(actor.userId, ws)}) e
        JOIN web.bundle b ON b.id = e.bundle_id
        JOIN web.workspace w ON w.id = ${ws}
        JOIN plane.current_pointer cp ON cp.workspace_id = ${ws} AND cp.bundle_id = b.id
        LEFT JOIN plane.version_digest vd
          ON vd.workspace_id = ${ws} AND vd.bundle_id = b.id AND vd.version_id = cp.version_id
        WHERE NOT EXISTS (
          SELECT 1 FROM web.device_exclusion dx
          WHERE dx.device_id = ${actor.deviceId} AND dx.bundle_id = b.id
        )
        ORDER BY b.name
      `);
      const skills: DeliverySkill[] = (skillRows.rows as Record<string, unknown>[]).map((r) => ({
        skill_id: r.skill_id as string,
        name: r.name as string,
        kind: r.kind as string,
        ...(r.display_name === null ? {} : { display_name: r.display_name as string }),
        protection: r.protection as string,
        version_id: r.version_id as string,
        // A pointer without its digest row is a custody fault; serve the honest empty string
        // rather than fail the whole delivery (the client's re-hash will refuse the bundle).
        bundle_digest: (r.bundle_digest as string | null) ?? "",
        generation: Number(r.generation),
        updated_at: Number(r.updated_at),
        via: { channels: r.via_channels as string[], direct: r.direct as boolean },
      }));

      // Detached: the person's own lapse records (plus standing unfollow stances), minus
      // anything re-entitled — every device freezes these in place, never cleaning them.
      const detachedRows = await tx.execute(sql`
        SELECT d.bundle_id FROM (
          SELECT bd.bundle_id FROM web.bundle_detachment bd
          WHERE bd.workspace_id = ${ws} AND bd.user_id = ${actor.userId}
          UNION
          SELECT bs.bundle_id FROM web.bundle_subscription bs
          WHERE bs.workspace_id = ${ws} AND bs.user_id = ${actor.userId}
            AND bs.state = 'unfollowed'
        ) d
        WHERE d.bundle_id NOT IN (${entitledBundlesSql(actor.userId, ws)})
        ORDER BY d.bundle_id
      `);
      const detached = (detachedRows.rows as { bundle_id: string }[]).map((r) => r.bundle_id);

      const excludedRows = await tx.execute(sql`
        SELECT dx.bundle_id FROM web.device_exclusion dx
        JOIN web.bundle b ON b.id = dx.bundle_id
        WHERE dx.device_id = ${actor.deviceId} AND b.status = 'active'
        ORDER BY dx.bundle_id
      `);
      const excluded = (excludedRows.rows as { bundle_id: string }[]).map((r) => r.bundle_id);

      const noticeRows = await tx.execute(sql`
        SELECT n.id, n.kind, n.payload, n.created_at, b.name AS live_name
        FROM web.notice n
        LEFT JOIN web.bundle b ON b.id = (n.payload ->> 'skill_id')
        WHERE n.workspace_id = ${ws} AND n.user_id = ${actor.userId} AND n.acked_at IS NULL
        ORDER BY n.created_at, n.id
      `);
      const notices: DeliveryNotice[] = (noticeRows.rows as Record<string, unknown>[]).map((r) => {
        const payload = (r.payload ?? {}) as Record<string, unknown>;
        const out: DeliveryNotice = {
          id: String(r.id),
          kind: r.kind as string,
          created_at: isoSeconds(new Date(r.created_at as string)),
        };
        for (const key of [
          "skill_id",
          "version_id",
          "actor",
          "outcome",
          "reason",
          "message",
        ] as const) {
          const value = payload[key];
          if (typeof value === "string" && value.length > 0) {
            out[key] = value;
          }
        }
        // The live catalog name outranks the payload snapshot (joined for narration).
        const liveName = r.live_name as string | null;
        const snapName = payload.skill_name;
        if (liveName !== null) {
          out.skill_name = liveName;
        } else if (typeof snapName === "string" && snapName.length > 0) {
          out.skill_name = snapName;
        }
        return out;
      });

      const proposalRows = await tx.execute(sql`
        SELECT COUNT(*) AS n FROM web.proposal p
        WHERE p.workspace_id = ${ws} AND p.status = 'open'
          AND p.bundle_id IN (${entitledBundlesSql(actor.userId, ws)})
      `);
      const proposalsAwaiting = Number((proposalRows.rows[0] as { n: string | number }).n);

      const wsRows = await tx
        .select({ stalenessWindowMs: workspace.stalenessWindowMs })
        .from(workspace)
        .where(eq(workspace.id, ws))
        .limit(1);

      const body: DeliveryBody = {
        schema_version: 1,
        workspace_id: ws,
        link_status: "active",
        skills,
        detached,
        ...(excluded.length > 0 ? { excluded } : {}),
        notices,
        proposals_awaiting: proposalsAwaiting,
        staleness_window_ms: wsRows[0]?.stalenessWindowMs ?? 604800000,
      };
      return body;
    },
    { isolationLevel: "repeatable read", accessMode: "read only" },
  );
}

// ── The applied-state report ─────────────────────────────────────────────────────────────────

/**
 * The fleet's applied-state report: UPSERT this device's (bundle, applied version) rows —
 * NEVER delete (a row whose bundle left the install set is the frozen "last known" record the
 * fleet derives blind spots from). A report is CLIENT-ASSERTED data, so every named bundle is
 * re-checked against the server's own entitlement predicate: only truly-delivered bundles are
 * recorded.
 */
export async function reportApplied(
  actor: DeviceActor,
  applied: { skillId: string; versionId: string }[],
): Promise<"ok"> {
  const ws = actor.workspaceId;
  const skillIds = pgTextArray(applied.map((a) => a.skillId));
  const versionIds = pgTextArray(applied.map((a) => a.versionId));
  await getDb().execute(sql`
    INSERT INTO web.device_bundle_state (device_id, bundle_id, applied_version_id, reported_at)
    SELECT ${actor.deviceId}, r.skill_id, r.version_id, now()
    FROM UNNEST(${skillIds}::text[], ${versionIds}::text[]) AS r(skill_id, version_id)
    JOIN (${entitledBundlesSql(actor.userId, ws)}) e ON e.bundle_id = r.skill_id
    ON CONFLICT (device_id, bundle_id) DO UPDATE
      SET applied_version_id = excluded.applied_version_id, reported_at = excluded.reported_at
  `);
  return "ok";
}

// ── The describe reads (me / channels / reach) ──────────────────────────────────────────────

export interface LaneMe {
  name: string;
  displayName: string;
  role: string;
  /** The inviter's login address, when the seat records one (display attribution only). */
  invitedBy: string | null;
}

/** The caller's own membership facts (`GET /me`). */
export async function laneMe(actor: DeviceActor): Promise<LaneMe | null> {
  const rows = await getDb().execute(sql`
    SELECT w.name, w.display_name, s.role, iu.email AS invited_by
    FROM web.workspace w
    JOIN web.seat s ON s.workspace_id = w.id AND s.user_id = ${actor.userId}
    LEFT JOIN web."user" iu ON iu.id = s.invited_by
    WHERE w.id = ${actor.workspaceId}
  `);
  const row = rows.rows[0] as
    | {
        name: string;
        display_name: string;
        role: string;
        invited_by: string | null;
      }
    | undefined;
  if (row === undefined) {
    return null;
  }
  return {
    name: row.name,
    displayName: row.display_name,
    role: row.role,
    invitedBy: row.invited_by,
  };
}

export interface LaneChannel {
  name: string;
  mode: string;
  builtin: boolean;
  member: boolean;
  memberCount: number;
  skills: { skillId: string; name: string }[];
}

/** The workspace channels index (`GET /channels`) — name-sorted, the default included. */
export async function laneChannels(actor: DeviceActor): Promise<LaneChannel[]> {
  const ws = actor.workspaceId;
  return await getDb().transaction(
    async (tx) => {
      const skillRows = await tx.execute(sql`
        SELECT cb.channel_id, cb.bundle_id, b.name
        FROM web.channel_bundle cb
        JOIN web.bundle b ON b.id = cb.bundle_id
        WHERE cb.workspace_id = ${ws}
        ORDER BY b.name
      `);
      const byChannel = new Map<string, { skillId: string; name: string }[]>();
      for (const raw of skillRows.rows as {
        channel_id: string;
        bundle_id: string;
        name: string;
      }[]) {
        const list = byChannel.get(raw.channel_id) ?? [];
        list.push({ skillId: raw.bundle_id, name: raw.name });
        byChannel.set(raw.channel_id, list);
      }
      const channelRows = await tx.execute(sql`
        SELECT ch.id, ch.name, ch.mode, ch.is_default,
          (CASE WHEN ch.is_default
                THEN NOT EXISTS (SELECT 1 FROM web.channel_optout o
                                 WHERE o.channel_id = ch.id AND o.user_id = ${actor.userId})
                ELSE EXISTS (SELECT 1 FROM web.channel_member cm
                             WHERE cm.channel_id = ch.id AND cm.user_id = ${actor.userId})
           END) AS member,
          (CASE WHEN ch.is_default
                THEN (SELECT COUNT(*) FROM web.seat s WHERE s.workspace_id = ch.workspace_id)
                     - (SELECT COUNT(*) FROM web.channel_optout o WHERE o.channel_id = ch.id)
                ELSE (SELECT COUNT(*) FROM web.channel_member cm WHERE cm.channel_id = ch.id)
           END) AS member_count
        FROM web.channel ch
        WHERE ch.workspace_id = ${ws}
        ORDER BY ch.name
      `);
      return (channelRows.rows as Record<string, unknown>[]).map((r) => ({
        name: r.name as string,
        mode: r.mode as string,
        builtin: r.is_default as boolean,
        member: r.member as boolean,
        memberCount: Number(r.member_count),
        skills: byChannel.get(r.id as string) ?? [],
      }));
    },
    { isolationLevel: "repeatable read", accessMode: "read only" },
  );
}

/** A bundle's audience (`GET /skills/{skill}/reach`): entitled persons + their live devices. */
export async function laneReach(
  actor: DeviceActor,
  bundleId: string,
): Promise<{ persons: number; devices: number } | null> {
  const ws = actor.workspaceId;
  const db = getDb();
  const exists = await db
    .select({ id: bundle.id })
    .from(bundle)
    .where(and(eq(bundle.workspaceId, ws), eq(bundle.id, bundleId)))
    .limit(1);
  if (exists.length === 0) {
    return null;
  }
  const persons = await db.execute(sql`
    SELECT COUNT(*) AS n FROM web.seat s
    WHERE s.workspace_id = ${ws}
      AND EXISTS (SELECT 1 FROM (${entitledBundlesSql(sql`s.user_id`, ws)}) e
                  WHERE e.bundle_id = ${bundleId})
  `);
  const devices = await db.execute(sql`
    SELECT COUNT(*) AS n FROM web.device d
    JOIN web.seat s ON s.workspace_id = ${ws} AND s.user_id = d.user_id
    WHERE d.revoked_at IS NULL
      AND EXISTS (SELECT 1 FROM (${entitledBundlesSql(sql`d.user_id`, ws)}) e
                  WHERE e.bundle_id = ${bundleId})
  `);
  return {
    persons: Number((persons.rows[0] as { n: string | number }).n),
    devices: Number((devices.rows[0] as { n: string | number }).n),
  };
}

// ── Subscriptions (the ONE stance row) + exclusions ─────────────────────────────────────────

/**
 * Direct-follow: upsert the ONE stance row to 'following', clear THIS device's exclusion (the
 * device fence is construction — the actor's own resolved device id is the only one named),
 * and clear the person's re-entitled detach records. Archived bundles refuse (a freed name is
 * a NEW identity; the old one is out of circulation).
 */
export async function followBundle(
  actor: DeviceActor,
  bundleId: string,
): Promise<"followed" | "unknown_skill" | "skill_not_active"> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const status = await bundleStatusInTx(tx, ws, bundleId);
    if (status === null) {
      return "unknown_skill";
    }
    if (status !== "active") {
      return "skill_not_active";
    }
    await tx
      .insert(bundleSubscription)
      .values({ userId: actor.userId, workspaceId: ws, bundleId, state: "following" })
      .onConflictDoUpdate({
        target: [bundleSubscription.userId, bundleSubscription.bundleId],
        set: { state: "following", updatedAt: new Date() },
      });
    await tx
      .delete(deviceExclusion)
      .where(
        and(eq(deviceExclusion.deviceId, actor.deviceId), eq(deviceExclusion.bundleId, bundleId)),
      );
    await reattachInTx(tx, ws, actor.userId);
    return "followed";
  });
}

/**
 * Unfollow: the stance flips to 'unfollowed' (the negative mask — delivery ends on ALL the
 * person's devices, whatever channels still reference it) + the detach records for exactly
 * what this unfollow lapsed. `follow` re-attaches.
 */
export async function unfollowBundle(
  actor: DeviceActor,
  bundleId: string,
): Promise<"unfollowed" | "unknown_skill"> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const status = await bundleStatusInTx(tx, ws, bundleId);
    if (status === null) {
      return "unknown_skill";
    }
    const before = await entitledIdsInTx(tx, ws, actor.userId);
    await tx
      .insert(bundleSubscription)
      .values({ userId: actor.userId, workspaceId: ws, bundleId, state: "unfollowed" })
      .onConflictDoUpdate({
        target: [bundleSubscription.userId, bundleSubscription.bundleId],
        set: { state: "unfollowed", updatedAt: new Date() },
      });
    const after = new Set(await entitledIdsInTx(tx, ws, actor.userId));
    await detachExactInTx(
      tx,
      ws,
      actor.userId,
      before.filter((id) => !after.has(id)),
      "unfollow",
    );
    return "unfollowed";
  });
}

/** Exclude a bundle from THIS device (the `remove` verb's row; `follow` here lifts it). */
export async function excludeOnDevice(
  actor: DeviceActor,
  bundleId: string,
): Promise<"excluded" | "unknown_skill"> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const status = await bundleStatusInTx(tx, ws, bundleId);
    if (status === null) {
      return "unknown_skill";
    }
    await tx
      .insert(deviceExclusion)
      .values({ deviceId: actor.deviceId, bundleId })
      .onConflictDoNothing();
    return "excluded";
  });
}

// ── Channel membership (join / leave, the default channel included) ─────────────────────────

async function channelByNameInTx(tx: Tx, ws: string, name: string) {
  const rows = await tx
    .select({ id: channel.id, isDefault: channel.isDefault, mode: channel.mode })
    .from(channel)
    .where(and(eq(channel.workspaceId, ws), eq(channel.name, name)))
    .limit(1);
  return rows[0];
}

/**
 * Join a channel by name. A NAMED channel gets a membership row; the DEFAULT channel's join
 * is the opt-out row's deletion (membership there is implicit). Both clear the re-entitled
 * detach records.
 */
export async function laneChannelJoin(
  actor: DeviceActor,
  channelName: string,
): Promise<"joined" | "unknown_channel"> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const row = await channelByNameInTx(tx, ws, channelName);
    if (row === undefined) {
      return "unknown_channel";
    }
    if (row.isDefault) {
      await tx
        .delete(channelOptout)
        .where(
          and(
            eq(channelOptout.workspaceId, ws),
            eq(channelOptout.channelId, row.id),
            eq(channelOptout.userId, actor.userId),
          ),
        );
    } else {
      await tx
        .insert(channelMember)
        .values({ channelId: row.id, workspaceId: ws, userId: actor.userId })
        .onConflictDoNothing();
    }
    await reattachInTx(tx, ws, actor.userId);
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, deviceId: actor.deviceId, display: actor.display },
      kind: "member_joined",
      subject: row.id,
      outcome: "ok",
      details: { userId: actor.userId },
    });
    return "joined";
  });
}

/**
 * Leave a channel by name. A NAMED channel's leave deletes the membership row; the DEFAULT
 * channel's leave INSERTS the opt-out row (the one negative membership row). Both write
 * detach records (cause 'channel_leave') for exactly what the leave lapsed.
 */
export async function laneChannelLeave(
  actor: DeviceActor,
  channelName: string,
): Promise<"left" | "not_member" | "unknown_channel"> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const row = await channelByNameInTx(tx, ws, channelName);
    if (row === undefined) {
      return "unknown_channel";
    }
    const before = await entitledIdsInTx(tx, ws, actor.userId);
    if (row.isDefault) {
      const inserted = await tx
        .insert(channelOptout)
        .values({ channelId: row.id, workspaceId: ws, userId: actor.userId })
        .onConflictDoNothing()
        .returning({ userId: channelOptout.userId });
      if (inserted.length === 0) {
        return "not_member";
      }
    } else {
      const deleted = await tx
        .delete(channelMember)
        .where(
          and(
            eq(channelMember.workspaceId, ws),
            eq(channelMember.channelId, row.id),
            eq(channelMember.userId, actor.userId),
          ),
        )
        .returning({ userId: channelMember.userId });
      if (deleted.length === 0) {
        return "not_member";
      }
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
      actor: { userId: actor.userId, deviceId: actor.deviceId, display: actor.display },
      kind: "member_left",
      subject: row.id,
      outcome: "ok",
      details: { userId: actor.userId },
    });
    return "left";
  });
}

// ── Curation (place / unplace, create-on-first-use) ─────────────────────────────────────────

/**
 * Place a bundle reference into a channel — creating the channel on FIRST use (member-level).
 * Everything past the name resolution is the ONE curation core shared with the web page's
 * id-keyed functions (queries.channels.server.ts): the bundle-active gate, the CURATED
 * channel's reviewer+ gate (symmetric with removal), the idempotent insert, the detachment
 * heal, and the audit row. The create-race loser places into the winner's row ('placed',
 * never a raw conflict): ids are minted randomly, so only the name unique can collide, and a
 * re-select resolves it.
 */
export async function lanePlaceBundle(
  actor: DeviceActor,
  channelName: string,
  bundleId: string,
): Promise<
  "placed" | "created" | "bad_name" | "unknown_skill" | "skill_not_active" | "curated_role_required"
> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const status = await bundleStatusInTx(tx, ws, bundleId);
    if (status === null) {
      return "unknown_skill";
    }
    if (status !== "active") {
      return "skill_not_active";
    }
    let row = await channelByNameInTx(tx, ws, channelName);
    let created = false;
    if (row === undefined) {
      if (!CHANNEL_NAME.test(channelName) || channelName.length > CHANNEL_NAME_MAX) {
        return "bad_name";
      }
      // ON CONFLICT (never try/catch): a unique violation would ABORT this whole transaction —
      // Postgres refuses every later statement — so the race must be absorbed without raising.
      // Ids are minted randomly, so only the name unique can collide.
      const id = mintChannelId();
      const inserted = await tx
        .insert(channel)
        .values({ id, workspaceId: ws, name: channelName, createdBy: actor.userId })
        .onConflictDoNothing({ target: [channel.workspaceId, channel.name] })
        .returning({ id: channel.id });
      if (inserted.length > 0) {
        row = { id, isDefault: false, mode: "open" };
        created = true;
        await auditInTx(tx, {
          workspaceId: ws,
          actor: { userId: actor.userId, deviceId: actor.deviceId, display: actor.display },
          kind: "channel_created",
          subject: id,
          outcome: "ok",
          details: { name: channelName },
        });
      } else {
        // The race loser: the name landed under someone else's insert — place into theirs.
        row = await channelByNameInTx(tx, ws, channelName);
        if (row === undefined) {
          return "bad_name";
        }
      }
    }
    const placed = await placeBundleRefInTx(tx, actor, row, bundleId);
    if (placed !== "placed") {
      return placed;
    }
    return created ? "created" : "placed";
  });
}

/** Remove a bundle reference from a channel — symmetric gate with place, the shared core. */
export async function laneUnplaceBundle(
  actor: DeviceActor,
  channelName: string,
  bundleId: string,
): Promise<"removed" | "not_placed" | "unknown_channel" | "curated_role_required"> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const row = await channelByNameInTx(tx, ws, channelName);
    if (row === undefined) {
      return "unknown_channel";
    }
    return await unplaceBundleRefInTx(tx, actor, row, bundleId);
  });
}

// ── Protection setters ───────────────────────────────────────────────────────────────────────

/** Tightening takes reviewer+; loosening back to open widens what members can do — owner. */
function protectionRoleGate(
  role: DeviceActor["role"],
  tightens: boolean,
): "owner_role_required" | "reviewer_role_required" | null {
  if (tightens) {
    return role === "member" ? "reviewer_role_required" : null;
  }
  return role === "owner" ? null : "owner_role_required";
}

/** Pin a bundle's protection level (`open` | `reviewed`; the route validated the value). */
export async function laneProtectBundle(
  actor: DeviceActor,
  bundleId: string,
  level: "open" | "reviewed",
): Promise<"set" | "unknown_skill" | "owner_role_required" | "reviewer_role_required"> {
  const gate = protectionRoleGate(actor.role, level === "reviewed");
  if (gate !== null) {
    return gate;
  }
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const updated = await tx
      .update(bundle)
      .set({ protection: level })
      .where(and(eq(bundle.workspaceId, ws), eq(bundle.id, bundleId), eq(bundle.status, "active")))
      .returning({ id: bundle.id });
    if (updated.length === 0) {
      return "unknown_skill";
    }
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, deviceId: actor.deviceId, display: actor.display },
      kind: "protect_skill",
      subject: bundleId,
      outcome: "ok",
      details: { level },
    });
    return "set";
  });
}

/** Set a channel's mode (`open` | `curated`; the route validated the value). */
export async function laneProtectChannel(
  actor: DeviceActor,
  channelName: string,
  mode: "open" | "curated",
): Promise<"set" | "unknown_channel" | "owner_role_required" | "reviewer_role_required"> {
  const gate = protectionRoleGate(actor.role, mode === "curated");
  if (gate !== null) {
    return gate;
  }
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const updated = await tx
      .update(channel)
      .set({ mode })
      .where(and(eq(channel.workspaceId, ws), eq(channel.name, channelName)))
      .returning({ id: channel.id });
    if (updated.length === 0) {
      return "unknown_channel";
    }
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, deviceId: actor.deviceId, display: actor.display },
      kind: `mode_${mode}`,
      subject: updated[0]?.id ?? channelName,
      outcome: "ok",
    });
    return "set";
  });
}

// ── Notices ack ──────────────────────────────────────────────────────────────────────────────

/** Mark the caller's own notices read by id — idempotent; unknown ids are ignored. */
export async function laneAckNotices(actor: DeviceActor, ids: string[]): Promise<"acked"> {
  const numeric = ids.map((id) => Number(id)).filter((n) => Number.isSafeInteger(n));
  if (numeric.length === 0) {
    return "acked";
  }
  await getDb()
    .update(notice)
    .set({ ackedAt: new Date() })
    .where(
      and(
        eq(notice.workspaceId, actor.workspaceId),
        eq(notice.userId, actor.userId),
        sql`${notice.ackedAt} IS NULL`,
        sql`${notice.id} = ANY(${`{${numeric.join(",")}}`}::bigint[])`,
      ),
    );
  return "acked";
}

// ── Invitations (a claim on a FUTURE user; requires armed mail — the route gates that) ─────

/** The device lane's minted invitations (folded address + fresh link token per address). */
export type LaneInviteOutcome =
  | { outcome: "invited"; minted: { email: string; token: string }[] }
  | { outcome: "owner_role_required" }
  | { outcome: "unknown_skill" }
  | { outcome: "unknown_channel" }
  | { outcome: "bad_email" };

/**
 * The device lane's invitation write. Inviting is OWNER-ONLY — the gate runs against the
 * actor's seat role. The optional FIRST-DESTINATION hint (at most one — a skill or a channel of this
 * workspace, named by the caller) must resolve (all-or-none), lands on the invitation row, and
 * is delivered by the accept ceremony: the seat first, then the direct follow / channel
 * membership in the same transaction. Each invitation mints a fresh single-use link token
 * (hash-stored); re-inviting an address supersedes its old link and any declined record.
 */
export async function laneInvite(
  actor: DeviceActor,
  emails: string[],
  hint: { skill?: string; channel?: string },
): Promise<LaneInviteOutcome> {
  const ws = actor.workspaceId;
  const folded: string[] = [];
  for (const email of emails) {
    const canonical = foldInviteEmail(email);
    if (canonical === null) {
      return { outcome: "bad_email" };
    }
    folded.push(canonical);
  }
  if (actor.role !== "owner") {
    return { outcome: "owner_role_required" };
  }
  return await getDb().transaction(async (tx) => {
    let hintBundleId: string | null = null;
    let hintChannelId: string | null = null;
    if (hint.skill !== undefined) {
      const rows = await tx
        .select({ id: bundle.id })
        .from(bundle)
        .where(
          and(eq(bundle.workspaceId, ws), eq(bundle.name, hint.skill), eq(bundle.status, "active")),
        )
        .limit(1);
      if (rows.length === 0) {
        return { outcome: "unknown_skill" };
      }
      hintBundleId = rows[0]?.id ?? null;
    } else if (hint.channel !== undefined) {
      const rows = await tx
        .select({ id: channel.id })
        .from(channel)
        .where(and(eq(channel.workspaceId, ws), eq(channel.name, hint.channel)))
        .limit(1);
      if (rows.length === 0) {
        return { outcome: "unknown_channel" };
      }
      hintChannelId = rows[0]?.id ?? null;
    }
    const expiresAt = new Date(Date.now() + INVITATION_TTL_MS);
    const minted: { email: string; token: string }[] = [];
    for (const email of folded) {
      const token = mintInviteToken();
      await supersedeDeclinedInvitationTx(tx, ws, email);
      await tx.execute(sql`
        insert into ${invitation}
          (id, workspace_id, email, role, status, invited_by, expires_at,
           token_sha256, hint_bundle_id, hint_channel_id)
        values (${mintInvitationId()}, ${ws}, ${email}, 'member', 'pending', ${actor.userId},
                ${expiresAt}, sha256(convert_to(${token}, 'UTF8')),
                ${hintBundleId}, ${hintChannelId})
        on conflict (email, workspace_id) where status = 'pending'
        do update set invited_by = excluded.invited_by, expires_at = excluded.expires_at,
                      token_sha256 = excluded.token_sha256,
                      hint_bundle_id = excluded.hint_bundle_id,
                      hint_channel_id = excluded.hint_channel_id,
                      created_at = now()
      `);
      await auditInTx(tx, {
        workspaceId: ws,
        actor: { userId: actor.userId, deviceId: actor.deviceId, display: actor.display },
        kind: "invitation_created",
        subject: email,
        outcome: "ok",
        details: {
          ...(hint.skill !== undefined ? { hint: { kind: "skill", name: hint.skill } } : {}),
          ...(hint.channel !== undefined ? { hint: { kind: "channel", name: hint.channel } } : {}),
        },
      });
      minted.push({ email, token });
    }
    return { outcome: "invited", minted };
  });
}

// ── The device-lane catalog read (`GET /v1/workspaces/{ws}/skills`) ─────────────────────────

export interface LaneSkillIndexEntry {
  skill_id: string;
  name: string;
  kind: string;
  status: string;
  version_id: string;
  bundle_digest: string;
  generation: number;
  display_name?: string;
  updated_at: number;
  open_proposals: number;
}

/** The workspace catalog — every bundle holding a `current`, ordered by id. */
export async function laneSkillsIndex(actor: DeviceActor): Promise<LaneSkillIndexEntry[]> {
  const ws = actor.workspaceId;
  const rows = await getDb()
    .select({
      skillId: bundle.id,
      name: bundle.name,
      kind: bundle.kind,
      status: bundle.status,
      displayName: bundle.displayName,
      versionId: planeCurrentPointer.versionId,
      generation: planeCurrentPointer.generation,
      updatedAtMs: sql<string>`(extract(epoch from ${planeCurrentPointer.movedAt}) * 1000)::bigint`,
      bundleDigest: planeVersionDigest.bundleDigest,
      openProposals: sql<string>`(
        SELECT COUNT(*) FROM web.proposal p
        WHERE p.workspace_id = ${ws} AND p.bundle_id = ${bundle.id} AND p.status = 'open'
      )`,
    })
    .from(bundle)
    .innerJoin(
      planeCurrentPointer,
      and(
        eq(planeCurrentPointer.workspaceId, bundle.workspaceId),
        eq(planeCurrentPointer.bundleId, bundle.id),
      ),
    )
    .leftJoin(
      planeVersionDigest,
      and(
        eq(planeVersionDigest.workspaceId, bundle.workspaceId),
        eq(planeVersionDigest.bundleId, bundle.id),
        eq(planeVersionDigest.versionId, planeCurrentPointer.versionId),
      ),
    )
    .where(and(eq(bundle.workspaceId, ws), sql`${bundle.status} <> 'deleted'`))
    .orderBy(asc(bundle.id));
  return rows.map((r) => ({
    skill_id: r.skillId,
    name: r.name,
    kind: r.kind,
    status: r.status,
    version_id: r.versionId,
    bundle_digest: r.bundleDigest ?? "",
    generation: Number(r.generation),
    ...(r.displayName === null ? {} : { display_name: r.displayName }),
    updated_at: Number(r.updatedAtMs),
    open_proposals: Number(r.openProposals),
  }));
}

// ── The delivery-era helpers other DAL modules share ────────────────────────────────────────

/** How many seats the workspace holds — the default channel's reach base. */
export async function workspaceSeatCount(ws: string): Promise<number> {
  const rows = await getDb().select({ n: count() }).from(seat).where(eq(seat.workspaceId, ws));
  return rows[0]?.n ?? 0;
}

/** The open-proposal rows of one bundle (the device lane's list read). */
export async function openProposalsOf(
  actor: DeviceActor,
  bundleId: string,
): Promise<{ versionId: string; createdAt: Date }[]> {
  const rows = await getDb()
    .select({ versionId: proposal.candidateVersionId, createdAt: proposal.createdAt })
    .from(proposal)
    .where(
      and(
        eq(proposal.workspaceId, actor.workspaceId),
        eq(proposal.bundleId, bundleId),
        eq(proposal.status, "open"),
      ),
    )
    .orderBy(asc(proposal.createdAt), asc(proposal.candidateVersionId));
  return rows;
}

/** The log decoration read: the bundle's catalog identity + this app's proposal events. */
export interface LaneLogIdentity {
  bundleId: string;
  name: string;
  kind: string;
  status: string;
  baseName: string | null;
}

export interface LaneLogProposal {
  versionId: string;
  status: string;
  proposer: string;
  resolvedBy: string | null;
  resolvedReason: string | null;
  resolvedAt: Date | null;
  createdAt: Date;
}

export async function laneLogOf(
  actor: DeviceActor,
  bundleId: string,
): Promise<{ identity: LaneLogIdentity; proposals: LaneLogProposal[] } | null> {
  const rows = await getDb()
    .select({
      bundleId: bundle.id,
      name: bundle.name,
      kind: bundle.kind,
      status: bundle.status,
      baseName: bundle.baseName,
    })
    .from(bundle)
    .where(and(eq(bundle.workspaceId, actor.workspaceId), eq(bundle.id, bundleId)))
    .limit(1);
  const identity = rows[0];
  if (identity === undefined) {
    return null;
  }
  // A second aliased `user` join resolves the RESOLVER's display (the proposer join already
  // resolves the proposer) so the wire serves a person display, never a raw user id.
  const resolver = alias(user, "resolver");
  const proposalRows = await getDb()
    .select({
      versionId: proposal.candidateVersionId,
      status: proposal.status,
      proposerDisplay: personDisplayLeftSql(user),
      resolvedBy: proposal.resolvedBy,
      resolverDisplay: personDisplayLeftSql(resolver),
      resolvedReason: proposal.resolvedReason,
      resolvedAt: proposal.resolvedAt,
      createdAt: proposal.createdAt,
    })
    .from(proposal)
    .leftJoin(user, eq(user.id, proposal.proposedBy))
    .leftJoin(resolver, eq(resolver.id, proposal.resolvedBy))
    .where(and(eq(proposal.workspaceId, actor.workspaceId), eq(proposal.bundleId, bundleId)))
    .orderBy(sql`${proposal.createdAt} DESC`);
  return {
    identity,
    proposals: proposalRows.map((p) => ({
      versionId: p.versionId,
      status: p.status,
      proposer: p.proposerDisplay ?? "former member",
      // Unresolved (open) carries no resolver — stays null; a resolved row serves the display,
      // falling back to "former member" when the resolver's user row is gone (mirrors proposer).
      resolvedBy: p.resolvedBy === null ? null : (p.resolverDisplay ?? "former member"),
      resolvedReason: p.resolvedReason,
      resolvedAt: p.resolvedAt,
      createdAt: p.createdAt,
    })),
  };
}

/** Every open proposal in the workspace (the review inbox), bundle name joined. */
export async function openProposalsIndex(actor: DeviceActor): Promise<
  {
    id: string;
    bundleId: string;
    bundleName: string;
    versionId: string;
    proposedBy: string | null;
    proposerDisplay: string;
    proposerEmail: string | null;
    createdAt: Date;
  }[]
> {
  const rows = await getDb()
    .select({
      id: proposal.id,
      bundleId: proposal.bundleId,
      bundleName: bundle.name,
      versionId: proposal.candidateVersionId,
      proposedBy: proposal.proposedBy,
      proposerName: personDisplayLeftSql(user),
      proposerEmail: user.email,
      createdAt: proposal.createdAt,
    })
    .from(proposal)
    .innerJoin(
      bundle,
      and(eq(bundle.workspaceId, proposal.workspaceId), eq(bundle.id, proposal.bundleId)),
    )
    .leftJoin(user, eq(user.id, proposal.proposedBy))
    .where(and(eq(proposal.workspaceId, actor.workspaceId), eq(proposal.status, "open")))
    .orderBy(asc(proposal.createdAt), asc(proposal.id));
  return rows.map((r) => ({
    id: r.id,
    bundleId: r.bundleId,
    bundleName: r.bundleName,
    versionId: r.versionId,
    proposedBy: r.proposedBy,
    proposerDisplay: r.proposerName ?? "former member",
    proposerEmail: r.proposerEmail,
    createdAt: r.createdAt,
  }));
}
