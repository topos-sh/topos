//! E2E — the device-link model: a device is REGISTERED once per server (the one browser
//! device-flow ceremony) and LINKED per workspace (`web.device_link` — a first-class row,
//! severable by both sides, DELETED never tombstoned). The suite proves:
//!
//! - an ENROLLED install joining a SECOND same-plane workspace takes the browser-free link lane
//!   (`follow <address>` two-phase — nothing mutates bare; `--yes` creates the link and lands the
//!   workspace's bytes THIS invocation) and NEVER re-mints the device (exactly one `web.device`
//!   row across both joins — the re-mint regression);
//! - SELF unlink (the account page) freezes the device's next sweep (placements intact) and turns
//!   every lane request for that workspace into the uniform 404, byte-identical to a never-linked
//!   workspace;
//! - OWNER remove (the fleet page's in-place-confirm arm) does the same, `device_unlinked`
//!   audit-rowed with its cause tag — and the seat still standing, the device may relink;
//! - the `device_approval` knob (the REAL settings ceremony): a member's new link is born PENDING
//!   — delivery answers `link_status: "pending"` with EMPTY sets, the sweep skips QUIETLY, no
//!   bytes land, and every OTHER lane op is the uniform 404 — until an owner APPROVES on the
//!   fleet page; an OWNER's own new link is born ACTIVE regardless (delivery flows immediately);
//! - the REJECT arm deletes the pending row; the CLI's next contact prints ONE typed `LINK_ENDED`
//!   line toward relink (the second sweep is silent), and a subsequent `follow --yes` relink
//!   succeeds (a fresh row, born per the knob again).

mod common;

use common::{OWNER_EMAIL, SKILL, expected, genesis_files};
use topos::test_support::{FollowHarness, LinkApplyOutcome, PublishResult};

const MEMBER_EMAIL: &str = "erin@acme.test";
/// The second workspace's ADDRESS name + the skill seeded there.
const B_NAME: &str = "othr";
const B_SKILL: &str = "b-runbook";

/// The second workspace's genesis bundle — distinct bytes from the shared [`genesis_files`], so a
/// landed placement proves WHICH workspace delivered.
fn b_files() -> Vec<(&'static str, bool, &'static [u8])> {
    vec![
        (
            "SKILL.md",
            false,
            b"# b-runbook\nRun the B workspace's book.\n" as &[u8],
        ),
        ("go.sh", true, b"#!/bin/sh\necho b-runbook\n" as &[u8]),
    ]
}

/// Publish a genesis for `skill`/`files` from `rig` into `workspace` (empty = the enrolled default).
fn publish_genesis(
    rig: &FollowHarness,
    skill: &str,
    files: &[(&str, bool, &[u8])],
    workspace: &str,
) {
    rig.adopt(skill, files);
    let digest = rig.draft_digest(skill);
    let approve = format!("{skill}@{digest}");
    let result = if workspace.is_empty() {
        rig.publish_message("", &approve, "genesis")
    } else {
        rig.publish_in_workspace("", &approve, workspace)
    }
    .expect("the genesis lands");
    match result {
        PublishResult::Published(_) => {}
        other => panic!("expected a direct genesis, got {other:?}"),
    }
}

// ── the second-workspace browser-free link + self unlink ────────────────────────────────────────────

