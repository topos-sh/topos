import { expect, test } from "@playwright/test";
import { BASE_URL } from "./env";
import { theWorkspace } from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * The signed-in LEFT PANEL: the workspace's Skills + MCP servers + Channels sections each carry a
 * section-header `+ new`, and the Skills one opens the publish-from-your-agent dialog that hands
 * the exact copyable lines composed for THIS workspace's real address. Each section renders
 * whether or not it holds anything — its header carries the only way to put the first thing in
 * it. Runs as the suite's default OWNER on the single-tenant boot workspace, so the address is
 * the bare origin.
 */

test("the panel carries Skills + MCP servers + Channels; the publish dialog hands the exact lines", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/`);

  // The three workspace sections are present via their section-header affordances (unique to the
  // panel — the dashboard main lists the catalog but carries none of these). The publish dialog
  // is the SKILLS way in and offers nothing else; adding a server is its own page.
  const publishNew = page.getByRole("button", { name: "Publish a skill from your agent" });
  await expect(publishNew).toBeVisible();
  // Scoped to the rail's own MCP group: the dashboard offers the same act under the same name,
  // which is deliberate — one act, one name.
  await expect(
    page
      .locator('[data-sidebar="group"]', { hasText: "MCP servers" })
      .first()
      .getByRole("link", { name: "Add an MCP server" }),
  ).toBeVisible();
  await expect(page.getByRole("link", { name: "New channel" })).toBeVisible();

  // The Channels list surfaces the implicit `everyone` channel (only the panel lists channels on `/`).
  await expect(page.getByRole("link", { name: "everyone" })).toBeVisible();

  // The Skills `+ new` opens the publish-from-your-agent dialog with the exact lines, composed for
  // this workspace's real address (single-tenant → the bare origin, no slug). The web app authors
  // no bundle — the lines are copyable placeholders the person runs on a logged-in machine.
  await publishNew.click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("heading", { name: "Publish from your agent" })).toBeVisible();
  // Each copyable line is one span, so match exactly to avoid double-matching its ancestor block.
  await expect(
    dialog.getByText("Share my <skill> skill with the team on Topos — publish it.", {
      exact: true,
    }),
  ).toBeVisible();
  await expect(dialog.getByText(`topos login ${BASE_URL}`, { exact: true })).toBeVisible();
  await expect(
    dialog.getByText("topos publish <path-to-skill-directory>", { exact: true }),
  ).toBeVisible();
});
