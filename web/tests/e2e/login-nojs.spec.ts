import { expect, test } from "@playwright/test";
import { latestMail } from "./seed";

/**
 * The sign-in page BEFORE hydration — the cold first paint right after a CLI opens the
 * browser. The lead form is a real form (method=post + the route action + a hidden canonical
 * `next`), so a submit with JavaScript OFF posts server-side and behaves identically: the
 * magic-link arm sends through the server auth API, renders the proper sent card (heading,
 * the address echoed, a resend arm, a way back), and the mailed link carries the SAME
 * callbackURL the hydrated rung would — the /verify resume survives the no-JS window.
 */

test.use({ storageState: { cookies: [], origins: [] }, javaScriptEnabled: false });

test("the magic-link arm works with JavaScript OFF — a native POST sends the link and weaves next", async ({
  page,
}) => {
  const email = "nojs-magic@e2e.test";
  const device = "ef".repeat(32); // a challenge-shaped next — the resume target under test
  await page.goto(`/login?next=${encodeURIComponent(`/verify?device=${device}`)}`);

  await page.getByLabel("Email").fill(email);
  await page.getByRole("button", { name: "Email me a sign-in link" }).click();

  // The PROPER sent card: heading + the mailbox it went to + the arms out of it.
  await expect(page.getByRole("heading", { name: "Check your email" })).toBeVisible();
  await expect(page.getByText(email)).toBeVisible();
  await expect(page.getByRole("button", { name: "Resend the link" })).toBeVisible();
  await expect(page.getByRole("link", { name: "Use a different email" })).toBeVisible();

  // The server-sent mail carries the canonical /verify resume as its callbackURL.
  const mail = await latestMail("magic-link", email);
  const link = mail.text.match(/https?:\/\/\S+/)?.[0] ?? "";
  expect(decodeURIComponent(link)).toContain(`/verify?device=${device}`);

  // The resend arm is a real form too — it posts and lands the same card again.
  await page.getByRole("button", { name: "Resend the link" }).click();
  await expect(page.getByRole("heading", { name: "Check your email" })).toBeVisible();
});
