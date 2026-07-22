import { relations, sql } from "drizzle-orm";
import {
  bigint,
  boolean,
  check,
  customType,
  foreignKey,
  index,
  jsonb,
  primaryKey,
  text,
  timestamp,
  unique,
  uniqueIndex,
  uuid,
} from "drizzle-orm/pg-core";
import { user, webSchema } from "./schema.auth";

/**
 * The app-owned directory: schema `web` holds EVERY identity, policy, and product row —
 * devices, workspace + seats, invitations, bundles, channels, subscriptions, per-device
 * state, notices, proposals, audit. The plane schema (read-only from this tier) holds byte
 * custody only and joins on opaque ids, never FKs.
 *
 * Integrity posture:
 *   · Same-workspace coherence is FK-ENFORCED: bundle and channel expose (id, workspace_id)
 *     composite keys, and every row that pairs them carries workspace_id pinned by composite
 *     FKs — a channel can never carry another workspace's bundle, a subscription can never
 *     name a foreign bundle. (Residual: device is deliberately workspace-less, so
 *     device_exclusion / device_bundle_state pin bundle-side only.)
 *   · Standing policy rows anchor to SEAT, not user: deleting a seat cascades away the
 *     member's channel memberships, subscriptions, and opt-outs — revocation is ONE row
 *     delete, and a later re-invite starts clean (no resurrected follows). Lapse RECORDS
 *     (bundle_detachment, audit_event) deliberately do NOT anchor to seat: they exist to
 *     survive removal.
 *   · In-lane protections (CHECKs, FKs, the revoke trigger) are BUG-guards: the app role
 *     owns its schema; append-only tables are append-only by code discipline + review
 *     gates, like every mainstream self-hosted product. The cross-lane boundary (the app
 *     cannot write plane; the vault cannot read web) stays grant-enforced.
 *
 * Validation placement: routes/ceremonies PARSE (types, friendly errors, product rules);
 * this schema is the TYPE of persistent state (integrity constraints + concurrency
 * invariants); one-line CHECKs are tripwires (charset / canonicalization / hash length),
 * never procedural validation.
 */

/** SHA-256 digests are stored as raw 32-byte bytea, hashed IN Postgres. */
const bytea = customType<{ data: Buffer; driverData: Buffer }>({
  dataType() {
    return "bytea";
  },
});

// ── Devices — a device is a POSSESSION of a user, never an identity ─────────────────────────

