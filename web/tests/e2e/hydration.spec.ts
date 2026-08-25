import { expect, type Page, test } from "@playwright/test";

/**
 * THE DOCUMENT HAS TO HYDRATE — and the proof is a console that stayed empty while it did.
 *
 * A hydration mismatch is not a cosmetic warning. React throws the served document away and
 * re-renders the whole tree, and in the window before that lands every form on the page is
 * inert: a click on the submit button clears the field and sends nothing. Production shipped
 * exactly that, and it read as "the approve form is flaky" — the only trace was a minified
 * React error in a console nobody had open.
 *
 * The cause was the two builds disagreeing about a stylesheet URL: the server bundle rendered
 * `<link href="/assets/app-A.css">` while the browser bundle asked for `/assets/app-B.css`, so
 * hydration could not match the link and gave up on the document. Hence the stylesheet
 * assertion below — what the document ships and what the hydrated page holds must be the same
 * list. Third-party scripts (the analytics loader) inserting their own nodes into `<head>` are
 * deliberately NOT asserted against: React tolerates foreign nodes there, and they were never
 * the problem.
 */

/** Console errors + uncaught exceptions, collected from before the navigation starts. */
function watchConsole(page: Page): string[] {
  const faults: string[] = [];
  page.on("console", (message) => {
    if (message.type() === "error") {
      faults.push(`console: ${message.text()}`);
    }
  });
  page.on("pageerror", (error) => faults.push(`uncaught: ${error.message}`));
  return faults;
}

/** The stylesheets the SERVER document shipped, in document order. */
function shippedStylesheets(html: string): string[] {
  return [...html.matchAll(/<link\b[^>]*\brel="stylesheet"[^>]*\bhref="([^"]+)"/g)].map(
    (match) => match[1] as string,
  );
}

/** The stylesheets the HYDRATED page holds, in document order. */
function heldStylesheets(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    Array.from(window.document.querySelectorAll('link[rel="stylesheet"]')).map(
      (link) => link.getAttribute("href") ?? "",
    ),
  );
}

test("the workspace dashboard hydrates on the stylesheets it shipped, with a clean console", async ({
  page,
}) => {
  const faults = watchConsole(page);

  const response = await page.goto("/");
  const shipped = shippedStylesheets((await response?.text()) ?? "");
  expect(shipped.length).toBeGreaterThan(0);

  // Hydration commits a scheduler tick after load, and React reports a mismatch a tick after
  // that — so settle the page before reading either the console or the DOM.
  await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
  await page.waitForLoadState("networkidle");

  expect(await heldStylesheets(page)).toEqual(shipped);
  expect(faults).toEqual([]);
});

test("the login-approve page hydrates, and its form answers a code that names nothing", async ({
  page,
}) => {
  const faults = watchConsole(page);

  const response = await page.goto("/verify");
  const shipped = shippedStylesheets((await response?.text()) ?? "");
  expect(shipped.length).toBeGreaterThan(0);

  // The symptom a broken hydration wore on this page: the click reached no action at all — the
  // field cleared and no request left the browser. A page that hydrated runs the lookup and
  // says what happened, in the page, where the click was.
  await page.getByLabel("Code").fill("ZZ99-ZZ99");
  await page.getByRole("button", { name: "Look up" }).click();
  await expect(page.getByText("No pending request for that code")).toBeVisible();

  expect(await heldStylesheets(page)).toEqual(shipped);
  expect(faults).toEqual([]);
});
