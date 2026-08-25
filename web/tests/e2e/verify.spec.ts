import { expect, type Page, test } from "@playwright/test";
import { BASE_URL, CLI_USER_AGENT, MEMBER_EMAIL, WORKSPACE_ADDRESS } from "./env";
import { adminQuery, latestMail, theWorkspace } from "./seed";
import { signIn } from "./sign-in";

/**
 * The login-approve ceremony end to end, over the pick-or-create /verify page: the CLI half is
 * the real `/api/v1/login/*` flow (start → poll), the browser half is /verify — a signed-in
 * person TYPES the short user code into a POST lookup form (the code never rides a URL), sees
 * exactly what is asking, CHOOSES the workspace (here: the one seat — the overwhelmingly
 * common case, one click total), and approves or denies. Approval records consent + the
 * choice; THE SESSION MINTS AT THE CLI'S NEXT POLL — approval from any browser on any device
 * completes the login, which the fragmentation spec proves with a magic link consumed in a
 * fresh browser context. Terminal poll answers repeat until the sweep.
 *
 * Runs with the suite's default storage state (the claimed owner) except where noted.
 */

test.describe.configure({ mode: "serial" });

interface LoginFlowStart {
  device_code: string;
  user_code: string;
  /** The whole start response — asserted against for the retired code-embedding URL. */
  raw: Record<string, unknown>;
}

async function startLoginFlow(
  page: Page,
  requestedName: string,
  opts: { redirect?: "loopback" } = {},
): Promise<LoginFlowStart> {
  const response = await page.request.post("/api/v1/login/authorize", {
    headers: { "user-agent": CLI_USER_AGENT },
    data: {
      requested_name: requestedName,
      preselect: WORKSPACE_ADDRESS,
      ...(opts.redirect === undefined ? {} : { redirect: opts.redirect }),
    },
  });
  expect(response.ok(), `login authorize failed: ${response.status()}`).toBe(true);
  const raw = (await response.json()) as Record<string, unknown>;
  return { device_code: String(raw.device_code), user_code: String(raw.user_code), raw };
}

async function pollLoginFlow(
  page: Page,
  deviceCode: string,
): Promise<{
  status: string;
  credential?: string;
  session_id?: string;
  session_status?: string;
  workspace?: { name: string };
}> {
  const response = await page.request.post("/api/v1/login/token", {
    headers: { "user-agent": CLI_USER_AGENT },
    data: { device_code: deviceCode },
  });
  expect(response.ok()).toBe(true);
  return response.json();
}

/** State one → two: type the code into the POST lookup form so the resolved card renders. */
async function lookUp(page: Page, userCode: string): Promise<void> {
  await page.goto("/verify");
  await page.getByLabel("Code").fill(userCode);
  await page.getByRole("button", { name: "Look up" }).click();
}

test.describe("signed out", () => {
  test.use({ storageState: { cookies: [], origins: [] } });

  test("the verify page bounces to /login carrying itself — the device pass-through included — as the next path", async ({
    page,
  }) => {
    // The loopback device-code HASH the CLI auto-opens with (identifying, never secret): the
    // signed-out bounce re-carries it as `next` so the sign-in returns to finish the approval.
    // The short code is NOT a URL param anymore, so nothing about it appears here.
    const device = "ab".repeat(32); // 64 hex — the device-code-hash shape the loader validates
    await page.goto(`/verify?device=${device}`);
    await page.waitForURL((u) => u.pathname === "/login");
    const next = new URL(page.url()).searchParams.get("next");
    expect(next).toBe(`/verify?device=${device}`);
  });
});

test("ENTER in the code field looks the request up — exactly as the button does", async ({
  page,
}) => {
  // The keyboard is how a code gets typed: hands are already on it. Enter must be the Look up
  // click, not a second, emptier page.
  const flow = await startLoginFlow(page, "e2e-keyboard");
  await page.goto("/verify");
  await page.getByLabel("Code").fill(flow.user_code);
  await page.getByLabel("Code").press("Enter");
  await expect(page.getByText("\u201ce2e-keyboard\u201d", { exact: true })).toBeVisible();
});

test("an unknown code is an honest in-page state, never a 404", async ({ page }) => {
  await lookUp(page, "ZZZZ-9999");
  await expect(page.getByRole("heading", { name: "Approve a login" })).toBeVisible();
  await expect(page.getByText("No pending request for that code")).toBeVisible();
});

