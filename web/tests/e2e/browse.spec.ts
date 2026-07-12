import { expect, test } from "@playwright/test";
import {
  BINARY_MARKER,
  CANDIDATE_ID,
  CURRENT_ID,
  WS as E2E_WS,
  JOINER_EMAIL,
  SKILL,
  XSS_PATH,
} from "../fixtures/plane/data.mjs";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The skill file browser on the member-session lane: the version file listing (tree + current
 * badge + README-style doc preview), the per-file view (rendered markdown, highlighted code, the
 * Rendered|Raw toggle), and the honest binary / too-large / fetch-failed cards. The hostile
 * fixture file proves the sanitizer end to end — a bundle's markdown renders inert.
 */

test.use({ storageState: { cookies: [], origins: [] } });

const base = `/workspaces/${E2E_WS}/skills/${SKILL}`;

test("the skill root IS the current listing; History links into the version page", async ({
  page,
}) => {
  await signIn(page, JOINER_EMAIL);
  await gotoSettled(page, base);

  // The root IS the current version's listing now (no entry-card click-through): the tree, the
  // "current" chip, and the README-style doc preview all render inline.
  await expect(page.getByText("current", { exact: true })).toBeVisible();
  await expect(page.getByText("docs/", { exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "SKILL.md" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Deploy runbook" })).toBeVisible();

  // The History tab lists the same current head; its short-id link opens the dedicated version
  // page, where the same listing renders.
  await page
    .getByRole("navigation", { name: "Skill sections" })
    .getByRole("link", { name: "History" })
    .click();
  await page.waitForURL(`**/skills/${SKILL}/history`);
  await page.getByRole("link", { name: CURRENT_ID.slice(0, 12) }).click();
  await page.waitForURL(`**/versions/${CURRENT_ID}`);

  await expect(page.getByText("docs/", { exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "SKILL.md" })).toBeVisible();
});

test("a markdown file renders by default and the Raw toggle shows the source", async ({ page }) => {
  await signIn(page, JOINER_EMAIL);
  await gotoSettled(page, `${base}/versions/${CURRENT_ID}/files/SKILL.md`);

  await expect(page.getByRole("heading", { name: "Deploy runbook" })).toBeVisible();

  await page.getByRole("link", { name: "Raw" }).click();
  await expect(page.locator("pre")).toContainText("# Deploy runbook");
  await expect(page.getByRole("heading", { name: "Deploy runbook" })).toHaveCount(0);
});

test("a script renders highlighted, with the executable chip and the toggle", async ({ page }) => {
  await signIn(page, JOINER_EMAIL);
  await gotoSettled(page, `${base}/versions/${CANDIDATE_ID}/files/scripts/deploy.sh`);

  await expect(page.getByText("executable")).toBeVisible();
  await expect(page.locator(".code-view pre")).toContainText('echo "deploy"');
  await expect(page.getByRole("link", { name: "Raw" })).toBeVisible();
});

test("a hostile markdown bundle file renders inert", async ({ page }) => {
  let dialogFired = false;
  page.on("dialog", async (dialog) => {
    dialogFired = true;
    await dialog.dismiss();
  });

  await signIn(page, JOINER_EMAIL);
  const encoded = XSS_PATH.split("/").map(encodeURIComponent).join("/");
  await gotoSettled(page, `${base}/versions/${CANDIDATE_ID}/files/${encoded}`);

  // The raw-HTML <script> is dropped at the mdast→hast boundary; the javascript: link loses its
  // href in the sanitizer; nothing executes.
  await expect(page.locator(".doc-prose")).toBeVisible();
  await expect(page.locator('.doc-prose a[href*="javascript"]')).toHaveCount(0);
  const scriptSmuggled = await page.evaluate(() =>
    Array.from(document.scripts).some((s) => s.textContent?.includes("xss-e2e")),
  );
  expect(scriptSmuggled).toBe(false);

  // Raw view: the hostile source appears as escaped TEXT, byte for byte.
  await page.getByRole("link", { name: "Raw" }).click();
  await expect(page.locator("pre")).toContainText('<script>alert("xss-e2e")</script>');
  expect(dialogFired).toBe(false);
});

test("binary, oversize, and unfetchable files get honest cards", async ({ page }) => {
  await signIn(page, JOINER_EMAIL);

  await gotoSettled(page, `${base}/versions/${CANDIDATE_ID}/files/assets/blob.bin`);
  await expect(page.getByText(/binary file — not rendered/)).toBeVisible();
  await expect(page.getByText(BINARY_MARKER, { exact: false })).toHaveCount(0);

  await gotoSettled(page, `${base}/versions/${CANDIDATE_ID}/files/data/big.dat`);
  await expect(page.getByText(/1 MiB per-file view budget/)).toBeVisible();

  await gotoSettled(page, `${base}/versions/${CANDIDATE_ID}/files/notes/broken.md`);
  await expect(page.getByText(/couldn't be fetched/)).toBeVisible();
});
