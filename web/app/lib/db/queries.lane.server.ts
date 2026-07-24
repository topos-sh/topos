import { and, asc, count, eq, sql } from "drizzle-orm";
import { alias } from "drizzle-orm/pg-core";
import type { SessionActor } from "@/lib/auth/guards.server";
import {
  auditInTx,
  mintChannelId,
  mintInvitationId,
  mintInviteToken,
  profileDemandSql,
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
  bundleUpstream,
  channel,
  invitation,
  notice,
  proposal,
  seat,
  workspace,
} from "@/lib/db/schema.app";
import { user } from "@/lib/db/schema.auth";
import { planeCurrentPointer, planeVersionDigest } from "@/lib/db/schema.custody";

/**
 * The SESSION lane's data access — the row-op half of `/api/v1`, served entirely by this
 * tier. Every op takes the branded `SessionActor` (minted only by requireSessionActor) and
 * passes the actor's server-resolved person/session — never anything client-asserted beyond
 * the credential itself. Role gates read the actor's seat role; existence misses answer the
 * same status vocabulary as before so the wire mapping stays uniform.
 *
 * Delivery is DEMAND ∩ ENTITLEMENT: the person-side demand is the profile (profileDemandSql —
 * the default-channel baseline + included channels + included bundles − excludes), the
 * entitlement is the seat itself (whole catalog). Project-side demand never reaches this
 * module: the client resolves `topos.toml` refs through ordinary catalog reads, each one
 * seat-gated the same way.
 *
 * Multi-read answers (delivery, the channels index) run inside ONE REPEATABLE READ
 * transaction — one snapshot, so the served sets can never straddle a profile change.
 */

const CHANNEL_NAME = /^[a-z0-9][a-z0-9-]*$/;
const CHANNEL_NAME_MAX = 64;

type Tx = Parameters<Parameters<ReturnType<typeof getDb>["transaction"]>[0]>[0];

/** A `{a,b,c}` Postgres array literal (values validated upstream as opaque ids). */
function pgTextArray(values: string[]): string {
  return `{${values.map((v) => `"${v.replaceAll('"', "")}"`).join(",")}}`;
}

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
  /** Why the profile delivers it: which channels carry it, and/or a direct include line. */
  via: { channels: string[]; direct: boolean };
  /** Set when a profile include pins a version — the served version_id IS the pin then. */
  pinned?: boolean;
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
  /** The session's status; "pending" delivers NOTHING (the empty body below). */
  session_status: "active" | "pending";
  skills: DeliverySkill[];
  notices: DeliveryNotice[];
  proposals_awaiting: number;
  staleness_window_ms: number;
}

/**
 * The PENDING session's delivery: shape-complete and EMPTY — no data flows over a pending
 * session (skills/notices empty, zero proposals), but the staleness clock still serves so the
 * client's freshness bookkeeping stays honest while it waits for approval.
 */
