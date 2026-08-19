import { expect, test } from "@playwright/test";
import { mcpRevisionId } from "../helpers/mcp-ids";
import { adminQuery, theWorkspace } from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * A SERVER'S OWN PAGE — the face a `kind: 'mcp'` bundle carries instead of a file tree.
 *
 * What it has to answer, in order: what the server IS (its address, how a person signs in,
 * whether anything answered when this plane last asked), what THIS WORKSPACE receives (the
 * current version, or a pin — and when a pinned version has been withdrawn, that too), and the
 * version history behind it. For a server the workspace wrote down itself, an owner edits the
 * document here, and every save is a new version rather than a rewrite of one already delivered.
 */

const CATALOG = { serverId: "e2e_mcps_catalog", bundleId: "e2e_b_catalog", name: "e2e-catalog" };
const PINNED = { serverId: "e2e_mcps_pinned", bundleId: "e2e_b_pinned", name: "e2e-pinned" };
const OWN = { serverId: "e2e_mcps_own", bundleId: "e2e_b_own", name: "e2e-own" };

function document(name: string, version: string, url: string): string {
  return JSON.stringify({
    name,
    description: `Everything ${name} knows.`,
    version,
    remotes: [{ type: "streamable-http", url }],
  });
}

async function seedServer(
  id: string,
  registryName: string,
  opts: { workspaceId?: string | null; authMode?: string | null; authNote?: string | null } = {},
): Promise<void> {
  await adminQuery(
    `insert into web.mcp_server (id, workspace_id, registry_name, display_name, description,
                                 auth_mode, auth_note, status)
     values ($1, $2, $3, $3, 'A server for the suite.', $4, $5, 'active')
     on conflict (id) do nothing`,
    [id, opts.workspaceId ?? null, registryName, opts.authMode ?? "none", opts.authNote ?? null],
  );
}

async function seedRevision(
  serverId: string,
  id: string,
  seq: number,
  version: string,
  opts: { current?: boolean; status?: "published" | "revoked"; url?: string } = {},
): Promise<void> {
  const registryName = (
    await adminQuery<{ registry_name: string }>(
      `select registry_name from web.mcp_server where id = $1`,
      [serverId],
    )
  )[0]?.registry_name as string;
  const url = opts.url ?? "https://acme.example/mcp";
  await adminQuery(
    `insert into web.mcp_server_revision
       (id, server_id, seq, status, upstream_version, document, transport, url, source,
        published_at, published_by, revoked_at)
     values ($1, $2, $3, $4, $5, $6::jsonb, 'streamable-http', $7, 'seed',
             now(), 'Staff', case when $4 = 'revoked' then now() end)
     on conflict (id) do nothing`,
    [
      id,
      serverId,
      seq,
      opts.status ?? "published",
      version,
      document(registryName, version, url),
      url,
    ],
  );
  if (opts.current === true) {
    await adminQuery(`update web.mcp_server set current_revision_id = $2 where id = $1`, [
      serverId,
      id,
    ]);
  }
}

async function connect(bundleId: string, name: string, serverId: string, pinned?: string) {
  const ws = await theWorkspace();
  await adminQuery(
    `insert into web.bundle (id, workspace_id, name, kind) values ($1, $2, $3, 'mcp')
     on conflict (id) do update set name = excluded.name, status = 'active'`,
    [bundleId, ws.id, name],
  );
  await adminQuery(
    `insert into web.bundle_mcp (bundle_id, workspace_id, server_id, pinned_revision_id)
     values ($1, $2, $3, $4) on conflict (bundle_id) do nothing`,
    [bundleId, ws.id, serverId, pinned ?? null],
  );
}

test.beforeAll(async () => {
  // A previous run's rows would change what the history shows, and this suite asserts exact
  // counts — so the three fixtures are taken down (connections first: a catalog row a workspace
  // is connected to is deliberately not deletable out from under it) and seeded fresh.
  await adminQuery(`delete from web.bundle where id = any($1::text[])`, [
    [CATALOG.bundleId, PINNED.bundleId, OWN.bundleId],
  ]);
  await adminQuery(`delete from web.mcp_server where id = any($1::text[])`, [
    [CATALOG.serverId, PINNED.serverId, OWN.serverId],
  ]);

  // A catalog server on its current version, with a sign-in a person has to arrange.
  await seedServer("e2e_mcps_catalog", "io.github.e2e/catalog", {
    authMode: "manual",
    authNote: "Mint a token in the e2e console first.",
  });
  await seedRevision(CATALOG.serverId, mcpRevisionId("e2e_cat_1"), 1, "1.0.0");
  await seedRevision(CATALOG.serverId, mcpRevisionId("e2e_cat_2"), 2, "2.0.0", {
    current: true,
    url: "https://catalog.e2e.example/mcp",
  });
  await connect(CATALOG.bundleId, CATALOG.name, CATALOG.serverId);

  // The same shape, PINNED to a version that has since been withdrawn.
  await seedServer("e2e_mcps_pinned", "io.github.e2e/pinned", { authMode: "oauth" });
  await seedRevision(PINNED.serverId, mcpRevisionId("e2e_pin_1"), 1, "1.0.0", {
    status: "revoked",
  });
  await seedRevision(PINNED.serverId, mcpRevisionId("e2e_pin_2"), 2, "2.0.0", { current: true });
  await connect(PINNED.bundleId, PINNED.name, PINNED.serverId, mcpRevisionId("e2e_pin_1"));

  // The workspace's OWN server — the one an owner may edit.
  const ws = await theWorkspace();
  await seedServer("e2e_mcps_own", "io.github.e2e/own", { workspaceId: ws.id, authMode: null });
  await seedRevision(OWN.serverId, mcpRevisionId("e2e_own_1"), 1, "1.0.0", {
    current: true,
    url: "https://own.e2e.example/mcp",
  });
  await connect(OWN.bundleId, OWN.name, OWN.serverId);
});

