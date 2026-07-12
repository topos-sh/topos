import { expect, test } from "@playwright/test";
import { Client } from "pg";
import { E2E_ADMIN_URL, E2E_PASSWORD } from "./env";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The channel surfaces: the index (list + counts), the detail (skills/members), and the two
 * owner existence-ceremonies (rename + delete, both step-up gated; delete also types the channel
 * name). Every assertion rides rows THIS spec seeds into the REAL directory (topos_e2e) — its OWN
 * workspace + owner, so it never collides with the roster/dashboard specs' seeds. auth.setup.ts
 * truncates the channel tables once at global setup; this spec's beforeAll seeds after, and the
 * suite runs serially so the mutating tests keep a deterministic order.
 *
 * SEED SHAPE (0015's constraints: canonical lowercase principals, catalog rows for placed skills):
 *  - w_channels, address channels-e2e; a confirmed OWNER (the signed-in identity) + one confirmed
 *    MEMBER, so `everyone` counts 2 structurally.
 *  - `everyone` (structural, via topos_ensure_everyone) + three plain channels: `reviews` (2 skill
 *    refs, 1 member — the index/detail/rename subject), `audits` (1 ref, 1 member — the delete
 *    subject), `logs` (1 ref, 1 member — the history + deletion-trail subject).
 *  - two catalog skills (`deploy`, `release`) with NO current pointer, so their skill pages render
 *    the honest "nothing published yet" (no vault scope needed for this workspace).
 */

const CHANNELS_WS = "w_channels";
const CHANNELS_ADDRESS = "channels-e2e";
const OWNER_EMAIL = "channels-owner@example.com";
const MEMBER_EMAIL = "channels-member@example.com";

test.describe.configure({ mode: "serial" });

async function admin<T = Record<string, unknown>>(
  sql: string,
  params: unknown[] = [],
): Promise<T[]> {
  const db = new Client({ connectionString: E2E_ADMIN_URL });
  await db.connect();
  try {
    const { rows } = await db.query(sql, params);
    return rows as T[];
  } finally {
    await db.end();
  }
}

test.beforeAll(async () => {
  const db = new Client({ connectionString: E2E_ADMIN_URL });
  await db.connect();
  try {
    // The guarded functions reference their tables unqualified; the superuser session's default
    // search_path does not include `plane`, so set it for this seeding connection.
    await db.query("SET search_path = plane, public");
    // Idempotent for a local re-run: clear only THIS workspace's rows, then seed fresh.
    // One statement per query: a parameterized (extended-protocol) query cannot carry
    // multiple commands.
    for (const table of [
      "channel_events",
      "channel_skills",
      "channel_members",
      "channels",
      "catalog",
      "workspace_member",
      "workspace_policy",
      "workspace",
    ]) {
      await db.query(`DELETE FROM plane.${table} WHERE workspace_id = $1`, [CHANNELS_WS]);
    }
    await db.query(
      `INSERT INTO plane.workspace (workspace_id, display_name, verified_domain_status, deployment_mode, created_at, name)
       VALUES ($1, 'Channels E2E', 'unverified', 'cloud', '2026-07-04T00:00:00Z', $2)`,
      [CHANNELS_WS, CHANNELS_ADDRESS],
    );
    await db.query(
      `INSERT INTO plane.workspace_policy (workspace_id, review_required) VALUES ($1, 0)`,
      [CHANNELS_WS],
    );
    // A confirmed OWNER (the signed-in actor) + a confirmed MEMBER — everyone counts the two.
    await db.query(
      `INSERT INTO plane.workspace_member (workspace_id, principal, role, status, added_at) VALUES
         ($1, $2, 'owner',  'confirmed', '2026-07-04T00:00:01Z'),
         ($1, $3, 'member', 'confirmed', '2026-07-04T00:00:02Z')`,
      [CHANNELS_WS, OWNER_EMAIL, MEMBER_EMAIL],
    );
    // Two catalog skills (no current pointer — the skill pages render "nothing published yet").
    await db.query(
      `INSERT INTO plane.catalog (workspace_id, skill_id, name, status, created_at) VALUES
         ($1, 's_deploy',  'deploy',  'active', '2026-07-04T00:00:03Z'),
         ($1, 's_release', 'release', 'active', '2026-07-04T00:00:04Z')`,
      [CHANNELS_WS],
    );
    // The structural everyone channel, created the way genesis does.
    await db.query(`SELECT plane.topos_ensure_everyone($1, '2026-07-04T00:00:05Z')`, [CHANNELS_WS]);
    // Three plain channels + their references + memberships.
    await db.query(
      `INSERT INTO plane.channels (workspace_id, channel_id, name, mode, builtin, created_by, created_at) VALUES
         ($1, 'reviews', 'reviews', 'open', 0, $2, '2026-07-04T00:00:06Z'),
         ($1, 'audits',  'audits',  'open', 0, $2, '2026-07-04T00:00:07Z'),
         ($1, 'logs',    'logs',    'open', 0, $2, '2026-07-04T00:00:08Z')`,
      [CHANNELS_WS, OWNER_EMAIL],
    );
    await db.query(
      `INSERT INTO plane.channel_skills (workspace_id, channel_id, skill_id, added_by, added_at) VALUES
         ($1, 'reviews', 's_deploy',  $2, '2026-07-04T00:00:09Z'),
         ($1, 'reviews', 's_release', $2, '2026-07-04T00:00:10Z'),
         ($1, 'audits',  's_deploy',  $2, '2026-07-04T00:00:11Z'),
         ($1, 'logs',    's_deploy',  $2, '2026-07-04T00:00:12Z')`,
      [CHANNELS_WS, OWNER_EMAIL],
    );
    await db.query(
      `INSERT INTO plane.channel_members (workspace_id, channel_id, principal, added_by, added_at) VALUES
         ($1, 'reviews', $2, NULL, '2026-07-04T00:00:13Z'),
         ($1, 'audits',  $2, NULL, '2026-07-04T00:00:14Z'),
         ($1, 'logs',    $2, NULL, '2026-07-04T00:00:15Z')`,
      [CHANNELS_WS, MEMBER_EMAIL],
    );
  } finally {
    await db.end();
  }
});

test("the index lists everyone and a seeded channel with their counts", async ({ page }) => {
  await signIn(page, OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${CHANNELS_WS}/channels`);

  // `everyone` is structural — its member count is the confirmed roster (2), marked as such.
  const everyone = page.getByRole("listitem").filter({ hasText: "everyone" });
  await expect(everyone).toBeVisible();
  await expect(everyone.getByText("every confirmed member, structural")).toBeVisible();
  await expect(everyone.getByText("2 members")).toBeVisible();

  // A plain channel counts its own rows: two skill references, one member.
  const reviews = page.getByRole("listitem").filter({ hasText: "reviews" });
  await expect(reviews.getByText("2 skills")).toBeVisible();
  await expect(reviews.getByText("1 member", { exact: true })).toBeVisible();
});

test("the detail shows the channel's skills and members", async ({ page }) => {
  await signIn(page, OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${CHANNELS_WS}/channels/reviews`);

  await expect(page.getByRole("heading", { name: "reviews" })).toBeVisible();
  // The referenced skills link to their skill pages (by catalog name).
  await expect(page.getByRole("link", { name: "deploy" })).toBeVisible();
  await expect(page.getByRole("link", { name: "release" })).toBeVisible();
  // The person-scoped membership.
  await expect(page.getByText(MEMBER_EMAIL)).toBeVisible();
});

test("rename is step-up gated: a wrong password fails and leaves the channel unchanged; the right password lands the new URL", async ({
  page,
}) => {
  await signIn(page, OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${CHANNELS_WS}/channels/reviews`);

  // Wrong password: the ceremony refuses, nothing is written, the page stays on #reviews.
  await page.getByLabel("New name").fill("reviews-renamed");
  // Two ceremonies on this page share the password label; target the rename form's own field.
  await page.locator("#rename-reviews-password").fill("wrong-password-9999");
  await page.getByRole("button", { name: "Rename channel" }).click();
  await expect(page.getByRole("alert")).toContainText("Password check failed");
  await expect(page.getByRole("heading", { name: "reviews" })).toBeVisible();
  const stillReviews = await admin<{ name: string }>(
    `SELECT name FROM plane.channels WHERE workspace_id = $1 AND channel_id = 'reviews'`,
    [CHANNELS_WS],
  );
  expect(stillReviews[0]?.name).toBe("reviews");

  // Right password: the rename lands and redirects to the new channel URL.
  await page.getByLabel("New name").fill("reviews-renamed");
  await page.locator("#rename-reviews-password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Rename channel" }).click();
  await page.waitForURL((u) => u.pathname.endsWith(`/channels/reviews-renamed`));
  await expect(page.getByRole("heading", { name: "reviews-renamed" })).toBeVisible();
  // The immutable channel_id never moved; only the display name did.
  const renamed = await admin<{ name: string }>(
    `SELECT name FROM plane.channels WHERE workspace_id = $1 AND channel_id = 'reviews'`,
    [CHANNELS_WS],
  );
  expect(renamed[0]?.name).toBe("reviews-renamed");
});

test("delete types the channel name and drops it from the index; the referenced skill's page survives", async ({
  page,
}) => {
  await signIn(page, OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${CHANNELS_WS}/channels/audits`);

  // The destructive ceremony: type the exact name + confirm with the password.
  await page.getByPlaceholder("audits").fill("audits");
  await page.locator("#delete-audits-password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Delete channel" }).click();

  // Redirected to the index, and the channel is gone from it.
  await page.waitForURL((u) => u.pathname.endsWith(`/channels`));
  await expect(page.getByRole("listitem").filter({ hasText: "audits" })).toHaveCount(0);

  // The skill the channel referenced still exists — a deletion is an upstream withdrawal, not a
  // catalog change. Its page renders (not the house 404).
  await gotoSettled(page, `/workspaces/${CHANNELS_WS}/skills/deploy`);
  await expect(page.getByRole("heading", { name: "deploy" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Not found" })).toHaveCount(0);
});

test("history shows the trigger-emitted events; a delete's trail survives in the audit though the page 404s", async ({
  page,
}) => {
  await signIn(page, OWNER_EMAIL);

  // Before deletion: the history page renders the seeded channel's trigger-emitted trail.
  await gotoSettled(page, `/workspaces/${CHANNELS_WS}/channels/logs/history`);
  await expect(page.getByText("Channel created")).toBeVisible();
  await expect(page.getByText("Skill added")).toBeVisible();
  await expect(page.getByText("Member joined")).toBeVisible();

  // Delete the channel through the UI.
  await gotoSettled(page, `/workspaces/${CHANNELS_WS}/channels/logs`);
  await page.getByPlaceholder("logs").fill("logs");
  await page.locator("#delete-logs-password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Delete channel" }).click();
  await page.waitForURL((u) => u.pathname.endsWith(`/channels`));

  // The channel_events rows SURVIVE the deletion and carry the deletion trail...
  const events = await admin<{ event: string }>(
    `SELECT event FROM plane.channel_events WHERE workspace_id = $1 AND channel_id = 'logs'`,
    [CHANNELS_WS],
  );
  const kinds = events.map((e) => e.event);
  expect(kinds).toContain("channel_deleted");
  expect(kinds).toContain("skill_removed");
  expect(kinds).toContain("member_left");

  // ...but the history page 404s once the channel row is gone (it resolves by name).
  await gotoSettled(page, `/workspaces/${CHANNELS_WS}/channels/logs/history`);
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
});
