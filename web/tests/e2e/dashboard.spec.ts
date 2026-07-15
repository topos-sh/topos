import { randomUUID } from "node:crypto";
import { expect, test } from "@playwright/test";
import { SKILL_MD_V1 } from "../fixtures/plane/data.mjs";
import { MEMBER_EMAIL } from "./env";
import {
  adminQuery,
  ensureBundle,
  ensureProposal,
  mintDevice,
  seedCustody,
  theWorkspace,
} from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * The distribute-read spine — THE HERO: a device's `POST /api/v1/publish` (the real door, a
 * bearer credential and nothing else) lands its genesis bundle row + the vault publish, and
 * the dashboard catalog renders it from the shared reads on the next load. No web-tier session
 * state of any kind feeds the catalog: the bundle row + the custody pointer mirror ARE the
 * surface. The same op_id replayed answers the stored receipt byte-for-byte.
 */

const CREDENTIAL = "cred-dk_e2e_publisher";
const DEVICE_ID = "dk_e2e_publisher";

test.describe.configure({ mode: "serial" });

test.beforeAll(async () => {
  await seedCustody([]); // reset custody to a clean world for this file
  const owner = (
    await adminQuery<{ user_id: string }>(
      `select user_id from web.seat where role = 'owner' limit 1`,
    )
  )[0]?.user_id as string;
  await adminQuery(`delete from web.device where id = $1`, [DEVICE_ID]);
  await mintDevice(owner, DEVICE_ID, "publisher", CREDENTIAL);
  // A fresh run of the hero on a reused database: the genesis name must be free.
  await adminQuery(`delete from web.bundle where name like 'release-checklist%'`, []);
});

test("HERO: a device publish lands the catalog row the dashboard renders — and replays verbatim", async ({
  page,
}) => {
  const ws = await theWorkspace();
  const opId = randomUUID();
  const body = {
    workspace_id: ws.id,
    skill_id: "client-side-name", // genesis: the server mints the real bundle id
    op_id: opId,
    expected: 0,
    display_name: "Release Checklist",
    candidate: {
      files: [
        {
          path: "SKILL.md",
          mode: "100644",
          content_base64: Buffer.from(SKILL_MD_V1, "utf8").toString("base64"),
        },
      ],
      parents: [],
      author: "publisher-device",
      message: "first publish",
    },
  };
  const response = await page.request.post("/api/v1/publish", {
    data: body,
    headers: { authorization: `Bearer ${CREDENTIAL}` },
  });
  expect(response.ok(), await response.text()).toBe(true);
  const envelope = (await response.json()) as {
    ok: boolean;
    receipt: { outcome: string; skill_id: string; version_id: string; bundle_digest: string };
  };
  expect(envelope.ok).toBe(true);
  expect(envelope.receipt.outcome).toBe("OK");
  const versionId = envelope.receipt.version_id;
  expect(versionId).toMatch(/^[0-9a-f]{64}$/);

  // Genesis registration: the birth name folded from the display name; placed into everyone.
  const bundleRow = await adminQuery<{ id: string; name: string }>(
    `select id, name from web.bundle where id = $1`,
    [envelope.receipt.skill_id],
  );
  expect(bundleRow[0]?.name).toBe("release-checklist");

  // The dashboard renders the row from the shared reads alone — pointer short id + digest.
  await gotoSettled(page, `/workspaces/${ws.id}`);
  const hero = page.getByRole("listitem").filter({ hasText: "Release Checklist" });
  await expect(hero).toBeVisible();
  await expect(hero.getByText(versionId.slice(0, 12))).toBeVisible();
  await expect(
    hero.getByText(`sha-256:${envelope.receipt.bundle_digest.slice(0, 12)}…`),
  ).toBeVisible();

  // The op receipt: the SAME op_id + the SAME bytes replays the stored envelope verbatim.
  const replay = await page.request.post("/api/v1/publish", {
    data: body,
    headers: { authorization: `Bearer ${CREDENTIAL}` },
  });
  expect(replay.ok()).toBe(true);
  expect(await replay.json()).toEqual(envelope);
});

test("a wrong credential is the uniform wire 404 — no oracle on the door", async ({ page }) => {
  const ws = await theWorkspace();
  // Shape refusals run BEFORE the credential resolve — a well-formed body isolates the miss.
  const miss = await page.request.post("/api/v1/publish", {
    data: {
      workspace_id: ws.id,
      skill_id: "x",
      op_id: randomUUID(),
      expected: 0,
      candidate: { files: [], parents: [], author: "a", message: "" },
    },
    headers: { authorization: "Bearer not-a-credential" },
  });
  expect(miss.status()).toBe(404);
  const body = (await miss.json()) as { ok: boolean; error: { code: string } };
  expect(body.ok).toBe(false);
  expect(body.error.code).toBe("NOT_FOUND");
});

test("the catalog stays honest: an open-proposal badge and a pointer-less row", async ({
  page,
}) => {
  const ws = await theWorkspace();
  const owner = (
    await adminQuery<{ id: string }>(`select id from web."user" where email = $1`, [MEMBER_EMAIL])
  )[0]?.id as string;

  // A bundle with an OPEN proposal row: the badge counts the app's own rows.
  await ensureBundle({ id: "s_e2e_dash_prop", name: "dash-proposals" });
  await ensureProposal({
    id: "p_e2e_dash_open",
    bundleId: "s_e2e_dash_prop",
    candidateVersionId: "ab".repeat(32),
    proposedBy: owner,
    status: "open",
  });
  // A named identity that has never published: the row renders, honestly pointer-less.
  await ensureBundle({ id: "s_e2e_dash_bare", name: "dash-unpublished" });

  await gotoSettled(page, `/workspaces/${ws.id}`);
  const withProposal = page.getByRole("listitem").filter({ hasText: "dash-proposals" });
  await expect(withProposal.getByText("1 proposal awaiting review")).toBeVisible();
  const bare = page.getByRole("listitem").filter({ hasText: "dash-unpublished" });
  await expect(bare.getByText("Nothing published yet")).toBeVisible();
});
