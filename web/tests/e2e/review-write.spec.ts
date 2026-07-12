import { expect, type Page, test } from "@playwright/test";
import {
  APPROVE_GENERATION,
  APPROVE_SKILL,
  CANDIDATE_ID,
  R_APPROVE_CAND,
  R_REJECT_CAND,
  R_SELF_CAND,
  R_STALE_CAND,
  REJECT_GENERATION,
  REJECT_SKILL,
  REVIEW_WS,
  REVIEWER_EMAIL,
  SELF_GENERATION,
  SELF_SKILL,
  SKILL,
  STALE_SKILL,
  WS,
} from "../fixtures/plane/data.mjs";
import { PLANE_PORT } from "./env";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The browser review decisions over the fixture vault — design's six flows: approve happy path,
 * the stale re-show, reject-with-reason, the four-eyes withhold (Approve gone, Withdraw live), the
 * member read-only posture, and the comment thread.
 *
 * The fixture applies the vault's keep == read rule: once a candidate is rejected or its base
 * staled, its meta and blobs 404 (reclaimed) — so every post-decision re-render asserts the honest
 * DIFF-LESS card, never a "fresh diff" the vault could not serve.
 *
 * Identities: the DECIDING flows sign in as REVIEWER_EMAIL, whose confirmed REVIEWER seat on
 * w_review (PLANE_SEED) is both the page admission and the `canDecide` role — one skill per
 * mutating flow, so no test shares fixture state. The read-only + comments flows ride the suite's
 * default storage state (a confirmed plain MEMBER of ws-e2e).
 *
 * HARNESS DISCIPLINE: every assertion here is a fixture-fed surface (the detail read, the current
 * pointer, the recorded /__test/calls wire payloads) or web-tier comment state this spec created.
 * DB-fed mirrors — the sidebar open-proposal badge above all — are DELIBERATELY unasserted.
 */

test.describe.configure({ mode: "serial" });

/** The canonical (lowercase) UUID form the vault's op-id slot requires. */
const CANONICAL_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

interface RecordedReviewCall {
  route: string;
  method: string;
  path: string;
  ws: string;
  skill: string;
  versionId: string;
  acting: string;
  body: Record<string, unknown>;
}

async function reviewCalls(
  page: Page,
  route: "review-approve" | "review-reject",
  versionId: string,
): Promise<RecordedReviewCall[]> {
  const response = await page.request.get(`http://127.0.0.1:${PLANE_PORT}/__test/calls`);
  const calls: RecordedReviewCall[] = await response.json();
  return calls.filter((c) => c.route === route && c.versionId === versionId);
}

const proposalUrl = (skill: string, versionId: string) =>
  `/workspaces/${REVIEW_WS}/skills/${skill}/proposals/${versionId}`;

test("approve happy path: one exact wire POST, then the accepted terminal state", async ({
  page,
}) => {
  await signIn(page, REVIEWER_EMAIL);
  await gotoSettled(page, proposalUrl(APPROVE_SKILL, R_APPROVE_CAND));

  await expect(page.getByText("Open — proposed against the team's current version.")).toBeVisible();
  const approve = page.getByRole("button", { name: "Approve — make this current" });
  await expect(approve).toBeVisible();
  await approve.click();

  // The revalidated page renders the accepted terminal state from the same fixture reads the
  // decision moved: banner + resolution panel, no forms, no CLI hand-off.
  await expect(
    page.getByText("Accepted — this candidate is the team's current version."),
  ).toBeVisible();
  await expect(
    page.getByText("This candidate was approved and is the team's current version."),
  ).toBeVisible();
  await expect(page.getByText(`decided by ${REVIEWER_EMAIL}`, { exact: false })).toBeVisible();
  await expect(page.getByRole("button", { name: "Approve — make this current" })).toHaveCount(0);
  await expect(page.getByText("Reject with a reason…")).toHaveCount(0);
  await expect(page.getByText("Prefer the CLI?")).toHaveCount(0);

  // The exact wire payload: ONE POST, a canonical-UUID request_id, and the generation the rendered
  // diff was computed against — nothing else in the body.
  const calls = await reviewCalls(page, "review-approve", R_APPROVE_CAND);
  expect(calls).toHaveLength(1);
  const call = calls[0] as RecordedReviewCall;
  expect(call.method).toBe("POST");
  expect(call.ws).toBe(REVIEW_WS);
  expect(call.skill).toBe(APPROVE_SKILL);
  expect(call.acting).toBe(REVIEWER_EMAIL);
  expect(String(call.body.request_id)).toMatch(CANONICAL_UUID);
  expect(call.body.expected_epoch).toBe(APPROVE_GENERATION.epoch);
  expect(call.body.expected_seq).toBe(APPROVE_GENERATION.seq);
  expect(Object.keys(call.body).sort()).toEqual(["expected_epoch", "expected_seq", "request_id"]);
});

