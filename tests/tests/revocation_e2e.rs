//! E2E — the revocation story on the unified-identity model: a device's self-revoke ends its lane
//! IMMEDIATELY (the very next request under the dead credential is the uniform 404), revocation is
//! FINAL (the DB trigger refuses any un-revoke — rotation is revoke + re-enroll), and a seat
//! removal ends delivery IN THE SAME REQUEST (the removal via the app's members ceremony; the next
//! delivery poll 404s; the client's sweep fails CLOSED into a freeze — placements intact, never a
//! clean — and resumes when the person is re-seated).

mod common;

use common::{OWNER_EMAIL, SKILL, expected, genesis_files};
use topos::test_support::{FollowHarness, PublishResult};

const MEMBER_EMAIL: &str = "dave@acme.test";

// ── self-revoke: immediate, final, recoverable only by re-enrolling ─────────────────────────────────

#[test]
fn e2e_self_revoke_is_immediate_final_and_re_enrollment_recovers() {
    let stack = common::start_stack("revoke");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // A probe device of the owner: alive, then self-revoked over the device lane.
    let probe = stack.mint_device(&owner, "laptop probe");
    let me = stack.device_get(
        &probe.credential,
        &format!("/v1/workspaces/{}/me", stack.workspace_id),
    );
    assert_eq!(me.status, 200, "the live credential reads: {}", me.body);

    let revoked = stack.device_delete(
        &probe.credential,
        &format!("/v1/workspaces/{}/devices", stack.workspace_id),
        Some(&serde_json::json!({
            "op_id": "a0000000-0000-4000-8000-0000000000aa",
            "target_device_key_id": probe.device_id,
        })),
    );
    assert_eq!(
        revoked.status, 200,
        "the self-revoke lands: {}",
        revoked.body
    );

    // IMMEDIATE: the very next request under the dead credential is the uniform 404.
    let after = stack.device_get(
        &probe.credential,
        &format!("/v1/workspaces/{}/me", stack.workspace_id),
    );
    assert_eq!(
        after.status, 404,
        "the revoked lane is dead: {}",
        after.body
    );

    // FINAL: no code path can un-revoke — the trigger refuses the row flip outright.
    let unrevoke = stack.rt.block_on(
        sqlx::query("UPDATE web.device SET revoked_at = NULL WHERE id = $1")
            .bind(&probe.device_id)
            .execute(&stack.pool),
    );
    let err = unrevoke.expect_err("the un-revoke is refused").to_string();
    assert!(
        err.contains("stays revoked"),
        "the trigger names the refusal: {err}"
    );

    // RECOVERY: re-enrolling mints a FRESH device (a new row, a new credential) that works.
    let fresh = stack.mint_device(&owner, "laptop probe (re-enrolled)");
    assert_ne!(fresh.device_id, probe.device_id, "a new device identity");
    let me = stack.device_get(
        &fresh.credential,
        &format!("/v1/workspaces/{}/me", stack.workspace_id),
    );
    assert_eq!(me.status, 200, "the re-enrolled lane works: {}", me.body);
}

// ── the CLI's own logout: best-effort self-revoke + the credential doc deleted ──────────────────────

#[test]
fn e2e_cli_logout_revokes_and_keeps_the_bytes() {
    let stack = common::start_stack("logout");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // An authoring CLI with a landed skill (so the logout has bytes to KEEP).
    let author = FollowHarness::new("logout-author");
    stack.enroll_begin_and_approve(&author, &owner);
    author.resume_apply().expect("the author enrolls");
    author.adopt(SKILL, &genesis_files());
    let digest = author.draft_digest(SKILL);
    match author
        .publish_message("", &format!("{SKILL}@{digest}"), "genesis")
        .expect("the genesis lands")
    {
        PublishResult::Published(_) => {}
        other => panic!("expected a direct genesis, got {other:?}"),
    }
    let device_id = author.device_id().expect("enrolled");

    let _ = author.auth_logout().expect("the logout applies");
    assert!(
        author.device_id().is_none(),
        "the credential doc is deleted with the logout"
    );
    assert_eq!(
        stack.text_witness(&format!(
            "SELECT CASE WHEN revoked_at IS NULL THEN 'live' ELSE 'revoked' END \
             FROM web.device WHERE id = '{device_id}'"
        )),
        Some("revoked".to_owned()),
        "the logout's best-effort self-revoke landed"
    );
    // Skills, drafts, and the adopted placement STAY — no credential is signed-out, not removed.
    assert_eq!(author.placement_files(SKILL), expected(&genesis_files()));
}

