import { expect, test } from "@playwright/test";
import { adminQuery, ensureSeatedUser, mintSession } from "./seed";
import { signIn } from "./sign-in";

/**
 * The account-level session list (/account/sessions). A session belongs to ONE person: the
 * page lists the signed-in person's OWN rows — nobody else's, whatever workspace they share —
 * and offers a self "End session" (a plain one-click act: ending your own session is the
 * escape hatch, not a ceremony over someone else's access). Sessions are DELETED, never
 * tombstoned — an ended one simply disappears; `topos login` mints a fresh one.
 *
 * A DEDICATED identity owns this spec's rows, with a DECOY session under a second user as the
 * negative control: the list keys on the person, never on "any session in a workspace I
 * belong to".
 */

const OWNER_EMAIL = "sessions-owner@example.com";
const DECOY_EMAIL = "sessions-decoy@example.com";
const SESS_A = "sn_e2e_yours_alpha";
const SESS_B = "sn_e2e_yours_beta";
const DECOY_SESS = "sn_e2e_yours_decoy";

test.describe.configure({ mode: "serial" });
test.use({ storageState: { cookies: [], origins: [] } });

test.beforeEach(async () => {
  const owner = await ensureSeatedUser(OWNER_EMAIL, "member");
  const decoy = await ensureSeatedUser(DECOY_EMAIL, "member");
  // A known two-session state per test (retry-safe): rows are deleted, so re-mint fresh.
  await adminQuery(`delete from web.cli_session where id = any($1::text[])`, [
    [SESS_A, SESS_B, DECOY_SESS],
  ]);
  await mintSession(owner.userId, SESS_A, "alpha-macbook", `cred-${SESS_A}`);
  await mintSession(owner.userId, SESS_B, "beta-desktop", `cred-${SESS_B}`);
  await mintSession(decoy.userId, DECOY_SESS, "decoy-machine", `cred-${DECOY_SESS}`);
  // One session has phoned home; the other never has (the honest "never seen" line).
  await adminQuery(
    `update web.cli_session set last_seen_at = now() - interval '1 hour' where id = $1`,
    [SESS_A],
  );
});

async function sessionExists(sessionId: string): Promise<boolean> {
  const rows = await adminQuery<{ n: string }>(
    `select count(*)::text as n from web.cli_session where id = $1`,
    [sessionId],
  );
  return rows[0]?.n !== "0";
}

test("lists exactly the person's own sessions; another user's session never renders", async ({
  page,
}) => {
  await signIn(page, OWNER_EMAIL);
  await page.goto("/account/sessions");
  await expect(page.getByRole("heading", { name: "Your sessions" })).toBeVisible();

  // Both of the person's own sessions render, with their ids and liveness lines.
  await expect(page.getByText("alpha-macbook")).toBeVisible();
  await expect(page.getByText("beta-desktop")).toBeVisible();
  await expect(page.getByText(SESS_A)).toBeVisible();
  await expect(page.getByText("never seen").first()).toBeVisible();

  // The decoy — another person's session in the SAME workspace — is filtered out entirely.
  await expect(page.getByText("decoy-machine")).toHaveCount(0);
  await expect(page.getByText(DECOY_SESS)).toHaveCount(0);

  // Exactly two sessions ⇒ exactly two End-session buttons.
  await expect(page.getByRole("button", { name: "End session" })).toHaveCount(2);

  // Off-workspace, the left panel keeps its workspace sections (the last-active fallback in the
  // chrome loader) — a person-scoped page never strips the rail down to logo + account.
  await expect(page.getByRole("button", { name: "Publish a skill from your agent" })).toBeVisible();
  await expect(page.getByRole("link", { name: "everyone" })).toBeVisible();
});

test("ending a session deletes exactly that row, persisting across a reload", async ({ page }) => {
  await signIn(page, OWNER_EMAIL);
  await page.goto("/account/sessions");

  const rowA = page.getByRole("listitem").filter({ hasText: "alpha-macbook" });
  await rowA.getByRole("button", { name: "End session" }).click();

  // After the action + revalidation: the honest receipt, one row gone, one left.
  await expect(page.getByText("Session ended.", { exact: false })).toBeVisible();
  await expect(page.getByRole("button", { name: "End session" })).toHaveCount(1);
  await expect(page.getByText("alpha-macbook")).toHaveCount(0);

  // The database delete is the proof — the row is GONE (never tombstoned), and it stays gone.
  expect(await sessionExists(SESS_A)).toBe(false);
  expect(await sessionExists(SESS_B)).toBe(true);

  await page.reload();
  await expect(page.getByText("alpha-macbook")).toHaveCount(0);
  await expect(
    page.getByRole("listitem").filter({ hasText: "beta-desktop" }).getByRole("button", {
      name: "End session",
    }),
  ).toBeVisible();
});
