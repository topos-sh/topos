import { sql } from "drizzle-orm";
import type { AnyPgColumn } from "drizzle-orm/pg-core";
import {
  bigint,
  boolean,
  check,
  customType,
  foreignKey,
  index,
  integer,
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
 * sessions, workspace + seats, invitations, bundles, channels, assignments + declines, notices,
 * proposals, audit. The plane schema (read-only from this tier) holds byte custody only and
 * joins on opaque ids, never FKs.
 *
 * The delivery model is DEMAND ∩ ENTITLEMENT:
 *   · Entitlement is the SEAT — a seat grants read access to the whole workspace catalog
 *     (git-clone-level trust). Channels are curated bundle SETS, never access control.
 *   · Demand, server-side, is the person's FEED: positive `assignment` rows (a bundle or a
 *     channel, aimed at one person or at EVERYONE) minus their own `decline` rows. The
 *     workspace baseline is the default channel assigned to everyone — an ordinary row, not a
 *     special case. Project-side demand stays in `topos.toml` files the client resolves; the
 *     server never learns project paths.
 *
 * Integrity posture:
 *   · Same-workspace coherence is FK-ENFORCED: bundle and channel expose (id, workspace_id)
 *     composite keys, and every row that pairs them carries workspace_id pinned by composite
 *     FKs — a channel can never carry another workspace's bundle, an assignment can never
 *     name a foreign bundle.
 *   · Standing policy rows anchor to SEAT, not user: deleting a seat cascades away the
 *     member's assignments and declines AND their sessions — revocation is ONE row delete, and
 *     a later re-invite starts clean.
 *   · In-lane protections (CHECKs, FKs) are BUG-guards: the app role owns its schema;
 *     append-only tables are append-only by code discipline + review gates. The cross-lane
 *     boundary (the app cannot write plane; the vault cannot read web) stays grant-enforced.
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
     * The session-approval knob: 'on' makes a non-owner's new session born 'pending' until an
     * owner approves it on the sessions page. Off by default; an owner's own act is always its
     * own approval.
     */
    sessionApproval: text("session_approval").default("off").notNull(),
    /**
     * The owner-set session expiry policy: a session older than this refuses (guard-time
     * check) and must log in again. NULL = sessions do not expire (the default — the
     * credential's lifetime is revocation, like a gh CLI login).
     */
    sessionMaxAgeMs: bigint("session_max_age_ms", { mode: "number" }),
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
    check("workspace_session_approval_check", sql`${table.sessionApproval} in ('off', 'on')`),
    check(
      "workspace_session_max_age_check",
      sql`${table.sessionMaxAgeMs} is null or ${table.sessionMaxAgeMs} > 0`,
    ),
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

// ── Sessions — user × workspace × installation (the ONE credentialed principal) ─────────────

/**
 * A SESSION is the credentialed attachment of one topos installation to one workspace, as one
 * person: minted by `topos login <workspace-address>` through the browser-approval flow, its
 * ONE bearer credential is WORKSPACE-SCOPED (a second workspace is a second login, a second
 * session, a second credential). Named `cli_session` because Better Auth owns `web.session`
 * (the browser session); the product noun is just "session".
 *
 * Revocable from BOTH sides — the user (self-service: `topos logout`, the account page) and
 * workspace owners (the sessions page: stolen device, offboarding) — and DELETED, never
 * tombstoned (history = cause-tagged audit). Seat-anchored by composite FK: removing the seat
 * cascades the person's sessions in that workspace away in the same delete. `pending` is the
 * session-approval knob's holding state (delivers nothing until an owner approves).
 */
export const cliSession = webSchema.table(
  "cli_session",
  {
    /** 'sn_…', server-minted. */
    id: text("id").primaryKey(),
    workspaceId: text("workspace_id").notNull(),
    userId: text("user_id").notNull(),
    /** The installation's self-reported label ("topos CLI (hostname)") — display only. */
    displayName: text("display_name").notNull(),
    /** SHA-256 of the one bearer credential; the plaintext is delivered once and never stored. */
    credentialSha256: bytea("credential_sha256").notNull().unique(),
    status: text("status").default("active").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    lastSeenAt: timestamp("last_seen_at", { withTimezone: true }),
  },
  (table) => [
    index("cli_session_workspace_idx").on(table.workspaceId),
    index("cli_session_user_idx").on(table.userId),
    foreignKey({
      name: "cli_session_seat_fk",
      columns: [table.workspaceId, table.userId],
      foreignColumns: [seat.workspaceId, seat.userId],
    }).onDelete("cascade"),
    check("cli_session_status_check", sql`${table.status} in ('pending', 'active')`),
    check("cli_session_credential_sha256_check", sql`octet_length(${table.credentialSha256}) = 32`),
  ],
);