export const device = webSchema.table(
  "device",
  {
    /** 'dk_…', server-derived. */
    id: text("id").primaryKey(),
    userId: text("user_id")
      .notNull()
      .references(() => user.id, { onDelete: "cascade" }),
    displayName: text("display_name").notNull(),
    /** SHA-256 of the one bearer credential; the plaintext is delivered once and never stored. */
    credentialSha256: bytea("credential_sha256").notNull().unique(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    updatedAt: timestamp("updated_at", { withTimezone: true })
      .defaultNow()
      .$onUpdate(() => /* @__PURE__ */ new Date())
      .notNull(),
    lastSeenAt: timestamp("last_seen_at", { withTimezone: true }),
    /** Set-once; NULL = active. Finality is trigger-enforced (see the migration's raw SQL). */
    revokedAt: timestamp("revoked_at", { withTimezone: true }),
  },
  (table) => [
    index("device_user_idx").on(table.userId),
    check("device_credential_sha256_check", sql`octet_length(${table.credentialSha256}) = 32`),
  ],
);

/**
 * gh-style device-authorization flow; approval mints the device row atomically (the
 * FOR UPDATE-fenced approve+mint in the data layer). 'expired' is NOT a status — expiry is
 * expires_at, one source of truth. Flow state dies with its device (CASCADE): these are
 * short-TTL ceremony rows, not history (audit_event holds the record).
 */
export const deviceAuthSession = webSchema.table(
  "device_auth_session",
  {
    id: text("id").primaryKey(),
    /** The short human code the person types at /verify. */
    userCode: text("user_code").notNull(),
    deviceCodeSha256: bytea("device_code_sha256").notNull().unique(),
    requestedName: text("requested_name").notNull(),
    /**
     * The workspace ADDRESS SLUG the authorize call named ('' only as the single-tenant
     * origin-addressed form). Stored, never resolved at mint time: the flow's workspace is
     * looked up — and the approver's seat in it required — at approval, under the same lock.
     */
    requestedWorkspace: text("requested_workspace").default("").notNull(),
    /**
     * The RESOLVED workspace id, persisted by the approval inside its fence — the granted
     * poll's `workspace` decoration reads THIS immutable id, never a re-resolution of the
     * mutable slug (a rename or delete+recreate inside the TTL must not re-point the flow).
     */
    approvedWorkspaceId: text("approved_workspace_id"),
    /**
     * SHA-256 of the invitation token a `follow <invite-url>` enrollment carries — recorded
     * UNVALIDATED at the unauthenticated start (no token oracle); the approval resolves it
     * under its own fence and weaves accept-the-invitation into the same transaction.
     */
    inviteTokenSha256: bytea("invite_token_sha256"),
    status: text("status").default("pending").notNull(),
    approvedBy: text("approved_by").references(() => user.id, { onDelete: "set null" }),
    deviceId: text("device_id").references(() => device.id, { onDelete: "cascade" }),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    expiresAt: timestamp("expires_at", { withTimezone: true }).notNull(),
  },
  (table) => [
    uniqueIndex("device_auth_live_code").on(table.userCode).where(sql`status = 'pending'`),
    index("device_auth_expires_idx").on(table.expiresAt),
    check(
      "device_auth_session_device_code_sha256_check",
      sql`octet_length(${table.deviceCodeSha256}) = 32`,
    ),
    check(
      "device_auth_session_invite_token_sha256_check",
      sql`${table.inviteTokenSha256} is null or octet_length(${table.inviteTokenSha256}) = 32`,
    ),
    check(
      "device_auth_session_status_check",
      sql`${table.status} in ('pending', 'approved', 'denied')`,
    ),
    check(
      "device_auth_session_approved_check",
      sql`${table.status} <> 'approved' or ${table.deviceId} is not null`,
    ),
  ],
);

/**
 * The device↔workspace LINK — a first-class row, severable by both sides. A device is
 * REGISTERED once (device ↔ server, user-owned: the `device` table above) and LINKED per
 * workspace: authorization on the device lane runs credential → un-revoked device → owner's
 * seat → LIVE LINK. Links are DELETED, never tombstoned (no ghost rows — history lives in
 * audit_event alone); `pending` is the device-approval knob's holding state (an owner approves
 * on the fleet page, or the link was created by an owner and is born active).
 */
export const deviceLink = webSchema.table(
  "device_link",
  {
    /** 'dl_…', server-minted. */
    id: text("id").primaryKey(),
    deviceId: text("device_id")
      .notNull()
      .references(() => device.id, { onDelete: "cascade" }),
    workspaceId: text("workspace_id")
      .notNull()
      .references(() => workspace.id, { onDelete: "cascade" }),
    status: text("status").default("pending").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    unique("device_link_device_id_workspace_id_unique").on(table.deviceId, table.workspaceId),
    index("device_link_workspace_idx").on(table.workspaceId),
    index("device_link_device_idx").on(table.deviceId),
    check("device_link_status_check", sql`${table.status} in ('pending', 'active')`),
  ],
);

// ── Workspace + membership ───────────────────────────────────────────────────────────────────

export const workspace = webSchema.table(
  "workspace",
  {
    id: text("id").primaryKey(),
    /** The address slug. */
    name: text("name").notNull().unique(),
    displayName: text("display_name").notNull(),
    /** Unclaimed carries a live setup-code hash; claimed carries none (CHECK below). */
    claimCodeSha256: bytea("claim_code_sha256"),
    claimedAt: timestamp("claimed_at", { withTimezone: true }),
    protectionDefault: text("protection_default").default("open").notNull(),
    /**
     * Milliseconds, deliberately: the sole consumer is this tier (interval would round-trip
     * through string parsing); the _ms suffix keeps the unit honest in both worlds.
     */
    stalenessWindowMs: bigint("staleness_window_ms", { mode: "number" })
      .default(604800000)
      .notNull(),
    registration: text("registration").default("invite_only").notNull(),
    /**
     * The device-approval knob: 'on' makes a non-owner's new device link born 'pending' until
     * an owner approves it on the fleet page. Off by default; an owner's own act is always its
     * own approval.
     */
    deviceApproval: text("device_approval").default("off").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    updatedAt: timestamp("updated_at", { withTimezone: true })
      .defaultNow()
      .$onUpdate(() => /* @__PURE__ */ new Date())
      .notNull(),
  },
  (table) => [
    check(
      "workspace_name_check",
      sql`${table.name} ~ '^[a-z0-9][a-z0-9-]*$' and length(${table.name}) <= 100`,
    ),
    check(
      "workspace_claim_code_sha256_check",
      sql`${table.claimCodeSha256} is null or octet_length(${table.claimCodeSha256}) = 32`,
    ),
    check(
      "workspace_protection_default_check",
      sql`${table.protectionDefault} in ('open', 'reviewed')`,
    ),
    check("workspace_registration_check", sql`${table.registration} in ('invite_only', 'open')`),
    check("workspace_device_approval_check", sql`${table.deviceApproval} in ('off', 'on')`),
    check(
      "workspace_claim_state_check",
      sql`(${table.claimedAt} is null) <> (${table.claimCodeSha256} is null)`,
    ),
  ],
);

export const seat = webSchema.table(
  "seat",
  {
    workspaceId: text("workspace_id")
      .notNull()
      .references(() => workspace.id, { onDelete: "cascade" }),
    userId: text("user_id")
      .notNull()
      .references(() => user.id, { onDelete: "cascade" }),
    role: text("role").notNull(),
    invitedBy: text("invited_by").references(() => user.id, { onDelete: "set null" }),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    // Last-owner lockout: the FOR UPDATE-fenced ceremony in the data layer, not a constraint.
    primaryKey({ columns: [table.workspaceId, table.userId] }),
    index("seat_user_idx").on(table.userId),
    check("seat_role_check", sql`${table.role} in ('owner', 'reviewer', 'member')`),
  ],
);

/**
 * A claim on a FUTURE user; requires armed SMTP; binds at verified sign-up → seat, OR redeems
 * through the tokened invite link (only the token's SHA-256 is stored — the claim-code
 * pattern; re-inviting mints a fresh token over the pending row, killing the old link).
 * expires_at NULL = does not lapse; the ceremony sets the product's actual policy. An
 * invitation may carry ONE optional first-destination hint — a bundle OR a channel of its own
 * workspace (at most one; workspace coherence FK-pinned in the migration's raw SQL with a
 * per-column SET NULL, so deleting the hinted thing clears the hint and never the invitation).
 * The token hash is KEPT after consumption: the device-flow grant looks the accepted
 * invitation up by it to decorate the hint.
 */
export const invitation = webSchema.table(
  "invitation",
  {
    id: text("id").primaryKey(),
    workspaceId: text("workspace_id")
      .notNull()
      .references(() => workspace.id, { onDelete: "cascade" }),
    email: text("email").notNull(),
    role: text("role").default("member").notNull(),
    status: text("status").default("pending").notNull(),
    /** SHA-256 of the single-use invite-link token; the plaintext travels only in the mail. */
    tokenSha256: bytea("token_sha256").unique(),
    /** The optional first-destination hint: at most one of the two references is set. */
    hintBundleId: text("hint_bundle_id"),
    hintChannelId: text("hint_channel_id"),
    invitedBy: text("invited_by").references(() => user.id, { onDelete: "set null" }),
    acceptedBy: text("accepted_by").references(() => user.id, { onDelete: "set null" }),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    expiresAt: timestamp("expires_at", { withTimezone: true }),
    acceptedAt: timestamp("accepted_at", { withTimezone: true }),
  },
  (table) => [
    // Email leads: the sign-up ceremony's lookup is BY EMAIL across the install.
    uniqueIndex("invitation_pending_once")
      .on(table.email, table.workspaceId)
      .where(sql`status = 'pending'`),
    check("invitation_email_check", sql`${table.email} = lower(${table.email})`),
    check("invitation_role_check", sql`${table.role} in ('owner', 'reviewer', 'member')`),
    check(
      "invitation_status_check",
      sql`${table.status} in ('pending', 'accepted', 'revoked', 'declined')`,
    ),
    check(
      "invitation_token_sha256_check",
      sql`${table.tokenSha256} is null or octet_length(${table.tokenSha256}) = 32`,
    ),
    check(
      "invitation_hint_one_check",
      sql`${table.hintBundleId} is null or ${table.hintChannelId} is null`,
    ),
    // Anchored on accepted_at, NOT accepted_by: accepted_by is SET NULL on user deletion,
    // and a CHECK on it would make that deletion impossible.
    check(
      "invitation_accepted_check",
      sql`(${table.status} = 'accepted') = (${table.acceptedAt} is not null)`,
    ),
  ],
);

// ── Bundles (naming lives HERE; the vault keys git refs on bundle.id, opaquely) ─────────────

/**
 * Lifecycle: active → archived → deleted (delete keeps the row as a tombstone so history FKs
 * survive; bytes are purged plane-side). Names are unique across EVERY status: archiving
 * renames to free the base name, base_name records the original so unarchive restores it
 * EXACTLY (no suffix parsing). protection NULL = inherit workspace.protection_default;
 * 'open'/'reviewed' = explicitly pinned per bundle (the protection gate reads the effective
 * value; publish on 'reviewed' downgrades to a proposal).
 */
export const bundle = webSchema.table(
  "bundle",
  {
    id: text("id").primaryKey(),
    workspaceId: text("workspace_id")
      .notNull()
      .references(() => workspace.id, { onDelete: "cascade" }),
    kind: text("kind").default("skill").notNull(),
    name: text("name").notNull(),
    displayName: text("display_name"),
    status: text("status").default("active").notNull(),
    protection: text("protection"),
    /** NULL unless archived/deleted. */
    baseName: text("base_name"),
    archivedAt: timestamp("archived_at", { withTimezone: true }),
    deletedAt: timestamp("deleted_at", { withTimezone: true }),
    createdBy: text("created_by").references(() => user.id, { onDelete: "set null" }),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    updatedAt: timestamp("updated_at", { withTimezone: true })
      .defaultNow()
      .$onUpdate(() => /* @__PURE__ */ new Date())
      .notNull(),
  },
  (table) => [
    unique("bundle_workspace_id_name_unique").on(table.workspaceId, table.name),
    // Composite-FK target (same-workspace coherence).
    unique("bundle_id_workspace_id_unique").on(table.id, table.workspaceId),
    check(
      "bundle_name_check",
      sql`${table.name} ~ '^[a-z0-9][a-z0-9-]*$' and length(${table.name}) <= 200`,
    ),
    check("bundle_status_check", sql`${table.status} in ('active', 'archived', 'deleted')`),
    check(
      "bundle_protection_check",
      sql`${table.protection} is null or ${table.protection} in ('open', 'reviewed')`,
    ),
    check(
      "bundle_deleted_check",
      sql`(${table.status} = 'deleted') = (${table.deletedAt} is not null)`,
    ),
    check(
      "bundle_archived_check",
      sql`${table.status} <> 'archived' or ${table.archivedAt} is not null`,
    ),
    check("bundle_base_name_check", sql`${table.baseName} is null or ${table.status} <> 'active'`),
  ],
);

export const bundleNameHint = webSchema.table(
  "bundle_name_hint",
  {
    workspaceId: text("workspace_id")
      .notNull()
      .references(() => workspace.id, { onDelete: "cascade" }),
    oldName: text("old_name").notNull(),
    bundleId: text("bundle_id")
      .notNull()
      .references(() => bundle.id, { onDelete: "cascade" }),
    renamedBy: text("renamed_by").references(() => user.id, { onDelete: "set null" }),
    renamedAt: timestamp("renamed_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.workspaceId, table.oldName] }),
    index("bundle_name_hint_bundle_idx").on(table.bundleId),
  ],
);

// ── Channels ─────────────────────────────────────────────────────────────────────────────────

/**
 * Every workspace is born with its default channel ('everyone', is_default = true — one per
 * workspace, partial-unique-enforced). Default-channel membership is IMPLICIT: every seat,
 * MINUS explicit self opt-outs (channel_optout below) — no channel_member rows are
 * materialized for it (no seat↔member double bookkeeping to drift). Deleting or renaming the
 * default channel is refused by the app ceremony.
 */
export const channel = webSchema.table(
  "channel",
  {
    id: text("id").primaryKey(),
    workspaceId: text("workspace_id")
      .notNull()
      .references(() => workspace.id, { onDelete: "cascade" }),
    name: text("name").notNull(),
    mode: text("mode").default("open").notNull(),
    isDefault: boolean("is_default").default(false).notNull(),
    createdBy: text("created_by").references(() => user.id, { onDelete: "set null" }),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    updatedAt: timestamp("updated_at", { withTimezone: true })
      .defaultNow()
      .$onUpdate(() => /* @__PURE__ */ new Date())
      .notNull(),
  },
  (table) => [
    unique("channel_workspace_id_name_unique").on(table.workspaceId, table.name),
    // Composite-FK target (same-workspace coherence).
    unique("channel_id_workspace_id_unique").on(table.id, table.workspaceId),
    uniqueIndex("channel_one_default").on(table.workspaceId).where(sql`is_default`),
    check("channel_mode_check", sql`${table.mode} in ('open', 'curated')`),
  ],
);

/**
 * Membership of NAMED channels (the default channel is implicit, above). Anchored to seat:
 * removing someone's seat removes their channel memberships in the same delete.
 */
export const channelMember = webSchema.table(
  "channel_member",
  {
    channelId: text("channel_id").notNull(),
    workspaceId: text("workspace_id").notNull(),
    userId: text("user_id").notNull(),
    addedBy: text("added_by").references(() => user.id, { onDelete: "set null" }),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.channelId, table.userId] }),
    index("channel_member_user_idx").on(table.userId, table.workspaceId),
    foreignKey({
      name: "channel_member_channel_fk",
      columns: [table.channelId, table.workspaceId],
      foreignColumns: [channel.id, channel.workspaceId],
    }).onDelete("cascade"),
    foreignKey({
      name: "channel_member_seat_fk",
      columns: [table.workspaceId, table.userId],
      foreignColumns: [seat.workspaceId, seat.userId],
    }).onDelete("cascade"),
  ],
);

