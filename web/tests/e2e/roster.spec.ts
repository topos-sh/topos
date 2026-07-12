import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { expect, type Page, test } from "@playwright/test";
import { Client } from "pg";
import {
  NOT_OWNER_WS,
  ROSTER_ADDRESS,
  ROSTER_MEMBER_EMAIL,
  ROSTER_OWNER_EMAIL,
  ROSTER_REMOVABLE_EMAIL,
  ROSTER_WS,
} from "../fixtures/plane/data.mjs";
import { BASE_URL, E2E_DATABASE_URL, E2E_PASSWORD, PLANE_PORT } from "./env";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The members page (/workspaces/:ws/members) over the roster wiring. The settings page now only
 * LINKS here; every membership act lives on this page. Three write backends:
 *  - INVITE is a guarded DATABASE write (`topos_invite` seats an invited member row directly).
 *  - REMOVE is an internal-lane VAULT write (the instant-revoke op), NOW behind a step-up.
 *  - ROLE CHANGE + LEAVE are guarded DATABASE writes (`topos_set_member_role` / `topos_leave_workspace`),
 *    also behind a step-up.
 * The share surface is the workspace ADDRESS: `topos follow <origin>/<address>` (links carry
 * nothing; the roster is the lock). Every invitee starts a member; roles are raised here.
 *
 * Identities ride the PLANE SEED: w_roster's owner holds a confirmed OWNER seat, its plain members
 * confirmed MEMBER seats; w_notowner seats the same owner email as a confirmed MEMBER. HARNESS
 * DISCIPLINE: the DB invite/role/leave writes mutate topos_e2e (re-seeded each run); the vault remove
 * records a wire call but never touches the seeded rows. The MUTATING tests (role change; leave) run
 * LAST and target seats nothing after them depends on.
 */

const HERE = path.dirname(fileURLToPath(import.meta.url));
const INVITE_EMAILS_FILE = path.resolve(HERE, "..", "..", ".invite-emails.jsonl");
const INVITED_A = "dana@example.com";
const INVITED_B = "erin@example.com";
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const FOLLOW_LINE = `topos follow ${BASE_URL}/${ROSTER_ADDRESS}`;

test.describe.configure({ mode: "serial" });

test.beforeAll(async () => {
  await fs.rm(INVITE_EMAILS_FILE, { force: true });
});

async function seatOf(email: string): Promise<{ role: string; status: string } | undefined> {
  const db = new Client({ connectionString: E2E_DATABASE_URL });
  await db.connect();
  try {
    const { rows } = await db.query(
      `select role, status from plane.workspace_member where workspace_id = $1 and principal = $2`,
      [ROSTER_WS, email],
    );
    return rows[0];
  } finally {
    await db.end();
  }
}

