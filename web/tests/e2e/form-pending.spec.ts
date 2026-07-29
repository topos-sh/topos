import { expect, test } from "@playwright/test";
import { gotoSettled } from "./sign-in";

/**
 * In-flight state must be scoped to the form's OWN submission. `useNavigation()` reports every
 * navigation on the page — a sidebar link, a breadcrumb, the header's own "All channels" — so a
 * form keyed on a bare `state !== "idle"` goes inert and claims to be working while you are
 * merely leaving. `useSubmittingIntent()` scopes it to a POST.
 */

test("leaving the page does not make the create form claim it is working", async ({ page }) => {
  await gotoSettled(page, "/channels/new");

  const name = page.getByLabel("Channel name");
  const create = page.getByRole("button", { name: "Create channel" });
  await expect(create).toBeVisible();

  // Hold the destination's loader so the departing navigation is observable while THIS page,
  // and its form, are still mounted.
  let release: () => void = () => {};
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  await page.route("**/channels.data*", async (route) => {
    await held;
    await route.continue();
  });

  await page.getByRole("link", { name: "All channels" }).click();

  // Mid-navigation the form is untouched: nothing of ours is on the wire.
  await expect(create).toBeVisible();
  await expect(create).toBeEnabled();
  await expect(name).toBeEnabled();

  release();
  await page.waitForURL(/\/channels$/);
});

/** A fresh, charset-legal channel name — the create lands for real, so it must not collide. */
function uniqueChannelName(): string {
  return `pending-probe-${Date.now().toString(36)}`;
}

test("submitting the create form does mark it in flight", async ({ page }) => {
  await gotoSettled(page, "/channels/new");

  const channel = uniqueChannelName();
  const name = page.getByLabel("Channel name");
  await name.fill(channel);

  let release: () => void = () => {};
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  // The form posts to its own route; hold the action so the in-flight frame is observable.
  await page.route("**/channels/new*", async (route) => {
    if (route.request().method() !== "POST") {
      await route.continue();
      return;
    }
    await held;
    await route.continue();
  });

  await page.getByRole("button", { name: "Create channel" }).click();

  await expect(page.getByRole("button", { name: "Creating…" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Creating…" })).toBeDisabled();
  await expect(name).toBeDisabled();

  release();
  // A landed create redirects onto the new channel — the wait spans the POST and that navigation.
  await page.waitForURL(new RegExp(`/channels/${channel}$`));
});