test("a conflicting approve re-shows the page stale: banner, diff-less card, no forms", async ({
  page,
}) => {
  await signIn(page, REVIEWER_EMAIL);
  await gotoSettled(page, proposalUrl(STALE_SKILL, R_STALE_CAND));

  const approve = page.getByRole("button", { name: "Approve — make this current" });
  await expect(approve).toBeVisible();
  await approve.click();

  // The fixture's alwaysConflict answers `conflict` AND moves the pointer — the revalidated page
  // derives `stale`: the moved-base banner, the honest diff-less card (the vault RECLAIMS a staled
  // candidate's bytes — keep == read), and NO decision forms (a fresh propose is the path).
  await expect(
    page.getByText("it can no longer be approved as-is", { exact: false }),
  ).toBeVisible();
  await expect(
    page.getByText("no longer readable — the server retains only current versions", {
      exact: false,
    }),
  ).toBeVisible();
  await expect(page.getByText("Step three: ship.")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Approve — make this current" })).toHaveCount(0);
  await expect(page.getByText("Reject with a reason…")).toHaveCount(0);
  // The CLI hand-off stays offered, collapsed, with the stale caveat inside.
  const cliSummary = page.getByText("Prefer the CLI?");
  await expect(cliSummary).toBeVisible();
  await cliSummary.click();
  await expect(
    page.getByText("an approve will be refused as stale", { exact: false }),
  ).toBeVisible();

  // The refused POST was a real, recorded wire call — nothing was decided.
  const calls = await reviewCalls(page, "review-approve", R_STALE_CAND);
  expect(calls).toHaveLength(1);
});

test("reject needs a reason: blocked empty client-side, then the verbatim reason lands", async ({
  page,
}) => {
  const REASON = "Steps two and three regress the canary gate — see run #482.";
  await signIn(page, REVIEWER_EMAIL);
  await gotoSettled(page, proposalUrl(REJECT_SKILL, R_REJECT_CAND));

  // The reject leg hides behind its collapsible — opening it is the deliberate step.
  await page.getByText("Reject with a reason…").click();
  const rejectButton = page.getByRole("button", { name: "Reject proposal" });
  await expect(rejectButton).toBeVisible();

  // An empty reason never leaves the browser: the textarea is `required`, so native constraint
  // validation blocks the submit — no wire call is recorded.
  await rejectButton.click();
  const reasonField = page.locator('textarea[name="reason"]');
  await expect(reasonField).toHaveJSProperty("validity.valueMissing", true);
  await page.waitForTimeout(250);
  expect(await reviewCalls(page, "review-reject", R_REJECT_CAND)).toHaveLength(0);

  await reasonField.fill(REASON);
  await rejectButton.click();

  // The rejected terminal state, with the recorded reason displayed verbatim — and the honest
  // diff-less card: the vault reclaims a rejected candidate's bytes (keep == read), so the
  // re-render keeps the RECORD (banner, resolution, thread) and never a diff.
  await expect(page.getByText("Rejected — the resolution below says why.")).toBeVisible();
  await expect(
    page.getByText("This candidate was rejected and never became current."),
  ).toBeVisible();
  await expect(page.getByText(`decided by ${REVIEWER_EMAIL}`, { exact: false })).toBeVisible();
  await expect(page.getByText(REASON, { exact: true })).toBeVisible();
  await expect(
    page.getByText("no longer readable — the server retains only current versions", {
      exact: false,
    }),
  ).toBeVisible();
  await expect(page.getByText("Step three: ship.")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Approve — make this current" })).toHaveCount(0);
  await expect(page.getByText("Reject with a reason…")).toHaveCount(0);

  // The exact wire payload: ONE POST binding the PROPOSAL's base generation + the verbatim reason.
  const calls = await reviewCalls(page, "review-reject", R_REJECT_CAND);
  expect(calls).toHaveLength(1);
  const call = calls[0] as RecordedReviewCall;
  expect(call.method).toBe("POST");
  expect(call.acting).toBe(REVIEWER_EMAIL);
  expect(String(call.body.request_id)).toMatch(CANONICAL_UUID);
  expect(call.body.expected_epoch).toBe(REJECT_GENERATION.epoch);
  expect(call.body.expected_seq).toBe(REJECT_GENERATION.seq);
  expect(call.body.reason).toBe(REASON);
  expect(Object.keys(call.body).sort()).toEqual([
    "expected_epoch",
    "expected_seq",
    "reason",
    "request_id",
  ]);
});

test("four-eyes: the proposer keeps withdraw — Approve withheld, the reject wire body lands", async ({
  page,
}) => {
  const REASON = "Withdrawing — a cleaner cut of this change is on the way.";
  await signIn(page, REVIEWER_EMAIL);
  await gotoSettled(page, proposalUrl(SELF_SKILL, R_SELF_CAND));

  // Still pending — the viewer proposed it and review-required is on. The four-eyes gate applies to
  // APPROVE only, so the decision panel renders with Approve withheld (the inline four-eyes line in
  // its place) and the reject flow live, relabeled as a withdraw.
  await expect(page.getByText("Open — proposed against the team's current version.")).toBeVisible();
  await expect(page.getByRole("heading", { name: "Your proposal" })).toBeVisible();
  await expect(
    page.getByText("A different owner or reviewer must approve your own proposal.", {
      exact: true,
    }),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "Approve — make this current" })).toHaveCount(0);
  await expect(page.getByText("Reject with a reason…")).toHaveCount(0);
  // The CLI hand-off stays offered (the same four-eyes rule holds on the device).
  await expect(page.getByText("Prefer the CLI?")).toBeVisible();
  // Approve was never callable: no approve POST exists for this candidate.
  expect(await reviewCalls(page, "review-approve", R_SELF_CAND)).toHaveLength(0);

  // Withdraw is LIVE: the collapsible opens on the mandatory reason, and submitting posts the SAME
  // reject write the vault serves for a proposer.
  await page.getByText("Withdraw your proposal…").click();
  const withdrawButton = page.getByRole("button", { name: "Withdraw proposal" });
  await expect(withdrawButton).toBeVisible();
  await page.locator('textarea[name="reason"]').fill(REASON);
  await withdrawButton.click();

  // The rejected terminal state (a withdraw IS the stored reject), reason verbatim, and the
  // diff-less card — the vault reclaims the withdrawn candidate's bytes.
  await expect(page.getByText("Rejected — the resolution below says why.")).toBeVisible();
  await expect(page.getByText(`decided by ${REVIEWER_EMAIL}`, { exact: false })).toBeVisible();
  await expect(page.getByText(REASON, { exact: true })).toBeVisible();
  await expect(
    page.getByText("no longer readable — the server retains only current versions", {
      exact: false,
    }),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "Withdraw proposal" })).toHaveCount(0);
  await expect(page.getByText("Withdraw your proposal…")).toHaveCount(0);

  // The exact wire payload: ONE reject POST binding the proposal's base generation + the verbatim
  // reason — the proposer's withdraw is the reject write, nothing bespoke.
  const calls = await reviewCalls(page, "review-reject", R_SELF_CAND);
  expect(calls).toHaveLength(1);
  const call = calls[0] as RecordedReviewCall;
  expect(call.method).toBe("POST");
  expect(call.ws).toBe(REVIEW_WS);
  expect(call.skill).toBe(SELF_SKILL);
  expect(call.acting).toBe(REVIEWER_EMAIL);
  expect(String(call.body.request_id)).toMatch(CANONICAL_UUID);
  expect(call.body.expected_epoch).toBe(SELF_GENERATION.epoch);
  expect(call.body.expected_seq).toBe(SELF_GENERATION.seq);
  expect(call.body.reason).toBe(REASON);
  expect(Object.keys(call.body).sort()).toEqual([
    "expected_epoch",
    "expected_seq",
    "reason",
    "request_id",
  ]);
});

