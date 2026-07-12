//! CHANNELS e2e — the delivery-driven reconcile end to end over loopback HTTP: the REAL client
//! engine (`ops::pull_reconcile` over the REAL `ureq` [`DeliverySource`]) against the REAL composed
//! plane (`topos_plane::router` over a real `plane-store::Authority`) on a real `127.0.0.1:0` socket.
//!
//! What only a cross-crate loopback run can prove for channels: that ONE `GET …/delivery` per enrolled
//! workspace answers "what should this device have", and the client converges — a genesis lands in
//! `everyone` and a fresh member INSTALLS it as a first-receive offer; joining/leaving channels adds
//! and withdraws bytes (reference-counted); an unfollow FREEZES in place while an upstream withdrawal
//! CLEANS the agent dir (retaining the sidecar, snapshotting any draft); a downgraded publish rides the
//! review flow to the follower; the fleet applied-state report lands; archive withdraws + notifies; a
//! purged ancestor doesn't break a fresh install of the live descendant; and a removed member freezes
//! everything (never a clean). Server-side setup rides `plane.authority`; row witnesses ride
//! `plane.pool`; the client half is driven through the `test-fixtures` `ReconcileHarness`/
//! `ContributeHarness`.

mod common;

use common::{NOW, Seeded};
use plane_store::{
    Authority, CandidateUpload, CommitId, DeploymentMode, FileMode, GovernanceOp,
    GovernanceRequest, OpId, Principal, SkillId, UploadedFile, WorkspaceId,
};
use topos::test_support::{ContributeHarness, PublishResult, ReconcileHarness};
use topos_types::Generation;
use topos_types::results::PullAction;

const WS: &str = "w_acme";
const AT: &str = "2026-07-08T00:00:00Z";

// The publisher/owner — seeds genesis + drives archive/purge/removal.
const OWNER: &str = "owner@acme.test";
const OWNER_DKID: &str = "dk_owner";
const OWNER_PUBKEY: [u8; 32] = [41u8; 32];
const OWNER_CRED: &str = "wc_owner_secret";

// The follower — a confirmed member whose device reconciles.
const FOLLOWER: &str = "follower@acme.test";
const FOL_DKID: &str = "dk_fol";
const FOL_PUBKEY: [u8; 32] = [42u8; 32];
const FOL_CRED: &str = "wc_fol_secret";

// A reviewer (scenario 6) — the four-eyes approver.
const REVIEWER: &str = "reviewer@acme.test";
const REV_DKID: &str = "dk_rev";
const REV_PUBKEY: [u8; 32] = [43u8; 32];
const REV_CRED: &str = "wc_rev_secret";

fn ws() -> WorkspaceId {
    WorkspaceId::parse(WS).unwrap()
}

/// The seed closure stands nothing up — every scenario seeds post-startup through `plane.authority`.
async fn empty_seed(_authority: &Authority) -> Seeded {
    Seeded::default()
}

/// Genesis-publish `skill` (name minted from `display_name`) into `everyone` (or `channel`), by the
/// device holding `credential`. Returns the genesis version id.
#[allow(clippy::too_many_arguments)]
async fn genesis(
    a: &Authority,
    skill_id: &str,
    credential: &str,
    op_id: &str,
    files: Vec<UploadedFile>,
    display_name: &str,
    channel: Option<&str>,
) -> CommitId {
    let auth = plane_store::DeviceOpAuth {
        credential: credential.to_owned(),
        op: plane_store::DeviceOp::PublishDirect,
        expected: Generation { epoch: 0, seq: 0 },
    };
    let receipt = a
        .publish(
            &ws(),
            &SkillId::parse(skill_id).unwrap(),
            &OpId::parse(op_id).unwrap(),
            CandidateUpload {
                files,
                parents: vec![],
                author: "d_seed".to_owned(),
                message: "topos publish".to_owned(),
            },
            auth,
            Some(display_name),
            channel,
            AT,
            NOW,
        )
        .await
        .expect("genesis publish");
    assert_eq!(
        receipt.outcome,
        topos_types::TerminalOutcome::Ok,
        "genesis lands"
    );
    receipt.version_id.expect("genesis version id")
}

/// The standard genesis bundle (a doc + an executable script — the exec bit must survive end to end).
fn deploy_files() -> Vec<UploadedFile> {
    common::genesis_files()
}

