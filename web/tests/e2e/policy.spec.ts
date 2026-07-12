import { expect, test } from "@playwright/test";
import { Client } from "pg";
import { ROSTER_MEMBER_EMAIL, ROSTER_OWNER_EMAIL, ROSTER_WS } from "../fixtures/plane/data.mjs";
import { E2E_ADMIN_URL, E2E_PASSWORD } from "./env";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The WORKSPACE-POLICY surface on /workspaces/:ws/settings: three owner-only knobs — the invite
 * policy, the fleet staleness window, and the review-required gate — each now a STEP-UP ceremony
 * (the owner re-enters their password inside the form, verified immediately before the write). The
 * proof of every landed change is the DIRECTORY row (`plane.workspace_policy`), read directly over
 * the superuser URL; a wrong password writes NOTHING.
 *
 * Identities ride the PLANE SEED: w_roster's owner (ROSTER_OWNER_EMAIL) holds a confirmed OWNER
 * seat, so the controls render. Each test resets the policy row to the seed defaults first, so
 * ordering never matters; the afterAll leaves it at defaults for any later spec.
 */

const FOURTEEN_DAYS_MS = "1209600000"; // 14 * 86_400_000.

test.describe.configure({ mode: "serial" });

async function withAdmin<T>(fn: (db: Client) => Promise<T>): Promise<T> {
  const db = new Client({ connectionString: E2E_ADMIN_URL });
  await db.connect();
  try {
    return await fn(db);
  } finally {
    await db.end();
  }
}

/** Reset w_roster's policy row to the seed defaults (review off / members / 7 days). */
async function resetPolicy(): Promise<void> {
  await withAdmin((db) =>
    db.query(
      `INSERT INTO plane.workspace_policy (workspace_id, review_required, invite_policy, staleness_window_ms)
       VALUES ($1, 0, 'members', 604800000)
       ON CONFLICT (workspace_id) DO UPDATE
         SET review_required = 0, invite_policy = 'members', staleness_window_ms = 604800000`,
      [ROSTER_WS],
    ),
  );
}

async function policyColumn(column: string): Promise<string | undefined> {
  return withAdmin(async (db) => {
    const { rows } = await db.query(
      `SELECT ${column}::text AS value FROM plane.workspace_policy WHERE workspace_id = $1`,
      [ROSTER_WS],
    );
    return rows[0]?.value as string | undefined;
  });
}

test.beforeEach(async () => {
  await resetPolicy();
});

test.afterAll(async () => {
  await resetPolicy();
});

test("the owner changes the invite policy behind step-up; a wrong password refuses, the right one persists", async ({
  page,
}) => {
  await signIn(page, ROSTER_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/settings`);

  // The seed default: any confirmed member may invite.
  await expect(page.getByRole("radio", { name: "Any confirmed member can invite" })).toBeChecked();

  // Choosing owners-only stages the change and reveals the step-up confirm — nothing is written yet.
  await page.getByRole("radio", { name: "Only owners can invite" }).check();
  await expect(page.getByLabel("Confirm with your password")).toBeVisible();

  // A WRONG password refuses: the typed error shows, and the directory row is unchanged.
  await page.getByLabel("Confirm with your password").fill("wrong-password-9999");
  await page.getByRole("button", { name: "Save invite policy" }).click();
  await expect(page.getByRole("alert")).toContainText(/password/i);
  expect(await policyColumn("invite_policy")).toBe("members");

  // A reload proves nothing persisted — members is still the selection.
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/settings`);
  await expect(page.getByRole("radio", { name: "Any confirmed member can invite" })).toBeChecked();

  // The RIGHT password lands it: the row flips to owners and survives a reload.
  await page.getByRole("radio", { name: "Only owners can invite" }).check();
  await page.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Save invite policy" }).click();
  await expect.poll(async () => policyColumn("invite_policy")).toBe("owners");
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/settings`);
  await expect(page.getByRole("radio", { name: "Only owners can invite" })).toBeChecked();
});

test("the owner sets the staleness window to 14 days behind step-up; it persists", async ({
  page,
}) => {
  await signIn(page, ROSTER_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/settings`);

  const days = page.getByLabel("Staleness window (days)");
  await expect(days).toHaveValue("7"); // the seeded 7-day default

  // Editing the field stages the change; the step-up confirm appears.
  await days.fill("14");
  await expect(page.getByLabel("Confirm with your password")).toBeVisible();
  await page.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Save staleness window" }).click();

  // 14 days in ms is the directory row's proof; the reloaded field shows 14.
  await expect.poll(async () => policyColumn("staleness_window_ms")).toBe(FOURTEEN_DAYS_MS);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/settings`);
  await expect(page.getByLabel("Staleness window (days)")).toHaveValue("14");
});

test("the review gate now demands step-up; the switch flips only with the right password", async ({
  page,
}) => {
  await signIn(page, ROSTER_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/settings`);

  const gate = page.getByRole("switch");
  await expect(gate).toBeVisible();
  await expect(gate).toHaveAttribute("aria-checked", "false"); // seeded off

  // Flipping the switch stages the change and reveals the confirm — the click alone writes nothing.
  await gate.click();
  await expect(page.getByLabel("Confirm with your password")).toBeVisible();
  expect(await policyColumn("review_required")).toBe("0");

  // The right password lands it: the row flips to 1 and the reloaded switch stays on.
  await page.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Require review" }).click();
  await expect.poll(async () => policyColumn("review_required")).toBe("1");
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/settings`);
  await expect(page.getByRole("switch")).toHaveAttribute("aria-checked", "true");
});

test("a non-owner member sees the policy values read-only — no controls", async ({ page }) => {
  // ROSTER_MEMBER holds a confirmed MEMBER seat: the settings page renders, but the owner controls
  // (the switch, the radios) do not — the honest read-only state.
  await signIn(page, ROSTER_MEMBER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/settings`);
  await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
  await expect(page.getByRole("switch")).toHaveCount(0);
  await expect(page.getByRole("radio")).toHaveCount(0);
  // The read-only copy names the current settings + the role boundary.
  await expect(page.getByText("Only an owner can change this").first()).toBeVisible();
});