export async function emptyDeliveryFor(actor: SessionActor): Promise<DeliveryBody> {
  const wsRows = await getDb()
    .select({ stalenessWindowMs: workspace.stalenessWindowMs })
    .from(workspace)
    .where(eq(workspace.id, actor.workspaceId))
    .limit(1);
  return {
    schema_version: 1,
    workspace_id: actor.workspaceId,
    session_status: "pending",
    skills: [],
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
 * The person-layer answer for ONE session: the profile's demand (default-channel baseline ∪
 * included channels ∪ included bundles − excludes), active + current-holding only, with `via`
 * attribution, the resolved protection, and the pin resolution (a pinned include serves the
 * pinned version when the plane still holds it, else falls back to current — honest, never a
 * hole), plus the unacked notices, the open-proposal count over the demanded set, and the ONE
 * staleness clock.
 */
export async function deliveryFor(actor: ProfileActor): Promise<DeliveryBody> {
  const ws = actor.workspaceId;
  return await getDb().transaction(
    async (tx) => {
      const skillRows = await tx.execute(sql`
        SELECT b.id AS skill_id, b.name, b.kind, b.display_name,
               COALESCE(b.protection, w.protection_default, 'open') AS protection,
               cp.version_id AS current_version_id, cp.generation,
               (extract(epoch from cp.moved_at) * 1000)::bigint AS updated_at,
               vd.bundle_digest AS current_digest,
               pe.pin AS pin, pvd.bundle_digest AS pin_digest,
               COALESCE((
                 SELECT array_agg(ch.name ORDER BY ch.name)
                 FROM web.channel_bundle cb
                 JOIN web.channel ch ON ch.id = cb.channel_id
                 WHERE cb.workspace_id = ${ws} AND cb.bundle_id = b.id
                   AND (
                     (ch.is_default AND NOT EXISTS (
                        SELECT 1 FROM web.profile_entry px
                        WHERE px.channel_id = ch.id AND px.user_id = ${actor.userId}
                          AND px.mode = 'exclude'))
                     OR EXISTS (
                        SELECT 1 FROM web.profile_entry pi
                        WHERE pi.channel_id = ch.id AND pi.user_id = ${actor.userId}
                          AND pi.mode = 'include')
                   )
               ), '{}') AS via_channels,
               (pe.bundle_id IS NOT NULL) AS direct
        FROM (${profileDemandSql(actor.userId, ws)}) e
        JOIN web.bundle b ON b.id = e.bundle_id
        JOIN web.workspace w ON w.id = ${ws}
        JOIN plane.current_pointer cp ON cp.workspace_id = ${ws} AND cp.bundle_id = b.id
        LEFT JOIN plane.version_digest vd
          ON vd.workspace_id = ${ws} AND vd.bundle_id = b.id AND vd.version_id = cp.version_id
        LEFT JOIN web.profile_entry pe
          ON pe.user_id = ${actor.userId} AND pe.bundle_id = b.id AND pe.mode = 'include'
        LEFT JOIN plane.version_digest pvd
          ON pe.pin IS NOT NULL AND pvd.workspace_id = ${ws} AND pvd.bundle_id = b.id
             AND pvd.version_id = pe.pin
        ORDER BY b.name
      `);
      const skills: DeliverySkill[] = (skillRows.rows as Record<string, unknown>[]).map((r) => {
        // Pin resolution: a live pin (its digest row still present) is the served target; a
        // stale pin (purged version) serves current instead of a hole.
        const pinLive = r.pin !== null && r.pin_digest !== null;
        return {
          skill_id: r.skill_id as string,
          name: r.name as string,
          kind: r.kind as string,
          ...(r.display_name === null ? {} : { display_name: r.display_name as string }),
          protection: r.protection as string,
          version_id: (pinLive ? r.pin : r.current_version_id) as string,
          // A pointer without its digest row is a custody fault; serve the honest empty string
          // rather than fail the whole delivery (the client's re-hash will refuse the bundle).
          bundle_digest: ((pinLive ? r.pin_digest : r.current_digest) as string | null) ?? "",
          generation: Number(r.generation),
          updated_at: Number(r.updated_at),
          via: { channels: r.via_channels as string[], direct: r.direct as boolean },
          ...(pinLive ? { pinned: true } : {}),
        };
      });

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
          AND p.bundle_id IN (${profileDemandSql(actor.userId, ws)})
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
        session_status: "active",
        skills,
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
 * The sessions page's applied-state report: UPSERT this session's (bundle, applied version)
 * rows and DELETE the rows it no longer reports — the session's report is a complete snapshot
 * of what the installation holds for this workspace, so absence is meaningful (a removed
 * project or an edited manifest stops reporting a bundle and the row goes). A report is
 * CLIENT-ASSERTED data, so every named bundle is re-checked to exist in the workspace. The
 * write FENCES on the live ACTIVE session row (FOR UPDATE): an in-flight report that lost a
 * race with a revocation must not resurrect state the ending just cascaded away.
 */
export async function reportApplied(
  actor: SessionActor,
  applied: { skillId: string; versionId: string }[],
): Promise<"ok" | "session_ended"> {
  const ws = actor.workspaceId;
  const skillIds = pgTextArray(applied.map((a) => a.skillId));
  const versionIds = pgTextArray(applied.map((a) => a.versionId));
  return await getDb().transaction(async (tx) => {
    const live = await tx.execute(
      sql`SELECT id FROM web.cli_session
          WHERE id = ${actor.sessionId} AND workspace_id = ${ws} AND status = 'active'
          FOR UPDATE`,
    );
    if (live.rows.length === 0) {
      return "session_ended";
    }
    await tx.execute(sql`
      INSERT INTO web.session_bundle_state (session_id, bundle_id, applied_version_id, reported_at)
      SELECT ${actor.sessionId}, r.skill_id, r.version_id, now()
      FROM UNNEST(${skillIds}::text[], ${versionIds}::text[]) AS r(skill_id, version_id)
      JOIN web.bundle b ON b.id = r.skill_id AND b.workspace_id = ${ws}
      ON CONFLICT (session_id, bundle_id) DO UPDATE
        SET applied_version_id = excluded.applied_version_id, reported_at = excluded.reported_at
    `);
    await tx.execute(sql`
      DELETE FROM web.session_bundle_state st
      WHERE st.session_id = ${actor.sessionId}
        AND NOT (st.bundle_id = ANY(${skillIds}::text[]))
    `);
    return "ok";
  });
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
export async function laneMe(actor: SessionActor): Promise<LaneMe | null> {
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
  /** The immutable channel id (the web profile editor's toggle key; the wire route omits it). */
  channelId: string;
  name: string;
  mode: string;
  builtin: boolean;
  /** Whether the CALLER's profile references this channel (the default: not excluded). */
  included: boolean;
  skills: { skillId: string; name: string }[];
}

/** The workspace channels index (`GET /channels`) — name-sorted, the default included. */
export async function laneChannels(actor: ProfileActor): Promise<LaneChannel[]> {
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
                THEN NOT EXISTS (SELECT 1 FROM web.profile_entry px
                                 WHERE px.channel_id = ch.id AND px.user_id = ${actor.userId}
                                   AND px.mode = 'exclude')
                ELSE EXISTS (SELECT 1 FROM web.profile_entry pi
                             WHERE pi.channel_id = ch.id AND pi.user_id = ${actor.userId}
                               AND pi.mode = 'include')
           END) AS included
        FROM web.channel ch
        WHERE ch.workspace_id = ${ws}
        ORDER BY ch.name
      `);
      return (channelRows.rows as Record<string, unknown>[]).map((r) => ({
        channelId: r.id as string,
        name: r.name as string,
        mode: r.mode as string,
        builtin: r.is_default as boolean,
        included: r.included as boolean,
        skills: byChannel.get(r.id as string) ?? [],
      }));
    },
    { isolationLevel: "repeatable read", accessMode: "read only" },
  );
}

/** A bundle's audience (`GET /skills/{skill}/reach`): demanding persons + their live sessions. */
export async function laneReach(
  actor: SessionActor,
  bundleId: string,
): Promise<{ persons: number; sessions: number } | null> {
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
      AND EXISTS (SELECT 1 FROM (${profileDemandSql(sql`s.user_id`, ws)}) e
                  WHERE e.bundle_id = ${bundleId})
  `);
  // A session counts toward reach only while ACTIVE — a pending session cannot receive
  // delivery in this workspace, whatever its owner's seat says.
  const sessions = await db.execute(sql`
    SELECT COUNT(*) AS n FROM web.cli_session cs
    WHERE cs.workspace_id = ${ws} AND cs.status = 'active'
      AND EXISTS (SELECT 1 FROM (${profileDemandSql(sql`cs.user_id`, ws)}) e
                  WHERE e.bundle_id = ${bundleId})
  `);
  return {
    persons: Number((persons.rows[0] as { n: string | number }).n),
    sessions: Number((sessions.rows[0] as { n: string | number }).n),
  };
}

// ── The profile (the person-side manifest: add -g / remove -g / the web editor) ─────────────

/**
 * The actor shape BOTH profile doors satisfy: the session lane's SessionActor and the web
 * page's MemberActor (the ops read only the person + workspace — a profile is personal, so
 * no role gates apply). Structural, so both branded actors pass without a cast.
 */
export interface ProfileActor {
  readonly userId: string;
  readonly workspaceId: string;
}

export interface ProfileEntryView {
  mode: "include" | "exclude";
  kind: "skill" | "channel";
  /** The catalog kind for bundles ('skill' today); 'channel' rows repeat the literal. */
  bundleKind?: string;
  name: string;
  pin: string | null;
}

/** The person's whole profile in this workspace, resolved to names (name-sorted per group). */
export async function profileOf(actor: ProfileActor): Promise<ProfileEntryView[]> {
  const rows = await getDb().execute(sql`
    SELECT pe.mode, pe.pin, b.name AS bundle_name, b.kind AS bundle_kind, c.name AS channel_name
    FROM web.profile_entry pe
    LEFT JOIN web.bundle b ON b.id = pe.bundle_id
    LEFT JOIN web.channel c ON c.id = pe.channel_id
    WHERE pe.workspace_id = ${actor.workspaceId} AND pe.user_id = ${actor.userId}
    ORDER BY pe.mode, COALESCE(b.name, c.name)
  `);
  return (rows.rows as Record<string, unknown>[]).map((r) => ({
    mode: r.mode as "include" | "exclude",
    kind: r.bundle_name !== null ? ("skill" as const) : ("channel" as const),
    ...(r.bundle_name !== null ? { bundleKind: r.bundle_kind as string } : {}),
    name: (r.bundle_name ?? r.channel_name) as string,
    pin: r.pin as string | null,
  }));
}

/**
 * `add -g <skill>`: upsert the include line (an exclude on the same bundle flips to include —
 * one stance per pair, the flip IS the re-add). Archived bundles refuse (a freed name is a
 * NEW identity; the old one is out of circulation).
 */
export async function profileIncludeBundle(
  actor: ProfileActor,
  bundleId: string,
  pin: string | null,
): Promise<"included" | "unknown_skill" | "skill_not_active"> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const status = await bundleStatusInTx(tx, ws, bundleId);
    if (status === null) {
      return "unknown_skill";
    }
    if (status !== "active") {
      return "skill_not_active";
    }
    await tx.execute(sql`
      INSERT INTO web.profile_entry (workspace_id, user_id, mode, bundle_id, pin)
      VALUES (${ws}, ${actor.userId}, 'include', ${bundleId}, ${pin})
      ON CONFLICT (user_id, bundle_id) WHERE bundle_id is not null
      DO UPDATE SET mode = 'include', pin = excluded.pin, updated_at = now()
    `);
    return "included";
  });
}

