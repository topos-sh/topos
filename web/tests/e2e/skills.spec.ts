import { expect, test } from "@playwright/test";
import { DEPLOY_SH, GUIDE_MD, SKILL_MD_V1, SKILL_MD_V2 } from "../fixtures/plane/data.mjs";
import { MEMBER_EMAIL } from "./env";
import { adminQuery, ensureBundle, ensureProposal, seedCustody, theWorkspace } from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * The skill page's TABBED layout. Opening a skill lands on the Current tab — the current
 * version's files + doc preview, inline — with Proposals and History as sibling ROUTES reached
 * from the tab bar. The URL keys on the CATALOG NAME; every custody read keys on the immutable
 * bundle id the catalog row resolves. Proposals are the app's OWN rows now, so the tab badge
 * and the open list agree by construction — they read the same table.
 */

const SKILL_ID = "s_e2e_skills";
const SKILL = "deploy-runbook";

let currentId: string;
let candidateId: string;

test.beforeAll(async () => {
  const ws = await theWorkspace();
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
            { path: "scripts/deploy.sh", mode: "100755", content: DEPLOY_SH },
          ],
          message: "genesis",
        },
        {
          files: [
            { path: "SKILL.md", content: SKILL_MD_V2 },
            { path: "docs/guide.md", content: GUIDE_MD },
            { path: "scripts/deploy.sh", mode: "100755", content: DEPLOY_SH },
          ],
          parent: 0,
          message: "tighten the steps",
        },
        {
          // The OPEN candidate — committed, never pointed at.
          files: [
            { path: "SKILL.md", content: `${SKILL_MD_V2}\nStep four: verify.\n` },
            { path: "docs/guide.md", content: GUIDE_MD },
          ],
          parent: 1,
          message: "add a verify step",
        },
      ],
      current: 1,
      generation: 2,
    },
  ]);
  currentId = seeded[0]?.versions[1]?.version_id as string;
  candidateId = seeded[0]?.versions[2]?.version_id as string;

  const owner = (
    await adminQuery<{ id: string }>(`select id from web."user" where email = $1`, [MEMBER_EMAIL])
  )[0]?.id as string;
  await adminQuery(`delete from web.proposal where bundle_id = $1`, [SKILL_ID]);
  await ensureProposal({
    id: "p_e2e_skills_open",
    bundleId: SKILL_ID,
    candidateVersionId: candidateId,
    proposedBy: owner,
    status: "open",
  });
});

test("the skill page opens on the Current tab: file listing, doc preview, and the tab bar", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/skills/${SKILL}`);

  // The header is the skill's catalog name over the mono locator line.
  await expect(page.getByRole("heading", { name: SKILL })).toBeVisible();
  await expect(page.getByText(`current ${currentId.slice(0, 12)}`)).toBeVisible();

  // Current is the default view: the listing + doc preview render INLINE (no click-through).
  // The root SKILL.md renders as real markdown — its H1 is a heading here.
  await expect(page.getByRole("link", { name: "SKILL.md" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Deploy runbook" })).toBeVisible();

  // The tab bar names the three sibling routes; the badge carries the open-proposal count.
  const tabs = page.getByRole("navigation", { name: "Skill sections" });
  await expect(tabs.getByRole("link", { name: "Current" })).toBeVisible();
  await expect(tabs.getByRole("link", { name: /Proposals/ })).toContainText("1");
  await expect(tabs.getByRole("link", { name: "History" })).toBeVisible();
});

test("the Proposals tab lists the open candidate with a Review link", async ({ page }) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/skills/${SKILL}`);

  await page
    .getByRole("navigation", { name: "Skill sections" })
    .getByRole("link", { name: /Proposals/ })
    .click();
  await page.waitForURL(`**/skills/${SKILL}/proposals`);

  const queue = page.getByRole("region", { name: "Awaiting review" });
  await expect(queue.getByText(candidateId.slice(0, 12), { exact: true })).toBeVisible();
  await expect(queue.getByRole("link", { name: "Review" })).toBeVisible();
});

test("the History tab walks first-parent from the current head", async ({ page }) => {
  const ws = await theWorkspace();
  await gotoSettled(page, `/workspaces/${ws.id}/skills/${SKILL}/history`);

  // The walk starts at the seeded head and reaches genesis; the candidate never appears (it
  // is not on the first-parent chain from current).
  const history = page.getByRole("region", { name: "History" });
  await expect(history.getByText(currentId.slice(0, 12), { exact: true })).toBeVisible();
  await expect(history.getByText("tighten the steps")).toBeVisible();
  await expect(history.getByText("genesis")).toBeVisible();
  await expect(history.getByText(candidateId.slice(0, 12), { exact: true })).toHaveCount(0);
});