test("says what the server is, what this workspace receives, and which versions exist", async ({
  page,
}) => {
  await gotoSettled(page, `/mcp/${CATALOG.name}`);
  await expect(page.getByRole("heading", { name: CATALOG.name })).toBeVisible();

  const panel = page.getByTestId("mcp-server-panel");
  await expect(panel).toContainText("io.github.e2e/catalog");
  await expect(panel).toContainText("streamable-http");
  await expect(panel).toContainText("manual setup");
  await expect(page.getByTestId("mcp-server-url")).toHaveText("https://catalog.e2e.example/mcp");
  // The errand, said where the server is read rather than left for the agent to discover.
  await expect(page.getByTestId("mcp-server-auth-note")).toContainText("Mint a token");
  // Nobody has asked this address anything — a fact about this plane, not about the server.
  await expect(page.getByTestId("mcp-probe")).toHaveText("not checked yet");

  // What the workspace receives, and the history behind it.
  await expect(page.getByTestId("mcp-connection")).toContainText(
    "Following the current version, 2.0.0.",
  );
  const revisions = page.getByTestId("mcp-revisions");
  await expect(revisions).toContainText("2.0.0");
  await expect(revisions).toContainText("1.0.0");
  await expect(revisions).toContainText("delivered here");

  // A server is not a file tree, and its section switcher offers none of the file pages.
  const tabs = page.getByRole("navigation", { name: "Skill sections" });
  await expect(tabs.getByRole("link", { name: "Current" })).toBeVisible();
  await expect(tabs.getByRole("link", { name: "History" })).toHaveCount(0);
  await expect(tabs.getByRole("link", { name: "Proposals" })).toHaveCount(0);
});

test("a pin is honored, and a withdrawn pin says so", async ({ page }) => {
  await gotoSettled(page, `/mcp/${PINNED.name}`);
  await expect(page.getByTestId("mcp-connection")).toContainText("Pinned to 1.0.0.");
  await expect(page.getByTestId("mcp-revoked")).toContainText("withdrawn after it was published");
});

test("an owner's save of their own server is a NEW version, not a rewrite", async ({ page }) => {
  await gotoSettled(page, `/mcp/${OWN.name}`);
  const editor = page.getByTestId("mcp-edit-document");
  await expect(editor).toBeVisible();

  await editor.fill(
    JSON.stringify(
      {
        name: "io.github.e2e/own",
        description: "The one this workspace wrote down, corrected.",
        version: "1.1.0",
        remotes: [{ type: "streamable-http", url: "https://own.e2e.example/v2" }],
      },
      null,
      2,
    ),
  );
  await page.getByTestId("mcp-edit-save").click();

  // The page now reports the new version, and BOTH are on the list — the delivered one moved.
  await expect(page.getByTestId("mcp-connection")).toContainText(
    "Following the current version, 1.1.0.",
  );
  await expect(page.getByTestId("mcp-server-url")).toHaveText("https://own.e2e.example/v2");
  const revisions = page.getByTestId("mcp-revisions");
  await expect(revisions).toContainText("1.1.0");
  await expect(revisions).toContainText("1.0.0");
  const rows = await adminQuery<{ n: string }>(
    `select count(*)::int as n from web.mcp_server_revision where server_id = $1`,
    [OWN.serverId],
  );
  expect(Number(rows[0]?.n)).toBe(2);
});

/** A CATALOG server is nobody's to edit here: the form is not on the page at all. */
test("a catalog server carries no edit form", async ({ page }) => {
  await gotoSettled(page, `/mcp/${CATALOG.name}`);
  await expect(page.getByTestId("mcp-edit-document")).toHaveCount(0);
});
