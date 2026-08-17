import { expect, type Page, test } from "@playwright/test";
import { adminQuery, ensureBundle, ensureSeatedUser } from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * ADDING AN MCP SERVER, the whole way through: the signed-in owner opens the add-a-server page
 * from the dashboard, connects one from the catalog or writes down one the catalog does not
 * carry, and finds it in the workspace as a `kind: 'mcp'` bundle.
 *
 * Two arms, and neither touches the network: CONNECTING reads a catalog row the page already
 * carries — so choosing one asks the server nothing at all — and WRITING ONE DOWN is what a team
 * with an internal server does. Both end with a bundle that names a server, which is the point
 * being asserted: a green run means the rows really were written, not that a form validated.
 */

const SERVER_NAME = "io.github.acme/tide-tables";
const CATALOG_NAME = "tide-tables";
/** A plain member — the one role a curated channel withholds a placement from. */
const MEMBER = "mcp-member@example.com";

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

/**
 * Every request the page makes that is not a plain asset read — a document navigation, or any
 * non-GET. Choosing a server must produce NONE of them. That round trip is exactly why this page
 * felt broken (the click posted, the grid went inert, the answer arrived below the fold), and a
 * counter is the only thing that keeps it from creeping back.
 */
function watchTraffic(page: Page): string[] {
  const seen: string[] = [];
  page.on("request", (request) => {
    if (request.method() !== "GET" || request.resourceType() === "document") {
      seen.push(`${request.method()} ${request.url()}`);
    }
  });
  return seen;
}

test.beforeEach(async () => {
  // A previous run's connections and private servers would change what the picker offers — the
  // assertions below read "nothing is added yet" off the catalog.
  await adminQuery(
    `delete from web.bundle where (name like $1 or name like 'linear%')
       and workspace_id in (select id from web.workspace)`,
    [`${CATALOG_NAME}%`],
  );
  // Only this suite's OWN private server: another suite's is connected to a bundle, and the
  // catalog row a connection points at is deliberately not deletable out from under it.
  await adminQuery(
    `delete from web.bundle where id in (
       select bm.bundle_id from web.bundle_mcp bm
       join web.mcp_server ms on ms.id = bm.server_id
       where ms.registry_name = $1)`,
    [SERVER_NAME],
  );
  await adminQuery(
    `delete from web.mcp_server where workspace_id is not null and registry_name = $1`,
    [SERVER_NAME],
  );
});

test("choosing a server opens its dialog with no request, and adding it connects", async ({
  page,
}) => {
  await gotoSettled(page, "/");
  // Scoped to the content pane: the rail's `+` carries the same name, which is the point —
  // one act, one name, wherever it is offered.
  await page.getByRole("main").getByRole("link", { name: "Add an MCP server" }).click();
  await expect(page.getByRole("heading", { name: "Add an MCP server" })).toBeVisible();

  // The page RESTS on the catalog: no typing needed to see what is on offer.
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

  // THE REGRESSION THIS REWORK EXISTS TO PREVENT: choosing a server is a client-side act. The
  // dialog opens on the click, and nothing was asked of the server to open it.
  const traffic = watchTraffic(page);
  await options.first().click();
  const dialog = page.getByTestId("mcp-pick-dialog");
  await expect(dialog).toBeVisible();
  expect(traffic).toEqual([]);
  // The grid is still there and still live — not replaced, not disabled.
  await expect(options.first()).toBeEnabled();

  // It shows what this workspace would be running, and the name it would take.
  await expect(dialog).toContainText("app.linear/linear");
  await expect(dialog).toContainText("streamable-http");
  await expect(dialog).toContainText("oauth");
  await expect(page.getByTestId("mcp-dialog-url")).toHaveText("https://mcp.linear.app/mcp");
  // The suggested catalog name is the product's, not the registry name's "mcp" tail.
  await expect(page.getByLabel("Add as")).toHaveValue("linear");
  // Opening it wrote nothing.
  expect(await adminQuery(`select 1 from web.bundle where name = 'linear'`)).toHaveLength(0);

  // Escape closes it with nothing changed — still no request, still nothing written.
  await page.keyboard.press("Escape");
  await expect(dialog).toHaveCount(0);
  expect(traffic).toEqual([]);
  expect(await adminQuery(`select 1 from web.bundle where name = 'linear'`)).toHaveLength(0);

  // The second click is the one that acts: the dialog's own button, and it lands on the new
  // server's page under /mcp.
  await options.first().click();
  await expect(dialog).toBeVisible();
  await page.getByTestId("mcp-connect").click();
  await page.waitForURL("**/mcp/linear");
  await expect(page.getByRole("heading", { name: "linear" })).toBeVisible();
  const rows = await adminQuery<{ kind: string }>(`select kind from web.bundle where name = $1`, [
    "linear",
  ]);
  expect(rows[0]?.kind).toBe("mcp");
  // The bundle NAMES the catalog row — that connection is the whole of what was written.
  expect(
    await adminQuery(
      `select 1 from web.bundle_mcp bm join web.bundle b on b.id = bm.bundle_id
       where b.name = 'linear'`,
    ),
  ).toHaveLength(1);
});

