import { expect, test } from "@playwright/test";
import { SKILL_MD_V1, SKILL_MD_V2 } from "../fixtures/plane/data.mjs";
import { PLANE_INTERNAL_TOKEN, PLANE_PORT } from "./env";
import {
  adminQuery,
  custodyCalls,
  ensureBundle,
  ensureSeatedUser,
  seedCustody,
  theWorkspace,
} from "./seed";
import { gotoSettled, signIn } from "./sign-in";

/**
 * The browser team-revert: a reviewer rolls a skill's current back to a known-good ancestor.
 * The affordance rides the History tab — one collapsible confirm per NON-current row,
 * owner|reviewer only — and the write is the vault's FORWARD revert: a server-constructed
 * commit carrying the good tree, CAS-bound to the generation the page rendered against. A
 * plain member never sees the control; a stale generation surfaces the honest conflict note
 * and rolls nothing back.
 */

const REVIEWER = "revert-writer@example.com";
const REVIEWER_DISPLAY = "revert-writer";
const MEMBER = "revert-member@example.com";

const SKILL = { id: "s_e2e_revert", name: "revert-runbook" };
const STALE = { id: "s_e2e_revert_stale", name: "revert-stale" };

let goodId: string;
let staleGoodId: string;
let staleCurrentId: string;

test.describe.configure({ mode: "serial" });
test.use({ storageState: { cookies: [], origins: [] } });

async function seedRevertBundle(bundle: { id: string; name: string }) {
  const ws = await theWorkspace();
  await ensureBundle(bundle);
  const seeded = await seedCustody(
    [
      {
        ws: ws.id,
        bundle: bundle.id,
        versions: [
          { files: [{ path: "SKILL.md", content: SKILL_MD_V1 }], message: "the good version" },
          {
            files: [{ path: "SKILL.md", content: SKILL_MD_V2 }],
            parent: 0,
            message: "the regressing version",
          },
        ],
        current: 1,
      },
    ],
    { reset: false },
  );
  return {
    good: seeded[0]?.versions[0]?.version_id as string,
    current: seeded[0]?.versions[1]?.version_id as string,
  };
}

test.beforeAll(async () => {
  await ensureSeatedUser(REVIEWER, "reviewer");
  await ensureSeatedUser(MEMBER, "member");
  await seedCustody([]); // a clean custody world for this file
  const a = await seedRevertBundle(SKILL);
  goodId = a.good;
  const b = await seedRevertBundle(STALE);
  staleGoodId = b.good;
  staleCurrentId = b.current;
});

test("a reviewer rolls back: the confirm, one exact wire POST, the forward move lands", async ({
  page,
}) => {
  const ws = await theWorkspace();
  await signIn(page, REVIEWER);
  await gotoSettled(page, `/workspaces/${ws.id}/skills/${SKILL.name}/history`);

  // The non-current ancestor row carries the collapsible roll-back control; the head does not.
  const summary = page.getByText("Roll back to this version…");
  await expect(summary).toHaveCount(1);
  await summary.click();

  // The confirm step names the honest consequence before firing.
  await expect(
    page.getByText("a forward move, nothing is deleted", { exact: false }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Roll back to this version" }).click();

  // The success copy renders in the still-mounted control…
  await expect(
    page.getByText("Rolled back — this version's bytes are the team's current version."),
  ).toBeVisible();

  // …the wire payload landed: the GOOD target + the live generation the page rendered
  // against + the reviewer's display attribution (the forward commit's message also rides the
  // wire, recorded verbatim — its exact copy is the app's to choose, so it stays unpinned).
  const calls = await custodyCalls({ route: "revert", bundle: SKILL.id });
  expect(calls).toHaveLength(1);
  const call = calls[0] as NonNullable<(typeof calls)[0]>;
  expect(call.body.to_version_id).toBe(goodId);
  expect(call.body.expected_generation).toBe(1);
  expect(call.body.attribution).toBe(REVIEWER_DISPLAY);

  // The revert is a FORWARD commit: the pointer advanced onto a NEW version carrying the good
  // tree — never a rewind of the id.
  const pointer = await adminQuery<{ version_id: string; generation: string }>(
    `select version_id, generation::text from plane.current_pointer where bundle_id = $1`,
    [SKILL.id],
  );
  expect(pointer[0]?.generation).toBe("2");
  expect(pointer[0]?.version_id).not.toBe(goodId);

  // The revalidated history walks from the NEW forward head.
  await gotoSettled(page, `/workspaces/${ws.id}/skills/${SKILL.name}/history`);
  await expect(
    page
      .getByRole("region", { name: "History" })
      .getByText((pointer[0]?.version_id as string).slice(0, 12), { exact: true }),
  ).toBeVisible();
});

test("a plain member sees no roll-back control on a non-current row", async ({ page }) => {
  const ws = await theWorkspace();
  await signIn(page, MEMBER);
  await gotoSettled(page, `/workspaces/${ws.id}/skills/${STALE.name}/history`);

  const history = page.getByRole("region", { name: "History" });
  await expect(history.getByText(staleGoodId.slice(0, 12), { exact: true })).toBeVisible();
  await expect(page.getByText("Roll back to this version…")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Roll back to this version" })).toHaveCount(0);
});

test("a stale generation renders the conflict note, nothing rolled back", async ({ page }) => {
  const ws = await theWorkspace();
  await signIn(page, REVIEWER);
  await gotoSettled(page, `/workspaces/${ws.id}/skills/${STALE.name}/history`);
  await page.getByText("Roll back to this version…").click();

  // The pointer moves UNDER the open page (a concurrent publish through the custody lane).
  const raced = await page.request.post(
    `http://127.0.0.1:${PLANE_PORT}/internal/v1/workspaces/${ws.id}/bundles/${STALE.id}/publish`,
    {
      headers: { authorization: `Bearer ${PLANE_INTERNAL_TOKEN}` },
      data: {
        files: [
          {
            path: "SKILL.md",
            mode: "100644",
            content_base64: Buffer.from(`${SKILL_MD_V2}raced\n`, "utf8").toString("base64"),
          },
        ],
        parent: staleCurrentId,
        attribution: "racer",
        message: "raced publish",
        expected_generation: 1,
      },
    },
  );
  expect(raced.ok()).toBe(true);

  await page.getByRole("button", { name: "Roll back to this version" }).click();

  // The vault's CAS refused the stale binding — the control shows the honest moved-pointer
  // note, and the recorded call proves the refusal was a real wire round-trip.
  await expect(
    page.getByText("The pointer moved while you were here", { exact: false }),
  ).toBeVisible();
  await expect(
    page.getByText("Rolled back — this version's bytes are the team's current version."),
  ).toHaveCount(0);
  expect(await custodyCalls({ route: "revert", bundle: STALE.id })).toHaveLength(1);

  // The pointer still sits where the racer left it — generation 2, the raced version.
  const pointer = await adminQuery<{ generation: string; version_id: string }>(
    `select generation::text, version_id from plane.current_pointer where bundle_id = $1`,
    [STALE.id],
  );
  expect(pointer[0]?.generation).toBe("2");
  expect(pointer[0]?.version_id).not.toBe(staleGoodId);
});