/**
 * The gh-style login flow (browser approval). The flow starts WORKSPACE-LESS: the workspace is
 * chosen (or created) at the browser approval, where the approver's seats are known, and the
 * SESSION is minted at the CLI's exchange — the first poll that finds the flow approved.
 * 'expired' is NOT a status — expiry is expires_at, one source of truth. Flow state dies with
 * its session (CASCADE): these are short-TTL ceremony rows, not history (audit_event holds the
 * record).
 */
export const loginFlow = webSchema.table(
  "login_flow",
  {
    id: text("id").primaryKey(),
    /** The short human code the person types at /verify. */
    userCode: text("user_code").notNull(),
    flowCodeSha256: bytea("flow_code_sha256").notNull().unique(),
    requestedName: text("requested_name").notNull(),
    /**
     * The workspace ADDRESS SLUG a `login <workspace>` shortcut named — a PRESELECTION for the
     * browser chooser, recorded shape-checked but UNRESOLVED (the unauthenticated start is
     * never an existence oracle) and display-only: the approval records the workspace the
     * human actually chose, never this hint.
     */
    preselectWorkspace: text("preselect_workspace"),
    /**
     * The CHOSEN workspace id, persisted by the approval inside its fence — the granted
     * poll's `workspace` decoration reads THIS immutable id, never a re-resolution of any
     * mutable slug (a rename or delete+recreate inside the TTL must not re-point the flow).
     */
    approvedWorkspaceId: text("approved_workspace_id"),
    /**
     * SHA-256 of the invitation token a `topos login <invite-url>` carries — recorded
     * UNVALIDATED at the unauthenticated start (no token oracle); the approval resolves it
     * under its own fence and weaves accept-the-invitation into the same transaction.
     */
    inviteTokenSha256: bytea("invite_token_sha256"),
    /**
     * HOW the approval outcome is ACCELERATED back — decided by the CLI at the unauthenticated
     * start and WRITE-ONCE thereafter:
     *
     *  - `device` (RFC 8628): the classic device grant. The short code is typed at /verify —
     *    the typing binds the approving human to the machine that asked — and the CLI's poll
     *    collects the outcome.
     *  - `loopback` (RFC 8252): the CLI has a browser on THIS machine and a listener on
     *    127.0.0.1. The approval redirect wakes that listener (state + outcome only — it
     *    carries no secret), so the waiting poll fires immediately instead of at its interval.
     *
     * Either way THE POLL IS THE ONE COMPLETION MECHANISM; the binding decides only whether
     * the /verify card may pre-arm from the URL-borne challenge (loopback) or demands the
     * typed code (device).
     */
    binding: text("binding").default("device").notNull(),
    status: text("status").default("pending").notNull(),
    approvedBy: text("approved_by").references(() => user.id, { onDelete: "set null" }),
    /**
     * The minted session — set at the EXCHANGE (the first poll after approval), not at the
     * approve: an approved row with a NULL session is consent recorded and nothing more, and
     * an approved flow never polled past its TTL mints nothing.
     */
    sessionId: text("session_id").references(() => cliSession.id, { onDelete: "cascade" }),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    expiresAt: timestamp("expires_at", { withTimezone: true }).notNull(),
  },
  (table) => [
    uniqueIndex("login_flow_live_code").on(table.userCode).where(sql`status = 'pending'`),
    index("login_flow_expires_idx").on(table.expiresAt),
    check("login_flow_flow_code_sha256_check", sql`octet_length(${table.flowCodeSha256}) = 32`),
    check(
      "login_flow_invite_token_sha256_check",
      sql`${table.inviteTokenSha256} is null or octet_length(${table.inviteTokenSha256}) = 32`,
    ),
    check("login_flow_status_check", sql`${table.status} in ('pending', 'approved', 'denied')`),
    check("login_flow_binding_check", sql`${table.binding} in ('device', 'loopback')`),
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
 * The hint PREFILLS the newcomer's profile on accept. The token hash is KEPT after
 * consumption: the login-flow grant looks the accepted invitation up by it to decorate the
 * hint.
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
    // The closed kind vocabulary — mirrors BUNDLE_KINDS in api/candidate.server.ts, so a kind no
    // client knows how to deliver cannot land in the catalog even past the door.
    check("bundle_kind_check", sql`${table.kind} in ('skill', 'mcp')`),
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

/**
 * A bundle's UPSTREAM — the external origin it was imported from (a fork that remembers its
 * parent): host + repo + path, recorded at publish when the published copy carries import
 * provenance, or by the web add-from-GitHub flow. One upstream per bundle; the server-side
 * checker polls it and imports new upstream bytes as ordinary PROPOSALS (external changes
 * ALWAYS propose — the outside world never moves `current`).
 */
export const bundleUpstream = webSchema.table(
  "bundle_upstream",
  {
    bundleId: text("bundle_id").primaryKey(),
    workspaceId: text("workspace_id").notNull(),
    /** 'github.com' today; the column keeps the door open without branching on it. */
    host: text("host").notNull(),
    /** 'owner/repo'. */
    repo: text("repo").notNull(),
    /** The subdirectory inside the repo ('' = the repo root). */
    path: text("path").default("").notNull(),
    license: text("license"),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    /** The checker's bookkeeping: when it last looked, and what commit it saw. */
    lastCheckedAt: timestamp("last_checked_at", { withTimezone: true }),
    lastSeenCommit: text("last_seen_commit"),
  },
  (table) => [
    index("bundle_upstream_ws_idx").on(table.workspaceId),
    foreignKey({
      name: "bundle_upstream_bundle_fk",
      columns: [table.bundleId, table.workspaceId],
      foreignColumns: [bundle.id, bundle.workspaceId],
    }).onDelete("cascade"),
    check("bundle_upstream_repo_check", sql`${table.repo} ~ '^[^/]+/[^/]+$'`),
  ],
);

/**
 * Which upstream commit a VERSION's bytes came from — absent on locally-edited versions, so
 * divergence from upstream is readable from the version history itself. version_id is the
 * plane's opaque content digest — no FK across the schema boundary, by design.
 */
export const versionUpstream = webSchema.table(
  "version_upstream",
  {
    workspaceId: text("workspace_id").notNull(),
    bundleId: text("bundle_id").notNull(),
    versionId: text("version_id").notNull(),
    commit: text("commit").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.bundleId, table.versionId] }),
    foreignKey({
      name: "version_upstream_bundle_fk",
      columns: [table.bundleId, table.workspaceId],
      foreignColumns: [bundle.id, bundle.workspaceId],
    }).onDelete("cascade"),
  ],
);

