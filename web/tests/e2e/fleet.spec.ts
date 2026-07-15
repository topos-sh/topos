import { expect, test } from "@playwright/test";
import { SKILL_MD_V1, SKILL_MD_V2 } from "../fixtures/plane/data.mjs";
import {
  adminQuery,
  ensureAccount,
  ensureBundle,
  ensureSeatedUser,
  seedCustody,
  theWorkspace,
} from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * The fleet page — a read-only visibility surface that enumerates every device touching the
 * workspace, the version each one last reported, and NAMES its blind spots (stale devices,
 * detached copies, per-device exclusions, removed members' still-live copies) instead of
 * omitting them. It carries NO revoke arm — a device is a possession, revocation is self-only
 * on /settings/devices.
 *
 * Staleness joins `device.last_seen_at` against the workspace window (set to ONE HOUR here,
 * restored after); per-copy status joins `device_bundle_state` against the custody pointer
 * mirror, the person's detach records, and the device's exclusions. The suite's default
 * identity is the claimed OWNER, so the WHOLE fleet is visible.
 */

const MATE_EMAIL = "fleet-mate@example.com";
const DEPARTED_EMAIL = "fleet-departed@example.com";

const SKILL_A = { id: "s_e2e_fleet_a", name: "release-guide" };
const SKILL_B = { id: "s_e2e_fleet_b", name: "handbook" };

const DEV_FRESH = "dk_e2e_fleet_fresh"; // owner: current + behind, fresh
const DEV_EXCL = "dk_e2e_fleet_excl"; // owner: excluded copy, stale
const DEV_MATE = "dk_e2e_fleet_mate"; // seated mate: detached copy, stale
const DEV_GONE = "dk_e2e_fleet_gone"; // departed (no seat): removed upstream

const WINDOW_1H = 3_600_000;

test.describe.configure({ mode: "serial" });

test.beforeAll(async () => {
  const ws = await theWorkspace();
  await adminQuery(`update web.workspace set staleness_window_ms = $1`, [WINDOW_1H]);

  const mate = await ensureSeatedUser(MATE_EMAIL, "member");
  const departed = await ensureAccount(DEPARTED_EMAIL); // an ACCOUNT, deliberately seatless
  const owner = (
    await adminQuery<{ user_id: string }>(
      `select user_id from web.seat where role = 'owner' limit 1`,
    )
  )[0]?.user_id as string;

  await ensureBundle(SKILL_A);
  await ensureBundle(SKILL_B);
  const seeded = await seedCustody([
    {
      ws: ws.id,
      bundle: SKILL_A.id,
      versions: [
        { files: [{ path: "SKILL.md", content: SKILL_MD_V1 }], message: "v1" },
        { files: [{ path: "SKILL.md", content: SKILL_MD_V2 }], parent: 0, message: "v2" },
      ],
      current: 1,
    },
    {
      ws: ws.id,
      bundle: SKILL_B.id,
      versions: [
        { files: [{ path: "SKILL.md", content: "# Handbook v1\n" }], message: "v1" },
        { files: [{ path: "SKILL.md", content: "# Handbook v2\n" }], parent: 0, message: "v2" },
      ],
      current: 1,
    },
  ]);
  const [oldA, curA] = [seeded[0]?.versions[0]?.version_id, seeded[0]?.versions[1]?.version_id];
  const oldB = seeded[1]?.versions[0]?.version_id;

  // A clean slate for THIS file's devices + records (idempotent on a reused database).
  await adminQuery(`delete from web.device where id = any($1::text[])`, [
    [DEV_FRESH, DEV_EXCL, DEV_MATE, DEV_GONE],
  ]);
  await adminQuery(`delete from web.bundle_detachment where user_id = $1`, [mate.userId]);

  const device = async (id: string, userId: string, name: string, lastSeenAgoMs: number) => {
    await adminQuery(
      `insert into web.device (id, user_id, display_name, credential_sha256, last_seen_at)
       values ($1, $2, $3, sha256(convert_to($4, 'UTF8')), now() - ($5 || ' milliseconds')::interval)`,
      [id, userId, name, `cred-${id}`, String(lastSeenAgoMs)],
    );
  };
  const state = async (deviceId: string, bundleId: string, applied: string) => {
    await adminQuery(
      `insert into web.device_bundle_state (device_id, bundle_id, applied_version_id, reported_at)
       values ($1, $2, $3, now() - interval '10 minutes')`,
      [deviceId, bundleId, applied],
    );
  };

  // The owner's fresh device: release-guide is current, handbook is behind.
  await device(DEV_FRESH, owner, "fresh-workstation", 60_000);
  await state(DEV_FRESH, SKILL_A.id, curA as string);
  await state(DEV_FRESH, SKILL_B.id, oldB as string);

  // The owner's stale device carries a per-device EXCLUDED copy (2h > the 1h window).
  await device(DEV_EXCL, owner, "excluded-laptop", 7_200_000);
  await state(DEV_EXCL, SKILL_B.id, oldB as string);
  await adminQuery(
    `insert into web.device_exclusion (device_id, bundle_id) values ($1, $2)
     on conflict do nothing`,
    [DEV_EXCL, SKILL_B.id],
  );

  // The seated mate's stale device holds a DETACHED copy (the person's detach record names it).
  await device(DEV_MATE, mate.userId, "mates-machine", 7_200_000);
  await state(DEV_MATE, SKILL_A.id, oldA as string);
  await adminQuery(
    `insert into web.bundle_detachment (user_id, workspace_id, bundle_id, cause)
     values ($1, $2, $3, 'channel_leave') on conflict do nothing`,
    [mate.userId, ws.id, SKILL_A.id],
  );

  // A REMOVED member's device — no seat, still reporting: the removed-upstream blind spot.
  await device(DEV_GONE, departed.userId, "departed-device", 1_800_000);
  await state(DEV_GONE, SKILL_A.id, oldA as string);
});

test.afterAll(async () => {
  await adminQuery(`update web.workspace set staleness_window_ms = 604800000`);
});

test("renders every device with the right freshness and per-copy status chips", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/fleet`);
  await expect(page.getByRole("heading", { name: "Fleet", exact: true })).toBeVisible();

  // The fresh device: release-guide current, handbook behind, a "fresh" liveness chip.
  const fresh = page.getByTestId(`fleet-device-${DEV_FRESH}`);
  await expect(fresh.getByText("current", { exact: true })).toBeVisible();
  await expect(fresh.getByText("behind", { exact: true })).toBeVisible();
  await expect(fresh.getByText("fresh", { exact: true })).toBeVisible();

  // The excluded copy: opted out on that one device; the device itself is past the window.
  const excluded = page.getByTestId(`fleet-device-${DEV_EXCL}`);
  await expect(excluded.getByText("excluded", { exact: true })).toBeVisible();
  await expect(excluded.getByText("stale", { exact: true })).toBeVisible();

  // The detached copy: the person's detach record names it, with its cause humanized.
  const mate = page.getByTestId(`fleet-device-${DEV_MATE}`);
  await expect(mate.getByText("detached", { exact: true })).toBeVisible();
  await expect(mate.getByText("last known state")).toBeVisible();
  await expect(mate.getByText("they left the channel")).toBeVisible();
});

test("names its blind spots: removed-upstream devices and the standing detach records", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/fleet`);

  // The removed-upstream section names the departed person and their still-present copy.
  await expect(page.getByRole("heading", { name: "Removed upstream" })).toBeVisible();
  const gone = page.getByTestId(`fleet-device-${DEV_GONE}`);
  await expect(gone.getByText("removed upstream", { exact: true }).first()).toBeVisible();
  await expect(gone.getByText(DEPARTED_EMAIL).first()).toBeVisible();

  // The chase list: whose copies froze, of what, and why — surviving quiet devices.
  const detached = page.getByRole("region", { name: "Detached copies" });
  await expect(detached).toBeVisible();
  await expect(detached.getByText(SKILL_A.name)).toBeVisible();
  await expect(detached.getByText("they left the channel")).toBeVisible();

  // The reading guide names the reporting cadence + every blind-spot vocabulary word.
  const guide = page.getByRole("region", { name: "Reading this page" });
  await expect(guide.getByText(/start of a session/)).toBeVisible();
  await expect(guide.getByText(/Detached/)).toBeVisible();
  await expect(guide.getByText(/Removed upstream/)).toBeVisible();
});

test("read-only by design: no revoke arm anywhere — your-devices is the sign-out surface", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/fleet`);
  await expect(page.getByRole("button", { name: /revoke/i })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Sign out" })).toHaveCount(0);
  // The header action (the reading guide carries a second, lowercase link to the same place).
  await expect(page.getByRole("link", { name: "Your devices", exact: true })).toBeVisible();
});
