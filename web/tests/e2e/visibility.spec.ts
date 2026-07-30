import { expect, test } from "@playwright/test";
import { gotoSettled } from "./sign-in";

/**
 * THE VISIBILITY PAGE — the disclosure of what this workspace reads from a member's machines.
 * Its whole design claim is the ORDER: somebody deciding whether to log a work machine in is
 * asking what this exposes, so the limits come first and the reporting second. The heading
 * order is therefore the thing under test, alongside the live proof list at the end.
 */

test("the never-see block leads, the does-see block follows, and the reader's own rows close it", async ({
  page,
}) => {
  await gotoSettled(page, "/visibility");
  await expect(page.getByRole("heading", { name: "What the team can see" })).toBeVisible();

  // The order, asserted as order — not merely as presence. Scoped to the content column, so
  // the sidebar's own group labels never enter the sequence.
  const headings = page.getByRole("main").locator("h2");
  await expect(headings.nth(0)).toHaveText("The team can never see");
  await expect(headings.nth(1)).toHaveText("What the team does see");
  await expect(headings.nth(2)).toHaveText("Your machines, as this workspace reads them");

  // The four limits are stated plainly, each naming what is NOT read.
  const never = page.getByTestId("visibility-never");
  await expect(never.getByText("The contents of your files.")).toBeVisible();
  await expect(never.getByText("Your prompts and conversations.")).toBeVisible();
  await expect(never.getByText("Your repository's code.")).toBeVisible();
  await expect(never.getByText("Anything you have not published.")).toBeVisible();

  // …and the reporting block names exactly the fields the list below shows.
  const sees = page.getByTestId("visibility-sees");
  await expect(sees.getByText("Which shared skills it holds,")).toBeVisible();
  await expect(sees.getByText("The version of each one,")).toBeVisible();
  await expect(page.getByTestId("visibility-your-sessions")).toBeVisible();
});

test("the sessions page points at it, so the disclosure is one click from the fleet view", async ({
  page,
}) => {
  await gotoSettled(page, "/settings/sessions");
  await page.getByRole("link", { name: "What the team can and cannot see" }).click();
  await page.waitForURL((u) => u.pathname.endsWith("/visibility"));
  await expect(page.getByRole("heading", { name: "What the team can see" })).toBeVisible();
});