// ── MCP servers — the catalog, its revisions, and what a workspace connects to ──────────────

/**
 * ONE ROW PER MCP SERVER — the catalog this install keeps, modeled as a subregistry of the
 * official MCP registry.
 *
 * A server is not a folder of files, so it is not a bundle of bytes: it is a name, an address,
 * and the facts somebody VERIFIED about it. Those facts are rows, which is what makes them
 * queryable, correctable, and shared — a sign-in tier learned by walking a vendor's discovery
 * chain belongs to everyone who connects that server, not to whichever workspace happened to
 * publish it first.
 *
 * GLOBAL AND PRIVATE ARE ONE TABLE, told apart by `workspace_id`:
 *
 *  · NULL — the generic/public catalog. Its rows come from the committed catalog file (a boot
 *    sync reconciles the file into these rows) or are added by staff. Their `name` is unique
 *    among them (the partial index below), because that reverse-DNS name is the identity every
 *    surface resolves by and a second claimant would make a lookup a coin flip.
 *  · SET — private to that workspace, created and edited by its owners, never exported. Two
 *    workspaces may hold the same `name` privately: neither is addressed by anyone else, so
 *    neither can shadow the other.
 *
 * `manually_curated` is the ONE bit that decides how a public row tracks the file. A row nobody
 * has touched in the panel tracks the file automatically — each new file version becomes its
 * `current`. Once a staff member edits or promotes it, the row is manually curated and future
 * file versions land as NON-CURRENT proposals for staff to promote. A self-hosted install has no
 * panel, so it never sets this, and always tracks the file. Private rows carry it false and
 * ignore it — an owner's own server has no file to track.
 *
 * `auth_mode` is VERIFIED TRUTH, never a vendor's claim: `oauth` means the endpoint's own
 * discovery chain was walked and its authorization server advertises dynamic client
 * registration, so an agent finishes the sign-in alone. `manual` means it costs a person a
 * one-time step per machine, and `auth_note` is the one line saying what that step is. NULL is
 * the honest fourth state — nobody has established it yet — and a row carrying it is not
 * promotable to `current`. `scope_menu` records the scope options verification found — data this
 * release, enforced by nothing yet.
 *
 * `current_revision_id` is the pointer people receive. It is moved in the SAME transaction as
 * the revision that earns it, under a FOR UPDATE lock on this row — the fenced-invariant pattern
 * the rest of this schema uses, and the ONE promotion function is the only writer of it. The
 * composite key below pins the pointer to a revision OF THIS SERVER.
 */
