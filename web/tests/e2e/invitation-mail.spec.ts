import { expect, test } from "@playwright/test";
import { BASE_URL, E2E_PASSWORD, WORKSPACE_ADDRESS } from "./env";
import { adminQuery, latestMail, theWorkspace } from "./seed";

/**
 * The MAIL-ARMED identity rung, end to end — the ONE spec where seat-binding through the real
 * invitation flow is the subject (everywhere else, seats are seeded arrangement). The suite
 * runs with all five TOPOS_MAIL_SMTP_* armed toward the sink, so `canSend` is true and every
 * "sent" mail's assertable copy lands in `.outbox.jsonl`.
 *
 * Registration is flipped back to `invite_only` for this file (auth.setup opened it for the
 * other specs' identity minting) and RESTORED in afterAll: under the closed knob,
 *  - an UNINVITED sign-up is refused with the ONE constant, non-enumerating string;
 *  - an INVITED sign-up is admitted, its verification mail rides the transport, driving the
 *    mailed URL verifies the address, and ONLY THEN does the pending invitation become a seat
 *    (bindInvitedSeats on the mailbox round-trip) — the invited member lands in the shell.
 */

const REFUSAL = "Sign-up is not open on this server. Ask a member to invite you.";
// Fresh per run: a reused local database must never hand this walk a pre-existing account.
const INVITEE = `invitee-${Date.now().toString(36)}@example.com`;

test.describe.configure({ mode: "serial" });

test.beforeAll(async () => {
  await adminQuery(`update web.workspace set registration = 'invite_only'`);
});

test.afterAll(async () => {
  // The other specs mint throwaway identities through open registration — restore it.
  await adminQuery(`update web.workspace set registration = 'open'`);
});

test("an uninvited sign-up is refused with the constant copy — nothing enumerated", async ({
  browser,
}) => {
  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const page = await context.newPage();
  try {
    await page.goto("/login");
    await page.getByRole("button", { name: "Create an account" }).click();
    await page.getByLabel("Email").fill(`uninvited-${Date.now().toString(36)}@example.com`);
    await page.getByLabel("Password").fill(E2E_PASSWORD);
    await page.getByRole("button", { name: "Create account" }).click();
    await expect(page.getByRole("alert")).toHaveText(REFUSAL);
  } finally {
    await context.close();
  }
});

test("invite → verified sign-up → the seat binds and the member lands in the shell", async ({
  page,
  browser,
}) => {
  const ws = await theWorkspace();

  // The OWNER (the suite's default identity) invites the fresh address from the members page.
  await page.goto(`/workspaces/${ws.id}/members`);
  await page.getByLabel("Invite by email").fill(INVITEE);
  await page.getByRole("button", { name: "Invite", exact: true }).click();
  await expect(page.getByRole("status").filter({ hasText: `Invited ${INVITEE}` })).toBeVisible();
  await expect(
    page.getByRole("listitem").filter({ hasText: INVITEE }).getByText("pending"),
  ).toBeVisible();

  // The notice mail rode the transport carrying the workspace ADDRESS — never a tokened link.
  const notice = await latestMail("invite", INVITEE);
  expect(notice.text).toContain(`topos follow ${BASE_URL}/${WORKSPACE_ADDRESS}`);

  // The INVITEE signs up in their own browser. Registration is invite_only, so this succeeds
  // ONLY because the pending invitation + armed mail admit it.
  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const invitee = await context.newPage();
  try {
    await invitee.goto("/login");
    await invitee.getByRole("button", { name: "Create an account" }).click();
    await invitee.getByLabel("Name").fill("Invited Member");
    await invitee.getByLabel("Email").fill(INVITEE);
    await invitee.getByLabel("Password").fill(E2E_PASSWORD);
    await invitee.getByRole("button", { name: "Create account" }).click();
    // Mail armed ⇒ the form holds at the mailbox hand-off instead of navigating in.
    await expect(invitee.getByText("Check your mailbox to verify your address")).toBeVisible();

    // No seat yet: the account exists, but the invitation binds only after verification.
    const before = await adminQuery<{ n: string }>(
      `select count(*)::text as n from web.seat s join web."user" u on u.id = s.user_id
       where u.email = $1`,
      [INVITEE],
    );
    expect(before[0]?.n).toBe("0");

    // The verification mail landed in the outbox — drive its URL (the mailbox round-trip).
    const mail = await latestMail("auth-verify", INVITEE);
    const url = mail.text.match(/finish joining: (\S+)/)?.[1];
    expect(url, `no verification URL in: ${mail.text}`).toBeTruthy();
    await invitee.goto(url as string);

    // Verified ⇒ the pending invitation became a seat; the member resolves into the shell.
    await invitee.goto("/app");
    await invitee.waitForURL(`**/workspaces/${ws.id}`);
    await expect(invitee.getByRole("banner")).toBeVisible();

    const seat = await adminQuery<{ role: string }>(
      `select s.role from web.seat s join web."user" u on u.id = s.user_id where u.email = $1`,
      [INVITEE],
    );
    expect(seat[0]?.role).toBe("member");
    const invitation = await adminQuery<{ status: string }>(
      `select status from web.invitation where email = $1 order by created_at desc limit 1`,
      [INVITEE],
    );
    expect(invitation[0]?.status).toBe("accepted");
  } finally {
    await context.close();
  }
});
