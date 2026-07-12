import { expect, test } from "@playwright/test";
import {
  CANDIDATE_ID,
  CURRENT_ID,
  WS as E2E_WS,
  JOINER_EMAIL,
  MOVED_ID,
  SKILL,
} from "../fixtures/plane/data.mjs";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The skill page's TABBED layout on the member-session lane. Opening a skill lands on the Current
 * tab — the current version's files + doc preview, inline — with Proposals and History as sibling
 * ROUTES reached from the tab bar. Every vault read (version metadata / proposals / history walk)
 * rides the internal lane authenticated by the guard-minted actor's verified session email in the
 * X-Topos-Acting-Email header; no token is opened anywhere. The URL keys on the CATALOG NAME; the
 * app resolves it to the immutable skill_id every vault call carries. JOINER_EMAIL holds a
 * confirmed seat on ws-e2e and ZERO web-tier rows, so the whole page renders from the roster
 * admission + the session reads alone.
 */

test.use({ storageState: { cookies: [], origins: [] } });

const base = `/workspaces/${E2E_WS}/skills/${SKILL}`;

test("the skill page opens on the Current tab: file listing, doc preview, and the tab bar", async ({
  page,
}) => {
  await signIn(page, JOINER_EMAIL);
  await gotoSettled(page, base);

  // The header is the skill's catalog name.
  await expect(page.getByRole("heading", { name: SKILL })).toBeVisible();

  // Current is the default view: the current version's listing + doc preview render INLINE (no
  // click-through). The root SKILL.md renders as real markdown — its H1 is a heading here.
  await expect(page.getByRole("link", { name: "SKILL.md" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Deploy runbook" })).toBeVisible();

  // The tab bar names the three sibling routes.
  const tabs = page.getByRole("navigation", { name: "Skill sections" });
  await expect(tabs.getByRole("link", { name: "Current" })).toBeVisible();
  await expect(tabs.getByRole("link", { name: /Proposals/ })).toBeVisible();
  await expect(tabs.getByRole("link", { name: "History" })).toBeVisible();
});

test("the Proposals tab lists the open candidate with a Review link", async ({ page }) => {
  await signIn(page, JOINER_EMAIL);
  await gotoSettled(page, base);

  await page
    .getByRole("navigation", { name: "Skill sections" })
    .getByRole("link", { name: /Proposals/ })
    .click();
  await page.waitForURL(`**/skills/${SKILL}/proposals`);

  // The fixture's raw table seeds TWO open proposals, but one's base moved — and the vault applies
  // its `open ∧ base == current` rule (a staled proposal vanishes from the list), so the list
  // carries ONE row and the tab badge's DB-mirrored count agrees with it by construction.
  const proposals = page.getByRole("region", { name: "Awaiting review" });
  await expect(proposals.getByText(CANDIDATE_ID.slice(0, 12), { exact: true })).toBeVisible();
  await expect(proposals.getByText(MOVED_ID.slice(0, 12), { exact: true })).toHaveCount(0);
  await expect(proposals.getByRole("link", { name: "Review" }).first()).toBeVisible();
  const tabs = page.getByRole("navigation", { name: "Skill sections" });
  await expect(tabs.getByRole("link", { name: /Proposals/ })).toContainText("1");
});

test("the History tab walks first-parent from the current head", async ({ page }) => {
  await signIn(page, JOINER_EMAIL);
  await gotoSettled(page, base);

  await page
    .getByRole("navigation", { name: "Skill sections" })
    .getByRole("link", { name: "History" })
    .click();
  await page.waitForURL(`**/skills/${SKILL}/history`);

  // The first-parent walk starts at the seeded current head.
  const history = page.getByRole("region", { name: "History" });
  await expect(history.getByText(CURRENT_ID.slice(0, 12), { exact: true })).toBeVisible();
});