async function inviteMailFor(email: string): Promise<{ to: string; address: string }> {
  for (let attempt = 0; attempt < 50; attempt++) {
    try {
      const raw = await fs.readFile(INVITE_EMAILS_FILE, "utf8");
      const lines = raw.split("\n").filter((line) => line.trim().length > 0);
      for (let i = lines.length - 1; i >= 0; i--) {
        const entry = JSON.parse(lines[i] as string) as { to: string; address: string };
        if (entry.to === email) return entry;
      }
    } catch {
      // File not written yet — keep polling.
    }
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
  throw new Error(`no invite mail record for ${email} appeared in ${INVITE_EMAILS_FILE}`);
}

async function recordedRosterRemoves(
  page: Page,
): Promise<{ route: string; ws: string; acting: string; body: Record<string, unknown> }[]> {
  const response = await page.request.get(`http://127.0.0.1:${PLANE_PORT}/__test/calls`);
  const calls: { route: string; ws: string; acting: string; body: Record<string, unknown> }[] =
    await response.json();
  return calls.filter((c) => c.route === "roster-remove");
}

async function invite(page: Page, email: string): Promise<void> {
  // No role picker: every invitee starts a member (roles are raised here later, on the web).
  await page.getByLabel("Invite by email").fill(email);
  await page.getByRole("button", { name: "Invite", exact: true }).click();
}

test("invite seats an invited MEMBER in the database; the panel shows the seat + the address block", async ({
  page,
}) => {
  await signIn(page, ROSTER_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/members`);

  // The pre-invite roster: the owner's confirmed seat with its role chip, and the workspace-address
  // paste block — `topos follow <origin>/<address>` + a copy affordance.
  const ownerRow = page.getByRole("listitem").filter({ hasText: ROSTER_OWNER_EMAIL });
  await expect(ownerRow.getByText("owner", { exact: true })).toBeVisible();
  await expect(ownerRow.getByText("confirmed")).toBeVisible();
  await expect(page.getByText(FOLLOW_LINE).first()).toBeVisible();
  await expect(page.getByRole("button", { name: /copy/i }).first()).toBeVisible();

  // Invite a member: the DB row lands (member / invited) and the panel confirms it.
  await invite(page, INVITED_A);
  await expect(page.getByRole("status").filter({ hasText: `Invited ${INVITED_A}` })).toBeVisible();
  expect(await seatOf(INVITED_A)).toEqual({ role: "member", status: "invited" });
  const rowA = page.getByRole("listitem").filter({ hasText: INVITED_A });
  await expect(rowA.getByText("member", { exact: true })).toBeVisible();
  await expect(rowA.getByText("invited")).toBeVisible();

  // A second invite: the database carries it too (no role distinction — member).
  await invite(page, INVITED_B);
  await expect(page.getByRole("status").filter({ hasText: `Invited ${INVITED_B}` })).toBeVisible();
  expect(await seatOf(INVITED_B)).toEqual({ role: "member", status: "invited" });
});

test("the invite mail rode the dev transport carrying the workspace ADDRESS", async () => {
  // Never a real send: APP_ENV=test appends {to,address,...} to .invite-emails.jsonl — the notice
  // carries the ADDRESS (the full `<origin>/<name>` form), never a tokened link.
  const mailA = await inviteMailFor(INVITED_A);
  expect(mailA.address).toBe(`${BASE_URL}/${ROSTER_ADDRESS}`);
  const mailB = await inviteMailFor(INVITED_B);
  expect(mailB.address).toBe(`${BASE_URL}/${ROSTER_ADDRESS}`);
});

test("remove is a step-up-gated instant revoke posted to the vault; the sole owner is never removable", async ({
  page,
}) => {
  await signIn(page, ROSTER_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/members`);

  const row = page.getByRole("listitem").filter({ hasText: ROSTER_REMOVABLE_EMAIL });
  // Expand the confirm panel, re-enter the acting owner's password, then confirm the removal.
  await row.getByRole("button", { name: "Remove" }).click();
  await row.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await row.getByRole("button", { name: "Remove", exact: true }).click();

  // The proof is the recorded internal-lane call: the acting owner, a UUID request_id, the target
  // email. (Harness discipline: the vault mock never edits the seeded rows, so the panel — a DB read
  // — does not reflect the removal mid-suite.)
  await expect.poll(async () => (await recordedRosterRemoves(page)).length).toBeGreaterThan(0);
  const remove = (await recordedRosterRemoves(page)).at(-1);
  expect(remove?.acting).toBe(ROSTER_OWNER_EMAIL);
  expect(remove?.ws).toBe(ROSTER_WS);
  expect(remove?.body.email).toBe(ROSTER_REMOVABLE_EMAIL);
  expect(String(remove?.body.request_id)).toMatch(UUID);

  // The sole owner's seat carries no controls at all — the vault would refuse to orphan it.
  const ownerRow = page.getByRole("listitem").filter({ hasText: ROSTER_OWNER_EMAIL });
  await expect(ownerRow.getByRole("button", { name: "Remove" })).toHaveCount(0);
  await expect(ownerRow.getByRole("button", { name: "Change role" })).toHaveCount(0);
  await expect(ownerRow.getByText("workspace owner")).toBeVisible();
});

test("the workspace page shows the join address block for a member", async ({ page }) => {
  await signIn(page, ROSTER_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}`);
  // Adding a device is `follow <origin>/<address>` — the SAME address hand-off, with the paste-block
  // treatment.
  await expect(page.getByText("Add my device")).toBeVisible();
  await expect(page.getByText(FOLLOW_LINE).first()).toBeVisible();
  await expect(page.getByRole("button", { name: /copy/i }).first()).toBeVisible();
});

test("a non-owner member reads seats but gets no owner controls (and can still leave)", async ({
  page,
}) => {
  // Admission rides the confirmed MEMBER seat; any confirmed member reads the roster (and may attempt
  // a member-level invite), but only the owner gets remove/role controls.
  await signIn(page, ROSTER_MEMBER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/members`);
  await expect(
    page.getByRole("listitem").filter({ hasText: ROSTER_OWNER_EMAIL }).first(),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "Remove", exact: true })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Change role", exact: true })).toHaveCount(0);
  // The self-serve leave ceremony is theirs, though.
  await expect(page.getByRole("button", { name: "Leave this workspace" })).toBeVisible();
});