#[test]
fn e2e_second_workspace_link_is_browser_free_and_self_unlink_freezes() {
    let stack = common::start_stack("dlink");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // Workspace B: the named mail-less arrangement (direct row + `everyone` + the owner's seat).
    let b_id = stack.add_workspace(B_NAME, "Other Team");
    stack.seat_in(&b_id, OWNER_EMAIL, "owner");
    let b_address = format!("{}/{B_NAME}", stack.origin);

    // A seeding CLI: ONE device flow into A, then the browser-free link into B, then B's genesis —
    // so the subject device below has bytes to land.
    let seeder = FollowHarness::new("dlink-seeder");
    stack.enroll_begin_and_approve(&seeder, &owner);
    seeder.resume_apply().expect("the seeder enrolls");
    match seeder.link_apply(&b_address).expect("the seeder links B") {
        LinkApplyOutcome::Applied(_) => {}
        other => panic!("an owner's link is born active, got {other:?}"),
    }
    publish_genesis(&seeder, B_SKILL, &b_files(), &b_id);

    // The SUBJECT device: enrolled into A through the one real device flow.
    let device = FollowHarness::new("dlink-device");
    stack.enroll_begin_and_approve(&device, &owner);
    device.resume_apply().expect("the subject enrolls");
    let device_id = device.device_id().expect("enrolled");
    let devices_before = stack.count("SELECT count(*) FROM web.device");

    // BARE = the link DESCRIBE: the standing facts, and NOTHING mutates.
    let describe = device
        .link_describe(&b_address)
        .expect("the link describe answers");
    assert_eq!(describe["workspace"]["name"], B_NAME);
    assert_eq!(describe["role"], "owner");
    assert_eq!(describe["link_status"], "none", "no link exists yet");
    assert_eq!(
        describe["born"], "active",
        "the knob is off; the seat is an owner's"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.device_link \
             WHERE device_id = '{device_id}' AND workspace_id = '{b_id}'"
        )),
        0,
        "the bare describe laid no row"
    );
    assert_eq!(
        device.memberships().len(),
        1,
        "the bare describe recorded no membership"
    );

    // `--yes` creates the link AND lands B's bytes THIS invocation (the enroll fold-in shape).
    match device.link_apply(&b_address).expect("the link applies") {
        LinkApplyOutcome::Applied(_) => {}
        other => panic!("expected the active-link continuation, got {other:?}"),
    }
    assert_eq!(
        device.placement_files(B_SKILL),
        expected(&b_files()),
        "B's genesis landed byte-exact on the `--yes`"
    );
    // The re-mint regression: joining the second workspace minted NO second device row — the one
    // registration now carries TWO links.
    assert_eq!(
        stack.count("SELECT count(*) FROM web.device"),
        devices_before,
        "exactly one web.device row for the machine across both joins"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.device_link WHERE device_id = '{device_id}'"
        )),
        2,
        "one registration, two links (A and B)"
    );
    assert_eq!(device.memberships().len(), 2, "both memberships recorded");

    // SELF unlink from the account page (per-link, self-only) — the row dies, cause-tagged.
    let unlinked = owner.post_form(
        "/account/devices",
        &[
            ("intent", "unlink"),
            ("device_id", &device_id),
            ("workspace_id", &b_id),
        ],
    );
    assert_eq!(unlinked.status, 200, "the unlink lands: {}", unlinked.body);
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.device_link \
             WHERE device_id = '{device_id}' AND workspace_id = '{b_id}'"
        )),
        0,
        "the link row is DELETED, never tombstoned"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.audit_event WHERE kind = 'device_unlinked' \
             AND subject = '{device_id}' AND workspace_id = '{b_id}' \
             AND details->>'cause' = 'self'"
        )),
        1,
        "the self unlink is audit-rowed with its cause"
    );

    // The next sweep FREEZES B: one warning, every placement byte intact, never a clean.
    let (_, warnings) = device.reconcile(false);
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("ACCESS_GONE") && w.contains(&b_id)),
        "the unlinked workspace freezes with the one warning: {warnings:?}"
    );
    assert_eq!(
        device.placement_files(B_SKILL),
        expected(&b_files()),
        "an unlink freezes — bytes stay"
    );

    // Every B lane request under the device's own credential is the uniform 404, byte-identical
    // to a workspace that never existed; the A lane stays live.
    let cred = device
        .credential()
        .expect("the credential doc holds the secret");
    let ghost = "w_ghost0000000000000000000000000000";
    let reference = stack.device_get(&cred, &format!("/v1/workspaces/{ghost}/me"));
    assert_eq!(reference.status, 404);
    for path in [
        format!("/v1/workspaces/{b_id}/me"),
        format!("/v1/workspaces/{b_id}/delivery"),
        format!("/v1/workspaces/{b_id}/skills"),
        format!("/v1/workspaces/{b_id}/channels"),
    ] {
        let answer = stack.device_get(&cred, &path);
        assert_eq!(
            answer.status, 404,
            "GET {path} after the unlink: {}",
            answer.body
        );
        assert_eq!(
            answer.body, reference.body,
            "GET {path} is byte-identical to the never-linked miss"
        );
    }
    let a_me = stack.device_get(&cred, &format!("/v1/workspaces/{}/me", stack.workspace_id));
    assert_eq!(a_me.status, 200, "the A link is untouched: {}", a_me.body);
}

