import { sql } from "drizzle-orm";
import { boolean, check, index, pgTable, text, timestamp, uuid } from "drizzle-orm/pg-core";

/**
 * The web tier's OWN tables — cloud-only state (the policy audit trail; Better Auth owns its
 * own set in schema.auth.ts). Membership, workspace names, and the skill catalog live on the
 * PLANE (`plane.workspace`, `plane.workspace_member`, `plane.current` — read via
 * schema.plane.ts); `workspace_id` here is a plain TEXT join key, never a foreign key: the web
 * tier holds no REFERENCES privilege on the authority tables and must never be able to veto an
 * authority-side delete.
 */

/** Audit trail for the review-required policy — one row per set attempt, whatever the outcome. */
export const policyEvent = pgTable(
  "policy_event",
  {
    id: uuid("id").primaryKey().defaultRandom(),
    /** The PLANE workspace id — a plain text join key (no FK into schema `plane`). */
    workspaceId: text("workspace_id").notNull(),
    reviewRequired: boolean("review_required").notNull(),
    setBy: text("set_by").notNull(),
    setAt: timestamp("set_at", { withTimezone: true }).defaultNow().notNull(),
    outcome: text("outcome").notNull(),
  },
  (table) => [
    check("policy_event_outcome_check", sql`${table.outcome} in ('ok', 'denied', 'error')`),
  ],
);

/**
 * Review-thread comments on a proposal — WEB-ONLY state (the plane never sees a comment; the
 * device lane has no comment surface). Append-only by design: no edit/delete surface exists, so
 * a thread reads as an honest record. The id is CLIENT-minted (a page-render UUID riding a
 * hidden field), so the PK doubles as the idempotency key — a retried submit lands ONE row via
 * ON CONFLICT DO NOTHING. `version_id` is the candidate's hex64 — the proposal's identity on
 * every review surface. The thread is DELIBERATELY keyed by the candidate version: it follows
 * the bytes, so a real rebase re-parents into a different candidate id and gets a fresh thread.
 */
export const proposalComment = pgTable(
  "proposal_comment",
  {
    id: uuid("id").primaryKey(),
    /** The PLANE workspace id — a plain text join key (no FK into schema `plane`). */
    workspaceId: text("workspace_id").notNull(),
    skillId: text("skill_id").notNull(),
    versionId: text("version_id").notNull(),
    authorEmail: text("author_email").notNull(),
    body: text("body").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).defaultNow().notNull(),
  },
  (table) => [
    check("proposal_comment_body_check", sql`char_length(${table.body}) between 1 and 4000`),
    index("proposal_comment_thread_idx").on(
      table.workspaceId,
      table.skillId,
      table.versionId,
      table.createdAt,
    ),
  ],
);
