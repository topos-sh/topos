import { expect, test } from "@playwright/test";
import { adminQuery, ensureBundle, seedCustody, theWorkspace } from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * YOUR SKILLS — the two views over a person's assignments. MINE groups everything effectively
 * theirs by what puts it there (the workspace baseline arriving with no per-person row at all,
 * each carried channel, anything aimed at them, their own picks) and carries the per-skill off
 * switch — a decline, off for me whatever assigns it, with the skill staying in the team
 * library. LIBRARY is the catalog with one act on it: add this to mine. The suite's default
 * identity is the claimed owner.
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
  // The guide rides the baseline; the loner sits catalog-only (the library's add subject).
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

test("the baseline group delivers with no per-person row; turning a skill off keeps it visible", async ({
  page,
}) => {
  await gotoSettled(page, "/profile");
  await expect(page.getByRole("heading", { name: "Your skills" })).toBeVisible();
  await expect(page.getByTestId("profile-tab-mine")).toHaveAttribute("aria-current", "page");

  // The baseline is the default channel assigned to everyone — its skills arrive with no row
  // of this person's own, and the attribution says so.
  const baseline = page.getByTestId("profile-group-baseline");
  await expect(baseline.getByText("baseline", { exact: true })).toBeVisible();
  const row = baseline.getByTestId(`profile-row-${SKILL.name}`);
  await expect(row).toBeVisible();
  await expect(row.getByText("assigned to everyone")).toBeVisible();
  // Nobody un-adds the baseline; its skills are turned off one at a time.
  await expect(baseline.getByRole("button", { name: "Un-add" })).toHaveCount(0);

  // Turning it OFF records the one negative row and stops delivery — and the row STAYS on the
  // page, dimmed, saying where the skill still lives.
  await row.getByRole("button", { name: "Turn off" }).click();
  await expect(row.getByText("off — still in the team library")).toBeVisible();
  await expect(row.getByRole("button", { name: "Turn on" })).toBeVisible();
  expect(
    await adminQuery(`SELECT 1 FROM web.decline WHERE bundle_id = $1`, [SKILL.id]),
  ).toHaveLength(1);

  // Turning it back on clears the row — delivery resumes from the baseline that never moved.
  await row.getByRole("button", { name: "Turn on" }).click();
  await expect(row.getByText("assigned to everyone")).toBeVisible();
  expect(
    await adminQuery(`SELECT 1 FROM web.decline WHERE bundle_id = $1`, [SKILL.id]),
  ).toHaveLength(0);
});

test("the library adds a skill as the person's OWN row, and un-adding takes it back", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, "/profile?tab=library");
  await expect(page.getByTestId("profile-tab-library")).toHaveAttribute("aria-current", "page");

  // A skill nothing assigns offers the one act; one the feed already carries shows its state.
  const shelf = page.getByTestId(`profile-library-${LONER.name}`);
  await expect(shelf.getByRole("button", { name: "Add to mine" })).toBeVisible();
  await expect(
    page.getByTestId(`profile-library-${SKILL.name}`).getByText("in your skills"),
  ).toBeVisible();

  await shelf.getByRole("button", { name: "Add to mine" }).click();
  await expect(shelf.getByText("in your skills")).toBeVisible();
  // The row records the person as BOTH audience and author — that is what makes it theirs.
  const assigned = await adminQuery<{ user_id: string; created_by: string }>(
    `SELECT user_id, created_by FROM web.assignment WHERE bundle_id = $1 AND workspace_id = $2`,
    [LONER.id, ws.id],
  );
  expect(assigned).toHaveLength(1);
  expect(assigned[0]?.user_id).toBe(assigned[0]?.created_by);

  // …and it lands in Mine under the person's own picks, attributed to them.
  await page.getByTestId("profile-tab-mine").click();
  const picked = page.getByTestId("profile-group-picked");
  const row = picked.getByTestId(`profile-row-${LONER.name}`);
  await expect(row.getByText("picked by you")).toBeVisible();

  // Un-adding deletes it outright; nothing broader carries the loner, so delivery just ends —
  // and un-adding is not declining, so no negative row is left behind.
  await row.getByRole("button", { name: "Un-add" }).click();
  await expect(page.getByTestId(`profile-row-${LONER.name}`)).toHaveCount(0);
  expect(
    await adminQuery(`SELECT 1 FROM web.assignment WHERE bundle_id = $1`, [LONER.id]),
  ).toHaveLength(0);
  expect(
    await adminQuery(`SELECT 1 FROM web.decline WHERE bundle_id = $1`, [LONER.id]),
  ).toHaveLength(0);
});
