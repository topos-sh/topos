//! E2E — the first-boot claim door + the registration knob, over the real stack:
//!
//! - boot mints the workspace and PRINTS the claim link (mirrored to `TOPOS_SETUP_LINK_FILE`); the
//!   printed link claims the workspace — the first account, seated as the first owner, signed in
//!   by the ceremony itself;
//! - a consumed code and a wrong code are the SAME uniform miss (GET and POST, byte-identical) —
//!   the door is single-use by construction and never an oracle;
//! - registration stays CLOSED (one constant, non-enumerating refusal) until the owner flips the
//!   knob through the REAL settings ceremony (step-up gated, audit-rowed); an uninvited sign-up
//!   then lands an ACCOUNT — never a seat.

mod common;

use common::{OWNER_EMAIL, SETUP_CODE};

// ── boot → the printed link → claim → consumed/stale codes are the uniform miss ────────────────────

#[test]
fn e2e_the_printed_claim_link_claims_once_then_the_door_is_a_uniform_miss() {
    let stack = common::start_stack("claim");

    // The printed line landed in the mirror file and carries the preset code.
    let printed = std::fs::read_to_string(&stack.setup_link_file).expect("the setup line file");
    assert!(
        printed.contains(&format!("/claim?code={SETUP_CODE}")),
        "the printed line carries the claim link: {printed}"
    );
    assert!(
        printed.contains("Finish setup"),
        "the honest label: {printed}"
    );

    // A WRONG code is the uniform miss BEFORE the claim (no existence probe on the door).
    let anon = common::Session::new(&stack.origin);
    let wrong_before = anon.get("/claim?code=not-the-real-code-000000");
    assert_eq!(wrong_before.status, 404);
    let bare = anon.get("/claim");
    assert_eq!(bare.status, 404, "no code — the same miss");

    // The claim: creates the first account, seats it as OWNER, lands signed in. Single-tenant: the
    // origin root IS the workspace dashboard — the claimant reaches it directly (there is no
    // `/workspaces` index or `/workspaces/<id>` shell anymore).
    let owner = stack.claim_owner(OWNER_EMAIL);
    let shell = owner.get("/");
    assert_eq!(
        shell.status, 200,
        "the claimant reaches the origin-rooted workspace dashboard"
    );
    let owner_id = stack.user_id(OWNER_EMAIL);
    assert_eq!(
        stack.text_witness(&format!(
            "SELECT role FROM web.seat WHERE user_id = '{owner_id}'"
        )),
        Some("owner".to_owned()),
        "the first seat is the owner"
    );
    assert_eq!(
        stack.count("SELECT count(*) FROM web.workspace WHERE claimed_at IS NOT NULL AND claim_code_sha256 IS NULL"),
        1,
        "the consume set claimed_at and CLEARED the hash in one statement"
    );

    // CONSUMED: the very same code is now the uniform miss — GET and POST, byte-identical to the
    // wrong-code miss.
    let spent_get = anon.get(&format!("/claim?code={SETUP_CODE}"));
    assert_eq!(spent_get.status, 404, "the consumed code is dead");
    let wrong_get = anon.get("/claim?code=another-wrong-code-000000");
    assert_eq!(
        spent_get.body, wrong_get.body,
        "consumed == wrong, byte-for-byte"
    );
    let spent_post = anon.post_form(
        &format!("/claim?code={SETUP_CODE}"),
        &[
            ("code", SETUP_CODE),
            ("name", "late"),
            ("email", "late@acme.test"),
            ("password", common::PASSWORD),
        ],
    );
    assert_eq!(spent_post.status, 404, "the POST arm refuses identically");
    assert_eq!(
        stack.count("SELECT count(*) FROM web.\"user\""),
        1,
        "the spent-code POST created nothing"
    );
}

// ── registration: closed with ONE constant refusal; the settings ceremony opens it ─────────────────

#[test]
fn e2e_the_registration_knob_admits_an_uninvited_signup_only_after_the_ceremony() {
    let stack = common::start_stack("regknob");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // CLOSED (the default): an uninvited sign-up is refused, no account row lands, and the
    // wire answer enumerates NOTHING (a generic create failure — same body whatever the cause).
    let refused = stack.sign_up_expect_refused("charlie@acme.test");
    assert_ne!(
        refused.status, 200,
        "the sign-up is refused: {}",
        refused.body
    );
    assert!(
        !refused.body.to_lowercase().contains("invit"),
        "the wire refusal names no cause: {}",
        refused.body
    );
    assert_eq!(
        stack.count("SELECT count(*) FROM web.\"user\" WHERE email = 'charlie@acme.test'"),
        0,
        "no account row for a refused sign-up"
    );
    // The HUMAN copy is the login page's — ONE constant string whatever failed, served to the
    // form via its loader (the page enumerates nothing either).
    let login = common::Session::new(&stack.origin).get("/login");
    assert!(
        login.body.contains("Sign-up is not open on this server"),
        "the login page carries the constant refusal copy"
    );

    // A WRONG step-up cannot flip the knob (the ceremony re-authenticates the actor). The settings
    // page is origin-rooted in single-tenant mode, and its step-up rung is UNCHANGED.
    let bad = owner.post_form(
        "/settings",
        &[
            ("intent", "set-registration"),
            ("registration", "open"),
            ("stepup_password", "not-the-password"),
        ],
    );
    assert_eq!(bad.status, 200, "a refused step-up renders the form error");
    assert_eq!(
        stack.text_witness("SELECT registration FROM web.workspace"),
        Some("invite_only".to_owned()),
        "the knob did not move on a refused step-up"
    );

    // The REAL ceremony: owner session + step-up → the knob flips, the audit row lands.
    let flipped = owner.post_form(
        "/settings",
        &[
            ("intent", "set-registration"),
            ("registration", "open"),
            ("stepup_password", common::PASSWORD),
        ],
    );
    assert_eq!(flipped.status, 200, "the ceremony lands: {}", flipped.body);
    assert_eq!(
        stack.text_witness("SELECT registration FROM web.workspace"),
        Some("open".to_owned()),
        "the knob is open"
    );
    assert!(
        stack.count(
            "SELECT count(*) FROM web.audit_event WHERE kind = 'policy_registration' AND outcome = 'ok'"
        ) >= 1,
        "the flip is audit-rowed"
    );

    // OPEN: the same sign-up now lands an ACCOUNT — and an account is NOT a seat.
    let charlie = stack.sign_up("charlie@acme.test");
    assert!(charlie.signed_in(), "the admitted sign-up lands a session");
    let charlie_id = stack.user_id("charlie@acme.test");
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.seat WHERE user_id = '{charlie_id}'"
        )),
        0,
        "an open-registration account receives no seat"
    );
}
