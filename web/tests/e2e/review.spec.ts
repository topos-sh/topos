import { expect, test } from "@playwright/test";
import {
  BINARY_MARKER,
  CANDIDATE_ID,
  CURRENT_ID,
  MOVED_ID,
  NOTOPEN_ID,
  SKILL,
  WS,
  XSS_PATH,
} from "../fixtures/plane/data.mjs";
import { gotoSettled } from "./sign-in";

const PAGE_URL = `/workspaces/${WS}/skills/${SKILL}/proposals/${CANDIDATE_ID}`;
const APPROVE_CMD = `topos review ${SKILL}@${CANDIDATE_ID} --approve`;
const DIFF_CMD = `topos diff ${SKILL}@${CANDIDATE_ID}`;

/**
 * The proposal page's READ states, viewed with the suite's default identity — a confirmed plain
 * MEMBER seat on ws-e2e (never a decider; the write flows live in review-write.spec.ts under the
 * reviewer identity). Every state is fixture-fed: the page REQUIRES the proposal-detail read, so
 * the status banner, the proposer line, and the terminal resolution panel all render the fixture's
 * proposalMeta — and a candidate with NO proposal row is the uniform 404 (the version stays
 * viewable under `…/versions/[versionId]`).
 *
 * The fixture applies the vault's keep == read rule to version metas and blobs: a rejected or
 * staled candidate's bytes 404 (reclaimed), so those pages must render the full state surface with
 * the honest DIFF-LESS card — never collapse into an empty "version isn't available" shell that
 * hides the resolution and the comments.
 */

