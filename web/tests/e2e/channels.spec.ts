import { expect, test } from "@playwright/test";
import { E2E_PASSWORD, MEMBER_EMAIL } from "./env";
import { adminQuery, ensureBundle, ensureSeatedUser, theWorkspace } from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The channel surfaces, now split into section TABS (Skills · Members · History · Settings) exactly
 * like the skill view: the index (everyone first + counts), member-level create, the Skills FACE
 * (skill references), the Members tab (the default channel's self-service stance over
 * `channel_optout`; named channels' explicit rows), the id-keyed History tab that outlives a rename
 * and survives a delete in the ledger, and the Settings tab hosting the two owner
 * existence-ceremonies (rename + delete — step-up; delete also types the channel name) with a quiet
 * read-only note for non-owners.
 *
 * All rows live in the app's own `web` schema. The suite's default identity is the claimed OWNER;
 * serial so the mutating tests keep a deterministic order.
 */

const GUILD = "e2e-guild";
const RENAMED = "e2e-guild-renamed";
const DOOMED = "e2e-doomed";
const CURATED = "e2e-curate";
const SKILL_ID = "s_e2e_chan";
const SKILL_NAME = "chan-notes";
const SKILL2_ID = "s_e2e_chan2";
const SKILL2_NAME = "chan-guide";
const CHAN_MEMBER = "chan-member@example.com";

test.describe.configure({ mode: "serial" });

test.beforeAll(async () => {
  // Idempotent for a reused local database: this file's channels start absent.
  await adminQuery(`delete from web.channel where name = any($1::text[])`, [
    [GUILD, RENAMED, DOOMED, CURATED],
  ]);
  await adminQuery(
    `delete from web.channel_optout o using web."user" u
     where u.id = o.user_id and u.email = $1`,
    [MEMBER_EMAIL],
  );
  await ensureBundle({ id: SKILL_ID, name: SKILL_NAME });
  await ensureBundle({ id: SKILL2_ID, name: SKILL2_NAME });
});

test("the index lists everyone first; a member creates a channel and lands on it", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/channels`);

  // `everyone` is implicit membership — the roster minus opt-outs, marked as such. Scope to the
  // content region: the left panel now lists channels too, so an unscoped listitem would double-match.
  const everyone = page.getByRole("main").getByRole("listitem").filter({ hasText: "everyone" });
  await expect(everyone).toBeVisible();
  await expect(everyone.getByText("every member, minus opt-outs")).toBeVisible();

  // Member-level create on the relocated Rails-style form (the same grade as the CLI's
  // create-on-first-use placement); the sidebar's Channels `+ new` links here.
  await gotoSettled(page, `/channels/new`);
  await page.getByLabel("Channel name").fill(GUILD);
  await page.getByRole("button", { name: "Create channel" }).click();
  await page.waitForURL(`**/channels/${GUILD}`);
  await expect(page.getByRole("heading", { name: GUILD })).toBeVisible();

  // A duplicate create is the honest name-taken refusal, never a 500.
  await gotoSettled(page, `/channels/new`);
  await page.getByLabel("Channel name").fill(GUILD);
  await page.getByRole("button", { name: "Create channel" }).click();
  await expect(page.getByRole("alert")).toContainText(`A channel named #${GUILD} already exists.`);
});

test("the Skills face lists the channel's skill references by catalog name", async ({ page }) => {
  const ws = await theWorkspace();
  // Curation arrangement: place the seeded bundle into the channel (a reference, not a copy).
  await adminQuery(
    `insert into web.channel_bundle (channel_id, workspace_id, bundle_id)
     select c.id, c.workspace_id, $2 from web.channel c
     where c.workspace_id = $1 and c.name = $3
     on conflict do nothing`,
    [ws.id, SKILL_ID, GUILD],
  );
  await gotoSettled(page, `/channels/${GUILD}`);
  await expect(page.getByRole("main").getByRole("link", { name: SKILL_NAME })).toBeVisible();

  // The index row now counts the reference (scoped to the content region — the left panel lists
  // channels too).
  await gotoSettled(page, `/channels`);
  await expect(
    page
      .getByRole("main")
      .getByRole("listitem")
      .filter({ hasText: GUILD })
      .getByText("1 skill", { exact: true }),
  ).toBeVisible();
});

