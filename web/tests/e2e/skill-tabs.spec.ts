import { expect, test } from "@playwright/test";
import { adminQuery, ensureBundle, ensureSeatedUser } from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The lifecycle surfaces' DISCOVERABILITY: the skill page's owner-only Settings tab and the
 * workspace Settings section's Archive tab. The tab is a link, never the gate — the routes
 * re-guard — so the member case asserts both halves: no tab rendered, and the direct URL still
 * answers the uniform 404. The suite's default identity is the claimed OWNER (storage state);
 * the member drives a fresh context.
 */

const SKILL = { id: "s_e2e_tabs", name: "tabs-notes" };
const MEMBER_EMAIL = "tabs-member@e2e.test";

test.beforeAll(async () => {
  await adminQuery(`delete from web.bundle where id = $1`, [SKILL.id]);
  await ensureBundle(SKILL);
  await ensureSeatedUser(MEMBER_EMAIL, "member");
});

test("the owner sees the Settings tab on the skill page and the Archive tab under settings", async ({
  page,
}) => {
  await gotoSettled(page, `/skills/${SKILL.name}`);
  const tabs = page.getByRole("navigation", { name: "Skill sections" });
  await expect(tabs.getByRole("link", { name: "Settings" })).toBeVisible();

  // The tab is a real route link: clicking lands on the settings page, tab row intact + pressed.
  await tabs.getByRole("link", { name: "Settings" }).click();
  await page.waitForURL(`**/skills/${SKILL.name}/settings`);
  await expect(
    page
      .getByRole("navigation", { name: "Skill sections" })
      .getByRole("link", { name: "Settings" }),
  ).toHaveAttribute("aria-current", "page");

  // The workspace Settings section: General · Devices · Archive, and Archive is a real page.
  await gotoSettled(page, "/settings");
  const settingsTabs = page.getByRole("navigation", { name: "Settings sections" });
  await expect(settingsTabs.getByRole("link", { name: "Archive" })).toBeVisible();
  await settingsTabs.getByRole("link", { name: "Archive" }).click();
  await page.waitForURL("**/settings/archive");
  await expect(page.getByRole("heading", { name: "Archive", exact: true })).toBeVisible();
});

test("a member gets no Settings tab, and the settings route stays the uniform 404", async ({
  browser,
}) => {
  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const p = await context.newPage();
  try {
    await signIn(p, MEMBER_EMAIL);
    await gotoSettled(p, `/skills/${SKILL.name}`);
    const tabs = p.getByRole("navigation", { name: "Skill sections" });
    await expect(tabs.getByRole("link", { name: "Current" })).toBeVisible();
    await expect(tabs.getByRole("link", { name: "Settings" })).toHaveCount(0);

    // The tab was discoverability, not the gate: the direct URL answers the house 404.
    await p.goto(`/skills/${SKILL.name}/settings`);
    await expect(p.getByRole("heading", { name: "Not found" })).toBeVisible();
  } finally {
    await context.close();
  }
});
