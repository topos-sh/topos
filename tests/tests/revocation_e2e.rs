//! E2E — the revocation story on the device-link model: a device's self-revoke (the account page's
//! sign-out — self-only, a device is a possession) ends its lane IMMEDIATELY (the very next request
//! under the dead credential is the uniform 404) and severs EVERY link in the same transaction;
//! revocation is FINAL (the DB trigger refuses any un-revoke — rotation is revoke + re-enroll); the
//! CLI's `auth logout` is ONE global `DELETE /v1/device`; and a seat removal severs the removed
//! person's links IN THE SAME REQUEST (the removal via the app's members ceremony; the link row is
//! gone, the next delivery poll 404s; the client's sweep fails CLOSED into a freeze — placements
//! intact, never a clean — and resumes only when re-seated AND relinked: the link is the gate).

mod common;

use common::{OWNER_EMAIL, SKILL, expected, genesis_files};
use topos::test_support::{FollowHarness, LinkApplyOutcome, PublishResult};

const MEMBER_EMAIL: &str = "dave@acme.test";

// ── self-revoke: immediate, final, link-severing; recoverable only by re-enrolling ──────────────────

#[test]
fn e2e_self_revoke_is_immediate_final_and_re_enrollment_recovers() {
    let stack = common::start_stack("revoke");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // A probe device of the owner: alive on the lane, then signed out from the ACCOUNT page (the
    // self-only ceremony — no owner arm reaches into someone else's pocket).
    let probe = stack.mint_device(&owner, "laptop probe");
    let me = stack.device_get(
        &probe.credential,
        &format!("/v1/workspaces/{}/me", stack.workspace_id),
    );
    assert_eq!(me.status, 200, "the live credential reads: {}", me.body);
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.device_link WHERE device_id = '{}'",
            probe.device_id
        )),
        1,
        "the /verify approval minted registration + the first link in one fence"
    );

    let revoked = owner.post_form(
        "/account/devices",
        &[("intent", "sign-out"), ("device_id", &probe.device_id)],
    );
    assert_eq!(
        revoked.status, 200,
        "the account-page sign-out lands: {}",
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

    // The revoke severed EVERY link in the same transaction, cause-tagged in the audit trail.
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.device_link WHERE device_id = '{}'",
            probe.device_id
        )),
        0,
        "the device's links are severed with the revoke"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.audit_event WHERE kind = 'device_unlinked' \
             AND subject = '{}' AND details->>'cause' = 'device_revoked'",
            probe.device_id
        )),
        1,
        "one cause-tagged device_unlinked audit row per severed link"
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

// ── the CLI's own logout: ONE global self-revoke + the credential doc deleted ───────────────────────

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

    // `auth logout --yes` is ONE global op: `DELETE /v1/device` — the server revokes the device
    // and severs its links + reported state in the same transaction; no per-workspace loop.
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
        "the logout's global self-revoke landed"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.device_link WHERE device_id = '{device_id}'"
        )),
        0,
        "the device's links died with the logout"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.audit_event WHERE kind = 'device_unlinked' \
             AND subject = '{device_id}' AND details->>'cause' = 'device_revoked'"
        )),
        1,
        "the severed link is audit-rowed, cause-tagged"
    );
    // Skills, drafts, and the adopted placement STAY — no credential is signed-out, not removed.
    assert_eq!(author.placement_files(SKILL), expected(&genesis_files()));
}

// ── seat removal severs the links in the same request; re-seat + relink resumes ─────────────────────

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

    // The owner REMOVES the seat through the app's members ceremony — an owner-guarded act that
    // wears a client-side in-place confirm in the UI; server-side it is the role guard + the
    // audited act. The detach records + the seat delete + the LINK CASCADE (every device link of
    // the removed person in this workspace) land in ONE transaction — delivery ends in this
    // request.
    let member_id = stack.user_id(MEMBER_EMAIL);
    // The members page is origin-rooted in single-tenant mode.
    let removed = owner.post_form("/members", &[("intent", "remove"), ("user_id", &member_id)]);
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
    // The link cascade: BOTH of the member's devices (the CLI + the probe) lost their link rows.
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.device_link dl \
             WHERE dl.device_id IN (SELECT id FROM web.device WHERE user_id = '{member_id}')"
        )),
        0,
        "the member's device links died with the seat, in the same transaction"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.audit_event WHERE kind = 'device_unlinked' \
             AND details->>'cause' = 'seat_removed' AND workspace_id = '{}'",
            stack.workspace_id
        )),
        2,
        "one cause-tagged device_unlinked row per severed device"
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

    // Re-seating alone does NOT resume delivery: the link rows are DELETED, never tombstoned, so
    // the device must RELINK (the arrangement helper stands in for the re-invite rung).
    stack.seat(MEMBER_EMAIL, "member");
    assert_eq!(
        stack.device_get(&probe.credential, &delivery_path).status,
        404,
        "a seat without a link still answers the uniform 404 — the link is the gate"
    );

    // The probe relinks over the raw link lane (the empty workspace is the single-tenant origin
    // form); the CLI relinks through `follow <address> --yes` — the link lane rides the ordinary
    // follow, then continues into the subscribe this invocation.
    let relinked = stack.device_post_json(
        Some(&probe.credential),
        "/v1/device/link",
        &serde_json::json!({ "workspace": "" }),
    );
    assert_eq!(relinked.status, 200, "the relink lands: {}", relinked.body);
    let envelope: serde_json::Value =
        serde_json::from_str(&relinked.body).expect("the link envelope parses");
    assert_eq!(
        envelope["data"]["link_status"], "active",
        "the knob is off, so the relink is born active: {envelope}"
    );
    assert_eq!(
        stack.device_get(&probe.credential, &delivery_path).status,
        200,
        "delivery resumed with the relink"
    );
    match client
        .link_apply(&stack.address())
        .expect("the CLI relink applies")
    {
        LinkApplyOutcome::Applied(_) => {}
        other => panic!("expected the active-link continuation, got {other:?}"),
    }
    let (_, warnings) = client.reconcile(true);
    assert!(
        warnings.is_empty(),
        "a clean post-relink sweep: {warnings:?}"
    );
    assert_eq!(client.placement_files(SKILL), expected(&genesis_files()));
}
