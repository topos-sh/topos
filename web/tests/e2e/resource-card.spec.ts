import { expect, test } from "@playwright/test";
import { BASE_URL, WORKSPACE_ADDRESS } from "./env";
import { adminQuery, theWorkspace } from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The resource ADDRESS (`<origin>/<workspace>[...]`) — its content-negotiated faces and the
 * no-existence-oracle promise. WORKSPACE_ADDRESS ("team") is the boot-minted workspace whose
 * claimed OWNER is the suite's default identity; a made-up address stands in for "nonexistent".
 *
 * The invariant under test: a non-browser fetcher gets ONE constant protocol card on every path
 * — real, nonexistent, or deep garbage, the origin root included — byte-for-byte identical, so
 * nothing about the response reveals whether the path names anything. A browser gets the
 * constant teaser when anonymous, a redirect into the workspace when it holds the seat, and the
 * uniform house 404 otherwise.
 */

const NONEXISTENT = "no-such-workspace-zzz";
const CARD = "topos-protocol-card";
const TEASER_MARKER = "A Topos resource address";
// A DISTINCT display name (set below, restored after): it must leak into NONE of the anonymous
// faces — the address bar is the visitor's own knowledge; the response never confirms it.
const WS_DISPLAY_NAME = "E2E Leak Canary Workspace";

let originalDisplayName: string;

test.beforeAll(async () => {
  originalDisplayName = (await theWorkspace()).displayName;
  await adminQuery(`update web.workspace set display_name = $1`, [WS_DISPLAY_NAME]);
});

test.afterAll(async () => {
  await adminQuery(`update web.workspace set display_name = $1`, [originalDisplayName]);
});

