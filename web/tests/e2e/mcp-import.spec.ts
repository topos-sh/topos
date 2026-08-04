import { expect, test } from "@playwright/test";
import { adminQuery } from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * ADDING AN MCP SERVER, the whole way through: the signed-in owner opens the import page from
 * the dashboard, picks a server or pastes a document, reads what it promises, publishes, and
 * finds it in the catalog as a `kind: 'mcp'` bundle.
 *
 * Two arms, and neither touches the network: the PICKER reads a row out of the committed list,
 * and the PASTE arm is what a team with an internal server uses. Both land in the same preview
 * and the same publish, which is the point being asserted. The bytes go through the ordinary
 * custody path (the fixture vault), so a green run means the whole publish really happened, not
 * that a form validated.
 */

const SERVER_NAME = "io.github.acme/tide-tables";
const CATALOG_NAME = "tide-tables";

const DOCUMENT = JSON.stringify(
  {
    name: SERVER_NAME,
    description: "Tide tables and station metadata for a coastline.",
    version: "2.1.0",
    remotes: [
      {
        type: "streamable-http",
        url: "https://tides.acme.example/mcp",
        headers: [{ name: "X-Region", value: "eu-west-1" }],
      },
    ],
  },
  null,
  2,
);

test.beforeEach(async () => {
  // A previous run's copy would collide on the embedded name (deliberately) — clear it first.
  await adminQuery(
    `delete from web.bundle where name like $1 and workspace_id in (select id from web.workspace)`,
    [`${CATALOG_NAME}%`],
  );
});

test("the picker lists popular servers, filters, and lands a pick in the same preview", async ({
  page,
}) => {
  await gotoSettled(page, "/");
  await page.getByRole("link", { name: "Add MCP server" }).click();
  await expect(page.getByRole("heading", { name: "Add an MCP server" })).toBeVisible();

  // The page RESTS on the list: no typing needed to see what is on offer.
  const picker = page.getByTestId("mcp-picker");
  await expect(picker).toBeVisible();
  const options = page.getByTestId("mcp-picker-option");
  const total = await options.count();
  expect(total).toBeGreaterThanOrEqual(20);

  // The search narrows it without a round trip.
  await page.getByTestId("mcp-picker-search").fill("linear");
  await expect(options).toHaveCount(1);
  await expect(options.first()).toContainText("Linear");
  await expect(options.first()).toContainText("mcp.linear.app");

  // Choosing one is a submit that lands in the SAME preview a paste reaches.
  await options.first().click();
  const preview = page.getByTestId("mcp-preview");
  await expect(preview).toBeVisible();
  await expect(preview).toContainText("app.linear/linear");
  await expect(preview).toContainText("streamable-http");
  await expect(preview).toContainText("oauth");
  await expect(page.getByTestId("mcp-preview-url")).toHaveText("https://mcp.linear.app/mcp");
  // The suggested catalog name is the product's, not the registry name's "mcp" tail.
  await expect(page.getByLabel("Publish as")).toHaveValue("linear");
  // A preview still writes nothing.
  expect(await adminQuery(`select 1 from web.bundle where name = 'linear'`)).toHaveLength(0);
});

test("paste a server.json, preview what it promises, publish it into the catalog", async ({
  page,
}) => {
  // The dashboard is where the affordance lives — reached by clicking, not by typing a URL.
  await gotoSettled(page, "/");
  await page.getByRole("link", { name: "Add MCP server" }).click();
  await expect(page.getByRole("heading", { name: "Add an MCP server" })).toBeVisible();

  // The typed sources sit behind the disclosure; the picker is what the page opens on.
  await page.getByText("Custom server", { exact: true }).click();
  await page.getByLabel("Where it comes from").selectOption("paste");
  await page.getByTestId("mcp-paste").fill(DOCUMENT);
  await page.getByRole("button", { name: "Preview" }).click();

  // The preview says what an agent would actually be pointed at.
  const preview = page.getByTestId("mcp-preview");
  await expect(preview).toBeVisible();
  await expect(preview).toContainText(SERVER_NAME);
  await expect(page.getByTestId("mcp-preview-url")).toHaveText("https://tides.acme.example/mcp");
  await expect(preview).toContainText("streamable-http");
  await expect(preview).toContainText("X-Region: eu-west-1");
  // Nothing is written by a preview.
  expect(await adminQuery(`select 1 from web.bundle where name = $1`, [CATALOG_NAME])).toHaveLength(
    0,
  );

  await page.getByTestId("mcp-publish").click();

  // The publish lands on the new skill's page, and the catalog row is an mcp bundle.
  await page.waitForURL(`**/skills/${CATALOG_NAME}`);
  await expect(page.getByRole("heading", { name: CATALOG_NAME })).toBeVisible();
  const rows = await adminQuery<{ kind: string }>(`select kind from web.bundle where name = $1`, [
    CATALOG_NAME,
  ]);
  expect(rows[0]?.kind).toBe("mcp");

  // And the dashboard lists it with its kind, like any other bundle.
  await gotoSettled(page, "/");
  // Scoped to the content pane: the sidebar lists the same name, and both links are real.
  await expect(
    page.getByRole("main").getByRole("link", { name: new RegExp(CATALOG_NAME) }),
  ).toContainText("mcp");
});

test("a document carrying a credential is refused, and nothing is published", async ({ page }) => {
  await gotoSettled(page, "/mcp/import");
  await page.getByText("Custom server", { exact: true }).click();
  await page.getByLabel("Where it comes from").selectOption("paste");
  await page.getByTestId("mcp-paste").fill(
    JSON.stringify({
      name: "io.github.acme/keyed",
      description: "A server that wants a key in a header.",
      version: "1.0.0",
      remotes: [
        {
          type: "streamable-http",
          url: "https://keyed.acme.example/mcp",
          headers: [{ name: "X-Api-Key", value: "fill-me-in", isSecret: true }],
        },
      ],
    }),
  );
  await page.getByRole("button", { name: "Preview" }).click();

  await expect(page.getByTestId("mcp-refusal")).toContainText("MCP_SECRET_REFUSED");
  await expect(page.getByTestId("mcp-preview")).toHaveCount(0);
  expect(await adminQuery(`select 1 from web.bundle where name = 'keyed'`)).toHaveLength(0);
});
