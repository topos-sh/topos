import type { DeviceActor } from "@/lib/auth/guards.server";
import { getPool } from "./index.server";

/**
 * The DEVICE lane's data access — the row-op half of `/api/v1`, served by this tier calling the
 * guarded `topos_*` functions directly under the scoped `topos_web` role (the same one-implementation
 * rule the web's own admin surfaces follow). The trust shape, in order:
 *
 *  - `deviceActorProbe` is the FRONT DOOR: `topos_device_actor` resolves the presented workspace
 *    credential — its storage-form hash is computed IN Postgres, so this tier holds no hashing
 *    code — to its non-revoked registry row joined onto a CONFIRMED `workspace_member` seat.
 *    Every miss is one empty result the guard folds to the uniform 404.
 *  - Every op after the door takes the branded `DeviceActor` (minted only by the guard) and passes
 *    the actor's server-resolved person/device — never anything client-asserted beyond the
 *    credential itself. The guarded functions re-run their own membership gates in-transaction
 *    (defense in depth, not the front door).
 */

/** The raw row `topos_device_actor` resolves — consumed ONLY by the guard's mint. */
export interface DeviceActorRow {
  person: string;
  deviceKeyId: string;
  role: "owner" | "reviewer" | "member";
}

export async function deviceActorProbe(
  workspaceId: string,
  credential: string,
): Promise<DeviceActorRow | null> {
  const { rows } = await getPool().query<{
    person: string;
    device_key_id: string;
    role: string;
  }>("SELECT person, device_key_id, role FROM topos_device_actor($1, $2)", [
    workspaceId,
    credential,
  ]);
  const row = rows[0];
  if (row === undefined) {
    return null;
  }
  if (row.role !== "owner" && row.role !== "reviewer" && row.role !== "member") {
    throw new Error("device actor resolve: stored role outside the closed set");
  }
  return { person: row.person, deviceKeyId: row.device_key_id, role: row.role };
}

/**
 * The delivery read, whole: `topos_delivery` assembles the COMPLETE `WireDelivery` body in ONE
 * SQL statement (one snapshot — the entitled/detached/notices sets can never straddle a
 * subscription change). NULL means the in-function membership gate refused — uniform 404.
 */
export async function deviceDelivery(actor: DeviceActor): Promise<unknown | null> {
  const { rows } = await getPool().query<{ body: unknown }>(
    "SELECT topos_delivery($1, $2, $3) AS body",
    [actor.workspaceId, actor.person, actor.deviceKeyId],
  );
  return rows[0]?.body ?? null;
}

/**
 * The applied-state report: `topos_report_applied` re-checks every client-asserted skill against
 * the server's own entitlement predicate, stamps the staleness clock, and fences itself FOR UPDATE
 * (its callers — this tier — are READ COMMITTED). `commits` are raw 32-byte buffers (the route
 * validates the hex). NULL means the gate refused — uniform 404; 'ok' is a 204.
 */
export async function deviceReportApplied(
  actor: DeviceActor,
  nowMs: number,
  skillIds: string[],
  commits: Buffer[],
): Promise<"ok" | null> {
  const { rows } = await getPool().query<{ outcome: string | null }>(
    "SELECT topos_report_applied($1, $2, $3, $4, $5::text[], $6::bytea[]) AS outcome",
    [actor.workspaceId, actor.person, actor.deviceKeyId, nowMs, skillIds, commits],
  );
  return rows[0]?.outcome === "ok" ? "ok" : null;
}

// ── the member-lane DESCRIBE reads ───────────────────────────────────────────────────────────────
//
// The PURE-directory-row reads only (`me` / `channels` / `reach`): they have NO guarded `topos_*`
// function (unlike delivery/report), so this tier mirrors the vault's raw SQL VERBATIM under the
// scoped role (`crates/plane-store/src/db/directory/describe.rs` + `channels.rs::channels_index`).
// The two BYTE-DECORATED describe reads — `proposals` (the proposed version's git commit message) and
// `skills/{skill}/log` (each version's git author/message) — stay on the vault and reach it through
// the `/api/v1` splat forwarder: this tier holds no byte custody, so it cannot serve them faithfully.
// Every miss here returns null, which the route folds to the uniform 404. The reads take the branded
// `DeviceActor` (its confirmed seat already resolved) and derive their scope from it; the guarded
// functions they DO call (`topos_invite_policy`, `topos_person_entitled`) re-run their own gates.

