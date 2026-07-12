import { expect, type Page, test } from "@playwright/test";
import { Client } from "pg";
import {
  HINT_OLD_NAME,
  JOINER_EMAIL,
  L_CUR,
  L_GOOD,
  LIFECYCLE_ADDRESS,
  LIFECYCLE_ARCHIVED_AT_MS,
  LIFECYCLE_ARCHIVED_BASE,
  LIFECYCLE_ARCHIVED_NAME,
  LIFECYCLE_ARCHIVED_SKILL_ID,
  LIFECYCLE_GENERATION,
  LIFECYCLE_OWNER_EMAIL,
  LIFECYCLE_OWNER2_EMAIL,
  LIFECYCLE_RENAME_TO,
  LIFECYCLE_SKILL,
  LIFECYCLE_WS,
  SKILL,
  WS,
} from "../fixtures/plane/data.mjs";
import { E2E_ADMIN_URL, E2E_PASSWORD, PLANE_PORT } from "./env";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The SKILL LIFECYCLE ceremonies on the web surface: rename + archive (skill settings), unarchive +
 * delete (the archive page), and per-version purge (the history tab) — every one a step-up ceremony,
 * the destructive ones also gated by typing a name. Two backends, harness discipline: the vault
 * WRITES are internal-lane calls (the proof is the recorded wire call keyed on the immutable
 * skill_id), and the vault mock never edits the seeded directory rows, so a redirect target that
 * reads the DB may 404 — the tests assert the RECORDED CALL and the redirect URL, never a mid-suite
 * DB reflection of a vault move. The rename REDIRECT rides a real catalog_name_hints row.
 *
 * The directory rows for the lifecycle workspace (its two owners, an active 2-version skill, a
 * pre-archived skill) + the ws-e2e rename hint are seeded HERE against E2E_ADMIN_URL (never the
 * SELECT-only app URL), after the setup project's one-time base seed — additive, in a workspace no
 * other spec touches. The vault scope (fixtures/plane/data.mjs initialScopes) serves the reads and
 * records the writes. Serial so the per-process step-up belt spends predictably.
 */

test.describe.configure({ mode: "serial" });
test.use({ storageState: { cookies: [], origins: [] } });

const B = (hex: string) => Buffer.from(hex, "hex");