/**
 * Self opt-out of the DEFAULT channel — the one negative membership row: named channels need
 * none (leaving = deleting your channel_member row), the default channel has no row to
 * delete, so absence lives here. Personal act, self-service both ways (opting back in
 * deletes the row); writes a detach record (cause 'channel_leave') for the bundles it
 * lapses. Only meaningful for is_default channels — the app writes it for no other.
 * Seat-anchored: removal + re-invite starts clean.
 */
export const channelOptout = webSchema.table(
  "channel_optout",
  {
    channelId: text("channel_id").notNull(),
    workspaceId: text("workspace_id").notNull(),
    userId: text("user_id").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.channelId, table.userId] }),
    index("channel_optout_user_idx").on(table.userId, table.workspaceId),
    foreignKey({
      name: "channel_optout_channel_fk",
      columns: [table.channelId, table.workspaceId],
      foreignColumns: [channel.id, channel.workspaceId],
    }).onDelete("cascade"),
    foreignKey({
      name: "channel_optout_seat_fk",
      columns: [table.workspaceId, table.userId],
      foreignColumns: [seat.workspaceId, seat.userId],
    }).onDelete("cascade"),
  ],
);

export const channelBundle = webSchema.table(
  "channel_bundle",
  {
    channelId: text("channel_id").notNull(),
    workspaceId: text("workspace_id").notNull(),
    bundleId: text("bundle_id").notNull(),
    addedBy: text("added_by").references(() => user.id, { onDelete: "set null" }),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.channelId, table.bundleId] }),
    index("channel_bundle_bundle_idx").on(table.bundleId),
    foreignKey({
      name: "channel_bundle_channel_fk",
      columns: [table.channelId, table.workspaceId],
      foreignColumns: [channel.id, channel.workspaceId],
    }).onDelete("cascade"),
    foreignKey({
      name: "channel_bundle_bundle_fk",
      columns: [table.bundleId, table.workspaceId],
      foreignColumns: [bundle.id, bundle.workspaceId],
    }).onDelete("cascade"),
  ],
);

