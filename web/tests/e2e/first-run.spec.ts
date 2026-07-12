import { expect, test } from "@playwright/test";

/**
 * The first-run CLAIM on the public landing page.
 *
 * HONEST GAP: the true virgin-boot flow — an empty plane leading with "This plane has no workspace
 * yet" and routing the first sign-in into standing the first workspace up — CANNOT be asserted
 * against this suite's database. The shared topos_e2e plane is globally seeded with workspaces
 * (auth.setup.ts) before any spec runs, so it is never virgin mid-suite, and re-ordering a spec to
 * run "before the seed" would be a rot-prone hack the seed's own invariants would fight. The virgin
 * boolean that drives the claim (hasAnyWorkspace) is therefore proven at the UNIT level
 * (tests/unit/first-run.test.ts: false on a fresh scratch schema, true after one workspace row),
 * and the real virgin-boot experience is a compose-stack concern proven when the packaged stack
 * boots against an empty database at the packaging stage.
 *
 * What this spec CAN prove is the other half of the branch: with workspaces already present, the
 * landing shows NO claim block — the ordinary marketing page renders unchanged.
 */

// The claim block's heading copy — present ONLY on a virgin plane.
const CLAIM_MARKER = "This plane has no workspace yet";

test.use({
  storageState: { cookies: [], origins: [] },
  contextOptions: { reducedMotion: "reduce" },
});

test("the landing shows no first-run claim block when workspaces already exist", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page).toHaveURL(/\/$/);
  // The ordinary hero renders…
  await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
  // …and the empty-plane claim copy is absent (this plane already holds seeded workspaces).
  await expect(page.getByText(CLAIM_MARKER)).toHaveCount(0);
});