export const mcpServer = webSchema.table(
  "mcp_server",
  {
    id: text("id").primaryKey(),
    /** NULL = the generic/public catalog; set = private to that workspace. */
    workspaceId: text("workspace_id").references(() => workspace.id, { onDelete: "cascade" }),
    /** The reverse-DNS identity (`io.github.acme/foo`) — the key every surface resolves by, and
     *  what the catalog file declares a server under. */
    name: text("name"),
    displayName: text("display_name").notNull(),
    description: text("description"),
    websiteUrl: text("website_url"),
    /** The brand mark this row flies, by KEY — never an image, never a remote URL. */
    icon: text("icon"),
    /**
     * The sign-in tier somebody ESTABLISHED, or NULL for a server where nobody has yet. Null is
     * never rendered as `none`: "the publisher said nothing" and "this server asks for nothing"
     * are different claims, and only one of them was made.
     */
    authMode: text("auth_mode"),
    /** What the person must do first, for a `manual` row — the whole reason such a row may
     *  stand in a catalog people receive from. */
    authNote: text("auth_note"),
    scopeMenu: jsonb("scope_menu"),
    /** `active` = on offer; `delisted` = deliberately taken off offer (relisting is its own act). */
    status: text("status").default("active").notNull(),
    /** A staff edit or promote flips this true; a file version then proposes instead of advancing.
     *  A self-hosted install never sets it, so it always tracks the file. */
    manuallyCurated: boolean("manually_curated").default(false).notNull(),
    currentRevisionId: text("current_revision_id"),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    updatedAt: timestamp("updated_at", { withTimezone: true })
      .defaultNow()
      .$onUpdate(() => /* @__PURE__ */ new Date())
      .notNull(),
  },
  (table) => [
    // THE PUBLIC NAMESPACE, as an index: one public row per name. Partial, so a workspace's
    // private server never collides with the catalog's or with another workspace's — nobody
    // addresses a private row by that name but its own workspace.
    uniqueIndex("mcp_server_public_name").on(table.name).where(sql`workspace_id is null`),
    // AND THE PRIVATE ONE, per workspace: two of a workspace's own servers under one name would
    // make its lookup a coin flip. Scoped to the workspace because the name is only ever
    // addressed inside it.
    uniqueIndex("mcp_server_private_name")
      .on(table.workspaceId, table.name)
      .where(sql`workspace_id is not null`),
    index("mcp_server_workspace_idx").on(table.workspaceId),
    check(
      "mcp_server_auth_mode_check",
      sql`${table.authMode} is null or ${table.authMode} in ('none', 'oauth', 'manual')`,
    ),
    check("mcp_server_status_check", sql`${table.status} in ('active', 'delisted')`),
    // A name is `namespace/name` — the shape the official format requires, as a tripwire (the
    // document gate is where a name is really judged).
    check("mcp_server_name_check", sql`${table.name} is null or ${table.name} ~ '^[^/]+/[^/]+$'`),
    foreignKey({
      name: "mcp_server_current_revision_fk",
      columns: [table.currentRevisionId, table.id],
      foreignColumns: [mcpServerRevision.id, mcpServerRevision.serverId],
    }),
  ],
);

/**
 * A SERVER'S VERSION HISTORY — append-only, immutable once written, one stream per server.
 *
 * MATURITY IS THE POINTER, NOT A COLUMN. The revision `mcp_server.current_revision_id` names is
 * the one people receive; every other revision is a proposal or history, told apart by `seq`
 * against the current (a higher seq than the current is a pending proposal, a lower one is
 * history). The single terminal state a revision carries of its own is `dismissed_at` — a staff
 * member declined this proposal, so it stops showing and the file re-offering the same document
 * never re-proposes it.
 *
 * `upstream_version` is the version string inside the document, and it is NULLABLE — null means
 * "no known version", the honest state of an editorial or self-maintained server whose document
 * carries no `version` field (we never fabricate one just to fill the column). Its uniqueness is
 * PARTIAL — `upstream_version IS NOT NULL` only. A document that names a version promises one
 * document per version, and a second under the same number is a contradiction this refuses; a
 * versionless revision is exempt, and Postgres treats nulls as distinct anyway.
 *
 * `schema_version` is the document's own `$schema`. The vocabulary of the ones this tier
 * understands lives in CODE, not in a CHECK here, and deliberately: supporting a newer schema is
 * reading its rules and writing the code that honors them, which no table migration can stand in
 * for. The write path refuses one it does not know (app/lib/db/queries.mcp-catalog.server.ts).
 *
 * `transport` and `url` are EXTRACTED from the document for querying — the same remote the
 * client would place, so a surface can show the address without parsing the document again.
 * They are null together for a server that offers only packages, which is a server a machine
 * installs rather than dials.
 */