// ── Person-scoped subscription state ────────────────────────────────────────────────────────

/**
 * ONE standing stance per (person, bundle) — 'following' (direct follow; survives a channel
 * dropping the bundle) or 'unfollowed' (the negative mask subtracted from the channel-derived
 * union). One row per pair makes "follows AND unfollowed" unrepresentable. Anchored to seat:
 * losing the seat deletes the stance rows — delivery authority ends with membership, and a
 * re-invite starts clean. installed(device) = ((default channels − opt-outs) + member
 * channels' bundles ∪ following) − unfollowed − this device's exclusions.
 */
export const bundleSubscription = webSchema.table(
  "bundle_subscription",
  {
    userId: text("user_id").notNull(),
    workspaceId: text("workspace_id").notNull(),
    bundleId: text("bundle_id").notNull(),
    state: text("state").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    /** When the current state took effect. */
    updatedAt: timestamp("updated_at", { withTimezone: true })
      .defaultNow()
      .$onUpdate(() => /* @__PURE__ */ new Date())
      .notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.userId, table.bundleId] }),
    index("bundle_subscription_bundle_idx").on(table.bundleId),
    check("bundle_subscription_state_check", sql`${table.state} in ('following', 'unfollowed')`),
    foreignKey({
      name: "bundle_subscription_seat_fk",
      columns: [table.workspaceId, table.userId],
      foreignColumns: [seat.workspaceId, seat.userId],
    }).onDelete("cascade"),
    foreignKey({
      name: "bundle_subscription_bundle_fk",
      columns: [table.bundleId, table.workspaceId],
      foreignColumns: [bundle.id, bundle.workspaceId],
    }).onDelete("cascade"),
  ],
);

