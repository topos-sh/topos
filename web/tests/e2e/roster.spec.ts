import { expect, test } from "@playwright/test";
import { BASE_URL, E2E_PASSWORD, MEMBER_EMAIL, WORKSPACE_ADDRESS } from "./env";
import { adminQuery, ensureSeatedUser, latestMail, theWorkspace } from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The members page (/workspaces/:ws/members) over the ONE seat table. Every membership act
 * lives here: INVITE (member-level, mail-armed — an invitation row + the notice mail, never a
 * seat), REVOKE-INVITATION / ROLE CHANGE / REMOVE (owner + step-up), LEAVE (the member's own
 * step-up act), and the LAST-OWNER fence that refuses orphaning the workspace. The proof of
 * every landed act is the `web` row; a wrong password writes NOTHING.
 *
 * The suite's default identity is the claimed OWNER; the two seeded members are this file's
 * own. Serial — the mutating tests keep a deterministic order.
 */

const MEMBER_ONE = "roster-m1@example.com";
const MEMBER_TWO = "roster-m2@example.com";
const INVITED = "dana-roster@example.com";
const FOLLOW_LINE = `topos follow ${BASE_URL}/${WORKSPACE_ADDRESS}`;

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
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/members`);
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

  // The share surface is the workspace ADDRESS — `topos follow <origin>/<address>`.
  await expect(page.getByText(FOLLOW_LINE).first()).toBeVisible();
});

test("invite lands an invitation row + the notice mail; revoke needs owner step-up", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/members`);

  // INVITE — member-level, no step-up (non-destructive; the invitation seats nobody).
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

  // REVOKE — owner + step-up. A wrong password refuses and the row stands…
  await invRow.getByRole("button", { name: "Revoke" }).click();
  await invRow.getByLabel("Confirm with your password").fill("wrong-password-9999");
  await invRow.getByRole("button", { name: "Revoke invitation" }).click();
  await expect(invRow.getByRole("alert")).toContainText("Password check failed");
  expect(
    (
      await adminQuery<{ status: string }>(`select status from web.invitation where email = $1`, [
        INVITED,
      ])
    )[0]?.status,
  ).toBe("pending");

  // …the right one flips it to revoked and the row revalidates away.
  await invRow.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
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

test("role change: a wrong password refuses, the right one promotes; the chip + row reflect it", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/members`);

  const row = page.getByRole("listitem").filter({ hasText: MEMBER_ONE });
  await row.getByRole("button", { name: "Change role" }).click();
  await row.getByRole("combobox").selectOption("reviewer");

  // A wrong password: the step-up refuses inline and NOTHING changes.
  await row.getByLabel("Confirm with your password").fill("not-the-password");
  await row.getByRole("button", { name: "Save role" }).click();
  await expect(row.getByRole("alert")).toContainText("Password check failed");
  expect(await seatRole(MEMBER_ONE)).toBe("member");

  // The right password promotes. Wait for the panel to close (the select gone) BEFORE reading
  // the chip, so "reviewer" resolves to the role chip alone — never the open select's option.
  await row.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await row.getByRole("button", { name: "Save role" }).click();
  await expect(row.getByRole("combobox")).toHaveCount(0);
  await expect(row.getByText("reviewer", { exact: true })).toBeVisible();
  expect(await seatRole(MEMBER_ONE)).toBe("reviewer");
});

test("remove is an owner step-up ceremony; the seat is deleted in one fenced act", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/members`);

  const row = page.getByRole("listitem").filter({ hasText: MEMBER_TWO });
  await row.getByRole("button", { name: "Remove" }).click();
  await row.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await row.getByRole("button", { name: "Remove", exact: true }).last().click();

  // The row revalidates away and the seat row is really gone.
  await expect(page.getByRole("listitem").filter({ hasText: MEMBER_TWO })).toHaveCount(0);
  expect(await seatRole(MEMBER_TWO)).toBeUndefined();
});

test("the sole owner cannot leave — transfer ownership first", async ({ page }) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/members`);

  await page.getByRole("button", { name: "Leave this workspace" }).click();
  await page.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Leave workspace" }).click();

  // The honest refusal: the workspace must always have an owner. The seat stands.
  await expect(page.getByText(/the workspace must\s+always have an owner/i)).toBeVisible();
  expect(await seatRole(MEMBER_EMAIL)).toBe("owner");
});

test("a member leaves their own seat; the honest seatless miss follows", async ({ browser }) => {
  const ws = await theWorkspace();
  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const page = await context.newPage();
  try {
    // MEMBER_ONE was promoted to reviewer above — any member may leave themselves.
    await signIn(page, MEMBER_ONE);
    await gotoSettled(page, `/workspaces/${ws.id}/members`);
    // A non-owner reads the roster but gets no owner controls.
    await expect(page.getByRole("button", { name: "Change role" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Remove", exact: true })).toHaveCount(0);

    await page.getByRole("button", { name: "Leave this workspace" }).click();
    await page.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
    await page.getByRole("button", { name: "Leave workspace" }).click();

    // The action redirects to the index; seatless renders the honest miss pane.
    await page.waitForURL("**/workspaces");
    await expect(page.getByRole("heading", { name: "No seat here" })).toBeVisible();
    expect(await seatRole(MEMBER_ONE)).toBeUndefined();
  } finally {
    await context.close();
  }
});
