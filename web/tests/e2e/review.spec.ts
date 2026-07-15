import { expect, test } from "@playwright/test";
import {
  GUIDE_MD,
  SKILL_MD_V1,
  SKILL_MD_V2,
  XSS_CONTENT,
  XSS_PATH,
} from "../fixtures/plane/data.mjs";
import { MEMBER_EMAIL } from "./env";
import {
  adminQuery,
  ensureBundle,
  ensureProposal,
  ensureSeatedUser,
  seedCustody,
  theWorkspace,
} from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The proposal page's READ states, viewed with a plain MEMBER seat (never a decider; the write
 * flows live in review-write.spec.ts). Proposals are the app's OWN rows: the status banner and
 * the resolution panel render the row's record; the diff renders the candidate's custody bytes
 * against the LIVE current.
 *
 * The vault retains a candidate's bytes only while trunk-reachable or under an open proposal —
 * a rejected candidate's bytes are RECLAIMED (seeded purged here), so its page must render the
 * full record with the honest DIFF-LESS card, never collapse into an empty shell that hides the
 * resolution.
 */

const SKILL_ID = "s_e2e_review";
const SKILL = "review-runbook";
const READER = "review-reader@example.com";
const CANDIDATE_MESSAGE = "Tighten the deploy steps";

let currentId: string;
let candidateId: string;
let candidateDigest: string;
let rejectedId: string;
let pageUrl: string;

test.use({ storageState: { cookies: [], origins: [] } });

test.beforeAll(async () => {
  const ws = await theWorkspace();
  await ensureSeatedUser(READER, "member");
  const proposer = (
    await adminQuery<{ id: string }>(`select id from web."user" where email = $1`, [MEMBER_EMAIL])
  )[0]?.id as string;

  await ensureBundle({ id: SKILL_ID, name: SKILL });
  const seeded = await seedCustody([
    {
      ws: ws.id,
      bundle: SKILL_ID,
      versions: [
        {
          files: [
            { path: "SKILL.md", content: SKILL_MD_V1 },
            { path: "docs/guide.md", content: GUIDE_MD },
          ],
          message: "genesis",
        },
        {
          // The OPEN candidate: a modified SKILL.md + an added hostile file.
          files: [
            { path: "SKILL.md", content: SKILL_MD_V2 },
            { path: "docs/guide.md", content: GUIDE_MD },
            { path: XSS_PATH, content: XSS_CONTENT },
          ],
          parent: 0,
          author: "dev-device",
          message: CANDIDATE_MESSAGE,
        },
        {
          // The REJECTED candidate, its bytes reclaimed (keep == read).
          files: [{ path: "SKILL.md", content: `${SKILL_MD_V1}rejected line\n` }],
          parent: 0,
          message: "a rejected cut",
          purged: true,
        },
      ],
      current: 0,
    },
  ]);
  currentId = seeded[0]?.versions[0]?.version_id as string;
  candidateId = seeded[0]?.versions[1]?.version_id as string;
  candidateDigest = seeded[0]?.versions[1]?.bundle_digest as string;
  rejectedId = seeded[0]?.versions[2]?.version_id as string;
  pageUrl = `/workspaces/${ws.id}/skills/${SKILL}/proposals/${candidateId}`;

  await adminQuery(`delete from web.proposal where bundle_id = $1`, [SKILL_ID]);
  await ensureProposal({
    id: "p_e2e_review_open",
    bundleId: SKILL_ID,
    candidateVersionId: candidateId,
    proposedBy: proposer,
    status: "open",
  });
  await ensureProposal({
    id: "p_e2e_review_rejected",
    bundleId: SKILL_ID,
    candidateVersionId: rejectedId,
    proposedBy: proposer,
    status: "rejected",
    resolvedBy: proposer,
    resolvedReason: "Superseded by a cleaner run of the same change.",
  });
});

