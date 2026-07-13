//! VERB-RESHAPE acceptance e2e — the reshaped verb surface end to end over loopback HTTP, on the
//! GENUINE client verbs (`topos::test_support::FollowHarness` driving the real `ureq` transports)
//! against the GENUINE plane, via the shared `common` harness. One scenario per test:
//!
//!  1. fresh machine → paste the workspace address → enroll → `everyone`'s set lands (`follow --yes`);
//!  2. `follow <ws>/channels/<name>` → the channel's set lands (join + reconcile, one invocation);
//!  3. `unfollow <skill>` on device A stops delivery on device B too (person-scoped); bytes stay on
//!     both; the detach record is row-witnessed;
//!  4. `remove` (a channel-delivered skill) on A → gone there, stays on B, does NOT return on A's
//!     next update; `follow` at A restores it (the exclusion lifts);
//!  5. the contribute loop: a member `publish -m` on a reviewed bundle downgrades to a proposal
//!     (message-first in the reviewer's bare `review`), a stale approve is CONFLICT, a re-propose
//!     supersedes the old proposal (row-witnessed), the approve lands, the author's next update
//!     narrates + acks the verdict notice, and a reject's `-m` reason rides into the notice;
//!  6. `publish -m` messages show in the route-backed log; a purge leaves a tombstone; a revert to
//!     the purged target is refused; an archive frees the base name (the successor facts);
//!  7. a multi-`--skill` follow resolves ALL-OR-NONE (one bad name ⇒ nothing joined);
//!  8. an anonymous GET on three different resource paths answers the byte-identical protocol card
//!     (no existence oracle); the JSON face carries `api_base_url` (the machine bootstrap);
//!  9. `invite` without SMTP answers the ADDRESS with `mailed: false` (the honest no-relay flag) and
//!     seats the invitee, who joins via the follow flow (the roster is the lock);
//! 10. `protect`: tighten as a reviewer works, loosen as a reviewer is refused naming the owner, and
//!     the describe carries the audience (reach);
//! 11. ONE login session mints credentials for TWO workspaces; `auth logout` keeps the bytes;
//!     `auth status` reports the causes;
//! 12. the hook posture: a removed member's `update --quiet` is exit-0 with the ONE freeze line, and
//!     an unreachable plane past the staleness window warns "last synced <age> ago".
//!
//! Row-level witnesses ride `plane.pool` where the wire deliberately does not disclose (detachments,
//! exclusions, supersede closures, notices ack state).
//!
//! Skill ids here are SLUG-CLEAN (`s-deploy`, not `s_deploy`) so the catalog NAME the plane mints from
//! the published display name equals the id — one spelling across ids, names, and placement dirs.

mod common;

use common::{NOW, Plane, WS, WS_NAME, expected, expected_placement, genesis_files, ws_address};
use plane_store::{
    Authority, BundleId, CandidateUpload, CommitId, DeploymentMode, GovernanceOp,
    GovernanceOutcome, GovernanceRequest, LifecycleOutcome, OpId, Principal, ProtectKind,
    ProtectLevel, PurgeOutcome, UploadedFile, WorkspaceId,
};
use topos::test_support::{FollowHarness, PublishResult};
use topos_types::requests::WireProtocolCard;
use topos_types::results::PullAction;
use topos_types::{Generation, TerminalOutcome};

// ── shared constants ──────────────────────────────────────────────────────────────────────────────

/// The everyone-delivered skill (slug-clean: id == catalog name == placement dirname).
const SKILL: &str = "s-deploy";
const OWNER: &str = "owner@acme.test";
const OWNER_DKID: &str = "dk_owner";
const OWNER_PUBKEY: [u8; 32] = [9u8; 32];
const OWNER_CRED: &str = "wc_owner_secret";
const ALICE: &str = "alice@acme.test";
const REVIEWER: &str = "rev@acme.test";
const AT: &str = "2026-07-11T00:00:00Z";

/// The REAL wall clock (epoch ms) — session ops compared against wire-stamped time ride it.
fn wall_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the wall clock is past the epoch")
            .as_millis(),
    )
    .expect("epoch millis fit i64")
}

/// A canonical UUID op id seeded by `n` (op ids are plane-unique per test database).
fn op(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}

// ── shared seeding (per-test closures compose these) ────────────────────────────────────────────────

/// The workspace + a confirmed OWNER holding [`OWNER_CRED`] (the seed every scenario starts from).
async fn seed_owner_ws(a: &Authority) {
    let ws = WorkspaceId::parse(WS).unwrap();
    a.seed_workspace(&ws, "Acme", "verified", "cloud")
        .await
        .expect("seed workspace");
    common::seed_member(
        a,
        &ws,
        OWNER_DKID,
        &OWNER_PUBKEY,
        OWNER,
        "owner",
        OWNER_CRED,
    )
    .await;
}

/// Seat `email` as an INVITED member at `role` (the roster row an invitation writes; the redeem
/// flips it to confirmed).
async fn seat_invited(a: &Authority, email: &str, role: &str) {
    let ws = WorkspaceId::parse(WS).unwrap();
    a.seed_workspace_member(&ws, &Principal::parse(email).unwrap(), role, "invited")
        .await
        .expect("seat invited member");
}

/// Genesis-publish `skill_id` through the REAL device-lane publish (display name = the id, so the
/// minted catalog name equals it) into `everyone` — or into `channel` (the `publish --to` placement:
/// the skill is then delivered ONLY via that channel). Returns the genesis commit id.
async fn genesis(
    a: &Authority,
    skill_id: &str,
    op_id: &str,
    files: Vec<UploadedFile>,
    channel: Option<&str>,
) -> CommitId {
    let auth = plane_store::DeviceOpAuth {
        credential: OWNER_CRED.to_owned(),
        op: plane_store::DeviceOp::PublishDirect,
        expected: Generation { epoch: 0, seq: 0 },
    };
    let receipt = a
        .publish(
            &WorkspaceId::parse(WS).unwrap(),
            &BundleId::parse(skill_id).unwrap(),
            &OpId::parse(op_id).unwrap(),
            CandidateUpload {
                files,
                parents: vec![],
                author: "d_seed".to_owned(),
                message: "topos publish".to_owned(),
            },
            auth,
            Some(skill_id),
            channel,
            AT,
            NOW,
        )
        .await
        .expect("genesis publish");
    assert_eq!(receipt.outcome, TerminalOutcome::Ok, "genesis lands");
    receipt.version_id.expect("genesis version id")
}

