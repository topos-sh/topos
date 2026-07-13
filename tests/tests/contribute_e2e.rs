//! CONTRIBUTE e2e — the client write verbs over loopback HTTP against the real plane.
//!
//! One real `plane-store` [`Authority`] (seeded through the feature-gated fixtures) served by the composed
//! [`topos_plane::router`] on a real loopback socket, via the shared `common` harness. A PUBLISHER drives
//! the GENUINE write verbs (`publish`/`review`/`revert`/`diff` via `topos::test_support::ContributeHarness`)
//! over the GENUINE `ureq` transport — the acting device rides the request's workspace **Bearer credential**
//! (never a body field), the op kind rides the route, nothing is signed; a separate FOLLOWER drives the
//! GENUINE pull engine ([`topos::test_support::PullHarness`]) and must receive the shipped bytes byte-exact.
//! The publisher enrolls with the workspace credential the genesis seed registered on a confirmed-member
//! device, so the plane authenticates its writes by resolving that credential to its registry row. These
//! cover the review_required-OFF loop; the review_required gate + the proposals-list route are exercised in
//! their own tests.

mod common;

use common::{Plane, SKILL, WS, expected};
use plane_store::{Authority, FileMode, UploadedFile};
use topos::test_support::{ContributeHarness, Follow, PublishResult, PullHarness, Scope};
use topos_types::Generation;
use topos_types::results::{DiffSource, PullAction};

const GENESIS_DKID: &str = "dk_genesis";
const PRINCIPAL: &str = "p_dev";
/// The publisher's workspace Bearer credential (the genesis seed registers it on a confirmed-member
/// device; the publisher enrolls with it, the follower reads with it).
const CRED: &str = "wc_contribute_secret";
const AUTHOR: &str = "d_genesis";
const MESSAGE: &str = "topos: add";
const CREATED_AT: &str = "2026-06-30T00:00:00Z";
/// The genesis device's registered 32-byte public key (a fixed test value; nothing verifies against it).
const GENESIS_PUBKEY: [u8; 32] = [9u8; 32];
const GENESIS_OP: &str = "b0000000-0000-4000-8000-000000000001";

/// The placeholder a client adopts before its first pull (NOT the plane's genesis, so the first pull
/// genuinely fast-forwards onto the plane's bytes).
const PLACEHOLDER: &[(&str, bool, &[u8])] = &[("SKILL.md", false, b"# local placeholder\n")];

fn genesis_files() -> Vec<UploadedFile> {
    vec![
        UploadedFile {
            path: "SKILL.md".to_owned(),
            mode: FileMode::Regular,
            bytes: b"# deploy\nv1\n".to_vec(),
        },
        UploadedFile {
            path: "run.sh".to_owned(),
            mode: FileMode::Executable,
            bytes: b"#!/bin/sh\necho v1\n".to_vec(),
        },
    ]
}

/// The publisher's new draft (a forward child of genesis) — shipped via `publish` / `--propose`.
const DRAFT: &[(&str, bool, &[u8])] = &[
    ("SKILL.md", false, b"# deploy\nv2 faster\n"),
    ("run.sh", true, b"#!/bin/sh\necho v2\n"),
];

// ── the loopback plane (the shared harness + this suite's scenario seeding) ─────────────────────────

/// Seed a real authority (genesis device+credential+confirmed-member → genesis) + serve `router(state)` on
/// a loopback socket via the shared harness. The publisher enrolls with the SAME genesis credential
/// afterward (via [`drafting_publisher`]) — the credential IS the authenticator, so no separate device
/// registration is needed.
fn start_plane(tag: &str) -> Plane {
    common::start_stack(
        "topos-contrib",
        tag,
        false,
        async |authority: &Authority| {
            let genesis = common::seed_genesis_plane(
                authority,
                common::GenesisSpec {
                    dkid: GENESIS_DKID,
                    device_pubkey: &GENESIS_PUBKEY,
                    op_id: GENESIS_OP,
                    files: genesis_files(),
                    principal: PRINCIPAL,
                    author: AUTHOR,
                    message: MESSAGE,
                    created_at: CREATED_AT,
                    credential: CRED,
                },
            )
            .await;
            common::Seeded {
                genesis: Some(genesis),
                ..Default::default()
            }
        },
    )
}

/// Turn the workspace `review_required` gate on/off (the anti-poisoning policy).
fn set_review_required(plane: &Plane, on: bool) {
    let ws = plane.ws();
    plane.rt.block_on(async {
        plane
            .authority
            .seed_review_required(&ws, on)
            .await
            .expect("set review_required");
    });
}

