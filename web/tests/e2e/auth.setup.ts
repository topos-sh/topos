import fs from "node:fs/promises";
import path from "node:path";
import { expect, test as setup } from "@playwright/test";
import { Client } from "pg";
import { PLANE_SEED, PLANE_SKILL_SEED } from "../fixtures/plane/data.mjs";
import { E2E_ADMIN_URL, E2E_DATABASE_URL, MEMBER_EMAIL, PLANE_PORT, STORAGE_STATE } from "./env";
import { signIn } from "./sign-in";

/**
 * Seeds the DIRECTORY (plane) authority rows, signs the e2e member in through the app's own
 * email+password flow (no session forgery), then resets the web tier's own state and the fixture
 * vault's in-memory scopes.
 *
 * DIRECTORY SEED: a superuser connection (E2E_ADMIN_URL — never the topos_web app URL, which is
 * SELECT-only on `plane` by design) TRUNCATEs the seeded tables and inserts plane.workspace +
 * plane.workspace_member + the catalog/current/skill_commit/proposals + workspace_policy rows from
 * the shared data.mjs constants — the single source the vault scopes also derive from. The
 * migrations themselves are applied once by db-setup.mjs (globalSetup); this only refreshes rows.
 *
 * HARNESS DISCIPLINE: specs assert VAULT-surface effects (review/revert wire calls) only after
 * fixture writes, and DB-surface effects (guards, dashboard rows, invited seats) ONLY from the
 * state seeded here (plus the DB-write invite path). The fixture's in-memory scopes and these
 * plane.* rows never sync mid-test.
 *
 * WEB SEED: sign-up marks the account verified (self-asserted — the OSS composition ships no
 * out-of-band delivery); the belt-and-braces update keeps the guard deterministic. The proposal
 * comment thread (web-tier state keyed by plane ids the seed reuses run to run) is emptied so the
 * comments spec's counts are exact.
 */

async function seedPlane(): Promise<void> {
  const db = new Client({ connectionString: E2E_ADMIN_URL });
  await db.connect();
  try {
    await db.query(`
      TRUNCATE plane.workspace, plane.workspace_member, plane.workspace_policy, plane.catalog,
        plane.skill_commit, plane.current, plane.proposals, plane.channels, plane.channel_members,
        plane.channel_skills, plane.channel_events, plane.notices, plane.skill_follows,
        plane.skill_unfollows, plane.device_exclusions, plane.device_registry
      RESTART IDENTITY CASCADE
    `);
    for (const ws of PLANE_SEED.workspaces) {
      await db.query(
        `insert into plane.workspace
           (workspace_id, display_name, verified_domain, verified_domain_status, deployment_mode, created_at, name)
         values ($1, $2, null, 'unverified', 'cloud', $3, $4)`,
        [ws.workspaceId, ws.displayName, ws.createdAt, ws.address],
      );
      // A policy row per workspace so the settings toggle + the fleet clock have one to read
      // (review off by default; readers COALESCE a missing row anyway).
      await db.query(
        `insert into plane.workspace_policy (workspace_id, review_required) values ($1, 0)`,
        [ws.workspaceId],
      );
    }
    for (const m of PLANE_SEED.members) {
      await db.query(
        `insert into plane.workspace_member
           (workspace_id, principal, role, status, invited_by, added_at)
         values ($1, $2, $3, $4, $5, $6)`,
        [m.workspaceId, m.principal, m.role, m.status, m.invitedBy, m.addedAt],
      );
    }
    // The catalog IS the identity surface: a skill exists the moment its name is minted.
    for (const c of PLANE_SKILL_SEED.catalog) {
      await db.query(
        `insert into plane.catalog (workspace_id, skill_id, name, display_name, status, created_at)
         values ($1, $2, $3, null, 'active', '2026-07-01T00:00:00Z')`,
        [c.ws, c.skillId, c.name],
      );
    }
    // Provenance first (current + proposals FK onto it). One row seeds a NULL bundle_digest (the
    // em-dash case); Buffers carry the raw 32-byte ids.
    for (const c of PLANE_SKILL_SEED.commits) {
      await db.query(
        `insert into plane.skill_commit (workspace_id, commit_id, skill_id, bundle_digest)
         values ($1, $2, $3, $4)`,
        [
          c.ws,
          Buffer.from(c.commitId, "hex"),
          c.skillId,
          c.bundleDigest === null ? null : Buffer.from(c.bundleDigest, "hex"),
        ],
      );
    }
    for (const cur of PLANE_SKILL_SEED.currents) {
      await db.query(
        `insert into plane.current (workspace_id, skill_id, commit_id, epoch, seq, record, updated_at)
         values ($1, $2, $3, $4, $5, null, $6)`,
        [
          cur.ws,
          cur.skillId,
          Buffer.from(cur.commitId, "hex"),
          cur.epoch,
          cur.seq,
          cur.updatedAtMs,
        ],
      );
    }
    // One proposal is deliberately STALE (open, base != current — the count-tripwire).
    for (const p of PLANE_SKILL_SEED.proposals) {
      await db.query(
        `insert into plane.proposals
           (workspace_id, id, skill_id, commit_id, base_commit_id, base_epoch, base_seq, status, proposer, resolved_by, created_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)`,
        [
          p.ws,
          p.id,
          p.skillId,
          Buffer.from(p.commitId, "hex"),
          Buffer.from(p.baseCommitId, "hex"),
          p.baseEpoch,
          p.baseSeq,
          p.status,
          p.proposer,
          p.resolvedBy,
          p.createdAt,
        ],
      );
    }
  } finally {
    await db.end();
  }
}

setup("sign in and seed the workspace", async ({ page, request }) => {
  await fs.mkdir(path.dirname(path.resolve(STORAGE_STATE)), { recursive: true });

  // Directory rows FIRST: the sign-in fast-path lands on a workspace that already reads them.
  await seedPlane();

  // Reset the fixture vault's in-memory scopes + recorded calls (a locally reused fixture server
  // must not carry one run's decisions into the next).
  await request.post(`http://127.0.0.1:${PLANE_PORT}/__test/seed`);

  // Sign the default member in through the real email+password flow, then persist the session.
  await signIn(page, MEMBER_EMAIL);
  await page.context().storageState({ path: STORAGE_STATE });

  // Web-tier reset. topos_web owns schema web, so it may write its own tables.
  const db = new Client({ connectionString: E2E_DATABASE_URL });
  await db.connect();
  try {
    await db.query(`update "user" set email_verified = true where email = $1`, [MEMBER_EMAIL]);
    await db.query(`delete from proposal_comment`);
  } finally {
    await db.end();
  }

  // Sanity: the seed + sign-in landed the member on their sole workspace.
  await expect(page.getByRole("banner")).toBeVisible();
});
