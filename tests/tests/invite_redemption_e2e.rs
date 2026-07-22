//! The INVITATION-REDEMPTION loop over the composed stack — the terminal-first invited person:
//!
//!   invite (device lane, with a first-destination skill hint) → the mailed tokened link →
//!   `topos follow <invite-url>` (the device flow CARRYING the token) → the ONE-visit browser
//!   weave (account minted passwordlessly on the invitation page → the invitation accepted →
//!   the device approved at /verify, resolved by the flow challenge with zero typing) → the
//!   enrolled resume describing the HINTED skill → and the bytes landing ONLY after the
//!   device-side `--yes` consent.
//!
//! Plus the already-enrolled arm: a member's device consuming an invite URL accepts directly
//! over the device lane — no browser, no new device — and continues into the hint's describe.
//!
//! The app runs MAIL-ARMED here (dummy relay coordinates; `APP_ENV=test` records every mail to
//! `web/.invite-emails.jsonl` instead of dialing) because inviting requires the mail rung and
//! the invitation page's account mint rides the magic-link rung the composition arms with it.

mod common;

use topos::test_support::FollowHarness;

const OWNER_EMAIL: &str = "owner@acme.test";
const SKILL: &str = "s-deploy";

/// The genesis bundle the author publishes (SKILL.md + an executable script).
fn genesis_files() -> Vec<(&'static str, bool, &'static [u8])> {
    vec![
        (
            "SKILL.md",
            false,
            b"# deploy runbook\nrun it right\n" as &[u8],
        ),
        ("run.sh", true, b"#!/bin/sh\necho ok\n"),
    ]
}

/// The mail-armed stack with a claimed owner, an enrolled author device (owned by the owner),
/// and the genesis skill published — the arrangement both tests start from.
fn stack_with_genesis(tag: &str) -> (common::Stack, common::Session, FollowHarness) {
    let stack = common::start_stack_mailed(tag);
    let owner = stack.claim_owner(OWNER_EMAIL);
    let author = FollowHarness::new(&format!("{tag}-author"));
    stack.enroll_begin_and_approve(&author, &owner);
    let applied = author.resume_apply().expect("the author's resume applies");
    assert!(applied.enrolled_now, "the author's device enrolled");
    author.adopt(SKILL, &genesis_files());
    let digest = author.draft_digest(SKILL);
    author
        .publish_message("", &format!("{SKILL}@{digest}"), "genesis: deploy runbook")
        .expect("the genesis publish lands");
    (stack, owner, author)
}

/// The recorded invitation mail for `to` — the dev-outbox stand-in for the recipient's mailbox
/// (`web/.invite-emails.jsonl`, shared across suites; the LAST line for the address wins).
fn recorded_invite_url(to: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("web")
        .join(".invite-emails.jsonl");
    let text = std::fs::read_to_string(&path).expect("the recorded invite mail file exists");
    let line = text
        .lines()
        .rev()
        .find(|l| l.contains(&format!("\"to\":\"{to}\"")))
        .expect("an invite mail was recorded for the address");
    let v: serde_json::Value = serde_json::from_str(line).expect("a JSON mail line");
    v["inviteUrl"]
        .as_str()
        .expect("the mail carries the tokened invite URL")
        .to_owned()
}

/// Mint one invitation over the device lane, with an optional skill hint; return the mailed URL.
fn invite_via_lane(
    stack: &common::Stack,
    credential: &str,
    email: &str,
    skill_hint: Option<&str>,
) -> String {
    let mut body = serde_json::json!({ "emails": [email] });
    if let Some(skill) = skill_hint {
        body["skill"] = serde_json::Value::String(skill.to_owned());
    }
    let resp = stack.device_post_json(
        Some(credential),
        &format!("/v1/workspaces/{}/invitations", stack.workspace_id),
        &body,
    );
    assert_eq!(resp.status, 200, "the invite lands: {}", resp.body);
    let env: serde_json::Value = serde_json::from_str(&resp.body).expect("invite envelope");
    assert_eq!(env["ok"], true, "the invite envelope is OK: {}", resp.body);
    assert_eq!(env["data"]["mailed"], true, "the server mailed the link");
    recorded_invite_url(email)
}

// ── the keystone: terminal-first invited person, one browser visit, consent-gated install ─────────

