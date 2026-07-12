import { expect, test } from "@playwright/test";
import {
  INVITED_EMAIL,
  JOINER_EMAIL,
  OUTSIDER_EMAIL,
  ROSTER_WS,
  SKILL,
  WS,
} from "../fixtures/plane/data.mjs";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The SEED-STATE membership proofs — the row-semantics invariants, asserted ONLY from what
 * auth.setup.ts seeded (PLANE_SEED plane.* rows). HARNESS DISCIPLINE: no fixture-vault mutation
 * happens here — SQL-derived surfaces (dashboard rows, guards) answer to the seed alone.
 *
 * Each test signs in as its own identity, so the suite's default storage state is not used.
 */

test.use({ storageState: { cookies: [], origins: [] } });

test("an enrolled joiner's seat alone puts the workspace on their dashboard", async ({ page }) => {
  // JOINER_EMAIL exists ONLY as a confirmed plane.workspace_member row — zero web-tier rows
  // (sign-in creates auth-tier rows, never membership).
  await signIn(page, JOINER_EMAIL);
  await gotoSettled(page, "/workspaces");
  const index = page.getByRole("main");
  const row = index.locator(`a[href="/workspaces/${WS}"]`);
  await expect(row).toBeVisible();
  await expect(row.getByText("E2E Workspace")).toBeVisible();
  // A plain confirmed seat: navigable, with no invited framing anywhere.
  await expect(index.getByText("invited", { exact: true })).toHaveCount(0);

  // The row is a real door: the workspace page renders for the joiner, catalog included.
  await row.click();
  await page.waitForURL(`**/workspaces/${WS}`);
  await expect(page.getByRole("heading", { name: "E2E Workspace" })).toBeVisible();
  await expect(page.getByRole("link", { name: SKILL })).toBeVisible();
});

test("an invited-only seat is index-visible with the join framing but never admits", async ({
  page,
}) => {
  // INVITED_EMAIL holds an invited (never confirmed) seat on w_roster: the index row IS its entire
  // surface — the join instructions — and direct navigation stays the uniform 404 (an invite
  // promises visibility; the enrollment proof is the real join).
  await signIn(page, INVITED_EMAIL);
  await gotoSettled(page, "/workspaces");
  const index = page.getByRole("main");
  await expect(index.getByText("Roster Workspace")).toBeVisible();
  await expect(index.getByText("invited", { exact: true })).toBeVisible();
  await expect(index.getByText("your seat confirms when a device enrolls")).toBeVisible();
  // A NON-link card: no anchor into the workspace exists anywhere on the index.
  await expect(index.locator(`a[href="/workspaces/${ROSTER_WS}"]`)).toHaveCount(0);

  await gotoSettled(page, `/workspaces/${ROSTER_WS}`);
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();

  // The members surface answers the same way — an invite is index visibility, never admission.
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/members`);
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Members" })).toHaveCount(0);
});

test("a signed-in non-member gets 404 on the workspace and no index row — the uniform miss", async ({
  page,
}) => {
  // OUTSIDER_EMAIL holds no seat anywhere: a signed-in identity outside the roster sees nothing and
  // enters nothing.
  await signIn(page, OUTSIDER_EMAIL);
  await gotoSettled(page, "/workspaces");
  const index = page.getByRole("main");
  // No index row for any seeded workspace — the empty state renders instead.
  await expect(index.getByText("Create your first workspace")).toBeVisible();
  await expect(index.locator(`a[href="/workspaces/${WS}"]`)).toHaveCount(0);
  await expect(index.getByText("E2E Workspace")).toHaveCount(0);

  // Direct navigation is the uniform 404 — the app does not confirm what exists.
  await gotoSettled(page, `/workspaces/${WS}`);
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "E2E Workspace" })).toHaveCount(0);

  // The nested surfaces answer the same way (settings + members included).
  await gotoSettled(page, `/workspaces/${WS}/settings`);
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Settings" })).toHaveCount(0);

  await gotoSettled(page, `/workspaces/${WS}/members`);
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Members" })).toHaveCount(0);
});
