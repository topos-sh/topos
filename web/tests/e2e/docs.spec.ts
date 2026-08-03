import { expect, type Page, test } from "@playwright/test";
import { BASE_URL } from "./env";

/**
 * The documentation, served by the app itself. Runs ANONYMOUSLY (empty storage state): docs are
 * what a stranger reads before they have an account, so the login bounce must not touch them.
 *
 * Deliberately CONTENT-AGNOSTIC — it asserts structure, mechanism, and reachability, never a
 * sentence of documentation copy. Every target is derived from the sidebar the app itself
 * renders, so the suite holds whether it runs against the repo's docs/ or a fixture content root
 * (TOPOS_DOCS_DIR), and does not go red when someone rewrites a page.
 */

test.use({
  storageState: { cookies: [], origins: [] },
  contextOptions: { reducedMotion: "reduce" },
});

/** Every page the sidebar links to, in nav order — the docs set as the app advertises it. */
async function sidebarPaths(page: Page): Promise<string[]> {
  const links = page.getByRole("navigation", { name: "Documentation" }).locator("a");
  const hrefs = await links.evaluateAll((nodes) =>
    nodes.map((node) => (node as HTMLAnchorElement).getAttribute("href") ?? ""),
  );
  const paths = hrefs.filter((href) => href.startsWith("/docs"));
  expect(paths.length).toBeGreaterThan(1);
  return paths;
}

