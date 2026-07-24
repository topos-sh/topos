import { expect, test } from "@playwright/test";
import { SKILL_MD_V1, SKILL_MD_V2 } from "../fixtures/plane/data.mjs";
import {
  adminQuery,
  ensureBundle,
  ensureSeatedUser,
  mintSession,
  seedCustody,
  theWorkspace,
} from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * The workspace Sessions page (the Settings section's Sessions tab) — a session is
 * user × workspace × installation. It enumerates the workspace's sessions (active AND
 * pending), the version each one last reported, and carries the OWNER arms (approve / reject a
 * pending session, remove any session). Sessions are DELETED, never tombstoned — an ended one
 * simply no longer appears. Ending your OWN sessions stays self-service on /account/sessions.
 *
 * Staleness joins `cli_session.last_seen_at` against the workspace window (set to ONE HOUR
 * here, restored after); per-copy status joins `session_bundle_state` against the custody
 * pointer mirror. The suite's default identity is the claimed OWNER, so every session shows.
 */

const MATE_EMAIL = "sessions-mate@example.com";

const SKILL_A = { id: "s_e2e_sess_a", name: "release-guide" };
const SKILL_B = { id: "s_e2e_sess_b", name: "handbook" };

const SESS_FRESH = "sn_e2e_sess_fresh"; // owner: current + behind, fresh
const SESS_STALE = "sn_e2e_sess_stale"; // owner: stale (past the 1h window)
const SESS_MATE = "sn_e2e_sess_mate"; // seated mate: behind, stale
const SESS_PEND = "sn_e2e_sess_pend"; // seated mate: PENDING — the owner's approval queue

const WINDOW_1H = 3_600_000;

test.describe.configure({ mode: "serial" });

test.beforeAll(async () => {
  const ws = await theWorkspace();
  await adminQuery(`update web.workspace set staleness_window_ms = $1`, [WINDOW_1H]);

  const mate = await ensureSeatedUser(MATE_EMAIL, "member");
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

  // A clean slate for THIS file's sessions (idempotent on a reused database).
  await adminQuery(`delete from web.cli_session where id = any($1::text[])`, [
    [SESS_FRESH, SESS_STALE, SESS_MATE, SESS_PEND],
  ]);

  const session = async (
    id: string,
    userId: string,
    name: string,
    lastSeenAgoMs: number,
    status: "active" | "pending" = "active",
  ) => {
    await mintSession(userId, id, name, `cred-${id}`, status);
    await adminQuery(
      `update web.cli_session set last_seen_at = now() - ($2 || ' milliseconds')::interval
       where id = $1`,
      [id, String(lastSeenAgoMs)],
    );
  };
  const state = async (sessionId: string, bundleId: string, applied: string) => {
    await adminQuery(
      `insert into web.session_bundle_state (session_id, bundle_id, applied_version_id, reported_at)
       values ($1, $2, $3, now() - interval '10 minutes')
       on conflict (session_id, bundle_id) do update
         set applied_version_id = excluded.applied_version_id`,
      [sessionId, bundleId, applied],
    );
  };

  // The owner's fresh session: release-guide is current, handbook is behind.
  await session(SESS_FRESH, owner, "fresh-workstation", 60_000);
  await state(SESS_FRESH, SKILL_A.id, curA as string);
  await state(SESS_FRESH, SKILL_B.id, oldB as string);

  // The owner's second session is stale (2h > the 1h window).
  await session(SESS_STALE, owner, "stale-laptop", 7_200_000);
  await state(SESS_STALE, SKILL_B.id, oldB as string);

  // The seated mate's stale session holds an old copy.
  await session(SESS_MATE, mate.userId, "mates-machine", 7_200_000);
  await state(SESS_MATE, SKILL_A.id, oldA as string);

  // The mate's second login is PENDING — the owner's approval queue.
  await session(SESS_PEND, mate.userId, "mates-new-box", 60_000, "pending");
});

test.afterAll(async () => {
  await adminQuery(`update web.workspace set staleness_window_ms = 604800000`);
});