#[test]
fn e2e_the_terminal_first_invited_person_weaves_one_visit_and_installs_after_consent() {
    let invitee_email = "terminal-invitee@acme.test";
    let (stack, owner, _author) = stack_with_genesis("invredeem");

    // The inviter's probe device (the CLI-shaped lane call), hinting the genesis skill.
    let inviter = stack.mint_device(&owner, "inviter probe");
    let invite_url = invite_via_lane(&stack, &inviter.credential, invitee_email, Some(SKILL));
    assert!(
        invite_url.starts_with(&stack.origin) && invite_url.contains("/invite/"),
        "the mailed link is this origin's tokened invitation URL: {invite_url}"
    );

    // `topos follow <invite-url>` — the device flow starts CARRYING the token; the browser
    // destination is the INVITATION page with the flow challenge (never the code).
    let invitee = FollowHarness::new("invredeem-invitee");
    let pending = invitee.follow(&invite_url).expect("follow call 1 begins");
    assert!(!pending.enrolled);
    let pending = pending.pending.expect("the pending verification handle");
    assert!(
        pending.verification_uri.starts_with(&invite_url)
            && pending.verification_uri.contains("?device="),
        "the browser destination is the invitation page + the flow challenge: {}",
        pending.verification_uri
    );
    assert!(
        !pending.verification_uri.contains(&pending.user_code),
        "the code never rides a URL"
    );

    // The ONE browser visit, driven over raw HTTP. Step 1: the invitation page's account-mint
    // arm (mail is armed, so it is PASSWORDLESS — the token's delivery is the proof).
    let weave_path = pending
        .verification_uri
        .strip_prefix(&stack.origin)
        .expect("the weave URL is app-origin")
        .to_owned();
    let browser = common::Session::new(&stack.origin);
    let page = browser.get(&weave_path);
    assert_eq!(
        page.status, 200,
        "the invitation page renders: {}",
        page.body
    );
    assert!(
        page.body.contains("Accept and create my account"),
        "the account-mint arm renders (passwordless)"
    );
    assert!(
        !page.body.contains("Choose a password"),
        "no password field on a mail-rung deployment"
    );
    assert!(
        page.body.contains("First up") && page.body.contains(SKILL),
        "the summary leads with the hinted skill: {}",
        page.body
    );
    assert!(
        page.body.contains(invitee_email),
        "the page names the invited address to the token holder"
    );

    // Step 2: accept — mints the account (born verified), consumes the invitation, seats the
    // person, writes the hint follow, and CONTINUES to /verify carrying the challenge.
    let accepted = browser.post_form(
        &weave_path,
        &[("intent", "accept-new"), ("name", "Terminal Invitee")],
    );
    assert_eq!(
        accepted.status, 302,
        "the accept redirects: {}",
        accepted.body
    );
    let location = accepted.location.clone().expect("a redirect target");
    assert!(
        location.starts_with("/verify?device="),
        "the weave continues into the device approval: {location}"
    );

    // Step 3: /verify resolves the flow by challenge (zero typing), shows the code for the
    // glance-check, and the now-seated invitee approves.
    let verify = browser.get(&location);
    assert_eq!(
        verify.status, 200,
        "the approval card renders: {}",
        verify.body
    );
    assert!(
        verify.body.contains(&pending.user_code),
        "the resolved card shows the code for the glance-check"
    );
    // The device-link card: the approval mints registration + THE ONE workspace link (no
    // whole-reach list — every further workspace takes its own explicit link from the device).
    assert!(
        verify.body.contains("Approving links it to"),
        "the card names the one workspace being linked: {}",
        verify.body
    );
    assert!(
        verify.body.contains("explicit link"),
        "the card says further workspaces each take their own link: {}",
        verify.body
    );
    let approved = browser.post_form(
        "/verify",
        &[("intent", "approve"), ("code", &pending.user_code)],
    );
    assert_eq!(
        approved.status, 200,
        "the approval lands: {}",
        approved.body
    );
    assert!(approved.body.contains("connected"), "{}", approved.body);

    // The row witnesses: the account is born VERIFIED, the seat stands, the invitation is
    // consumed, and the hint follow rides the accept.
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.\"user\" WHERE email = '{invitee_email}' AND email_verified"
        )),
        1,
        "the token's delivery to the mailbox IS the verification"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.seat s JOIN web.\"user\" u ON u.id = s.user_id \
             WHERE u.email = '{invitee_email}' AND s.role = 'member'"
        )),
        1
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.invitation WHERE email = '{invitee_email}' \
             AND status = 'accepted'"
        )),
        1
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.bundle_subscription bs \
             JOIN web.\"user\" u ON u.id = bs.user_id \
             WHERE u.email = '{invitee_email}' AND bs.state = 'following'"
        )),
        1,
        "the hint subscribed — nothing landed on any device from the web accept"
    );

    // The resume: the grant carries the HINT, so the describe targets the invited-to skill —
    // and NOTHING has installed yet (the two-phase consent still gates the bytes).
    let describe = invitee
        .resume_describe()
        .expect("the granted resume describes");
    assert!(describe.enrolled_now, "the resume persisted the enrollment");
    assert_eq!(
        describe.targets,
        vec![("skill".to_owned(), SKILL.to_owned())],
        "the subscribe targets the hinted skill, not the whole workspace"
    );
    assert_eq!(describe.installs.len(), 1, "{:?}", describe.installs);
    assert_eq!(describe.installs[0].name, SKILL);
    assert_eq!(
        invitee.follows_count(),
        0,
        "no local follow row before the consent"
    );

    // The consent: `--yes` on the described target lands the genesis byte-exact.
    let applied = invitee
        .follow_apply(&format!("{}/skills/{SKILL}", common::WS_NAME))
        .expect("the --yes apply lands the hinted skill");
    assert_eq!(applied.installed.len(), 1, "{:?}", applied.installed);
    assert_eq!(applied.installed[0].name, SKILL);
    let placed = invitee.placement_files(&applied.installed[0].skill_id);
    let skill_md = placed
        .iter()
        .find(|(p, _, _)| p == "SKILL.md")
        .expect("SKILL.md placed");
    assert_eq!(
        skill_md.2,
        genesis_files()[0].2.to_vec(),
        "the genesis bytes land byte-exact, only after the consent"
    );
}

