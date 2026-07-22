import { expect, test } from "@playwright/test";
import { adminQuery, ensureSeatedUser, mintDevice } from "./seed";
import { signIn } from "./sign-in";

/**
 * The account-level device list (/account/devices). A device is a POSSESSION of one user now:
 * the page lists the signed-in person's OWN rows — nobody else's, whatever workspace they share
 * — and offers a self sign-out (a plain one-click act: signing out your own device is the escape
 * hatch, not a ceremony over someone else's access). Revocation is FINAL — a DB trigger refuses any
 * un-revoke, so there is deliberately NO un-revoke UI to test.
 *
 * A DEDICATED identity owns this spec's rows (the suite's other specs mint devices for the
 * default owner), with a DECOY device under a second user as the negative control: the list
 * keys on the person, never on "any device in a workspace I belong to".
 */

const OWNER_EMAIL = "devices-owner@example.com";
const DECOY_EMAIL = "devices-decoy@example.com";
const DEVICE_A = "dk_e2e_yourdev_alpha";
const DEVICE_B = "dk_e2e_yourdev_beta";
const DECOY_DEVICE = "dk_e2e_yourdev_decoy";

test.describe.configure({ mode: "serial" });
test.use({ storageState: { cookies: [], origins: [] } });

test.beforeEach(async () => {
  const owner = await ensureSeatedUser(OWNER_EMAIL, "member");
  const decoy = await ensureSeatedUser(DECOY_EMAIL, "member");
  // A known two-active state per test (retry-safe): the trigger refuses un-revoking, so a
  // revoked row from a previous run is re-minted fresh instead of flipped back.
  await adminQuery(`delete from web.device where id = any($1::text[])`, [
    [DEVICE_A, DEVICE_B, DECOY_DEVICE],
  ]);
  await mintDevice(owner.userId, DEVICE_A, "alpha-macbook", `cred-${DEVICE_A}`);
  await mintDevice(owner.userId, DEVICE_B, "beta-desktop", `cred-${DEVICE_B}`);
  await mintDevice(decoy.userId, DECOY_DEVICE, "decoy-machine", `cred-${DECOY_DEVICE}`);
  // One device has phoned home; the other never has (the honest "never seen" line).
  await adminQuery(`update web.device set last_seen_at = now() - interval '1 hour' where id = $1`, [
    DEVICE_A,
  ]);
});

async function revokedAt(deviceId: string): Promise<string | null> {
  const rows = await adminQuery<{ revoked_at: string | null }>(
    `select revoked_at from web.device where id = $1`,
    [deviceId],
  );
  return rows[0]?.revoked_at ?? null;
}

test("lists exactly the person's own devices; another user's device never renders", async ({
  page,
}) => {
  await signIn(page, OWNER_EMAIL);
  await page.goto("/account/devices");
  await expect(page.getByRole("heading", { name: "Your devices" })).toBeVisible();

  // Both of the person's own devices render, with their ids and liveness lines.
  await expect(page.getByText("alpha-macbook")).toBeVisible();
  await expect(page.getByText("beta-desktop")).toBeVisible();
  await expect(page.getByText(DEVICE_A)).toBeVisible();
  await expect(page.getByText("never seen").first()).toBeVisible();

  // The decoy — another person's device in the SAME workspace — is filtered out entirely.
  await expect(page.getByText("decoy-machine")).toHaveCount(0);
  await expect(page.getByText(DECOY_DEVICE)).toHaveCount(0);

  // Exactly two active devices ⇒ exactly two Sign out buttons.
  await expect(page.getByRole("button", { name: "Sign out" })).toHaveCount(2);

  // Off-workspace, the left panel keeps its workspace sections (the last-active fallback in the
  // chrome loader) — a person-scoped page never strips the rail down to logo + account.
  await expect(page.getByRole("button", { name: "Publish a skill from your agent" })).toBeVisible();
  await expect(page.getByRole("link", { name: "everyone" })).toBeVisible();
});

test("self sign-out flips one device to the revoked treatment, persisting across a reload", async ({
  page,
}) => {
  await signIn(page, OWNER_EMAIL);
  await page.goto("/account/devices");

  const rowA = page.getByRole("listitem").filter({ hasText: "alpha-macbook" });
  await rowA.getByRole("button", { name: "Sign out" }).click();

  // After the action + revalidation, device A wears the revoked treatment (the re-enroll
  // hint), its Sign out button is gone, and device B stays active (one button left).
  await expect(page.getByText("signed out — re-enroll to use this device again:")).toBeVisible();
  await expect(page.getByRole("button", { name: "Sign out" })).toHaveCount(1);

  // The database flip is the proof — final by trigger, and it survives a full reload.
  expect(await revokedAt(DEVICE_A)).not.toBeNull();
  expect(await revokedAt(DEVICE_B)).toBeNull();

  await page.reload();
  await expect(rowA.getByRole("button", { name: "Sign out" })).toHaveCount(0);
  await expect(
    page.getByRole("listitem").filter({ hasText: "beta-desktop" }).getByRole("button", {
      name: "Sign out",
    }),
  ).toBeVisible();
});