test("renders the banner, header, trust panel, and the diff — read-only for a member seat", async ({
  page,
}) => {
  await signIn(page, READER);
  await gotoSettled(page, pageUrl);

  // The open-status banner is the FIRST thing a reviewer must know.
  await expect(
    page.getByText("Open — awaiting a reviewer's decision", { exact: false }),
  ).toBeVisible();

  // Header: the skill's catalog name, the candidate's message, author + proposer attribution.
  await expect(page.getByRole("heading", { name: SKILL })).toBeVisible();
  await expect(page.getByText(CANDIDATE_MESSAGE)).toBeVisible();
  await expect(page.getByText("authored by dev-device", { exact: false })).toBeVisible();
  await expect(page.getByText("proposed by reviewer", { exact: false })).toBeVisible();

  // Trust panel: the vault-recorded consent digest, sourced to the server.
  await expect(page.getByText(`sha-256:${candidateDigest.slice(0, 12)}…`)).toBeVisible();
  await expect(page.getByText("recorded by the server", { exact: false })).toBeVisible();

  // The rendered diff: the changed-file anchors and the new line inside the unified diff.
  await expect(page.getByRole("navigation", { name: "Changed files" })).toBeVisible();
  await expect(page.getByText("Step three: ship.").first()).toBeVisible();

  // A member seat reads and comments, never decides — stated up front, no forms.
  await expect(
    page.getByText("An owner or reviewer seat decides this proposal", { exact: false }),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "Approve — make this current" })).toHaveCount(0);
  await expect(page.getByText("Reject with a reason…")).toHaveCount(0);

  // The CLI hand-off is DEMOTED on a pending proposal: collapsed, full command only inside.
  const cliSummary = page.getByText("Prefer the CLI?");
  await expect(cliSummary).toBeVisible();
  const handoffHeading = page.getByRole("heading", { name: "Decide on an enrolled device" });
  await expect(handoffHeading).not.toBeVisible();
  await cliSummary.click();
  await expect(handoffHeading).toBeVisible();
  await expect(
    page.getByText(`topos review ${SKILL}@${candidateId} --approve`, { exact: true }),
  ).toBeVisible();
});

test("adversarial contents render inert in the diff", async ({ page }) => {
  let dialogSeen = false;
  page.on("dialog", (dialog) => {
    dialogSeen = true;
    void dialog.dismiss();
  });
  await signIn(page, READER);
  await gotoSettled(page, pageUrl);

  // The payload file's path renders as text (summary + card header)…
  await expect(page.getByText(XSS_PATH).first()).toBeVisible();
  // …the script payload is VISIBLE as escaped text inside the rendered diff…
  await expect(page.getByText('<script>alert("xss-e2e")</script>').first()).toBeVisible();
  // …and never executed.
  expect(dialogSeen).toBe(false);
  const html = await page.content();
  expect(html).not.toContain('<script>alert("xss-e2e")');
});

test("a rejected proposal renders the resolution panel and the diff-less card, no forms", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await signIn(page, READER);
  await gotoSettled(page, `/workspaces/${ws.id}/skills/${SKILL}/proposals/${rejectedId}`);

  await expect(page.getByText("Rejected — the resolution below says why.")).toBeVisible();
  await expect(
    page.getByText("This candidate was rejected and never became current."),
  ).toBeVisible();
  // The row's recorded resolution facts, rendered as text: who, and the verbatim reason.
  await expect(page.getByText("decided by reviewer", { exact: false })).toBeVisible();
  await expect(
    page.getByText("Superseded by a cleaner run of the same change.", { exact: true }),
  ).toBeVisible();
  // The vault reclaimed the rejected candidate's bytes (keep == read): the honest diff-less
  // card — never a diff, never a dead-end shell that hides the record.
  await expect(
    page.getByText("no longer readable — the server retains only current versions", {
      exact: false,
    }),
  ).toBeVisible();
  // Terminal: no decision surface, no CLI hand-off (the decision is done).
  await expect(page.getByRole("button", { name: "Approve — make this current" })).toHaveCount(0);
  await expect(page.getByText("Reject with a reason…")).toHaveCount(0);
  await expect(page.getByText("Prefer the CLI?")).toHaveCount(0);
});

test("a never-proposed candidate URL is the uniform 404", async ({ page }) => {
  const ws = await theWorkspace();
  await signIn(page, READER);
  // currentId is a real, readable version — but no proposal row exists for it, so the proposal
  // URL misses uniformly (the version stays viewable under …/versions/).
  await gotoSettled(page, `/workspaces/${ws.id}/skills/${SKILL}/proposals/${currentId}`);
  await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
});