/** The caller's own membership facts (`GET /me`) — the workspace slug + seat, or null (no seat/row). */
export interface DeviceMeRow {
  name: string;
  displayName: string;
  role: string;
  invitedBy: string | null;
  invitePolicy: string;
}

/**
 * The caller's membership row — the workspace identity (name + display name), the confirmed seat
 * (role + inviter), and the invite policy through its one accessor. Mirrors `Db::membership_row`
 * (ONE statement). NULL when no confirmed seat / no workspace row → the route's uniform 404. The
 * share ADDRESS (`<base>/<name>`) is built at the route edge from the request origin (the vault
 * builds it from its `link_base`; here the origin IS that base — the app is the door).
 */
export async function deviceMe(actor: DeviceActor): Promise<DeviceMeRow | null> {
  const { rows } = await getPool().query<{
    name: string;
    display_name: string;
    role: string;
    invited_by: string | null;
    invite_policy: string;
  }>(
    `SELECT w.name, w.display_name, m.role, m.invited_by, topos_invite_policy($1) AS invite_policy
     FROM workspace w
     JOIN workspace_member m ON m.workspace_id = w.workspace_id
     WHERE w.workspace_id = $1 AND m.principal = $2 AND m.status = 'confirmed'`,
    [actor.workspaceId, actor.person],
  );
  const row = rows[0];
  if (row === undefined) {
    return null;
  }
  return {
    name: row.name,
    displayName: row.display_name,
    role: row.role,
    invitedBy: row.invited_by,
    invitePolicy: row.invite_policy,
  };
}

/** One channel as the index read returns it (a channel_id-free projection; skills name-sorted). */
export interface DeviceChannelRow {
  name: string;
  mode: string;
  builtin: boolean;
  member: boolean;
  memberCount: number;
  skills: { skillId: string; name: string }[];
}

/**
 * The workspace channels index (`GET /channels`) — every channel, `everyone` included, name-sorted,
 * with the caller's membership, its member count (roster-derived for the builtin, else the
 * `channel_members` count), and its name-sorted skill references. Mirrors `Db::channels_index`: two
 * pool reads (skill refs grouped by channel, then the channels) assembled here.
 */
export async function deviceChannels(actor: DeviceActor): Promise<DeviceChannelRow[]> {
  const pool = getPool();
  const skillRows = await pool.query<{ channel_id: string; skill_id: string; name: string }>(
    `SELECT cs.channel_id, cs.skill_id, cat.name
     FROM channel_skills cs
     JOIN catalog cat ON cat.workspace_id = cs.workspace_id AND cat.skill_id = cs.skill_id
     WHERE cs.workspace_id = $1
     ORDER BY cat.name`,
    [actor.workspaceId],
  );
  const byChannel = new Map<string, { skillId: string; name: string }[]>();
  for (const r of skillRows.rows) {
    const list = byChannel.get(r.channel_id) ?? [];
    list.push({ skillId: r.skill_id, name: r.name });
    byChannel.set(r.channel_id, list);
  }
  const channelRows = await pool.query<{
    channel_id: string;
    name: string;
    mode: string;
    builtin: string;
    member: boolean;
    member_count: string;
  }>(
    `SELECT ch.channel_id, ch.name, ch.mode, ch.builtin,
       (ch.builtin = 1 OR EXISTS (SELECT 1 FROM channel_members cm
          WHERE cm.workspace_id = ch.workspace_id AND cm.channel_id = ch.channel_id
            AND cm.principal = $2)) AS member,
       (CASE WHEN ch.builtin = 1
             THEN (SELECT COUNT(*) FROM workspace_member m
                   WHERE m.workspace_id = ch.workspace_id AND m.status = 'confirmed')
             ELSE (SELECT COUNT(*) FROM channel_members cm
                   WHERE cm.workspace_id = ch.workspace_id AND cm.channel_id = ch.channel_id)
        END) AS member_count
     FROM channels ch
     WHERE ch.workspace_id = $1
     ORDER BY ch.name`,
    [actor.workspaceId, actor.person],
  );
  return channelRows.rows.map((r) => ({
    name: r.name,
    mode: r.mode,
    builtin: Number(r.builtin) !== 0,
    member: r.member,
    memberCount: Number(r.member_count),
    skills: byChannel.get(r.channel_id) ?? [],
  }));
}