/// Whether a `SELECT 1` row exists (a builtin-channel / placement row witness).
async fn exists(pool: &sqlx::PgPool, sql: &str, binds: &[&str]) -> bool {
    let mut q = sqlx::query(sql);
    for b in binds {
        q = q.bind(*b);
    }
    q.fetch_optional(pool).await.unwrap().is_some()
}
/// ONE sweep is all a brand-new arrival takes: the reconcile binds the skill to its workspace
/// credential before any fetch (the `follows.json`-derived per-skill map cannot yet name a skill
/// this device has never held), so the first sweep installs the baseline AND discloses the offer.
/// A second sweep would be a no-op — the kernel's I-TOFU consent holds the bytes behind one accept.
fn install_then_offer(rig: &ReconcileHarness) -> (topos_types::results::PullData, Vec<String>) {
    let (data, warnings) = rig.reconcile();
    assert!(
        warnings.is_empty(),
        "a new arrival must install cleanly on its FIRST sweep — no credential gap: {warnings:?}"
    );
    (data, warnings)
}

// ── 1. genesis → everyone → a fresh member installs it as a first-receive offer ──────────────────────

#[test]
fn genesis_delivers_via_everyone_and_a_fresh_member_installs_then_accepts() {
    let plane = common::start_plane("topos-channels-e2e", "s1", true, empty_seed);
    let files = deploy_files();
    let files2 = files.clone();
    plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        common::seed_member(a, &ws(), OWNER_DKID, &OWNER_PUBKEY, OWNER, "owner", OWNER_CRED).await;
        common::seed_member(a, &ws(), FOL_DKID, &FOL_PUBKEY, FOLLOWER, "member", FOL_CRED).await;
        genesis(a, "s_deploy", OWNER_CRED, "c0000000-0000-4000-8000-000000000001", files2, "Deploy", None).await;

        // Row witnesses: `everyone` is the structural builtin, and the publish placed the skill there.
        assert!(
            exists(&plane.pool, "SELECT 1 FROM channels WHERE workspace_id = $1 AND channel_id = 'everyone' AND builtin = 1", &[WS]).await,
            "everyone is the structural builtin channel"
        );
        assert!(
            exists(&plane.pool, "SELECT 1 FROM channel_skills WHERE workspace_id = $1 AND channel_id = 'everyone' AND skill_id = $2", &[WS, "s_deploy"]).await,
            "the genesis is placed in everyone"
        );
    });

    let rig = ReconcileHarness::new("chn-s1");
    rig.enroll_member(&plane.base_url, WS, FOL_CRED);

    // The reconcile OFFERS the first receive (never auto-lands — I-TOFU consent).
    let (data, warnings) = install_then_offer(&rig);
    assert!(warnings.is_empty(), "clean reconcile: {warnings:?}");
    let offer = data
        .skills
        .iter()
        .find(|s| s.action == PullAction::Offered)
        .expect("a first-receive offer row");
    assert!(offer.offer.is_some(), "the offer re-discloses the version");
    assert!(
        !rig.placement_exists("s_deploy"),
        "offered, not yet materialized"
    );
    assert_eq!(
        rig.follows(),
        vec![("s_deploy".to_owned(), WS.to_owned(), true)]
    );

    // The explicit accept lands the bytes byte-exact (incl. the executable bit).
    let _ = rig.accept("deploy");
    assert_eq!(
        rig.placement_files("s_deploy"),
        common::expected_placement(&files),
        "the accepted bundle is byte-exact"
    );
}

// ── 2. channel add installs; channel remove withdraws (draft snapshotted, sidecar retained) ──────────