test("renders every session with the right freshness and per-copy status chips", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/settings/sessions`);
  await expect(page.getByRole("heading", { name: "Sessions", level: 1 })).toBeVisible();

  // It is the Sessions tab of the Settings section: the shared tab header names both tabs and
  // marks Sessions current.
  const tabs = page.getByRole("navigation", { name: "Settings sections" });
  await expect(tabs.getByRole("link", { name: "General" })).toBeVisible();
  await expect(tabs.getByRole("link", { name: "Sessions" })).toHaveAttribute(
    "aria-current",
    "page",
  );

  // The fresh session: release-guide current, handbook behind, a "fresh" liveness chip.
  const fresh = page.getByTestId(`sessions-session-${SESS_FRESH}`);
  await expect(fresh.getByText("current", { exact: true })).toBeVisible();
  await expect(fresh.getByText("behind", { exact: true })).toBeVisible();
  await expect(fresh.getByText("fresh", { exact: true })).toBeVisible();

  // The stale session is past the window.
  const stale = page.getByTestId(`sessions-session-${SESS_STALE}`);
  await expect(stale.getByText("stale", { exact: true })).toBeVisible();
});

test("an over-age session reads expired — the same predicate the guard enforces", async ({
  page,
}) => {
  await theWorkspace();
  // Arm a 1-hour expiry and backdate the stale session's mint past it. The guard already
  // refuses its credential; the page must agree instead of showing a live machine.
  await adminQuery(`update web.workspace set session_max_age_ms = 3600000`);
  await adminQuery(
    `update web.cli_session set created_at = now() - interval '2 hours' where id = $1`,
    [SESS_STALE],
  );
  try {
    await gotoSettled(page, `/settings/sessions`);
    const expired = page.getByTestId(`sessions-session-${SESS_STALE}`);
    await expect(expired.getByText("expired", { exact: true })).toBeVisible();
    await expect(expired.getByText(/Past the session expiry/)).toBeVisible();
    // The meta counts expired sessions out of the active tally and names them (the shared e2e
    // database carries sessions from other specs, so the exact count is not pinned here).
    await expect(page.getByText(/\d+ expired/)).toBeVisible();
    // The policy note states the owner-set expiry where sessions are read.
    await expect(page.getByTestId("sessions-expiry-policy")).toContainText(
      "Sessions here expire after 1 hour",
    );
  } finally {
    // Restore for the rest of this serial file (and later specs).
    await adminQuery(`update web.workspace set session_max_age_ms = null`);
    await adminQuery(`update web.cli_session set created_at = now() where id = $1`, [SESS_STALE]);
  }
});

test("the pending queue: an owner approves a waiting session in place (two-step confirm)", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/settings/sessions`);

  const pending = page.getByTestId(`sessions-pending-${SESS_PEND}`);
  await expect(pending).toBeVisible();
  await expect(pending.getByText("mates-new-box")).toBeVisible();

  // The in-place two-step: the first activation ARMS (performing nothing), the armed submit
  // posts. After approval the session moves into the active list.
  await pending.getByRole("button", { name: "Approve", exact: true }).click();
  await pending.getByRole("button", { name: "Approve — confirm?" }).click();
  await expect(page.getByTestId(`sessions-session-${SESS_PEND}`)).toBeVisible();

  const rows = await adminQuery<{ status: string }>(
    `select status from web.cli_session where id = $1`,
    [SESS_PEND],
  );
  expect(rows[0]?.status).toBe("active");
});

test("owner Remove ends a session; the row is deleted, its reported state with it", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/settings/sessions`);

  // No global sign-out arm anywhere — ending your own sessions is self-service on the account
  // page; this page carries the OWNER arms.
  await expect(page.getByRole("link", { name: "Your sessions", exact: true })).toBeVisible();

  // Remove the mate's session: two-step confirm, then the card is gone and so is the row +
  // its reported state (bytes on the machine stay — the page copy says so).
  const mate = page.getByTestId(`sessions-session-${SESS_MATE}`);
  await mate.getByRole("button", { name: "Remove", exact: true }).click();
  await mate.getByRole("button", { name: "Remove — confirm?" }).click();
  await expect(page.getByTestId(`sessions-session-${SESS_MATE}`)).toHaveCount(0);

  const rows = await adminQuery<{ n: string }>(
    `select count(*)::text as n from web.cli_session where id = $1`,
    [SESS_MATE],
  );
  expect(rows[0]?.n).toBe("0");
  const state = await adminQuery<{ n: string }>(
    `select count(*)::text as n from web.session_bundle_state where session_id = $1`,
    [SESS_MATE],
  );
  expect(state[0]?.n).toBe("0");

  // The audit trail keeps the cause-tagged record (deleted never tombstoned — history is audit).
  const audit = await adminQuery<{ n: string }>(
    `select count(*)::text as n from web.audit_event
     where kind = 'session_ended' and subject = $1`,
    [SESS_MATE],
  );
  expect(Number(audit[0]?.n)).toBeGreaterThanOrEqual(1);
});

test("the status chips carry a focusable, hover/focus-only tooltip explainer", async ({ page }) => {
  await theWorkspace();
  await gotoSettled(page, `/settings/sessions`);

  // The freshness chip's tooltip trigger is a REAL control (keyboard-reachable), marked cursor-help
  // — the reading legend rides the chips themselves, not a separate explainer section.
  const fresh = page.getByTestId(`sessions-session-${SESS_FRESH}`);
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
