import { expect, test } from "@playwright/test";
import { SKILL_MD_V1, SKILL_MD_V2 } from "../fixtures/plane/data.mjs";
import { adminQuery, custodyCalls, ensureBundle, seedCustody, theWorkspace } from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * The SKILL LIFECYCLE ceremonies: rename + protection pin + archive (skill settings), unarchive
 * + delete (the archive page), and per-version purge (the history tab) — every one an OWNER act
 * gated by the owner guard alone (no re-authentication). Rename, archive, and unarchive wear a
 * lightweight IN-PLACE confirm (arm, then confirm); the destructive ones (delete, purge)
 * additionally require typing the resource's exact name before a plain submit. All app-tier row
 * transactions over `web.bundle` now, keyed on the immutable skill id; only the byte halves
 * (delete's bundle drop, the purge) reach the vault, and the recorded custody calls prove
 * exactly those. The suite's default identity is the claimed OWNER. Serial — one identity's
 * ceremonies in a deterministic order.
 */

const SKILL = { id: "s_e2e_lc", name: "lifecycle-notes" };
const RENAMED = "lifecycle-notes-next";
const PURGE_SKILL = { id: "s_e2e_purge", name: "purge-notes" };

let goodId: string; // the purge subject — PURGE_SKILL's non-current ancestor

test.describe.configure({ mode: "serial" });

test.beforeAll(async () => {
  const ws = await theWorkspace();
  // A clean slate on a reused database: drop this file's rows whatever state a previous run
  // left them in (tombstones and archived names included; hints cascade with the bundle).
  await adminQuery(`delete from web.bundle where id = any($1::text[])`, [
    [SKILL.id, PURGE_SKILL.id],
  ]);
  await ensureBundle(SKILL);
  await ensureBundle(PURGE_SKILL);
  const seeded = await seedCustody([
    {
      ws: ws.id,
      bundle: SKILL.id,
      versions: [
        { files: [{ path: "SKILL.md", content: SKILL_MD_V1 }], message: "v1" },
        { files: [{ path: "SKILL.md", content: SKILL_MD_V2 }], parent: 0, message: "v2" },
      ],
      current: 1,
    },
    {
      ws: ws.id,
      bundle: PURGE_SKILL.id,
      versions: [
        {
          files: [{ path: "SKILL.md", content: "# Purge target\n\nleaked-secret\n" }],
          message: "leaky",
        },
        {
          files: [{ path: "SKILL.md", content: "# Purge target\n\nscrubbed\n" }],
          parent: 0,
          message: "scrubbed",
        },
      ],
      current: 1,
    },
  ]);
  goodId = seeded[1]?.versions[0]?.version_id as string;
});

test("rename: an in-place confirm renames (id-keyed) and the old name redirects", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/skills/${SKILL.name}/settings`);

  // Rename is an in-place confirm — no password. The Enter-key path may NOT skip it: the
  // resting control is the form's default button with an intercepted activation, so a keyboard
  // submit from the text field ARMS instead of renaming (the implicit-submission bypass
  // regression). Arming alone writes nothing.
  await page.locator("#rename-new-name").fill(RENAMED);
  await page.locator("#rename-new-name").press("Enter");
  await expect(page.getByRole("button", { name: "Rename — confirm?" })).toBeVisible();
  expect(
    (await adminQuery<{ name: string }>(`select name from web.bundle where id = $1`, [SKILL.id]))[0]
      ?.name,
  ).toBe(SKILL.name);

  // The second activation confirms — arming moved focus onto the armed submit, so a second
  // Enter is the confirm. Renames (id-keyed) and redirects to the new name's settings.
  await page.getByRole("button", { name: "Rename — confirm?" }).press("Enter");
  await page.waitForURL(`**/skills/${RENAMED}/settings`);
  const row = await adminQuery<{ name: string }>(`select name from web.bundle where id = $1`, [
    SKILL.id,
  ]);
  expect(row[0]?.name).toBe(RENAMED);

  // The old name became a resolving hint: a bookmark keeps working through the redirect.
  const hint = await adminQuery<{ bundle_id: string }>(
    `select bundle_id from web.bundle_name_hint where old_name = $1`,
    [SKILL.name],
  );
  expect(hint[0]?.bundle_id).toBe(SKILL.id);
  await gotoSettled(page, `/skills/${SKILL.name}`);
  await page.waitForURL(`**/skills/${RENAMED}`);
});

test("the protection pin: an owner Save flips the bundle to reviewed", async ({ page }) => {
  await theWorkspace();
  await gotoSettled(page, `/skills/${RENAMED}/settings`);

  await page.getByRole("radio", { name: /Reviewed — a member's publish/ }).check();
  // Choosing a new value reveals the dirty Save; it writes immediately (no password re-entry).
  await page.getByRole("button", { name: "Save protection" }).click();
  await expect
    .poll(
      async () =>
        (
          await adminQuery<{ protection: string | null }>(
            `select protection from web.bundle where id = $1`,
            [SKILL.id],
          )
        )[0]?.protection,
    )
    .toBe("reviewed");
});

test("archive retires the skill: the base name frees, the catalog drops it, the archive lists it", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/skills/${RENAMED}/settings`);

  // Archive is an in-place confirm — arm, then confirm.
  await page.getByRole("button", { name: `Archive ${RENAMED}` }).click();
  await page.getByRole("button", { name: "Archive — confirm?" }).click();
  await page.waitForURL(`**/archive`);

  const row = await adminQuery<{ status: string; name: string; base_name: string }>(
    `select status, name, base_name from web.bundle where id = $1`,
    [SKILL.id],
  );
  expect(row[0]?.status).toBe("archived");
  expect(row[0]?.name.startsWith(`${RENAMED}-archived-`)).toBe(true);
  expect(row[0]?.base_name).toBe(RENAMED);

  // The archive page lists it under the archived name; the dashboard no longer does.
  await expect(page.getByText(row[0]?.name as string).first()).toBeVisible();
  await gotoSettled(page, `/`);
  await expect(page.getByRole("link", { name: RENAMED })).toHaveCount(0);
});

