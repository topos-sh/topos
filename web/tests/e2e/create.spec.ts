import { expect, type Page, test } from "@playwright/test";
import { CREATED_ADDRESS } from "../fixtures/plane/data.mjs";
import { MEMBER_EMAIL, PLANE_PORT } from "./env";

/**
 * The /workspaces/new page (signed in via the suite's default storage state): the one optional-name
 * form → the vault's create write → the paste-to-your-agent block. The recorded fixture call proves
 * the wire: the acting identity is the session-derived acting-email header, the body carries only a
 * server-minted UUID request_id. The success hand-off is the workspace ADDRESS — `topos follow
 * <address>`, the vault's FULL address rendered verbatim — not a tokened link (links carry nothing;
 * the roster is the lock).
 *
 * HTTP-surface proof ONLY (harness discipline): creation seats the owner on the DIRECTORY's roster;
 * the fixture's in-memory create never syncs into the seeded plane.* SQL rows, so the created
 * workspace deliberately does NOT appear on any dashboard mid-suite.
 */

async function recordedCreates(
  page: Page,
): Promise<{ route: string; acting: string; body: Record<string, unknown> }[]> {
  const response = await page.request.get(`http://127.0.0.1:${PLANE_PORT}/__test/calls`);
  const calls: { route: string; acting: string; body: Record<string, unknown> }[] =
    await response.json();
  return calls.filter((c) => c.route === "create-workspace");
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

test("create a workspace: prefilled name → submit → the paste-to-agent block", async ({ page }) => {
  await page.goto("/workspaces/new");
  await expect(page.getByRole("heading", { name: "Create a workspace" })).toBeVisible();
  const name = page.getByRole("textbox", { name: "Workspace name" });
  await expect(name).toHaveValue("reviewer's workspace");
  await name.fill("Acme Platform");
  await page.getByRole("button", { name: "Create workspace" }).click();

  // The success screen IS the hand-off: the join address (the address itself teaches the agent),
  // a copy button, and the explicit follow command as the terminal-user fallback line.
  await expect(page.getByRole("heading", { name: "Acme Platform is ready" })).toBeVisible();
  await expect(page.getByText("Paste this command to your agent", { exact: false })).toBeVisible();
  const followCmd = `topos follow ${CREATED_ADDRESS}`;
  await expect(page.getByText(followCmd)).toBeVisible();
  await expect(page.getByRole("button", { name: /copy/i })).toBeVisible();

  const creates = await recordedCreates(page);
  expect(creates.length).toBeGreaterThan(0);
  const last = creates.at(-1);
  expect(last?.acting).toBe(MEMBER_EMAIL);
  expect((last?.body as { request_id: string }).request_id).toMatch(UUID);
});
