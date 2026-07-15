import { expect, test } from "@playwright/test";
import { E2E_PASSWORD } from "./env";
import { adminQuery, ensureSeatedUser, theWorkspace } from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The WORKSPACE-POLICY surface on /workspaces/:ws/settings: four owner-only knobs — the review
 * gate (the protection default), the invite policy, the fleet staleness window, and the
 * registration knob — each a STEP-UP ceremony (the owner re-enters their password inside the
 * form, verified immediately before the write). The proof of every landed change is the
 * `web.workspace` column; a wrong password writes NOTHING. The audit ledger feeds each knob's
 * "last set by" line.
 *
 * The suite's default identity is the claimed OWNER. Each test resets the knobs to the column
 * defaults first, so ordering never matters; afterAll leaves them at defaults for later specs
 * (REGISTRATION restores to 'open' — the other specs mint identities through it).
 */

const FOURTEEN_DAYS_MS = "1209600000"; // 14 * 86_400_000

test.describe.configure({ mode: "serial" });

async function resetKnobs(): Promise<void> {
  await adminQuery(
    `update web.workspace set protection_default = 'open', invite_policy = 'members',
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

test("the review gate demands step-up; the switch flips only with the right password", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/settings`);

  const gate = page.getByRole("switch");
  await expect(gate).toBeVisible();
  await expect(gate).toHaveAttribute("aria-checked", "false"); // the column default

  // Flipping the switch stages the change and reveals the confirm — the click alone writes
  // nothing.
  await gate.click();
  await expect(page.getByLabel("Confirm with your password")).toBeVisible();
  expect(await knob("protection_default")).toBe("open");

  // A WRONG password refuses; the row is unchanged.
  await page.getByLabel("Confirm with your password").fill("wrong-password-9999");
  await page.getByRole("button", { name: "Require review" }).click();
  await expect(page.getByRole("alert")).toContainText("Password check failed");
  expect(await knob("protection_default")).toBe("open");

  // The right password lands it, and the audit ledger feeds the "last set by" line.
  await page.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Require review" }).click();
  await expect.poll(async () => knob("protection_default")).toBe("reviewed");
  await gotoSettled(page, `/workspaces/${ws.id}/settings`);
  await expect(page.getByRole("switch")).toHaveAttribute("aria-checked", "true");
  await expect(page.getByText(/Last set: ON, by reviewer/)).toBeVisible();
});

test("the invite policy flips to owners-only behind step-up and persists", async ({ page }) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/settings`);

  await expect(page.getByRole("radio", { name: "Any member can invite" })).toBeChecked();
  await page.getByRole("radio", { name: "Only owners can invite" }).check();
  await page.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Save invite policy" }).click();

  await expect.poll(async () => knob("invite_policy")).toBe("owners");
  await gotoSettled(page, `/workspaces/${ws.id}/settings`);
  await expect(page.getByRole("radio", { name: "Only owners can invite" })).toBeChecked();
  await expect(page.getByText(/Last set: owners only, by reviewer/)).toBeVisible();
});

test("the staleness window converts days to milliseconds and persists", async ({ page }) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/settings`);

  const days = page.getByLabel("Staleness window (days)");
  await expect(days).toHaveValue("7"); // the 7-day column default
  await days.fill("14");
  await expect(page.getByLabel("Confirm with your password")).toBeVisible();
  await page.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Save staleness window" }).click();

  await expect.poll(async () => knob("staleness_window_ms")).toBe(FOURTEEN_DAYS_MS);
  await gotoSettled(page, `/workspaces/${ws.id}/settings`);
  await expect(page.getByLabel("Staleness window (days)")).toHaveValue("14");
});

test("the registration knob closes sign-up behind step-up", async ({ page }) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/settings`);

  // The suite runs registration-open (auth.setup); close it through the ceremony.
  await page
    .getByRole("radio", { name: "Invite-only — sign-up requires a pending invitation" })
    .check();
  await page.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Require an invitation" }).click();

  await expect.poll(async () => knob("registration")).toBe("invite_only");
  await gotoSettled(page, `/workspaces/${ws.id}/settings`);
  await expect(
    page.getByRole("radio", { name: "Invite-only — sign-up requires a pending invitation" }),
  ).toBeChecked();
  // afterAll restores 'open' — the later specs mint identities through it.
});

test("a non-owner member sees the policy values read-only — no controls", async ({ browser }) => {
  const ws = await theWorkspace();
  await ensureSeatedUser("policy-member@example.com", "member");
  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const page = await context.newPage();
  try {
    await signIn(page, "policy-member@example.com");
    await gotoSettled(page, `/workspaces/${ws.id}/settings`);
    await expect(page.getByRole("heading", { name: "Settings", level: 1 })).toBeVisible();
    await expect(page.getByRole("switch")).toHaveCount(0);
    await expect(page.getByRole("radio")).toHaveCount(0);
    // The read-only copy names the current settings + the role boundary.
    await expect(page.getByText("Only an owner can change this").first()).toBeVisible();
  } finally {
    await context.close();
  }
});
