import { expect, test } from "@playwright/test";
import { ensureSeatedUser, theWorkspace } from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The SEAT-semantics invariants on the single-tenant install: a seat is the ONLY admission —
 * an account without one sees nothing and enters nothing; a seat admits everywhere. Plus the
 * two adjacent surfaces that ride the same truth: `/api/memberships` (the rail's refetch
 * target) and the historical `/create` shape's permanent redirect.
 *
 * Each test signs in as its own identity, so the suite's default storage state is not used.
 */

test.use({ storageState: { cookies: [], origins: [] } });

test("an account WITHOUT a seat sees the honest miss everywhere — never a 403", async ({
  page,
}) => {
  const ws = await theWorkspace();
  // The account exists (registration is open on this stack) but holds NO seat row.
  await signIn(page, "seatless@example.com");
  await page.waitForURL("**/workspaces");
  // The workspaces index renders the honest seatless pane (a 404-shaped page inside the shell).
  await expect(page.getByRole("heading", { name: "No seat here" })).toBeVisible();

  // Direct navigation is the uniform 404 — the app does not confirm what exists.
  await gotoSettled(page, `/workspaces/${ws.id}`);
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
  await gotoSettled(page, `/workspaces/${ws.id}/members`);
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Members" })).toHaveCount(0);
});

test("a seat row alone admits: the member lands on the dashboard through /app", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await ensureSeatedUser("plain-member@example.com", "member");
  await signIn(page, "plain-member@example.com");
  await gotoSettled(page, "/app");
  await page.waitForURL(`**/workspaces/${ws.id}`);
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

test("the historical /create shape permanently redirects to /workspaces", async ({ request }) => {
  const response = await request.get("/create", { maxRedirects: 0 });
  expect(response.status()).toBe(301);
  expect(response.headers().location).toBe("/workspaces");
});
