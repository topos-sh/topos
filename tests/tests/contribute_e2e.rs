//! E2E — the contribute-back loop over the real composed stack: a MEMBER's direct publish on a
//! protected bundle DOWNGRADES to a proposal (never an error), the reviewer's approve promotes it
//! (the pointer CAS the app orchestrates against the vault), the follower lands the approved bytes,
//! and the author's next sweep narrates + ACKS the verdict notice. The reject arm carries the
//! reviewer's reason verbatim into the author's notice.
//!
//! Roles are REAL seats: the owner claims the workspace; the member is a seated `member` account
//! whose devices enroll through the same `/verify` ceremony as everyone else's.

mod common;

use common::{OWNER_EMAIL, SKILL, expected, genesis_files};
use topos::test_support::{FollowHarness, PublishResult, RevertProbe};

const MEMBER_EMAIL: &str = "bob@acme.test";

/// The member's improvement draft.
fn v2_files() -> Vec<(&'static str, bool, &'static [u8])> {
    vec![
        (
            "SKILL.md",
            false,
            b"# deploy\nDeploy the service.\nRollback notes included.\n" as &[u8],
        ),
        ("run.sh", true, b"#!/bin/sh\necho deploying\n" as &[u8]),
    ]
}

/// A later draft the reject arm proposes.
fn v3_files() -> Vec<(&'static str, bool, &'static [u8])> {
    vec![
        (
            "SKILL.md",
            false,
            b"# deploy\nA rewrite the reviewer will refuse.\n" as &[u8],
        ),
        ("run.sh", true, b"#!/bin/sh\necho deploying\n" as &[u8]),
    ]
}