test("the ONE-SEAT one-click: approve records the choice and THE POLL mints the credential", async ({
  page,
}) => {
  const flow = await startLoginFlow(page, "e2e-laptop");
  // The reworked start: the code never enters ANY URL — the retired `verification_uri_complete`
  // (which embedded the code in a GET) is gone, and `verification_uri` is the bare /verify page.
  expect(flow.raw.verification_uri_complete).toBeUndefined();
  expect(String(flow.raw.verification_uri).endsWith("/verify")).toBe(true);
  expect(String(flow.raw.verification_uri)).not.toContain("code");

  // Before approval the terminal's poll is still pending.
  expect((await pollLoginFlow(page, flow.device_code)).status).toBe("pending");

  await lookUp(page, flow.user_code);
  // The resolved request names what is asking, honestly, shows the CODE for the terminal
  // glance-check, and — one seat, no invitations — names THE workspace with no radio in
  // sight: the one primary button IS the whole choice. One click total.
  await expect(page.getByText("“e2e-laptop”", { exact: true })).toBeVisible();
  await expect(page.getByText("wants to connect as you", { exact: false })).toBeVisible();
  await expect(page.getByText(flow.user_code, { exact: false }).first()).toBeVisible();
  await expect(page.getByText("Approving connects it to", { exact: false })).toBeVisible();
  await expect(
    page.getByText("Any further workspace is its own login", { exact: false }),
  ).toBeVisible();
  expect(await page.getByRole("radio").count()).toBe(0);

  await page.getByRole("button", { name: "Connect and approve this device" }).click();
  // HONEST success copy: nothing is minted yet — the machine finishes on its next poll.
  await expect(page.getByRole("heading", { name: "Approved" })).toBeVisible();
  await expect(page.getByText("finishes connecting on its next poll")).toBeVisible();

  // No session exists until the exchange — THIS flow's row still names none (a display-name
  // count would trip over a reused local database's earlier runs).
  const before = await adminQuery<{ session_id: string | null }>(
    `select session_id from web.login_flow where user_code = $1`,
    [flow.user_code],
  );
  expect(before[0]?.session_id).toBeNull();

  // …THE POLL MINTS: the presented device_code IS the promoted credential.
  const granted = await pollLoginFlow(page, flow.device_code);
  expect(granted.status).toBe("granted");
  expect(granted.credential).toBe(flow.device_code);
  expect(granted.workspace?.name).toBe(WORKSPACE_ADDRESS);

  // The minted session row: owned by the approver, named as requested, hash-stored credential.
  const rows = await adminQuery<{ id: string; display_name: string; email: string }>(
    `select s.id, s.display_name, u.email
     from web.cli_session s join web."user" u on u.id = s.user_id
     where s.id = $1`,
    [granted.session_id],
  );
  expect(rows[0]?.display_name).toBe("e2e-laptop");
  expect(rows[0]?.email).toBe(MEMBER_EMAIL);

  // The grant REPEATS (idempotent): a re-poll after a crash re-delivers the same credential and
  // the SAME session, so a client that crashed before persisting recovers by polling again.
  const rePoll = await pollLoginFlow(page, flow.device_code);
  expect(rePoll.status).toBe("granted");
  expect(rePoll.credential).toBe(flow.device_code);
  expect(rePoll.session_id).toBe(granted.session_id);
});

test("deny destroys the pending request and mints nothing", async ({ page }) => {
  const flow = await startLoginFlow(page, "e2e-stranger");
  await lookUp(page, flow.user_code);
  await expect(page.getByText("“e2e-stranger”", { exact: true })).toBeVisible();

  await page.getByRole("button", { name: "Deny — this isn’t me" }).click();
  await expect(page.getByRole("heading", { name: "Request denied" })).toBeVisible();

  // The machine learns the denial on its next poll — repeatably (terminal answers are delivered
  // idempotently until the expiry sweep reaps the row).
  expect((await pollLoginFlow(page, flow.device_code)).status).toBe("denied");
  expect((await pollLoginFlow(page, flow.device_code)).status).toBe("denied");

  // Nothing was minted for the denied request.
  const rows = await adminQuery<{ n: string }>(
    `select count(*)::text as n from web.cli_session where display_name = 'e2e-stranger'`,
  );
  expect(rows[0]?.n).toBe("0");
});

