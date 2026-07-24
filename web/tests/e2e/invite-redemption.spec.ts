import { randomBytes } from "node:crypto";
import { expect, type Page, test } from "@playwright/test";
import { MEMBER_EMAIL } from "./env";
import {
  adminQuery,
  ensureAccount,
  ensureBundle,
  ensureSeatedUser,
  latestMail,
  mintSession,
  theWorkspace,
} from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The tokened INVITATION page at /invite/:token — the mailed link's landing, redeemed end to
 * end. One link is worth ONE invitation, never an account or a credential: viewing never
 * consumes, accept/decline are explicit POSTs, and the page reshapes itself to the visitor —
 *
 *   - a brand-new person mints their account inline (PASSWORDLESS here, since SMTP is armed and
 *     the mail sign-in rung exists — the token's delivery to the mailbox is the proof);
 *   - a signed-in account whose verified email IS the invited address accepts in one click;
 *   - a signed-out visitor whose address already has an account is sent to sign in and back;
 *   - a session on a DIFFERENT address gets the switch page and never accepts as the wrong one;
 *   - decline is recorded (the inviter sees it); every dead token is ONE constant page that
 *     names neither the workspace nor any email; an already-member arrival just redirects in;
 *   - a skill-hinted invitation frames the skill, subscribes on accept, and lands on it.
 *
 * The invite arranges through two lanes: the real /members form + the device lane (where the
 * INVITE flow is the subject, tests 1 and 8) and superuser SQL that seeds a pending row with a
 * known token (mere arrangement, the redemption being the subject). Each test uses a UNIQUE
 * address and its own fresh signed-out context; the suite's default storage state is the owner.
 */

let seq = 0;
/** A fresh, charset-legal invitee address, unique across the run. */
function uniqueEmail(prefix: string): string {
  seq += 1;
  return `${prefix}-${Date.now().toString(36)}-${seq}@example.com`;
}

/** The claimed owner's user id — the seeded invitations' inviter (realistic attribution). */
async function ownerUserId(): Promise<string> {
  const rows = await adminQuery<{ id: string }>(`select id from web."user" where email = $1`, [
    MEMBER_EMAIL,
  ]);
  const id = rows[0]?.id;
  if (id === undefined) {
    throw new Error("owner user row not found — did auth.setup run?");
  }
  return id;
}

/**
 * Seed ONE pending invitation with a known plaintext token (hashed in Postgres exactly as the
 * app hashes it) and return the origin-rooted redeem path. Arrangement for the redemption tests.
 */
async function seedInvitation(
  email: string,
  opts: { hintBundleId?: string } = {},
): Promise<string> {
  const ws = await theWorkspace();
  const invitedBy = await ownerUserId();
  const token = randomBytes(32).toString("base64url");
  const id = `inv_${randomBytes(12).toString("hex")}`;
  // A stale pending row from a reused local stack would trip the (email, ws) pending-once index.
  await adminQuery(`delete from web.invitation where email = $1 and workspace_id = $2`, [
    email.toLowerCase(),
    ws.id,
  ]);
  const expiresAt = new Date(Date.now() + 7 * 24 * 60 * 60 * 1000).toISOString();
  await adminQuery(
    `insert into web.invitation
       (id, workspace_id, email, role, status, token_sha256, invited_by, hint_bundle_id, expires_at)
     values ($1, $2, $3, 'member', 'pending', sha256(convert_to($4, 'UTF8')), $5, $6, $7)`,
    [id, ws.id, email.toLowerCase(), token, invitedBy, opts.hintBundleId ?? null, expiresAt],
  );
  return `/invite/${token}`;
}

/** Invite through the real /members form as the owner (test 1's arranged path). */
async function inviteViaMembers(page: Page, email: string): Promise<void> {
  await gotoSettled(page, "/members");
  await page.getByLabel("Invite by email").fill(email);
  await page.getByRole("button", { name: "Invite", exact: true }).click();
  await expect(page.getByRole("status").filter({ hasText: `Invited ${email}` })).toBeVisible();
}

/** Fish the tokened invite URL out of the dev outbox and return its origin-rooted path. */
async function fishInvitePath(email: string): Promise<string> {
  const mail = await latestMail("invite", email);
  const match = mail.text.match(/Accept in your browser:\s+(\S+)/);
  if (!match?.[1]) {
    throw new Error(`no invite URL in the mail for ${email}: ${mail.text}`);
  }
  return new URL(match[1]).pathname;
}

async function seatRole(email: string): Promise<string | undefined> {
  const rows = await adminQuery<{ role: string }>(
    `select s.role from web.seat s join web."user" u on u.id = s.user_id where u.email = $1`,
    [email.toLowerCase()],
  );
  return rows[0]?.role;
}

async function invitationStatus(email: string): Promise<string | undefined> {
  const rows = await adminQuery<{ status: string }>(
    `select status from web.invitation where email = $1 order by created_at desc limit 1`,
    [email.toLowerCase()],
  );
  return rows[0]?.status;
}

async function emailVerified(email: string): Promise<boolean | undefined> {
  const rows = await adminQuery<{ email_verified: boolean }>(
    `select email_verified from web."user" where email = $1`,
    [email.toLowerCase()],
  );
  return rows[0]?.email_verified;
}

test("a new person accepts passwordlessly end-to-end", async ({ page, browser }) => {
  const email = uniqueEmail("redeem-new");
  await inviteViaMembers(page, email);
  const invitePath = await fishInvitePath(email);
  const ws = await theWorkspace();

  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const p = await context.newPage();
  try {
    await p.goto(invitePath);

    // The pre-accept summary: who invited, where to, the role, and who the link is for.
    await expect(p.getByRole("heading", { level: 1 })).toContainText(
      `invited you to ${ws.displayName}`,
    );
    await expect(p.getByText(/This invitation is for/)).toBeVisible();
    await expect(p.getByText(email).first()).toBeVisible();
    await expect(p.getByText("member", { exact: true })).toBeVisible();

    // The account-mint arm is PASSWORDLESS — no password field is rendered.
    await expect(p.locator('input[name="password"]')).toHaveCount(0);
    const accept = p.getByRole("button", { name: "Accept and create my account" });
    await expect(accept).toBeVisible();
    await accept.click();

    // The redirect lands in the app, signed in, at the workspace root (the shell chrome renders).
    await p.waitForURL((u) => u.pathname === "/");
    await expect(p.getByRole("banner")).toBeVisible();
  } finally {
    await context.close();
  }

  // The seat is real, and holding the mailed token proved the mailbox (email_verified flips true).
  expect(await seatRole(email)).toBe("member");
  expect(await emailVerified(email)).toBe(true);
});

test("a signed-in matching account accepts in one click", async ({ browser }) => {
  const email = uniqueEmail("redeem-match");
  await ensureAccount(email);
  // The one-click arm is the VERIFIED-match branch — prove the mailbox so it isn't the fence.
  await adminQuery(`update web."user" set email_verified = true where email = $1`, [
    email.toLowerCase(),
  ]);
  const invitePath = await seedInvitation(email);

  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const p = await context.newPage();
  try {
    await signIn(p, email);
    await p.goto(invitePath);
    const accept = p.getByRole("button", { name: "Accept invitation" });
    await expect(accept).toBeVisible();
    await accept.click();
    await p.waitForURL((u) => u.pathname === "/");
  } finally {
    await context.close();
  }
  expect(await seatRole(email)).toBe("member");
});

test("signed out with an existing account: sign-in-first returns", async ({ browser }) => {
  const email = uniqueEmail("redeem-existing");
  await ensureAccount(email);
  await adminQuery(`update web."user" set email_verified = true where email = $1`, [
    email.toLowerCase(),
  ]);
  const invitePath = await seedInvitation(email);

  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const p = await context.newPage();
  try {
    await p.goto(invitePath);
    // The invited address already has an account: sign in first, then return here to accept.
    const signInLink = p.getByRole("link", { name: "Sign in as", exact: false });
    await expect(signInLink).toBeVisible();
    await expect(signInLink).toContainText(email);
    // The link carries THIS page as the sign-in return target.
    await expect(signInLink).toHaveAttribute("href", /^\/login\?next=%2Finvite%2F/);

    // Sign in (the returns-here wiring), come back, and the one-click accept now renders.
    await signIn(p, email);
    await p.goto(invitePath);
    await expect(p.getByRole("button", { name: "Accept invitation" })).toBeVisible();
  } finally {
    await context.close();
  }
});

/**
 * The switch arm signs the CURRENT account out and returns HERE — the page then renders the
 * signed-out account-mint arm for the invited address. (The self-redirect normalizes the
 * single-fetch `.data` suffix and rebuilds only the validated pass-through params, so the
 * post-sign-out reload lands on the real token path.) This test uses a DEDICATED signed-in
 * identity, never the shared owner storage state, so its sign-out can never invalidate the
 * session other tests ride.
 */
test("the wrong account gets the switch page and never accepts", async ({ browser }) => {
  const invitee = uniqueEmail("redeem-wrong"); // a fresh address with NO account
  const other = uniqueEmail("redeem-wrong-session"); // a DIFFERENT address, signed in below
  await ensureAccount(other);
  const invitePath = await seedInvitation(invitee);

  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const p = await context.newPage();
  try {
    // A session on a different address than the invitee gets the switch page — never an accept.
    await signIn(p, other);
    await p.goto(invitePath);
    await expect(p.getByText(/This invitation is for/)).toBeVisible();
    await expect(p.getByText(invitee).first()).toBeVisible();

    const switchBtn = p.getByRole("button", { name: "Sign out and continue as", exact: false });
    await expect(switchBtn).toBeVisible();
    await expect(switchBtn).toContainText(invitee);
    // Nothing was accepted just by landing on the switch page.
    expect(await invitationStatus(invitee)).toBe("pending");
    await switchBtn.click();

    // Intended: signed out now, the invited address has no account, so the account-mint arm
    // renders — never an accept as the current (wrong) account. (Blocked by the bug above.)
    await expect(p.getByRole("button", { name: "Accept and create my account" })).toBeVisible();
  } finally {
    await context.close();
  }

  // Nothing was accepted or seated — the claim is still open.
  expect(await invitationStatus(invitee)).toBe("pending");
  expect(await seatRole(invitee)).toBeUndefined();
});

test("decline is recorded and the inviter sees it", async ({ page, browser }) => {
  const email = uniqueEmail("redeem-decline");
  const invitePath = await seedInvitation(email);

  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const p = await context.newPage();
  try {
    await p.goto(invitePath);
    await p.getByRole("button", { name: "Decline this invitation" }).click();
    await expect(p.getByRole("heading", { name: "Invitation declined" })).toBeVisible();
  } finally {
    await context.close();
  }
  expect(await invitationStatus(email)).toBe("declined");

  // The inviter (the owner) sees the recorded decline on the members page.
  await gotoSettled(page, "/members");
  const row = page.getByRole("listitem").filter({ hasText: email });
  await expect(row.getByText("declined")).toBeVisible();
});

test("an expired link is the one constant page", async ({ browser }) => {
  const email = uniqueEmail("redeem-expired");
  const invitePath = await seedInvitation(email);
  await adminQuery(
    `update web.invitation set expires_at = now() - interval '1 minute' where email = $1`,
    [email.toLowerCase()],
  );
  const ws = await theWorkspace();

  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const p = await context.newPage();
  try {
    await p.goto(invitePath);
    await expect(p.getByRole("heading", { name: "Nothing to accept here" })).toBeVisible();

    // The dead page names NEITHER the workspace nor the invited email.
    const body = (await p.locator("body").innerText()).toLowerCase();
    expect(body).not.toContain(ws.displayName.toLowerCase());
    expect(body).not.toContain(email.toLowerCase());

    // Non-enumeration: a syntactically-valid but invented token is the SAME constant page.
    const invented = `/invite/${randomBytes(32).toString("base64url")}`;
    await p.goto(invented);
    await expect(p.getByRole("heading", { name: "Nothing to accept here" })).toBeVisible();
  } finally {
    await context.close();
  }
});

test("an already-member arrival redirects into the workspace", async ({ browser }) => {
  const email = uniqueEmail("redeem-member");
  await ensureSeatedUser(email, "member");
  const invitePath = await seedInvitation(email);

  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const p = await context.newPage();
  try {
    await signIn(p, email);
    // Already seated: the loader redirects straight into the workspace root, consuming nothing.
    await p.goto(invitePath);
    await p.waitForURL((u) => u.pathname === "/");
    await expect(p.getByRole("banner")).toBeVisible();
  } finally {
    await context.close();
  }
  // A GET stayed a view — the invitation is untouched.
  expect(await invitationStatus(email)).toBe("pending");
});

test("a skill-hinted invitation frames, subscribes, and lands on the skill", async ({
  page,
  browser,
}) => {
  const email = uniqueEmail("redeem-skill");
  const skillName = "redeem-skill-onboarding";
  const bundleId = "bndl-redeem-skill-0001";
  await ensureBundle({ id: bundleId, name: skillName, displayName: skillName });

  // A session credential for the seated owner, so the session-lane invite carries the skill hint.
  const ws = await theWorkspace();
  const owner = await ownerUserId();
  const credential = `cred-redeem-skill-${randomBytes(8).toString("hex")}`;
  await mintSession(
    owner,
    `sn-redeem-skill-${randomBytes(6).toString("hex")}`,
    "skill-inviter",
    credential,
  );

  const resp = await page.request.post(`/api/v1/workspaces/${ws.id}/invitations`, {
    headers: { Authorization: `Bearer ${credential}` },
    data: { emails: [email], skill: skillName },
  });
  expect(resp.ok(), `skill-hinted invite failed: ${resp.status()} ${await resp.text()}`).toBe(true);

  // The hinted mail leads with the skill in its subject.
  const mail = await latestMail("invite", email);
  expect(mail.subject).toContain(skillName);
  const match = mail.text.match(/Accept in your browser:\s+(\S+)/);
  if (!match?.[1]) {
    throw new Error(`no invite URL in the skill-hinted mail for ${email}: ${mail.text}`);
  }
  const invitePath = new URL(match[1]).pathname;

  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const p = await context.newPage();
  try {
    await p.goto(invitePath);
    // The summary names the skill as the first destination.
    await expect(p.getByText("First up:", { exact: false })).toBeVisible();
    await expect(p.getByText(skillName).first()).toBeVisible();

    // Accept passwordlessly → the redirect lands on the hinted skill face.
    await p.getByRole("button", { name: "Accept and create my account" }).click();
    await p.waitForURL((u) => u.pathname === `/skills/${skillName}`);
  } finally {
    await context.close();
  }

  // Seated + prefilled: the accept wrote the hinted skill into the new person's profile.
  expect(await seatRole(email)).toBe("member");
  const line = await adminQuery<{ mode: string }>(
    `select p.mode from web.profile_entry p
       join web."user" u on u.id = p.user_id
     where u.email = $1 and p.bundle_id = $2`,
    [email.toLowerCase(), bundleId],
  );
  expect(line[0]?.mode).toBe("include");
});