/**
 * A skill's audience (`GET /skills/{skill}/reach`) — confirmed members entitled to the skill and
 * their non-revoked devices. Mirrors `Db::reach`: validate the immutable id against the catalog
 * (any status; unknown → null → the route's uniform 404), then two counts over
 * `topos_person_entitled`. Counts come back as bigint text from the driver → coerced to numbers.
 */
export async function deviceReach(
  actor: DeviceActor,
  skillId: string,
): Promise<{ persons: number; devices: number } | null> {
  const ws = actor.workspaceId;
  const pool = getPool();
  const exists = await pool.query<{ skill_id: string }>(
    "SELECT skill_id FROM catalog WHERE workspace_id = $1 AND skill_id = $2",
    [ws, skillId],
  );
  if (exists.rows[0] === undefined) {
    return null;
  }
  const persons = await pool.query<{ n: string }>(
    `SELECT COUNT(*) AS n FROM workspace_member m
     WHERE m.workspace_id = $1 AND m.status = 'confirmed'
       AND EXISTS (SELECT 1 FROM topos_person_entitled($1, m.principal) e WHERE e.skill_id = $2)`,
    [ws, skillId],
  );
  const devices = await pool.query<{ n: string }>(
    `SELECT COUNT(*) AS n FROM device_registry dr
     WHERE dr.workspace_id = $1 AND dr.revoked = 0
       AND EXISTS (SELECT 1 FROM topos_person_entitled($1, dr.principal) e WHERE e.skill_id = $2)`,
    [ws, skillId],
  );
  return {
    persons: Number(persons.rows[0]?.n ?? 0),
    devices: Number(devices.rows[0]?.n ?? 0),
  };
}

// ── the member-lane ROW-OP writes (guarded-function callers) ─────────────────────────────────────
//
// Each is ONE call to the guarded `topos_*` policy function under the scoped role, returning the
// function's RAW status string (the route maps it to the wire envelope: an OK `status`, a coded
// DENIED, the uniform 404 for `member_required`/`unknown_skill`/`unknown_channel`, or a 500 for any
// out-of-contract status). No skill pre-resolve is needed: every guarded function re-checks
// existence itself (returning `unknown_skill`/`unknown_channel`), so a direct call yields the same
// wire outcome the vault's pre-resolve does. `p_now` is epoch-ms, `p_created_at` an RFC-3339 string;
// the route stamps ONE clock per request (mirroring the vault handler).

/** The single row the guarded functions return — a status text. */
async function guardedStatus(sql: string, params: unknown[]): Promise<string> {
  const { rows } = await getPool().query<{ outcome: string }>(sql, params);
  const outcome = rows[0]?.outcome;
  if (outcome === undefined) {
    throw new Error("guarded policy function returned no outcome");
  }
  return outcome;
}

/** Ack the caller's own notices by id (`topos_notices_ack`): `acked` | `member_required`. */
export function deviceNoticesAck(
  actor: DeviceActor,
  ids: string[],
  nowMs: number,
): Promise<string> {
  return guardedStatus("SELECT topos_notices_ack($1, $2, $3::text[], $4) AS outcome", [
    actor.workspaceId,
    actor.person,
    ids,
    nowMs,
  ]);
}

/**
 * Seat the (already folded) emails as invited members + any channel pre-placements (`topos_invite`):
 * `invited` | `owner_role_required` | `unknown_channel` | `member_required`. The emails MUST arrive
 * canonically folded (ASCII-lowercase) — the workspace_member CHECK rejects a non-canonical principal
 * — so the route folds them at the edge (like the vault's `Principal::parse`).
 */