export const mcpServerRevision = webSchema.table(
  "mcp_server_revision",
  {
    id: text("id").primaryKey(),
    // The reference is ANNOTATED because these two tables name each other: a server points at
    // its current revision and a revision belongs to a server, which is a cycle the type
    // inference cannot walk on its own.
    serverId: text("server_id")
      .notNull()
      .references((): AnyPgColumn => mcpServer.id, { onDelete: "cascade" }),
    /** Monotonic per server, from 1 — the history's reading order, minted under the row lock. */
    seq: integer("seq").notNull(),
    /** The `version` inside the document (never this row's own numbering); NULL when the document
     *  names none — an editorial or self-maintained server we never stamped a fake version on. */
    upstreamVersion: text("upstream_version"),
    schemaVersion: text("schema_version"),
    /** The `server.json` verbatim, in the official registry format. */
    document: jsonb("document").notNull(),
    transport: text("transport"),
    url: text("url"),
    /** What this plane saw when it asked — the probe's own four-word vocabulary. */
    probeOutcome: text("probe_outcome"),
    probedAt: timestamp("probed_at", { withTimezone: true }),
    /** The protocol versions the endpoint answered with — an internal fact, never a surface. */
    protocolVersions: jsonb("protocol_versions"),
    /** What automatic verification concluded: the format gate's result and the auth reprobe,
     *  including the whole OAuth discovery-chain walk that a claim of `oauth` rests on. */
    verification: jsonb("verification"),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    /** When this revision was last moved to `current`, and by whom — set by the ONE promotion
     *  function. A revision never promoted carries neither. */
    publishedAt: timestamp("published_at", { withTimezone: true }),
    /** Attribution as display text — the catalog outlives whichever account acted. */
    publishedBy: text("published_by"),
    /** A staff member declined this proposal — terminal, so it stops showing and the file never
     *  re-proposes the same document. */
    dismissedAt: timestamp("dismissed_at", { withTimezone: true }),
    dismissedBy: text("dismissed_by"),
  },
  (table) => [
    unique("mcp_server_revision_seq_unique").on(table.serverId, table.seq),
    // THE ID'S SHAPE IS LOAD-BEARING, so the database holds it rather than trusting every writer
    // to. A revision id TRAVELS: a machine reports back the revision it holds, and the door that
    // receives that report accepts the minted shape and nothing else — so a row written with any
    // other spelling would have that machine's WHOLE applied report refused, taking every other
    // bundle on it along, on every update, forever. The catalog sync, a hand-run repair and a
    // future importer all write ids; this is the one place that answers for all of them.
    check("mcp_server_revision_id_shape_check", sql`${table.id} ~ '^mcpr_[0-9a-f]{32}$'`),
    // Composite-FK target: a pointer or a pin names a revision OF THE SERVER it belongs to.
    unique("mcp_server_revision_id_server_unique").on(table.id, table.serverId),
    // One document per version — for documents that name one. A versionless revision is exempt.
    uniqueIndex("mcp_server_revision_upstream_version")
      .on(table.serverId, table.upstreamVersion)
      .where(sql`upstream_version is not null`),
    index("mcp_server_revision_server_idx").on(table.serverId),
    check("mcp_server_revision_seq_check", sql`${table.seq} >= 1`),
    // The probe's vocabulary, shared with the delivery-independent report it has always been.
    check(
      "mcp_server_revision_probe_outcome_check",
      sql`${table.probeOutcome} is null or ${table.probeOutcome} in ('responding', 'sign_in_required', 'not_verifiable', 'not_responding')`,
    ),
    check(
      "mcp_server_revision_probed_check",
      sql`(${table.probeOutcome} is null) = (${table.probedAt} is null)`,
    ),
    // A placed remote is an address AND the transport it speaks, or neither.
    check(
      "mcp_server_revision_remote_check",
      sql`(${table.url} is null) = (${table.transport} is null)`,
    ),
    check("mcp_server_revision_url_check", sql`${table.url} is null or ${table.url} ~ '^https://'`),
    // Attribution rides with the promotion stamp: set together or not at all.
    check(
      "mcp_server_revision_published_by_check",
      sql`(${table.publishedAt} is null) = (${table.publishedBy} is null)`,
    ),
    // A dismissal is a fact with a name on it: set together or not at all.
    check(
      "mcp_server_revision_dismissed_by_check",
      sql`(${table.dismissedAt} is null) = (${table.dismissedBy} is null)`,
    ),
  ],
);

