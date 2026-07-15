import fs from "node:fs/promises";
import path from "node:path";
import { expect, test as setup } from "@playwright/test";
import { Client } from "pg";
import {
  BASE_URL,
  E2E_DATABASE_URL,
  E2E_PASSWORD,
  E2E_SETUP_CODE,
  MEMBER_EMAIL,
  PLANE_PORT,
  STORAGE_STATE,
} from "./env";
import { signIn } from "./sign-in";

/**
 * The setup project for the UNIFIED-IDENTITY app: the workspace is BOOT-MINTED (no plane
 * authority tables to seed anymore — identity/policy rows live in the app's own `web` schema),
 * registration is never open by default, and the FIRST user arrives through the printed claim
 * link. So this project:
 *
 *  1. loads one real page — the first document request runs the migrations AND `ensureSetup`,
 *     which mints the workspace with the PRESET claim code (`TOPOS_SETUP_CODE` in appEnv);
 *  2. claims the workspace AS the e2e member on a fresh database (the claim ceremony creates
 *     the account and seats it as the first OWNER — the app's own sign-up is refused outside
 *     claim/invitation/open by design); a reused local stack finds the workspace already
 *     claimed and skips to sign-in;
 *  3. flips `workspace.registration = 'open'` so specs that mint ADDITIONAL identities can
 *     sign them up through the normal flow (they receive accounts, not seats — a spec that
 *     needs a seated identity must seat it itself);
 *  4. signs the member in through the real email+password flow and persists the storage state;
 *  5. resets the web-tier leftovers (proposal comments) and the fixture vault's scopes.
 *
 * NOTE for the e2e pass that owns the specs: the default member is now the workspace OWNER
 * (the claimant), not a mid-roster reviewer — specs that assumed the old seeded roles need
 * their own arrangement.
 */

setup("claim the workspace and sign the member in", async ({ page, request }) => {
  await fs.mkdir(path.dirname(path.resolve(STORAGE_STATE)), { recursive: true });

  // Reset the fixture vault's in-memory scopes + recorded calls (a locally reused fixture server
  // must not carry one run's decisions into the next).
  await request.post(`http://127.0.0.1:${PLANE_PORT}/__test/seed`);

  // ONE document request boots the app: migrations + ensureSetup (the boot-minted workspace).
  await page.goto("/login");

  const db = new Client({ connectionString: E2E_DATABASE_URL });
  await db.connect();
  try {
    // Claim only while the code is live — a reused stack is already claimed.
    const { rows } = await db.query(
      `select claimed_at is null as claimable from workspace limit 1`,
    );
    expect(rows).toHaveLength(1);
    if (rows[0]?.claimable === true) {
      const claimed = await page.request.post(`/claim?code=${E2E_SETUP_CODE}`, {
        form: {
          code: E2E_SETUP_CODE,
          name: MEMBER_EMAIL.split("@")[0] ?? "member",
          email: MEMBER_EMAIL,
          password: E2E_PASSWORD,
        },
        headers: { origin: BASE_URL },
      });
      expect(claimed.ok(), `claim failed: ${claimed.status()} ${await claimed.text()}`).toBe(true);
    }

    // Open registration so specs can mint further identities through the normal sign-up flow
    // (accounts only — seats still come from claims/invitations/roster acts).
    await db.query(`update workspace set registration = 'open' where registration <> 'open'`);

    // Web-tier reset: the guard stays deterministic; the comments spec's counts stay exact.
    await db.query(`update "user" set email_verified = true where email = $1`, [MEMBER_EMAIL]);
    await db.query(`delete from proposal_comment`);
  } finally {
    await db.end();
  }

  // Sign the member in through the real email+password flow, then persist the session.
  await signIn(page, MEMBER_EMAIL);
  await page.context().storageState({ path: STORAGE_STATE });

  // Sanity: the claimed seat lands the member on their workspace shell.
  await expect(page.getByRole("banner")).toBeVisible();
});