#[test]
fn a_channel_placement_installs_and_its_removal_withdraws_snapshotting_a_draft() {
    let plane = common::start_plane("topos-channels-e2e", "s2", true, empty_seed);
    let files = deploy_files();
    let files2 = files.clone();
    plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        common::seed_member(
            a,
            &ws(),
            OWNER_DKID,
            &OWNER_PUBKEY,
            OWNER,
            "owner",
            OWNER_CRED,
        )
        .await;
        common::seed_member(
            a,
            &ws(),
            FOL_DKID,
            &FOL_PUBKEY,
            FOLLOWER,
            "member",
            FOL_CRED,
        )
        .await;
        // B is born in `ops` only; the follower joins ops.
        genesis(
            a,
            "s_beacon",
            OWNER_CRED,
            "c0000000-0000-4000-8000-000000000002",
            files2,
            "Beacon",
            Some("ops"),
        )
        .await;
        a.channel_join(&ws(), FOL_CRED, "ops", AT).await.unwrap();
    });

    let rig = ReconcileHarness::new("chn-s2");
    rig.enroll_member(&plane.base_url, WS, FOL_CRED);
    let _ = rig.reconcile();
    let _ = rig.accept("beacon");
    assert_eq!(
        rig.placement_files("s_beacon"),
        common::expected_placement(&files)
    );

    // A LOCAL draft ahead of current (so the withdrawal must snapshot it), then upstream drops B.
    rig.edit_placement("s_beacon", &[("SKILL.md", false, b"# beacon DRAFT\n")]);
    plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        // Remove B from ops — it is now referenced by NO channel.
        assert_eq!(
            a.channel_unplace(&ws(), OWNER_CRED, "ops", "s_beacon", AT)
                .await
                .unwrap(),
            plane_store::CurationOutcome::Removed
        );
    });

    let (data, warnings) = rig.reconcile();
    assert!(warnings.is_empty(), "clean reconcile: {warnings:?}");
    assert!(
        data.skills
            .iter()
            .any(|s| s.action == PullAction::Withdrawn),
        "upstream withdrawal is classified Withdrawn: {:?}",
        data.skills
    );
    assert!(
        !rig.placement_exists("s_beacon"),
        "the agent dir is cleaned on withdrawal"
    );
    assert!(
        rig.store_version_count("s_beacon") >= 2,
        "the sidecar retains the fetched version AND the snapshotted draft"
    );
    // A withdrawal is a DELIVERY change, not a SUBSCRIPTION change: the follow entry stays LIVE, so a
    // curator re-placing the skill re-installs it clean on the very next reconcile. (Contrast the
    // person-scoped Detached path in the sibling scenario, where the placement survives instead.)
    assert_eq!(
        rig.follows(),
        vec![("s_beacon".to_owned(), WS.to_owned(), true)],
        "the subscription survives an upstream withdrawal"
    );

    // …and because the subscription survived, a curator RE-PLACING the skill puts it straight back
    // into the person's delivered set (the server-side half of the self-heal).
    plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        a.channel_place(&ws(), OWNER_CRED, "ops", "s_beacon", AT)
            .await
            .unwrap();
        let d = a.delivery(&ws(), FOL_CRED).await.unwrap();
        assert!(
            d.skills.iter().any(|s| s.skill_id == "s_beacon"),
            "a re-placed skill is delivered again — no detachment stranded the subscription"
        );
        assert!(
            !d.detached.contains(&"s_beacon".to_owned()),
            "an UPSTREAM withdrawal never wrote a person-scoped detachment"
        );
    });

    // …and the CLIENT completes the round trip: the withdrawal reset the sync state to the
    // never-received sentinel, so the re-delivered skill is a fresh first-receive — offered (I-TOFU),
    // then accepted, and the bytes are back on disk byte-exact. Without that reset the skill would
    // read as "already current" against an absent placement and never come back.
    let (data, warnings) = rig.reconcile();
    assert!(warnings.is_empty(), "clean re-arrival: {warnings:?}");
    assert!(
        data.skills.iter().any(|s| s.action == PullAction::Offered),
        "a re-placed skill re-arrives as a disclosed first-receive offer: {:?}",
        data.skills
    );
    let _ = rig.accept("beacon");
    assert!(
        rig.placement_exists("s_beacon"),
        "accepting the re-arrival restores the agent dir"
    );
    assert_eq!(
        rig.placement_files("s_beacon"),
        common::expected_placement(&files),
        "the restored bytes are byte-exact — the pristine team version, the draft still in the store"
    );
}

// ── 3. two channels, one copy: one follows entry, one placement, one row ──────────────────────────────

#[test]
fn a_skill_in_two_joined_channels_delivers_exactly_one_copy() {
    let plane = common::start_plane("topos-channels-e2e", "s3", true, empty_seed);
    let files = deploy_files();
    let files2 = files.clone();
    plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        common::seed_member(
            a,
            &ws(),
            OWNER_DKID,
            &OWNER_PUBKEY,
            OWNER,
            "owner",
            OWNER_CRED,
        )
        .await;
        common::seed_member(
            a,
            &ws(),
            FOL_DKID,
            &FOL_PUBKEY,
            FOLLOWER,
            "member",
            FOL_CRED,
        )
        .await;
        // B in ops AND eng; the follower joins both.
        genesis(
            a,
            "s_beacon",
            OWNER_CRED,
            "c0000000-0000-4000-8000-000000000003",
            files2,
            "Beacon",
            Some("ops"),
        )
        .await;
        a.channel_place(&ws(), OWNER_CRED, "eng", "s_beacon", AT)
            .await
            .unwrap();
        a.channel_join(&ws(), FOL_CRED, "ops", AT).await.unwrap();
        a.channel_join(&ws(), FOL_CRED, "eng", AT).await.unwrap();
    });

    let rig = ReconcileHarness::new("chn-s3");
    rig.enroll_member(&plane.base_url, WS, FOL_CRED);
    let (data, _) = install_then_offer(&rig);
    assert_eq!(
        data.skills
            .iter()
            .filter(|s| s.action == PullAction::Offered)
            .count(),
        1,
        "two channels deliver ONE offer row: {:?}",
        data.skills
    );
    assert_eq!(
        rig.follows(),
        vec![("s_beacon".to_owned(), WS.to_owned(), true)],
        "exactly one follows entry"
    );
    let _ = rig.accept("beacon");
    assert_eq!(
        rig.placement_files("s_beacon"),
        common::expected_placement(&files)
    );
}