/// Enroll a fresh rig BY ADDRESS as `email` and land the entitled set (`--yes`): the whole scenario-1
/// motion, reused as the setup step of most scenarios.
fn join_and_land(plane: &Plane, tag: &str, email: &str) -> FollowHarness {
    let client = FollowHarness::new(tag);
    common::begin_address_enroll(plane, &client, &ws_address(&plane.link_base_url), email);
    let applied = client.resume_apply().expect("resume enrolls + applies");
    assert!(applied.enrolled_now, "THIS invocation enrolled the device");
    client
}

// ── row-level witnesses ─────────────────────────────────────────────────────────────────────────────

/// Whether a `SELECT 1` row exists for `sql` with string binds.
fn row_exists(plane: &Plane, sql: &str, binds: &[&str]) -> bool {
    plane.rt.block_on(async {
        let mut q = sqlx::query(sql);
        for b in binds {
            q = q.bind(*b);
        }
        q.fetch_optional(&plane.pool)
            .await
            .expect("witness query")
            .is_some()
    })
}

/// A COUNT(*) witness with string binds.
fn row_count(plane: &Plane, sql: &str, binds: &[&str]) -> i64 {
    plane.rt.block_on(async {
        let mut q = sqlx::query_scalar::<_, i64>(sql);
        for b in binds {
            q = q.bind(*b);
        }
        q.fetch_one(&plane.pool).await.expect("count query")
    })
}

// ═════════════════════════════════════════════════════════════════════════════════════════════════
// 1 · fresh machine → paste the address → enroll → everyone lands
// ═════════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn s01_fresh_machine_pastes_the_address_and_everyone_lands() {
    let plane = common::start_stack("topos-verbs", "s01", true, async |a: &Authority| {
        seed_owner_ws(a).await;
        genesis(a, SKILL, &op(1), genesis_files(), None).await;
        seat_invited(a, ALICE, "member").await;
        common::Seeded::default()
    });
    let client = join_and_land(&plane, "vr-s01", ALICE);
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&genesis_files()),
        "the everyone genesis lands byte-exact off one pasted address"
    );
}

// ═════════════════════════════════════════════════════════════════════════════════════════════════
// 2 · follow <ws>/channels/<name> — the channel-qualified address joins + lands the set
// ═════════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn s02_follow_a_channel_qualified_address_joins_and_lands_the_set() {
    const OPS_SKILL: &str = "s-ops";
    let plane = common::start_stack("topos-verbs", "s02", true, async |a: &Authority| {
        seed_owner_ws(a).await;
        genesis(a, SKILL, &op(1), genesis_files(), None).await;
        // The channel-only skill: `publish --to ops` places it in #ops (creating the channel on
        // first place), NOT in everyone — so receiving it requires the join.
        genesis(
            a,
            OPS_SKILL,
            &op(2),
            vec![UploadedFile {
                path: "SKILL.md".to_owned(),
                mode: plane_store::FileMode::Regular,
                bytes: b"# ops\nOps runbook.\n".to_vec(),
            }],
            Some("ops"),
        )
        .await;
        seat_invited(a, ALICE, "member").await;
        common::Seeded::default()
    });

    // The fresh machine pastes the CHANNEL-qualified address: enroll + join + land in one flow.
    let client = FollowHarness::new("vr-s02");
    let address = format!("{}/{WS_NAME}/channels/ops", plane.link_base_url);
    common::begin_address_enroll(&plane, &client, &address, ALICE);
    let applied = client.resume_apply().expect("resume enrolls + applies");
    assert!(applied.enrolled_now);
    assert!(
        applied
            .subscribed
            .contains(&("channel".to_owned(), "ops".to_owned())),
        "the apply joined #ops: {:?}",
        applied.subscribed
    );
    // The reconcile landed the WHOLE entitled set: the channel's skill AND the everyone genesis.
    let installed: Vec<&str> = applied
        .installed
        .iter()
        .map(|i| i.skill_id.as_str())
        .collect();
    assert!(
        installed.contains(&OPS_SKILL),
        "the channel set landed: {installed:?}"
    );
    assert!(
        installed.contains(&SKILL),
        "everyone still delivers: {installed:?}"
    );
    assert_eq!(
        client.placement_files(OPS_SKILL),
        expected(&[("SKILL.md", false, b"# ops\nOps runbook.\n")]),
        "the channel skill lands byte-exact"
    );
    // The membership row is the join's server-side witness.
    assert!(
        row_exists(
            &plane,
            "SELECT 1 FROM channel_members cm JOIN channels c \
             ON c.workspace_id = cm.workspace_id AND c.channel_id = cm.channel_id \
             WHERE cm.workspace_id = $1 AND c.name = $2 AND cm.principal = $3",
            &[WS, "ops", ALICE],
        ),
        "the channel_members row exists"
    );
}