export type ProfileRemoveOutcome =
  /** The include line was deleted; nothing broader provides it — delivery just ends. */
  | "removed"
  /** A broader layer (a channel, the baseline) still provides it — an EXCLUDE line was
   * recorded (the one negative state; the receipt says so). */
  | "excluded"
  /** Neither an include line nor any channel provides it — nothing to do. */
  | "not_in_profile"
  | "unknown_skill";

/**
 * `remove -g <skill>`: delete the include line; when a broader layer (an included channel or
 * the default baseline) still provides the bundle, record an EXCLUDE line instead — the
 * manifest-layer semantics, applied to the profile.
 */
export async function profileRemoveBundle(
  actor: ProfileActor,
  bundleId: string,
): Promise<ProfileRemoveOutcome> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const status = await bundleStatusInTx(tx, ws, bundleId);
    if (status === null) {
      return "unknown_skill";
    }
    const deleted = await tx.execute(sql`
      DELETE FROM web.profile_entry
      WHERE user_id = ${actor.userId} AND bundle_id = ${bundleId} AND mode = 'include'
      RETURNING bundle_id
    `);
    // Still provided by a channel the profile carries (baseline or include)? Then the removal
    // needs the one negative state: an exclude line.
    const provided = await tx.execute(sql`
      SELECT 1 FROM (${profileDemandSql(actor.userId, ws)}) e
      WHERE e.bundle_id = ${bundleId}
    `);
    if (provided.rows.length > 0) {
      await tx.execute(sql`
        INSERT INTO web.profile_entry (workspace_id, user_id, mode, bundle_id)
        VALUES (${ws}, ${actor.userId}, 'exclude', ${bundleId})
        ON CONFLICT (user_id, bundle_id) WHERE bundle_id is not null
        DO UPDATE SET mode = 'exclude', pin = NULL, updated_at = now()
      `);
      return "excluded";
    }
    return deleted.rows.length > 0 ? "removed" : "not_in_profile";
  });
}