// ── 4. leaving a channel freezes nothing while another still references it; unfollow detaches ──────────

#[test]
fn leaving_a_channel_keeps_a_still_referenced_skill_but_unfollow_detaches_in_place() {
    let plane = common::start_plane("topos-channels-e2e", "s4", true, empty_seed);
    let files = deploy_files();
    let files2 = files.clone();
    plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        common::seed_member(
            a,
            &ws(),
            OWNER_DKID,
            &OWNER_PUBKEY,
            OWNER,
            "owner",
            OWNER_CRED,
        )
        .await;
        common::seed_member(
            a,
            &ws(),
            FOL_DKID,
            &FOL_PUBKEY,
            FOLLOWER,
            "member",
            FOL_CRED,
        )
        .await;
        // B in everyone AND ops; the follower joins ops.
        genesis(
            a,
            "s_beacon",
            OWNER_CRED,
            "c0000000-0000-4000-8000-000000000004",
            files2,
            "Beacon",
            None,
        )
        .await;
        a.channel_place(&ws(), OWNER_CRED, "ops", "s_beacon", AT)
            .await
            .unwrap();
        a.channel_join(&ws(), FOL_CRED, "ops", AT).await.unwrap();
    });

    let rig = ReconcileHarness::new("chn-s4");
    rig.enroll_member(&plane.base_url, WS, FOL_CRED);
    let _ = rig.reconcile();
    let _ = rig.accept("beacon");
    assert!(rig.placement_exists("s_beacon"));

    // Leave ops — everyone still references B, so it stays LIVE (not withdrawn).
    plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        a.channel_leave(&ws(), FOL_CRED, "ops", NOW, AT)
            .await
            .unwrap();
        // The server still delivers B (via everyone).
        let d = a.delivery(&ws(), FOL_CRED).await.unwrap();
        assert!(
            d.skills.iter().any(|s| s.skill_id == "s_beacon"),
            "everyone keeps it live"
        );
    });
    let (data, _) = rig.reconcile();
    assert!(
        !data
            .skills
            .iter()
            .any(|s| s.action == PullAction::Withdrawn),
        "a still-referenced skill is never withdrawn on a channel leave"
    );
    assert!(rig.placement_exists("s_beacon"), "the placement is intact");

    // Now UNFOLLOW B (person-scoped) — the reconcile FREEZES it in place (Detached), bytes intact.
    plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        a.unfollow_skill(&ws(), FOL_CRED, "s_beacon", NOW, AT)
            .await
            .unwrap();
    });
    let (data, _) = rig.reconcile();
    assert!(
        data.skills.iter().any(|s| s.action == PullAction::Detached),
        "an unfollow is classified Detached: {:?}",
        data.skills
    );
    assert!(
        rig.placement_exists("s_beacon"),
        "a detach freezes in place — the bytes stay"
    );
}

// ── 5. exclusion round-trip (the remove verb's local half + the server exclusion) ─────────────────────