// ═════════════════════════════════════════════════════════════════════════════════════════════════
// 3 · unfollow is PERSON-scoped: device A's unfollow stops device B too; bytes stay everywhere
// ═════════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn s03_unfollow_is_person_scoped_and_freezes_bytes_everywhere() {
    let plane = common::start_stack("topos-verbs", "s03", true, async |a: &Authority| {
        seed_owner_ws(a).await;
        genesis(a, SKILL, &op(1), genesis_files(), None).await;
        seat_invited(a, ALICE, "member").await;
        common::Seeded::default()
    });
    // ONE person, TWO devices — each a full real enrollment (the second device redeems onto the
    // already-confirmed seat).
    let device_a = join_and_land(&plane, "vr-s03-a", ALICE);
    let device_b = join_and_land(&plane, "vr-s03-b", ALICE);
    assert_eq!(
        device_b.placement_files(SKILL),
        expected_placement(&genesis_files())
    );

    // Device A unfollows — a PERSON-scoped detach (the describe's all-devices disclosure is the law).
    let applied = device_a.unfollow_apply(SKILL).expect("unfollow --yes");
    assert_eq!(
        applied["bytes_kept"],
        serde_json::json!(true),
        "an unfollow never touches a byte"
    );
    assert!(
        applied["items"][0]["stops"]
            .as_array()
            .is_some_and(|s| s.iter().any(|v| v == SKILL)),
        "the item names what STOPS: {applied}"
    );
    // The final detach record — person-scoped, event-exact (who acted, and how).
    assert!(
        row_exists(
            &plane,
            "SELECT 1 FROM skill_detachments \
             WHERE workspace_id = $1 AND principal = $2 AND skill_id = $3 AND cause = 'unfollow'",
            &[WS, ALICE, SKILL],
        ),
        "the skill_detachments row records the unfollow"
    );

    // Device B's next update FREEZES the skill in place (Detached) — delivery stopped for the PERSON.
    let (data, _) = device_b.reconcile(false);
    let row = data
        .skills
        .iter()
        .find(|r| r.skill == SKILL)
        .expect("the detached skill is narrated, never silently dropped");
    assert_eq!(
        row.action,
        PullAction::Detached,
        "person-scoped: B detaches too"
    );
    assert_eq!(
        device_b.placement_files(SKILL),
        expected_placement(&genesis_files()),
        "B's bytes stay frozen in place"
    );
    assert_eq!(
        device_a.placement_files(SKILL),
        expected_placement(&genesis_files()),
        "A's bytes stay too — an unfollow is not a remove"
    );
}

// ═════════════════════════════════════════════════════════════════════════════════════════════════
// 4 · remove excludes ONE device; the copy stays elsewhere; follow lifts the exclusion
// ═════════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn s04_remove_excludes_one_device_and_follow_restores_it() {
    let plane = common::start_stack("topos-verbs", "s04", true, async |a: &Authority| {
        seed_owner_ws(a).await;
        genesis(a, SKILL, &op(1), genesis_files(), None).await;
        seat_invited(a, ALICE, "member").await;
        common::Seeded::default()
    });
    let device_a = join_and_land(&plane, "vr-s04-a", ALICE);
    let device_b = join_and_land(&plane, "vr-s04-b", ALICE);
    let dkid_a = device_a.device_key_id();

    // A removes the (channel-delivered — `everyone` IS a channel) skill from THIS device.
    let removed = device_a.remove_apply(SKILL).expect("remove --yes");
    assert!(removed.applied);
    assert!(removed.items[0].bytes_kept, "the sidecar keeps every byte");
    assert_eq!(removed.items[0].workspace_id.as_deref(), Some(WS));
    assert!(
        device_a.placement_files(SKILL).is_empty(),
        "the agent dir is cleaned on A"
    );
    assert!(
        row_exists(
            &plane,
            "SELECT 1 FROM device_exclusions \
             WHERE workspace_id = $1 AND device_key_id = $2 AND skill_id = $3",
            &[WS, &dkid_a, SKILL],
        ),
        "the per-device exclusion row exists"
    );

    // It does NOT come back on A's next update — and B is untouched.
    let (_data, warnings) = device_a.reconcile(false);
    assert!(warnings.is_empty(), "a clean sweep: {warnings:?}");
    assert!(
        device_a.placement_files(SKILL).is_empty(),
        "the excluded skill does not return on A's next update"
    );
    let (_data, _) = device_b.reconcile(false);
    assert_eq!(
        device_b.placement_files(SKILL),
        expected_placement(&genesis_files()),
        "B (the person's other device) keeps receiving"
    );

    // `follow` at A lifts the exclusion: the row dies, and the batch-accepting apply re-lands the
    // BYTES on A in the same invocation (remove reset the sync state to the never-received
    // baseline, so the re-delivery is a fresh first-receive — not an "already current" no-op over
    // an empty dir).
    let _applied = device_a
        .follow_apply_skills(&[SKILL])
        .expect("follow --skill --yes");
    assert!(
        !row_exists(
            &plane,
            "SELECT 1 FROM device_exclusions \
             WHERE workspace_id = $1 AND device_key_id = $2 AND skill_id = $3",
            &[WS, &dkid_a, SKILL],
        ),
        "the exclusion row is lifted"
    );
    assert_eq!(
        device_a.placement_files(SKILL),
        expected_placement(&genesis_files()),
        "follow at A restores the bytes"
    );
    let (data, _) = device_a.reconcile(false);
    let row = data
        .skills
        .iter()
        .find(|r| r.skill == SKILL)
        .expect("delivered again");
    assert!(
        !matches!(row.action, PullAction::Excluded | PullAction::Detached),
        "the skill is back in A's delivered set (not excluded/frozen): {:?}",
        row.action
    );
}

// ═════════════════════════════════════════════════════════════════════════════════════════════════
// 5 · the contribute loop: downgrade → message-first inbox → stale CONFLICT → supersede → verdicts
// ═════════════════════════════════════════════════════════════════════════════════════════════════

