import { expect, test } from "@playwright/test";
import { BASE_URL, MEMBER_EMAIL } from "./env";
import { adminQuery, ensureSeatedUser, latestMail, theWorkspace } from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The members page (/workspaces/:ws/members) over the ONE seat table. Every membership act
 * lives here: INVITE (owner-only, mail-armed — an invitation row + the notice mail, never a
 * seat), REVOKE-INVITATION (owner-only — non-destructive like the invite, its own small confirm
 * panel), ROLE CHANGE / REMOVE (owner acts, each worn as a lightweight IN-PLACE confirm — no
 * re-authentication), LEAVE (the member's own in-place confirm), and the LAST-OWNER fence that
 * refuses orphaning the workspace. The proof of every landed act is the `web` row; arming a
 * confirm and never confirming writes NOTHING.
 *
 * The suite's default identity is the claimed OWNER; the two seeded members are this file's
 * own. Serial — the mutating tests keep a deterministic order.
 */

const MEMBER_ONE = "roster-m1@example.com";
const MEMBER_TWO = "roster-m2@example.com";
const INVITED = "dana-roster@example.com";
// Single-tenant grammar: the install's ORIGIN is the workspace address — no slug suffix.
const FOLLOW_LINE = `topos login ${BASE_URL}`;

test.describe.configure({ mode: "serial" });

test.beforeAll(async () => {
  await ensureSeatedUser(MEMBER_ONE, "member");
  await ensureSeatedUser(MEMBER_TWO, "member");
  // A pending row from a previous local run would make the invite an upsert — fine — but the
  // revoke test wants a deterministic start.
  await adminQuery(`delete from web.invitation where email = $1`, [INVITED]);
});

async function seatRole(email: string): Promise<string | undefined> {
  const rows = await adminQuery<{ role: string }>(
    `select s.role from web.seat s join web."user" u on u.id = s.user_id where u.email = $1`,
    [email],
  );
  return rows[0]?.role;
}

test("the members page: the roster rows, the address block, and the sole owner's honest lockout", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/members`);
  await expect(page.getByRole("heading", { name: "Members", level: 1 })).toBeVisible();

  // The owner's own row: role chip + "(you)", and NO controls — the sole owner can be neither
  // removed nor demoted (the honest lockout the data layer also enforces).
  const ownerRow = page.getByRole("listitem").filter({ hasText: MEMBER_EMAIL });
  await expect(ownerRow.getByText("owner", { exact: true })).toBeVisible();
  await expect(ownerRow.getByText("(you)")).toBeVisible();
  await expect(ownerRow.getByText("workspace owner")).toBeVisible();
  await expect(ownerRow.getByRole("button", { name: "Change role" })).toHaveCount(0);
  await expect(ownerRow.getByRole("button", { name: "Remove" })).toHaveCount(0);

  // The seeded members carry controls (the viewer is an owner).
  const m1 = page.getByRole("listitem").filter({ hasText: MEMBER_ONE });
  await expect(m1.getByText("member", { exact: true })).toBeVisible();
  await expect(m1.getByRole("button", { name: "Change role" })).toBeVisible();

  // The share surface is the workspace ADDRESS — `topos login <origin>/<address>`.
  await expect(page.getByText(FOLLOW_LINE).first()).toBeVisible();
});

test("invite lands an invitation row + the notice mail; the owner revokes with one confirm", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/members`);

  // INVITE — owner-only (non-destructive; the invitation seats nobody).
  await page.getByLabel("Invite by email").fill(INVITED);
  await page.getByRole("button", { name: "Invite", exact: true }).click();
  await expect(page.getByRole("status").filter({ hasText: `Invited ${INVITED}` })).toBeVisible();
  const invRow = page.getByRole("listitem").filter({ hasText: INVITED });
  await expect(invRow.getByText("pending")).toBeVisible();
  await expect(invRow.getByText("lapses in 7 days")).toBeVisible();

  // The row is a claim on a FUTURE user — no seat exists for the address.
  const pending = await adminQuery<{ status: string }>(
    `select status from web.invitation where email = $1`,
    [INVITED],
  );
  expect(pending[0]?.status).toBe("pending");
  expect(await seatRole(INVITED)).toBeUndefined();

  // The notice mail carries the ADDRESS, never a token.
  const mail = await latestMail("invite", INVITED);
  expect(mail.text).toContain(FOLLOW_LINE);

  // REVOKE — owner-only: the small confirm panel alone flips the row to revoked and it
  // revalidates away (no re-authentication, nothing further to type).
  await invRow.getByRole("button", { name: "Revoke" }).click();
  await invRow.getByRole("button", { name: "Revoke invitation" }).click();
  await expect(page.getByRole("listitem").filter({ hasText: INVITED })).toHaveCount(0);
  expect(
    (
      await adminQuery<{ status: string }>(`select status from web.invitation where email = $1`, [
        INVITED,
      ])
    )[0]?.status,
  ).toBe("revoked");
});