#[test]
fn a_device_exclusion_frozen_locally_no_ops_and_a_follow_lifts_it() {
    let plane = common::start_plane("topos-channels-e2e", "s5", true, empty_seed);
    let files = deploy_files();
    let files2 = files.clone();
    plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        common::seed_member(
            a,
            &ws(),
            OWNER_DKID,
            &OWNER_PUBKEY,
            OWNER,
            "owner",
            OWNER_CRED,
        )
        .await;
        common::seed_member(
            a,
            &ws(),
            FOL_DKID,
            &FOL_PUBKEY,
            FOLLOWER,
            "member",
            FOL_CRED,
        )
        .await;
        genesis(
            a,
            "s_deploy",
            OWNER_CRED,
            "c0000000-0000-4000-8000-000000000005",
            files2,
            "Deploy",
            None,
        )
        .await;
    });

    let rig = ReconcileHarness::new("chn-s5");
    rig.enroll_member(&plane.base_url, WS, FOL_CRED);
    let _ = rig.reconcile();
    let _ = rig.accept("deploy");
    assert!(rig.placement_exists("s_deploy"));

    // The `remove` verb's halves: the server device-exclusion + the LOCAL freeze (following=false +
    // placement removed) BEFORE the next sync — so the reconcile finds an already-frozen entry.
    plane.rt.block_on(async {
        plane
            .authority
            .exclude_device(&ws(), FOL_CRED, "s_deploy", AT)
            .await
            .unwrap();
    });
    rig.simulate_local_remove("s_deploy");
    let (data, warnings) = rig.reconcile();
    assert!(
        warnings.is_empty(),
        "no ACCESS_GONE — the workspace is still reachable: {warnings:?}"
    );
    assert!(
        !data.skills.iter().any(|s| s.action == PullAction::Offered),
        "a locally-frozen + server-excluded skill is NOT re-installed"
    );
    assert!(
        !rig.placement_exists("s_deploy"),
        "the removal stays clean (no resurrection)"
    );

    // A `follow` lifts the exclusion server-side + resumes the local subscription → the skill RETURNS.
    plane.rt.block_on(async {
        plane
            .authority
            .follow_skill(&ws(), FOL_CRED, "s_deploy", AT)
            .await
            .unwrap();
        // The server delivers it again (the exclusion is lifted).
        let d = plane.authority.delivery(&ws(), FOL_CRED).await.unwrap();
        assert!(
            d.skills.iter().any(|s| s.skill_id == "s_deploy"),
            "exclusion lifted server-side"
        );
    });
    rig.resume_local_following("s_deploy");
    let (_data, warnings) = rig.reconcile();
    assert!(
        warnings.is_empty(),
        "the resume reconciles cleanly: {warnings:?}"
    );
    assert_eq!(
        rig.follows(),
        vec![("s_deploy".to_owned(), WS.to_owned(), true)],
        "the local subscription is live again"
    );
}

// ── 6. protection: a member's publish downgrades, a reviewer approves, the follower lands v2 ──────────

#[test]
fn a_downgraded_publish_is_approved_and_reaches_the_follower_with_a_verdict_notice() {
    let plane = common::start_plane("topos-channels-e2e", "s6", true, empty_seed);
    let v1_files = deploy_files();
    let v1_seed = v1_files.clone();
    plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        // The author is a MEMBER (so a direct publish downgrades under review-required); a reviewer + a follower.
        common::seed_member(
            a,
            &ws(),
            OWNER_DKID,
            &OWNER_PUBKEY,
            OWNER,
            "member",
            OWNER_CRED,
        )
        .await;
        common::seed_member(
            a,
            &ws(),
            REV_DKID,
            &REV_PUBKEY,
            REVIEWER,
            "reviewer",
            REV_CRED,
        )
        .await;
        common::seed_member(
            a,
            &ws(),
            FOL_DKID,
            &FOL_PUBKEY,
            FOLLOWER,
            "member",
            FOL_CRED,
        )
        .await;
        genesis(
            a,
            "s_deploy",
            OWNER_CRED,
            "c0000000-0000-4000-8000-000000000006",
            v1_seed,
            "Deploy",
            None,
        )
        .await;
        a.set_review_required(&ws(), true).await.unwrap();
    });

    // The follower installs + accepts v1.
    let follower = ReconcileHarness::new("chn-s6-fol");
    follower.enroll_member(&plane.base_url, WS, FOL_CRED);
    let _ = follower.reconcile();
    let _ = follower.accept("deploy");

    // The member-author publishes v2 through the REAL client — the server DOWNGRADES it to a proposal.
    let mut author = ContributeHarness::new("chn-s6-author");
    author.enroll(
        &plane.base_url,
        WS,
        "s_deploy",
        OWNER_CRED,
        true,
        &[("SKILL.md", false, b"placeholder\n")],
    );
    let _ = author.pull(); // land v1
    let v2: &[(&str, bool, &[u8])] = &[
        ("SKILL.md", false, b"# deploy v2\n"),
        ("run.sh", true, b"#!/bin/sh\necho v2\n"),
    ];
    author.edit_placement(v2);
    let digest = author.draft_digest();
    match author
        .publish(false, &format!("s_deploy@{digest}"))
        .unwrap()
    {
        PublishResult::Proposed(_) => {}
        other => panic!("a member's direct publish on a reviewed bundle must downgrade: {other:?}"),
    }

    // A reviewer approves the open proposal (session lane; four-eyes across the distinct principal).
    plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        let (cand, base_epoch, base_seq): (Vec<u8>, i64, i64) = sqlx::query_as(
            "SELECT commit_id, base_epoch, base_seq FROM proposals \
             WHERE workspace_id = $1 AND skill_id = $2 AND status = 'open'",
        )
        .bind(WS)
        .bind("s_deploy")
        .fetch_one(&plane.pool)
        .await
        .unwrap();
        let candidate = CommitId(cand.try_into().expect("32-byte commit"));
        let expected = Generation {
            epoch: base_epoch as u64,
            seq: base_seq as u64,
        };
        let r = a
            .review_approve_session(
                &ws(),
                &SkillId::parse("s_deploy").unwrap(),
                candidate,
                expected,
                "77777777-0000-4000-8000-000000000001",
                REVIEWER,
                DeploymentMode::Cloud,
                AT,
                NOW,
            )
            .await
            .unwrap();
        assert_eq!(
            r.outcome,
            topos_types::TerminalOutcome::Ok,
            "the reviewer's approve lands v2"
        );

        // The author gets a verdict NOTICE (the approve emitted it to the proposer).
        let d = a.delivery(&ws(), OWNER_CRED).await.unwrap();
        assert!(
            d.notices.iter().any(|n| n.kind == "verdict"),
            "the author is notified of the verdict: {:?}",
            d.notices
        );
    });

    // The follower's next reconcile auto-lands v2 byte-exact (an already-followed skill that moved).
    let _ = follower.reconcile();
    assert_eq!(
        follower.placement_files("s_deploy"),
        common::expected(v2),
        "the follower lands the approved v2"
    );
}