// ── owner remove on the fleet page: freeze + audit; the seat stands, so relink is open ──────────────

#[test]
fn e2e_owner_remove_on_the_fleet_page_freezes_and_audits() {
    let stack = common::start_stack("dlrm");
    let owner = stack.claim_owner(OWNER_EMAIL);

    let author = FollowHarness::new("dlrm-author");
    stack.enroll_begin_and_approve(&author, &owner);
    author.resume_apply().expect("the author enrolls");
    publish_genesis(&author, SKILL, &genesis_files(), "");

    // The member's device, enrolled + bytes landed.
    let member = stack.add_member(MEMBER_EMAIL, "member");
    let client = FollowHarness::new("dlrm-member");
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
    let device_id = client.device_id().expect("enrolled");

    // The OWNER removes the link on the fleet page — the in-place-confirm arm's armed submit is
    // the same form fields a browser posts (arming is client-side; the server gate is the owner
    // guard + the audited act).
    let removed = owner.post_form(
        "/settings/devices",
        &[("intent", "remove-link"), ("device_id", &device_id)],
    );
    assert_eq!(removed.status, 200, "the remove lands: {}", removed.body);
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.device_link WHERE device_id = '{device_id}'"
        )),
        0,
        "the link row is gone"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.audit_event WHERE kind = 'device_unlinked' \
             AND subject = '{device_id}' AND details->>'cause' = 'owner_removed'"
        )),
        1,
        "the owner remove is audit-rowed with its cause tag"
    );

    // The device freezes: the whole-workspace 404, placements intact, the one warning.
    let (_, warnings) = client.reconcile(false);
    assert!(
        warnings.iter().any(|w| w.contains("ACCESS_GONE")),
        "the removed device's sweep warns: {warnings:?}"
    );
    assert_eq!(
        client.placement_files(SKILL),
        expected(&genesis_files()),
        "removing ends delivery, never recalls bytes"
    );

    // The SEAT still stands, so the device may RELINK (the real ban is the seat's, or the knob's):
    // `follow <address> --yes` creates a fresh link and the sweep is clean again.
    match client
        .link_apply(&stack.address())
        .expect("the relink applies")
    {
        LinkApplyOutcome::Applied(_) => {}
        other => panic!("the knob is off — the relink is born active, got {other:?}"),
    }
    let (_, warnings) = client.reconcile(true);
    assert!(
        warnings.is_empty(),
        "a clean post-relink sweep: {warnings:?}"
    );
    assert_eq!(client.placement_files(SKILL), expected(&genesis_files()));
}

// ── the device-approval knob: born pending → empty typed delivery → owner approves → delivers ───────

