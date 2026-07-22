import { expect, test } from "@playwright/test";
import { adminQuery, ensureSeatedUser, theWorkspace } from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The WORKSPACE-POLICY surface on /workspaces/:ws/settings: three owner-only knobs — the review
 * gate (the protection default), the fleet staleness window, and the registration knob — each a
 * dirty-reveal Save/Cancel form that writes IMMEDIATELY (the owner guard is the whole ceremony;
 * there is no password re-entry). The proof of every landed change is the `web.workspace` column;
 * a staged-but-unsaved edit writes NOTHING. The audit ledger feeds each knob's "last set by" line.
 *
 * The suite's default identity is the claimed OWNER. Each test resets the knobs to the column
 * defaults first, so ordering never matters; afterAll leaves them at defaults for later specs
 * (REGISTRATION restores to 'open' — the other specs mint identities through it).
 */

const FOURTEEN_DAYS_MS = "1209600000"; // 14 * 86_400_000

test.describe.configure({ mode: "serial" });

async function resetKnobs(): Promise<void> {
  await adminQuery(
    `update web.workspace set protection_default = 'open',
       staleness_window_ms = 604800000, registration = 'open'`,
  );
}

async function knob(column: string): Promise<string | undefined> {
  const rows = await adminQuery<{ value: string }>(
    `select ${column}::text as value from web.workspace limit 1`,
  );
  return rows[0]?.value;
}

test.beforeEach(async () => {
  await resetKnobs();
});

test.afterAll(async () => {
  await resetKnobs();
});

test("the review gate stages on flip and lands only on Save; the flip alone writes nothing", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/settings`);

  const gate = page.getByRole("switch");
  await expect(gate).toBeVisible();
  await expect(gate).toHaveAttribute("aria-checked", "false"); // the column default

  // Flipping the switch stages the change and reveals the Save/Cancel pair — the flip alone
  // writes nothing.
  await gate.click();
  await expect(page.getByRole("button", { name: "Require review" })).toBeVisible();
  expect(await knob("protection_default")).toBe("open");

  // Save lands it immediately (the owner guard is the whole ceremony — no re-authentication), and
  // the audit ledger feeds the "last set by" line.
  await page.getByRole("button", { name: "Require review" }).click();
  await expect.poll(async () => knob("protection_default")).toBe("reviewed");
  await gotoSettled(page, `/settings`);
  await expect(page.getByRole("switch")).toHaveAttribute("aria-checked", "true");
  await expect(page.getByText(/Last set: ON, by reviewer/)).toBeVisible();
});

test("the staleness window converts days to milliseconds and persists", async ({ page }) => {
  await theWorkspace();
  await gotoSettled(page, `/settings`);

  const days = page.getByLabel("Staleness window (days)");
  await expect(days).toHaveValue("7"); // the 7-day column default
  await days.fill("14");
  // Editing reveals the dirty Save; it writes immediately (no password re-entry).
  await page.getByRole("button", { name: "Save staleness window" }).click();

  await expect.poll(async () => knob("staleness_window_ms")).toBe(FOURTEEN_DAYS_MS);
  await gotoSettled(page, `/settings`);
  await expect(page.getByLabel("Staleness window (days)")).toHaveValue("14");
});

test("the registration knob closes sign-up with one Save", async ({ page }) => {
  await theWorkspace();
  await gotoSettled(page, `/settings`);

  // The suite runs registration-open (auth.setup); close it through the dirty-reveal Save.
  await page
    .getByRole("radio", { name: "Invite-only — sign-up requires a pending invitation" })
    .check();
  await page.getByRole("button", { name: "Require an invitation" }).click();

  await expect.poll(async () => knob("registration")).toBe("invite_only");
  await gotoSettled(page, `/settings`);
  await expect(
    page.getByRole("radio", { name: "Invite-only — sign-up requires a pending invitation" }),
  ).toBeChecked();
  // afterAll restores 'open' — the later specs mint identities through it.
});

test("a non-owner member sees the policy values read-only — no controls", async ({ browser }) => {
  await theWorkspace();
  await ensureSeatedUser("policy-member@example.com", "member");
  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const page = await context.newPage();
  try {
    await signIn(page, "policy-member@example.com");
    await gotoSettled(page, `/settings`);
    await expect(page.getByRole("heading", { name: "Settings", level: 1 })).toBeVisible();
    await expect(page.getByRole("switch")).toHaveCount(0);
    await expect(page.getByRole("radio")).toHaveCount(0);
    // The read-only copy names the current settings + the role boundary.
    await expect(page.getByText("Only an owner can change this").first()).toBeVisible();
  } finally {
    await context.close();
  }
});