// ── 7. the fleet report — after a reconcile applies v1, the device_skill_state row + last_report_at ──

#[test]
fn the_reconcile_reports_applied_state_to_the_fleet() {
    let plane = common::start_plane("topos-channels-e2e", "s7", true, empty_seed);
    let files = deploy_files();
    let files2 = files.clone();
    let v1 = plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        common::seed_member(
            a,
            &ws(),
            OWNER_DKID,
            &OWNER_PUBKEY,
            OWNER,
            "owner",
            OWNER_CRED,
        )
        .await;
        common::seed_member(
            a,
            &ws(),
            FOL_DKID,
            &FOL_PUBKEY,
            FOLLOWER,
            "member",
            FOL_CRED,
        )
        .await;
        genesis(
            a,
            "s_deploy",
            OWNER_CRED,
            "c0000000-0000-4000-8000-000000000007",
            files2,
            "Deploy",
            None,
        )
        .await
    });

    let rig = ReconcileHarness::new("chn-s7");
    rig.enroll_member(&plane.base_url, WS, FOL_CRED);
    let _ = rig.reconcile(); // offer
    let _ = rig.accept("deploy"); // land v1 (map.applied_commit = v1)
    let _ = rig.reconcile(); // the reconcile that reports v1 as applied

    plane.rt.block_on(async {
        // The fleet row names the applied version, live (detached 0), keyed by the seeded device.
        let (applied, detached): (Vec<u8>, i64) = sqlx::query_as(
            "SELECT applied_commit, detached FROM device_skill_state \
             WHERE workspace_id = $1 AND device_key_id = $2 AND skill_id = $3",
        )
        .bind(WS)
        .bind(FOL_DKID)
        .bind("s_deploy")
        .fetch_one(&plane.pool)
        .await
        .expect("a fleet row after the reconcile report");
        assert_eq!(applied, v1.0.to_vec(), "the fleet names the applied version");
        assert_eq!(detached, 0, "live, not detached");

        // The staleness clock is set.
        let last: Option<i64> = sqlx::query_scalar(
            "SELECT last_report_at FROM device_registry WHERE workspace_id = $1 AND device_key_id = $2",
        )
        .bind(WS)
        .bind(FOL_DKID)
        .fetch_one(&plane.pool)
        .await
        .unwrap();
        assert!(last.is_some(), "the report stamps last_report_at");
    });
}

// ── 8. archive withdraws the follower + closes the open proposal + notifies the author ────────────────