test("unarchive restores the base name exactly", async ({ page }) => {
  await theWorkspace();
  await gotoSettled(page, `/settings/archive`);

  await page.getByText("Unarchive…", { exact: true }).click();
  // Unarchive is an in-place confirm — arm, then confirm.
  await page.getByRole("button", { name: "Unarchive", exact: true }).click();
  await page.getByRole("button", { name: "Unarchive — confirm?" }).click();

  // A landed unarchive revalidates the row off the archive list…
  await expect(page.getByText(`${RENAMED}-archived-`, { exact: false })).toHaveCount(0);
  // …and the row is active again under its exact pre-archive name.
  const row = await adminQuery<{ status: string; name: string }>(
    `select status, name from web.bundle where id = $1`,
    [SKILL.id],
  );
  expect(row[0]?.status).toBe("active");
  expect(row[0]?.name).toBe(RENAMED);
});

test("delete is archive-first + typed-name gated; the tombstone stays, the bytes drop", async ({
  page,
}) => {
  await theWorkspace();
  // Archive again — deletion is a step further than archive, never a shortcut around it.
  await gotoSettled(page, `/skills/${RENAMED}/settings`);
  await page.getByRole("button", { name: `Archive ${RENAMED}` }).click();
  await page.getByRole("button", { name: "Archive — confirm?" }).click();
  await page.waitForURL(`**/archive`);
  const archivedName = (
    await adminQuery<{ name: string }>(`select name from web.bundle where id = $1`, [SKILL.id])
  )[0]?.name as string;

  await page.getByText("Delete permanently…", { exact: true }).click();
  const confirm = page.locator(`#delete-${SKILL.id}-confirm`);

  // The WRONG name is refused by the typed-name gate — no byte drop.
  await confirm.fill("not-the-name");
  await page.getByRole("button", { name: "Delete permanently" }).click();
  await expect(page.getByText(/Type the exact name/)).toBeVisible();
  expect(await custodyCalls({ route: "delete-bundle", bundle: SKILL.id })).toHaveLength(0);

  // The EXACT archived name: the tombstone lands, then the vault drops the whole custody.
  await confirm.fill(archivedName);
  await page.getByRole("button", { name: "Delete permanently" }).click();
  await expect(page.getByText(`Deleted ${archivedName}`, { exact: false })).toBeVisible();
  await expect(page.getByText("Its bytes are reclaimed from the server")).toBeVisible();

  expect(await custodyCalls({ route: "delete-bundle", bundle: SKILL.id })).toHaveLength(1);
  // The row survives as a tombstone (history outlives the bytes); the custody rows are gone.
  const row = await adminQuery<{ status: string }>(`select status from web.bundle where id = $1`, [
    SKILL.id,
  ]);
  expect(row[0]?.status).toBe("deleted");
  const custody = await adminQuery<{ n: string }>(
    `select count(*)::text as n from plane.version where bundle_id = $1`,
    [SKILL.id],
  );
  expect(custody[0]?.n).toBe("0");
});

test("purge drops ONE past version's bytes; the hash stays a tombstone and history says so", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/skills/${PURGE_SKILL.name}/history`);

  const purgeSection = page.getByRole("region", { name: "Purge version bytes" });
  await expect(purgeSection).toBeVisible();
  await purgeSection.locator("summary").first().click();

  const short = goodId.slice(0, 12);
  await page.locator(`#purge-${short}-confirm`).fill(PURGE_SKILL.name);
  await page.getByRole("button", { name: "Purge this version" }).click();
  await expect(
    page.getByText("Purged — this version's bytes are gone from the server"),
  ).toBeVisible();

  // The vault call carried exactly this version; the mirror rows wear the tombstone.
  const calls = await custodyCalls({ route: "purge", bundle: PURGE_SKILL.id });
  expect(calls).toHaveLength(1);
  expect(calls[0]?.body.version_id).toBe(goodId);
  const purged = await adminQuery<{ purged: boolean }>(
    `select purged_at is not null as purged from plane.version
     where bundle_id = $1 and version_id = $2`,
    [PURGE_SKILL.id, goodId],
  );
  expect(purged[0]?.purged).toBe(true);

  // The purge is about BYTES: the version's metadata may keep rendering from the app's
  // immutable-content LRU (a hit can never be stale — retention is the vault's fact), but the
  // leaked bytes themselves are no longer served: the file view degrades to an honest card and
  // the secret never reaches the page.
  await gotoSettled(page, `/skills/${PURGE_SKILL.name}/versions/${goodId}/files/SKILL.md`);
  await expect(page.getByText(/couldn't be fetched|This version isn't available/)).toBeVisible();
  expect(await page.content()).not.toContain("leaked-secret");
});