/**
 * THE CONNECTION — one workspace's use of one server, 1:1 with the `kind: 'mcp'` bundle row that
 * carries it.
 *
 * The bundle stays the anchor everything else already keys on: channels curate it, assignments
 * aim it, declines refuse it, a manifest references it by name. What the bundle no longer holds
 * is the DOCUMENT — this row names the server instead, and delivery resolves the bytes from it.
 *
 * `pinned_revision_id` NULL is the ordinary case: follow the server's `current_revision_id` and
 * receive corrections as they are published. A pin is the opposite promise, and the composite key
 * below is what keeps it a promise about THIS server's history rather than someone else's.
 *
 * `workspace_id` is denormalized off the bundle so the unique below can exist at all: ONE
 * connection per server per workspace, refused by the database rather than by a scan.
 */
export const bundleMcp = webSchema.table(
  "bundle_mcp",
  {
    bundleId: text("bundle_id").primaryKey(),
    workspaceId: text("workspace_id").notNull(),
    serverId: text("server_id")
      .notNull()
      .references(() => mcpServer.id),
    pinnedRevisionId: text("pinned_revision_id"),
    createdBy: text("created_by").references(() => user.id, { onDelete: "set null" }),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    unique("bundle_mcp_workspace_server_unique").on(table.workspaceId, table.serverId),
    index("bundle_mcp_server_idx").on(table.serverId),
    foreignKey({
      name: "bundle_mcp_bundle_fk",
      columns: [table.bundleId, table.workspaceId],
      foreignColumns: [bundle.id, bundle.workspaceId],
    }).onDelete("cascade"),
    // NO CASCADE from the server: a catalog row that workspaces are connected to is not
    // something a delete may quietly take their connections down with. The reference simply
    // stands in the way until the connections are gone.
    foreignKey({
      name: "bundle_mcp_pinned_revision_fk",
      columns: [table.pinnedRevisionId, table.serverId],
      foreignColumns: [mcpServerRevision.id, mcpServerRevision.serverId],
    }),
  ],
);

/**
 * WHICH OF A SERVER'S TOOLS THIS WORKSPACE'S AGENTS MAY USE — one row per connection, and the
 * connection is what it hangs off (the composite FK below names `bundle_mcp`'s unique
 * workspace/server pair, so disconnecting a server takes its policy with it).
 *
 * NO ROW IS `all`. A workspace that never opened the panel gets every tool the server offers,
 * which is what a connection already promised; the row exists only once somebody narrowed it.
 * `selected` reads the companion table below: a tool is enabled iff it has a row there, so a
 * tool the server adds later starts OUT — the safe direction, and the one the panel's copy
 * states outright.
 *
 * The gateway READS this per call (it holds SELECT on the two tables); nothing here is a
 * client-side hint. Policy is decided in one place and enforced where the call is made.
 */
export const mcpToolPolicy = webSchema.table(
  "mcp_tool_policy",
  {
    workspaceId: text("workspace_id").notNull(),
    serverId: text("server_id").notNull(),
    mode: text("mode").default("all").notNull(),
    /** Who last set it — display comes from the audit row; this is the id, as everywhere. */
    updatedBy: text("updated_by").references(() => user.id, { onDelete: "set null" }),
    updatedAt: timestamp("updated_at", { withTimezone: true })
      .defaultNow()
      .$onUpdate(() => /* @__PURE__ */ new Date())
      .notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.workspaceId, table.serverId] }),
    foreignKey({
      name: "mcp_tool_policy_connection_fk",
      columns: [table.workspaceId, table.serverId],
      foreignColumns: [bundleMcp.workspaceId, bundleMcp.serverId],
    }).onDelete("cascade"),
    check("mcp_tool_policy_mode_check", sql`${table.mode} in ('all', 'selected')`),
  ],
);

/**
 * ONE ENABLED TOOL under `selected` mode. A row IS the enablement; there is no disabled row and
 * no third state. Rows may name a tool the server no longer offers (a checked tool that
 * disappeared) — harmless, and keeping them means a tool that comes back keeps the answer the
 * workspace already gave. The cascade is to the POLICY row, so switching a connection back to
 * `all` is free to leave the selection standing, and deleting the policy clears it.
 */
export const mcpToolSelection = webSchema.table(
  "mcp_tool_selection",
  {
    workspaceId: text("workspace_id").notNull(),
    serverId: text("server_id").notNull(),
    toolName: text("tool_name").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.workspaceId, table.serverId, table.toolName] }),
    foreignKey({
      name: "mcp_tool_selection_policy_fk",
      columns: [table.workspaceId, table.serverId],
      foreignColumns: [mcpToolPolicy.workspaceId, mcpToolPolicy.serverId],
    }).onDelete("cascade"),
  ],
);

// ── Channels — named, curated BUNDLE SETS (nothing else; never access control) ──────────────

