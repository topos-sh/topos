import { expect, test } from "@playwright/test";

/**
 * The public landing page at "/". Runs ANONYMOUSLY (empty storage state): the landing is the one
 * signed-in-shell-free page, and `/install` is excluded from the login bounce, so these tests
 * prove the exclusion holds and the auth boundary bounces everything else. Marketing copy is
 * deliberately UNASSERTED (it moves independently of the app) — structure and behavior only.
 */

test.use({
  storageState: { cookies: [], origins: [] },
  contextOptions: { reducedMotion: "reduce" },
});

test.describe("the public landing page", () => {
  test("/install serves the installer bytes to a signed-out curl, never login HTML", async ({
    request,
  }) => {
    const response = await request.get("/install", { maxRedirects: 0 });
    expect(response.status()).toBe(200);
    expect(response.headers()["content-type"]).toContain("text/plain");
    const body = await response.text();
    expect(body.startsWith("#!/bin/sh")).toBe(true);
    // The in-repo checksummed installer, not a rendered page: it refuses to skip verification.
    expect(body).toContain("SHA256SUMS");
    expect(body).not.toContain("<html");
  });

  test("serves anonymously at / with a hero and no login bounce", async ({ page }) => {
    await page.goto("/");
    await expect(page).toHaveURL(/\/$/);
    await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
    // The nav links into the app entry, which routes signed-in visitors on and bounces the
    // signed-out to /login (asserted by href, never by marketing label).
    await expect(page.locator('a[href="/app"]').first()).toBeVisible();
  });

  test("signed-out visits to the app bounce to login", async ({ page }) => {
    await page.goto("/workspaces");
    await page.waitForURL((u) => u.pathname === "/login");
    await page.goto("/app");
    await page.waitForURL((u) => u.pathname === "/login");
  });
});