const P_REVIEWER: &str = "p_reviewer";
const REVIEWER_DKID: &str = "dk_reviewer";
/// The reviewer device's registered 32-byte public key (a fixed test value; the credential authenticates).
const REVIEWER_PUBKEY: [u8; 32] = [11u8; 32];
/// The reviewer's workspace Bearer credential — a DISTINCT confirmed member (for four-eyes).
const REVIEWER_CRED: &str = "wc_reviewer_secret";

/// An enrolled reviewer under a DISTINCT principal (four-eyes), seated as a confirmed member holding
/// [`REVIEWER_CRED`] with the skill adopted — able to `review` a proposal.
fn enrolled_reviewer(plane: &Plane, tag: &str) -> ContributeHarness {
    let mut h = ContributeHarness::new(tag);
    plane.rt.block_on(common::seed_member(
        &plane.authority,
        &plane.ws(),
        REVIEWER_DKID,
        &REVIEWER_PUBKEY,
        P_REVIEWER,
        "member",
        REVIEWER_CRED,
    ));
    h.enroll(&plane.base_url, WS, SKILL, REVIEWER_CRED, true, PLACEHOLDER);
    h
}

/// An enrolled publisher authenticating with the genesis workspace credential (the genesis seed registered
/// it on a confirmed-member device), sitting at `current` (pulled), with `DRAFT` staged as a local edit.
fn drafting_publisher(plane: &Plane, tag: &str) -> ContributeHarness {
    let mut pub_h = ContributeHarness::new(tag);
    pub_h.enroll(&plane.base_url, WS, SKILL, CRED, false, PLACEHOLDER);
    // Reach the plane's current (1,1), then stage the draft.
    let pulled = pub_h.pull();
    assert_eq!(
        pulled.skills[0].action,
        PullAction::FastForwarded,
        "publisher reaches current"
    );
    pub_h.edit_placement(DRAFT);
    pub_h
}

fn approve_token(skill: &str, digest: &str) -> String {
    format!("{skill}@{digest}")
}

/// A fresh follower that has adopted + follows the skill (a placeholder); a pull lands `current`. It reads
/// with the workspace credential (a confirmed member reads every skill).
fn follower(tag: &str) -> PullHarness {
    let mut f = PullHarness::new(tag);
    f.adopt_followed(SKILL, WS, CRED, Follow::Auto, PLACEHOLDER);
    f
}

// ── scenario 1: publish-direct → the follower auto-applies byte-exact ──────────────────────────────────

#[test]
fn publish_direct_lands_on_a_follower_byte_exact() {
    let plane = start_plane("pubdirect");
    let pub_h = drafting_publisher(&plane, "pubdirect");

    let digest = pub_h.draft_digest();
    let outcome = pub_h
        .publish(false, &approve_token(SKILL, &digest))
        .expect("publish succeeds");
    let data = match outcome {
        PublishResult::Published(d) => d,
        other => panic!("expected a direct publish, got {other:?}"),
    };
    assert_eq!(
        data.current_generation,
        Some(Generation { epoch: 1, seq: 2 }),
        "current moved +1"
    );
    assert_eq!(
        data.bundle_digest, digest,
        "the published digest is the disclosed one"
    );

    // A separate follower pulls and auto-applies the EXACT shipped bytes (incl. the exec bit).
    let follower = follower("pubdirect-f");
    let pulled = follower.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(pulled.skills[0].action, PullAction::FastForwarded);
    assert_eq!(pulled.skills[0].applied, Generation { epoch: 1, seq: 2 });
    assert_eq!(
        follower.placement_files(SKILL),
        expected(DRAFT),
        "the follower holds the published draft byte-exact"
    );
}

// ── scenario 2: publish --propose → self-approve (review_required OFF) → follower applies ───────────────

#[test]
fn propose_then_approve_lands_on_a_follower() {
    let plane = start_plane("propose");
    let pub_h = drafting_publisher(&plane, "propose");

    let digest = pub_h.draft_digest();
    let proposed = pub_h
        .publish(true, &approve_token(SKILL, &digest))
        .expect("propose succeeds");
    let proposal = match proposed {
        PublishResult::Proposed(d) => d.proposal,
        other => panic!("expected a proposal, got {other:?}"),
    };

    // The proposer self-approves (allowed with review_required OFF) — current moves to the candidate.
    let review = pub_h.review(&proposal, true).expect("approve succeeds");
    assert_eq!(
        review.current_generation,
        Some(Generation { epoch: 1, seq: 2 }),
        "approving the proposal moved current"
    );

    let follower = follower("propose-f");
    let pulled = follower.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(pulled.skills[0].applied, Generation { epoch: 1, seq: 2 });
    assert_eq!(
        follower.placement_files(SKILL),
        expected(DRAFT),
        "the follower applies the approved candidate (delegated consent, no prompt)"
    );
}

