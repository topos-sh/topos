import { expect, type Page, test } from "@playwright/test";
import { Client } from "pg";
import { E2E_ADMIN_URL, E2E_PASSWORD } from "./env";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The fleet page over a DEDICATED workspace, so it never races the other specs' shared rows. The
 * directory seed (a superuser connection — never the SELECT-only app URL) stands up ONE workspace
 * with a confirmed OWNER, two skills with current pointers, and three enrolled devices that cover
 * every state the page names: current, behind, detached, stale, and a removed member's still-live
 * copy. Signed in as the owner, the whole fleet is visible (a member would see only their own).
 *
 * HARNESS DISCIPLINE: everything here is a DIRECTORY-row surface — the guards, the chips, and the
 * guarded revoke all read/write plane.* rows this spec owns; no vault call is involved. The seed is
 * idempotent (delete-then-insert), so a reused local database converges run to run.
 */

const FLEET_WS = "w_fleet_e2e";
const FLEET_ADDRESS = "fleet-e2e";
const FLEET_OWNER_EMAIL = "fleet-owner@example.com";
const DEPARTED_EMAIL = "departed@example.com";

const SKILL_A = "release-guide";
const SKILL_A_ID = "fg-release";
const SKILL_B = "handbook";
const SKILL_B_ID = "fg-handbook";

const CUR_A = "e1".repeat(32);
const OLD_A = "e0".repeat(32);
const CUR_B = "f1".repeat(32);
const OLD_B = "f0".repeat(32);

// A one-hour staleness window, so the "stale" device is deterministic under a 2-hour-old report.
const WINDOW_1H = 3_600_000;

const DEV_OWNER_FRESH = "own1device"; // current + behind; the revoke target
const DEV_OWNER_STALE = "own2device"; // detached + stale
const DEV_DEPARTED = "gonedevice"; // removed upstream, still reporting

const PUBKEY = Buffer.alloc(32, 9);

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

async function seedFleet(): Promise<void> {
  const now = Date.now();
  await withAdmin(async (db) => {
    // Idempotent teardown FIRST (current before skill_commit for the FK).
    for (const table of [
      "device_skill_state",
      "device_registry",
      "current",
      "skill_commit",
      "catalog",
      "workspace_member",
      "workspace_policy",
      "workspace",
    ]) {
      await db.query(`DELETE FROM plane.${table} WHERE workspace_id = $1`, [FLEET_WS]);
    }

    await db.query(
      `INSERT INTO plane.workspace
         (workspace_id, display_name, verified_domain, verified_domain_status, deployment_mode, created_at, name)
       VALUES ($1, 'Fleet E2E', null, 'unverified', 'cloud', '2026-07-01T00:00:00Z', $2)`,
      [FLEET_WS, FLEET_ADDRESS],
    );
    // The one-hour override — the fleet clock the page reads via topos_staleness_window.
    await db.query(
      `INSERT INTO plane.workspace_policy (workspace_id, review_required, invite_policy, staleness_window_ms)
       VALUES ($1, 0, 'members', $2)`,
      [FLEET_WS, WINDOW_1H],
    );
    await db.query(
      `INSERT INTO plane.workspace_member (workspace_id, principal, role, status, invited_by, added_at)
       VALUES ($1, $2, 'owner', 'confirmed', null, '2026-07-01T00:00:00Z')`,
      [FLEET_WS, FLEET_OWNER_EMAIL],
    );
    // DEPARTED_EMAIL is deliberately NOT seated — its device must read "removed upstream".

    for (const [skillId, name, cur] of [
      [SKILL_A_ID, SKILL_A, CUR_A],
      [SKILL_B_ID, SKILL_B, CUR_B],
    ] as const) {
      await db.query(
        `INSERT INTO plane.catalog (workspace_id, skill_id, name, display_name, status, created_at)
         VALUES ($1, $2, $3, null, 'active', '2026-07-01T00:00:00Z')`,
        [FLEET_WS, skillId, name],
      );
      await db.query(
        `INSERT INTO plane.skill_commit (workspace_id, commit_id, skill_id, bundle_digest)
         VALUES ($1, $2, $3, null)`,
        [FLEET_WS, Buffer.from(cur, "hex"), skillId],
      );
      await db.query(
        `INSERT INTO plane.current (workspace_id, skill_id, commit_id, epoch, seq, record, updated_at)
         VALUES ($1, $2, $3, 1, 1, null, $4)`,
        [FLEET_WS, skillId, Buffer.from(cur, "hex"), now],
      );
    }

    const device = async (
      deviceKeyId: string,
      principal: string,
      lastReportAt: number | null,
    ): Promise<void> => {
      await db.query(
        `INSERT INTO plane.device_registry (workspace_id, device_key_id, public_key, principal, revoked, last_report_at)
         VALUES ($1, $2, $3, $4, 0, $5)`,
        [FLEET_WS, deviceKeyId, PUBKEY, principal, lastReportAt],
      );
    };
    const state = async (
      deviceKeyId: string,
      skillId: string,
      appliedHex: string | null,
      reportedAt: number,
      detached = 0,
      detachedAt: number | null = null,
    ): Promise<void> => {
      await db.query(
        `INSERT INTO plane.device_skill_state
           (workspace_id, device_key_id, skill_id, applied_commit, reported_at, detached, detached_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)`,
        [
          FLEET_WS,
          deviceKeyId,
          skillId,
          appliedHex === null ? null : Buffer.from(appliedHex, "hex"),
          reportedAt,
          detached,
          detachedAt,
        ],
      );
    };

    // The owner's fresh device: release-guide is current, handbook is behind.
    await device(DEV_OWNER_FRESH, FLEET_OWNER_EMAIL, now - 60_000);
    await state(DEV_OWNER_FRESH, SKILL_A_ID, CUR_A, now - 60_000);
    await state(DEV_OWNER_FRESH, SKILL_B_ID, OLD_B, now - 60_000);

    // The owner's stale device (2 h ago, past the 1 h window), carrying a detached copy.
    await device(DEV_OWNER_STALE, FLEET_OWNER_EMAIL, now - 7_200_000);
    await state(DEV_OWNER_STALE, SKILL_A_ID, OLD_A, now - 7_200_000, 1, now - 10_800_000);

    // A removed member's device — still reporting recently, still holding a behind copy.
    await device(DEV_DEPARTED, DEPARTED_EMAIL, now - 1_800_000);
    await state(DEV_DEPARTED, SKILL_A_ID, OLD_A, now - 1_800_000);
  });
}

