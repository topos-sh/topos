import { expect, test } from "@playwright/test";
import { ensureBundle, theWorkspace } from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * The global breadcrumbs in the signed-in header bar. The trail is the workspace itself (from the
 * chrome) followed by the deepest route the central registry knows. The suite's default storage
 * state signs in as the seeded member (the workspace owner), so every page below renders WITH the
 * chrome — breadcrumbs are member-scoped by construction (the anonymous face renders no chrome).
 *
 * The bar is one `<nav aria-label="Breadcrumb">`; the current page is the last, unlinked segment
 * (`aria-current="page"`); earlier segments that address a resource are links.
 */

/** The one breadcrumb bar on the page. */
function breadcrumb(page: import("@playwright/test").Page) {
  return page.getByRole("navigation", { name: "Breadcrumb" });
}

test("the dashboard shows the workspace as the current root crumb", async ({ page }) => {
  const ws = await theWorkspace();
  await gotoSettled(page, "/");

  const bc = breadcrumb(page);
  await expect(bc).toBeVisible();
  // The sole crumb is the workspace itself, and it IS the current page (unlinked).
  await expect(bc.locator('[aria-current="page"]')).toHaveText(ws.displayName);
});

test("a channel sub-page shows workspace → Channels → #everyone → Members, Channels linking home", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, "/channels/everyone/members");

  const bc = breadcrumb(page);
  // The full trail: the workspace root, the Channels index, the channel face, then the current tab.
  await expect(bc.getByText(ws.displayName)).toBeVisible();
  await expect(bc.getByRole("link", { name: "Channels" })).toBeVisible();
  await expect(bc.getByRole("link", { name: "#everyone" })).toBeVisible();
  await expect(bc.locator('[aria-current="page"]')).toHaveText("Members");

  // The Channels crumb is a live link back to the index.
  await bc.getByRole("link", { name: "Channels" }).click();
  await page.waitForURL("**/channels");
  await expect(page.getByRole("heading", { level: 1, name: "Channels" })).toBeVisible();
});

test("a skill page shows Skills → the skill name (the name unlinked as current)", async ({
  page,
}) => {
  await ensureBundle({
    id: "s_e2e_breadcrumbs",
    name: "breadcrumb-skill",
    displayName: "Breadcrumb Skill",
  });
  await gotoSettled(page, "/skills/breadcrumb-skill");

  const bc = breadcrumb(page);
  // "Skills" is an unlinked segment (there is no skills index page); the skill's DISPLAY name is
  // the current crumb.
  await expect(bc.getByText("Skills")).toBeVisible();
  await expect(bc.locator('[aria-current="page"]')).toHaveText("Breadcrumb Skill");
});