test("a member seat reads only: the note and the exact collapsed CLI hand-off", async ({
  page,
}) => {
  // The suite's DEFAULT identity: a confirmed plain MEMBER of ws-e2e (no signIn — the page's role
  // comes from the guard's directory-roster read, not the fixture).
  await gotoSettled(page, `/workspaces/${WS}/skills/${SKILL}/proposals/${CANDIDATE_ID}`);

  await expect(
    page.getByText("An owner or reviewer seat decides this proposal", { exact: false }),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "Approve — make this current" })).toHaveCount(0);
  await expect(page.getByText("Reject with a reason…")).toHaveCount(0);

  // The CLI hand-off: collapsed summary first, the full device commands only after opening.
  const cliSummary = page.getByText("Prefer the CLI?");
  await expect(cliSummary).toBeVisible();
  const handoffHeading = page.getByRole("heading", { name: "Decide on an enrolled device" });
  await expect(handoffHeading).not.toBeVisible();
  await cliSummary.click();
  await expect(handoffHeading).toBeVisible();
  await expect(
    page.getByText(`topos review ${SKILL}@${CANDIDATE_ID} --approve`, { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText(`topos review ${SKILL}@${CANDIDATE_ID} --reject`, { exact: true }),
  ).toBeVisible();
});

test("comments: a member posts one; script bodies render inert; a replay lands no dupe", async ({
  page,
}) => {
  const BODY = '<script>alert("comment-xss")</script> — reviewers beware';
  let dialogSeen = false;
  page.on("dialog", (dialog) => {
    dialogSeen = true;
    void dialog.dismiss();
  });

  await gotoSettled(page, `/workspaces/${WS}/skills/${SKILL}/proposals/${CANDIDATE_ID}`);
  await expect(page.getByText("No comments yet", { exact: false })).toBeVisible();

  // Post — and capture the action POST so the replay below reuses the SAME comment id.
  await page.locator('textarea[name="body"]').fill(BODY);
  const [actionRequest] = await Promise.all([
    page.waitForRequest(
      (r) => r.method() === "POST" && r.url().includes(`/proposals/${CANDIDATE_ID}`),
    ),
    page.getByRole("button", { name: "Comment", exact: true }).click(),
  ]);

  // The comment renders as a TEXT NODE — the script tag is visible prose, never markup.
  const posted = page.getByRole("listitem").filter({ hasText: "reviewers beware" });
  await expect(posted).toHaveCount(1);
  await expect(posted.getByText(BODY, { exact: true })).toBeVisible();
  expect(dialogSeen).toBe(false);
  const html = await page.content();
  expect(html).not.toContain('<script>alert("comment-xss")');

  // Replay the captured POST byte-for-byte: the same render-minted comment_id is the idempotency
  // key, so the retry lands in the same row — ONE comment, not two.
  const headers = await actionRequest.allHeaders();
  delete headers["content-length"];
  const replay = await page.request.fetch(actionRequest.url(), {
    method: "POST",
    headers,
    data: actionRequest.postDataBuffer() ?? undefined,
  });
  expect(replay.status()).toBe(200);
  await page.reload();
  await expect(page.getByRole("listitem").filter({ hasText: "reviewers beware" })).toHaveCount(1);
});