/**
 * The detach RECORD — "delivery lapsed; every device keeps its bytes; the copies are yours."
 * A record of a lapse, never a mask (a later re-follow or channel join simply re-attaches;
 * the app clears the record then). Deliberately NOT seat-anchored: membership_removed is one
 * of its causes, so it must survive the seat's deletion for the fleet page to chase detached
 * copies. Dies with the user or the bundle tombstone's hard purge.
 */
export const bundleDetachment = webSchema.table(
  "bundle_detachment",
  {
    userId: text("user_id")
      .notNull()
      .references(() => user.id, { onDelete: "cascade" }),
    workspaceId: text("workspace_id").notNull(),
    bundleId: text("bundle_id").notNull(),
    cause: text("cause").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.userId, table.bundleId] }),
    index("bundle_detachment_ws_idx").on(table.workspaceId, table.bundleId),
    check(
      "bundle_detachment_cause_check",
      sql`${table.cause} in ('unfollow', 'channel_leave', 'membership_removed')`,
    ),
    foreignKey({
      name: "bundle_detachment_bundle_fk",
      columns: [table.bundleId, table.workspaceId],
      foreignColumns: [bundle.id, bundle.workspaceId],
    }).onDelete("cascade"),
  ],
);

// ── Per-device state ─────────────────────────────────────────────────────────────────────────