test("the channel face shows the four section tabs, Skills active; each tab navigates", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/channels/${GUILD}`);

  // The face IS the Skills tab — pressed, the other three quiet. Scope to the channel nav: the left
  // panel and the workspace nav also carry Skills/Settings links.
  const tabs = () => page.getByRole("navigation", { name: "Channel sections" });
  await expect(tabs().getByRole("link", { name: "Skills" })).toHaveAttribute(
    "aria-current",
    "page",
  );
  await expect(tabs().getByRole("link", { name: "Members" })).toBeVisible();
  await expect(tabs().getByRole("link", { name: "History" })).toBeVisible();
  await expect(tabs().getByRole("link", { name: "Settings" })).toBeVisible();

  // Members tab → the members section.
  await tabs().getByRole("link", { name: "Members" }).click();
  await page.waitForURL(`**/channels/${GUILD}/members`);
  await expect(page.getByRole("heading", { name: "Members" })).toBeVisible();

  // History tab → the id-keyed audit trail (the UI create landed a row).
  await tabs().getByRole("link", { name: "History" }).click();
  await page.waitForURL(`**/channels/${GUILD}/history`);
  await expect(page.getByText("Channel created")).toBeVisible();

  // Settings tab → the owner controls (this suite's identity is the owner).
  await tabs().getByRole("link", { name: "Settings" }).click();
  await page.waitForURL(`**/channels/${GUILD}/settings`);
  await expect(page.getByRole("button", { name: "Rename channel" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Delete channel" })).toBeVisible();

  // Skills tab → back to the face.
  await tabs().getByRole("link", { name: "Skills" }).click();
  await page.waitForURL((u) => u.pathname.endsWith(`/channels/${GUILD}`));
});

test("the default channel's stance is self-service on the Members tab; Settings states it's structural", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/channels/everyone/members`);
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

  // The everyone channel offers NO existence ceremonies — its Settings tab states it's structural.
  await gotoSettled(page, `/channels/everyone/settings`);
  await expect(page.getByText("The everyone channel is structural")).toBeVisible();
  await expect(page.getByRole("button", { name: "Rename channel" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Delete channel" })).toHaveCount(0);
});

test("rename on the Settings tab is step-up gated: a wrong password refuses; the right one lands the new URL, id unchanged", async ({
  page,
}) => {
  const ws = await theWorkspace();
  const before = await adminQuery<{ id: string }>(
    `select id from web.channel where workspace_id = $1 and name = $2`,
    [ws.id, GUILD],
  );
  const channelId = before[0]?.id as string;

  await gotoSettled(page, `/channels/${GUILD}/settings`);
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
  // A landed rename redirects to the RENAMED channel's own settings tab.
  await page.waitForURL((u) => u.pathname.endsWith(`/channels/${RENAMED}/settings`));
  await expect(page.getByRole("heading", { name: RENAMED })).toBeVisible();
  // The immutable channel id never moved; only the name did — references and history survive.
  const renamed = await adminQuery<{ name: string }>(`select name from web.channel where id = $1`, [
    channelId,
  ]);
  expect(renamed[0]?.name).toBe(RENAMED);
});

test("delete on the Settings tab types the channel name; the ledger keeps the trail though the page 404s by name", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/channels/new`);
  await page.getByLabel("Channel name").fill(DOOMED);
  await page.getByRole("button", { name: "Create channel" }).click();
  await page.waitForURL(`**/channels/${DOOMED}`);
  const created = await adminQuery<{ id: string }>(
    `select id from web.channel where workspace_id = $1 and name = $2`,
    [ws.id, DOOMED],
  );
  const channelId = created[0]?.id as string;

  // The UI create landed its audit row — the History tab renders the id-keyed trail.
  await gotoSettled(page, `/channels/${DOOMED}/history`);
  await expect(page.getByText("Channel created")).toBeVisible();

  // The WRONG typed name (with the right password) is refused by the typed-name gate.
  await gotoSettled(page, `/channels/${DOOMED}/settings`);
  await page.locator(`#delete-${DOOMED}-confirm`).fill("not-the-name");
  await page.locator(`#delete-${DOOMED}-password`).fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Delete channel" }).click();
  await expect(page.getByRole("alert")).toContainText(/Type the exact name/);

  // The EXACT name + the password land the delete; the index no longer lists it.
  await page.locator(`#delete-${DOOMED}-confirm`).fill(DOOMED);
  await page.locator(`#delete-${DOOMED}-password`).fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Delete channel" }).click();
  await page.waitForURL((u) => u.pathname.endsWith("/channels"));
  await expect(
    page.getByRole("main").getByRole("listitem").filter({ hasText: DOOMED }),
  ).toHaveCount(0);

  // The append-only ledger keeps the deletion under the immutable id…
  const trail = await adminQuery<{ kind: string }>(
    `select kind from web.audit_event where subject = $1 order by id`,
    [channelId],
  );
  expect(trail.map((t) => t.kind)).toContain("channel_created");
  expect(trail.map((t) => t.kind)).toContain("channel_deleted");

  // …but history resolves by NAME, so the page is the uniform miss once the row is gone.
  await gotoSettled(page, `/channels/${DOOMED}/history`);
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
});