test("role change: the panel's Save arms in place then commits; the chip + row reflect it", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/members`);

  const row = page.getByRole("listitem").filter({ hasText: MEMBER_ONE });
  await row.getByRole("button", { name: "Change role" }).click();
  await row.getByRole("combobox").selectOption("reviewer");

  // Saving is an in-place confirm — the first click arms ("Save — confirm?"), the second commits.
  // No password stands in the way, and arming alone changes nothing.
  await row.getByRole("button", { name: "Save role" }).click();
  await expect(row.getByRole("button", { name: "Save — confirm?" })).toBeVisible();
  expect(await seatRole(MEMBER_ONE)).toBe("member");

  // Confirm promotes. Wait for the panel to close (the select gone) BEFORE reading the chip, so
  // "reviewer" resolves to the role chip alone — never the open select's option.
  await row.getByRole("button", { name: "Save — confirm?" }).click();
  await expect(row.getByRole("combobox")).toHaveCount(0);
  await expect(row.getByText("reviewer", { exact: true })).toBeVisible();
  expect(await seatRole(MEMBER_ONE)).toBe("reviewer");
});

test("the per-seat Remove confirm arms in place without acting; Cancel, a focus move, and the timeout each disarm", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/members`);
  const row = page.getByRole("listitem").filter({ hasText: MEMBER_TWO });

  // 1) The first click ARMS — the swap is client-only, so nothing reaches the server. The armed
  // submit and its Cancel are both present, and the seat still exists.
  await row.getByRole("button", { name: "Remove", exact: true }).click();
  await expect(row.getByRole("button", { name: "Remove — confirm?" })).toBeVisible();
  await expect(row.getByRole("button", { name: "Cancel" })).toBeVisible();
  expect(await seatRole(MEMBER_TWO)).toBe("member");

  // 2) Cancel disarms back to the resting Remove — and still nothing happened.
  await row.getByRole("button", { name: "Cancel" }).click();
  await expect(row.getByRole("button", { name: "Remove", exact: true })).toBeVisible();
  await expect(row.getByRole("button", { name: "Remove — confirm?" })).toHaveCount(0);
  expect(await seatRole(MEMBER_TWO)).toBe("member");

  // 3) Moving focus away disarms (the blur watcher): arm again, then focus another control.
  await row.getByRole("button", { name: "Remove", exact: true }).click();
  await expect(row.getByRole("button", { name: "Remove — confirm?" })).toBeVisible();
  await page.getByLabel("Invite by email").focus();
  await expect(row.getByRole("button", { name: "Remove — confirm?" })).toHaveCount(0);
  await expect(row.getByRole("button", { name: "Remove", exact: true })).toBeVisible();

  // 4) The ~8s idle timeout disarms on its own — the suite's one long wait.
  await row.getByRole("button", { name: "Remove", exact: true }).click();
  await expect(row.getByRole("button", { name: "Remove — confirm?" })).toBeVisible();
  await page.waitForTimeout(9000);
  await expect(row.getByRole("button", { name: "Remove — confirm?" })).toHaveCount(0);
  await expect(row.getByRole("button", { name: "Remove", exact: true })).toBeVisible();
  expect(await seatRole(MEMBER_TWO)).toBe("member");
});

test("a full arm → confirm removes the seat in one fenced act", async ({ page }) => {
  await theWorkspace();
  await gotoSettled(page, `/members`);

  const row = page.getByRole("listitem").filter({ hasText: MEMBER_TWO });
  await row.getByRole("button", { name: "Remove", exact: true }).click();
  await row.getByRole("button", { name: "Remove — confirm?" }).click();

  // The row revalidates away and the seat row is really gone.
  await expect(page.getByRole("listitem").filter({ hasText: MEMBER_TWO })).toHaveCount(0);
  expect(await seatRole(MEMBER_TWO)).toBeUndefined();
});

test("the sole owner cannot leave — transfer ownership first", async ({ page }) => {
  await theWorkspace();
  await gotoSettled(page, `/members`);

  // Leave is an in-place confirm — arm, then confirm; no password.
  await page.getByRole("button", { name: "Leave workspace", exact: true }).click();
  await page.getByRole("button", { name: "Leave — confirm?" }).click();

  // The honest refusal: the workspace must always have an owner. The seat stands.
  await expect(page.getByText(/the workspace must\s+always have an owner/i)).toBeVisible();
  expect(await seatRole(MEMBER_EMAIL)).toBe("owner");
});

test("a member leaves their own seat; the honest seatless miss follows", async ({ browser }) => {
  await theWorkspace();
  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const page = await context.newPage();
  try {
    // MEMBER_ONE was promoted to reviewer above — any member may leave themselves.
    await signIn(page, MEMBER_ONE);
    await gotoSettled(page, `/members`);
    // A non-owner reads the roster but gets no owner controls.
    await expect(page.getByRole("button", { name: "Change role" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Remove", exact: true })).toHaveCount(0);

    await page.getByRole("button", { name: "Leave workspace", exact: true }).click();
    await page.getByRole("button", { name: "Leave — confirm?" }).click();

    // The action redirects to the door resolver; a now-seatless person gets the house 404.
    await page.waitForURL("**/app");
    await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
    expect(await seatRole(MEMBER_ONE)).toBeUndefined();
  } finally {
    await context.close();
  }
});