test.describe("the documentation", () => {
  test("renders anonymously at /docs with a title, sidebar, contents and next link", async ({
    page,
  }) => {
    await page.goto("/docs");
    await expect(page).toHaveURL(/\/docs$/);

    // The h1 and the document title both come from the page's frontmatter. The home page's own
    // title IS the product name, so it reads "Topos documentation" rather than "Topos · Topos";
    // every other page wears the suffix (asserted on the deep page below).
    await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
    await expect(page).toHaveTitle("Topos documentation");

    // The sidebar is built from docs/nav.json and marks the page being read.
    const sidebar = page.getByRole("navigation", { name: "Documentation" });
    await expect(sidebar).toBeVisible();
    await expect(sidebar.locator('a[aria-current="page"]')).toHaveAttribute("href", "/docs");

    // The on-page contents are the article's own h2/h3 anchors, and they resolve.
    const contents = page.getByRole("navigation", { name: "On this page" });
    const anchor = await contents.locator("a").first().getAttribute("href");
    expect(anchor).toMatch(/^#/);
    await expect(page.locator(`article ${anchor}`)).toBeVisible();

    // The first page has no previous, so prev/next carries the next page only.
    const neighbours = page.getByRole("navigation", { name: "Previous and next page" });
    await expect(neighbours.locator("a")).toHaveCount(1);
  });

  test("the sidebar navigates, and every docs page renders with its own chrome", async ({
    page,
  }) => {
    await page.goto("/docs");
    const paths = await sidebarPaths(page);

    let sawWarning = false;
    for (const path of paths) {
      await page.goto(path);
      await expect(page.getByRole("heading", { level: 1 }), path).toBeVisible();
      // Every page but the home one wears the product suffix in its document title.
      if (path !== "/docs") {
        await expect(page, path).toHaveTitle(/ · Topos$/);
      }
      await expect(
        page.getByRole("navigation", { name: "Documentation" }).locator('a[aria-current="page"]'),
        path,
      ).toHaveAttribute("href", path);
      // Nothing may reach the reader as an unrendered component tag.
      const article = await page.locator("article").innerText();
      expect(article, path).not.toMatch(/<\/?(Note|Warning|Tip|Steps|Step|Tabs|Tab|Card|CardGrid)/);
      if ((await page.locator("article .docs-aside--warning").count()) > 0) {
        sawWarning = true;
      }
    }
    expect(sawWarning, "the docs render at least one <Warning> hazard aside").toBe(true);
  });

  test("a <Warning> reads as a hazard — distinct from a Note, and labelled", async ({ page }) => {
    await page.goto("/docs");
    const paths = await sidebarPaths(page);
    for (const path of paths) {
      await page.goto(path);
      const warning = page.locator("article .docs-aside--warning").first();
      const note = page.locator("article .docs-aside--note").first();
      if ((await warning.count()) === 0 || (await note.count()) === 0) {
        continue;
      }
      await expect(warning).toBeVisible();
      await expect(warning).toContainText("Warning");
      // Visually distinct, not merely labelled: the hazard rule is its own colour.
      expect(await warning.evaluate((el) => getComputedStyle(el).borderLeftColor)).not.toBe(
        await note.evaluate((el) => getComputedStyle(el).borderLeftColor),
      );
      return;
    }
    test.skip(true, "no page in this content set carries both a Warning and a Note");
  });

  test("code is highlighted at BUILD time — the page ships markup, not a highlighter", async ({
    page,
  }) => {
    await page.goto("/docs");
    for (const path of await sidebarPaths(page)) {
      await page.goto(path);
      if ((await page.locator("article pre.shiki").count()) > 0) {
        await expect(page.locator("article pre.shiki").first()).toBeVisible();
        // Shiki's colours are inline on the spans it emitted; nothing is computed in the browser.
        await expect(page.locator("article pre.shiki span[style]").first()).toBeVisible();
        return;
      }
    }
    throw new Error("no highlighted code block anywhere in the docs");
  });

  test("tabs switch without JavaScript — one radio group, panels shown by :checked", async ({
    page,
  }) => {
    await page.goto("/docs");
    for (const path of await sidebarPaths(page)) {
      await page.goto(path);
      const panels = page.locator("article .docs-tabs").first().locator(".docs-tab");
      if ((await panels.count()) < 2) {
        continue;
      }
      await expect(panels.nth(0)).toBeVisible();
      await expect(panels.nth(1)).toBeHidden();
      await page.locator("article .docs-tabs").first().locator(".docs-tabs__label").nth(1).click();
      await expect(panels.nth(0)).toBeHidden();
      await expect(panels.nth(1)).toBeVisible();
      return;
    }
    test.skip(true, "no page in this content set uses <Tabs>");
  });

  test("every page has a markdown twin at its own path plus .md", async ({ page, request }) => {
    await page.goto("/docs");
    for (const path of await sidebarPaths(page)) {
      const response = await request.get(`${path}.md`, { maxRedirects: 0 });
      expect(response.status(), path).toBe(200);
      expect(response.headers()["content-type"], path).toContain("text/markdown");
      const body = await response.text();
      expect(body.startsWith("# "), path).toBe(true);
      expect(body, path).not.toContain("<html");
      // The tags are reduced to markdown: an agent never has to parse JSX.
      expect(body, path).not.toMatch(/<\/?(Note|Warning|Tip|Steps|Step|Tabs|Tab|Card|CardGrid)/);
    }
  });

  test("every page opens on the two markdown actions, each naming that page's own twin", async ({
    page,
  }) => {
    await page.goto("/docs");
    for (const path of await sidebarPaths(page)) {
      await page.goto(path);
      const twin = path === "/docs" ? "/docs.md" : `${path}.md`;
      // The exits sit in the header — above the article, not buried under it.
      const header = page.locator("main header");
      await expect(header.getByRole("button"), path).toHaveText(/copy page as markdown/i);
      await expect(
        header.getByRole("link", { name: /view page as markdown/i }),
        path,
      ).toHaveAttribute("href", twin);
      // The footer's old duplicate of the same link is gone — one address, stated once.
      await expect(
        page.locator("footer").getByRole("link", { name: /markdown/i }),
        path,
      ).toHaveCount(0);
    }
  });

  test("copy page puts the twin's own bytes on the clipboard", async ({
    page,
    request,
    context,
  }) => {
    await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.goto("/docs/install");

    // Located by POSITION, not by name: the label is the button's own accessible name, and it
    // changes to "copied" on success — a name-based locator would stop matching the thing it
    // just clicked.
    const copyPage = page.locator("main header button");
    await expect(copyPage).toHaveText(/copy page as markdown/i);
    await copyPage.click();
    // The button reports the write rather than staying silent about it.
    await expect(copyPage).toHaveText(/copied/i);

    // What landed is EXACTLY what the twin serves — the page copies the markdown, it does not
    // re-derive a second rendering of its own prose.
    const clipboard = await page.evaluate(() => navigator.clipboard.readText());
    const twin = await (await request.get("/docs/install.md", { maxRedirects: 0 })).text();
    expect(clipboard).toBe(twin);
    expect(clipboard.startsWith("# ")).toBe(true);
  });

  test("/docs/llms.txt indexes every page by its markdown twin, and every link resolves", async ({
    page,
    request,
  }) => {
    await page.goto("/docs");
    const paths = await sidebarPaths(page);
    const response = await request.get("/docs/llms.txt", { maxRedirects: 0 });
    expect(response.status()).toBe(200);
    expect(response.headers()["content-type"]).toContain("text/plain");
    const body = await response.text();
    for (const path of paths) {
      // The MARKDOWN twin, not the page: a non-browser fetch of a page path gets the protocol
      // card, so an index that pointed there would be useless to the machines it is written for.
      const twin = path === "/docs" ? "/docs.md" : `${path}.md`;
      expect(body, path).toContain(`${BASE_URL}${twin})`);
    }
    // Follow every emitted link — an index of dead or intercepted URLs is worse than none.
    const linked = [...body.matchAll(/\]\((https?:\/\/[^)]+)\)/g)].map((m) => m[1] ?? "");
    expect(linked.length).toBeGreaterThanOrEqual(paths.length);
    for (const url of linked) {
      const response = await request.get(url, { maxRedirects: 0 });
      expect(response.status(), url).toBe(200);
      expect(response.headers()["content-type"], url).toContain("text/markdown");
    }
  });

  test("no docs page links to a dead internal path", async ({ page, request }) => {
    await page.goto("/docs");
    const seen = new Set<string>();
    for (const path of await sidebarPaths(page)) {
      await page.goto(path);
      const hrefs = await page
        .locator("article a[href^='/']")
        .evaluateAll((nodes) =>
          nodes.map((node) => (node as HTMLAnchorElement).getAttribute("href") ?? ""),
        );
      for (const href of hrefs) {
        const target = href.split("#")[0] ?? "";
        if (target === "" || seen.has(target)) {
          continue;
        }
        seen.add(target);
        expect((await request.get(target)).status(), `${path} → ${target}`).not.toBe(404);
      }
    }
  });

  test("the home page has ONE address — /docs/index redirects rather than duplicating it", async ({
    page,
  }) => {
    await page.goto("/docs/index");
    await expect(page).toHaveURL(/\/docs$/);
    await expect(page).toHaveTitle("Topos documentation");
  });

  test("a docs path that names no page is the house 404, not a blank page", async ({ page }) => {
    const response = await page.goto("/docs/no-such-page");
    expect(response?.status()).toBe(404);
    await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
  });

  test("a missing markdown twin 404s without inventing a page", async ({ request }) => {
    expect((await request.get("/docs/no-such-page.md")).status()).toBe(404);
  });

  test("the index twin has ONE address too — /docs/index.md redirects to /docs.md", async ({
    request,
  }) => {
    const response = await request.get("/docs/index.md", { maxRedirects: 0 });
    expect(response.status()).toBe(302);
    expect(response.headers().location).toBe("/docs.md");
  });
});