#[test]
fn e2e_device_approval_knob_holds_delivery_until_the_owner_approves() {
    let stack = common::start_stack("dlknob");
    let owner = stack.claim_owner(OWNER_EMAIL);

    let author = FollowHarness::new("dlknob-author");
    stack.enroll_begin_and_approve(&author, &owner);
    author.resume_apply().expect("the author enrolls");
    publish_genesis(&author, SKILL, &genesis_files(), "");

    // Flip the knob ON through the REAL settings ceremony (a plain owner form save).
    let flipped = owner.post_form(
        "/settings",
        &[("intent", "set-device-approval"), ("device_approval", "on")],
    );
    assert_eq!(flipped.status, 200, "the knob save lands: {}", flipped.body);
    assert_eq!(
        stack.text_witness(&format!(
            "SELECT device_approval FROM web.workspace WHERE id = '{}'",
            stack.workspace_id
        )),
        Some("on".to_owned()),
        "the knob row flipped"
    );

    // A MEMBER's new device: the enrollment's first link is born PENDING — the typed receipt,
    // nothing subscribed, no bytes.
    let member = stack.add_member(MEMBER_EMAIL, "member");
    let client = FollowHarness::new("dlknob-member");
    stack.enroll_begin_and_approve(&client, &member);
    let receipt = client
        .resume_link_pending()
        .expect("the pending-link receipt");
    assert_eq!(receipt.link_status, "pending");
    assert!(receipt.enrolled_now, "this invocation enrolled the device");
    assert_eq!(receipt.workspace_id, stack.workspace_id);
    let device_id = client.device_id().expect("registered");
    assert_eq!(
        stack.text_witness(&format!(
            "SELECT status FROM web.device_link WHERE device_id = '{device_id}'"
        )),
        Some("pending".to_owned()),
        "the link row is born pending"
    );

    // Delivery is one of the exactly TWO pending-tolerant routes: the shape-complete EMPTY body.
    let cred = client
        .credential()
        .expect("the credential doc holds the secret");
    let delivery_path = format!("/v1/workspaces/{}/delivery", stack.workspace_id);
    let held = stack.device_get(&cred, &delivery_path);
    assert_eq!(held.status, 200, "delivery answers typed: {}", held.body);
    let held: serde_json::Value = serde_json::from_str(&held.body).expect("delivery JSON");
    assert_eq!(held["link_status"], "pending");
    assert_eq!(held["skills"], serde_json::json!([]), "empty sets: {held}");
    assert_eq!(held["detached"], serde_json::json!([]));
    assert_eq!(held["notices"], serde_json::json!([]));

    // `/me` is the other: the normal body, `link_status` marked. Every OTHER lane op folds to the
    // uniform 404, byte-identical to a never-existed workspace (no oracle from a pending seat).
    let me = stack.device_get(&cred, &format!("/v1/workspaces/{}/me", stack.workspace_id));
    assert_eq!(
        me.status, 200,
        "me answers typed for a pending link: {}",
        me.body
    );
    assert!(
        me.body.contains("\"link_status\":\"pending\""),
        "me carries the pending fact: {}",
        me.body
    );
    let ghost = "w_ghost0000000000000000000000000000";
    let reference = stack.device_get(&cred, &format!("/v1/workspaces/{ghost}/me"));
    assert_eq!(reference.status, 404);
    let skills_read = stack.device_get(
        &cred,
        &format!("/v1/workspaces/{}/skills", stack.workspace_id),
    );
    assert_eq!(
        skills_read.status, 404,
        "a non-delivery read is the uniform 404"
    );
    assert_eq!(
        skills_read.body, reference.body,
        "byte-identical to the ghost miss"
    );
    let follow_put = stack.device_put(
        &cred,
        &format!("/v1/workspaces/{}/follows/{SKILL}", stack.workspace_id),
    );
    assert_eq!(follow_put.status, 404, "a row-op write is the uniform 404");
    assert_eq!(
        follow_put.body, reference.body,
        "byte-identical to the ghost miss"
    );

    // The sweep skips a pending workspace QUIETLY: no warning, no subscription, no bytes.
    let (data, warnings) = client.reconcile(false);
    assert!(
        warnings.is_empty(),
        "the pending skip is quiet: {warnings:?}"
    );
    assert!(
        data.skills.is_empty(),
        "nothing delivered: {:?}",
        data.skills
    );
    assert_eq!(
        client.follows_count(),
        0,
        "nothing subscribed while pending"
    );

    // An OWNER's own new link is born ACTIVE regardless of the knob — delivery flows immediately.
    let owner_probe = stack.mint_device(&owner, "owner probe");
    let flowing = stack.device_get(&owner_probe.credential, &delivery_path);
    assert_eq!(flowing.status, 200);
    let flowing: serde_json::Value = serde_json::from_str(&flowing.body).expect("delivery JSON");
    assert_eq!(
        flowing["link_status"], "active",
        "an owner's link skips the queue"
    );
    assert!(
        flowing["skills"].as_array().is_some_and(|s| !s.is_empty()),
        "the owner's delivery carries the catalog: {flowing}"
    );

    // The owner APPROVES on the fleet page → the next sweep delivers (the arrival is offered
    // I-TOFU, the accept lands byte-exact — the same consent every new device walks).
    let approved = owner.post_form(
        "/settings/devices",
        &[("intent", "approve-link"), ("device_id", &device_id)],
    );
    assert_eq!(approved.status, 200, "the approve lands: {}", approved.body);
    assert_eq!(
        stack.text_witness(&format!(
            "SELECT status FROM web.device_link WHERE device_id = '{device_id}'"
        )),
        Some("active".to_owned()),
        "the approve flips the row active"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.audit_event WHERE kind = 'link_approved' \
             AND subject = '{device_id}'"
        )),
        1,
        "the approve is audit-rowed"
    );
    let (data, warnings) = client.reconcile(true);
    assert!(
        warnings.is_empty(),
        "a clean delivering sweep: {warnings:?}"
    );
    if data
        .skills
        .iter()
        .any(|s| s.skill == SKILL && s.action == topos_types::results::PullAction::Offered)
    {
        let _ = client.pull(topos::test_support::Scope::Accept {
            name: SKILL.to_owned(),
        });
    }
    assert_eq!(
        client.placement_files(SKILL),
        expected(&genesis_files()),
        "delivery opened with the approval"
    );
    assert_eq!(
        client.membership_link_statuses(),
        vec![(stack.workspace_id.clone(), "active".to_owned())],
        "the local record self-healed to active"
    );
}