test.describe("the rendered review page", () => {
  test("renders the banner, header, trust panel, and every card kind", async ({ page }) => {
    await page.goto(PAGE_URL);

    // The open-status banner is the FIRST thing a reviewer must know (fixture-fed detail).
    await expect(
      page.getByText("Open — proposed against the team's current version."),
    ).toBeVisible();

    // Header + message (the skill's catalog name; the candidate short id rides beside it) + the
    // detail's proposer disclosure.
    await expect(page.getByRole("heading", { name: SKILL })).toBeVisible();
    await expect(page.getByText("Tighten the deploy steps")).toBeVisible();
    await expect(page.getByText("proposed by dev-bbbb2222", { exact: false })).toBeVisible();

    // Trust panel: the vault-recorded value, sourced to the vault. Never an epoch/seq.
    await expect(page.getByText(`sha-256:${"f4".repeat(6)}…`)).toBeVisible();
    await expect(page.getByText("recorded by the server", { exact: false })).toBeVisible();

    // File summary anchors.
    await expect(page.getByRole("navigation", { name: "Changed files" })).toBeVisible();

    // Modified text file: the new line is IN the rendered diff.
    await expect(page.getByText("Step three: ship.").first()).toBeVisible();

    // Mode-only card.
    await expect(page.getByText("mode 100644 → 100755").first()).toBeVisible();

    // Moved card: both paths, no content rendered.
    await expect(page.getByText("docs/old-name.md → docs/new-name.md")).toBeVisible();

    // Binary card.
    await expect(page.getByText("Binary file changed")).toBeVisible();

    // Too-large card names the device-side diff command.
    await expect(page.getByText(DIFF_CMD).first()).toBeVisible();

    // Per-file fetch failure degrades that card only — the page rendered around it.
    await expect(
      page.getByText("Couldn't fetch this file's bytes", { exact: false }),
    ).toBeVisible();

    // Deleted file appears in the summary.
    await expect(page.getByText("notes/removed.md").first()).toBeVisible();

    // The CLI hand-off is DEMOTED on a pending proposal: a collapsed <details> at the bottom —
    // the full-hash command appears only after opening it.
    const cliSummary = page.getByText("Prefer the CLI?");
    await expect(cliSummary).toBeVisible();
    const handoffHeading = page.getByRole("heading", { name: "Decide on an enrolled device" });
    await expect(handoffHeading).not.toBeVisible();
    await cliSummary.click();
    await expect(handoffHeading).toBeVisible();
    await expect(page.getByText(APPROVE_CMD, { exact: true })).toBeVisible();
  });

  test("adversarial contents render inert; binary bytes never ship", async ({ page }) => {
    let dialogSeen = false;
    page.on("dialog", (dialog) => {
      dialogSeen = true;
      void dialog.dismiss();
    });
    await page.goto(PAGE_URL);
    // The payload file's path renders as text (summary + card header).
    await expect(page.getByText(XSS_PATH).first()).toBeVisible();
    // The script payload is VISIBLE as escaped text inside the rendered diff…
    await expect(page.getByText('<script>alert("xss-e2e")</script>').first()).toBeVisible();
    // …and never executed.
    expect(dialogSeen).toBe(false);
    const html = await page.content();
    expect(html).not.toContain('<script>alert("xss-e2e")');
    expect(html).not.toContain(BINARY_MARKER);
  });

  test("a rejected proposal renders the resolution panel and the diff-less card, no forms", async ({
    page,
  }) => {
    await gotoSettled(page, `/workspaces/${WS}/skills/${SKILL}/proposals/${NOTOPEN_ID}`);
    await expect(page.getByText("Rejected — the resolution below says why.")).toBeVisible();
    await expect(
      page.getByText("This candidate was rejected and never became current."),
    ).toBeVisible();
    // The vault's recorded resolution facts, rendered as text: who, and the verbatim reason.
    await expect(page.getByText("decided by dev-aaaa1111", { exact: false })).toBeVisible();
    await expect(
      page.getByText("Superseded by a cleaner run of the same change.", { exact: true }),
    ).toBeVisible();
    // The vault reclaims a rejected candidate's bytes (keep == read), so the page renders the
    // honest diff-less card — never a diff, never the old "version isn't available" dead end.
    await expect(
      page.getByText("no longer readable — the server retains only current versions", {
        exact: false,
      }),
    ).toBeVisible();
    await expect(page.getByText("Step three: ship.")).toHaveCount(0);
    // Terminal: no decision surface, no CLI hand-off (the decision is done).
    await expect(page.getByRole("button", { name: "Approve — make this current" })).toHaveCount(0);
    await expect(page.getByText("Reject with a reason…")).toHaveCount(0);
    await expect(page.getByText("Prefer the CLI?")).toHaveCount(0);
  });

  test("an open proposal whose base moved renders the stale banner and the diff-less card", async ({
    page,
  }) => {
    await gotoSettled(page, `/workspaces/${WS}/skills/${SKILL}/proposals/${MOVED_ID}`);
    // The stale banner (its unique clause — the collapsed CLI hand-off repeats the lead-in).
    await expect(
      page.getByText("it can no longer be approved as-is", { exact: false }),
    ).toBeVisible();
    // A staled candidate is RECLAIMED by the vault (readable only while trunk-reachable or an open
    // non-stale proposal), so no diff renders — the honest diff-less card does.
    await expect(
      page.getByText("no longer readable — the server retains only current versions", {
        exact: false,
      }),
    ).toBeVisible();
    await expect(page.getByText("Step three: ship.")).toHaveCount(0);
    // No decision forms on a stale proposal; the CLI hand-off stays, collapsed.
    await expect(page.getByRole("button", { name: "Approve — make this current" })).toHaveCount(0);
    await expect(page.getByText("Prefer the CLI?")).toBeVisible();
  });

  test("a never-proposed candidate URL is the uniform 404", async ({ page }) => {
    // CURRENT_ID is a real, readable version — but no proposal row exists for it, so the proposal
    // URL misses uniformly (the version stays viewable under …/versions/).
    await gotoSettled(page, `/workspaces/${WS}/skills/${SKILL}/proposals/${CURRENT_ID}`);
    await expect(page.getByRole("heading", { name: "Not found" })).toBeVisible();
    await expect(page.getByRole("heading", { name: SKILL })).toHaveCount(0);
  });
});
