import { bigint, pgSchema, text, timestamp } from "drizzle-orm/pg-core";

/**
 * READ-ONLY Drizzle mirror of the vault's custody-state tables — the columns the app's
 * delivery/fleet/history queries join against. The vault owns schema `plane` and migrates it
 * itself; this tier holds SELECT only (grant-enforced — the app role cannot write or ALTER
 * plane). DELIBERATELY excluded from drizzle-kit generation (drizzle.config.ts): the web
 * tier never generates DDL against these rows.
 *
 * The vault is identity-free: workspace_id / bundle_id here are the OPAQUE ids the app
 * supplied on write, and attribution columns are pass-through display text. Joins from web
 * tables use these ids; there are no cross-schema FKs in either direction.
 */
const plane = pgSchema("plane");

/** A version IS the hash of its bytes (content-addressed). */
export const planeVersion = plane.table("version", {
  workspaceId: text("workspace_id").notNull(),
  bundleId: text("bundle_id").notNull(),
  versionId: text("version_id").notNull(),
  commitId: text("commit_id").notNull(),
  authorDisplay: text("author_display").notNull(),
  createdAt: timestamp("created_at", { withTimezone: true }).notNull(),
  /** Byte-purge tombstone; the hash stays. */
  purgedAt: timestamp("purged_at", { withTimezone: true }),
});

/** The movable 'current', CAS-fenced by generation. */
export const planeCurrentPointer = plane.table("current_pointer", {
  workspaceId: text("workspace_id").notNull(),
  bundleId: text("bundle_id").notNull(),
  versionId: text("version_id").notNull(),
  generation: bigint("generation", { mode: "number" }).notNull(),
  movedByDisplay: text("moved_by_display").notNull(),
  movedAt: timestamp("moved_at", { withTimezone: true }).notNull(),
});

/** The consent digest of a version's file tree — what delivery pins for the client's re-hash. */
export const planeVersionDigest = plane.table("version_digest", {
  workspaceId: text("workspace_id").notNull(),
  bundleId: text("bundle_id").notNull(),
  versionId: text("version_id").notNull(),
  bundleDigest: text("bundle_digest").notNull(),
});