async function seedLifecycle(): Promise<void> {
  const db = new Client({ connectionString: E2E_ADMIN_URL });
  await db.connect();
  try {
    await db.query(
      `insert into plane.workspace
         (workspace_id, display_name, verified_domain, verified_domain_status, deployment_mode, created_at, name)
       values ($1, 'Lifecycle Workspace', null, 'unverified', 'cloud', '2026-07-05T09:00:00Z', $2)
       on conflict (workspace_id) do nothing`,
      [LIFECYCLE_WS, LIFECYCLE_ADDRESS],
    );
    await db.query(
      `insert into plane.workspace_policy (workspace_id, review_required) values ($1, 0)
       on conflict (workspace_id) do nothing`,
      [LIFECYCLE_WS],
    );
    for (const email of [LIFECYCLE_OWNER_EMAIL, LIFECYCLE_OWNER2_EMAIL]) {
      await db.query(
        `insert into plane.workspace_member (workspace_id, principal, role, status, invited_by, added_at)
         values ($1, $2, 'owner', 'confirmed', null, '2026-07-05T09:05:00Z')
         on conflict do nothing`,
        [LIFECYCLE_WS, email],
      );
    }
    // The ACTIVE skill (settings + history surfaces) and the pre-ARCHIVED skill (the archive page).
    await db.query(
      `insert into plane.catalog (workspace_id, skill_id, name, display_name, status, created_at)
       values ($1, $2, $2, null, 'active', '2026-07-05T09:00:00Z')
       on conflict do nothing`,
      [LIFECYCLE_WS, LIFECYCLE_SKILL],
    );
    await db.query(
      `insert into plane.catalog
         (workspace_id, skill_id, name, display_name, status, base_name, archived_at, created_at)
       values ($1, $2, $3, null, 'archived', $4, $5, '2026-07-05T09:00:00Z')
       on conflict do nothing`,
      [
        LIFECYCLE_WS,
        LIFECYCLE_ARCHIVED_SKILL_ID,
        LIFECYCLE_ARCHIVED_NAME,
        LIFECYCLE_ARCHIVED_BASE,
        LIFECYCLE_ARCHIVED_AT_MS,
      ],
    );
    // Provenance first (current FKs onto it), then the pointer at L_CUR with a readable L_GOOD parent.
    for (const commitId of [L_CUR, L_GOOD]) {
      await db.query(
        `insert into plane.skill_commit (workspace_id, commit_id, skill_id, bundle_digest)
         values ($1, $2, $3, $2) on conflict do nothing`,
        [LIFECYCLE_WS, B(commitId), LIFECYCLE_SKILL],
      );
    }
    await db.query(
      `insert into plane.current (workspace_id, skill_id, commit_id, epoch, seq, record, updated_at)
       values ($1, $2, $3, $4, $5, null, $6) on conflict do nothing`,
      [
        LIFECYCLE_WS,
        LIFECYCLE_SKILL,
        B(L_CUR),
        LIFECYCLE_GENERATION.epoch,
        LIFECYCLE_GENERATION.seq,
        Date.parse("2026-07-05T09:00:00Z"),
      ],
    );
    // The rename REDIRECT fixture: an old name on ws-e2e resolving to SKILL (the catalog row the
    // base seed already planted). GET …/skills/<old> follows the hint to <SKILL>.
    await db.query(
      `insert into plane.catalog_name_hints (workspace_id, name, skill_id, renamed_by, created_at)
       values ($1, $2, $3, $4, '2026-07-05T09:00:00Z')
       on conflict (workspace_id, name) do nothing`,
      [WS, HINT_OLD_NAME, SKILL, LIFECYCLE_OWNER_EMAIL],
    );
  } finally {
    await db.end();
  }
}

interface RecordedCall {
  route: string;
  ws: string;
  skill: string;
  acting: string;
  body: Record<string, unknown>;
}

async function recorded(page: Page, route: string): Promise<RecordedCall[]> {
  const response = await page.request.get(`http://127.0.0.1:${PLANE_PORT}/__test/calls`);
  const calls: RecordedCall[] = await response.json();
  return calls.filter((c) => c.route === route && c.ws === LIFECYCLE_WS);
}

test.beforeAll(async () => {
  await seedLifecycle();
});

test("archive: a wrong step-up password records no call; the right one archives and redirects", async ({
  page,
}) => {
  await signIn(page, LIFECYCLE_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${LIFECYCLE_WS}/skills/${LIFECYCLE_SKILL}/settings`);

  // A WRONG password: the ceremony refuses at step-up — no vault call is made.
  await page.locator("#archive-password").fill("not-the-password");
  await page.getByRole("button", { name: `Archive ${LIFECYCLE_SKILL}` }).click();
  await expect(page.getByText(/Password check failed/i)).toBeVisible();
  expect(await recorded(page, "archive")).toHaveLength(0);

  // The RIGHT password: the internal-lane archive call lands (keyed on the immutable skill_id) and
  // the page redirects to the archive list.
  await page.locator("#archive-password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: `Archive ${LIFECYCLE_SKILL}` }).click();
  await page.waitForURL(`**/workspaces/${LIFECYCLE_WS}/archive`);

  await expect.poll(async () => (await recorded(page, "archive")).length).toBeGreaterThan(0);
  const call = (await recorded(page, "archive")).at(-1);
  expect(call?.skill).toBe(LIFECYCLE_SKILL);
  expect(call?.acting).toBe(LIFECYCLE_OWNER_EMAIL);
});

test("rename: a valid new name renames and redirects to the new name's settings", async ({
  page,
}) => {
  await signIn(page, LIFECYCLE_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${LIFECYCLE_WS}/skills/${LIFECYCLE_SKILL}/settings`);

  await page.locator("#rename-new-name").fill(LIFECYCLE_RENAME_TO);
  await page.locator("#rename-password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Rename skill" }).click();

  // The redirect is the assertion (the DB catalog still holds the old name — harness discipline —
  // so the target may render the house 404; the URL is what we pin).
  await page.waitForURL(`**/workspaces/${LIFECYCLE_WS}/skills/${LIFECYCLE_RENAME_TO}/settings`);
  const call = (await recorded(page, "rename")).at(-1);
  expect(call?.skill).toBe(LIFECYCLE_SKILL);
  expect(call?.body.new_name).toBe(LIFECYCLE_RENAME_TO);
  expect(call?.acting).toBe(LIFECYCLE_OWNER_EMAIL);
});

