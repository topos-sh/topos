//! The distribute HERO — the whole product loop through the REAL composed stack, one test:
//! boot → claim the workspace (preset code) → the owner's authoring CLI enrolls through the
//! device flow → publishes a genesis → a SECOND device (same person) follows and lands the bytes
//! byte-exact → v2 propagates on the next sweep (and a repeat sweep is a no-op) → a revert
//! restores the v1 bytes as a NEW forward version the follower lands. SMTP stays unset the whole
//! way — nothing in the loop needs mail.
//!
//! This is the release-blocker probe: every hop rides the public surfaces (the claim ceremony,
//! `/verify`, the `/api/v1` device lane) exactly as production traffic would.

mod common;

use common::{OWNER_EMAIL, SKILL, Stack, expected, genesis_files};
use topos::test_support::{FollowHarness, PublishResult, Scope};

/// The v2 draft the author ships after the genesis.
fn v2_files() -> Vec<(&'static str, bool, &'static [u8])> {
    vec![
        (
            "SKILL.md",
            false,
            b"# deploy\nDeploy the service.\nNow with canary steps.\n" as &[u8],
        ),
        (
            "run.sh",
            true,
            b"#!/bin/sh\necho deploying --canary\n" as &[u8],
        ),
    ]
}

/// Publish the current draft of `skill` from `client`, expecting a DIRECT landing; returns
/// `(version_id_hex, new_generation)`.
fn publish_direct(client: &FollowHarness, skill: &str, message: &str) -> (String, u64) {
    let digest = client.draft_digest(skill);
    match client
        .publish_message("", &format!("{skill}@{digest}"), message)
        .expect("the publish lands")
    {
        PublishResult::Published(d) => (d.version_id, d.current_generation),
        other => panic!("expected a direct publish, got {other:?}"),
    }
}

/// Land a delivered skill on `client` regardless of the consent shape the sweep chose: run the
/// bare reconcile, and if the entry is a first-receive OFFER, accept it explicitly (I-TOFU).
fn land(stack: &Stack, client: &FollowHarness, name: &str) {
    let _ = stack; // the stack pins the composed topology alive for the sweep
    let (data, warnings) = client.reconcile(true);
    assert!(warnings.is_empty(), "a clean sweep: {warnings:?}");
    if data
        .skills
        .iter()
        .any(|s| s.skill == name && s.action == topos_types::results::PullAction::Offered)
    {
        let _ = client.pull(Scope::Accept {
            name: name.to_owned(),
        });
    }
}

#[test]
fn e2e_the_full_loop_publish_propagate_revert() {
    let stack = common::start_stack("hero");

    // Boot → claim: the first account, seated as the owner, signed in by the ceremony itself.
    let owner = stack.claim_owner(OWNER_EMAIL);

    // Device 1 — the authoring CLI: the gh-style device flow, approved at /verify.
    let author = FollowHarness::new("hero-author");
    stack.enroll_begin_and_approve(&author, &owner);
    let applied = author.resume_apply().expect("the author's resume applies");
    assert!(applied.enrolled_now);

    // The genesis: adopt the draft, publish it. `current` is born at generation 1.
    author.adopt(SKILL, &genesis_files());
    let (v1, gen1) = publish_direct(&author, SKILL, "genesis: deploy runbook");
    assert_eq!(gen1, 1, "genesis creates the pointer at generation 1");
    // The author's own copy is followed locally (the workspace scope every later verb resolves).
    author.follow_locally(SKILL, &stack.workspace_id);

    // Device 2 — a second machine of the SAME person: follow the address, approve, apply.
    let follower = FollowHarness::new("hero-follower");
    stack.enroll_begin_and_approve(&follower, &owner);
    let applied = follower
        .resume_apply()
        .expect("the follower's resume applies");
    assert!(applied.enrolled_now);
    land(&stack, &follower, SKILL);
    assert_eq!(
        follower.placement_files(SKILL),
        expected(&genesis_files()),
        "the genesis lands byte-exact on the second device"
    );

    // v2: the author edits + publishes; the follower's next sweep fast-forwards byte-exact.
    author.edit_placement(SKILL, &v2_files());
    let (_v2, gen2) = publish_direct(&author, SKILL, "v2: canary steps");
    assert_eq!(gen2, 2);
    let (data, warnings) = follower.reconcile(true);
    assert!(warnings.is_empty(), "a clean v2 sweep: {warnings:?}");
    let entry = data
        .skills
        .iter()
        .find(|s| s.skill == SKILL)
        .expect("the followed skill rides the sweep");
    assert_eq!(
        entry.action,
        topos_types::results::PullAction::FastForwarded,
        "v2 auto-applies on a standing follow"
    );
    assert_eq!(follower.placement_files(SKILL), expected(&v2_files()));

    // A REPEAT sweep is a commit-sensitive no-op.
    let (data, _) = follower.reconcile(true);
    let entry = data
        .skills
        .iter()
        .find(|s| s.skill == SKILL)
        .expect("still followed");
    assert_eq!(
        entry.action,
        topos_types::results::PullAction::UpToDate,
        "nothing moved — nothing re-applies"
    );

    // The fleet report: the follower's applied snapshot landed as a row.
    let follower_device = follower.device_id().expect("the follower is enrolled");
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.device_bundle_state WHERE device_id = '{follower_device}'"
        )),
        1,
        "the reconcile reported this device's applied state"
    );

    // REVERT: the owner rolls back to v1 — a FORWARD commit carrying the good bytes (generation
    // 3), never a pointer rollback. The follower's next sweep lands the restored bytes.
    let reverted = author.revert(SKILL, &v1, true).expect("the revert lands");
    assert_eq!(reverted.reverted_to, v1);
    assert_eq!(reverted.current_generation, 3, "a revert moves FORWARD");
    assert_ne!(reverted.new_version_id, v1, "the revert is a NEW version");
    let (data, warnings) = follower.reconcile(true);
    assert!(warnings.is_empty(), "a clean revert sweep: {warnings:?}");
    let entry = data
        .skills
        .iter()
        .find(|s| s.skill == SKILL)
        .expect("still followed");
    assert_eq!(
        entry.action,
        topos_types::results::PullAction::FastForwarded
    );
    assert_eq!(
        follower.placement_files(SKILL),
        expected(&genesis_files()),
        "the revert restored the v1 bytes byte-exact"
    );
}