/**
 * Device-scoped INTENT — "not on THIS machine" (the `remove` verb); person-scoped following
 * cannot express it, and without it the fleet cannot distinguish excluded from drifted. Kept
 * distinct from device_bundle_state below: exclusion is user-written intent, state is
 * device-reported fact — different writers, different lifecycles. (device is
 * workspace-less, so this table pins bundle-side coherence only.)
 */
export const deviceExclusion = webSchema.table(
  "device_exclusion",
  {
    deviceId: text("device_id")
      .notNull()
      .references(() => device.id, { onDelete: "cascade" }),
    bundleId: text("bundle_id")
      .notNull()
      .references(() => bundle.id, { onDelete: "cascade" }),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.deviceId, table.bundleId] }),
    index("device_exclusion_bundle_idx").on(table.bundleId),
  ],
);

/**
 * Fleet truth: device × applied version (version id is an opaque plane digest). The
 * reconcile only UPSERTS rows for delivered bundles and never deletes: a row whose bundle
 * has left the device's install set is the frozen "last known" record — the fleet derives
 * detached/excluded (vs merely behind) by joining bundle_detachment / device_exclusion
 * over it.
 */
export const deviceBundleState = webSchema.table(
  "device_bundle_state",
  {
    deviceId: text("device_id")
      .notNull()
      .references(() => device.id, { onDelete: "cascade" }),
    bundleId: text("bundle_id")
      .notNull()
      .references(() => bundle.id, { onDelete: "cascade" }),
    appliedVersionId: text("applied_version_id").notNull(),
    reportedAt: timestamp("reported_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.deviceId, table.bundleId] }),
    index("device_bundle_state_bundle_idx").on(table.bundleId),
  ],
);

// ── Notices ──────────────────────────────────────────────────────────────────────────────────

export const notice = webSchema.table(
  "notice",
  {
    id: bigint("id", { mode: "number" }).primaryKey().generatedAlwaysAsIdentity(),
    userId: text("user_id")
      .notNull()
      .references(() => user.id, { onDelete: "cascade" }),
    workspaceId: text("workspace_id")
      .notNull()
      .references(() => workspace.id, { onDelete: "cascade" }),
    kind: text("kind").notNull(),
    /** Display snapshots ride here. */
    payload: jsonb("payload").default(sql`'{}'::jsonb`).notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    ackedAt: timestamp("acked_at", { withTimezone: true }),
  },
  (table) => [
    index("notice_inbox").on(table.userId, table.workspaceId).where(sql`acked_at is null`),
    // The workspace-CASCADE path.
    index("notice_ws_idx").on(table.workspaceId),
  ],
);

// ── Review workflow (references plane versions by opaque digest) ────────────────────────────

export const proposal = webSchema.table(
  "proposal",
  {
    id: text("id").primaryKey(),
    workspaceId: text("workspace_id").notNull(),
    bundleId: text("bundle_id").notNull(),
    candidateVersionId: text("candidate_version_id").notNull(),
    proposedBy: text("proposed_by").references(() => user.id, { onDelete: "set null" }),
    status: text("status").default("open").notNull(),
    resolvedBy: text("resolved_by").references(() => user.id, { onDelete: "set null" }),
    resolvedReason: text("resolved_reason"),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    resolvedAt: timestamp("resolved_at", { withTimezone: true }),
  },
  (table) => [
    index("proposal_open").on(table.workspaceId, table.bundleId).where(sql`status = 'open'`),
    // At most ONE open proposal per candidate: a concurrent re-propose of the same bytes
    // converges on the existing row (the data-layer insert rides ON CONFLICT), so the review
    // inbox never shows two identical open proposals.
    uniqueIndex("proposal_one_open_per_candidate")
      .on(table.workspaceId, table.bundleId, table.candidateVersionId)
      .where(sql`status = 'open'`),
    check(
      "proposal_status_check",
      sql`${table.status} in ('open', 'approved', 'rejected', 'withdrawn')`,
    ),
    check(
      "proposal_resolved_check",
      sql`(${table.status} = 'open') = (${table.resolvedAt} is null)`,
    ),
    foreignKey({
      name: "proposal_bundle_fk",
      columns: [table.bundleId, table.workspaceId],
      foreignColumns: [bundle.id, bundle.workspaceId],
    }).onDelete("cascade"),
  ],
);

/**
 * Working state for N-reviewer approval. reviewer CASCADEs with the user by design: the
 * durable "who approved" record is the audit_event row, not this working row.
 */
export const approval = webSchema.table(
  "approval",
  {
    proposalId: text("proposal_id")
      .notNull()
      .references(() => proposal.id, { onDelete: "cascade" }),
    reviewer: text("reviewer")
      .notNull()
      .references(() => user.id, { onDelete: "cascade" }),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.proposalId, table.reviewer] }),
    index("approval_reviewer_idx").on(table.reviewer),
  ],
);

