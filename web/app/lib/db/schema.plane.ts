import { bigint, customType, pgSchema, text } from "drizzle-orm/pg-core";

/**
 * READ-ONLY Drizzle mirror of the directory's `plane` schema — the columns this tier renders
 * or feeds to the guarded SQL functions. The plane's own SQL migrations (the Rust repo's
 * `crates/plane-store/migrations/`) are the single source of truth; this mirror is
 * drift-checked against them by a unit test that applies those migrations to a scratch
 * database and introspects every table modeled here.
 *
 * The web role's write path into this schema is EXCLUSIVELY the guarded `topos_*` SQL
 * functions (invoked via `callGuarded` in queries.server.ts) — policy logic lives in the
 * database, written once; this tier never re-implements a role gate. Vault tables (`current`,
 * `skill_commit`, `proposals`) are SELECT-only by grant.
 */
export const plane = pgSchema("plane");

/** 32-byte content ids (commit ids, digests) — hex-string view over BYTEA. */
const hexBytea = customType<{ data: string; driverData: Buffer }>({
  dataType: () => "bytea",
  fromDriver: (value) => value.toString("hex"),
  toDriver: (value) => Buffer.from(value, "hex"),
});

export const planeWorkspace = plane.table("workspace", {
  workspaceId: text("workspace_id").primaryKey(),
  displayName: text("display_name").notNull(),
  verifiedDomain: text("verified_domain"),
  verifiedDomainStatus: text("verified_domain_status").notNull(),
  deploymentMode: text("deployment_mode").notNull(),
  createdAt: text("created_at").notNull(),
  /** The unique URL address slug joining and sharing speak. */
  name: text("name").notNull(),
});

export const planeWorkspaceMember = plane.table("workspace_member", {
  workspaceId: text("workspace_id").notNull(),
  principal: text("principal").notNull(),
  role: text("role", { enum: ["owner", "reviewer", "member"] }).notNull(),
  status: text("status", { enum: ["invited", "confirmed"] }).notNull(),
  invitedBy: text("invited_by"),
  addedAt: text("added_at").notNull(),
});

/** The name→skill catalog: identity is the immutable `skill_id`; `name` is the user key. */
export const planeCatalog = plane.table("catalog", {
  workspaceId: text("workspace_id").notNull(),
  skillId: text("skill_id").notNull(),
  name: text("name").notNull(),
  displayName: text("display_name"),
  status: text("status", { enum: ["active", "archived", "deleted"] }).notNull(),
  /** The bundle kind — `'skill'` for everything today; an OPEN vocabulary, display-only (never branched on). */
  kind: text("kind").notNull(),
  protection: text("protection", { enum: ["open", "reviewed"] }),
  baseName: text("base_name"),
  archivedAt: bigint("archived_at", { mode: "number" }),
  deletedAt: bigint("deleted_at", { mode: "number" }),
  createdAt: text("created_at").notNull(),
});

/** The one movable pointer per skill (vault table — SELECT only). */
export const planeCurrent = plane.table("current", {
  workspaceId: text("workspace_id").notNull(),
  skillId: text("skill_id").notNull(),
  commitId: hexBytea("commit_id").notNull(),
  epoch: bigint("epoch", { mode: "number" }).notNull(),
  seq: bigint("seq", { mode: "number" }).notNull(),
  updatedAt: bigint("updated_at", { mode: "number" }).notNull(),
});

/** Version provenance rows (vault table — SELECT only); purged versions keep a tombstone. */
export const planeSkillCommit = plane.table("skill_commit", {
  workspaceId: text("workspace_id").notNull(),
  commitId: hexBytea("commit_id").notNull(),
  skillId: text("skill_id").notNull(),
  bundleDigest: hexBytea("bundle_digest"),
  purgedAt: bigint("purged_at", { mode: "number" }),
  purgedBy: text("purged_by"),
});

