import { expect, test } from "@playwright/test";

/**
 * Client-side navigation into card-carrying routes — the regression the protocol card's old
 * route-middleware placement caused. The router's own `.data` fetches carry the same bare
 * wildcard Accept header curl sends, so a route-level card middleware answered them with the
 * card, the client failed to decode it as a navigation payload, and the miss rendered the root
 * boundary's bogus 500. The card lives in the server entry (document requests only); the
 * canonical repro — land on `/`, client-navigate to `/login`, press Back — must re-render the
 * landing page. Runs ANONYMOUSLY against the PRODUCTION build (the bug never reproduced under
 * the dev server: only the production hydration path surfaced it). The landing hero is asserted
 * structurally (an h1), never by its marketing copy.
 */

test.use({
  storageState: { cookies: [], origins: [] },
  contextOptions: { reducedMotion: "reduce" },
});

test.describe("history navigation through the landing page", () => {
  test("back from /login re-renders the landing page, never the error boundary", async ({
    page,
  }) => {
    const pageErrors: string[] = [];
    page.on("pageerror", (error) => {
      pageErrors.push(String(error));
    });

    await page.goto("/");
    const hero = page.getByRole("heading", { level: 1 });
    await expect(hero).toBeVisible();

    // A CLIENT-SIDE navigation (the nav links to /login) — a goto() would document-load and
    // sidestep the `.data` lane this spec exists to pin.
    await page.locator('a[href="/login"]').first().click();
    await expect(page.getByRole("heading", { name: "Sign in to Topos" })).toBeVisible();

    // The user's repro: the browser Back button. The pop re-runs the landing loader over the
    // `.data` lane; a carded response here is exactly the regression.
    await page.goBack();
    await expect(hero).toBeVisible();
    await expect(page.getByText("Something went wrong")).not.toBeVisible();

    // And the round trip again — forward to login, back once more — stays healthy.
    await page.goForward();
    await expect(page.getByRole("heading", { name: "Sign in to Topos" })).toBeVisible();
    await page.goBack();
    await expect(hero).toBeVisible();

    // No hydration faults anywhere in the run (the production build's streamed-boundary races
    // surfaced as React #418 before the server entry rendered complete documents).
    const hydrationFaults = pageErrors.filter((e) => /hydrat|#418|\b418\b/i.test(e));
    expect(hydrationFaults).toEqual([]);
  });
});
