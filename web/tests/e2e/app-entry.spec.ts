import { expect, test } from "@playwright/test";
import { WS } from "../fixtures/plane/data.mjs";

/**
 * The app entry ("/app"), the door into the product. Runs with the default signed-in storage
 * state: "/" stays the marketing landing for everyone, but a signed-in visitor who reaches "/app"
 * is carried on to the main app page. (The signed-out bounce to /login is covered anonymously in
 * landing.spec.ts.)
 *
 * FAST-PATH RULE (seed-state assertion): /app redirects into a workspace only when the signed-in
 * email holds exactly ONE membership TOTAL — a second row, a pending invite included, must be
 * SEEN on the /workspaces index, never skipped past. The default member's one membership is the
 * confirmed ws-e2e seat (PLANE_SEED), so the entry lands directly on that workspace.
 */

test("the app entry fast-paths the sole-membership member straight to the workspace", async ({
  page,
}) => {
  await page.goto("/app");
  // Blocking SSR redirects are HTTP hops: the sole-membership member lands straight on
  // /workspaces/<ws> without ever committing /workspaces itself.
  await page.waitForURL(`**/workspaces/${WS}`);
  await expect(page.getByRole("banner")).toBeVisible();
});