// ── scenario 3: revert → the follower rolls FORWARD to the good (genesis) bytes ─────────────────────────

#[test]
fn revert_rolls_a_follower_forward_to_the_good_bytes() {
    let plane = start_plane("revert");
    let pub_h = drafting_publisher(&plane, "revert");

    // Publish v2 (current → (1,2)).
    let digest = pub_h.draft_digest();
    pub_h
        .publish(false, &approve_token(SKILL, &digest))
        .expect("publish v2");

    // Revert to the GOOD genesis version — a forward move (current → (1,3)) restoring the v1 bytes.
    let good = hex::encode(plane.genesis().0);
    let reverted = pub_h
        .revert(&good, &approve_token(SKILL, &good), false)
        .expect("revert succeeds");
    assert_eq!(reverted.reverted_to, good);
    assert_eq!(
        reverted.current_generation,
        Generation { epoch: 1, seq: 3 },
        "forward, +1"
    );

    // A follower pulls and lands the restored genesis bytes (NOT the v2 it never saw).
    let follower = follower("revert-f");
    let pulled = follower.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(pulled.skills[0].applied, Generation { epoch: 1, seq: 3 });
    assert_eq!(
        follower.placement_files(SKILL),
        expected(&[
            ("SKILL.md", false, b"# deploy\nv1\n"),
            ("run.sh", true, b"#!/bin/sh\necho v1\n"),
        ]),
        "the follower rolls forward to the restored good (genesis) bytes"
    );
}

// ── scenario 4: a plane diff renders a proposal (`current..<hash>`) ─────────────────────────────────────

#[test]
fn diff_renders_a_proposal() {
    let plane = start_plane("diff");
    let pub_h = drafting_publisher(&plane, "diff");

    let digest = pub_h.draft_digest();
    let proposal = match pub_h
        .publish(true, &approve_token(SKILL, &digest))
        .expect("propose")
    {
        PublishResult::Proposed(d) => d.proposal,
        other => panic!("expected a proposal, got {other:?}"),
    };
    // `<skill>@<hash>` → just the hash for the diff ref.
    let hash = proposal
        .split_once('@')
        .expect("proposal is skill@hash")
        .1
        .to_owned();

    let diff = pub_h
        .diff(Some(&format!("current..{hash}")))
        .expect("plane diff renders");
    assert_eq!(
        diff.source,
        DiffSource::Plane,
        "a proposal review is a plane diff"
    );
    assert_eq!(
        diff.version_id, hash,
        "the diff targets the proposal version"
    );
    assert!(
        diff.diff.contains("v2"),
        "the diff shows the proposed change: {}",
        diff.diff
    );
}

// ── scenario 5: a mismatched --approve digest is refused BEFORE any send ────────────────────────────────

#[test]
fn a_mismatched_approve_digest_is_refused() {
    let plane = start_plane("mismatch");
    let pub_h = drafting_publisher(&plane, "mismatch");

    // Approve a digest that does NOT match the staged draft → refused locally (never signed/sent).
    let wrong = "0".repeat(64);
    let err = pub_h
        .publish(false, &approve_token(SKILL, &wrong))
        .expect_err("a digest mismatch must be refused");
    assert!(
        err.contains("--approve") || err.contains("digest"),
        "got: {err}"
    );

    // current never moved — still the genesis (1,1).
    let follower = follower("mismatch-f");
    let pulled = follower.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(
        pulled.skills[0].applied,
        Generation { epoch: 1, seq: 1 },
        "a refused publish never moved current"
    );
}

// ── scenario 6: under review_required, a plain member's DIRECT publish DOWNGRADES to a proposal ─────────