/**
 * Review-thread comments on a proposal — append-only by design (no edit/delete surface
 * exists, so a thread reads as an honest record). The id is CLIENT-minted (a page-render
 * UUID riding a hidden field), so the PK doubles as the idempotency key — a retried submit
 * lands ONE row via ON CONFLICT DO NOTHING. `version_id` is the candidate's opaque digest —
 * the proposal's identity on every review surface; the thread follows the bytes, so a real
 * rebase re-parents into a different candidate id and gets a fresh thread. Authorship is a
 * user id + a display snapshot (readable after renames/deletes).
 */
export const proposalComment = webSchema.table(
  "proposal_comment",
  {
    id: uuid("id").primaryKey(),
    workspaceId: text("workspace_id").notNull(),
    bundleId: text("bundle_id").notNull(),
    versionId: text("version_id").notNull(),
    authorUserId: text("author_user_id").references(() => user.id, {
      onDelete: "set null",
    }),
    authorDisplay: text("author_display").notNull(),
    body: text("body").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    index("proposal_comment_thread_idx").on(
      table.workspaceId,
      table.bundleId,
      table.versionId,
      table.createdAt,
    ),
    check("proposal_comment_body_check", sql`char_length(${table.body}) between 1 and 4000`),
    foreignKey({
      name: "proposal_comment_bundle_fk",
      columns: [table.bundleId, table.workspaceId],
      foreignColumns: [bundle.id, bundle.workspaceId],
    }).onDelete("cascade"),
  ],
);

// ── Audit + idempotency ──────────────────────────────────────────────────────────────────────

/**
 * Append-only by code discipline (no app path updates or deletes audit rows — review-gated);
 * survives workspace/user deletion (no FK on workspace_id; actor FKs SET NULL, actor_display
 * keeps history readable after renames/deletes). Every mutating data-layer op emits its row
 * in the same transaction.
 */
