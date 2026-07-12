import { expect, test } from "@playwright/test";
import { Client } from "pg";
import { WS } from "../fixtures/plane/data.mjs";
import { E2E_ADMIN_URL, MEMBER_EMAIL } from "./env";

/**
 * The account-level device list (/settings/devices) over the default signed-in identity
 * (MEMBER_EMAIL — a confirmed MEMBER of ws-e2e, seeded by auth.setup.ts). The page groups the
 * PERSON's OWN device_registry rows by workspace and offers a self sign-out (topos_revoke_device;
 * no step-up — signing out your own device is the escape hatch, not a ceremony over someone
 * else's access).
 *
 * SEED (superuser E2E_ADMIN_URL — never the topos_web app URL, SELECT-only on `plane`): two active
 * devices for MEMBER_EMAIL in ws-e2e, plus a DECOY device owned by another principal in the SAME
 * workspace. The decoy is the negative control — it proves the list keys on the person's own
 * principal, never "any device in a workspace I belong to". Re-seeded before each test so the
 * sign-out test starts from a known two-active state (retry-safe).
 */

const DEVICE_A = "dk_yourdev_alpha";
const DEVICE_B = "dk_yourdev_beta";
const DECOY_DEVICE = "dk_yourdev_decoy";
const DECOY_PRINCIPAL = "decoy-device@example.com";

async function resetDevices(): Promise<void> {
  const db = new Client({ connectionString: E2E_ADMIN_URL });
  await db.connect();
  try {
    await db.query(
      `delete from plane.device_registry where workspace_id = $1 and principal = any($2::text[])`,
      [WS, [MEMBER_EMAIL, DECOY_PRINCIPAL]],
    );
    // public_key is a NOT-NULL 32-byte BYTEA; revoked defaults 0 (active).
    const key = Buffer.alloc(32);
    await db.query(
      `insert into plane.device_registry (workspace_id, device_key_id, public_key, principal, revoked, last_report_at)
       values
         ($1, $2, $6, $5, 0, 1700000000000),
         ($1, $3, $6, $5, 0, null),
         ($1, $4, $6, $7, 0, null)`,
      [WS, DEVICE_A, DEVICE_B, DECOY_DEVICE, MEMBER_EMAIL, key, DECOY_PRINCIPAL],
    );
  } finally {
    await db.end();
  }
}

async function revokedFlag(deviceKeyId: string): Promise<number | undefined> {
  const db = new Client({ connectionString: E2E_ADMIN_URL });
  await db.connect();
  try {
    const { rows } = await db.query(
      `select revoked from plane.device_registry where workspace_id = $1 and device_key_id = $2`,
      [WS, deviceKeyId],
    );
    return rows[0] === undefined ? undefined : Number(rows[0].revoked);
  } finally {
    await db.end();
  }
}

test.describe.configure({ mode: "serial" });

test.beforeEach(async () => {
  await resetDevices();
});

test("lists exactly the person's two devices; the decoy from another principal never renders", async ({
  page,
}) => {
  await page.goto("/settings/devices");
  await expect(page.getByRole("heading", { name: "Your devices" })).toBeVisible();

  // Both of MEMBER_EMAIL's own devices render, headed by the workspace section.
  await expect(page.getByText(DEVICE_A, { exact: true })).toBeVisible();
  await expect(page.getByText(DEVICE_B, { exact: true })).toBeVisible();
  // The never-reported device shows the honest "never reported" line.
  await expect(page.getByText("never reported").first()).toBeVisible();

  // The decoy — another principal's device in the SAME workspace — is filtered out entirely.
  await expect(page.getByText(DECOY_DEVICE, { exact: true })).toHaveCount(0);

  // Exactly two active devices ⇒ exactly two Sign out buttons.
  await expect(page.getByRole("button", { name: "Sign out" })).toHaveCount(2);
});

test("self sign-out flips one device to the revoked treatment, persisting across a reload", async ({
  page,
}) => {
  await page.goto("/settings/devices");

  const rowA = page.getByRole("listitem").filter({ hasText: DEVICE_A });
  await rowA.getByRole("button", { name: "Sign out" }).click();

  // After the action + revalidation, device A wears the revoked treatment (the re-enroll hint),
  // its Sign out button is gone, and device B stays active (one button left).
  await expect(
    page.getByText("signed out — re-enroll to use this device again:").first(),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "Sign out" })).toHaveCount(1);

  // The database flip is the proof — and it survives a full reload (not just optimistic UI).
  expect(await revokedFlag(DEVICE_A)).toBe(1);
  expect(await revokedFlag(DEVICE_B)).toBe(0);

  await page.reload();
  await expect(rowA.getByRole("button", { name: "Sign out" })).toHaveCount(0);
  await expect(
    page.getByRole("listitem").filter({ hasText: DEVICE_B }).getByRole("button", {
      name: "Sign out",
    }),
  ).toBeVisible();
});