async function revokedFlag(deviceKeyId: string): Promise<string | undefined> {
  return withAdmin(async (db) => {
    const { rows } = await db.query(
      `SELECT revoked::text AS revoked FROM plane.device_registry
       WHERE workspace_id = $1 AND device_key_id = $2`,
      [FLEET_WS, deviceKeyId],
    );
    return rows[0]?.revoked as string | undefined;
  });
}

async function openFleet(page: Page): Promise<void> {
  await signIn(page, FLEET_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${FLEET_WS}/fleet`);
  await expect(page.getByRole("heading", { name: "Fleet", exact: true })).toBeVisible();
}

test.beforeAll(async () => {
  await seedFleet();
});

test("renders every enrolled device with the right status chips", async ({ page }) => {
  await openFleet(page);

  // The owner sees the WHOLE fleet grouped by person — the owner's email heads their group.
  await expect(page.getByRole("heading", { name: FLEET_OWNER_EMAIL })).toBeVisible();

  // The fresh device: release-guide current, handbook behind, and a "fresh" liveness chip.
  const fresh = page.getByTestId(`fleet-device-${DEV_OWNER_FRESH}`);
  await expect(fresh.getByText("current", { exact: true })).toBeVisible();
  await expect(fresh.getByText("behind", { exact: true })).toBeVisible();
  await expect(fresh.getByText("fresh", { exact: true })).toBeVisible();

  // The stale device: a "detached" copy and a "stale" liveness chip.
  const stale = page.getByTestId(`fleet-device-${DEV_OWNER_STALE}`);
  await expect(stale.getByText("detached", { exact: true })).toBeVisible();
  await expect(stale.getByText("stale", { exact: true })).toBeVisible();
  await expect(stale.getByText("last known state")).toBeVisible();
});

test("names its blind spots — detached copies and removed-upstream devices", async ({ page }) => {
  await openFleet(page);

  // The removed-upstream section names the departed principal and its still-present copy.
  await expect(page.getByRole("heading", { name: "Removed upstream" })).toBeVisible();
  const gone = page.getByTestId(`fleet-device-${DEV_DEPARTED}`);
  await expect(gone.getByText("removed upstream", { exact: true })).toBeVisible();
  await expect(gone.getByText(DEPARTED_EMAIL)).toBeVisible();
  await expect(gone.getByText("behind", { exact: true })).toBeVisible();

  // The reading-guide footnote names the reporting cadence + the detached/removed blind spots.
  const guide = page.getByRole("region", { name: "Reading this page" });
  await expect(guide.getByText(/start of a session/)).toBeVisible();
  await expect(guide.getByText(/Detached/)).toBeVisible();
  await expect(guide.getByText(/Removed upstream/)).toBeVisible();
});

test("an owner revokes a device through step-up; the row flips to revoked on reload", async ({
  page,
}) => {
  await openFleet(page);

  const fresh = page.getByTestId(`fleet-device-${DEV_OWNER_FRESH}`);
  // Open the revoke ceremony, confirm with the account password, and revoke.
  await fresh.getByText("Revoke this device").click();
  await fresh.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await fresh.getByRole("button", { name: "Revoke device" }).click();

  // The revalidated loader re-reads the flipped row: the "revoked" chip appears and the control is
  // gone (a revoked device offers no revoke).
  await expect(fresh.getByText("revoked", { exact: true })).toBeVisible();
  await expect(fresh.getByRole("button", { name: "Revoke device" })).toHaveCount(0);

  // The guarded write is the proof — the directory row is flipped.
  expect(await revokedFlag(DEV_OWNER_FRESH)).toBe("1");

  // And it survives a fresh load.
  await gotoSettled(page, `/workspaces/${FLEET_WS}/fleet`);
  const reloaded = page.getByTestId(`fleet-device-${DEV_OWNER_FRESH}`);
  await expect(reloaded.getByText("revoked", { exact: true })).toBeVisible();
});
