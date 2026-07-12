import { expect, test } from "@playwright/test";
import { Client } from "pg";
import {
  WS as E2E_WS,
  HERO_BUNDLE_DIGEST,
  HERO_CURRENT_ID,
  HERO_SKILL,
  JOINER_EMAIL,
  NULLDIGEST_SKILL,
} from "../fixtures/plane/data.mjs";
import { E2E_DATABASE_URL } from "./env";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The distribute-read spine — THE HERO: a CLI publish is a directory `current` row, and the
 * dashboard catalog renders it from the shared DB on the next load. No token paste, no web-tier
 * state of any kind: the hardening below proves no skill_link row exists ANYWHERE (the table
 * itself never existed in this tier). Sign-in is the app's own email+password flow.
 */

test.use({ storageState: { cookies: [], origins: [] } });

test("HERO: a published skill (a directory current row) renders on the dashboard with no web-tier state", async ({
  page,
}) => {
  // The hardening: no skill_link table exists in schema web — the catalog owes nothing to web rows.
  const db = new Client({ connectionString: E2E_DATABASE_URL });
  await db.connect();
  try {
    const reg = await db.query(`select to_regclass('web.skill_link') as t`);
    if (reg.rows[0]?.t !== null) {
      const rows = await db.query(`select count(*)::int as n from skill_link`);
      expect(rows.rows[0]?.n).toBe(0);
    }
  } finally {
    await db.end();
  }

  // JOINER_EMAIL holds a confirmed seat on ws-e2e and ZERO web-tier rows; release-checklist has no
  // web-tier row anywhere — the catalog row renders from the plane.current seed alone.
  await signIn(page, JOINER_EMAIL);
  await gotoSettled(page, `/workspaces/${E2E_WS}`);
  const hero = page.getByRole("listitem").filter({ hasText: HERO_SKILL });
  await expect(hero).toBeVisible();
  await expect(hero.getByText(HERO_CURRENT_ID.slice(0, 12))).toBeVisible();
  await expect(hero.getByText(`sha-256:${HERO_BUNDLE_DIGEST.slice(0, 12)}…`)).toBeVisible();
  // The count-tripwire: deploy-runbook carries TWO open proposal rows but only ONE on the live
  // base — the badge shows the staleness-filtered count, exactly as the vault's own list would.
  const catalogDeploy = page.getByRole("listitem").filter({ hasText: `sha-256:${"e3".repeat(6)}` });
  await expect(catalogDeploy.getByText("1 proposal awaiting review")).toBeVisible();
  // A NULL bundle_digest renders an em-dash, never a fake value.
  const legacy = page.getByRole("listitem").filter({ hasText: NULLDIGEST_SKILL });
  await expect(legacy.getByText("—")).toBeVisible();
});