// ── the reject arm: row deleted, ONE typed line toward relink, and the relink succeeds ──────────────

#[test]
fn e2e_reject_ends_the_link_typed_once_and_relink_succeeds() {
    let stack = common::start_stack("dlrej");
    let owner = stack.claim_owner(OWNER_EMAIL);

    let flipped = owner.post_form(
        "/settings",
        &[("intent", "set-device-approval"), ("device_approval", "on")],
    );
    assert_eq!(flipped.status, 200, "the knob save lands: {}", flipped.body);

    let member = stack.add_member(MEMBER_EMAIL, "member");
    let client = FollowHarness::new("dlrej-member");
    stack.enroll_begin_and_approve(&client, &member);
    let receipt = client
        .resume_link_pending()
        .expect("the pending-link receipt");
    assert_eq!(receipt.link_status, "pending");
    let device_id = client.device_id().expect("registered");

    // The owner REJECTS on the fleet page: the row is DELETED (not tombstoned), audit-rowed.
    let rejected = owner.post_form(
        "/settings/devices",
        &[("intent", "reject-link"), ("device_id", &device_id)],
    );
    assert_eq!(rejected.status, 200, "the reject lands: {}", rejected.body);
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.device_link WHERE device_id = '{device_id}'"
        )),
        0,
        "the pending row is deleted"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.audit_event WHERE kind = 'link_rejected' \
             AND subject = '{device_id}'"
        )),
        1,
        "the reject is audit-rowed"
    );

    // The CLI's next contact: ONE typed line toward relink — and only once (the second sweep is
    // silent; the ended membership leaves the fan-out).
    let (_, warnings) = client.reconcile(false);
    assert_eq!(
        warnings.len(),
        1,
        "exactly the one typed line: {warnings:?}"
    );
    assert!(
        warnings[0].contains("LINK_ENDED") && warnings[0].contains("follow"),
        "the line names the fact and the relink verb: {}",
        warnings[0]
    );
    let (_, warnings) = client.reconcile(false);
    assert!(
        warnings.is_empty(),
        "the ended line prints once: {warnings:?}"
    );
    assert_eq!(
        client.membership_link_statuses(),
        vec![(stack.workspace_id.clone(), "ended".to_owned())],
        "the local record is marked ended"
    );

    // A subsequent relink succeeds: a FRESH row (born per the knob again — pending), the typed
    // receipt, and the membership record back to waiting.
    match client
        .link_apply(&stack.address())
        .expect("the relink applies")
    {
        LinkApplyOutcome::Pending(receipt) => {
            assert_eq!(receipt.link_status, "pending");
            assert_eq!(receipt.workspace_id, stack.workspace_id);
            assert!(!receipt.enrolled_now, "the device was already registered");
        }
        other => panic!("the knob is on — the relink is born pending, got {other:?}"),
    }
    assert_eq!(
        stack.text_witness(&format!(
            "SELECT status FROM web.device_link WHERE device_id = '{device_id}'"
        )),
        Some("pending".to_owned()),
        "a fresh pending row exists — rejection never bans the device"
    );
    assert_eq!(
        client.membership_link_statuses(),
        vec![(stack.workspace_id.clone(), "pending".to_owned())],
        "the local record waits again"
    );
}