export function deviceInvite(
  actor: DeviceActor,
  emails: string[],
  channels: string[],
  createdAt: string,
): Promise<string> {
  return guardedStatus("SELECT topos_invite($1, $2, $3::text[], $4::text[], $5) AS outcome", [
    actor.workspaceId,
    actor.person,
    emails,
    channels,
    createdAt,
  ]);
}

/** Direct-follow a skill by its immutable id (`topos_follow_skill`). */
export function deviceFollowSkill(
  actor: DeviceActor,
  skillId: string,
  createdAt: string,
): Promise<string> {
  return guardedStatus("SELECT topos_follow_skill($1, $2, $3, $4, $5) AS outcome", [
    actor.workspaceId,
    actor.person,
    skillId,
    actor.deviceKeyId,
    createdAt,
  ]);
}

/** Unfollow a skill by its immutable id (`topos_unfollow_skill`). */
export function deviceUnfollowSkill(
  actor: DeviceActor,
  skillId: string,
  nowMs: number,
  createdAt: string,
): Promise<string> {
  return guardedStatus("SELECT topos_unfollow_skill($1, $2, $3, $4, $5) AS outcome", [
    actor.workspaceId,
    actor.person,
    skillId,
    nowMs,
    createdAt,
  ]);
}

/** Exclude a skill from THIS device (`topos_exclude_device`; args: ws, device, skill, created_at). */
export function deviceExcludeDevice(
  actor: DeviceActor,
  skillId: string,
  createdAt: string,
): Promise<string> {
  return guardedStatus("SELECT topos_exclude_device($1, $2, $3, $4) AS outcome", [
    actor.workspaceId,
    actor.deviceKeyId,
    skillId,
    createdAt,
  ]);
}

/** Join a channel by name (`topos_channel_join`). */
export function deviceChannelJoin(
  actor: DeviceActor,
  channel: string,
  createdAt: string,
): Promise<string> {
  return guardedStatus("SELECT topos_channel_join($1, $2, $3, $4) AS outcome", [
    actor.workspaceId,
    channel,
    actor.person,
    createdAt,
  ]);
}

/** Leave a channel by name (`topos_channel_leave`). */
export function deviceChannelLeave(
  actor: DeviceActor,
  channel: string,
  nowMs: number,
  createdAt: string,
): Promise<string> {
  return guardedStatus("SELECT topos_channel_leave($1, $2, $3, $4, $5) AS outcome", [
    actor.workspaceId,
    channel,
    actor.person,
    nowMs,
    createdAt,
  ]);
}

/** Place a skill reference into a channel (`topos_channel_place`; creates the channel on first use). */
export function deviceChannelPlace(
  actor: DeviceActor,
  channel: string,
  skillId: string,
  createdAt: string,
): Promise<string> {
  return guardedStatus("SELECT topos_channel_place($1, $2, $3, $4, $5) AS outcome", [
    actor.workspaceId,
    channel,
    skillId,
    actor.person,
    createdAt,
  ]);
}

/** Remove a skill reference from a channel (`topos_channel_unplace`). */
export function deviceChannelUnplace(
  actor: DeviceActor,
  channel: string,
  skillId: string,
  createdAt: string,
): Promise<string> {
  return guardedStatus("SELECT topos_channel_unplace($1, $2, $3, $4, $5) AS outcome", [
    actor.workspaceId,
    channel,
    skillId,
    actor.person,
    createdAt,
  ]);
}

/** Set a skill's protection level (`topos_protect_skill`; `level` is `open` | `reviewed`). */
export function deviceProtectSkill(
  actor: DeviceActor,
  skillId: string,
  level: string,
): Promise<string> {
  return guardedStatus("SELECT topos_protect_skill($1, $2, $3, $4) AS outcome", [
    actor.workspaceId,
    skillId,
    level,
    actor.person,
  ]);
}

/** Set a channel's mode (`topos_protect_channel`; `mode` is `open` | `curated`). */
export function deviceProtectChannel(
  actor: DeviceActor,
  channel: string,
  mode: string,
  createdAt: string,
): Promise<string> {
  return guardedStatus("SELECT topos_protect_channel($1, $2, $3, $4, $5) AS outcome", [
    actor.workspaceId,
    channel,
    mode,
    actor.person,
    createdAt,
  ]);
}
