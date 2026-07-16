import { expect, type Page } from "@playwright/test";
import { BASE_URL, E2E_PASSWORD } from "./env";

/**
 * The shared email+password sign-in flow for specs that need a SPECIFIC identity (the suite's
 * default storage state is the seeded member from auth.setup.ts). Sign-in is better-auth's own
 * REST flow: an account is created once (self-asserted verified — the OSS composition ships no
 * out-of-band delivery, so possession of the password IS the identity claim) and re-used on later
 * runs. Every account uses the one shared password; the session cookie lands in the page context,
 * which `storageState` then captures.
 */

/** Ensure `email` has an account and sign it in, leaving the session cookie in the page context. */
export async function signIn(page: Page, email: string): Promise<void> {
  const name = email.split("@")[0] ?? "user";
  // Idempotent create: a first run signs up (which also opens the session); a later run finds the
  // account already exists and signs in instead. Both leave a session cookie in the context.
  // better-auth refuses a credential POST without an Origin (its CSRF check); page.request
  // sends none by default, so both calls assert the app's own origin explicitly.
  const created = await page.request.post("/api/auth/sign-up/email", {
    data: { email, password: E2E_PASSWORD, name },
    headers: { origin: BASE_URL },
    failOnStatusCode: false,
  });
  if (!created.ok()) {
    const signedIn = await page.request.post("/api/auth/sign-in/email", {
      data: { email, password: E2E_PASSWORD },
      headers: { origin: BASE_URL },
      failOnStatusCode: false,
    });
    expect(signedIn.ok(), `sign-in failed for ${email}: ${await signedIn.text()}`).toBe(true);
  }
  // Commit the session so a subsequent goto doesn't race the sign-in redirect chain. A seated
  // identity lands in the shell (banner); a SEATLESS one gets /app's house 404 — both are a
  // settled, signed-in document.
  await page.goto("/app");
  await page.waitForURL((u) => !u.pathname.startsWith("/login") && !u.pathname.startsWith("/api"));
  await expect(
    page
      .getByRole("banner")
      .or(page.getByRole("heading", { name: "Not found" }))
      .first(),
  ).toBeVisible();
}

/**
 * A goto that survives one net::ERR_ABORTED. Even after signIn commits the shell, a stray late
 * navigation can supersede the next goto on a loaded runner. Settle once and retry; anything else
 * rethrows.
 */
export async function gotoSettled(page: Page, url: string): Promise<void> {
  try {
    await page.goto(url);
  } catch (error) {
    if (!String(error).includes("ERR_ABORTED")) {
      throw error;
    }
    await page.waitForLoadState();
    await page.goto(url);
  }
}