/** `add -g @ws/channels/x`: upsert the channel include (an exclude flips back to include). */
export async function profileIncludeChannel(
  actor: ProfileActor,
  channelName: string,
): Promise<"included" | "unknown_channel"> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const row = await channelByNameInTx(tx, ws, channelName);
    if (row === undefined) {
      return "unknown_channel";
    }
    if (row.isDefault) {
      // The baseline is implicit; "including" it = clearing any exclude line.
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
 * `remove -g @ws/channels/x`: delete the include line; the DEFAULT channel — the implicit
 * baseline with no include line to delete — takes an exclude line instead (the one negative
 * state).
 */
export async function profileRemoveChannel(
  actor: ProfileActor,
  channelName: string,
): Promise<"removed" | "excluded" | "not_in_profile" | "unknown_channel"> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const row = await channelByNameInTx(tx, ws, channelName);
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
      return "excluded";
    }
    const deleted = await tx.execute(sql`
      DELETE FROM web.profile_entry
      WHERE user_id = ${actor.userId} AND channel_id = ${row.id} AND mode = 'include'
      RETURNING channel_id
    `);
    return deleted.rows.length > 0 ? "removed" : "not_in_profile";
  });
}

// ── Curation (place / unplace, create-on-first-use) ─────────────────────────────────────────