// ── seat removal ends delivery in the same request; re-seating resumes it ───────────────────────────

#[test]
fn e2e_seat_removal_ends_delivery_in_the_same_request() {
    let stack = common::start_stack("unseat");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // The genesis (so the member's device holds bytes worth freezing).
    let author = FollowHarness::new("unseat-author");
    stack.enroll_begin_and_approve(&author, &owner);
    author.resume_apply().expect("the author enrolls");
    author.adopt(SKILL, &genesis_files());
    let digest = author.draft_digest(SKILL);
    author
        .publish_message("", &format!("{SKILL}@{digest}"), "genesis")
        .expect("the genesis lands");

    // The member: seated, enrolled, bytes landed, plus a probe credential for the poll witness.
    let member = stack.add_member(MEMBER_EMAIL, "member");
    let client = FollowHarness::new("unseat-member");
    stack.enroll_begin_and_approve(&client, &member);
    client.resume_apply().expect("the member enrolls");
    let (data, _) = client.reconcile(true);
    if data
        .skills
        .iter()
        .any(|s| s.skill == SKILL && s.action == topos_types::results::PullAction::Offered)
    {
        let _ = client.pull(topos::test_support::Scope::Accept {
            name: SKILL.to_owned(),
        });
    }
    assert_eq!(client.placement_files(SKILL), expected(&genesis_files()));
    let probe = stack.mint_device(&member, "member poll probe");
    let delivery_path = format!("/v1/workspaces/{}/delivery", stack.workspace_id);
    assert_eq!(
        stack.device_get(&probe.credential, &delivery_path).status,
        200
    );

    // The owner REMOVES the seat through the app's members ceremony (step-up gated). The detach
    // records + the seat delete land in ONE transaction — delivery ends in this request.
    let member_id = stack.user_id(MEMBER_EMAIL);
    // The members page is origin-rooted in single-tenant mode; its step-up rung is UNCHANGED.
    let removed = owner.post_form(
        "/members",
        &[
            ("intent", "remove"),
            ("user_id", &member_id),
            ("stepup_password", common::PASSWORD),
        ],
    );
    assert_eq!(
        removed.status, 200,
        "the removal ceremony lands: {}",
        removed.body
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.seat WHERE user_id = '{member_id}'"
        )),
        0,
        "the seat is gone"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.bundle_detachment \
             WHERE user_id = '{member_id}' AND cause = 'membership_removed'"
        )),
        1,
        "the detach record was written for the delivered bundle"
    );

    // The VERY NEXT delivery poll is the uniform 404 — same request-boundary the removal made.
    assert_eq!(
        stack.device_get(&probe.credential, &delivery_path).status,
        404,
        "delivery ended with the seat"
    );

    // The client fails CLOSED: the whole-workspace 404 freezes everything with a warning — the
    // placement is INTACT (never a clean), and the quiet hook stays exit-0 with its one-liner.
    let (_, warnings) = client.reconcile(false);
    assert!(!warnings.is_empty(), "the removed member's sweep warns");
    assert_eq!(
        client.placement_files(SKILL),
        expected(&genesis_files()),
        "a removal freezes — bytes stay"
    );
    let lines = client.quiet_update().expect("the hook posture is exit-0");
    assert!(!lines.is_empty(), "the one-liner a person must not miss");

    // Re-seating resumes delivery (the arrangement helper stands in for the re-invite rung).
    stack.seat(MEMBER_EMAIL, "member");
    assert_eq!(
        stack.device_get(&probe.credential, &delivery_path).status,
        200,
        "delivery resumed with the seat"
    );
    let (_, warnings) = client.reconcile(true);
    assert!(
        warnings.is_empty(),
        "a clean post-reseat sweep: {warnings:?}"
    );
}
