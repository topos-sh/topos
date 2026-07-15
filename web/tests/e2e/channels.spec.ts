import { expect, test } from "@playwright/test";
import { E2E_PASSWORD, MEMBER_EMAIL } from "./env";
import { adminQuery, ensureBundle, theWorkspace } from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * The channel surfaces: the index (everyone first + counts), member-level create, the detail
 * (skill references, members, the default channel's self-service stance over `channel_optout`),
 * the two owner existence-ceremonies (rename + delete — step-up; delete also types the channel
 * name), and the id-keyed audit history that outlives a rename and survives a delete in the
 * ledger even though the page 404s by name.
 *
 * All rows live in the app's own `web` schema now. The suite's default identity is the claimed
 * OWNER; serial so the mutating tests keep a deterministic order.
 */

const GUILD = "e2e-guild";
const RENAMED = "e2e-guild-renamed";
const DOOMED = "e2e-doomed";
const SKILL_ID = "s_e2e_chan";
const SKILL_NAME = "chan-notes";

test.describe.configure({ mode: "serial" });

test.beforeAll(async () => {
  // Idempotent for a reused local database: this file's channels start absent.
  await adminQuery(`delete from web.channel where name = any($1::text[])`, [
    [GUILD, RENAMED, DOOMED],
  ]);
  await adminQuery(
    `delete from web.channel_optout o using web."user" u
     where u.id = o.user_id and u.email = $1`,
    [MEMBER_EMAIL],
  );
  await ensureBundle({ id: SKILL_ID, name: SKILL_NAME });
});

test("the index lists everyone first; a member creates a channel and lands on it", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/channels`);

  // `everyone` is implicit membership — the roster minus opt-outs, marked as such.
  const everyone = page.getByRole("listitem").filter({ hasText: "everyone" });
  await expect(everyone).toBeVisible();
  await expect(everyone.getByText("every member, minus opt-outs")).toBeVisible();

  // Member-level create (the same grade as the CLI's create-on-first-use placement).
  await page.getByLabel("Channel name").fill(GUILD);
  await page.getByRole("button", { name: "Create channel" }).click();
  await page.waitForURL(`**/channels/${GUILD}`);
  await expect(page.getByRole("heading", { name: GUILD })).toBeVisible();

  // A duplicate create is the honest name-taken refusal, never a 500.
  await gotoSettled(page, `/workspaces/${ws.id}/channels`);
  await page.getByLabel("Channel name").fill(GUILD);
  await page.getByRole("button", { name: "Create channel" }).click();
  await expect(page.getByRole("alert")).toContainText(`A channel named #${GUILD} already exists.`);
});

test("the detail lists the channel's skill references by catalog name", async ({ page }) => {
  const ws = await theWorkspace();
  // Curation arrangement: place the seeded bundle into the channel (a reference, not a copy).
  await adminQuery(
    `insert into web.channel_bundle (channel_id, workspace_id, bundle_id)
     select c.id, c.workspace_id, $2 from web.channel c
     where c.workspace_id = $1 and c.name = $3
     on conflict do nothing`,
    [ws.id, SKILL_ID, GUILD],
  );
  await gotoSettled(page, `/workspaces/${ws.id}/channels/${GUILD}`);
  await expect(page.getByRole("link", { name: SKILL_NAME })).toBeVisible();

  // The index row now counts the reference.
  await gotoSettled(page, `/workspaces/${ws.id}/channels`);
  await expect(
    page.getByRole("listitem").filter({ hasText: GUILD }).getByText("1 skill", { exact: true }),
  ).toBeVisible();
});

