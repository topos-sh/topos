import { bigint, boolean, integer, pgSchema, text, timestamp } from "drizzle-orm/pg-core";

/**
 * READ-ONLY Drizzle mirror of the gateway's WEB-READABLE tables — the rows the connected server's
 * page renders (which sign-in stands, which tools the server has offered, what agents actually
 * called). The gateway owns schema `gateway` and migrates it itself; this tier holds SELECT on
 * exactly the tables below, grant-enforced. DELIBERATELY excluded from drizzle-kit generation
 * (drizzle.config.ts lists only schema.auth.ts + schema.app.ts): the web tier never generates DDL
 * against these rows.
 *
 * WHAT IS NOT HERE IS THE POINT. `gateway.credential_secret` (the ciphertext) and
 * `gateway.workspace_key` (the wrapped data key) carry NO grant to this role and no mirror here:
 * the web tier can see THAT a sign-in exists and who connected it, never what it is. Changing a
 * credential is a lane call to the gateway, never a write from this tier — this schema is read
 * models only, and every table below is SELECT-only by grant as well as by convention.
 *
 * There are no cross-schema FKs in either direction: `workspace_id` / `server_id` / `session_id` /
 * `user_id` here are the same opaque ids the `web` schema minted, joined by equality.
 */
const gateway = pgSchema("gateway");

/**
 * ONE SIGN-IN'S METADATA — never its secret. `user_id` NULL is the workspace service account (the
 * sign-in an owner connected on the workspace's behalf); a row with a user id is that person's own.
 */
export const gatewayCredential = gateway.table("credential", {
  id: text("id").primaryKey(),
  workspaceId: text("workspace_id").notNull(),
  serverId: text("server_id").notNull(),
  /** NULL = the workspace service account; set = that person's own sign-in. */
  userId: text("user_id"),
  /** How it was obtained: the authorize walk, or a secret a person pasted. */
  authKind: text("auth_kind").notNull(),
  createdAt: timestamp("created_at", { withTimezone: true }).notNull(),
  /** Last successful token refresh — absent for a `manual` secret, which never rotates itself. */
  lastRefreshedAt: timestamp("last_refreshed_at", { withTimezone: true }),
});

/**
 * A TOOL THE SERVER HAS ACTUALLY OFFERED, as the gateway saw it in a live `tools/list` answer.
 * This is observation, not configuration: nothing here is authoritative about what a server can
 * do, and a server that has never been called through the gateway has no rows at all.
 */
export const gatewayObservedTool = gateway.table("observed_tool", {
  workspaceId: text("workspace_id").notNull(),
  serverId: text("server_id").notNull(),
  name: text("name").notNull(),
  description: text("description"),
  firstSeenAt: timestamp("first_seen_at", { withTimezone: true }).notNull(),
  lastSeenAt: timestamp("last_seen_at", { withTimezone: true }).notNull(),
  /** Whether the most recent listing still carried it. */
  currentlyOffered: boolean("currently_offered").notNull(),
});

/**
 * ONE CALL THROUGH THE GATEWAY — person, machine, tool, outcome, when. Never arguments, never
 * results, never a header value: the ledger records that a call happened and how it ended, which
 * is the whole of what the Usage table shows.
 *
 * `user_id` is NULL for a call a MACHINE TOKEN made (CI holding the workspace's sign-in): the
 * session id then names a `web.service_session` rather than a `web.cli_session`, and there is no
 * person to attribute it to.
 */
export const gatewayUsageEvent = gateway.table("usage_event", {
  id: bigint("id", { mode: "number" }).primaryKey(),
  workspaceId: text("workspace_id").notNull(),
  serverId: text("server_id").notNull(),
  sessionId: text("session_id").notNull(),
  /** NULL = a machine token's call; set = the person whose session made it. */
  userId: text("user_id"),
  /** NULL for a non-tool method (initialize, a listing, a notification). */
  toolName: text("tool_name"),
  method: text("method").notNull(),
  outcome: text("outcome").notNull(),
  durationMs: integer("duration_ms").notNull(),
  createdAt: timestamp("created_at", { withTimezone: true }).notNull(),
});