/**
 * Every workspace is born with its default channel ('everyone', is_default = true — one per
 * workspace, partial-unique-enforced) AND with that channel assigned to everyone: the BASELINE
 * is an ordinary assignment row, not a rule written into the query. A channel has NO
 * membership — people carry a channel because an assignment aims it at them (or at everyone),
 * projects by referencing it in `topos.toml`. `mode` gates who edits its references (open =
 * any member, curated = reviewer+ — the curation gate). Deleting or renaming the default
 * channel is refused by the app ceremony.
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

// ── The feed — assignments (the one positive row) and declines (the one negative one) ───────

/**
 * An ASSIGNMENT says one thing should reach one audience: a bundle or a channel (exactly one,
 * CHECK-enforced), aimed at ONE PERSON (`user_id`) or at EVERYONE in the workspace
 * (`user_id IS NULL`). It is born two ways and the row is identical either way — a curator
 * aims it at someone, or the person adds it to their own feed (`created_by = user_id`). There
 * are no strengths and no pins: every assignment is declinable, and delivery always serves the
 * workspace's `current`.
 *
 * The workspace BASELINE is not a rule — it is the default channel assigned to everyone, one
 * row minted with the workspace.
 *
 * PROVENANCE is part of the row's identity: `self` says whether the person placed it in their
 * own name (a pick) or someone else aimed it (a curator / everyone row). One self-row and one
 * curator-row over the same (person, target) COEXIST — the partial uniques include `self` — so
 * a curator's aim never collapses into (or is deleted by) the person's own pick: each unassign
 * arm removes only its own provenance, and delivery unions both.
 *
 * Same-workspace coherence is composite-FK-pinned on both targets. The seat FK is MATCH
 * SIMPLE by construction: it binds only when `user_id` is present, which is exactly the
 * intent — losing a seat cascades that person's assignments away, while everyone-rows (a
 * workspace-level fact) survive any roster change.
 */
export const assignment = webSchema.table(
  "assignment",
  {
    workspaceId: text("workspace_id")
      .notNull()
      .references(() => workspace.id, { onDelete: "cascade" }),
    /** NULL = everyone in the workspace. */
    userId: text("user_id"),
    bundleId: text("bundle_id"),
    channelId: text("channel_id"),
    /** The person placed it themselves (their own pick — theirs to take back). `false` = a
     * curator's aim, including every everyone-row (CHECK-enforced: an everyone-row is never
     * `self`). No default: every writer states the provenance it means. */
    self: boolean("self").notNull(),
    /** Who made it — an attribution snapshot (a `user.id`, or 'system' for a birth row); it
     * carries no FK so an assignment outlives the account that placed it. */
    createdBy: text("created_by").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
    updatedAt: timestamp("updated_at", { withTimezone: true })
      .defaultNow()
      .$onUpdate(() => /* @__PURE__ */ new Date())
      .notNull(),
  },
  (table) => [
    // One assignment per (audience, target, PROVENANCE) — partial uniques because a NULL
    // audience is its own key: person-scoped and everyone-scoped rows never collide with each
    // other, and a self-pick coexists with a curator's aim at the same target.
    uniqueIndex("assignment_person_bundle_once")
      .on(table.workspaceId, table.userId, table.bundleId, table.self)
      .where(sql`bundle_id is not null and user_id is not null`),
    uniqueIndex("assignment_everyone_bundle_once")
      .on(table.workspaceId, table.bundleId)
      .where(sql`bundle_id is not null and user_id is null`),
    uniqueIndex("assignment_person_channel_once")
      .on(table.workspaceId, table.userId, table.channelId, table.self)
      .where(sql`channel_id is not null and user_id is not null`),
    uniqueIndex("assignment_everyone_channel_once")
      .on(table.workspaceId, table.channelId)
      .where(sql`channel_id is not null and user_id is null`),
    index("assignment_ws_user_idx").on(table.workspaceId, table.userId),
    index("assignment_bundle_idx").on(table.bundleId),
    index("assignment_channel_idx").on(table.channelId),
    check(
      "assignment_target_check",
      sql`(${table.bundleId} is null) <> (${table.channelId} is null)`,
    ),
    // An everyone-row is never a self-pick: `self` implies a named person.
    check("assignment_self_check", sql`${table.userId} is not null or not ${table.self}`),
    foreignKey({
      name: "assignment_seat_fk",
      columns: [table.workspaceId, table.userId],
      foreignColumns: [seat.workspaceId, seat.userId],
    }).onDelete("cascade"),
    foreignKey({
      name: "assignment_bundle_fk",
      columns: [table.bundleId, table.workspaceId],
      foreignColumns: [bundle.id, bundle.workspaceId],
    }).onDelete("cascade"),
    foreignKey({
      name: "assignment_channel_fk",
      columns: [table.channelId, table.workspaceId],
      foreignColumns: [channel.id, channel.workspaceId],
    }).onDelete("cascade"),
  ],
);