/** Proposal rows (vault table — SELECT only); the review ceremony's row surface. */
export const planeProposals = plane.table("proposals", {
  workspaceId: text("workspace_id").notNull(),
  id: text("id").notNull(),
  skillId: text("skill_id").notNull(),
  commitId: hexBytea("commit_id").notNull(),
  baseCommitId: hexBytea("base_commit_id").notNull(),
  baseEpoch: bigint("base_epoch", { mode: "number" }).notNull(),
  baseSeq: bigint("base_seq", { mode: "number" }).notNull(),
  status: text("status", { enum: ["open", "accepted", "rejected", "closed"] }).notNull(),
  proposer: text("proposer").notNull(),
  resolvedBy: text("resolved_by"),
  createdAt: text("created_at").notNull(),
  resolvedReason: text("resolved_reason"),
  resolvedAt: text("resolved_at"),
});

export const planeWorkspacePolicy = plane.table("workspace_policy", {
  workspaceId: text("workspace_id").primaryKey(),
  reviewRequired: bigint("review_required", { mode: "number" }).notNull(),
  invitePolicy: text("invite_policy", { enum: ["members", "owners"] }).notNull(),
  stalenessWindowMs: bigint("staleness_window_ms", { mode: "number" }).notNull(),
});

export const planeChannels = plane.table("channels", {
  workspaceId: text("workspace_id").notNull(),
  channelId: text("channel_id").notNull(),
  name: text("name").notNull(),
  mode: text("mode", { enum: ["open", "curated"] }).notNull(),
  builtin: bigint("builtin", { mode: "number" }).notNull(),
  createdBy: text("created_by"),
  createdAt: text("created_at").notNull(),
});

/** The skill references a channel holds (labels, not folders — one skill, delivered once). */
export const planeChannelSkills = plane.table("channel_skills", {
  workspaceId: text("workspace_id").notNull(),
  channelId: text("channel_id").notNull(),
  skillId: text("skill_id").notNull(),
  addedBy: text("added_by").notNull(),
  addedAt: text("added_at").notNull(),
});

/** Person-scoped channel membership (`everyone` is structural — it has NO rows here). */
export const planeChannelMembers = plane.table("channel_members", {
  workspaceId: text("workspace_id").notNull(),
  channelId: text("channel_id").notNull(),
  principal: text("principal").notNull(),
  addedBy: text("added_by"),
  addedAt: text("added_at").notNull(),
});

/** The append-only, trigger-emitted channel audit — the history page's read (SELECT only). */
export const planeChannelEvents = plane.table("channel_events", {
  id: bigint("id", { mode: "number" }).primaryKey(),
  workspaceId: text("workspace_id").notNull(),
  channelId: text("channel_id").notNull(),
  event: text("event").notNull(),
  skillId: text("skill_id"),
  principal: text("principal"),
  actor: text("actor").notNull(),
  createdAt: text("created_at").notNull(),
});

/** The fleet's applied-state rows; `detached = 1` is a FINAL detach record, frozen as written. */
export const planeDeviceSkillState = plane.table("device_skill_state", {
  workspaceId: text("workspace_id").notNull(),
  deviceKeyId: text("device_key_id").notNull(),
  skillId: text("skill_id").notNull(),
  appliedCommit: hexBytea("applied_commit"),
  reportedAt: bigint("reported_at", { mode: "number" }).notNull(),
  detached: bigint("detached", { mode: "number" }).notNull(),
  detachedAt: bigint("detached_at", { mode: "number" }),
});

/** Rename redirects: an old catalog name that keeps resolving (and the rename's audit record). */
export const planeCatalogNameHints = plane.table("catalog_name_hints", {
  workspaceId: text("workspace_id").notNull(),
  name: text("name").notNull(),
  skillId: text("skill_id").notNull(),
  renamedBy: text("renamed_by").notNull(),
  createdAt: text("created_at").notNull(),
});

export const planeDeviceRegistry = plane.table("device_registry", {
  workspaceId: text("workspace_id").notNull(),
  deviceKeyId: text("device_key_id").notNull(),
  principal: text("principal").notNull(),
  revoked: bigint("revoked", { mode: "number" }).notNull(),
  lastReportAt: bigint("last_report_at", { mode: "number" }),
});
