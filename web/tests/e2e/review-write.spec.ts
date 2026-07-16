import { expect, test } from "@playwright/test";
import { GUIDE_MD, SKILL_MD_V1, SKILL_MD_V2 } from "../fixtures/plane/data.mjs";
import { MEMBER_EMAIL, PLANE_INTERNAL_TOKEN, PLANE_PORT } from "./env";
import {
  adminQuery,
  custodyCalls,
  ensureBundle,
  ensureProposal,
  ensureSeatedUser,
  seedCustody,
  theWorkspace,
} from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The browser review decisions — APP-ORCHESTRATED now: the seat gate and four-eyes run against
 * the app's own rows, the vault only CAS-moves the pointer, and the proposal row resolves in
 * the same flow. Five proofs: the approve happy path (ONE exact pointer CAS on the wire, bound
 * to the generation the rendered diff was computed against), the conflict re-show (a moved
 * pointer refuses the stale approve WITHOUT any vault call — the fresh current read catches
 * it), reject-with-reason (the row resolves, the candidate's bytes reclaim best-effort), the
 * four-eyes withhold (Approve gone, Withdraw live), and the comment thread (inert bodies,
 * idempotent replay).
 *
 * The DECIDING flows sign in as a REVIEWER seat; one bundle per mutating flow, so no test
 * shares custody state.
 */

const REVIEWER = "review-writer@example.com";
const REVIEWER_DISPLAY = "review-writer";

test.describe.configure({ mode: "serial" });
test.use({ storageState: { cookies: [], origins: [] } });

interface Arranged {
  currentId: string;
  candidateId: string;
  url: string;
}

/** Stand up one bundle: current v0, an open candidate v1, and the proposal row. */
async function arrange(
  bundleId: string,
  name: string,
  opts: { proposedByEmail: string; protection?: "reviewed" | null },
): Promise<Arranged> {
  const ws = await theWorkspace();
  const proposer = (
    await adminQuery<{ id: string }>(`select id from web."user" where email = $1`, [
      opts.proposedByEmail,
    ])
  )[0]?.id as string;
  await ensureBundle({ id: bundleId, name, protection: opts.protection ?? null });
  const seeded = await seedCustody(
    [
      {
        ws: ws.id,
        bundle: bundleId,
        versions: [
          {
            files: [
              { path: "SKILL.md", content: SKILL_MD_V1 },
              { path: "docs/guide.md", content: GUIDE_MD },
            ],
            message: "genesis",
          },
          {
            files: [
              { path: "SKILL.md", content: SKILL_MD_V2 },
              { path: "docs/guide.md", content: GUIDE_MD },
            ],
            parent: 0,
            author: "dev-device",
            message: "tighten the steps",
          },
        ],
        current: 0,
      },
    ],
    { reset: false }, // additive: this file arranges several bundles side by side
  );
  const currentId = seeded[0]?.versions[0]?.version_id as string;
  const candidateId = seeded[0]?.versions[1]?.version_id as string;
  await adminQuery(`delete from web.proposal where bundle_id = $1`, [bundleId]);
  await ensureProposal({
    id: `p_${bundleId}`,
    bundleId,
    candidateVersionId: candidateId,
    proposedBy: proposer,
    status: "open",
  });
  return {
    currentId,
    candidateId,
    url: `/skills/${name}/proposals/${candidateId}`,
  };
}

test.beforeAll(async () => {
  await ensureSeatedUser(REVIEWER, "reviewer");
  await seedCustody([]); // one clean custody world for this file's bundles
});

test("approve happy path: one exact pointer CAS, then the accepted terminal state", async ({
  page,
}) => {
  const a = await arrange("s_e2e_rw_approve", "rw-approve", { proposedByEmail: MEMBER_EMAIL });
  await signIn(page, REVIEWER);
  await gotoSettled(page, a.url);

  await expect(
    page.getByText("Open — awaiting a reviewer's decision", { exact: false }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Approve — make this current" }).click();

  // The revalidated page renders the accepted terminal state from the same rows the decision
  // moved: banner + resolution panel, no forms, no CLI hand-off.
  await expect(
    page.getByText("Accepted — this candidate is the team's current version."),
  ).toBeVisible();
  await expect(page.getByText(`decided by ${REVIEWER_DISPLAY}`, { exact: false })).toBeVisible();
  await expect(page.getByRole("button", { name: "Approve — make this current" })).toHaveCount(0);
  await expect(page.getByText("Reject with a reason…")).toHaveCount(0);
  await expect(page.getByText("Prefer the CLI?")).toHaveCount(0);

  // The exact wire: ONE pointer CAS, bound to the generation the rendered diff was computed
  // against, attributed with the reviewer's display — nothing else in the body.
  const calls = await custodyCalls({ route: "pointer", bundle: "s_e2e_rw_approve" });
  expect(calls).toHaveLength(1);
  const call = calls[0] as NonNullable<(typeof calls)[0]>;
  expect(call.body.version_id).toBe(a.candidateId);
  expect(call.body.expected_generation).toBe(1);
  expect(call.body.attribution).toBe(REVIEWER_DISPLAY);
  expect(Object.keys(call.body).sort()).toEqual([
    "attribution",
    "expected_generation",
    "version_id",
  ]);

  // Both truths agree: the row resolved and the custody mirror moved.
  const row = await adminQuery<{ status: string }>(
    `select status from web.proposal where id = 'p_s_e2e_rw_approve'`,
  );
  expect(row[0]?.status).toBe("approved");
  const pointer = await adminQuery<{ version_id: string; generation: string }>(
    `select version_id, generation::text from plane.current_pointer where bundle_id = 's_e2e_rw_approve'`,
  );
  expect(pointer[0]?.version_id).toBe(a.candidateId);
  expect(pointer[0]?.generation).toBe("2");
});

test("a moved pointer refuses the stale approve — the fresh current read, no vault CAS", async ({
  page,
}) => {
  const a = await arrange("s_e2e_rw_conflict", "rw-conflict", { proposedByEmail: MEMBER_EMAIL });
  await signIn(page, REVIEWER);
  await gotoSettled(page, a.url);
  await expect(page.getByRole("button", { name: "Approve — make this current" })).toBeVisible();

  // The pointer moves UNDER the open page (a concurrent publish through the custody lane).
  const ws = await theWorkspace();
  const raced = await page.request.post(
    `http://127.0.0.1:${PLANE_PORT}/internal/v1/workspaces/${ws.id}/bundles/s_e2e_rw_conflict/publish`,
    {
      headers: { authorization: `Bearer ${PLANE_INTERNAL_TOKEN}` },
      data: {
        files: [
          {
            path: "SKILL.md",
            mode: "100644",
            content_base64: Buffer.from(`${SKILL_MD_V1}raced\n`, "utf8").toString("base64"),
          },
        ],
        parent: a.currentId,
        attribution: "racer",
        message: "raced publish",
        expected_generation: 1,
      },
    },
  );
  expect(raced.ok()).toBe(true);

  await page.getByRole("button", { name: "Approve — make this current" }).click();
  // The action re-read the live pointer, saw the diff's binding was stale, and refused —
  // honestly, before any vault call.
  await expect(page.getByRole("alert")).toContainText("current moved while you reviewed");
  expect(await custodyCalls({ route: "pointer", bundle: "s_e2e_rw_conflict" })).toHaveLength(0);
});

test("reject needs a reason: blocked empty client-side, then the verbatim reason lands and the bytes reclaim", async ({
  page,
}) => {
  const REASON = "Steps two and three regress the canary gate — see run #482.";
  const a = await arrange("s_e2e_rw_reject", "rw-reject", { proposedByEmail: MEMBER_EMAIL });
  await signIn(page, REVIEWER);
  await gotoSettled(page, a.url);

  // The reject leg hides behind its collapsible — opening it is the deliberate step.
  await page.getByText("Reject with a reason…").click();
  const rejectButton = page.getByRole("button", { name: "Reject proposal" });
  await expect(rejectButton).toBeVisible();

  // An empty reason never leaves the browser: native constraint validation blocks the submit.
  await rejectButton.click();
  const reasonField = page.locator('textarea[name="reason"]');
  await expect(reasonField).toHaveJSProperty("validity.valueMissing", true);
  expect(
    (
      await adminQuery<{ status: string }>(
        `select status from web.proposal where id = 'p_s_e2e_rw_reject'`,
      )
    )[0]?.status,
  ).toBe("open");

  await reasonField.fill(REASON);
  await rejectButton.click();

  // The rejected terminal state, the reason verbatim.
  await expect(page.getByText("Rejected — the resolution below says why.")).toBeVisible();
  await expect(page.getByText(`decided by ${REVIEWER_DISPLAY}`, { exact: false })).toBeVisible();
  await expect(page.getByText(REASON, { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Approve — make this current" })).toHaveCount(0);

  // The row resolved with the verbatim reason…
  const row = await adminQuery<{ status: string; resolved_reason: string }>(
    `select status, resolved_reason from web.proposal where id = 'p_s_e2e_rw_reject'`,
  );
  expect(row[0]?.status).toBe("rejected");
  expect(row[0]?.resolved_reason).toBe(REASON);

  // …and the candidate's bytes reclaim BEST-EFFORT after the row commits (fire-and-forget, so
  // poll the recorded purge; the record — row + resolution — already stands either way).
  await expect
    .poll(async () => (await custodyCalls({ route: "purge", bundle: "s_e2e_rw_reject" })).length)
    .toBeGreaterThan(0);

  // A reload now renders the honest diff-less card in place of the files (keep == read).
  await gotoSettled(page, a.url);
  await expect(
    page.getByText("no longer readable — the server retains only current versions", {
      exact: false,
    }),
  ).toBeVisible();
});

test("four-eyes: the proposer keeps withdraw — Approve withheld, the closed record lands", async ({
  page,
}) => {
  const REASON = "Withdrawing — a cleaner cut of this change is on the way.";
  // The REVIEWER proposed this candidate and the bundle pins 'reviewed': approve is withheld.
  const a = await arrange("s_e2e_rw_self", "rw-self", {
    proposedByEmail: REVIEWER,
    protection: "reviewed",
  });
  await signIn(page, REVIEWER);
  await gotoSettled(page, a.url);

  await expect(page.getByRole("heading", { name: "Your proposal" })).toBeVisible();
  await expect(
    page.getByText("A different owner or reviewer must approve your own proposal.", {
      exact: true,
    }),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "Approve — make this current" })).toHaveCount(0);
  await expect(page.getByText("Reject with a reason…")).toHaveCount(0);
  // Approve was never callable: no pointer CAS exists for this bundle.
  expect(await custodyCalls({ route: "pointer", bundle: "s_e2e_rw_self" })).toHaveLength(0);

  // Withdraw is LIVE: the same resolve under the proposer's own name, verdict `withdrawn`.
  await page.getByText("Withdraw your proposal…").click();
  await page.locator('textarea[name="reason"]').fill(REASON);
  await page.getByRole("button", { name: "Withdraw proposal" }).click();

  await expect(page.getByRole("heading", { name: "Closed without a decision" })).toBeVisible();
  await expect(page.getByText(REASON, { exact: true })).toBeVisible();
  const row = await adminQuery<{ status: string }>(
    `select status from web.proposal where id = 'p_s_e2e_rw_self'`,
  );
  expect(row[0]?.status).toBe("withdrawn");
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

  const a = await arrange("s_e2e_rw_comments", "rw-comments", { proposedByEmail: MEMBER_EMAIL });
  await adminQuery(`delete from web.proposal_comment where bundle_id = 's_e2e_rw_comments'`);
  await signIn(page, REVIEWER);
  await gotoSettled(page, a.url);
  await expect(page.getByText("No comments yet", { exact: false })).toBeVisible();

  // Post — and capture the action POST so the replay below reuses the SAME comment id.
  await page.locator('textarea[name="body"]').fill(BODY);
  const [actionRequest] = await Promise.all([
    page.waitForRequest(
      (r) => r.method() === "POST" && r.url().includes(`/proposals/${a.candidateId}`),
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

  // Replay the captured POST byte-for-byte: the render-minted comment_id is the idempotency
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