#[test]
fn e2e_member_publish_downgrades_reviewer_approves_follower_lands() {
    let stack = common::start_stack("contribute");
    let owner = stack.claim_owner(OWNER_EMAIL);
    let member = stack.add_member(MEMBER_EMAIL, "member");

    // The member's authoring CLI enrolls and publishes the GENESIS — a genesis always lands
    // directly (there is no base to review against), even for a member.
    let author = FollowHarness::new("contrib-author");
    stack.enroll_begin_and_approve(&author, &member);
    author.resume_apply().expect("the author enrolls");
    author.adopt(SKILL, &genesis_files());
    let digest = author.draft_digest(SKILL);
    let genesis = match author
        .publish_message("", &format!("{SKILL}@{digest}"), "genesis")
        .expect("the genesis lands")
    {
        PublishResult::Published(d) => d,
        other => panic!("a genesis lands directly, got {other:?}"),
    };
    author.follow_locally(SKILL, &stack.workspace_id);

    // The owner's CLI enrolls and receives the genesis (entitled via `everyone`).
    let reviewer = FollowHarness::new("contrib-reviewer");
    stack.enroll_begin_and_approve(&reviewer, &owner);
    let applied = reviewer.resume_apply().expect("the reviewer enrolls");
    assert_eq!(
        applied.installed.len(),
        1,
        "the genesis delivered: {:?}",
        applied.installed
    );
    assert_eq!(reviewer.placement_files(SKILL), expected(&genesis_files()));

    // The owner PROTECTS the bundle: a bare tighten to `reviewed` (catalog row witnessed).
    let protect = reviewer
        .protect(SKILL, None, true)
        .expect("the owner may tighten");
    assert_eq!(protect.level, "reviewed", "the tighten's applied level");
    assert_eq!(
        stack.text_witness(&format!(
            "SELECT protection FROM web.bundle WHERE name = '{SKILL}'"
        )),
        Some("reviewed".to_owned()),
        "the protection row landed"
    );

    // The member's DIRECT publish now DOWNGRADES to a proposal — surfaced as Proposed, never an
    // error; `current` does not move.
    author.edit_placement(SKILL, &v2_files());
    let digest = author.draft_digest(SKILL);
    let proposed = match author
        .publish_message("", &format!("{SKILL}@{digest}"), "add rollback notes")
        .expect("the downgraded publish still succeeds")
    {
        PublishResult::Proposed(d) => d,
        other => panic!("the protection gate downgrades, got {other:?}"),
    };
    let candidate = proposed
        .proposal
        .split('@')
        .nth(1)
        .expect("the proposal names its candidate")
        .to_owned();
    assert_eq!(
        stack.count("SELECT count(*) FROM web.proposal WHERE status = 'open'"),
        1,
        "one open proposal row"
    );

    // The reviewer's inbox leads with the author's message; the approve promotes the candidate.
    let inbox = reviewer.review_inbox().expect("the review inbox");
    assert_eq!(
        inbox.inbox.len(),
        1,
        "one reviewable proposal: {:?}",
        inbox.inbox
    );
    let v2_candidate = candidate.clone();
    let approved = reviewer
        .review_approve(&format!("{SKILL}@{candidate}"))
        .expect("the approve lands");
    assert_eq!(
        approved.current_generation,
        Some(2),
        "the approve CAS-moved current onto the candidate"
    );
    assert_eq!(
        stack.count("SELECT count(*) FROM web.proposal WHERE status = 'approved'"),
        1,
        "the proposal row resolved approved"
    );

    // The reviewer's own device lands the approved bytes on its next sweep.
    let (_, warnings) = reviewer.reconcile(true);
    assert!(
        warnings.is_empty(),
        "a clean post-approve sweep: {warnings:?}"
    );
    assert_eq!(reviewer.placement_files(SKILL), expected(&v2_files()));

    // The AUTHOR's next sweep narrates the verdict notice and ACKS it; a second sweep is silent.
    let (data, _) = author.reconcile(true);
    assert!(
        !data.notices.is_empty(),
        "the approve verdict rides the author's next update"
    );
    let member_id = stack.user_id(MEMBER_EMAIL);
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.notice WHERE user_id = '{member_id}' AND acked_at IS NOT NULL"
        )),
        1,
        "the interactive update acked exactly what it narrated"
    );
    let (data, _) = author.reconcile(true);
    assert!(
        data.notices.is_empty(),
        "nothing re-narrates: {:?}",
        data.notices
    );

    // The REJECT arm: a further proposal is refused with a reason that rides the notice verbatim.
    author.edit_placement(SKILL, &v3_files());
    let digest = author.draft_digest(SKILL);
    let proposed = match author
        .publish_message("", &format!("{SKILL}@{digest}"), "a rewrite")
        .expect("the downgraded publish succeeds")
    {
        PublishResult::Proposed(d) => d,
        other => panic!("still protected, got {other:?}"),
    };
    let candidate = proposed
        .proposal
        .split('@')
        .nth(1)
        .expect("candidate")
        .to_owned();
    reviewer
        .review_reject(
            &format!("{SKILL}@{candidate}"),
            "not yet — needs the canary half",
        )
        .expect("the reject lands");
    let (data, _) = author.reconcile(true);
    let narrated = serde_json::to_string(&data.notices).expect("serialize notices");
    assert!(
        narrated.contains("not yet"),
        "the reject reason rides the verdict notice verbatim: {narrated}"
    );

    // The genesis version stays reachable: a revert to it still works (the owner's undo path).
    let reverted = reviewer
        .revert(SKILL, &genesis.version_id, true)
        .expect("the revert to v1 lands");
    assert_eq!(reverted.reverted_to, genesis.version_id);
    let (_, warnings) = reviewer.reconcile(true);
    assert!(warnings.is_empty());
    assert_eq!(
        reviewer.placement_files(SKILL),
        expected(&genesis_files()),
        "the revert restored the genesis bytes"
    );

    // The SAME revert again is a byte-level NO-OP — detected by TREE, not commit id (the forward
    // revert minted a NEW id over the genesis bytes, so an id compare would keep minting
    // generation after generation): no forward commit, no pointer move, on BOTH phases.
    let versions_before = stack.count("SELECT count(*) FROM plane.version");
    let generation_before =
        stack.text_witness("SELECT max(generation)::text FROM plane.current_pointer");
    match reviewer
        .revert_probe(SKILL, &genesis.version_id, true)
        .expect("the --yes no-op acks")
    {
        RevertProbe::NoOp => {}
        other => panic!("a repeat revert is a byte-level no-op, got {other:?}"),
    }
    match reviewer
        .revert_probe(SKILL, &genesis.version_id, false)
        .expect("the bare no-op describes")
    {
        RevertProbe::NoOp => {}
        other => panic!("bare or --yes, identical bytes are a no-op, got {other:?}"),
    }
    // A BARE revert to a version whose bytes DO differ is the two-phase DESCRIBE — nothing lands
    // until `--yes`.
    match reviewer
        .revert_probe(SKILL, &v2_candidate, false)
        .expect("the bare revert describes")
    {
        RevertProbe::Described => {}
        other => panic!("a bare revert with differing bytes describes, got {other:?}"),
    }
    assert_eq!(
        stack.count("SELECT count(*) FROM plane.version"),
        versions_before,
        "neither the no-op nor the describe minted a forward commit"
    );
    assert_eq!(
        stack.text_witness("SELECT max(generation)::text FROM plane.current_pointer"),
        generation_before,
        "the pointer never moved"
    );
}

