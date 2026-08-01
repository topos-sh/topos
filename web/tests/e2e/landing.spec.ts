import { expect, test } from "@playwright/test";

/**
 * The public landing page at "/". Runs ANONYMOUSLY (empty storage state): the landing is the one
 * signed-in-shell-free page, and `/install` is excluded from the login bounce, so these tests
 * prove the exclusion holds and the auth boundary bounces everything else.
 *
 * Marketing SENTENCES stay unasserted (they move independently of the app) — this suite asserts
 * the SPINE (every band is present, in order), the two behaviors the page owes a visitor (the
 * bot-safe founder address, and a scriptless render that still tells the whole story), and the
 * links whose destinations are product contracts.
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
    // A member page under the shell: the cookie middleware bounces a signed-out visitor to /login.
    await page.goto("/members");
    await page.waitForURL((u) => u.pathname === "/login");
    await page.goto("/app");
    await page.waitForURL((u) => u.pathname === "/login");
  });

  test("the whole spine renders, band by band", async ({ page }) => {
    await page.goto("/");

    // 2 — the proof row: the license claim links to the repo, and the works-with chips are text.
    const proof = page.getByRole("link", { name: /Open source/ });
    await expect(proof).toHaveAttribute("href", "https://github.com/topos-sh/topos");
    for (const app of ["Claude Code", "OpenClaw", "Hermes", "Codex", "Cursor"]) {
      await expect(page.getByText(app, { exact: true }).first()).toBeVisible();
    }
    // Verified unsupported — the page must claim no Claude-app delivery anywhere.
    await expect(page.getByText("Claude app")).toHaveCount(0);

    // 3 — the four team cards, each with its agent-app badge and its bundle chips.
    for (const team of ["Marketing", "Sales", "Support", "Engineering"]) {
      await expect(page.getByText(team, { exact: true }).first()).toBeVisible();
    }
    await expect(page.getByText("brand-voice", { exact: false }).first()).toBeVisible();
    await expect(page.getByText("client-history", { exact: false }).first()).toBeVisible();

    // 4 — the loop: the three numbered steps and the chat scene with its receipt.
    const loop = page.locator("#demo");
    await expect(loop).toContainText("Publish");
    await expect(loop).toContainText("Delivered automatically");
    await expect(loop).toContainText("Improved, then reviewed");
    await expect(loop).toContainText("customer-lookup");
    await expect(loop).toContainText("Published: delivered to Support, Sales, and Engineering");

    // 5 — the schematic review component: address bar, proposer, diff, and the real button copy.
    // Asserted as TEXT, not by role: the loop bands' static variant draws the approve control as an
    // inert block, so the copy is the only thing both variants share.
    await expect(page.getByText("topos.sh/acme/skills/brand-voice/proposals")).toBeVisible();
    await expect(page.getByText("proposed by Kim · Marketing")).toBeVisible();
    await expect(page.getByText("Approve", { exact: true })).toBeVisible();
    await expect(
      page.getByText("Use sentence case for headings. No exclamation points."),
    ).toBeVisible();

    // 6 — the pricing band: three tiers, the seat definition, and the one accent CTA. The tier
    // figures are asserted as text because the hierarchy between the three cards is carried by the
    // neutral ramp, which the DOM does not spell.
    const pricing = page.locator("#pricing");
    await expect(pricing).toContainText("Self-hosted");
    await expect(pricing).toContainText("$0");
    await expect(pricing).toContainText("Team");
    await expect(pricing).toContainText("$20");
    await expect(pricing).toContainText("Enterprise");
    await expect(pricing).toContainText("$40");
    await expect(pricing).toContainText("A seat is anyone who works with an AI agent");
    await expect(pricing.getByRole("link", { name: "Talk to us" })).toHaveAttribute(
      "href",
      "#contact",
    );

    // 7 — the FAQ, fully visible text (no accordion): six questions, six answers on the page.
    const faq = page.locator("#faq");
    await expect(faq.locator("dt")).toHaveCount(6);
    await expect(faq.locator("dd")).toHaveCount(6);
    await expect(faq.locator("dd").first()).toBeVisible();

    // 8 — the founder band, which is also the pricing band's "Talk to us" destination.
    await expect(page.locator("#contact")).toHaveCount(1);
    await expect(
      page.getByRole("heading", { name: "Setting this up for a team? Email me." }),
    ).toBeVisible();

    // 9 — the footer: the browser-path button leads its link row; no security link, no address.
    const footer = page.locator("footer");
    // Two matches are legitimate in single tenancy: the primary button (/login) plus the plain
    // "Sign in" link (/app). The button is the first — assert it specifically.
    await expect(
      footer.getByRole("link", { name: /Create a workspace|Sign in/ }).first(),
    ).toBeVisible();
    await expect(footer.getByRole("link", { name: /GitHub/ })).toBeVisible();
    await expect(footer.getByRole("link", { name: "Docs" })).toBeVisible();
    await expect(footer.getByRole("link", { name: /Security model/ })).toHaveCount(0);
    await expect(footer).toContainText("Apache-2.0");

    // The closing CTA band is gone — the footer carries the last call to action.
    await expect(
      page.getByRole("heading", { name: "Share your first skill in five minutes." }),
    ).toHaveCount(0);
  });

  test("the retired bands are gone", async ({ page }) => {
    await page.goto("/");
    // The git-comparison band and the verb-card band both left; the nav points at the FAQ now.
    await expect(page.locator("#vs")).toHaveCount(0);
    await expect(page.locator("#agent")).toHaveCount(0);
    await expect(page.getByRole("link", { name: "Why Topos" })).toHaveCount(0);
    await expect(page.getByRole("link", { name: "FAQ" })).toHaveAttribute("href", "#faq");
    await expect(page.getByRole("link", { name: "Pricing" })).toHaveAttribute("href", "#pricing");
  });

  test("the founder's address is never in the served bytes, and never a mailto", async ({
    request,
    page,
  }) => {
    // The document a harvester actually fetches: the address must not be assembled anywhere in it,
    // and no `mailto:` may exist for one to scrape.
    const html = await (await request.get("/")).text();
    expect(html).not.toContain("robert@topos.sh");
    expect(html).not.toContain("mailto:");

    // In a browser the same address IS readable — assembled after mount, from parts.
    await page.goto("/");
    await expect(page.getByText("robert@topos.sh").first()).toBeVisible();
    await expect(page.locator('a[href^="mailto:"]')).toHaveCount(0);
  });
});

/**
 * The scriptless page. The loop bands' fleet rail is a scripted scene, so a page with no
 * JavaScript must fall back to the static pair — the story still has to land, and nothing may
 * rest invisible waiting for a beat that will never fire.
 *
 * Written to hold for BOTH loop-band implementations, so flipping the rail switch never edits a
 * test: with the rail off the fan-out table is simply the layout, and with it on the scriptless
 * rule reveals the table and takes the rail out.
 */
test.describe("the landing page without JavaScript", () => {
  test.use({ javaScriptEnabled: false });

  test("the static loop bands render in place of the fleet rail", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByRole("heading", { level: 1 })).toBeVisible();

    const rail = page.locator(".landing-rail");
    const fanout = page.locator(".landing-fanout");
    await expect(rail).toBeHidden();
    await expect(fanout).toBeVisible();
    await expect(fanout).toContainText("customer-lookup added");

    // Everything that rests hidden for a beat is revealed instead of left blank.
    await expect(page.locator("#demo")).toContainText("Yes.");
    await expect(page.locator("#demo")).toContainText(
      "Published: delivered to Support, Sales, and Engineering",
    );

    // And the scriptless reader still gets the honest instruction in place of the address.
    await expect(page.getByText("email robert at this domain").first()).toBeVisible();
  });
});
