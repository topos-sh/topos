import { expect, test } from "@playwright/test";

/**
 * The sign-in page must never look idle while it is waiting. Every rung here crosses the network
 * before anything on screen changes — the magic link is an SMTP round-trip, a password is a
 * deliberate slow hash — so without an in-flight state the click reads as "nothing happened" and
 * the obvious next move is to click again.
 *
 * Each test HOLDS the auth request open (a route handler that resolves only when the assertions
 * are done) so the in-flight frame is observable at all, then releases it. The suite runs
 * MAIL-ARMED, so the OSS composition arms the magic-link rung and `/login` leads with it — the
 * password rung sits behind the quiet "Sign in with a password instead" toggle.
 *
 * These run SIGNED OUT: the project's storageState carries a seeded member, whom `/login` would
 * bounce, so each test clears its context cookies first.
 */

test.beforeEach(async ({ context }) => {
  await context.clearCookies();
});

test("the magic-link rung: the field and button go inert and the button names the wait", async ({
  page,
}) => {
  await page.goto("/login");

  const email = page.getByLabel("Email");
  const submit = page.getByRole("button", { name: "Email me a sign-in link" });
  await expect(submit).toBeEnabled();
  await email.fill("pending-magic@e2e.test");

  // Hold the send open so the in-flight frame is observable, and release it from inside the
  // assertions below.
  let release: () => void = () => {};
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  await page.route("**/api/auth/sign-in/magic-link", async (route) => {
    await held;
    await route.continue();
  });

  await submit.click();

  // The whole point: mid-flight the page SAYS it is working, and cannot be submitted again.
  await expect(page.getByRole("button", { name: "Sending the link…" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Sending the link…" })).toBeDisabled();
  await expect(email).toBeDisabled();

  release();
  // …and the wait resolves into the sent card.
  await expect(page.getByText("Check your email")).toBeVisible();
});

test("the password rung: both fields and the button go inert while the credential is checked", async ({
  page,
}) => {
  await page.goto("/login");
  // Magic link leads when mail is armed; the password rung is the quiet alternative under it.
  await page.getByRole("button", { name: "Sign in with a password instead" }).click();

  const email = page.getByLabel("Email");
  const password = page.getByLabel("Password");
  await email.fill("pending-password@e2e.test");
  await password.fill("e2e-password-1234");

  let release: () => void = () => {};
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  await page.route("**/api/auth/sign-in/email", async (route) => {
    await held;
    await route.continue();
  });

  await page.getByRole("button", { name: "Sign in", exact: true }).click();

  await expect(page.getByRole("button", { name: "Signing in…" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Signing in…" })).toBeDisabled();
  await expect(email).toBeDisabled();
  await expect(password).toBeDisabled();
  // The rung switch is inert too — flipping the form out from under a live request would strand it.
  await expect(page.getByRole("button", { name: /sign-in link instead/i })).toBeDisabled();

  release();
  // This address has no account, so the answer is the page's one constant refusal — and the form
  // comes back ARMED, so a corrected password can be tried immediately.
  await expect(page.getByRole("alert")).toContainText("Couldn’t sign in");
  await expect(email).toBeEnabled();
  await expect(page.getByRole("button", { name: "Sign in", exact: true })).toBeEnabled();
});

test("a request that never completes re-arms the form instead of locking it", async ({ page }) => {
  await page.goto("/login");
  await page.getByLabel("Email").fill("offline@e2e.test");

  // Better Auth's client REJECTS on a transport failure (it returns `{ error }` only for
  // answers the server gave), and a rejection that skipped the re-arm would leave every
  // control inside the disabled fieldset stuck until a reload.
  await page.route("**/api/auth/sign-in/magic-link", (route) => route.abort("failed"));

  const email = page.getByLabel("Email");
  await page.getByRole("button", { name: "Email me a sign-in link" }).click();

  await expect(page.getByRole("alert")).toContainText("Couldn’t reach the server");
  await expect(email).toBeEnabled();
  await expect(page.getByRole("button", { name: "Email me a sign-in link" })).toBeEnabled();
  // The rung switch comes back too — the whole card re-arms, not just the field that failed.
  await expect(page.getByRole("button", { name: "Sign in with a password instead" })).toBeEnabled();
});

test("a second click during the send cannot post the magic link twice", async ({ page }) => {
  await page.goto("/login");
  await page.getByLabel("Email").fill("double-click@e2e.test");

  let sends = 0;
  let release: () => void = () => {};
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  await page.route("**/api/auth/sign-in/magic-link", async (route) => {
    sends += 1;
    await held;
    await route.continue();
  });

  const submit = page.getByRole("button", { name: "Email me a sign-in link" });
  await submit.click();
  const sending = page.getByRole("button", { name: "Sending the link…" });
  await expect(sending).toBeDisabled();
  // A disabled button swallows the click; assert the request count, not just the attribute.
  await sending.click({ force: true, trial: false }).catch(() => {});

  release();
  await expect(page.getByText("Check your email")).toBeVisible();
  expect(sends).toBe(1);
});