// ── the already-enrolled arm: the device lane accepts directly, no browser ────────────────────────

#[test]
fn e2e_an_enrolled_device_consumes_an_invite_url_directly() {
    let (stack, owner, author) = stack_with_genesis("invdirect");

    // The owner's account must be mailbox-proven for a lane accept (the browser weave proves it
    // via the token; an enrolled device presents no token-holder ceremony, so the standing fence
    // applies). The claim ceremony verifies no mailbox — arrange the fact directly.
    stack
        .rt
        .block_on(
            sqlx::query("UPDATE web.\"user\" SET email_verified = true WHERE email = $1")
                .bind(OWNER_EMAIL)
                .execute(&stack.pool),
        )
        .expect("mark the owner's mailbox proven");

    // Invite the OWNER's own address with a skill hint (an existing member being pointed at a
    // first destination — the accept is idempotent on the seat and still delivers the hint).
    let probe = stack.mint_device(&owner, "inviter probe");
    let invite_url = invite_via_lane(&stack, &probe.credential, OWNER_EMAIL, Some(SKILL));

    let devices_before = stack.count("SELECT count(*) FROM web.device WHERE revoked_at IS NULL");

    // The ENROLLED author device consumes the URL: the accept rides the device lane (no browser,
    // no new device flow) and the verb continues into the hint's two-phase describe.
    let describe = author
        .follow_describe(&invite_url)
        .expect("the direct accept continues into the describe");
    assert_eq!(
        describe.targets,
        vec![("skill".to_owned(), SKILL.to_owned())],
        "the describe targets the hinted skill"
    );

    // No browser ceremony ran: no new device row, no pending flow — and the invitation consumed.
    assert_eq!(
        stack.count("SELECT count(*) FROM web.device WHERE revoked_at IS NULL"),
        devices_before,
        "no device flow started"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.invitation WHERE email = '{OWNER_EMAIL}' \
             AND status = 'accepted'"
        )),
        1
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.bundle_subscription bs \
             JOIN web.\"user\" u ON u.id = bs.user_id WHERE u.email = '{OWNER_EMAIL}' \
             AND bs.state = 'following'"
        )),
        1,
        "the hint follow landed with the accept"
    );
}