test("the LOOPBACK login: the card pre-arms, the redirect carries NO secret, the poll mints", async ({
  page,
}) => {
  // The RFC 8252-shaped accelerator against a REAL 127.0.0.1 listener — bind, declare,
  // pre-armed approve, state-bound wake-up redirect, poll-minted session. The redirect is a
  // PURE accelerator now: state + outcome only, no code, no second secret.
  const { createServer } = await import("node:http");
  const { createHash, randomUUID } = await import("node:crypto");

  let delivered: URL | undefined;
  const server = createServer((req, res) => {
    delivered = new URL(req.url ?? "/", "http://127.0.0.1");
    res.writeHead(200, { "content-type": "text/plain" });
    res.end("ok");
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as { port: number }).port;
  const state = randomUUID().replace(/-/g, "");

  try {
    // The CLI declares `loopback` because its listener is already bound.
    const flow = await startLoginFlow(page, "e2e-loopback", { redirect: "loopback" });
    const challenge = createHash("sha256").update(flow.device_code, "utf8").digest("hex");

    // The card PRE-ARMS from the challenge — no typing (loopback flows only).
    await page.goto(`/verify?device=${challenge}&port=${port}&state=${state}`);
    await expect(page.getByText("“e2e-loopback”", { exact: true })).toBeVisible();

    // Before approval, the poll is pending and no session exists.
    expect((await pollLoginFlow(page, flow.device_code)).status).toBe("pending");

    await page.getByRole("button", { name: "Connect and approve this device" }).click();
    await expect.poll(() => delivered !== undefined, { timeout: 10_000 }).toBe(true);

    // The redirect landed on OUR listener, state-bound — and carries NOTHING but the outcome:
    // the poll is the one completion mechanism, so no secret rides a URL.
    expect(delivered?.pathname).toBe("/cb");
    expect(delivered?.searchParams.get("state")).toBe(state);
    expect(delivered?.searchParams.get("outcome")).toBe("approved");
    expect(delivered?.searchParams.get("code")).toBeNull();

    // THE POLL MINTS — the device code alone redeems once a human has approved.
    const granted = await pollLoginFlow(page, flow.device_code);
    expect(granted.status).toBe("granted");
    expect(granted.credential).toBe(flow.device_code);

    // Idempotent: the re-poll re-delivers instead of stranding a crash mid-persist.
    expect((await pollLoginFlow(page, flow.device_code)).status).toBe("granted");
  } finally {
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
});

test("a pending invitation SURFACES ON THE CHOOSER and its accept connects", async ({
  browser,
}) => {
  // A zero-seat person with a pending invitation: the chooser leads with it — accepting seats
  // them AND connects the machine in one act.
  const email = "chooser-invitee@e2e.test";
  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const page = await context.newPage();
  try {
    await signIn(page, email);
    const ws = await theWorkspace();
    await adminQuery(`update web."user" set email_verified = true where email = $1`, [email]);
    // Idempotent across local re-runs: the invitee must arrive SEATLESS (an earlier run's
    // accept seated them — the seat delete cascades their sessions too), and a consumed
    // invitation flips back to pending WHOLE (the accepted_* pair must clear with it — the
    // invitation CHECK ties the three).
    await adminQuery(
      `delete from web.seat where user_id in (select id from web."user" where email = $1)`,
      [email],
    );
    await adminQuery(
      `insert into web.invitation (id, workspace_id, email, role, status)
       values ('inv_e2e_chooser', $1, $2, 'member', 'pending')
       on conflict (id) do update
         set status = 'pending', accepted_by = null, accepted_at = null`,
      [ws.id, email],
    );

    const flow = await startLoginFlow(page, "invitee-laptop");
    await lookUp(page, flow.user_code);
    await expect(page.getByText("“invitee-laptop”", { exact: true })).toBeVisible();
    await expect(page.getByText("You’re invited to", { exact: false }).first()).toBeVisible();

    await page.getByRole("button", { name: "Accept and connect" }).click();
    await expect(page.getByRole("heading", { name: "Approved" })).toBeVisible();

    // The poll mints into the invited workspace; the invitation is consumed and the seat stands.
    const granted = await pollLoginFlow(page, flow.device_code);
    expect(granted.status).toBe("granted");
    expect(granted.workspace?.name).toBe(WORKSPACE_ADDRESS);
    const seats = await adminQuery<{ role: string }>(
      `select s.role from web.seat s join web."user" u on u.id = s.user_id where u.email = $1`,
      [email],
    );
    expect(seats[0]?.role).toBe("member");
    const consumed = await adminQuery<{ status: string }>(
      `select status from web.invitation where id = 'inv_e2e_chooser'`,
    );
    expect(consumed[0]?.status).toBe("accepted");
  } finally {
    await context.close();
  }
});

test("session-approval knob on: a member's approval births the session PENDING", async ({
  browser,
}) => {
  // The receipt path: the knob holds a member-minted session until an owner approves — the
  // card says so up front, and the poll answers session_status=pending.
  const email = "pending-member@e2e.test";
  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const page = await context.newPage();
  try {
    await signIn(page, email);
    const ws = await theWorkspace();
    await adminQuery(
      `insert into web.seat (workspace_id, user_id, role)
       select $1, id, 'member' from web."user" where email = $2
       on conflict (workspace_id, user_id) do update set role = 'member'`,
      [ws.id, email],
    );
    await adminQuery(`update web.workspace set session_approval = 'on' where id = $1`, [ws.id]);

    const flow = await startLoginFlow(page, "held-laptop");
    await lookUp(page, flow.user_code);
    // The static disclosure rides the card BEFORE the click — no surprise pending state.
    await expect(page.getByText("Session approval is on", { exact: false })).toBeVisible();
    await page.getByRole("button", { name: "Connect and approve this device" }).click();
    await expect(page.getByRole("heading", { name: "Approved" })).toBeVisible();

    const granted = await pollLoginFlow(page, flow.device_code);
    expect(granted.status).toBe("granted");
    expect(granted.session_status).toBe("pending");
  } finally {
    const ws = await theWorkspace();
    await adminQuery(`update web.workspace set session_approval = 'off' where id = $1`, [ws.id]);
    await context.close();
  }
});

test("FRAGMENTATION RESUME: a magic link consumed in a fresh browser resolves the card and completes", async ({
  page,
  browser,
}) => {
  // The whole point of mint-at-exchange: approval ANYWHERE completes. The flow starts here;
  // the approval happens in a SECOND browser context (fresh cookie jar — another device, in
  // effect) whose session arrives via the mailed magic link, whose callback resumes the
  // canonical /verify path. The CLI's poll then mints as if nothing unusual happened.
  const { createHash } = await import("node:crypto");
  const flow = await startLoginFlow(page, "resumed-laptop", { redirect: "loopback" });
  const challenge = createHash("sha256").update(flow.device_code, "utf8").digest("hex");

  const second = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const other = await second.newPage();
  try {
    // Signed out, the verify arrival bounces to /login carrying the canonical next — and the
    // FIRST screen already shows the glance code (the terminal's waiting line says "the page
    // shows the same code", which must be true from first paint, not only after sign-in).
    await other.goto(`/verify?device=${challenge}`);
    await other.waitForURL((u) => u.pathname === "/login");
    const hint = `A device is waiting to connect — code ${flow.user_code}. Sign in to approve it.`;
    await expect(other.getByText(hint)).toBeVisible();
    // Ask for the magic link (mail is armed suite-wide; the sink records the send).
    await other.getByLabel("Email").fill(MEMBER_EMAIL);
    await other.getByRole("button", { name: "Email me a sign-in link" }).click();
    await expect(other.getByText("Check your email")).toBeVisible();
    // The hint (with the code) PERSISTS on the sent card — visible through the whole
    // pre-approval stretch in this tab.
    await expect(other.getByText(hint)).toBeVisible();
    const mail = await latestMail("magic-link", MEMBER_EMAIL);
    const link = mail.text.match(/https?:\/\/\S+/)?.[0];
    expect(link, "the magic-link mail carries the sign-in URL").toBeTruthy();

    // Consuming the link in THIS fresh jar signs in and lands back on /verify — where the
    // challenge resolves the card with zero typing (a loopback flow).
    await other.goto(String(link).replace(/^https?:\/\/[^/]+/, BASE_URL));
    await other.waitForURL((u) => u.pathname === "/verify");
    await expect(other.getByText("“resumed-laptop”", { exact: true })).toBeVisible();
    // The card's glance line shows THE SAME code the /login hint promised.
    await expect(other.getByText(flow.user_code)).toBeVisible();
    await other.getByRole("button", { name: "Connect and approve this device" }).click();
    await expect(other.getByRole("heading", { name: "Approved" })).toBeVisible();
  } finally {
    await second.close();
  }

  // The waiting terminal — which never saw that browser — completes on its next poll.
  const granted = await pollLoginFlow(page, flow.device_code);
  expect(granted.status).toBe("granted");
  expect(granted.credential).toBe(flow.device_code);
});