#[test]
#[allow(clippy::too_many_lines)]
fn s05_contribute_loop_downgrade_conflict_supersede_and_verdict_notices() {
    let plane = common::start_stack("topos-verbs", "s05", true, async |a: &Authority| {
        seed_owner_ws(a).await;
        let g = genesis(a, SKILL, &op(1), genesis_files(), None).await;
        // Per-bundle protection: reviewed (the owner tightens — the reviewer-level gate admits it).
        let outcome = a
            .protect(
                &WorkspaceId::parse(WS).unwrap(),
                OWNER_CRED,
                ProtectKind::Skill,
                SKILL,
                ProtectLevel::Protected,
                AT,
            )
            .await
            .expect("tighten to reviewed");
        assert!(
            matches!(outcome, plane_store::ProtectOutcome::Set),
            "the seed tighten lands: {outcome:?}"
        );
        seat_invited(a, ALICE, "member").await;
        seat_invited(a, REVIEWER, "reviewer").await;
        common::Seeded {
            genesis: Some(g),
            invites: Vec::new(),
        }
    });
    let author = join_and_land(&plane, "vr-s05-author", ALICE);
    let reviewer = join_and_land(&plane, "vr-s05-reviewer", REVIEWER);

    // The author drafts + publishes with -m. The protection gate REROUTES the member's direct
    // publish into a proposal (NEEDS_REVIEW + downgraded — never a refusal).
    author.edit_placement(
        SKILL,
        &[
            (
                "SKILL.md",
                false,
                b"# deploy\nDeploy the service, sharper.\n",
            ),
            ("run.sh", true, b"#!/bin/sh\necho deploying\n"),
        ],
    );
    let d1 = author.draft_digest(SKILL);
    let published = author
        .publish_message(&plane.base_url, &format!("{SKILL}@{d1}"), "sharper deploy")
        .expect("the member publish runs");
    let PublishResult::Proposed(p1) = published else {
        panic!("a member publish on a reviewed bundle downgrades to a proposal, got {published:?}");
    };
    let hash1 = p1
        .proposal
        .split_once('@')
        .expect("skill@hash")
        .1
        .to_owned();

    // The reviewer's bare `review` leads with the author's message.
    let inbox = reviewer.review_inbox().expect("the review inbox");
    assert_eq!(
        inbox.inbox.len(),
        1,
        "one proposal awaits: {:?}",
        inbox.inbox
    );
    assert_eq!(inbox.inbox[0].message, "sharper deploy", "message-first");
    assert_eq!(inbox.inbox[0].proposer, ALICE);
    // The AUTHOR's bare `review` files the same proposal under the OUTBOX (their own).
    let author_view = author.review_inbox().expect("the author's review view");
    assert_eq!(
        author_view.outbox.len(),
        1,
        "the author sees their own proposal as outbox"
    );

    // The base MOVES (the owner ships a non-overlapping v2), so the approve is a stale CONFLICT.
    plane.rt.block_on(async {
        let receipt = plane
            .authority
            .seed_published_child(
                &plane.ws(),
                &BundleId::parse(SKILL).unwrap(),
                OWNER_CRED,
                &OpId::parse(&op(2)).unwrap(),
                plane.genesis(),
                vec![
                    UploadedFile {
                        path: "SKILL.md".to_owned(),
                        mode: plane_store::FileMode::Regular,
                        bytes: b"# deploy\nDeploy the service.\n".to_vec(),
                    },
                    UploadedFile {
                        path: "run.sh".to_owned(),
                        mode: plane_store::FileMode::Executable,
                        bytes: b"#!/bin/sh\necho deploying\n".to_vec(),
                    },
                    UploadedFile {
                        path: "OWNER.md".to_owned(),
                        mode: plane_store::FileMode::Regular,
                        bytes: b"# owner notes\n".to_vec(),
                    },
                ],
                "d_seed",
                "owner v2",
                AT,
                NOW,
            )
            .await
            .expect("the owner's direct publish (owner role passes the reviewed gate)");
        assert_eq!(receipt.outcome, TerminalOutcome::Ok);
    });
    let stale = reviewer.review_approve(&format!("{SKILL}@{hash1}"));
    let err = stale.expect_err("an approve after the base moved must be refused");
    // The refusal's client surface is the UNIFORM not-served answer: a stale candidate deliberately
    // leaves the read surface (the shared `open ∧ base == current` predicate), so the reviewer cannot
    // even fetch bytes to bind a verdict over — nothing moves. (The CAS's own CONFLICT for a race that
    // slips past the read is plane-store's in-crate proof; the read-first client never reaches it.)
    assert!(
        err.contains("not served here"),
        "the stale approve is refused at the read surface: {err}"
    );
    // …and the reviewer's inbox now DISCLOSES the staleness (the typed flag a re-propose clears).
    let inbox = reviewer
        .review_inbox()
        .expect("the inbox after the base moved");
    assert!(
        inbox.inbox[0].stale,
        "the stale flag is raised: {:?}",
        inbox.inbox
    );

    // The author's next update MERGES the draft onto the moved base (non-overlapping ⇒ clean), and
    // the RE-PROPOSE supersedes the stale proposal — closed with resolved_reason 'superseded'.
    let (data, _) = author.reconcile(false);
    let row = data
        .skills
        .iter()
        .find(|r| r.skill == SKILL)
        .expect("the skill row");
    assert_eq!(
        row.action,
        PullAction::Merged,
        "the draft merges cleanly onto the moved base"
    );
    let d2 = author.draft_digest(SKILL);
    let PublishResult::Proposed(p2) = author
        .publish_message(
            &plane.base_url,
            &format!("{SKILL}@{d2}"),
            "sharper deploy, rebased",
        )
        .expect("the re-propose runs")
    else {
        panic!("the re-propose downgrades to a proposal again");
    };
    let hash2 = p2
        .proposal
        .split_once('@')
        .expect("skill@hash")
        .1
        .to_owned();
    assert_ne!(hash1, hash2, "a fresh candidate");
    assert!(
        row_exists(
            &plane,
            "SELECT 1 FROM proposals \
             WHERE workspace_id = $1 AND skill_id = $2 AND commit_id = decode($3, 'hex') \
               AND status <> 'open' AND resolved_reason = 'superseded'",
            &[WS, SKILL, &hash1],
        ),
        "the author's earlier open proposal is CLOSED superseded (row witness)"
    );

    // The approve lands; the author's next update NARRATES the verdict notice and ACKS it.
    let approved = reviewer
        .review_approve(&format!("{SKILL}@{hash2}"))
        .expect("the fresh proposal approves");
    assert!(
        approved.current_generation.is_some(),
        "the approve moved current"
    );
    let (data, _) = author.reconcile(true);
    let verdict = data
        .notices
        .iter()
        .find(|n| n.kind == "verdict" && n.version_id.as_deref() == Some(hash2.as_str()))
        .expect("the approve verdict notice is narrated");
    assert_eq!(
        verdict.outcome.as_deref(),
        Some("accepted"),
        "the verdict names the outcome: {verdict:?}"
    );
    assert_eq!(
        row_count(
            &plane,
            "SELECT COUNT(*) FROM notices \
             WHERE workspace_id = $1 AND principal = $2 AND acked_at IS NULL",
            &[WS, ALICE],
        ),
        0,
        "the interactive update ACKED exactly what it narrated"
    );

    // The reject path: a third proposal, refused with -m — the reason rides into the notice.
    author.edit_placement(
        SKILL,
        &[
            (
                "SKILL.md",
                false,
                b"# deploy\nDeploy the service, sharper still.\n",
            ),
            ("run.sh", true, b"#!/bin/sh\necho deploying\n"),
            ("OWNER.md", false, b"# owner notes\n"),
        ],
    );
    let d3 = author.draft_digest(SKILL);
    let PublishResult::Proposed(p3) = author
        .publish_message(&plane.base_url, &format!("{SKILL}@{d3}"), "sharper still")
        .expect("the third propose runs")
    else {
        panic!("the third publish downgrades to a proposal");
    };
    let hash3 = p3
        .proposal
        .split_once('@')
        .expect("skill@hash")
        .1
        .to_owned();
    reviewer
        .review_reject(&format!("{SKILL}@{hash3}"), "needs a rollback plan")
        .expect("the reject (with the required -m reason) runs");
    let (data, _) = author.reconcile(true);
    let verdict = data
        .notices
        .iter()
        .find(|n| n.kind == "verdict" && n.version_id.as_deref() == Some(hash3.as_str()))
        .expect("the reject verdict notice is narrated");
    assert_eq!(
        verdict.outcome.as_deref(),
        Some("rejected"),
        "the verdict names the outcome: {verdict:?}"
    );
    assert_eq!(
        verdict.reason.as_deref(),
        Some("needs a rollback plan"),
        "the -m reason rides into the author's notice"
    );
}

