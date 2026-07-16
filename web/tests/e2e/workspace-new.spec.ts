import { expect, test } from "@playwright/test";
import { gotoSettled } from "./sign-in";

/**
 * Self-serve workspace creation is a MULTI-tenant page; the suite runs SINGLE-tenant (the install
 * IS its one workspace, born at boot). So `/new` mounts no route here and falls through to the
 * house 404 — the same uniform miss any unclaimed path gets — even for the signed-in owner.
 */

test("/new is the house 404 in single tenancy — nothing to create", async ({ page }) => {
  await gotoSettled(page, "/new");
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
});