async function channelByNameInTx(tx: Tx, ws: string, name: string) {
  const rows = await tx
    .select({ id: channel.id, isDefault: channel.isDefault, mode: channel.mode })
    .from(channel)
    .where(and(eq(channel.workspaceId, ws), eq(channel.name, name)))
    .limit(1);
  return rows[0];
}

/**
 * Place a bundle reference into a channel — creating the channel on FIRST use (member-level).
 * Everything past the name resolution is the ONE curation core shared with the web page's
 * id-keyed functions (queries.channels.server.ts): the bundle-active gate, the CURATED
 * channel's reviewer+ gate (symmetric with removal), the idempotent insert, and the audit
 * row. The create-race loser places into the winner's row ('placed', never a raw conflict):
 * ids are minted randomly, so only the name unique can collide, and a re-select resolves it.
 */
export async function lanePlaceBundle(
  actor: SessionActor,
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
          actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
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
  actor: SessionActor,
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
  role: SessionActor["role"],
  tightens: boolean,
): "owner_role_required" | "reviewer_role_required" | null {
  if (tightens) {
    return role === "member" ? "reviewer_role_required" : null;
  }
  return role === "owner" ? null : "owner_role_required";
}

/** Pin a bundle's protection level (`open` | `reviewed`; the route validated the value). */
export async function laneProtectBundle(
  actor: SessionActor,
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
      actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
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
  actor: SessionActor,
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
      actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
      kind: `mode_${mode}`,
      subject: updated[0]?.id ?? channelName,
      outcome: "ok",
    });
    return "set";
  });
}

// ── Notices ack ──────────────────────────────────────────────────────────────────────────────

/** Mark the caller's own notices read by id — idempotent; unknown ids are ignored. */
export async function laneAckNotices(actor: SessionActor, ids: string[]): Promise<"acked"> {
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

/** The session lane's minted invitations (folded address + fresh link token per address). */
export type LaneInviteOutcome =
  | { outcome: "invited"; minted: { email: string; token: string }[] }
  | { outcome: "owner_role_required" }
  | { outcome: "unknown_skill" }
  | { outcome: "unknown_channel" }
  | { outcome: "bad_email" };

/**
 * The session lane's invitation write. Inviting is OWNER-ONLY — the gate runs against the
 * actor's seat role. The optional FIRST-DESTINATION hint (at most one — a skill or a channel
 * of this workspace, named by the caller) must resolve (all-or-none), lands on the invitation
 * row, and is delivered by the accept ceremony as a PROFILE PREFILL: the seat first, then the
 * include line in the same transaction. Each invitation mints a fresh single-use link token
 * (hash-stored); re-inviting an address supersedes its old link and any declined record.
 */
export async function laneInvite(
  actor: SessionActor,
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
        actor: { userId: actor.userId, sessionId: actor.sessionId, display: actor.display },
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

// ── The session-lane catalog read (`GET /v1/workspaces/{ws}/skills`) ────────────────────────

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
  /** The recorded upstream origin, present when the bundle was imported from an external
   * source — lets a client suggest the governed copy when the same source is added again. */
  upstream_host?: string;
  upstream_repo?: string;
  upstream_path?: string;
}

/** The workspace catalog — every bundle holding a `current`, ordered by id. */
export async function laneSkillsIndex(actor: SessionActor): Promise<LaneSkillIndexEntry[]> {
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
      upstreamHost: bundleUpstream.host,
      upstreamRepo: bundleUpstream.repo,
      upstreamPath: bundleUpstream.path,
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
      bundleUpstream,
      and(
        eq(bundleUpstream.workspaceId, bundle.workspaceId),
        eq(bundleUpstream.bundleId, bundle.id),
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
    ...(r.upstreamHost === null || r.upstreamRepo === null
      ? {}
      : {
          upstream_host: r.upstreamHost,
          upstream_repo: r.upstreamRepo,
          upstream_path: r.upstreamPath ?? "",
        }),
  }));
}

// ── The shared helpers other DAL modules use ────────────────────────────────────────────────

/** How many seats the workspace holds — the default channel's reach base. */
export async function workspaceSeatCount(ws: string): Promise<number> {
  const rows = await getDb().select({ n: count() }).from(seat).where(eq(seat.workspaceId, ws));
  return rows[0]?.n ?? 0;
}

/** The open-proposal rows of one bundle (the session lane's list read). */
export async function openProposalsOf(
  actor: SessionActor,
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
  actor: SessionActor,
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
export async function openProposalsIndex(actor: SessionActor): Promise<
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