// ═════════════════════════════════════════════════════════════════════════════════════════════════
// 6 · log carries -m messages; purge tombstones; revert-to-purged refused; archive frees the name
// ═════════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn s06_log_messages_purge_tombstone_revert_refusal_and_archive_facts() {
    const LOG_SKILL: &str = "s-log";
    let plane = common::start_stack("topos-verbs", "s06", true, async |a: &Authority| {
        seed_owner_ws(a).await;
        seat_invited(a, OWNER, "owner").await; // the owner's own address join (never demoted)
        common::Seeded::default()
    });
    // The owner enrolls by address and genesis-publishes a fresh skill with -m.
    let owner = FollowHarness::new("vr-s06-owner");
    common::begin_address_enroll(&plane, &owner, &ws_address(&plane.link_base_url), OWNER);
    let _ = owner.resume_describe().expect("owner enrolls");
    owner.adopt(LOG_SKILL, &[("SKILL.md", false, b"# log\nv1\n")]);
    // The follow entry for the AUTHORED skill (revert is follow-scoped: you revert what you follow).
    owner.follow_locally(LOG_SKILL, WS);
    let d1 = owner.draft_digest(LOG_SKILL);
    let PublishResult::Published(v1) = owner
        .publish_message(
            &plane.base_url,
            &format!("{LOG_SKILL}@{d1}"),
            "add deploy script",
        )
        .expect("the genesis publish")
    else {
        panic!("an owner genesis publishes direct");
    };
    let v1_hex = v1.version_id.expect("v1 id");
    owner.edit_placement(LOG_SKILL, &[("SKILL.md", false, b"# log\nv2 faster\n")]);
    let d2 = owner.draft_digest(LOG_SKILL);
    let PublishResult::Published(_v2) = owner
        .publish_message(
            &plane.base_url,
            &format!("{LOG_SKILL}@{d2}"),
            "faster deploy",
        )
        .expect("the v2 publish")
    else {
        panic!("the owner's v2 publishes direct");
    };

    // The route-backed log carries BOTH -m messages (the same wire read the `log` verb merges).
    let log = owner.skill_log_wire(WS, LOG_SKILL).expect("the skill log");
    let messages: Vec<&str> = log
        .versions
        .iter()
        .filter_map(|v| v.message.as_deref())
        .collect();
    assert!(
        messages.contains(&"add deploy script"),
        "v1's -m shows: {messages:?}"
    );
    assert!(
        messages.contains(&"faster deploy"),
        "v2's -m shows: {messages:?}"
    );

    // A purge leaves the tombstone (who, when) — the hash stays in history, the bytes go.
    let v1_commit = CommitId(
        hex::decode(&v1_hex)
            .expect("hex")
            .try_into()
            .expect("32 bytes"),
    );
    let purged = plane
        .rt
        .block_on(plane.authority.purge_version_session(
            &plane.ws(),
            OWNER,
            LOG_SKILL,
            v1_commit,
            DeploymentMode::Cloud,
            AT,
            wall_ms(),
        ))
        .expect("the purge op runs");
    assert!(
        matches!(purged, PurgeOutcome::Purged),
        "v1 purges: {purged:?}"
    );
    let log = owner
        .skill_log_wire(WS, LOG_SKILL)
        .expect("the post-purge log");
    let tombstone = log
        .versions
        .iter()
        .find(|v| v.version_id == v1_hex)
        .expect("the purged version stays listed");
    assert!(tombstone.purged_at.is_some(), "the tombstone names when");
    assert!(tombstone.purged_by.is_some(), "the tombstone names who");

    // A revert TO the purged target is refused — the pointer never moves. The client surface is the
    // UNIFORM not-served answer: the purged bytes are GONE, so the pre-post byte fetch (the tree
    // re-derivation a revert binds) dies at the read surface. (The server's own purge-naming typed
    // refusal on the POST path is plane-store's in-crate proof; the read-first client never reaches it.)
    let refused = owner
        .revert(LOG_SKILL, &v1_hex, true)
        .expect_err("a revert to a purged version must be refused");
    assert!(
        refused.contains("not served here"),
        "the revert is refused at the read surface: {refused}"
    );
    let log = owner
        .skill_log_wire(WS, LOG_SKILL)
        .expect("the post-refusal log");
    let current: Vec<&str> = log
        .versions
        .iter()
        .filter(|v| v.current)
        .map(|v| v.version_id.as_str())
        .collect();
    assert!(
        !current.contains(&v1_hex.as_str()),
        "the pointer never moved to the purged target: {current:?}"
    );

    // An archive renames-and-frees the base name; the log's facts carry the successor identity.
    let archived = plane
        .rt
        .block_on(plane.authority.archive_skill_session(
            &plane.ws(),
            OWNER,
            LOG_SKILL,
            DeploymentMode::Cloud,
            AT,
            wall_ms(),
        ))
        .expect("the archive op runs");
    let LifecycleOutcome::Archived { archived_name } = archived else {
        panic!("expected Archived, got {archived:?}");
    };
    let log = owner
        .skill_log_wire(WS, LOG_SKILL)
        .expect("the post-archive log");
    assert_eq!(
        log.base_name.as_deref(),
        Some(LOG_SKILL),
        "the freed base name is recorded (the archived-successor hint's source)"
    );
    assert_eq!(
        log.name, archived_name,
        "the log answers under the archived name"
    );
    assert_eq!(log.status, "archived");
}