test("a non-member gets the uniform 404 on the members page", async ({ page }) => {
  // The suite's default storage state (reviewer@example.com) holds NO seat in w_roster: the members
  // surface never renders — the house miss, never a 403 that would confirm the workspace exists.
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/members`);
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Members" })).toHaveCount(0);
});

test("a member of a workspace they do not own sees the members page without owner controls", async ({
  page,
}) => {
  // w_notowner: the guard admits this email through its confirmed MEMBER seat, but with no owner
  // controls — the honest state, never a crash.
  await signIn(page, ROSTER_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${NOT_OWNER_WS}/members`);
  // The page-header h1 (level 1) — distinct from the section's "Members" sub-heading (h2).
  await expect(page.getByRole("heading", { name: "Members", level: 1 })).toBeVisible();
  await expect(page.getByRole("button", { name: "Remove", exact: true })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Change role", exact: true })).toHaveCount(0);
});

test("role change: a wrong password refuses, the right password promotes; the chip + DB reflect it", async ({
  page,
}) => {
  await signIn(page, ROSTER_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/members`);

  const row = page.getByRole("listitem").filter({ hasText: ROSTER_MEMBER_EMAIL });
  await row.getByRole("button", { name: "Change role" }).click();
  await row.getByRole("combobox").selectOption("reviewer");

  // A wrong password: the step-up refuses inline and NOTHING changes.
  await row.getByLabel("Confirm with your password").fill("not-the-password");
  await row.getByRole("button", { name: "Save role" }).click();
  await expect(row.getByRole("alert")).toContainText("Password check failed");
  expect(await seatOf(ROSTER_MEMBER_EMAIL)).toEqual({ role: "member", status: "confirmed" });

  // The right password promotes. Wait for the panel to close (the select gone) BEFORE reading the
  // chip, so "reviewer" resolves to the role chip alone — never the still-open select's option.
  await row.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await row.getByRole("button", { name: "Save role" }).click();
  await expect(row.getByRole("combobox")).toHaveCount(0);
  await expect(row.getByText("reviewer", { exact: true })).toBeVisible();
  expect(await seatOf(ROSTER_MEMBER_EMAIL)).toEqual({ role: "reviewer", status: "confirmed" });
});

test("leave is refused for the sole owner — transfer ownership first", async ({ page }) => {
  await signIn(page, ROSTER_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/members`);

  await page.getByRole("button", { name: "Leave this workspace" }).click();
  await page.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Leave workspace" }).click();

  // The honest refusal: a sole owner can't orphan the workspace. No redirect — the seat stands.
  await expect(page.getByText(/the workspace must always have an owner/i)).toBeVisible();
  expect(await seatOf(ROSTER_OWNER_EMAIL)).toEqual({ role: "owner", status: "confirmed" });
});

test("leave: a member leaves; the workspace drops off their rail", async ({ page }) => {
  // A confirmed member leaves through the step-up ceremony; the seat is deleted and the workspace
  // disappears from the index. Runs LAST — carol's seat is nothing later depends on.
  await signIn(page, ROSTER_REMOVABLE_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/members`);

  await page.getByRole("button", { name: "Leave this workspace" }).click();
  await page.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Leave workspace" }).click();

  // The action redirects to the index (the fetcher follows it), and the workspace row is gone.
  await page.waitForURL("**/workspaces");
  await expect(page.getByRole("main").locator(`a[href="/workspaces/${ROSTER_WS}"]`)).toHaveCount(0);
  // The seat is really gone from the directory roster.
  expect(await seatOf(ROSTER_REMOVABLE_EMAIL)).toBeUndefined();
});
