import { expect, test } from "@playwright/test";
import { ROSTER_ADDRESS, WS, WS_ADDRESS } from "../fixtures/plane/data.mjs";

/**
 * The resource ADDRESS (`<origin>/<workspace>[...]`) — its content-negotiated faces and the
 * no-existence-oracle promise. The seeded directory rows come from auth.setup.ts (a superuser pg
 * seed on E2E_ADMIN_URL): WS_ADDRESS ("e2e") is a real workspace whose confirmed member is the
 * suite's default identity (reviewer@example.com); ROSTER_ADDRESS ("roster-ws") is a real
 * workspace that identity holds NO seat in. A single made-up address stands in for "nonexistent".
 *
 * The invariant under test: a non-browser fetcher gets ONE constant protocol card on every path
 * — real, nonexistent, or deep garbage — byte-for-byte identical, so nothing about the response
 * reveals whether the path names anything. A browser gets the constant teaser when anonymous, a
 * redirect into the workspace when it holds the seat, and the uniform house 404 otherwise.
 */

const NONEXISTENT = "no-such-workspace-zzz";
const CARD = "topos-protocol-card";
const TEASER_MARKER = "A Topos resource address";
// The seeded display name of WS (PLANE_SEED.workspaces[0]) — it must leak into NONE of the
// anonymous faces (the address bar is the visitor's own knowledge; the response never confirms it).
const WS_DISPLAY_NAME = "E2E Workspace";

test.describe("anonymous — the constant protocol card + teaser (no existence oracle)", () => {
  test.use({ storageState: { cookies: [], origins: [] } });

  test("JSON: a real and a nonexistent address return the SAME 200 card", async ({ request }) => {
    const real = await request.get(`/${WS_ADDRESS}`, {
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
    // Byte-identical: the ONLY input to the card is the deployment's follow base, never the path.
    expect(bodyReal).toBe(bodyMissing);

    const card = JSON.parse(bodyReal) as { card: string; api_base_url: string };
    expect(card.card).toBe(CARD);
    expect(typeof card.api_base_url).toBe("string");
    expect(card.api_base_url.length).toBeGreaterThan(0);

    // Card headers: never cached keyed on path, varies on Accept, never indexed.
    expect(real.headers()["cache-control"]).toContain("no-store");
    expect(real.headers().vary).toContain("accept");
    expect(real.headers()["x-robots-tag"]).toContain("noindex");
  });

  test("curl (*/*): a real and a nonexistent address return the SAME markdown card", async ({
    request,
  }) => {
    const real = await request.get(`/${WS_ADDRESS}`, {
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
    const real = await request.get(`/${WS_ADDRESS}`, {
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
    const workspace = await request.get(`/${WS_ADDRESS}`, {
      headers: { accept: "application/json" },
      maxRedirects: 0,
    });
    expect(deep.status()).toBe(200);

    const deepBody = await deep.text();
    // The unmatched-path fallback serves the SAME constant card the resource addresses do.
    expect(deepBody).toBe(await workspace.text());
    expect((JSON.parse(deepBody) as { card: string }).card).toBe(CARD);
  });
});

test.describe("signed in — the browser faces resolve against the confirmed seat", () => {
  test("a member browsing the workspace address lands on the workspace surface", async ({
    page,
  }) => {
    // The suite's default identity holds a confirmed seat in WS (address WS_ADDRESS).
    await page.goto(`/${WS_ADDRESS}`);
    await page.waitForURL((u) => u.pathname === `/workspaces/${WS}`);
    expect(page.url()).toContain(`/workspaces/${WS}`);
  });

  test("the channel + skill address shapes redirect a member to the matching subpage", async ({
    page,
  }) => {
    // A raw HTML fetch over the page's own session — assert the 302 target directly (the subpage it
    // lands on may be empty; the REDIRECT is what these address shapes promise).
    const channel = await page.request.get(`/${WS_ADDRESS}/channels/general`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    expect(channel.status()).toBe(302);
    expect(channel.headers().location).toBe(`/workspaces/${WS}/channels/general`);

    const skill = await page.request.get(`/${WS_ADDRESS}/skills/deploy-runbook`, {
      headers: { accept: "text/html" },
      maxRedirects: 0,
    });
    expect(skill.status()).toBe(302);
    expect(skill.headers().location).toBe(`/workspaces/${WS}/skills/deploy-runbook`);
  });

  test("a signed-in NON-member gets the house 404 (a miss is indistinguishable from a denial)", async ({
    page,
  }) => {
    // The default identity holds no seat in ROSTER_ADDRESS's workspace: the address resolves to
    // nothing they can see, and the loader's notFound() renders the uniform house 404.
    await page.goto(`/${ROSTER_ADDRESS}`);
    await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
  });
});