// ═════════════════════════════════════════════════════════════════════════════════════════════════
// 7 · multi-flag follow: resolve ALL-OR-NONE — one bad name, nothing joined
// ═════════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn s07_multi_flag_follow_resolves_all_or_applies_none() {
    const ALPHA: &str = "s-alpha";
    const BETA: &str = "s-beta";
    let plane = common::start_stack("topos-verbs", "s07", true, async |a: &Authority| {
        seed_owner_ws(a).await;
        genesis(a, ALPHA, &op(1), genesis_files(), None).await;
        genesis(
            a,
            BETA,
            &op(2),
            vec![UploadedFile {
                path: "SKILL.md".to_owned(),
                mode: plane_store::FileMode::Regular,
                bytes: b"# beta\n".to_vec(),
            }],
            None,
        )
        .await;
        seat_invited(a, ALICE, "member").await;
        common::Seeded::default()
    });
    // Enroll WITHOUT applying (describe only) — the subscription state starts empty.
    let client = FollowHarness::new("vr-s07");
    common::begin_address_enroll(&plane, &client, &ws_address(&plane.link_base_url), ALICE);
    let _ = client.resume_describe().expect("enroll (describe only)");

    // One bad name in the --skill set refuses the WHOLE batch — nothing joined, nothing installed.
    let refused = client
        .follow_apply_skills(&[ALPHA, "no-such-skill"])
        .expect_err("one unresolvable selector refuses the whole invocation");
    assert!(
        refused.contains("not found") || refused.contains("no-such-skill"),
        "the uniform not-found: {refused}"
    );
    assert_eq!(
        row_count(
            &plane,
            "SELECT COUNT(*) FROM skill_follows WHERE workspace_id = $1 AND principal = $2",
            &[WS, ALICE],
        ),
        0,
        "resolve-all-or-apply-none: NO follow row landed"
    );

    // The positive control: the same batch with both names real applies BOTH.
    let applied = client
        .follow_apply_skills(&[ALPHA, BETA])
        .expect("the clean batch applies");
    assert_eq!(
        applied.subscribed.len(),
        2,
        "both direct follows: {:?}",
        applied.subscribed
    );
    assert_eq!(
        row_count(
            &plane,
            "SELECT COUNT(*) FROM skill_follows WHERE workspace_id = $1 AND principal = $2",
            &[WS, ALICE],
        ),
        2,
        "both follow rows landed"
    );
}

// ═════════════════════════════════════════════════════════════════════════════════════════════════
// 8 · the protocol card: byte-identical on every path; the JSON face carries the API base
// ═════════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn s08_the_protocol_card_is_identical_on_every_path() {
    let plane = common::start_stack("topos-verbs", "s08", true, async |a: &Authority| {
        seed_owner_ws(a).await;
        common::Seeded::default()
    });
    // Four DIFFERENT resource paths — the ORIGIN ROOT (what the token-less doors card-fetch), a real
    // workspace, a real-shaped channel path, and pure noise — answer the byte-identical markdown
    // card: an unmatched GET is never an existence oracle, and the root is an address like any other.
    let paths = [
        format!("{}/", plane.link_base_url),
        format!("{}/{WS_NAME}", plane.link_base_url),
        format!("{}/{WS_NAME}/channels/ops", plane.link_base_url),
        format!("{}/totally/made/up", plane.base_url),
    ];
    let bodies: Vec<String> = paths.iter().map(|p| http_get_body(p, "*/*")).collect();
    assert_eq!(bodies[0], bodies[1], "the origin root serves the same card");
    assert_eq!(bodies[1], bodies[2], "the card echoes no path");
    assert_eq!(bodies[2], bodies[3], "noise answers the same card");
    assert!(
        bodies[0].contains("topos follow"),
        "the agent hand-off line: {}",
        bodies[0]
    );

    // The machine bootstrap: the JSON face carries the API base a client re-roots onto.
    let card: WireProtocolCard =
        serde_json::from_str(http_get_body(&paths[2], "application/json").trim())
            .expect("a WireProtocolCard");
    assert_eq!(card.card, "topos-protocol-card");
    assert_eq!(card.api_base_url, plane.base_url);
}