/**
 * A REFUSAL LANDS WHERE THE ACT WAS. One connection per server per workspace is the database's
 * rule, so the second attempt is the live refusal — and the answer has to arrive INSIDE the
 * dialog that asked, with the row still on screen, or the click would look like it did nothing.
 */
test("a refusal comes back into the dialog that asked for it", async ({ page }) => {
  const dialog = page.getByTestId("mcp-pick-dialog");
  await gotoSettled(page, "/mcp/new");
  await page.getByTestId("mcp-picker-search").fill("linear");
  await page.getByTestId("mcp-picker-option").first().click();
  await expect(dialog).toBeVisible();
  await page.getByTestId("mcp-connect").click();
  await page.waitForURL("**/mcp/linear");

  // A server this workspace already runs is not offered a second time — it links to itself.
  await gotoSettled(page, "/mcp/new");
  await page.getByTestId("mcp-picker-search").fill("linear");
  await expect(page.getByTestId("mcp-picker-option")).toHaveCount(0);
  const added = page.getByTestId("mcp-picker-added");
  await expect(added).toHaveCount(1);
  await expect(added).toContainText("added");
  await added.click();
  await page.waitForURL("**/mcp/linear");
  // And no second row was minted for it.
  expect(await adminQuery(`select 1 from web.bundle where name like 'linear%'`)).toHaveLength(1);
});

test("write down a server the catalog does not carry, preview it, add it", async ({ page }) => {
  // The dashboard is where the affordance lives — reached by clicking, not by typing a URL.
  await gotoSettled(page, "/");
  await page.getByRole("main").getByRole("link", { name: "Add an MCP server" }).click();
  await expect(page.getByRole("heading", { name: "Add an MCP server" })).toBeVisible();

  // The typed sources sit behind the disclosure; the catalog is what the page opens on.
  await page.getByText("A server that is not on this list", { exact: true }).click();
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

  await page.getByTestId("mcp-create").click();

  // It lands on the new server's OWN page — under /mcp, never /skills — and the row is the
  // workspace's own server, exported nowhere.
  await page.waitForURL(`**/mcp/${CATALOG_NAME}`);
  await expect(page.getByRole("heading", { name: CATALOG_NAME })).toBeVisible();
  await expect(page.getByTestId("mcp-server-panel")).toContainText(SERVER_NAME);
  const rows = await adminQuery<{ kind: string }>(`select kind from web.bundle where name = $1`, [
    CATALOG_NAME,
  ]);
  expect(rows[0]?.kind).toBe("mcp");
  expect(
    await adminQuery(
      `select 1 from web.mcp_server where registry_name = $1 and workspace_id is not null`,
      [SERVER_NAME],
    ),
  ).toHaveLength(1);

  // And the dashboard lists it under MCP servers, with its kind, like any other bundle.
  await gotoSettled(page, "/");
  await expect(
    page.getByRole("main").getByRole("link", { name: new RegExp(CATALOG_NAME) }),
  ).toContainText("mcp");
});

/**
 * ADDING IS NOT SHARING. The destination field rests on no channel, so an untouched form leaves
 * the server in the workspace and nowhere else — the server's own page says "In no channel." and
 * the placement table agrees. Choosing a channel is the separate, deliberate act, and it works
 * from the same field.
 */
test("an added server rests in no channel until a channel is chosen", async ({ page }) => {
  const placements = () =>
    adminQuery(
      `select 1 from web.channel_bundle cb join web.bundle b on b.id = cb.bundle_id
       where b.name = $1`,
      [CATALOG_NAME],
    );

  await gotoSettled(page, "/mcp/new");
  await page.getByText("A server that is not on this list", { exact: true }).click();
  await page.getByLabel("Where it comes from").selectOption("paste");
  await page.getByTestId("mcp-paste").fill(DOCUMENT);
  await page.getByRole("button", { name: "Preview" }).click();
  await expect(page.getByTestId("mcp-preview")).toBeVisible();

  // Untouched. Whatever the workspace's channels are, the resting answer is none of them.
  await expect(page.getByLabel("Share into")).toHaveValue("");
  await page.getByTestId("mcp-create").click();
  await page.waitForURL(`**/mcp/${CATALOG_NAME}`);

  // The landing page is plain — no withheld-placement note, because nothing was withheld — and
  // the delivery section says where it stands.
  await expect(page.getByTestId("placement-note")).toHaveCount(0);
  await expect(page.getByTestId("skill-channels")).toContainText("In no channel.");
  expect(await placements()).toHaveLength(0);

  // Now the deliberate act, on a catalog server, through the same field.
  await gotoSettled(page, "/mcp/new");
  await page.getByTestId("mcp-picker-search").fill("linear");
  await page.getByTestId("mcp-picker-option").first().click();
  await page.getByLabel("Share into").selectOption("everyone");
  await page.getByTestId("mcp-connect").click();
  await page.waitForURL("**/mcp/linear");

  await expect(page.getByTestId("skill-channels")).toContainText("everyone");
  expect(
    await adminQuery(
      `select 1 from web.channel_bundle cb join web.bundle b on b.id = cb.bundle_id
       where b.name = 'linear'`,
    ),
  ).toHaveLength(1);
});

