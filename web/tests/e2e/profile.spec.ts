import { expect, test } from "@playwright/test";
import { adminQuery, ensureBundle, seedCustody, theWorkspace } from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * YOUR SKILLS — the web face of a person's FEED: the workspace baseline arriving with no
 * per-person row at all, the per-skill off switch (a decline — off for me, whatever assigns
 * it, and the skill stays in the team library), un-adding something you added yourself, and
 * the direct add from the catalog. The suite's default identity is the claimed owner.
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
  // A clean slate for the suite's owner (idempotent on a reused database) — the baseline's
  // own everyone-assignment is left alone; it is what makes the workspace deliver at all.
  await adminQuery(`DELETE FROM web.decline WHERE bundle_id = ANY($1::text[])`, [
    [SKILL.id, LONER.id],
  ]);
  await adminQuery(
    `DELETE FROM web.assignment WHERE bundle_id = ANY($1::text[]) AND user_id IS NOT NULL`,
    [[SKILL.id, LONER.id]],
  );
});

test("the baseline delivers with no per-person row; turning a skill off holds it back", async ({
  page,
}) => {
  await gotoSettled(page, "/profile");
  await expect(page.getByRole("heading", { name: "Your skills" })).toBeVisible();

  // The baseline channel reads carried — it is assigned to everyone — and its skill arrives.
  const everyone = page.getByTestId("profile-channel-everyone");
  await expect(everyone.getByText("baseline", { exact: true })).toBeVisible();
  await expect(everyone.getByText("in your skills", { exact: true })).toBeVisible();
  await expect(everyone.getByText("assigned to everyone")).toBeVisible();
  const delivered = page.getByTestId(`profile-delivered-${SKILL.name}`);
  await expect(delivered).toBeVisible();
  await expect(delivered.getByText("via everyone")).toBeVisible();

  // Turning it OFF records the one negative row and stops delivery — the skill stays in the
  // library, visible and dimmed, with the switch back.
  await delivered.getByRole("button", { name: "Turn off" }).click();
  await expect(page.getByTestId(`profile-delivered-${SKILL.name}`)).toHaveCount(0);
  const declined = page.getByTestId(`profile-declined-${SKILL.name}`);
  await expect(declined).toBeVisible();
  await expect(declined.getByText("off — still in the team library")).toBeVisible();
  const rows = await adminQuery(`SELECT 1 FROM web.decline WHERE bundle_id = $1`, [SKILL.id]);
  expect(rows).toHaveLength(1);

  // Turning it back on clears the row — delivery resumes from the baseline that never moved.
  await declined.getByRole("button", { name: "Turn on" }).click();
  await expect(page.getByTestId(`profile-delivered-${SKILL.name}`)).toBeVisible();
  expect(
    await adminQuery(`SELECT 1 FROM web.decline WHERE bundle_id = $1`, [SKILL.id]),
  ).toHaveLength(0);
});

test("the baseline channel is everyone's — no one person drops it", async ({ page }) => {
  await gotoSettled(page, "/profile");
  const everyone = page.getByTestId("profile-channel-everyone");
  await expect(everyone.getByText("assigned to everyone")).toBeVisible();
  await expect(everyone.getByRole("button", { name: /Drop everyone/ })).toHaveCount(0);
});

test("a direct add is the person's own row, and un-adding takes it back", async ({ page }) => {
  const ws = await theWorkspace();
  await gotoSettled(page, "/profile");
  await page
    .getByTestId(`profile-addable-${LONER.name}`)
    .getByRole("button", { name: "Add" })
    .click();
  const delivered = page.getByTestId(`profile-delivered-${LONER.name}`);
  await expect(delivered).toBeVisible();
  await expect(delivered.getByText("added by you")).toBeVisible();
  // The row records the person as BOTH audience and author — that is what makes it theirs.
  const assigned = await adminQuery<{ user_id: string; created_by: string }>(
    `SELECT user_id, created_by FROM web.assignment WHERE bundle_id = $1 AND workspace_id = $2`,
    [LONER.id, ws.id],
  );
  expect(assigned).toHaveLength(1);
  expect(assigned[0]?.user_id).toBe(assigned[0]?.created_by);

  // Un-adding deletes it outright; nothing broader carries the loner, so delivery just ends.
  await delivered.getByRole("button", { name: "Un-add" }).click();
  await expect(page.getByTestId(`profile-delivered-${LONER.name}`)).toHaveCount(0);
  expect(
    await adminQuery(`SELECT 1 FROM web.assignment WHERE bundle_id = $1`, [LONER.id]),
  ).toHaveLength(0);
  // Un-adding is not declining: no negative row is left behind.
  expect(
    await adminQuery(`SELECT 1 FROM web.decline WHERE bundle_id = $1`, [LONER.id]),
  ).toHaveLength(0);
});