// ═════════════════════════════════════════════════════════════════════════════════════════════════
// 9 · invite without SMTP: the honest mailed:false + the address; the invitee joins by it
// ═════════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn s09_invite_without_smtp_prints_the_address_and_the_invitee_joins() {
    const NEWBIE: &str = "newbie@acme.test";
    let plane = common::start_stack("topos-verbs", "s09", true, async |a: &Authority| {
        seed_owner_ws(a).await;
        genesis(a, SKILL, &op(1), genesis_files(), None).await;
        seat_invited(a, OWNER, "owner").await;
        common::Seeded::default()
    });
    let owner = FollowHarness::new("vr-s09-owner");
    common::begin_address_enroll(&plane, &owner, &ws_address(&plane.link_base_url), OWNER);
    let _ = owner.resume_describe().expect("owner enrolls");

    // The invitation: a roster write + the address to paste. This plane has NO SMTP relay (the
    // silent NoopMailer), so the server truthfully reports mailed:false — the inviter pastes the
    // address by hand. (The mailed:true half needs a relay; the plane's own in-crate mailer tests
    // cover it — the capturing test mailer is crate-private to the plane by design.)
    let (address, invited, mailed) = owner.invite_full(&[NEWBIE], &[]).expect("invite --yes");
    assert!(!mailed, "no relay ⇒ the honest mailed:false");
    assert_eq!(invited, vec![NEWBIE.to_owned()]);
    assert!(
        address.ends_with(&format!("/{WS_NAME}")),
        "the paste-able address roots on the workspace name: {address}"
    );
    assert!(
        row_exists(
            &plane,
            "SELECT 1 FROM workspace_member \
             WHERE workspace_id = $1 AND principal = $2 AND status = 'invited'",
            &[WS, NEWBIE],
        ),
        "the invitee is seated invited"
    );

    // The invitee joins via the follow flow — the address carries nothing; the roster is the lock.
    let newbie = FollowHarness::new("vr-s09-newbie");
    common::begin_address_enroll(&plane, &newbie, &address, NEWBIE);
    let applied = newbie.resume_apply().expect("the invitee's join");
    assert!(applied.enrolled_now);
    assert_eq!(
        newbie.placement_files(SKILL),
        expected_placement(&genesis_files()),
        "the invitee lands everyone's set"
    );
}

// ═════════════════════════════════════════════════════════════════════════════════════════════════
// 10 · protect: tighten as reviewer works; loosen as reviewer refused naming owner; audience shown
// ═════════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn s10_protect_tighten_reviewer_loosen_owner_audience_in_describe() {
    let plane = common::start_stack("topos-verbs", "s10", true, async |a: &Authority| {
        seed_owner_ws(a).await;
        genesis(a, SKILL, &op(1), genesis_files(), None).await;
        seat_invited(a, REVIEWER, "reviewer").await;
        common::Seeded::default()
    });
    let reviewer = join_and_land(&plane, "vr-s10-reviewer", REVIEWER);

    // The bare describe: tighten-to-reviewed, nothing applied, the AUDIENCE disclosed (the reach —
    // the owner + the reviewer are the entitled persons here).
    let describe = reviewer
        .protect(SKILL, None, false)
        .expect("the protect describe");
    assert!(!describe.applied, "a bare protect changes nothing");
    assert_eq!(
        describe.level, "reviewed",
        "bare = tighten to the skill default"
    );
    assert_eq!(
        describe.audience,
        Some(2),
        "the describe carries the reach: {describe:?}"
    );

    // Tighten as a reviewer WORKS (tighten is reviewer-gated).
    let applied = reviewer
        .protect(SKILL, None, true)
        .expect("the reviewer tightens");
    assert!(applied.applied);
    assert!(
        row_exists(
            &plane,
            "SELECT 1 FROM catalog \
             WHERE workspace_id = $1 AND skill_id = $2 AND protection = 'reviewed'",
            &[WS, SKILL],
        ),
        "the protection row is set"
    );

    // Loosen as a reviewer is REFUSED, naming the role that can (owner).
    let refused = reviewer
        .protect(SKILL, Some("open"), true)
        .expect_err("loosening is the owner's act");
    assert!(
        refused.to_lowercase().contains("owner"),
        "the refusal names the owner: {refused}"
    );
    assert!(
        row_exists(
            &plane,
            "SELECT 1 FROM catalog \
             WHERE workspace_id = $1 AND skill_id = $2 AND protection = 'reviewed'",
            &[WS, SKILL],
        ),
        "the protection stands"
    );
}

