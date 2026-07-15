import { expect, test } from "@playwright/test";
import {
  BIG_CONTENT_BASE64,
  BINARY_CONTENT_BASE64,
  BINARY_MARKER,
  DEPLOY_SH,
  GUIDE_MD,
  SKILL_MD_V1,
  SKILL_MD_V2,
  XSS_CONTENT,
  XSS_PATH,
} from "../fixtures/plane/data.mjs";
import { ensureBundle, seedCustody, theWorkspace } from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * The skill file browser: the version file listing (tree + current badge + README-style doc
 * preview), the per-file view (rendered markdown, highlighted code, the Rendered|Raw toggle),
 * and the honest binary / too-large / fetch-failed cards. The hostile fixture file proves the
 * sanitizer end to end — a bundle's markdown renders inert. Every byte rides the fixture
 * vault's object reads, content-addressed.
 */

const SKILL_ID = "s_e2e_browse";
const SKILL = "browse-runbook";

let currentId: string;
let base: string;

test.beforeAll(async () => {
  const ws = await theWorkspace();
  base = `/workspaces/${ws.id}/skills/${SKILL}`;
  await ensureBundle({ id: SKILL_ID, name: SKILL });
  const seeded = await seedCustody([
    {
      ws: ws.id,
      bundle: SKILL_ID,
      versions: [
        { files: [{ path: "SKILL.md", content: SKILL_MD_V1 }], message: "genesis" },
        {
          files: [
            { path: "SKILL.md", content: SKILL_MD_V2 },
            { path: "docs/guide.md", content: GUIDE_MD },
            { path: "scripts/deploy.sh", mode: "100755", content: DEPLOY_SH },
            { path: "assets/blob.bin", content_base64: BINARY_CONTENT_BASE64 },
            { path: "data/big.dat", content_base64: BIG_CONTENT_BASE64 },
            { path: "notes/broken.md", content: "these bytes will be dropped\n" },
            { path: XSS_PATH, content: XSS_CONTENT },
          ],
          parent: 0,
          message: "the full tree",
          // The fetch-failed card's subject: listed in the manifest, bytes deliberately gone.
          drop_objects: ["notes/broken.md"],
        },
      ],
      current: 1,
      generation: 2,
    },
  ]);
  currentId = seeded[0]?.versions[1]?.version_id as string;
});

test("the skill root IS the current listing; the versions page shows the same tree with the current chip", async ({
  page,
}) => {
  await gotoSettled(page, base);

  // The root renders the current version inline: tree + chip + doc preview.
  await expect(page.getByText("current", { exact: true })).toBeVisible();
  await expect(page.getByText("docs/", { exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "SKILL.md" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Deploy runbook" })).toBeVisible();

  // The dedicated version page renders the same listing, with the LIVE current comparison.
  await gotoSettled(page, `${base}/versions/${currentId}`);
  await expect(page.getByText("current", { exact: true })).toBeVisible();
  await expect(page.getByText("docs/", { exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "SKILL.md" })).toBeVisible();
});

test("a markdown file renders by default and the Raw toggle shows the source", async ({ page }) => {
  await gotoSettled(page, `${base}/versions/${currentId}/files/SKILL.md`);

  await expect(page.getByRole("heading", { name: "Deploy runbook" })).toBeVisible();

  await page.getByRole("link", { name: "Raw" }).click();
  await expect(page.locator("pre")).toContainText("# Deploy runbook");
  await expect(page.getByRole("heading", { name: "Deploy runbook" })).toHaveCount(0);
});

test("a script renders highlighted, with the executable chip and the toggle", async ({ page }) => {
  await gotoSettled(page, `${base}/versions/${currentId}/files/scripts/deploy.sh`);

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

  const encoded = XSS_PATH.split("/").map(encodeURIComponent).join("/");
  await gotoSettled(page, `${base}/versions/${currentId}/files/${encoded}`);

  // The raw-HTML <script> is dropped at the mdast→hast boundary; the javascript: link loses
  // its href in the sanitizer; nothing executes.
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
  await gotoSettled(page, `${base}/versions/${currentId}/files/assets/blob.bin`);
  await expect(page.getByText(/binary file — not rendered/)).toBeVisible();
  await expect(page.getByText(BINARY_MARKER, { exact: false })).toHaveCount(0);

  await gotoSettled(page, `${base}/versions/${currentId}/files/data/big.dat`);
  await expect(page.getByText(/1 MiB per-file view budget/)).toBeVisible();

  await gotoSettled(page, `${base}/versions/${currentId}/files/notes/broken.md`);
  await expect(page.getByText(/couldn't be fetched/)).toBeVisible();
});