test.describe("anonymous — the constant protocol card + teaser (no existence oracle)", () => {
  test.use({ storageState: { cookies: [], origins: [] } });

  test("JSON: a real and a nonexistent address return the SAME 200 card", async ({ request }) => {
    const real = await request.get(`/${WORKSPACE_ADDRESS}`, {
      headers: { accept: "application/json" },
      maxRedirects: 0,
    });
    const missing = await request.get(`/${NONEXISTENT}`, {
      headers: { accept: "application/json" },
      maxRedirects: 0,
    });
    expect(real.status()).toBe(200);
    expect(missing.status()).toBe(200);
    expect(real.headers()["content-type"]).toContain("application/json");

    const bodyReal = await real.text();
    const bodyMissing = await missing.text();
    // Byte-identical: the ONLY input to the card is the deployment's own base, never the path.
    expect(bodyReal).toBe(bodyMissing);

    const card = JSON.parse(bodyReal) as { card: string; api_base_url: string };
    expect(card.card).toBe(CARD);
    // The API base the card teaches a client to re-root onto: this origin's own /api mount.
    expect(card.api_base_url).toBe(`${BASE_URL}/api`);

    // Card headers: never cached keyed on path, varies on Accept, never indexed.
    expect(real.headers()["cache-control"]).toContain("no-store");
    expect(real.headers().vary).toContain("accept");
    expect(real.headers()["x-robots-tag"]).toContain("noindex");
  });

  test("curl (*/*): a real and a nonexistent address return the SAME markdown card", async ({
    request,
  }) => {
    const real = await request.get(`/${WORKSPACE_ADDRESS}`, {
      headers: { accept: "*/*" },
      maxRedirects: 0,
    });
    const missing = await request.get(`/${NONEXISTENT}`, {
      headers: { accept: "*/*" },
      maxRedirects: 0,
    });
    expect(real.status()).toBe(200);
    expect(missing.status()).toBe(200);
    expect(real.headers()["content-type"]).toContain("text/plain");

    const textReal = await real.text();
    expect(textReal).toBe(await missing.text());
    expect(textReal).toContain(TEASER_MARKER);
    expect(textReal).toContain("topos follow");
    expect(textReal).not.toContain(WS_DISPLAY_NAME);
  });

  test("browser (text/html): both addresses render the SAME constant teaser, no name leak", async ({
    request,
  }) => {
    const real = await request.get(`/${WORKSPACE_ADDRESS}`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    const missing = await request.get(`/${NONEXISTENT}`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    expect(real.status()).toBe(200);
    expect(missing.status()).toBe(200);

    const htmlReal = await real.text();
    const htmlMissing = await missing.text();
    // The constant teaser marker is present on BOTH pages…
    expect(htmlReal).toContain(TEASER_MARKER);
    expect(htmlMissing).toContain(TEASER_MARKER);
    // …and the real workspace's name leaks into NEITHER (the teaser is path-blind).
    expect(htmlReal).not.toContain(WS_DISPLAY_NAME);
    expect(htmlMissing).not.toContain(WS_DISPLAY_NAME);
  });

  test("a deep garbage path returns the same constant JSON card (the catch-all)", async ({
    request,
  }) => {
    const deep = await request.get("/nothing/here/at/all", {
      headers: { accept: "application/json" },
      maxRedirects: 0,
    });
    const workspace = await request.get(`/${WORKSPACE_ADDRESS}`, {
      headers: { accept: "application/json" },
      maxRedirects: 0,
    });
    expect(deep.status()).toBe(200);
    // The unmatched-path fallback serves the SAME constant card the resource addresses do.
    const deepBody = await deep.text();
    expect(deepBody).toBe(await workspace.text());
    expect((JSON.parse(deepBody) as { card: string }).card).toBe(CARD);
  });

  test("JSON at `/` answers the same constant card (the token-less doors card-fetch the bare origin)", async ({
    request,
  }) => {
    const root = await request.get("/", {
      headers: { accept: "application/json" },
      maxRedirects: 0,
    });
    const workspace = await request.get(`/${WORKSPACE_ADDRESS}`, {
      headers: { accept: "application/json" },
      maxRedirects: 0,
    });
    expect(root.status()).toBe(200);
    // Byte-identical to every other path's card: the root is a resource address like any other.
    expect(await root.text()).toBe(await workspace.text());
  });

  test("a browser at `/` still gets the landing page, never the card", async ({ request }) => {
    const root = await request.get("/", {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    expect(root.status()).toBe(200);
    expect(root.headers()["content-type"]).toContain("text/html");
    expect(await root.text()).toContain("<html");
  });
});

test.describe("signed in — the browser faces resolve against the confirmed seat", () => {
  test("a member browsing the workspace address lands on the workspace surface", async ({
    page,
  }) => {
    // The suite's default identity holds the claimed owner seat on WORKSPACE_ADDRESS.
    const ws = await theWorkspace();
    await page.goto(`/${WORKSPACE_ADDRESS}`);
    await page.waitForURL((u) => u.pathname === `/workspaces/${ws.id}`);
    expect(page.url()).toContain(`/workspaces/${ws.id}`);
  });

  test("the channel + skill address shapes redirect a member to the matching subpage", async ({
    page,
  }) => {
    const ws = await theWorkspace();
    // A raw HTML fetch over the page's own session — assert the 302 target directly (the
    // subpage it lands on may be empty; the REDIRECT is what these address shapes promise).
    const channel = await page.request.get(`/${WORKSPACE_ADDRESS}/channels/everyone`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    expect(channel.status()).toBe(302);
    expect(channel.headers().location).toBe(`/workspaces/${ws.id}/channels/everyone`);

    const skill = await page.request.get(`/${WORKSPACE_ADDRESS}/skills/deploy-runbook`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    expect(skill.status()).toBe(302);
    expect(skill.headers().location).toBe(`/workspaces/${ws.id}/skills/deploy-runbook`);
  });

  test("a signed-in NON-member gets the house 404 (a miss is indistinguishable from a denial)", async ({
    browser,
  }) => {
    // A fresh account with NO seat: the address resolves to nothing they may see, and the
    // loader's notFound() renders the uniform house 404.
    const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
    const page = await context.newPage();
    try {
      await signIn(page, "card-outsider@example.com");
      await gotoSettled(page, `/${WORKSPACE_ADDRESS}`);
      await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
    } finally {
      await context.close();
    }
  });
});