#[test]
fn an_archive_withdraws_the_follower_frees_the_name_and_closes_the_open_proposal() {
    let plane = common::start_plane("topos-channels-e2e", "s8", true, empty_seed);
    let files = deploy_files();
    let v1 = plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        common::seed_member(
            a,
            &ws(),
            OWNER_DKID,
            &OWNER_PUBKEY,
            OWNER,
            "owner",
            OWNER_CRED,
        )
        .await;
        common::seed_member(
            a,
            &ws(),
            FOL_DKID,
            &FOL_PUBKEY,
            FOLLOWER,
            "member",
            FOL_CRED,
        )
        .await;
        genesis(
            a,
            "s_deploy",
            OWNER_CRED,
            "c0000000-0000-4000-8000-000000000008",
            files.clone(),
            "Deploy",
            None,
        )
        .await
    });

    let rig = ReconcileHarness::new("chn-s8");
    rig.enroll_member(&plane.base_url, WS, FOL_CRED);
    let _ = rig.reconcile();
    let _ = rig.accept("deploy");
    assert!(rig.placement_exists("s_deploy"));

    plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        // The FOLLOWER opens a real proposal (so the archive closes it + notifies them).
        let cand = CandidateUpload {
            files: vec![UploadedFile {
                path: "SKILL.md".to_owned(),
                mode: FileMode::Regular,
                bytes: b"# proposed\n".to_vec(),
            }],
            parents: vec![v1],
            author: "d_seed".to_owned(),
            message: "topos publish".to_owned(),
        };
        let pr = a
            .propose(
                &ws(),
                &SkillId::parse("s_deploy").unwrap(),
                &OpId::parse("88888888-0000-4000-8000-000000000001").unwrap(),
                cand,
                plane_store::DeviceOpAuth {
                    credential: FOL_CRED.to_owned(),
                    op: plane_store::DeviceOp::PublishPropose,
                    expected: Generation { epoch: 1, seq: 1 },
                },
                None,
                None,
                AT,
                NOW,
            )
            .await
            .unwrap();
        assert_eq!(pr.outcome, topos_types::TerminalOutcome::NeedsReview);

        // The owner archives it.
        assert!(matches!(
            a.archive_skill_session(&ws(), OWNER, "deploy", DeploymentMode::Cloud, AT, NOW)
                .await
                .unwrap(),
            plane_store::LifecycleOutcome::Archived { .. }
        ));
        // The base name is freed (the catalog row is renamed away).
        assert!(
            !exists(
                &plane.pool,
                "SELECT 1 FROM catalog WHERE workspace_id = $1 AND name = 'deploy'",
                &[WS]
            )
            .await,
            "the base name is freed"
        );
    });

    // The follower's reconcile withdraws (agent dir cleaned, sidecar retained) + the author notice arrives.
    let (data, warnings) = rig.reconcile();
    assert!(warnings.is_empty(), "clean reconcile: {warnings:?}");
    assert!(
        data.skills
            .iter()
            .any(|s| s.action == PullAction::Withdrawn),
        "the archived skill is withdrawn: {:?}",
        data.skills
    );
    assert!(
        !rig.placement_exists("s_deploy"),
        "the agent dir is cleaned"
    );
    assert!(
        rig.store_version_count("s_deploy") >= 1,
        "the sidecar retains the bytes"
    );

    plane.rt.block_on(async {
        // The FOLLOWER (the proposer) has a proposal_closed notice.
        let d = plane.authority.delivery(&ws(), FOL_CRED).await.unwrap();
        assert!(
            d.notices.iter().any(|n| n.kind == "proposal_closed"),
            "the proposer is notified of the auto-close: {:?}",
            d.notices
        );
    });
}

// ── 9. a purged ancestor doesn't break a fresh install of the live descendant ─────────────────────────

