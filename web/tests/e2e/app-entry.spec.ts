import { expect, test } from "@playwright/test";
import { theWorkspace } from "./seed";

/**
 * The app entry ("/app"), the door into the product. Runs with the default signed-in storage
 * state — the workspace's claimed OWNER: "/" stays the marketing landing for everyone, but a
 * signed-in visitor who reaches "/app" is carried straight on. (The signed-out bounce to /login
 * is covered anonymously in landing.spec.ts.)
 *
 * Single-tenant: a seat (there is at most one) goes straight to its dashboard; the seatless
 * variant lands on /workspaces' honest miss (membership.spec.ts).
 */

test("the app entry carries a seated member straight to the workspace", async ({ page }) => {
  const ws = await theWorkspace();
  await page.goto("/app");
  // Blocking SSR redirects are HTTP hops: the seated member lands on /workspaces/<ws> without
  // ever committing /workspaces itself.
  await page.waitForURL(`**/workspaces/${ws.id}`);
  await expect(page.getByRole("banner")).toBeVisible();
});
