import { expect, test } from "@playwright/test";
import { SKILL_MD_V1, SKILL_MD_V2 } from "../fixtures/plane/data.mjs";
import {
  adminQuery,
  ensureAccount,
  ensureBundle,
  ensureSeatedUser,
  mintDevice,
  seedCustody,
  theWorkspace,
} from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * The fleet page — DEVICE-LINK-driven: it enumerates the workspace's linked devices (active
 * AND pending), the version each one last reported, and carries the OWNER arms (approve /
 * reject a pending link, remove any link). A device without a link to this workspace does not
 * appear at all — seat removal severs links in the same fence, so there are no ghost rows to
 * enumerate. Signing a device out whole stays self-only on /account/devices.
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
const DEV_PEND = "dk_e2e_fleet_pend"; // seated mate: a PENDING link awaiting the owner
const DEV_GONE = "dk_e2e_fleet_gone"; // departed: registration with NO link — never listed

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
    [DEV_FRESH, DEV_EXCL, DEV_MATE, DEV_PEND, DEV_GONE],
  ]);
  await adminQuery(`delete from web.bundle_detachment where user_id = $1`, [mate.userId]);

  const device = async (
    id: string,
    userId: string,
    name: string,
    lastSeenAgoMs: number,
    linkStatus: "active" | "pending" | null = "active",
  ) => {
    await mintDevice(userId, id, name, `cred-${id}`, linkStatus);
    await adminQuery(
      `update web.device set last_seen_at = now() - ($2 || ' milliseconds')::interval
       where id = $1`,
      [id, String(lastSeenAgoMs)],
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

  // The mate's second device: its link is PENDING — the owner's approval queue.
  await device(DEV_PEND, mate.userId, "mates-new-box", 60_000, "pending");

  // The departed person's device: a registration with NO link here — the severed state seat
  // removal leaves behind. It must not appear on this page at all.
  await device(DEV_GONE, departed.userId, "departed-device", 1_800_000, null);
  await state(DEV_GONE, SKILL_A.id, oldA as string);
});

test.afterAll(async () => {
  await adminQuery(`update web.workspace set staleness_window_ms = 604800000`);
});

test("renders every LINKED device with the right freshness and per-copy status chips", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/settings/devices`);
  await expect(page.getByRole("heading", { name: "Linked devices", level: 1 })).toBeVisible();

  // It is the Devices tab of the Settings section: the shared tab header names both tabs and
  // marks Devices current.
  const tabs = page.getByRole("navigation", { name: "Settings sections" });
  await expect(tabs.getByRole("link", { name: "General" })).toBeVisible();
  await expect(tabs.getByRole("link", { name: "Devices" })).toHaveAttribute("aria-current", "page");

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

  // A device with NO link to this workspace does not appear — severed means gone from here.
  await expect(page.getByTestId(`fleet-device-${DEV_GONE}`)).toHaveCount(0);
  await expect(page.getByText("departed-device")).toHaveCount(0);
});

test("the pending queue: an owner approves a waiting link in place (two-step confirm)", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/settings/devices`);

  const pending = page.getByTestId(`fleet-pending-${DEV_PEND}`);
  await expect(pending).toBeVisible();
  await expect(pending.getByText("mates-new-box")).toBeVisible();

  // The in-place two-step: the first activation ARMS (performing nothing), the armed submit
  // posts. After approval the device moves into the linked list.
  await pending.getByRole("button", { name: "Approve", exact: true }).click();
  await pending.getByRole("button", { name: "Approve — confirm?" }).click();
  await expect(page.getByTestId(`fleet-device-${DEV_PEND}`)).toBeVisible();

  const rows = await adminQuery<{ status: string }>(
    `select status from web.device_link where device_id = $1`,
    [DEV_PEND],
  );
  expect(rows[0]?.status).toBe("active");
});

test("owner Remove severs a link; the whole-device sign-out stays on your-devices", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/settings/devices`);

  // No sign-out arm anywhere — a device is a possession; this page severs LINKS only.
  await expect(page.getByRole("button", { name: "Sign out" })).toHaveCount(0);
  await expect(page.getByRole("link", { name: "Your devices", exact: true })).toBeVisible();

  // Remove the mate's linked device: two-step confirm, then the card is gone and so is the
  // link row + its reported state (bytes on the machine stay — the page copy says so).
  const mate = page.getByTestId(`fleet-device-${DEV_MATE}`);
  await mate.getByRole("button", { name: "Remove", exact: true }).click();
  await mate.getByRole("button", { name: "Remove — confirm?" }).click();
  await expect(page.getByTestId(`fleet-device-${DEV_MATE}`)).toHaveCount(0);

  const links = await adminQuery<{ n: string }>(
    `select count(*)::text as n from web.device_link where device_id = $1`,
    [DEV_MATE],
  );
  expect(links[0]?.n).toBe("0");
  const state = await adminQuery<{ n: string }>(
    `select count(*)::text as n from web.device_bundle_state where device_id = $1`,
    [DEV_MATE],
  );
  expect(state[0]?.n).toBe("0");
});

test("the status chips carry a focusable, hover/focus-only tooltip explainer", async ({ page }) => {
  await theWorkspace();
  await gotoSettled(page, `/settings/devices`);

  // The freshness chip's tooltip trigger is a REAL control (keyboard-reachable), marked cursor-help
  // — the reading legend rides the chips themselves, not a separate explainer section.
  const fresh = page.getByTestId(`fleet-device-${DEV_FRESH}`);
  const trigger = fresh.getByRole("button", { name: "fresh", exact: true });
  await expect(trigger).toBeVisible();
  await expect(trigger).toHaveClass(/cursor-help/);

  // Nothing is shown until the trigger is engaged (hover/focus only — never click-to-open).
  await expect(page.getByRole("tooltip")).toHaveCount(0);

  // Keyboard focus alone reveals the explainer.
  await trigger.focus();
  await expect(trigger).toBeFocused();
  await expect(page.getByRole("tooltip").first()).toBeVisible();
});
