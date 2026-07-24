import { expect, test } from "@playwright/test";
import { ensureBundle, theWorkspace } from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * The global breadcrumbs — now rendered UNDER the page title on every signed-in page (they moved
 * out of the top header bar and into each page's title block, inside the `main` region). The trail
 * is the workspace itself (from the layout chrome) followed by the deepest route the central
 * registry knows. The suite's default storage state signs in as the seeded member (the workspace
 * owner), so every page below renders WITH the chrome — breadcrumbs are member-scoped by
 * construction (the anonymous face renders no chrome, and the component itself finds none).
 *
 * The bar is one `<nav aria-label="Breadcrumb">`; the current page is the last, unlinked segment
 * (`aria-current="page"`); earlier segments that address a resource are links. The role-based
 * locators here are placement-agnostic — they survived the move from the header bar to the title.
 */

/** The one breadcrumb bar on the page. */
function breadcrumb(page: import("@playwright/test").Page) {
  return page.getByRole("navigation", { name: "Breadcrumb" });
}

test("the dashboard shows the workspace as the current root crumb, inside the main region", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, "/");

  const bc = breadcrumb(page);
  await expect(bc).toBeVisible();
  // The sole crumb is the workspace itself, and it IS the current page (unlinked).
  await expect(bc.locator('[aria-current="page"]')).toHaveText(ws.displayName);

  // The trail lives under the page title, inside the page's `main` region (not the header bar).
  await expect(
    page.getByRole("main").getByRole("navigation", { name: "Breadcrumb" }),
  ).toBeVisible();
});

test("a channel sub-page shows workspace → Channels → #everyone → History, Channels linking home", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, "/channels/everyone/history");

  const bc = breadcrumb(page);
  // The full trail: the workspace root, the Channels index, the channel face, then the current tab.
  await expect(bc.getByText(ws.displayName)).toBeVisible();
  await expect(bc.getByRole("link", { name: "Channels" })).toBeVisible();
  await expect(bc.getByRole("link", { name: "#everyone" })).toBeVisible();
  await expect(bc.locator('[aria-current="page"]')).toHaveText("History");

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

test("the members page shows workspace → Members, the workspace linking home", async ({ page }) => {
  const ws = await theWorkspace();
  await gotoSettled(page, "/members");

  const bc = breadcrumb(page);
  await expect(bc).toBeVisible();
  // The workspace root is a link (not the last crumb); Members is the current page.
  await expect(bc.getByRole("link", { name: ws.displayName })).toBeVisible();
  await expect(bc.locator('[aria-current="page"]')).toHaveText("Members");
});

test("the your-sessions page shows workspace → Your sessions (the account tail off a workspace root)", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, "/account/sessions");

  const bc = breadcrumb(page);
  await expect(bc).toBeVisible();
  // your-sessions is top-level (an account page), yet the single-tenant chrome still scopes the sole
  // workspace, so the root crumb precedes the account tail.
  await expect(bc.getByRole("link", { name: ws.displayName })).toBeVisible();
  await expect(bc.locator('[aria-current="page"]')).toHaveText("Your sessions");
});
