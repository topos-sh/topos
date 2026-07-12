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
  WS,
} from "../fixtures/plane/data.mjs";
import { BASE_URL, E2E_ADMIN_URL, E2E_DATABASE_URL, PLANE_PORT } from "./env";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The members panel over the NEW roster wiring. Two writes, two backends: INVITE is a guarded
 * DATABASE write (`topos_invite` seats an invited member row directly — the proof is the row
 * landing + the address-bearing invite mail record), and REMOVE is an internal-lane VAULT write
 * (the instant-revoke op — the proof is the recorded wire call). The share surface is the workspace
 * ADDRESS: `topos follow <origin>/<address>` (links carry nothing; the roster is the lock — the
 * tokened door + its rotation are gone). Every invitee starts a member; there is no role picker,
 * and inviting is member-level (the database re-runs the invite-policy gate). Only REMOVE and the
 * review-gate toggle are owner-gated in the UI.
 *
 * Identities ride the PLANE SEED: w_roster's owner holds a confirmed OWNER seat, its plain member a
 * confirmed MEMBER seat; w_notowner seats the same owner email as a confirmed MEMBER. HARNESS
 * DISCIPLINE: the DB invite mutates topos_e2e (re-seeded each run); the vault remove records a wire
 * call but never touches the seeded rows.
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
  // No role picker: every invitee starts a member (roles are raised later, on the web).
  await page.getByLabel("Invite by email").fill(email);
  await page.getByRole("button", { name: "Invite", exact: true }).click();
}

test("invite seats an invited MEMBER in the database; the panel shows the seat + the address block", async ({
  page,
}) => {
  await signIn(page, ROSTER_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/settings`);

  // The pre-invite roster: the owner's confirmed seat with its role chip, and the workspace-address
  // paste block — `topos follow <origin>/<address>` + a copy affordance.
  const ownerRow = page.getByRole("listitem").filter({ hasText: ROSTER_OWNER_EMAIL });
  await expect(ownerRow.getByText("owner", { exact: true })).toBeVisible();
  await expect(ownerRow.getByText("confirmed")).toBeVisible();
  // The settings page shows the address block plus its inline copy affordance — assert the first.
  await expect(page.getByText(FOLLOW_LINE).first()).toBeVisible();
  await expect(page.getByRole("button", { name: /copy/i }).first()).toBeVisible();

  // The owner-gated review toggle renders for the confirmed OWNER seat — the positive control for
  // the member-denial assertion in the last test (the guard, not the fixture, decides).
  await expect(page.getByRole("switch")).toBeVisible();

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
  // carries the ADDRESS (a plain slug), never a tokened link.
  // The address is the FULL `<origin>/<name>` form — the same string the follow command pastes.
  const mailA = await inviteMailFor(INVITED_A);
  expect(mailA.address).toBe(`${BASE_URL}/${ROSTER_ADDRESS}`);
  const mailB = await inviteMailFor(INVITED_B);
  expect(mailB.address).toBe(`${BASE_URL}/${ROSTER_ADDRESS}`);
});

test("remove posts the instant-revoke to the vault; the sole owner is never removable", async ({
  page,
}) => {
  await signIn(page, ROSTER_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/settings`);

  const row = page.getByRole("listitem").filter({ hasText: ROSTER_REMOVABLE_EMAIL });
  await expect(row).toBeVisible();
  await row.getByRole("button", { name: "Remove" }).click();

  // The proof is the recorded internal-lane call: the acting owner, a UUID request_id, and the
  // target email. (Harness discipline: the vault mock never edits the seeded rows, so the panel —
  // a DB read — does not reflect the removal mid-suite.)
  await expect.poll(async () => (await recordedRosterRemoves(page)).length).toBeGreaterThan(0);
  const remove = (await recordedRosterRemoves(page)).at(-1);
  expect(remove?.acting).toBe(ROSTER_OWNER_EMAIL);
  expect(remove?.ws).toBe(ROSTER_WS);
  expect(remove?.body.email).toBe(ROSTER_REMOVABLE_EMAIL);
  expect(String(remove?.body.request_id)).toMatch(UUID);

  // The sole owner's seat never offers Remove — the vault would refuse to orphan it.
  const ownerRow = page.getByRole("listitem").filter({ hasText: ROSTER_OWNER_EMAIL });
  await expect(ownerRow.getByRole("button", { name: "Remove" })).toHaveCount(0);
  await expect(ownerRow.getByText("workspace owner")).toBeVisible();
});

test("the workspace page shows the join address block for a member", async ({ page }) => {
  await signIn(page, ROSTER_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}`);
  // Adding a device is `follow <origin>/<address>` — the SAME address hand-off, with the paste-block
  // treatment.
  await expect(page.getByText("Add my device")).toBeVisible();
  // The settings page shows the address block plus its inline copy affordance — assert the first.
  await expect(page.getByText(FOLLOW_LINE).first()).toBeVisible();
  await expect(page.getByRole("button", { name: /copy/i }).first()).toBeVisible();
});

test("a non-owner member reads seats but gets no remove or review-gate controls", async ({
  page,
}) => {
  // Admission rides the confirmed MEMBER seat; any confirmed member reads the roster (and may
  // attempt a member-level invite), but only the owner gets remove controls and the review toggle.
  await signIn(page, ROSTER_MEMBER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/settings`);
  await expect(
    page.getByRole("listitem").filter({ hasText: ROSTER_OWNER_EMAIL }).first(),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "Remove", exact: true })).toHaveCount(0);
  await expect(page.getByRole("switch")).toHaveCount(0);
});

test("a member of a workspace they do not own sees settings without owner controls", async ({
  page,
}) => {
  // w_notowner: the guard admits this email through its confirmed MEMBER seat, but with no owner
  // controls — the honest state, never a crash.
  await signIn(page, ROSTER_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${NOT_OWNER_WS}/settings`);
  await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Remove", exact: true })).toHaveCount(0);
  await expect(page.getByRole("switch")).toHaveCount(0);
});

test("a confirmed MEMBER cannot flip the review gate; a non-member workspace 404s", async ({
  page,
}) => {
  // The suite's default storage state: reviewer@example.com, a confirmed MEMBER (not owner) of
  // ws-e2e. The settings page renders (requireMember), but the review-required toggle must not
  // (the web owner-guard hides it; the database's own owner gate backs the write either way).
  await gotoSettled(page, `/workspaces/${WS}/settings`);
  await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
  await expect(page.getByRole("switch")).toHaveCount(0);

  // No seat in w_roster: the uniform 404 — the settings surface never renders.
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/settings`);
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Settings" })).toHaveCount(0);
});

test("the owner flips the review gate; the database row is the proof", async ({ page }) => {
  await signIn(page, ROSTER_OWNER_EMAIL);
  await gotoSettled(page, `/workspaces/${ROSTER_WS}/settings`);

  const gate = page.getByRole("switch");
  await expect(gate).toBeVisible();
  const before = await gate.getAttribute("aria-checked");
  await gate.click();
  // The write is `topos_set_review_default` — owner-gated IN the database; the row flips and the
  // switch re-renders from the revalidated loader read.
  const expected = before === "true" ? "false" : "true";
  await expect(gate).toHaveAttribute("aria-checked", expected);
  const db = new Client({ connectionString: E2E_ADMIN_URL });
  await db.connect();
  try {
    const { rows } = await db.query(
      `select review_required from plane.workspace_policy where workspace_id = $1`,
      [ROSTER_WS],
    );
    expect(String(rows[0]?.review_required)).toBe(expected === "true" ? "1" : "0");
  } finally {
    await db.end();
  }
});
