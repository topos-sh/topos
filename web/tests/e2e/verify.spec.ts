import { expect, type Page, test } from "@playwright/test";
import { MEMBER_EMAIL, PLANE_PORT } from "./env";

/**
 * The verification page's two halves. Signed OUT (fresh context): the zero-JS disclosure. Signed IN
 * (the suite's default storage state): the browser approve legs — the acting identity is the
 * session-derived acting-email header (never a client-supplied field), and the recorded fixture
 * calls prove what actually went over the wire (enroll → outcome `confirmed`, standup → `approved`).
 */

test.describe("the verification page, signed out", () => {
  test.use({ storageState: { cookies: [], origins: [] } });

  test("verified domain: full disclosure with the sign-in gate", async ({ page }) => {
    await page.goto("/verify/APPROVED1");
    await expect(page.getByRole("heading", { name: "Is this your device?" })).toBeVisible();
    await expect(page.getByText("roberts-macbook")).toBeVisible();
    // The fingerprint renders grouped in 4s for eyeball comparison.
    await expect(page.getByText("9f3a 7c21 b4d8 e650")).toBeVisible();
    await expect(page.getByText("This device will join")).toBeVisible();
    await expect(page.getByText("acme.dev — domain verified by the server")).toBeVisible();
    await expect(page.getByRole("link", { name: "Sign in to continue" })).toBeVisible();
  });
});

test.describe("the verification page, signed in", () => {
  async function recordedCalls(
    page: Page,
  ): Promise<
    { route: string; key: string | null; acting: string; body: Record<string, unknown> }[]
  > {
    const response = await page.request.get(`http://127.0.0.1:${PLANE_PORT}/__test/calls`);
    return response.json();
  }

  test("an enroll session renders Join + one Approve; approving confirms as the session email", async ({
    page,
  }) => {
    await page.goto("/verify/APPROVED1");
    await expect(page.getByRole("heading", { name: "Join Acme Platform" })).toBeVisible();
    await expect(page.getByText("roberts-macbook")).toBeVisible();

    await page.getByRole("button", { name: `Approve — join as ${MEMBER_EMAIL}` }).click();
    await expect(page).toHaveURL(/\/verify\/APPROVED1\?outcome=approved/);
    await expect(page.getByRole("heading", { name: "Approved" })).toBeVisible();

    const approve = (await recordedCalls(page)).filter(
      (c) => c.route === "approve" && c.key === "APPROVED1",
    );
    expect(approve.length).toBeGreaterThan(0);
    // The acting identity is the SESSION's header — nothing client-supplied; the approve body is empty.
    expect(approve.at(-1)?.acting).toBe(MEMBER_EMAIL);
    expect(approve.at(-1)?.body).toEqual({});
  });

  test("a standup session prefils the name from the email; the default untouched is a complete standup", async ({
    page,
  }) => {
    await page.goto("/verify/STANDUP42");
    await expect(page.getByRole("heading", { name: "Create your workspace" })).toBeVisible();
    const name = page.getByRole("textbox", { name: "Workspace name" });
    // Prefilled `<localpart>'s workspace` from the session email.
    await expect(name).toHaveValue("reviewer's workspace");

    await page
      .getByRole("button", { name: `Approve — create workspace as ${MEMBER_EMAIL}` })
      .click();
    await expect(page).toHaveURL(/\/verify\/STANDUP42\?outcome=approved/);
    await expect(page.getByRole("heading", { name: "Approved" })).toBeVisible();

    const standup = (await recordedCalls(page)).filter(
      (c) => c.route === "approve-standup" && c.key === "STANDUP42",
    );
    expect(standup.length).toBeGreaterThan(0);
    expect(standup.at(-1)?.acting).toBe(MEMBER_EMAIL);
    expect(standup.at(-1)?.body).toEqual({ display_name: "reviewer's workspace" });
  });
});