test("a non-owner reads the Settings tab as a read-only note; the owner forms don't render", async ({
  page,
}) => {
  await theWorkspace();
  // A seated MEMBER (not the owner). signIn overrides this test's session with that identity.
  await ensureSeatedUser(CHAN_MEMBER, "member");
  await signIn(page, CHAN_MEMBER);

  await gotoSettled(page, `/channels/everyone/settings`);
  // The page is member-visible; only the controls are owner-gated — an honest read-only note.
  await expect(page.getByText(/Only workspace owners can rename or delete/)).toBeVisible();
  await expect(page.getByRole("button", { name: "Rename channel" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Delete channel" })).toHaveCount(0);
  // The tabs still render for a member — the Settings tab reads pressed.
  await expect(
    page
      .getByRole("navigation", { name: "Channel sections" })
      .getByRole("link", { name: "Settings" }),
  ).toHaveAttribute("aria-current", "page");
});

test("the owner adds a skill via the Skills-face picker; the row links out and the picker drops it", async ({
  page,
}) => {
  await theWorkspace();
  // A fresh channel with no references — the picker offers the whole active catalog. Created via
  // the member-level form, landing on the channel FACE (the Skills tab).
  await gotoSettled(page, `/channels/new`);
  await page.getByLabel("Channel name").fill(CURATED);
  await page.getByRole("button", { name: "Create channel" }).click();
  await page.waitForURL(`**/channels/${CURATED}`);
  await expect(page.getByText("This channel references no skills yet.")).toBeVisible();

  // Add the seeded, not-yet-referenced skill through the picker (its value is the immutable id).
  const picker = page.getByRole("main").getByLabel("Add a skill");
  await picker.selectOption(SKILL2_ID);
  await page.getByRole("main").getByRole("button", { name: "Add", exact: true }).click();

  // The reference row appears as a link to the skill face…
  await expect(page.getByRole("main").getByRole("link", { name: SKILL2_NAME })).toBeVisible();
  // …and the revalidated picker no longer offers what's now placed.
  await expect(picker.getByRole("option", { name: SKILL2_NAME })).toHaveCount(0);
});

test("the owner removes the skill via the row control; the empty state returns", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/channels/${CURATED}`);
  // The row's own quiet Remove (its small fetcher form) — scope to the row so the click is exact.
  const row = page.getByRole("main").getByRole("listitem").filter({ hasText: SKILL2_NAME });
  await row.getByRole("button", { name: "Remove" }).click();

  // The last reference gone, the empty state returns and the row's link is no longer present.
  await expect(page.getByText("This channel references no skills yet.")).toBeVisible();
  await expect(page.getByRole("main").getByRole("link", { name: SKILL2_NAME })).toHaveCount(0);
});

test("the channel History tab records the add and the remove", async ({ page }) => {
  await theWorkspace();
  await gotoSettled(page, `/channels/${CURATED}/history`);
  // The DAL landed a skill_added then a skill_removed audit row under the channel's immutable id;
  // the History tab already labels both.
  await expect(page.getByText("Skill added")).toBeVisible();
  await expect(page.getByText("Skill removed")).toBeVisible();
});

test("a non-owner member on a CURATED channel sees no add/remove controls, only the quiet note", async ({
  page,
}) => {
  const ws = await theWorkspace();
  // Flip the channel to curated — the gate now demands reviewer-or-owner (arrangement, not subject).
  await adminQuery(
    `update web.channel set mode = 'curated' where workspace_id = $1 and name = $2`,
    [ws.id, CURATED],
  );
  // A seated MEMBER (not the owner); signIn overrides this test's session with that identity.
  await ensureSeatedUser(CHAN_MEMBER, "member");
  await signIn(page, CHAN_MEMBER);

  await gotoSettled(page, `/channels/${CURATED}`);
  // The Skills face is member-visible; only the curation controls are gated — an honest note.
  await expect(page.getByText(/Reviewers and owners manage/)).toBeVisible();
  await expect(page.getByRole("main").getByLabel("Add a skill")).toHaveCount(0);
  await expect(
    page.getByRole("main").getByRole("button", { name: "Add", exact: true }),
  ).toHaveCount(0);
  await expect(page.getByRole("main").getByRole("button", { name: "Remove" })).toHaveCount(0);
});
