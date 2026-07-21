import { expect, test } from "@playwright/test";
import { adminQuery, ensureBundle, latestMail, theWorkspace } from "./seed";
import { gotoSettled } from "./sign-in";

/**
 * The skill face's "invite a teammate to this skill" affordance. An owner expands the quiet
 * button into a one-email form; submitting mints an invitation whose FIRST destination is THIS
 * skill — the invitation row carries the skill's bundle id as its hint, and the notice mail leads
 * with the skill in its subject. The suite runs MAIL-ARMED (all five TOPOS_MAIL_SMTP_* point at
 * the sink, so `canSend` is true), so the affordance is enabled and the "sent" copy lands in the
 * dev outbox. The default storage state is the claimed OWNER; the bare bundle (no published
 * version — the affordance renders regardless) is this file's own arrangement.
 */

const SKILL_ID = "s_e2e_skill_invite";
const SKILL = "invite-target";
// Fresh per run: a reused local database must never hand this walk an already-pending row.
const INVITED = `skill-invitee-${Date.now().toString(36)}@example.com`;

test.beforeAll(async () => {
  await ensureBundle({ id: SKILL_ID, name: SKILL });
  await adminQuery(`delete from web.invitation where email = $1`, [INVITED]);
});

test("invite to this skill: the collapsed affordance, the hinted row, the skill-led mail", async ({
  page,
}) => {
  await theWorkspace();
  await gotoSettled(page, `/skills/${SKILL}`);

  // The affordance is a quiet, collapsed button; expanding it reveals the one-email form.
  await page.getByRole("button", { name: "Invite a teammate" }).click();
  await page.getByLabel("Invite by email").fill(INVITED);
  await page.getByRole("button", { name: "Invite", exact: true }).click();

  // (a) The reply confirmation names the invited address and says the mail leads with the skill.
  await expect(page.getByRole("status").filter({ hasText: `Invited ${INVITED}` })).toContainText(
    "the mail leads with this skill",
  );

  // (b) The invitation row carries THIS skill's bundle id as its first-destination hint, and a
  // single-use link token (only its hash is stored).
  const rows = await adminQuery<{ hint_bundle_id: string | null; token_sha256: unknown }>(
    `select hint_bundle_id, token_sha256 from web.invitation where email = $1`,
    [INVITED],
  );
  expect(rows[0]?.hint_bundle_id).toBe(SKILL_ID);
  expect(rows[0]?.token_sha256).toBeTruthy();

  // (c) The notice mail LEADS with the skill in its subject and carries the tokened /invite/ link.
  const mail = await latestMail("invite", INVITED);
  expect(mail.subject).toContain(`starting with the ${SKILL} skill`);
  expect(mail.text).toContain("/invite/");
});
