import { expect, test } from "@playwright/test";
import { BASE_URL, E2E_PASSWORD } from "./env";
import { ensureSeatedUser, theWorkspace } from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The SEAT-semantics invariants on the single-tenant install: a seat is the ONLY admission —
 * an account without one sees nothing and enters nothing; a seat admits everywhere. Plus the
 * adjacent `/api/memberships` surface (the rail's refetch target) that rides the same truth.
 *
 * Each test signs in as its own identity, so the suite's default storage state is not used.
 */

test.use({ storageState: { cookies: [], origins: [] } });

test("an account WITHOUT a seat sees the honest miss everywhere — never a 403", async ({
  page,
}) => {
  await theWorkspace();
  // Create-or-sign-in a SEATLESS account via the auth REST flow (a seatless person never lands the
  // signed-in shell, so the banner-asserting signIn helper can't front this — sign in by hand).
  const email = "seatless@example.com";
  const created = await page.request.post("/api/auth/sign-up/email", {
    data: { email, password: E2E_PASSWORD, name: "seatless" },
    headers: { origin: BASE_URL },
    failOnStatusCode: false,
  });
  if (!created.ok()) {
    const signedIn = await page.request.post("/api/auth/sign-in/email", {
      data: { email, password: E2E_PASSWORD },
      headers: { origin: BASE_URL },
      failOnStatusCode: false,
    });
    expect(signedIn.ok(), `sign-in failed for ${email}: ${await signedIn.text()}`).toBe(true);
  }

  // The door resolver sends a seatless person to the house 404 — never a seatless pane, never 403.
  await gotoSettled(page, "/app");
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
  // Direct navigation to any workspace surface is the same uniform 404.
  await gotoSettled(page, "/");
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
  await gotoSettled(page, "/members");
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Members" })).toHaveCount(0);
});

test("a seat row alone admits: the member lands on the dashboard through /app", async ({
  page,
}) => {
  await theWorkspace();
  await ensureSeatedUser("plain-member@example.com", "member");
  await signIn(page, "plain-member@example.com");
  await gotoSettled(page, "/app");
  await page.waitForURL(`**/`);
  await expect(page.getByRole("banner")).toBeVisible();
});

test("/api/memberships answers the session's seats — 401 signed out, rows signed in", async ({
  page,
  request,
}) => {
  const anonymous = await request.get("/api/memberships");
  expect(anonymous.status()).toBe(401);

  const ws = await theWorkspace();
  await ensureSeatedUser("plain-member@example.com", "member");
  await signIn(page, "plain-member@example.com");
  const response = await page.request.get("/api/memberships");
  expect(response.status()).toBe(200);
  const memberships = (await response.json()) as { id: string; address: string }[];
  expect(memberships).toHaveLength(1);
  expect(memberships[0]?.id).toBe(ws.id);
  expect(memberships[0]?.address).toBe(ws.name);
});