export const auditEvent = webSchema.table(
  "audit_event",
  {
    id: bigint("id", { mode: "number" }).primaryKey().generatedAlwaysAsIdentity(),
    workspaceId: text("workspace_id").notNull(),
    actorUserId: text("actor_user_id").references(() => user.id, { onDelete: "set null" }),
    actorDeviceId: text("actor_device_id").references(() => device.id, {
      onDelete: "set null",
    }),
    actorDisplay: text("actor_display").notNull(),
    kind: text("kind").notNull(),
    subject: text("subject"),
    outcome: text("outcome").notNull(),
    details: jsonb("details").default(sql`'{}'::jsonb`).notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    index("audit_ws_time").on(table.workspaceId, table.createdAt),
    index("audit_actor_user").on(table.actorUserId).where(sql`actor_user_id is not null`),
    index("audit_actor_device").on(table.actorDeviceId).where(sql`actor_device_id is not null`),
  ],
);

/**
 * The metadata-only mail send log — ONE row per send attempt through the one transport
 * (transport.server.ts), so an operator surface can answer "did the invite mail send".
 * DELIBERATELY metadata-only: kind, recipient, outcome, and at most a coarse machine code —
 * NEVER the subject, body, token, or relay response (a mail body can carry a live credential,
 * and the coarse-failure posture of the transport extends to its log). A SYSTEM write with no
 * actor: mail leaves the server, not a workspace, so the row is server-global by design.
 * Append-only by code discipline, like audit_event; no retention sweep yet.
 */
export const mailEvent = webSchema.table(
  "mail_event",
  {
    id: bigint("id", { mode: "number" }).primaryKey().generatedAlwaysAsIdentity(),
    /** Which product flow produced the mail (invite / auth-verify / auth-reset / magic-link). */
    kind: text("kind").notNull(),
    recipient: text("recipient").notNull(),
    outcome: text("outcome").notNull(),
    /** The coarse machine code on a failure ('unconfigured' | 'send_failed') — never relay text. */
    code: text("code"),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    index("mail_event_time_idx").on(table.createdAt),
    // The closed kind vocabulary — mirrors MAIL_EVENT_KINDS in mail-log.server.ts, so a drifted
    // caller refuses at the boundary instead of polluting the log.
    check(
      "mail_event_kind_check",
      sql`${table.kind} in ('magic-link', 'invite', 'auth-verify', 'auth-reset')`,
    ),
    check("mail_event_outcome_check", sql`${table.outcome} in ('ok', 'failed')`),
    check(
      "mail_event_code_check",
      sql`${table.code} is null or ${table.code} in ('unconfigured', 'send_failed')`,
    ),
    check(
      "mail_event_code_on_failure_check",
      sql`${table.outcome} = 'failed' or ${table.code} is null`,
    ),
  ],
);

/**
 * Device-op idempotency slots (same op_id replays the same outcome). Insert-once by code
 * discipline; the app's retention sweep deletes by age (the index below).
 */
export const opReceipt = webSchema.table(
  "op_receipt",
  {
    workspaceId: text("workspace_id").notNull(),
    deviceId: text("device_id")
      .notNull()
      .references(() => device.id, { onDelete: "cascade" }),
    opId: uuid("op_id").notNull(),
    requestSha256: bytea("request_sha256").notNull(),
    outcome: jsonb("outcome").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.workspaceId, table.deviceId, table.opId] }),
    // The retention sweep.
    index("op_receipt_retention_idx").on(table.createdAt),
    check("op_receipt_request_sha256_check", sql`octet_length(${table.requestSha256}) = 32`),
  ],
);

// ── Relations (query-layer navigation; the FKs above are the integrity) ─────────────────────

export const deviceRelations = relations(device, ({ one }) => ({
  owner: one(user, { fields: [device.userId], references: [user.id] }),
}));

export const seatRelations = relations(seat, ({ one }) => ({
  workspace: one(workspace, { fields: [seat.workspaceId], references: [workspace.id] }),
  user: one(user, { fields: [seat.userId], references: [user.id] }),
}));

export const bundleRelations = relations(bundle, ({ one }) => ({
  workspace: one(workspace, { fields: [bundle.workspaceId], references: [workspace.id] }),
}));

export const channelRelations = relations(channel, ({ one }) => ({
  workspace: one(workspace, { fields: [channel.workspaceId], references: [workspace.id] }),
}));
