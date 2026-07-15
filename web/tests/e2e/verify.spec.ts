import { expect, type Page, test } from "@playwright/test";
import { E2E_PASSWORD, MEMBER_EMAIL, WORKSPACE_ADDRESS } from "./env";
import { adminQuery } from "./seed";

/**
 * The gh-style DEVICE-APPROVE ceremony end to end: the CLI half is the real `/api/v1/device/*`
 * flow (start → poll), the browser half is /verify — a signed-in person resolves the short
 * user code, sees what is asking, and approves BEHIND STEP-UP (approval mints a credential
 * that acts as them) or denies without one. Terminal poll answers are delivered ONCE.
 *
 * Runs with the suite's default storage state (the claimed owner) except the signed-out leg.
 */

test.describe.configure({ mode: "serial" });

interface DeviceFlowStart {
  device_code: string;
  user_code: string;
  verification_uri: string;
  verification_uri_complete: string;
}

async function startDeviceFlow(page: Page, requestedName: string): Promise<DeviceFlowStart> {
  const response = await page.request.post("/api/v1/device/authorize", {
    data: { requested_name: requestedName, workspace: WORKSPACE_ADDRESS },
  });
  expect(response.ok(), `device authorize failed: ${response.status()}`).toBe(true);
  return (await response.json()) as DeviceFlowStart;
}

async function pollDeviceFlow(
  page: Page,
  deviceCode: string,
): Promise<{
  status: string;
  credential?: string;
  device_id?: string;
  workspace?: { name: string };
}> {
  const response = await page.request.post("/api/v1/device/token", {
    data: { device_code: deviceCode },
  });
  expect(response.ok()).toBe(true);
  return response.json();
}

test.describe("signed out", () => {
  test.use({ storageState: { cookies: [], origins: [] } });

  test("the verify page bounces to /login carrying itself — code included — as the next path", async ({
    page,
  }) => {
    await page.goto("/verify?code=AB12-CD34");
    await page.waitForURL((u) => u.pathname === "/login");
    const next = new URL(page.url()).searchParams.get("next");
    expect(next).toBe("/verify?code=AB12-CD34");
  });
});

test("an unknown code is an honest in-page state, never a 404", async ({ page }) => {
  await page.goto("/verify?code=ZZZZ-9999");
  await expect(page.getByRole("heading", { name: "Approve a device" })).toBeVisible();
  await expect(page.getByText("No pending request for this code")).toBeVisible();
});

test("approve is step-up gated: a wrong password mints nothing; the right one mints the credential the poll delivers", async ({
  page,
}) => {
  const flow = await startDeviceFlow(page, "e2e-laptop");
  expect(flow.verification_uri_complete).toContain(`/verify?code=`);

  await page.goto(`/verify?code=${encodeURIComponent(flow.user_code)}`);
  // The resolved request names what is asking, honestly (the name also rides the approve
  // button's label, so pin the disclosure span exactly).
  await expect(page.getByText("“e2e-laptop”", { exact: true })).toBeVisible();
  await expect(page.getByText("wants to act as you", { exact: false })).toBeVisible();

  // A WRONG password refuses the mint; the device keeps polling `pending`.
  await page.getByLabel("Confirm with your password").fill("not-the-password-999");
  await page.getByRole("button", { name: "Approve “e2e-laptop”" }).click();
  await expect(page.getByRole("alert")).toContainText("Password check failed");
  expect((await pollDeviceFlow(page, flow.device_code)).status).toBe("pending");

  // The RIGHT password mints the device and the page says so.
  await page.getByLabel("Confirm with your password").fill(E2E_PASSWORD);
  await page.getByRole("button", { name: "Approve “e2e-laptop”" }).click();
  await expect(page.getByRole("heading", { name: "Device connected" })).toBeVisible();

  // The poll delivers the grant ONCE: the presented device_code IS the promoted credential.
  const granted = await pollDeviceFlow(page, flow.device_code);
  expect(granted.status).toBe("granted");
  expect(granted.credential).toBe(flow.device_code);
  expect(granted.workspace?.name).toBe(WORKSPACE_ADDRESS);

  // The minted device row: owned by the approver, named as requested, hash-stored credential.
  const rows = await adminQuery<{ id: string; display_name: string; email: string }>(
    `select d.id, d.display_name, u.email from web.device d join web."user" u on u.id = d.user_id
     where d.id = $1`,
    [granted.device_id],
  );
  expect(rows[0]?.display_name).toBe("e2e-laptop");
  expect(rows[0]?.email).toBe(MEMBER_EMAIL);

  // Terminal answers are delivered once — the flow row died as it was reported.
  expect((await pollDeviceFlow(page, flow.device_code)).status).toBe("expired");
});

test("deny destroys the pending request and mints nothing — no step-up needed", async ({
  page,
}) => {
  const flow = await startDeviceFlow(page, "e2e-stranger");
  await page.goto(`/verify?code=${encodeURIComponent(flow.user_code)}`);
  await expect(page.getByText("“e2e-stranger”", { exact: true })).toBeVisible();

  await page.getByRole("button", { name: "Deny — this isn’t my device" }).click();
  await expect(page.getByRole("heading", { name: "Request denied" })).toBeVisible();

  // The device learns the denial on its next poll — once; after that the code names nothing.
  expect((await pollDeviceFlow(page, flow.device_code)).status).toBe("denied");
  expect((await pollDeviceFlow(page, flow.device_code)).status).toBe("expired");

  // Nothing was minted for the denied request.
  const rows = await adminQuery<{ n: string }>(
    `select count(*)::text as n from web.device where display_name = 'e2e-stranger'`,
  );
  expect(rows[0]?.n).toBe("0");
});
