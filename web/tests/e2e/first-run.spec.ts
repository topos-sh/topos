import { expect, test } from "@playwright/test";
import { E2E_SETUP_CODE } from "./env";

/**
 * The first-boot CLAIM ceremony's public probes, on an install whose workspace auth.setup.ts
 * has ALREADY claimed. The claim page is uniform-miss by design: no code, a wrong code, and the
 * SPENT setup code all render the same house 404 as any missing route — a spent link discloses
 * nothing about what it once was.
 *
 * HONEST GAP: the full fresh-claim walk (the printed link → the first owner) runs once per
 * database, in auth.setup.ts itself — this suite's stack is claimed before any spec runs, so
 * re-proving it here would need a second database. The probes below are the halves a claimed
 * install can still prove.
 */

test.use({
  storageState: { cookies: [], origins: [] },
  contextOptions: { reducedMotion: "reduce" },
});

test("/claim without a code is the uniform miss", async ({ page }) => {
  await page.goto("/claim");
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
});

test("/claim with a wrong code is the same uniform miss", async ({ page }) => {
  await page.goto("/claim?code=not-the-setup-code-000000");
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
  await expect(page.getByText("Claim", { exact: false })).toHaveCount(0);
});

test("the SPENT setup code answers the uniform miss — dead after one use", async ({ page }) => {
  // auth.setup.ts consumed this exact code claiming the workspace; the one-time link is dead.
  await page.goto(`/claim?code=${E2E_SETUP_CODE}`);
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
});

test("a claimed install's landing shows no first-run claim block", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
  // The unclaimed-install band (the printed-link hint) is absent once an owner exists.
  await expect(page.getByText("This install is waiting for its owner.")).toHaveCount(0);
});