/// The author-self-approve wedge regression: four-eyes must hold as a TYPED refusal whose wire
/// carries a receipt — the op SETTLES (no pending WAL, no PENDING_OP wedge on later writes), a
/// retry re-settles, and a verdict on an already-DECIDED proposal is a terminal outcome, never a
/// retry loop. The author's own proposal also lists under the OUTBOX (`yours`), not the inbox.
#[test]
fn e2e_author_self_approve_refuses_typed_and_never_wedges() {
    let stack = common::start_stack("selfapprove");
    let owner = stack.claim_owner(OWNER_EMAIL);
    // The author holds a REVIEWER seat: four-eyes is exactly the case where someone who COULD
    // approve others' proposals proposes one themself (a plain member is refused on role first).
    let author_account = stack.add_member(MEMBER_EMAIL, "reviewer");

    // The author's CLI enrolls, ships the genesis, and follows it locally.
    let author = FollowHarness::new("selfapprove-author");
    stack.enroll_begin_and_approve(&author, &author_account);
    author.resume_apply().expect("the author enrolls");
    author.adopt(SKILL, &genesis_files());
    let digest = author.draft_digest(SKILL);
    author
        .publish_message("", &format!("{SKILL}@{digest}"), "genesis")
        .expect("the genesis lands");
    author.follow_locally(SKILL, &stack.workspace_id);

    // The owner's CLI enrolls (delivered via `everyone`) and tightens the bundle to `reviewed`.
    let reviewer = FollowHarness::new("selfapprove-reviewer");
    stack.enroll_begin_and_approve(&reviewer, &owner);
    reviewer.resume_apply().expect("the reviewer enrolls");
    reviewer
        .protect(SKILL, None, true)
        .expect("the owner tightens");

    // The author PROPOSES an improvement (a reviewer's direct publish would land; `--propose`
    // opens review voluntarily — and on a `reviewed` bundle four-eyes now governs the verdict).
    author.edit_placement(SKILL, &v2_files());
    let digest = author.draft_digest(SKILL);
    let proposed = match author
        .propose_message(&format!("{SKILL}@{digest}"), "improve")
        .expect("the proposal opens")
    {
        PublishResult::Proposed(d) => d,
        other => panic!("--propose opens a proposal, got {other:?}"),
    };
    let handle = proposed.proposal.clone();

    // The author's own proposal lists under the OUTBOX — the server-computed `yours`, immune to
    // any display-vs-principal skew.
    let author_view = author.review_inbox().expect("the author's review view");
    assert_eq!(
        author_view.outbox.len(),
        1,
        "the author's own proposal is yours: {author_view:?}"
    );
    assert!(
        author_view.inbox.is_empty(),
        "never offered back to its author for review: {author_view:?}"
    );

    // The self-approve is the TYPED four-eyes refusal — and it SETTLES: no pending op remains,
    // so no later write on this skill can wedge on PENDING_OP.
    let err = author
        .review_approve(&handle)
        .expect_err("four-eyes holds against the proposer");
    assert!(err.contains("DENIED"), "{err}");
    assert!(err.contains("four-eyes"), "{err}");
    assert!(
        author.pending_ops().is_empty(),
        "the refused op settled — nothing pending: {:?}",
        author.pending_ops()
    );

    // Re-running the same verdict is the SAME typed refusal again (a fresh, equally-settled op) —
    // never a CORRUPT_STATE / PENDING_OP loop.
    let err2 = author
        .review_approve(&handle)
        .expect_err("four-eyes still holds");
    assert!(err2.contains("DENIED"), "{err2}");
    assert!(author.pending_ops().is_empty());

    // A REAL second pair of eyes approves.
    reviewer
        .review_approve(&handle)
        .expect("the reviewer's approve lands");

    // The author retries the verdict on the now-DECIDED proposal: a terminal outcome, settled
    // clean and typed — never a retry.
    let err3 = author
        .review_approve(&handle)
        .expect_err("the proposal is already decided");
    assert!(
        err3.contains("not an open proposal"),
        "the decided proposal answers the honest refusal: {err3}"
    );
    assert!(
        author.pending_ops().is_empty(),
        "a decided proposal settles the op: {:?}",
        author.pending_ops()
    );
}