#[test]
fn a_fresh_follower_installs_v2_over_a_purged_v1_ancestor() {
    let plane = common::start_plane("topos-channels-e2e", "s9", true, empty_seed);
    // v1 = {shared, secret}; v2 = {shared (unchanged), other}. v1 is purged.
    let shared: &[u8] = b"shared-content";
    let v2_files = vec![
        UploadedFile {
            path: "shared.txt".to_owned(),
            mode: FileMode::Regular,
            bytes: shared.to_vec(),
        },
        UploadedFile {
            path: "other.txt".to_owned(),
            mode: FileMode::Regular,
            bytes: b"v2-new".to_vec(),
        },
    ];
    let v2_expected = v2_files.clone();
    plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        common::seed_member(
            a,
            &ws(),
            OWNER_DKID,
            &OWNER_PUBKEY,
            OWNER,
            "owner",
            OWNER_CRED,
        )
        .await;
        common::seed_member(
            a,
            &ws(),
            FOL_DKID,
            &FOL_PUBKEY,
            FOLLOWER,
            "member",
            FOL_CRED,
        )
        .await;
        let v1 = genesis(
            a,
            "s_deploy",
            OWNER_CRED,
            "c0000000-0000-4000-8000-000000000009",
            vec![
                UploadedFile {
                    path: "shared.txt".to_owned(),
                    mode: FileMode::Regular,
                    bytes: shared.to_vec(),
                },
                UploadedFile {
                    path: "secret.txt".to_owned(),
                    mode: FileMode::Regular,
                    bytes: b"v1-secret".to_vec(),
                },
            ],
            "Deploy",
            None,
        )
        .await;
        let c1 = SkillId::parse("s_deploy").unwrap();
        a.seed_published_child(
            &ws(),
            &c1,
            OWNER_CRED,
            &OpId::parse("99999999-0000-4000-8000-000000000002").unwrap(),
            v1,
            v2_files,
            "d_seed",
            "topos publish",
            AT,
            NOW,
        )
        .await
        .unwrap();
        // Purge the v1 ancestor (its bytes drop out of history; v2 stays current).
        assert_eq!(
            a.purge_version_session(&ws(), OWNER, "deploy", v1, DeploymentMode::Cloud, AT, NOW)
                .await
                .unwrap(),
            plane_store::PurgeOutcome::Purged
        );
        a.run_gc(&ws(), NOW + 1_000_000).await.unwrap();
    });

    // A FRESH follower installs v2 cleanly — the ancestor backfill shallow-stops at the purged v1.
    let rig = ReconcileHarness::new("chn-s9");
    rig.enroll_member(&plane.base_url, WS, FOL_CRED);
    let (_data, warnings) = install_then_offer(&rig);
    assert!(
        warnings.is_empty(),
        "the offer sweep is NOT tripped by the purged ancestor: {warnings:?}"
    );
    let _ = rig.accept("deploy");
    assert_eq!(
        rig.placement_files("s_deploy"),
        common::expected_placement(&v2_expected),
        "v2 installs byte-exact (the shared file included) despite the purged ancestor"
    );
}

// ── 10. a removed member freezes everything (ACCESS_GONE, never a clean); re-adding resumes ───────────

#[test]
fn a_removed_member_freezes_in_place_and_re_adding_resumes() {
    let plane = common::start_plane("topos-channels-e2e", "s10", true, empty_seed);
    let files = deploy_files();
    plane.rt.block_on(async {
        let a: &Authority = &plane.authority;
        common::seed_member(
            a,
            &ws(),
            OWNER_DKID,
            &OWNER_PUBKEY,
            OWNER,
            "owner",
            OWNER_CRED,
        )
        .await;
        common::seed_member(
            a,
            &ws(),
            FOL_DKID,
            &FOL_PUBKEY,
            FOLLOWER,
            "member",
            FOL_CRED,
        )
        .await;
        genesis(
            a,
            "s_deploy",
            OWNER_CRED,
            "c0000000-0000-4000-8000-00000000000a",
            files.clone(),
            "Deploy",
            None,
        )
        .await;
    });

    let rig = ReconcileHarness::new("chn-s10");
    rig.enroll_member(&plane.base_url, WS, FOL_CRED);
    let _ = rig.reconcile();
    let _ = rig.accept("deploy");
    assert!(rig.placement_exists("s_deploy"));

    // The owner removes the follower (device lane).
    plane.rt.block_on(async {
        let out = plane
            .authority
            .roster_remove(
                &ws(),
                "aaaa0000-0000-4000-8000-00000000000a",
                GovernanceRequest {
                    credential: OWNER_CRED.to_owned(),
                    op: GovernanceOp::RosterRemove {
                        target: Principal::parse(FOLLOWER).unwrap(),
                    },
                },
                AT,
                NOW,
            )
            .await
            .unwrap();
        assert_eq!(out, plane_store::GovernanceOutcome::Ok);
    });

    // The removed follower's reconcile FAILS CLOSED: an ACCESS_GONE warning, everything frozen in place.
    let (_data, warnings) = rig.reconcile();
    assert!(
        warnings.iter().any(|w| w.contains("ACCESS_GONE")),
        "a removed member's whole-workspace 404 is ACCESS_GONE: {warnings:?}"
    );
    assert!(
        rig.placement_exists("s_deploy"),
        "the bytes stay — removal is never a clean"
    );

    // Re-adding the member re-enables the same device: the reconcile resumes.
    plane.rt.block_on(async {
        plane
            .authority
            .seed_workspace_member(
                &ws(),
                &Principal::parse(FOLLOWER).unwrap(),
                "member",
                "confirmed",
            )
            .await
            .unwrap();
    });
    let (_data, warnings) = rig.reconcile();
    assert!(
        warnings.is_empty(),
        "re-adding restores access: {warnings:?}"
    );
    assert!(
        rig.placement_exists("s_deploy"),
        "the placement is intact through the round-trip"
    );
}