#[test]
fn a_member_direct_publish_on_a_reviewed_bundle_downgrades_to_a_proposal() {
    let plane = start_plane("downgrade");
    set_review_required(&plane, true);
    let pub_h = drafting_publisher(&plane, "downgrade");

    // A plain member's DIRECT publish (no `--propose`) is DOWNGRADED to a proposal IN-TRANSACTION — the
    // client surfaces the NEEDS_REVIEW outcome as `Proposed` (never an error), and `current` does not move.
    let digest = pub_h.draft_digest();
    let proposal = match pub_h
        .publish(false, &approve_token(SKILL, &digest))
        .expect("a direct publish on a reviewed bundle downgrades, it does not error")
    {
        PublishResult::Proposed(d) => d.proposal,
        other => panic!("expected a downgraded proposal, got {other:?}"),
    };

    // The follower does NOT receive new bytes — `current` is frozen at the genesis (1,1).
    let follower_before = follower("downgrade-f0");
    let pulled = follower_before.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(pulled.skills[0].applied, Generation { epoch: 1, seq: 1 });

    // A DISTINCT reviewer approves the downgraded proposal (four-eyes satisfied) — `current` advances.
    let reviewer = enrolled_reviewer(&plane, "downgrade-rev");
    let review = reviewer
        .review(&proposal, true)
        .expect("a different reviewer may approve the downgraded proposal");
    assert_eq!(
        review.current_generation,
        Some(Generation { epoch: 1, seq: 2 })
    );

    // Now a follower applies the reviewed candidate (the downgrade reached the team via review).
    let follower_after = follower("downgrade-f1");
    let pulled = follower_after.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(pulled.skills[0].applied, Generation { epoch: 1, seq: 2 });
    assert_eq!(follower_after.placement_files(SKILL), expected(DRAFT));
}

// ── scenario 7: four-eyes — a proposer cannot self-approve under review_required ────────────────────────

#[test]
fn four_eyes_blocks_a_self_approve_under_review_required() {
    let plane = start_plane("foureyes");
    set_review_required(&plane, true);
    let pub_h = drafting_publisher(&plane, "foureyes");

    let digest = pub_h.draft_digest();
    let proposal = match pub_h
        .publish(true, &approve_token(SKILL, &digest))
        .expect("propose is allowed under review_required")
    {
        PublishResult::Proposed(d) => d.proposal,
        other => panic!("expected a proposal, got {other:?}"),
    };

    // The SAME identity approving its own proposal under review_required ⇒ DENIED (four-eyes).
    let err = pub_h
        .review(&proposal, true)
        .expect_err("four-eyes blocks self-approve");
    assert!(err.to_lowercase().contains("denied"), "got: {err}");

    // current never moved.
    let follower = follower("foureyes-f");
    let pulled = follower.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(pulled.skills[0].applied, Generation { epoch: 1, seq: 1 });
}

// ── scenario 8: delegated consent — a DIFFERENT reviewer approves; the follower applies, no prompt ──────

#[test]
fn delegated_consent_lands_on_a_follower_under_review_required() {
    let plane = start_plane("delegated");
    set_review_required(&plane, true);
    let pub_h = drafting_publisher(&plane, "delegated");

    let digest = pub_h.draft_digest();
    let proposal = match pub_h
        .publish(true, &approve_token(SKILL, &digest))
        .expect("propose")
    {
        PublishResult::Proposed(d) => d.proposal,
        other => panic!("expected a proposal, got {other:?}"),
    };

    // A DISTINCT reviewer approves (four-eyes satisfied) — current moves to the candidate.
    let reviewer = enrolled_reviewer(&plane, "delegated-rev");
    let review = reviewer
        .review(&proposal, true)
        .expect("a different reviewer may approve");
    assert_eq!(
        review.current_generation,
        Some(Generation { epoch: 1, seq: 2 })
    );

    // The follower applies the reviewed candidate with no prompt (delegated consent).
    let follower = follower("delegated-f");
    let pulled = follower.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(pulled.skills[0].applied, Generation { epoch: 1, seq: 2 });
    assert_eq!(follower.placement_files(SKILL), expected(DRAFT));
}

// ── scenario 9: the proposals read route — pull's count + list's enumeration ────────────────────────────

#[test]
fn an_open_proposal_surfaces_in_pull_count_and_list() {
    let plane = start_plane("route");
    let pub_h = drafting_publisher(&plane, "route");

    // Before any proposal: zero.
    assert_eq!(pub_h.proposals_awaiting(), 0, "no proposals yet");

    let digest = pub_h.draft_digest();
    let proposal = match pub_h
        .publish(true, &approve_token(SKILL, &digest))
        .expect("propose")
    {
        PublishResult::Proposed(d) => d.proposal,
        other => panic!("expected a proposal, got {other:?}"),
    };

    // `pull --json` reports a real count; `list <skill>` enumerates the proposal by `<skill>@<hash>`.
    assert_eq!(
        pub_h.proposals_awaiting(),
        1,
        "one open proposal on the followed skill"
    );
    let pending = pub_h.list_pending_proposals();
    assert_eq!(
        pending,
        vec![proposal.clone()],
        "list enumerates the open proposal's @hash"
    );
}