// ═════════════════════════════════════════════════════════════════════════════════════════════════
// 11 · ONE login session, TWO workspaces; logout keeps bytes; status reports causes
// ═════════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn s11_one_login_session_mints_credentials_for_two_workspaces() {
    const WS_B: &str = "w_beta";
    let plane = common::start_stack("topos-verbs", "s11", true, async |a: &Authority| {
        seed_owner_ws(a).await;
        let g = genesis(a, SKILL, &op(1), genesis_files(), None).await;
        // The SECOND workspace on the same plane; alice holds a CONFIRMED seat in BOTH (login mints
        // per confirmed seat — no per-workspace enrollment needed).
        let ws_b = WorkspaceId::parse(WS_B).unwrap();
        a.seed_workspace(&ws_b, "Beta", "unverified", "cloud")
            .await
            .expect("seed the second workspace");
        for ws in [WorkspaceId::parse(WS).unwrap(), ws_b] {
            a.seed_workspace_member(
                &ws,
                &Principal::parse(ALICE).unwrap(),
                "member",
                "confirmed",
            )
            .await
            .expect("seat alice confirmed");
        }
        common::Seeded {
            genesis: Some(g),
            invites: Vec::new(),
        }
    });

    // ONE login session: begin (device flow, intent login) → the identity leg ONCE → resume redeems
    // at POST /v1/login, minting one credential per confirmed seat.
    let client = FollowHarness::new("vr-s11");
    let begin = client
        .auth_login(Some(&plane.base_url))
        .expect("login call 1");
    let user_code = begin["pending"]["user_code"]
        .as_str()
        .expect("the pending user code")
        .to_owned();
    plane
        .rt
        .block_on(
            plane
                .authority
                .confirm_external_identity(&user_code, ALICE, NOW),
        )
        .expect("the one identity leg");
    let done = client
        .auth_login(Some(&plane.base_url))
        .expect("login resume");
    let memberships = done["done"]["memberships"]
        .as_array()
        .expect("the memberships array");
    assert_eq!(
        memberships.len(),
        2,
        "one browser round, two workspaces: {memberships:?}"
    );
    assert!(
        memberships
            .iter()
            .all(|m| m["minted"] == serde_json::json!(true)),
        "a credential minted per confirmed seat: {memberships:?}"
    );
    assert_eq!(done["done"]["principal"], serde_json::json!(ALICE));
    assert_eq!(client.memberships().len(), 2, "user.json carries both");

    // The signed-in device syncs + accepts the first receive (bytes land), then logs out.
    let (_data, _) = client.reconcile(false);
    let target = format!("{SKILL}@{}", hex::encode(plane.genesis().0));
    client
        .approve(&plane.base_url, &[target])
        .expect("the first-receive accept");
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&genesis_files())
    );

    let status = client.auth_status().expect("auth status (signed in)");
    assert_eq!(status["signed_in"], serde_json::json!(true));

    // Logout: the credentials die; the BYTES (and follows, and the principal) stay.
    let out = client.auth_logout().expect("auth logout --yes");
    assert_eq!(out["credentials_deleted"], serde_json::json!(true));
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&genesis_files()),
        "logout keeps every byte"
    );
    let status = client.auth_status().expect("auth status (signed out)");
    assert_eq!(status["signed_in"], serde_json::json!(false));
    let causes: Vec<&str> = status["workspaces"]
        .as_array()
        .expect("workspace statuses")
        .iter()
        .filter_map(|w| w["health"].as_str())
        .collect();
    assert!(
        causes.iter().all(|c| c.contains("no credential")),
        "status reports the signed-out CAUSE per workspace: {causes:?}"
    );
}

// ═════════════════════════════════════════════════════════════════════════════════════════════════
// 12 · the hook posture: the one freeze line, and the staleness warning
// ═════════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn s12_hook_posture_freeze_line_and_staleness_warning() {
    // (a) A removed member's `update --quiet` exits 0 with the ONE freeze line; bytes stay.
    let plane = common::start_stack("topos-verbs", "s12a", true, async |a: &Authority| {
        seed_owner_ws(a).await;
        genesis(a, SKILL, &op(1), genesis_files(), None).await;
        seat_invited(a, ALICE, "member").await;
        common::Seeded::default()
    });
    let client = join_and_land(&plane, "vr-s12a", ALICE);
    let removed = plane
        .rt
        .block_on(plane.authority.roster_remove(
            &plane.ws(),
            &op(90),
            GovernanceRequest {
                credential: OWNER_CRED.to_owned(),
                op: GovernanceOp::RosterRemove {
                    target: Principal::parse(ALICE).unwrap(),
                },
            },
            AT,
            NOW,
        ))
        .expect("roster_remove runs");
    assert!(matches!(removed, GovernanceOutcome::Ok));

    let lines = client.quiet_update().expect("the quiet hook exits 0");
    assert_eq!(lines.len(), 1, "exactly ONE line: {lines:?}");
    assert!(
        lines[0].contains("no longer has access") && lines[0].contains("frozen"),
        "the freeze line: {}",
        lines[0]
    );
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&genesis_files()),
        "frozen in place — never a clean"
    );

    // (b) Unreachable past the staleness window: kill the listener, backdate the freshness doc —
    // the quiet hook warns "last synced <age> ago — server unreachable" and still exits 0.
    let plane_b = common::start_stack("topos-verbs", "s12b", true, async |a: &Authority| {
        seed_owner_ws(a).await;
        genesis(a, SKILL, &op(1), genesis_files(), None).await;
        seat_invited(a, ALICE, "member").await;
        common::Seeded::default()
    });
    let client_b = join_and_land(&plane_b, "vr-s12b", ALICE);
    client_b.backdate_sync_status(WS, wall_ms() - 3_600_000, 60_000);
    drop(plane_b); // the listener dies with the runtime — the next dial is connection-refused
    let lines = client_b
        .quiet_update()
        .expect("an unreachable plane is hook-soft (exit 0)");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("last synced") && l.contains("server unreachable")),
        "the staleness warning line: {lines:?}"
    );
}

// ── raw HTTP helper (the e2e crate carries no client library) ───────────────────────────────────────

/// GET `url` with an explicit `Accept` over a plain `TcpStream` and return the BODY (after the header
/// terminator).
fn http_get_body(url: &str, accept: &str) -> String {
    use std::io::{Read as _, Write as _};

    let rest = url.strip_prefix("http://").expect("a loopback http url");
    let (host, path) = rest.split_once('/').expect("a path");
    let mut stream = std::net::TcpStream::connect(host).expect("connect the host");
    write!(
        stream,
        "GET /{path} HTTP/1.1\r\nHost: {host}\r\nAccept: {accept}\r\nConnection: close\r\n\r\n"
    )
    .expect("send the request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read the response");
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .expect("a body follows the headers");
    // The web app (a node server) frames the card with `Transfer-Encoding: chunked`, so the raw
    // body carries chunk sizes (`<hex>\r\n<bytes>\r\n…0\r\n`) — strip them (the pre-cutover axum
    // plane sent Content-Length, which needed no decoding). Byte-identity across paths survives
    // either way (identical content ⇒ identical framing); a JSON parse needs the payload clean.
    if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let mut out = String::new();
        let mut rest = body;
        while let Some((size_line, tail)) = rest.split_once("\r\n") {
            let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
            if size == 0 {
                break;
            }
            out.push_str(&tail[..size.min(tail.len())]);
            rest = tail.get(size + 2..).unwrap_or("");
        }
        out
    } else {
        body.to_owned()
    }
}