/**
 * WHERE A SERVER LIVES. The founder's rule, asserted end to end: an MCP server is its own
 * section with its own way in and its own address — never a row under Skills. The two seeded
 * bundles differ ONLY in kind, so anything that mixes them shows up here.
 */
test("an MCP server has its own section, its own + , and its own address", async ({ page }) => {
  await ensureBundle({ id: "s_e2e_mcp_panel", name: "panel-server", kind: "mcp" });
  await ensureBundle({ id: "s_e2e_skill_panel", name: "panel-skill" });
  await gotoSettled(page, "/");

  // The rail carries a section per kind, and each lists only its own.
  const skills = page.locator('[data-sidebar="group"]', { hasText: "Skills" }).first();
  const servers = page.locator('[data-sidebar="group"]', { hasText: "MCP servers" }).first();
  await expect(servers.getByRole("link", { name: "panel-server" })).toBeVisible();
  await expect(servers.getByRole("link", { name: "panel-skill" })).toHaveCount(0);
  await expect(skills.getByRole("link", { name: "panel-skill" })).toBeVisible();
  await expect(skills.getByRole("link", { name: "panel-server" })).toHaveCount(0);

  // The Skills `+` opens the publish dialog (skills only); the MCP `+` is a link to the
  // add-a-server page, at its own Rails-shaped address.
  await expect(page.getByRole("button", { name: "Publish a skill from your agent" })).toBeVisible();
  await servers.getByRole("link", { name: "Add an MCP server" }).click();
  await page.waitForURL("**/mcp/new");
  await expect(page.getByRole("heading", { name: "Add an MCP server" })).toBeVisible();

  // The server's page is at its own address, and its trail reads as the MCP section.
  await gotoSettled(page, "/mcp/panel-server");
  await expect(page.getByRole("heading", { name: "panel-server" })).toBeVisible();
  await expect(page.getByRole("navigation", { name: "Breadcrumb" })).toContainText("MCP servers");

  // A member who addresses it the other way is sent to the canonical path — both directions.
  await gotoSettled(page, "/skills/panel-server");
  await page.waitForURL("**/mcp/panel-server");
  await gotoSettled(page, "/mcp/panel-skill");
  await page.waitForURL("**/skills/panel-skill");
});

/**
 * A CURATED DESTINATION TAKES A MEMBER'S PLACEMENT — and the page says so twice: on the way in,
 * as the destination's own label, and on the way out, on the server's page. The connection itself
 * still lands (existence is never curation-blocked); what is withheld is the REACH, and a page
 * that showed a plain success here would be promising an address nobody was given.
 */
test("a member adding into a curated channel is told the placement was withheld", async ({
  page,
}) => {
  await ensureSeatedUser(MEMBER, "member");
  await adminQuery(`update web.channel set mode = 'curated' where is_default`);
  try {
    await signIn(page, MEMBER);
    await gotoSettled(page, "/mcp/new");
    await page.getByTestId("mcp-picker-search").fill("linear");
    await page.getByTestId("mcp-picker-option").first().click();
    await expect(page.getByTestId("mcp-pick-dialog")).toBeVisible();

    // On the way IN: the field rests on no channel at all, and the destination that WOULD reach
    // people says what it will do with a member's placement, beside the button that would do it.
    const into = page.getByLabel("Share into");
    await expect(into).toHaveValue("");
    await expect(into).toContainText(
      "everyone (everyone here) — curated; placement needs a reviewer",
    );
    await into.selectOption("everyone");

    await page.getByTestId("mcp-connect").click();

    // On the way OUT: the server's own page carries the one line that says what did not happen.
    await page.waitForURL("**/mcp/linear?*");
    await expect(page.getByTestId("placement-note")).toContainText(
      "Published to the catalog — placing it into #everyone takes a reviewer or owner.",
    );
    // And the row really is in no channel — the note is not decoration.
    expect(
      await adminQuery(
        `select 1 from web.channel_bundle cb join web.bundle b on b.id = cb.bundle_id
         where b.name = 'linear'`,
      ),
    ).toHaveLength(0);
  } finally {
    await adminQuery(`update web.channel set mode = 'open' where is_default`);
  }
});

/**
 * WRITING A SERVER DOWN IS AN OWNER'S ACT. A member gets the catalog and nothing else: the
 * disclosure that opens the typed sources is not on their page at all.
 */
test("a member is offered the catalog, and no way to write a server down", async ({ page }) => {
  await ensureSeatedUser(MEMBER, "member");
  await signIn(page, MEMBER);
  await gotoSettled(page, "/mcp/new");
  await expect(page.getByTestId("mcp-picker")).toBeVisible();
  await expect(page.getByTestId("mcp-custom")).toHaveCount(0);
});

test("a document carrying a credential is refused, and nothing is added", async ({ page }) => {
  await gotoSettled(page, "/mcp/new");
  await page.getByText("A server that is not on this list", { exact: true }).click();
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