/**
 * A DECLINE is the whole negative half of the model: this person does not want this BUNDLE,
 * whatever assigns it. Keyed to bundle IDENTITY, so it survives new versions, channel
 * reshuffles, and a curator re-assigning the same thing. There is no channel-level decline —
 * a set is declined one bundle at a time, which is what keeps "off" meaningful when the set's
 * contents change.
 *
 * Seat-anchored like every standing row: losing the seat clears the stance, and a later
 * re-invite starts clean.
 */
export const decline = webSchema.table(
  "decline",
  {
    workspaceId: text("workspace_id").notNull(),
    userId: text("user_id").notNull(),
    bundleId: text("bundle_id").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    unique("decline_user_id_bundle_id_unique").on(table.userId, table.bundleId),
    index("decline_ws_user_idx").on(table.workspaceId, table.userId),
    foreignKey({
      name: "decline_seat_fk",
      columns: [table.workspaceId, table.userId],
      foreignColumns: [seat.workspaceId, seat.userId],
    }).onDelete("cascade"),
    foreignKey({
      name: "decline_bundle_fk",
      columns: [table.bundleId, table.workspaceId],
      foreignColumns: [bundle.id, bundle.workspaceId],
    }).onDelete("cascade"),
  ],
);

// ── Per-session applied state ────────────────────────────────────────────────────────────────

/**
 * Applied-state truth: session × applied version (version id is an opaque plane digest). The
 * reconcile only UPSERTS rows for delivered bundles; rows die with the session (CASCADE — a
 * revoked or re-minted session re-reports fresh). The sessions page reads this for its
 * per-bundle applied state.
 */
export const sessionBundleState = webSchema.table(
  "session_bundle_state",
  {
    sessionId: text("session_id")
      .notNull()
      .references(() => cliSession.id, { onDelete: "cascade" }),
    bundleId: text("bundle_id")
      .notNull()
      .references(() => bundle.id, { onDelete: "cascade" }),
    appliedVersionId: text("applied_version_id").notNull(),
    /** The per-harness applied states a CONFIG-placed bundle reports ('mcp'): a
     * `[{slug, state, note?}]` snapshot of which detected agents hold the entry and how. NULL
     * for a file bundle, whose one applied version says everything. The state vocabulary is
     * OPEN — the client's word, stored verbatim for display, never branched on. */
    harnessState: jsonb("harness_state"),
    reportedAt: timestamp("reported_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.sessionId, table.bundleId] }),
    index("session_bundle_state_bundle_idx").on(table.bundleId),
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
 * in the same transaction. actor_session_id records WHICH installation acted when the act
 * came over the session lane; the row outlives the session (SET NULL). workspace_id is
 * NULLABLE for the few SERVER-scoped events (a login deny lands before any workspace is
 * chosen); workspace-scoped readers query by equality, so a NULL row never surfaces there.
 */
export const auditEvent = webSchema.table(
  "audit_event",
  {
    id: bigint("id", { mode: "number" }).primaryKey().generatedAlwaysAsIdentity(),
    workspaceId: text("workspace_id"),
    actorUserId: text("actor_user_id").references(() => user.id, { onDelete: "set null" }),
    actorSessionId: text("actor_session_id").references(() => cliSession.id, {
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
    index("audit_actor_session").on(table.actorSessionId).where(sql`actor_session_id is not null`),
    // The per-address invite-cooldown read (server-wide, subject = the invited address).
    index("audit_invite_subject")
      .on(table.subject, table.createdAt)
      .where(sql`kind = 'invitation_created'`),
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
 * Session-op idempotency slots (same op_id replays the same outcome). Insert-once by code
 * discipline; the app's retention sweep deletes by age (the index below).
 */
export const opReceipt = webSchema.table(
  "op_receipt",
  {
    workspaceId: text("workspace_id").notNull(),
    sessionId: text("session_id")
      .notNull()
      .references(() => cliSession.id, { onDelete: "cascade" }),
    opId: uuid("op_id").notNull(),
    requestSha256: bytea("request_sha256").notNull(),
    outcome: jsonb("outcome").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.workspaceId, table.sessionId, table.opId] }),
    // The retention sweep.
    index("op_receipt_retention_idx").on(table.createdAt),
    check("op_receipt_request_sha256_check", sql`octet_length(${table.requestSha256}) = 32`),
  ],
);
