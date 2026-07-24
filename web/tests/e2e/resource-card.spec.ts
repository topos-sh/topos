import { expect, test } from "@playwright/test";
import { BASE_URL, WORKSPACE_ADDRESS } from "./env";
import { adminQuery, ensureBundle, theWorkspace } from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The resource ADDRESS (`<origin>/<workspace>[...]`) — its content-negotiated faces and the
 * no-existence-oracle promise. WORKSPACE_ADDRESS ("team") is the boot-minted workspace whose
 * claimed OWNER is the suite's default identity; a made-up address stands in for "nonexistent".
 *
 * The invariant under test: a non-browser fetcher gets ONE constant protocol card on every path
 * — real, nonexistent, or deep garbage, the origin root included — byte-for-byte identical, so
 * nothing about the response reveals whether the path names anything. An anonymous browser gets
 * the constant teaser/landing ONLY at the workspace root; a skill or channel face is members-only,
 * so a signed-out browser gets the uniform house 404 there — indistinguishable from a mistyped
 * path, and byte-identical whether the name is real or invented. A member gets the canonical page.
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
  // The member-face tests below assert a real skill's page renders (a catalog row is enough — the
  // face is 200 the moment a skill NAME exists, published or not). Seed it so a fresh CI DB has it.
  await ensureBundle({ id: "s_e2e_card_skill", name: "card-face-runbook" });
});

test.afterAll(async () => {
  await adminQuery(`update web.workspace set display_name = $1`, [originalDisplayName]);
  // Remove the dedicated face skill so it never leaks into another spec's catalog assertions.
  await adminQuery(`delete from web.bundle where id = $1`, ["s_e2e_card_skill"]);
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
    expect(textReal).toContain("topos login");
    expect(textReal).not.toContain(WS_DISPLAY_NAME);
  });

  test("browser (text/html): slug-shaped paths answer the SAME house 404, no name leak", async ({
    request,
  }) => {
    // Single-tenant grammar: the ORIGIN is the workspace address, so a `/<slug>` path names
    // nothing — the real slug and a made-up one must be indistinguishable (the house 404).
    const real = await request.get(`/${WORKSPACE_ADDRESS}`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    const missing = await request.get(`/${NONEXISTENT}`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    expect(real.status()).toBe(404);
    expect(missing.status()).toBe(404);

    const htmlReal = await real.text();
    const htmlMissing = await missing.text();
    expect(htmlReal).toContain("Page not found");
    expect(htmlMissing).toContain("Page not found");
    // The real workspace's name leaks into NEITHER (the miss is path-blind).
    expect(htmlReal).not.toContain(WS_DISPLAY_NAME);
    expect(htmlMissing).not.toContain(WS_DISPLAY_NAME);
  });

  test("browser (text/html): the skill FACE is the uniform house 404 for a signed-out visitor — real and missing names alike", async ({
    request,
  }) => {
    // A skill page is members-only. Anonymous → the house 404, NOT a teaser: the marker is gone,
    // the status is 404, and a real skill name is byte-identical to an invented one (no oracle).
    const real = await request.get(`/skills/card-face-runbook`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    const missing = await request.get(`/skills/no-such-skill-zzz`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    expect(real.status()).toBe(404);
    expect(missing.status()).toBe(404);

    const htmlReal = await real.text();
    const htmlMissing = await missing.text();
    // The house 404, not the teaser: the resource-address marker is on NEITHER page, and both
    // carry the constant "Page not found" screen. The response is EXISTENCE-BLIND — both throw
    // before any workspace/catalog read, so a real skill name and an invented one differ only by
    // the path the visitor themselves typed (which the address bar already holds), never by any
    // server-side signal. The real workspace's display name — the true secret — leaks into NEITHER.
    expect(htmlReal).not.toContain(TEASER_MARKER);
    expect(htmlMissing).not.toContain(TEASER_MARKER);
    expect(htmlReal).toContain("Page not found");
    expect(htmlMissing).toContain("Page not found");
    expect(htmlReal).not.toContain(WS_DISPLAY_NAME);
    expect(htmlMissing).not.toContain(WS_DISPLAY_NAME);
  });

  test("browser (text/html): the channel FACE is the uniform house 404 for a signed-out visitor — real and missing names alike", async ({
    request,
  }) => {
    // Same posture for a channel: members-only, so anonymous → the house 404. `everyone` is the
    // implicit default channel every workspace is born with — the realest name there is.
    const real = await request.get(`/channels/everyone`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    const missing = await request.get(`/channels/no-such-channel-zzz`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    expect(real.status()).toBe(404);
    expect(missing.status()).toBe(404);

    const htmlReal = await real.text();
    const htmlMissing = await missing.text();
    expect(htmlReal).not.toContain(TEASER_MARKER);
    expect(htmlMissing).not.toContain(TEASER_MARKER);
    expect(htmlReal).toContain("Page not found");
    expect(htmlMissing).toContain("Page not found");
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
  test("a member browsing the ORIGIN lands on the workspace dashboard", async ({ page }) => {
    // The suite's default identity holds the claimed owner seat; the origin IS the address.
    // The member SEES the display name the anonymous faces must never leak — the same string,
    // opposite sides of the seat check.
    await theWorkspace();
    await page.goto(`/`);
    await expect(
      page.getByRole("main").getByText(WS_DISPLAY_NAME, { exact: false }).first(),
    ).toBeVisible();
  });

  test("a member's channel + skill faces render directly at their origin-rooted addresses", async ({
    page,
  }) => {
    await theWorkspace();
    const channel = await page.request.get(`/channels/everyone`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    expect(channel.status()).toBe(200);
    expect(await channel.text()).not.toContain(TEASER_MARKER);

    const skill = await page.request.get(`/skills/card-face-runbook`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    expect(skill.status()).toBe(200);
    expect(await skill.text()).not.toContain(TEASER_MARKER);
  });

  test("a member at a stale slug-shaped path gets the house 404 (the origin is the address)", async ({
    page,
  }) => {
    await theWorkspace();
    const stale = await page.request.get(`/${WORKSPACE_ADDRESS}`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    expect(stale.status()).toBe(404);
  });

  test("a signed-in NON-member gets the house 404 (a miss is indistinguishable from a denial)", async ({
    browser,
  }) => {
    // A fresh account with NO seat: the origin face resolves to nothing they may see, and the
    // loader's notFound() renders the uniform house 404.
    const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
    const page = await context.newPage();
    try {
      await signIn(page, "card-outsider@example.com");
      await gotoSettled(page, `/`);
      await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
    } finally {
      await context.close();
    }
  });
});