test("the default channel's stance is self-service: leave writes the opt-out, rejoin clears it", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/channels/everyone`);
  await expect(page.getByText("You're in.", { exact: false })).toBeVisible();

  // LEAVE — the member's own stance, deliberately step-up-less.
  await page.getByRole("button", { name: "Leave everyone" }).click();
  await expect(page.getByText("You've opted out", { exact: false })).toBeVisible();
  const optedOut = await adminQuery<{ n: string }>(
    `select count(*)::text as n from web.channel_optout o
     join web."user" u on u.id = o.user_id where u.email = $1`,
    [MEMBER_EMAIL],
  );
  expect(optedOut[0]?.n).toBe("1");

  // REJOIN — deletes the opt-out row; delivery resumes on the next update.
  await page.getByRole("button", { name: "Rejoin everyone" }).click();
  await expect(page.getByText("You're in.", { exact: false })).toBeVisible();
  const rejoined = await adminQuery<{ n: string }>(
    `select count(*)::text as n from web.channel_optout o
     join web."user" u on u.id = o.user_id where u.email = $1`,
    [MEMBER_EMAIL],
  );
  expect(rejoined[0]?.n).toBe("0");

  // The everyone channel offers NO existence ceremonies — structural, honestly stated.
  await expect(page.getByText("The everyone channel is structural")).toBeVisible();
  await expect(page.getByRole("button", { name: "Rename channel" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Delete channel" })).toHaveCount(0);
});

test("rename is step-up gated: a wrong password refuses; the right one lands the new URL, id unchanged", async ({
  page,
}) => {
  const ws = await theWorkspace();
  const before = await adminQuery<{ id: string }>(
    `select id from web.channel where workspace_id = $1 and name = $2`,
    [ws.id, GUILD],
  );
  const channelId = before[0]?.id as string;

  await gotoSettled(page, `/workspaces/${ws.id}/channels/${GUILD}`);
  await page.getByLabel("New name").fill(RENAMED);
  // Two ceremonies on this page carry password fields; target the rename form's own.
  await page.locator(`#rename-${GUILD}-password`).fill("wrong-password-9999");
  await page.getByRole("button", { name: "Rename channel" }).click();
  await expect(page.getByRole("alert")).toContainText("Password check failed");
  const unchanged = await adminQuery<{ name: string }>(
    `select name from web.channel where id = $1`,
    [channelId],
  );
  expect(unchanged[0]?.name).toBe(GUILD);

  await page.getByLabel("New name").fill(RENAMED);
  await page.locator(`#rename-${GUILD}-password`).fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Rename channel" }).click();
  await page.waitForURL((u) => u.pathname.endsWith(`/channels/${RENAMED}`));
  await expect(page.getByRole("heading", { name: RENAMED })).toBeVisible();
  // The immutable channel id never moved; only the name did — references and history survive.
  const renamed = await adminQuery<{ name: string }>(`select name from web.channel where id = $1`, [
    channelId,
  ]);
  expect(renamed[0]?.name).toBe(RENAMED);
});

test("delete types the channel name; the ledger keeps the trail though the page 404s by name", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/channels`);
  await page.getByLabel("Channel name").fill(DOOMED);
  await page.getByRole("button", { name: "Create channel" }).click();
  await page.waitForURL(`**/channels/${DOOMED}`);
  const created = await adminQuery<{ id: string }>(
    `select id from web.channel where workspace_id = $1 and name = $2`,
    [ws.id, DOOMED],
  );
  const channelId = created[0]?.id as string;

  // The UI create landed its audit row — the history page renders the id-keyed trail.
  await gotoSettled(page, `/workspaces/${ws.id}/channels/${DOOMED}/history`);
  await expect(page.getByText("Channel created")).toBeVisible();

  // The WRONG typed name (with the right password) is refused by the typed-name gate.
  await gotoSettled(page, `/workspaces/${ws.id}/channels/${DOOMED}`);
  await page.locator(`#delete-${DOOMED}-confirm`).fill("not-the-name");
  await page.locator(`#delete-${DOOMED}-password`).fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Delete channel" }).click();
  await expect(page.getByRole("alert")).toContainText(/Type the exact name/);

  // The EXACT name + the password land the delete; the index no longer lists it.
  await page.locator(`#delete-${DOOMED}-confirm`).fill(DOOMED);
  await page.locator(`#delete-${DOOMED}-password`).fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Delete channel" }).click();
  await page.waitForURL((u) => u.pathname.endsWith("/channels"));
  await expect(page.getByRole("listitem").filter({ hasText: DOOMED })).toHaveCount(0);

  // The append-only ledger keeps the deletion under the immutable id…
  const trail = await adminQuery<{ kind: string }>(
    `select kind from web.audit_event where subject = $1 order by id`,
    [channelId],
  );
  expect(trail.map((t) => t.kind)).toContain("channel_created");
  expect(trail.map((t) => t.kind)).toContain("channel_deleted");

  // …but history resolves by NAME, so the page is the uniform miss once the row is gone.
  await gotoSettled(page, `/workspaces/${ws.id}/channels/${DOOMED}/history`);
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
});
