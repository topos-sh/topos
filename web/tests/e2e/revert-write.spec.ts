import { expect, type Page, test } from "@playwright/test";
import {
  GENESIS_ID,
  R_REVERT_GOOD,
  REVERT_GENERATION,
  REVERT_SKILL,
  REVERT_STALE_SKILL,
  REVIEW_WS,
  REVIEWER_EMAIL,
  SKILL,
  WS,
} from "../fixtures/plane/data.mjs";
import { PLANE_PORT } from "./env";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The browser team-revert over the fixture vault: a reviewer rolls a skill's current back to a
 * known-good ancestor (the exact wire payload asserted), a plain member never sees the control, and
 * a stale generation surfaces the conflict note.
 *
 * The roll-back affordance rides the skill's History tab — one collapsible confirm per NON-current
 * row, owner|reviewer only. The DECIDING flows sign in as REVIEWER_EMAIL, whose confirmed REVIEWER
 * seat on w_review (PLANE_SEED) is both the page admission and the role that renders the control;
 * the member-posture flow rides the suite's default storage state (a confirmed plain MEMBER of
 * ws-e2e).
 *
 * HARNESS DISCIPLINE: every assertion here is a fixture-fed surface (the recorded /__test/calls
 * wire payloads, the control's own success/conflict copy). DB-fed mirrors (the skill header's
 * current short-id, the sidebar badge) are DELIBERATELY unasserted.
 */

test.describe.configure({ mode: "serial" });

/** The canonical (lowercase) UUID form the vault's op-id slot requires. */
const CANONICAL_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

interface RecordedRevertCall {
  route: string;
  method: string;
  path: string;
  ws: string;
  skill: string;
  good: string;
  acting: string;
  body: Record<string, unknown>;
}

async function revertCalls(page: Page, skill: string): Promise<RecordedRevertCall[]> {
  const response = await page.request.get(`http://127.0.0.1:${PLANE_PORT}/__test/calls`);
  const calls: RecordedRevertCall[] = await response.json();
  return calls.filter((c) => c.route === "revert" && c.skill === skill);
}

const historyUrl = (ws: string, skill: string) => `/workspaces/${ws}/skills/${skill}/history`;

test("a reviewer rolls back: the confirm, the success, one exact wire POST", async ({ page }) => {
  await signIn(page, REVIEWER_EMAIL);
  await gotoSettled(page, historyUrl(REVIEW_WS, REVERT_SKILL));

  // The non-current ancestor row carries the collapsible roll-back control; the head does not.
  const summary = page.getByText("Roll back to this version…");
  await expect(summary).toHaveCount(1);
  await summary.click();

  // The confirm step names the honest consequence before firing.
  await expect(
    page.getByText("a forward move, nothing is deleted", { exact: false }),
  ).toBeVisible();
  const rollBack = page.getByRole("button", { name: "Roll back to this version" });
  await expect(rollBack).toBeVisible();
  await rollBack.click();

  // The success copy (the fixture leaves its pointer put, so the mounted control stays and shows it).
  await expect(
    page.getByText("Rolled back — this version's bytes are the team's current version."),
  ).toBeVisible();

  // The exact wire payload: ONE POST, a canonical-UUID request_id, the GOOD target, and the live
  // current generation the history page rendered against — nothing else in the body.
  const calls = await revertCalls(page, REVERT_SKILL);
  expect(calls).toHaveLength(1);
  const call = calls[0] as RecordedRevertCall;
  expect(call.method).toBe("POST");
  expect(call.ws).toBe(REVIEW_WS);
  expect(call.skill).toBe(REVERT_SKILL);
  expect(call.good).toBe(R_REVERT_GOOD);
  expect(call.acting).toBe(REVIEWER_EMAIL);
  expect(String(call.body.request_id)).toMatch(CANONICAL_UUID);
  expect(call.body.good_version_id).toBe(R_REVERT_GOOD);
  expect(call.body.expected_epoch).toBe(REVERT_GENERATION.epoch);
  expect(call.body.expected_seq).toBe(REVERT_GENERATION.seq);
  expect(Object.keys(call.body).sort()).toEqual([
    "expected_epoch",
    "expected_seq",
    "good_version_id",
    "request_id",
  ]);
});

test("a plain member sees no roll-back control on a non-current row", async ({ page }) => {
  // The suite's DEFAULT identity: a confirmed plain MEMBER of ws-e2e (no signIn — the role comes
  // from the guard's directory-roster read). deploy-runbook has a readable genesis ancestor, so
  // there IS a non-current row — the control is simply withheld from a member seat.
  await gotoSettled(page, historyUrl(WS, SKILL));

  const history = page.getByRole("region", { name: "History" });
  await expect(history.getByText(GENESIS_ID.slice(0, 12), { exact: true })).toBeVisible();
  await expect(page.getByText("Roll back to this version…")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Roll back to this version" })).toHaveCount(0);

  // No revert wire call was ever made for this skill.
  expect(await revertCalls(page, SKILL)).toHaveLength(0);
});

test("a stale generation renders the conflict note, nothing rolled back", async ({ page }) => {
  await signIn(page, REVIEWER_EMAIL);
  await gotoSettled(page, historyUrl(REVIEW_WS, REVERT_STALE_SKILL));

  await page.getByText("Roll back to this version…").click();
  const rollBack = page.getByRole("button", { name: "Roll back to this version" });
  await expect(rollBack).toBeVisible();
  await rollBack.click();

  // The fixture's alwaysConflict answers `conflict` — the action derives `conflict` and the control
  // shows the honest moved-pointer note (a reload rebinds against the live current).
  await expect(
    page.getByText("The pointer moved while you were here", { exact: false }),
  ).toBeVisible();
  await expect(
    page.getByText("Rolled back — this version's bytes are the team's current version."),
  ).toHaveCount(0);

  // The refused POST was a real, recorded wire call — nothing was decided.
  expect(await revertCalls(page, REVERT_STALE_SKILL)).toHaveLength(1);
});