test("delete: the archived name must be typed exactly before the byte drop", async ({ page }) => {
  await signIn(page, LIFECYCLE_OWNER2_EMAIL);
  await gotoSettled(page, `/workspaces/${LIFECYCLE_WS}/archive`);

  // The archived skill is listed by its archived name.
  await expect(page.getByText(LIFECYCLE_ARCHIVED_NAME).first()).toBeVisible();
  await page.getByText("Delete permanently…", { exact: true }).click();

  const confirm = page.locator(`#delete-${LIFECYCLE_ARCHIVED_SKILL_ID}-confirm`);
  const password = page.locator(`#delete-${LIFECYCLE_ARCHIVED_SKILL_ID}-password`);

  // The WRONG name (with the right password) is refused by the typed-name gate — no vault call.
  await confirm.fill("not-the-name");
  await password.fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Delete permanently" }).click();
  await expect(page.getByText(/Type the exact name/i)).toBeVisible();
  expect(await recorded(page, "delete")).toHaveLength(0);

  // The EXACT archived name: the delete call lands, keyed on the immutable skill_id.
  await confirm.fill(LIFECYCLE_ARCHIVED_NAME);
  await password.fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Delete permanently" }).click();

  await expect.poll(async () => (await recorded(page, "delete")).length).toBeGreaterThan(0);
  const call = (await recorded(page, "delete")).at(-1);
  expect(call?.skill).toBe(LIFECYCLE_ARCHIVED_SKILL_ID);
  expect(call?.acting).toBe(LIFECYCLE_OWNER2_EMAIL);
});

test("purge: a non-current version records the internal-lane purge call", async ({ page }) => {
  await signIn(page, LIFECYCLE_OWNER2_EMAIL);
  await gotoSettled(page, `/workspaces/${LIFECYCLE_WS}/skills/${LIFECYCLE_SKILL}/history`);

  const purgeSection = page.getByRole("region", { name: "Purge version bytes" });
  await expect(purgeSection).toBeVisible();
  await purgeSection.locator("summary").first().click();

  const short = L_GOOD.slice(0, 12);
  await page.locator(`#purge-${short}-confirm`).fill(LIFECYCLE_SKILL);
  await page.locator(`#purge-${short}-password`).fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Purge this version" }).click();

  await expect.poll(async () => (await recorded(page, "purge")).length).toBeGreaterThan(0);
  const call = (await recorded(page, "purge")).at(-1);
  expect(call?.skill).toBe(LIFECYCLE_SKILL);
  expect(call?.body.version_id).toBe(L_GOOD);
  expect(call?.acting).toBe(LIFECYCLE_OWNER2_EMAIL);
});

test("hint redirect: an old renamed name lands on the live skill name", async ({ page }) => {
  await signIn(page, JOINER_EMAIL);
  await gotoSettled(page, `/workspaces/${WS}/skills/${HINT_OLD_NAME}`);

  // The catalog_name_hints row resolves the old name to SKILL; the loader 302s to the live name.
  await page.waitForURL(`**/workspaces/${WS}/skills/${SKILL}`);
  await expect(page.getByRole("heading", { name: SKILL })).toBeVisible();
});
