import { expect, test } from "@playwright/test";
import { adminQuery, ensureBundle, seedCustody, theWorkspace } from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * The PROFILE editor ("Your skills") — the web face of the person-side manifest: the baseline
 * channel carried by default, the one-click drop/carry toggles, the direct add from the
 * catalog, and the exclude-line semantics (removing a channel-provided skill records the one
 * negative line; adding it back clears it). The suite's default identity is the claimed owner.
 */

const SKILL = { id: "s_e2e_prof", name: "profile-guide" };
const LONER = { id: "s_e2e_prof_loner", name: "profile-loner" };

test.describe.configure({ mode: "serial" });

test.beforeAll(async () => {
  const ws = await theWorkspace();
  await ensureBundle(SKILL);
  await ensureBundle(LONER);
  await seedCustody([
    {
      ws: ws.id,
      bundle: SKILL.id,
      versions: [{ files: [{ path: "SKILL.md", content: "# Profile guide\n" }], message: "v1" }],
      current: 0,
    },
    {
      ws: ws.id,
      bundle: LONER.id,
      versions: [{ files: [{ path: "SKILL.md", content: "# Loner\n" }], message: "v1" }],
      current: 0,
    },
  ]);
  // The guide rides the baseline; the loner sits catalog-only (the add picker's subject).
  await adminQuery(
    `INSERT INTO web.channel_bundle (channel_id, workspace_id, bundle_id)
     SELECT id, workspace_id, $1 FROM web.channel WHERE is_default AND workspace_id = $2
     ON CONFLICT DO NOTHING`,
    [SKILL.id, ws.id],
  );
  // A clean profile slate for the suite's owner (idempotent on a reused database).
  await adminQuery(
    `DELETE FROM web.profile_entry WHERE bundle_id = ANY($1::text[])
       OR channel_id IN (SELECT id FROM web.channel WHERE is_default AND workspace_id = $2)`,
    [[SKILL.id, LONER.id], ws.id],
  );
});

test("the baseline delivers by default; removing a baseline skill records the exclude", async ({
  page,
}) => {
  await gotoSettled(page, "/profile");
  await expect(page.getByRole("heading", { name: "Your skills" })).toBeVisible();

  // The baseline channel reads carried, and its skill is delivered.
  const everyone = page.getByTestId("profile-channel-everyone");
  await expect(everyone.getByText("baseline", { exact: true })).toBeVisible();
  await expect(everyone.getByText("in your skills", { exact: true })).toBeVisible();
  const delivered = page.getByTestId(`profile-delivered-${SKILL.name}`);
  await expect(delivered).toBeVisible();
  await expect(delivered.getByText("via everyone")).toBeVisible();

  // Removing it (the baseline still provides it) records the ONE negative line, disclosed.
  await delivered.getByRole("button", { name: "Remove" }).click();
  await expect(page.getByText("a personal exclude line now holds it back")).toBeVisible();
  await expect(page.getByTestId(`profile-delivered-${SKILL.name}`)).toHaveCount(0);
  await expect(
    page.getByRole("heading", { name: "Excluded by you" }).locator("xpath=.."),
  ).toContainText(SKILL.name);

  // Adding it back (the picker now offers it) clears the exclude — delivery resumes.
  await page
    .getByTestId(`profile-addable-${SKILL.name}`)
    .getByRole("button", { name: "Add" })
    .click();
  await expect(page.getByTestId(`profile-delivered-${SKILL.name}`)).toBeVisible();
});

test("dropping and carrying the baseline channel toggles the whole set", async ({ page }) => {
  await gotoSettled(page, "/profile");
  const everyone = page.getByTestId("profile-channel-everyone");
  await everyone.getByRole("button", { name: "Drop everyone" }).click();
  await expect(everyone.getByText("not carried", { exact: true })).toBeVisible();
  await everyone.getByRole("button", { name: "Carry everyone" }).click();
  await expect(everyone.getByText("in your skills", { exact: true })).toBeVisible();
});

test("a direct add from the catalog delivers and is removable outright", async ({ page }) => {
  await gotoSettled(page, "/profile");
  await page
    .getByTestId(`profile-addable-${LONER.name}`)
    .getByRole("button", { name: "Add" })
    .click();
  const delivered = page.getByTestId(`profile-delivered-${LONER.name}`);
  await expect(delivered).toBeVisible();
  await expect(delivered.getByText("added by you")).toBeVisible();
  // Nothing broader provides it, so the removal deletes the line — no exclude appears.
  await delivered.getByRole("button", { name: "Remove" }).click();
  await expect(page.getByTestId(`profile-delivered-${LONER.name}`)).toHaveCount(0);
  const rows = await adminQuery(`SELECT 1 FROM web.profile_entry WHERE bundle_id = $1`, [LONER.id]);
  expect(rows).toHaveLength(0);
});
