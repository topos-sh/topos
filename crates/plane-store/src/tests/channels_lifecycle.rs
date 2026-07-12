//! Channels — the skill LIFECYCLE (archive / unarchive / delete / purge) + the downgraded revert.
//!
//! These drive the owner-gated session lifecycle ops through `Authority` against a real Postgres + git
//! store, asserting on the DIRECTORY policy (rename-on-archive, name freeing, proposal auto-close +
//! author notices, the state machine) AND the CUSTODY half (delete un-roots + the GC reclaims; purge
//! un-roots ONE version and only its unique bytes drop out — read through the public read paths, never
//! raw git). Lifecycle is load-bearing because a wrong rename leaks a name (or strands one), and a
//! wrong un-root either keeps a leaked blob readable or reclaims a shared one still in use.

use super::*;

use crate::catalog::{LifecycleOutcome, PurgeOutcome};
use crate::channels::SubscriptionOutcome;
use crate::delivery::{DeliveredSkill, Delivery};

const ALICE: &str = "alice@acme.com";
const BOB: &str = "bob@acme.com";

/// Seat a person's device + workspace_member seat.
async fn seat(fx: &Fixture, w: &WorkspaceId, dkid: &str, seed: u8, principal: &str, role: &str) {
    let p = prin(principal);
    fx.authority
        .db()
        .seed_device(w, dkid, &dev_key(seed), &p, false, &cred(w, dkid))
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_workspace_member(w, &p, role, "confirmed")
        .await
        .unwrap();
}

/// Genesis-publish `skill` as `dkid` with `display_name`, into `everyone`.
async fn gpub(
    fx: &Fixture,
    w: &WorkspaceId,
    s: &SkillId,
    dkid: &str,
    op_id: &str,
    files: Vec<UploadedFile>,
    display_name: &str,
) -> crate::SetCurrentReceipt {
    let auth = DeviceOpAuth {
        credential: cred(w, dkid),
        op: DeviceOp::PublishDirect,
        expected: gn(0, 0),
    };
    fx.authority
        .publish(
            w,
            s,
            &op(op_id),
            genesis(files),
            auth,
            Some(display_name),
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap()
}

fn find<'a>(d: &'a Delivery, skill_id: &str) -> Option<&'a DeliveredSkill> {
    d.skills.iter().find(|s| s.skill_id == skill_id)
}

async fn deliver(fx: &Fixture, w: &WorkspaceId, dkid: &str) -> Delivery {
    fx.authority.delivery(w, &cred(w, dkid)).await.unwrap()
}

/// A skill's catalog `(status, name)` (raw `sqlx`, off the committed `.sqlx` drift surface).
async fn catalog(pool: &PgPool, w: &str, skill_id: &str) -> Option<(String, String)> {
    use sqlx::Row as _;
    sqlx::query("SELECT status, name FROM catalog WHERE workspace_id = $1 AND skill_id = $2")
        .bind(w)
        .bind(skill_id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .map(|r| (r.get::<String, _>("status"), r.get::<String, _>("name")))
}

/// A skill's OPEN/closed proposal `(status, resolved_reason)` for the seeded proposal id.
async fn proposal(pool: &PgPool, w: &str, id: &str) -> Option<(String, Option<String>)> {
    use sqlx::Row as _;
    sqlx::query("SELECT status, resolved_reason FROM proposals WHERE workspace_id = $1 AND id = $2")
        .bind(w)
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .map(|r| {
            (
                r.get::<String, _>("status"),
                r.get::<Option<String>, _>("resolved_reason"),
            )
        })
}

/// How many `channel_skills` rows reference a skill (0 ⇒ placed nowhere).
async fn placements(pool: &PgPool, w: &str, skill_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::int8 FROM channel_skills WHERE workspace_id = $1 AND skill_id = $2",
    )
    .bind(w)
    .bind(skill_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// `skill_commit.purged_at`/`purged_by` for a version (raw `sqlx`).
async fn purge_stamp(pool: &PgPool, w: &str, commit: &[u8]) -> (Option<i64>, Option<String>) {
    use sqlx::Row as _;
    let r = sqlx::query(
        "SELECT purged_at, purged_by FROM skill_commit WHERE workspace_id = $1 AND commit_id = $2",
    )
    .bind(w)
    .bind(commit)
    .fetch_one(pool)
    .await
    .unwrap();
    (
        r.get::<Option<i64>, _>("purged_at"),
        r.get::<Option<String>, _>("purged_by"),
    )
}

/// A fake candidate commit id (for seeding a proposal row against a real base).
fn fake_commit(tag: &[u8]) -> CommitId {
    CommitId(digest::sha256(tag))
}

/// A one-parent child publish (review off ⇒ lands) — the real pointer-move, since the `test-fixtures`
/// `seed_published_child` shim is not compiled in the in-crate test build.
async fn child_pub(
    fx: &Fixture,
    w: &WorkspaceId,
    s: &SkillId,
    dkid: &str,
    op_id: &str,
    parent: CommitId,
    files: Vec<UploadedFile>,
) -> crate::SetCurrentReceipt {
    let expected = fx
        .authority
        .db()
        .read_current_generation(w, s)
        .await
        .unwrap()
        .unwrap();
    let auth = DeviceOpAuth {
        credential: cred(w, dkid),
        op: DeviceOp::PublishDirect,
        expected,
    };
    fx.authority
        .publish(
            w,
            s,
            &op(op_id),
            child(parent, files),
            auth,
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap()
}

// ── archive ────────────────────────────────────────────────────────────────────────────────────────

/// The owner archive: it renames the catalog entry `<name>-archived-<date>` FREEING the base name (a
/// NEW genesis under the same folder name mints a fresh identity that lands), drops the old skill out
/// of delivery, and CLOSES an open proposal with an author notice — the full "out of circulation, not
/// out of history" move.
#[sqlx::test]
async fn owner_archive_renames_frees_the_name_closes_proposals_and_notifies(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chl-archive").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member").await;
    let g = gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        "Deploy",
    )
    .await;
    // An OPEN proposal from bob on the skill (closed by the archive with a notice to bob).
    fx.authority
        .db()
        .seed_proposal(
            &w,
            "0f0f0f0f-0000-4000-8000-000000000001",
            &s,
            fake_commit(b"cand-archive"),
            g.version_id.unwrap(),
            1,
            1,
            "open",
            &prin(BOB),
        )
        .await
        .unwrap();

    let out = fx
        .authority
        .archive_skill_session(
            &w,
            ALICE,
            "s_deploy",
            DeploymentMode::Cloud,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    let LifecycleOutcome::Archived { archived_name } = out else {
        panic!("expected Archived, got {out:?}");
    };
    assert_eq!(
        archived_name, "deploy-archived-2026-06-28",
        "renamed with the date label"
    );
    assert_eq!(
        catalog(&pool, "w_acme", "s_deploy").await,
        Some(("archived".to_owned(), archived_name.clone())),
        "the catalog row is archived under its suffixed name"
    );

    // The base name is FREE — a NEW genesis under folder name "Deploy" mints a fresh skill_id and lands.
    let s2 = skill("s_deploy2");
    let g2 = gpub(
        &fx,
        &w,
        &s2,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000002",
        vec![file("SKILL.md", b"fresh")],
        "Deploy",
    )
    .await;
    assert!(g2.is_ok(), "a fresh identity claims the freed base name");
    assert_eq!(
        catalog(&pool, "w_acme", "s_deploy2").await.unwrap().1,
        "deploy"
    );

    // The OLD skill is out of delivery; the NEW one is in (via everyone).
    let d = deliver(&fx, &w, "dk_bob").await;
    assert!(
        find(&d, "s_deploy").is_none(),
        "the archived skill is out of delivery"
    );
    assert!(find(&d, "s_deploy2").is_some(), "the fresh skill delivers");

    // The proposal is CLOSED (not rejected) with the circumstantial reason, and bob has a notice.
    assert_eq!(
        proposal(&pool, "w_acme", "0f0f0f0f-0000-4000-8000-000000000001").await,
        Some(("closed".to_owned(), Some("skill archived".to_owned())))
    );
    assert!(
        d.notices
            .iter()
            .any(|n| n.kind == "proposal_closed" && n.outcome.as_deref() == Some("closed")),
        "the proposer gets a proposal_closed notice: {:?}",
        d.notices
    );
}

/// A same-day archive of a second skill under the SAME base name gets a numeric suffix — two
/// retirements of "deploy" on one day never collide on the archived name.
#[sqlx::test]
async fn same_day_archive_repeats_get_a_numeric_suffix(pool: PgPool) {
    let fx = Fixture::new(pool, "chl-archive-repeat").await;
    let w = ws("w_acme");
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;

    // skill 1 "deploy" → archived at the date.
    gpub(
        &fx,
        &w,
        &skill("s_one"),
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("f", b"1")],
        "Deploy",
    )
    .await;
    fx.authority
        .archive_skill_session(&w, ALICE, "s_one", DeploymentMode::Cloud, CREATED_AT, NOW)
        .await
        .unwrap();
    // skill 2 claims the freed "deploy", then archives SAME day → the "-2" suffix.
    gpub(
        &fx,
        &w,
        &skill("s_two"),
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000002",
        vec![file("f", b"2")],
        "Deploy",
    )
    .await;
    let out = fx
        .authority
        .archive_skill_session(&w, ALICE, "s_two", DeploymentMode::Cloud, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(
        out,
        LifecycleOutcome::Archived {
            archived_name: "deploy-archived-2026-06-28-2".to_owned()
        },
        "the same-day repeat gets the numeric suffix"
    );
}

/// The archive refusals: a non-owner is `OwnerRoleRequired`, archiving an already-archived skill is
/// `NotActive`, an unknown skill id is the uniform `NotFound`, and a self-host plane ANSWERS the op exactly
/// like a hosted one (the owner gate runs identically — a non-owner still gets `OwnerRoleRequired`, not a
/// blanket posture denial) — the typed reasons a confirmed member is entitled to, and the uniform misses
/// that leak nothing.
#[sqlx::test]
async fn archive_refusals_non_owner_not_active_unknown_and_self_host(pool: PgPool) {
    let fx = Fixture::new(pool, "chl-archive-refuse").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;
    seat(&fx, &w, "dk_mem", 12, BOB, "member").await;
    gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("f", b"v0")],
        "Deploy",
    )
    .await;

    // A confirmed non-owner member gets the typed role refusal.
    assert_eq!(
        fx.authority
            .archive_skill_session(&w, BOB, "s_deploy", DeploymentMode::Cloud, CREATED_AT, NOW)
            .await
            .unwrap(),
        LifecycleOutcome::OwnerRoleRequired
    );
    // An unknown skill id is the uniform NotFound.
    assert!(matches!(
        fx.authority
            .archive_skill_session(&w, ALICE, "s_nope", DeploymentMode::Cloud, CREATED_AT, NOW)
            .await,
        Err(AuthorityError::NotFound)
    ));
    // A self-host plane ANSWERS the op exactly like a hosted one — the acting gate is the confirmed-seat
    // check, identical on both postures (no blanket self-host denial). The OWNER role gate runs on
    // self-host too: a confirmed non-owner still gets the typed OwnerRoleRequired (a non-mutating probe).
    assert_eq!(
        fx.authority
            .archive_skill_session(
                &w,
                BOB,
                "s_deploy",
                DeploymentMode::SelfHost,
                CREATED_AT,
                NOW
            )
            .await
            .unwrap(),
        LifecycleOutcome::OwnerRoleRequired
    );
    // Archiving twice: the second is NotActive (the id names the same identity across the rename).
    assert!(matches!(
        fx.authority
            .archive_skill_session(
                &w,
                ALICE,
                "s_deploy",
                DeploymentMode::Cloud,
                CREATED_AT,
                NOW
            )
            .await
            .unwrap(),
        LifecycleOutcome::Archived { .. }
    ));
    assert_eq!(
        fx.authority
            .archive_skill_session(
                &w,
                ALICE,
                "s_deploy",
                DeploymentMode::Cloud,
                CREATED_AT,
                NOW
            )
            .await
            .unwrap(),
        LifecycleOutcome::NotActive
    );
}

// ── unarchive ────────────────────────────────────────────────────────────────────────────────────────

/// Unarchive restores the base name and the AUTHOR'S self-follow survives the round-trip (so the
/// author gets the skill back via their direct follow) while a non-author member does NOT (the channel
/// placements are NOT restored — curation moved on).
#[sqlx::test]
async fn unarchive_restores_the_name_and_the_author_self_follow_survives(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chl-unarchive").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await; // ALICE authors it
    seat(&fx, &w, "dk_bob", 12, BOB, "member").await;
    gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("f", b"v0")],
        "Deploy",
    )
    .await;
    // Both receive it before the archive (ALICE direct+everyone, bob everyone).
    assert!(find(&deliver(&fx, &w, "dk_owner").await, "s_deploy").is_some());
    assert!(find(&deliver(&fx, &w, "dk_bob").await, "s_deploy").is_some());

    fx.authority
        .archive_skill_session(
            &w,
            ALICE,
            "s_deploy",
            DeploymentMode::Cloud,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    let out = fx
        .authority
        .unarchive_skill_session(&w, ALICE, "s_deploy", DeploymentMode::Cloud)
        .await
        .unwrap();
    assert_eq!(
        out,
        LifecycleOutcome::Unarchived {
            name: "deploy".to_owned()
        }
    );

    // Placements were NOT restored.
    assert_eq!(
        placements(&pool, "w_acme", "s_deploy").await,
        0,
        "curation is not restored"
    );
    // The author gets it back via the surviving self-follow; the non-author member does not.
    let da = deliver(&fx, &w, "dk_owner").await;
    let ds = find(&da, "s_deploy").expect("the author's self-follow survives");
    assert!(ds.direct, "delivered by the direct follow, not a channel");
    assert!(ds.via_channels.is_empty());
    assert!(
        find(&deliver(&fx, &w, "dk_bob").await, "s_deploy").is_none(),
        "a non-author member has no follow and no placement ⇒ nothing"
    );
}

/// Unarchive refuses (`NameTaken`) when the base name was reused by a NEW identity while the skill was
/// archived — two identities cannot share one name; keep the suffix (or rename on the web).
#[sqlx::test]
async fn unarchive_refuses_when_the_base_name_was_reused(pool: PgPool) {
    let fx = Fixture::new(pool, "chl-unarchive-taken").await;
    let w = ws("w_acme");
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;
    gpub(
        &fx,
        &w,
        &skill("s_old"),
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("f", b"old")],
        "Deploy",
    )
    .await;
    fx.authority
        .archive_skill_session(&w, ALICE, "s_old", DeploymentMode::Cloud, CREATED_AT, NOW)
        .await
        .unwrap();
    // A fresh identity claims "deploy".
    gpub(
        &fx,
        &w,
        &skill("s_new"),
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000002",
        vec![file("f", b"new")],
        "Deploy",
    )
    .await;
    // Unarchiving the OLD skill now collides on the base name.
    assert_eq!(
        fx.authority
            .unarchive_skill_session(&w, ALICE, "s_old", DeploymentMode::Cloud)
            .await
            .unwrap(),
        LifecycleOutcome::NameTaken
    );
}

// ── delete ────────────────────────────────────────────────────────────────────────────────────────

/// Delete is archive-first (an active skill is `NotArchived`); once deleted the catalog row is a
/// tombstone, `current` is GONE, the `commit_object` edges are un-rooted, the GC reclaims the bytes
/// (read_object → NotFound), and a publish onto the deleted skill id is DENIED "deleted".
#[sqlx::test]
async fn delete_requires_archive_first_then_un_roots_reclaims_and_denies_writes(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chl-delete").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;
    let body: &[u8] = b"delete-me-bytes";
    gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", body)],
        "Deploy",
    )
    .await;
    let obj = object_id(body);
    // The object is readable while the skill lives.
    assert!(
        fx.authority
            .read_object(&prin(ALICE), &w, &s, obj)
            .await
            .is_ok(),
        "readable before delete"
    );

    // Delete of an ACTIVE skill is refused (archive-first).
    assert_eq!(
        fx.authority
            .delete_skill_session(&w, ALICE, "s_deploy", DeploymentMode::Cloud, NOW)
            .await
            .unwrap(),
        LifecycleOutcome::NotArchived
    );
    // Archive, then delete.
    fx.authority
        .archive_skill_session(
            &w,
            ALICE,
            "s_deploy",
            DeploymentMode::Cloud,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(
        fx.authority
            .delete_skill_session(&w, ALICE, "s_deploy", DeploymentMode::Cloud, NOW)
            .await
            .unwrap(),
        LifecycleOutcome::Deleted
    );

    // The catalog row is a tombstone (status deleted, archived spelling kept); current is gone.
    assert_eq!(
        catalog(&pool, "w_acme", "s_deploy").await.unwrap().0,
        "deleted"
    );
    assert!(
        fx.authority
            .db()
            .read_current_commit(&w, &s)
            .await
            .unwrap()
            .is_none(),
        "the current pointer is dropped"
    );
    // The object is un-rooted (read_object → NotFound) and the GC reclaims it.
    assert!(matches!(
        fx.authority.read_object(&prin(ALICE), &w, &s, obj).await,
        Err(AuthorityError::NotFound)
    ));
    let reclaimed = fx.authority.run_gc(&w, NOW + 1_000_000).await.unwrap();
    assert!(
        reclaimed >= 1,
        "the deleted skill's bytes are reclaimed: {reclaimed}"
    );

    // A publish onto the deleted skill id is DENIED "deleted".
    let r = fx
        .authority
        .publish(
            &w,
            &s,
            &op("aaaaaaaa-0000-4000-8000-000000000009"),
            genesis(vec![file("SKILL.md", b"resurrect")]),
            DeviceOpAuth {
                credential: cred(&w, "dk_owner"),
                op: DeviceOp::PublishDirect,
                expected: gn(0, 0),
            },
            Some("Deploy"),
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    assert_eq!(
        r.details
            .as_ref()
            .and_then(|d| d.get("message"))
            .and_then(serde_json::Value::as_str),
        Some("the skill is deleted"),
    );
}

/// A publish onto an ARCHIVED skill id is a typed DENIED, and a follow of the archived NAME is
/// `SkillNotActive` — an archived identity is out of circulation for both writes and follows.
#[sqlx::test]
async fn publish_and_follow_on_an_archived_skill_are_typed_refusals(pool: PgPool) {
    let fx = Fixture::new(pool, "chl-archived-writes").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member").await;
    let g = gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("f", b"v0")],
        "Deploy",
    )
    .await;
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();
    fx.authority
        .archive_skill_session(
            &w,
            ALICE,
            "s_deploy",
            DeploymentMode::Cloud,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();

    // A child publish onto the archived skill id is DENIED "archived".
    let r = fx
        .authority
        .publish(
            &w,
            &s,
            &op("aaaaaaaa-0000-4000-8000-000000000002"),
            child(c0, vec![file("f", b"v1")]),
            DeviceOpAuth {
                credential: cred(&w, "dk_owner"),
                op: DeviceOp::PublishDirect,
                expected: g.current.unwrap(),
            },
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    assert_eq!(
        r.details
            .as_ref()
            .and_then(|d| d.get("message"))
            .and_then(serde_json::Value::as_str),
        Some("the skill is archived"),
    );
    // A follow of the archived name is SkillNotActive.
    assert_eq!(
        fx.authority
            .follow_skill(&w, &cred(&w, "dk_bob"), s.as_str(), CREATED_AT)
            .await
            .unwrap(),
        SubscriptionOutcome::SkillNotActive
    );
}

// ── purge ────────────────────────────────────────────────────────────────────────────────────────

/// Purge is the leak tool: it refuses the CURRENT version (`IsCurrent`), tombstones ONE prior
/// version (who/when) and un-roots only ITS edges so the GC reclaims that version's UNIQUE bytes while
/// a blob it SHARES with a live version stays readable, drops it out of the version-metadata read, and
/// closes a dependent proposal with an author notice; a re-purge is `AlreadyPurged`.
#[sqlx::test]
async fn purge_un_roots_one_version_reclaims_its_unique_bytes_and_closes_dependents(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chl-purge").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member").await;

    // v1 = {shared, secret}; v2 = {shared (unchanged), other}. v2 does NOT reference secret's object.
    let shared: &[u8] = b"shared-content";
    let secret: &[u8] = b"v1-secret-unique";
    let v1 = gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("shared.txt", shared), file("secret.txt", secret)],
        "Deploy",
    )
    .await;
    let c1 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();
    let v2 = child_pub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000002",
        c1,
        vec![file("shared.txt", shared), file("other.txt", b"v2-new")],
    )
    .await;
    let v1_commit = v1.version_id.unwrap();
    let v2_commit = v2.version_id.unwrap();
    let obj_secret = object_id(secret);
    let obj_shared = object_id(shared);

    // A dependent OPEN proposal rooted on v1 (closed by the purge with a notice to bob).
    fx.authority
        .db()
        .seed_proposal(
            &w,
            "0f0f0f0f-0000-4000-8000-000000000002",
            &s,
            fake_commit(b"cand-purge"),
            v1_commit,
            1,
            1,
            "open",
            &prin(BOB),
        )
        .await
        .unwrap();

    // Purging the CURRENT version is refused.
    assert_eq!(
        fx.authority
            .purge_version_session(
                &w,
                ALICE,
                "s_deploy",
                v2_commit,
                DeploymentMode::Cloud,
                CREATED_AT,
                NOW
            )
            .await
            .unwrap(),
        PurgeOutcome::IsCurrent
    );
    // Purge v1: un-rooted + tombstoned.
    assert_eq!(
        fx.authority
            .purge_version_session(
                &w,
                ALICE,
                "s_deploy",
                v1_commit,
                DeploymentMode::Cloud,
                CREATED_AT,
                NOW
            )
            .await
            .unwrap(),
        PurgeOutcome::Purged
    );
    let (purged_at, purged_by) = purge_stamp(&pool, "w_acme", &v1_commit.0).await;
    assert_eq!(purged_at, Some(NOW), "the tombstone records WHEN");
    assert_eq!(purged_by.as_deref(), Some(ALICE), "…and WHO");

    // The GC reclaims v1's UNIQUE blob; the SHARED blob stays readable via v2.
    fx.authority.run_gc(&w, NOW + 1_000_000).await.unwrap();
    assert!(
        matches!(
            fx.authority
                .read_object(&prin(ALICE), &w, &s, obj_secret)
                .await,
            Err(AuthorityError::NotFound)
        ),
        "v1's unique blob is unreadable after the purge + GC"
    );
    assert_eq!(
        fx.authority
            .read_object(&prin(ALICE), &w, &s, obj_shared)
            .await
            .unwrap(),
        shared.to_vec(),
        "the shared blob stays readable via v2"
    );
    // v1's version-metadata read is now the uniform NotFound (un-rooted ⇒ not reachable).
    assert!(matches!(
        fx.authority
            .read_version_metadata_session(
                &w,
                "deploy",
                &digest::to_hex(&v1_commit.0),
                ALICE,
                DeploymentMode::Cloud
            )
            .await,
        Err(AuthorityError::NotFound)
    ));

    // The dependent proposal is closed with the circumstantial reason + a notice to bob.
    assert_eq!(
        proposal(&pool, "w_acme", "0f0f0f0f-0000-4000-8000-000000000002").await,
        Some((
            "closed".to_owned(),
            Some("a version it rests on was purged".to_owned())
        ))
    );
    assert!(
        deliver(&fx, &w, "dk_bob")
            .await
            .notices
            .iter()
            .any(|n| n.kind == "proposal_closed"),
        "the proposer is notified"
    );

    // A re-purge is idempotent information.
    assert_eq!(
        fx.authority
            .purge_version_session(
                &w,
                ALICE,
                "s_deploy",
                v1_commit,
                DeploymentMode::Cloud,
                CREATED_AT,
                NOW
            )
            .await
            .unwrap(),
        PurgeOutcome::AlreadyPurged
    );
}

// ── the downgraded revert ────────────────────────────────────────────────────────────────────────────

/// Under an effectively-REVIEWED bundle a plain member's `revert` is DOWNGRADED to a proposal
/// (NEEDS_REVIEW + a `downgraded` detail, the pointer frozen) exactly as a direct publish is, while a
/// reviewer's revert LANDS (the protected-branch model applies to revert too — the safety net still
/// respects the branch protection for a non-reviewer).
#[sqlx::test]
async fn a_member_revert_on_a_reviewed_bundle_downgrades_but_a_reviewer_lands(pool: PgPool) {
    let fx = Fixture::new(pool, "chl-revert-downgrade").await;
    let w = ws("w_acme");
    let (sa, sb) = (skill("s_alpha"), skill("s_beta"));
    seat(&fx, &w, "dk_mem", 11, BOB, "member").await;
    seat(&fx, &w, "dk_rev", 12, "rev@acme.com", "reviewer").await;

    // Build a v1→v2 chain on BOTH skills while review is OFF (so the member's children land), capturing
    // each genesis commit id as the revert target.
    let a_genesis;
    let b_genesis;
    {
        let mut genesis = std::collections::HashMap::new();
        for (s, tag) in [(&sa, 'a'), (&sb, 'b')] {
            let g = gpub(
                &fx,
                &w,
                s,
                "dk_mem",
                &format!("{tag}0000000-0000-4000-8000-000000000001"),
                vec![file("f", format!("v1-{tag}").as_bytes())],
                &format!("Skill {tag}"),
            )
            .await;
            genesis.insert(s.as_str().to_owned(), g.version_id.unwrap());
            let c1 = fx
                .authority
                .db()
                .read_current_commit(&w, s)
                .await
                .unwrap()
                .unwrap();
            child_pub(
                &fx,
                &w,
                s,
                "dk_mem",
                &format!("{tag}0000000-0000-4000-8000-000000000002"),
                c1,
                vec![file("f", format!("v2-{tag}").as_bytes())],
            )
            .await;
        }
        a_genesis = genesis[sa.as_str()];
        b_genesis = genesis[sb.as_str()];
    }
    fx.authority.set_review_required(&w, true).await.unwrap();

    // A member's revert on skill A DOWNGRADES to a proposal (pointer unmoved at v2).
    let a_current = fx
        .authority
        .db()
        .read_current_generation(&w, &sa)
        .await
        .unwrap()
        .unwrap();
    let mr = fx
        .authority
        .revert(
            &w,
            &sa,
            a_genesis,
            DeviceOpAuth {
                credential: cred(&w, "dk_mem"),
                op: DeviceOp::Revert,
                expected: a_current,
            },
            "d_test",
            "topos revert",
            &op("a0000000-0000-4000-8000-0000000000aa"),
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(mr.outcome, TerminalOutcome::NeedsReview);
    assert_eq!(
        mr.details
            .as_ref()
            .and_then(|d| d.get("downgraded"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "a member's revert downgrades to a proposal"
    );
    assert_eq!(
        fx.authority
            .db()
            .read_current_generation(&w, &sa)
            .await
            .unwrap(),
        Some(a_current),
        "the pointer is frozen"
    );

    // A reviewer's revert on skill B LANDS (the pointer advances).
    let b_current = fx
        .authority
        .db()
        .read_current_generation(&w, &sb)
        .await
        .unwrap()
        .unwrap();
    let rr = fx
        .authority
        .revert(
            &w,
            &sb,
            b_genesis,
            DeviceOpAuth {
                credential: cred(&w, "dk_rev"),
                op: DeviceOp::Revert,
                expected: b_current,
            },
            "d_test",
            "topos revert",
            &op("b0000000-0000-4000-8000-0000000000bb"),
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(rr.outcome, TerminalOutcome::Ok, "a reviewer's revert lands");
    assert!(
        fx.authority
            .db()
            .read_current_generation(&w, &sb)
            .await
            .unwrap()
            .unwrap()
            .seq
            > b_current.seq,
        "the pointer advanced"
    );
}

// ── rename ────────────────────────────────────────────────────────────────────────────────────────

/// The owner rename: the identity (and every id-keyed reference) is untouched — only the catalog
/// name moves — and the OLD name keeps resolving as a hint (`topos_resolve_skill` answers the live
/// spelling with `via = 'hint'`) until a new identity claims it. The refusals are the typed reasons a
/// confirmed member is entitled to: a non-owner `OwnerRoleRequired`, a rule-breaking name `BadName`,
/// a name another identity holds `NameTaken`, an archived skill `NotActive`; an unknown id is the
/// uniform `NotFound`.
#[sqlx::test]
async fn rename_moves_the_name_leaves_a_resolving_hint_and_refuses_typed(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chl-rename").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member").await;
    gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("f", b"v0")],
        "Deploy",
    )
    .await;
    // A second identity holding "docs" (the NameTaken witness).
    gpub(
        &fx,
        &w,
        &skill("s_docs"),
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000002",
        vec![file("f", b"docs")],
        "Docs",
    )
    .await;

    // A confirmed non-owner member gets the typed role refusal; an unknown id the uniform miss.
    assert_eq!(
        fx.authority
            .rename_skill_session(
                &w,
                BOB,
                "s_deploy",
                "ship",
                DeploymentMode::Cloud,
                CREATED_AT
            )
            .await
            .unwrap(),
        crate::RenameOutcome::OwnerRoleRequired
    );
    assert!(matches!(
        fx.authority
            .rename_skill_session(
                &w,
                ALICE,
                "s_nope",
                "ship",
                DeploymentMode::Cloud,
                CREATED_AT
            )
            .await,
        Err(AuthorityError::NotFound)
    ));
    // The name rules hold: charset/length and the reserved archive-rename pattern.
    assert_eq!(
        fx.authority
            .rename_skill_session(
                &w,
                ALICE,
                "s_deploy",
                "Bad Name!",
                DeploymentMode::Cloud,
                CREATED_AT
            )
            .await
            .unwrap(),
        crate::RenameOutcome::BadName
    );
    assert_eq!(
        fx.authority
            .rename_skill_session(
                &w,
                ALICE,
                "s_deploy",
                "ship-archived-2026",
                DeploymentMode::Cloud,
                CREATED_AT
            )
            .await
            .unwrap(),
        crate::RenameOutcome::BadName
    );
    // Two identities cannot share one name.
    assert_eq!(
        fx.authority
            .rename_skill_session(
                &w,
                ALICE,
                "s_deploy",
                "docs",
                DeploymentMode::Cloud,
                CREATED_AT
            )
            .await
            .unwrap(),
        crate::RenameOutcome::NameTaken
    );

    // The happy path: renamed, and the OLD name resolves as a hint to the live identity.
    assert_eq!(
        fx.authority
            .rename_skill_session(
                &w,
                ALICE,
                "s_deploy",
                "ship",
                DeploymentMode::Cloud,
                CREATED_AT
            )
            .await
            .unwrap(),
        crate::RenameOutcome::Renamed {
            name: "ship".to_owned()
        }
    );
    assert_eq!(
        catalog(&pool, "w_acme", "s_deploy").await,
        Some(("active".to_owned(), "ship".to_owned()))
    );
    let hint = resolve(&pool, "w_acme", "deploy").await;
    assert_eq!(
        hint,
        Some((
            "s_deploy".to_owned(),
            "ship".to_owned(),
            "active".to_owned(),
            "hint".to_owned()
        )),
        "the old name keeps resolving, flagged as a hint carrying the live spelling"
    );
    // The live name resolves via the catalog arm, and a fresh identity claiming the freed old name
    // SHADOWS the hint (one name, one meaning at a time).
    assert_eq!(
        resolve(&pool, "w_acme", "ship").await.map(|r| r.3),
        Some("name".to_owned())
    );
    gpub(
        &fx,
        &w,
        &skill("s_fresh"),
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000003",
        vec![file("f", b"fresh")],
        "Deploy",
    )
    .await;
    assert_eq!(
        resolve(&pool, "w_acme", "deploy").await,
        Some((
            "s_fresh".to_owned(),
            "deploy".to_owned(),
            "active".to_owned(),
            "name".to_owned()
        )),
        "a live identity shadows the hint"
    );

    // An archived skill refuses the rename typed.
    fx.authority
        .archive_skill_session(
            &w,
            ALICE,
            "s_deploy",
            DeploymentMode::Cloud,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(
        fx.authority
            .rename_skill_session(
                &w,
                ALICE,
                "s_deploy",
                "boat",
                DeploymentMode::Cloud,
                CREATED_AT
            )
            .await
            .unwrap(),
        crate::RenameOutcome::NotActive
    );
}

/// One `topos_resolve_skill` row as `(skill_id, name, status, via)` (raw `sqlx`, off the committed
/// `.sqlx` drift surface).
async fn resolve(pool: &PgPool, w: &str, name: &str) -> Option<(String, String, String, String)> {
    use sqlx::Row as _;
    sqlx::query("SELECT skill_id, name, status, via FROM topos_resolve_skill($1, $2)")
        .bind(w)
        .bind(name)
        .fetch_optional(pool)
        .await
        .unwrap()
        .map(|r| {
            (
                r.get::<String, _>("skill_id"),
                r.get::<String, _>("name"),
                r.get::<String, _>("status"),
                r.get::<String, _>("via"),
            )
        })
}

// ── the web-admin roster/channel/device acts (guarded functions, called as the web tier calls them) ──

/// One guarded-function call answering a TEXT outcome, with string binds (raw `sqlx`, exactly the
/// call shape the web tier uses).
async fn fn_outcome(pool: &PgPool, sql: &str, binds: &[&str]) -> String {
    let mut q = sqlx::query_scalar::<_, String>(sql);
    for b in binds {
        q = q.bind(*b);
    }
    q.fetch_one(pool).await.unwrap()
}

/// `topos_set_member_role`: an owner act on any seat, with the last-owner lockout — demoting the
/// sole confirmed owner refuses (`sole_owner`) until a second confirmed owner exists.
#[sqlx::test]
async fn set_member_role_is_owner_gated_and_the_sole_owner_cannot_be_demoted(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chl-role").await;
    let w = ws("w_acme");
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member").await;
    let sql = "SELECT topos_set_member_role($1, $2, $3, $4)";

    // The gates: a stranger is member_required, a plain member owner_role_required, a bad role and
    // an unknown target typed.
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", "eve@else.com", BOB, "reviewer"]).await,
        "member_required"
    );
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", BOB, ALICE, "member"]).await,
        "owner_role_required"
    );
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", ALICE, BOB, "admin"]).await,
        "bad_role"
    );
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", ALICE, "ghost@acme.com", "member"]).await,
        "unknown_member"
    );

    // The owner raises bob to reviewer; the row moves.
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", ALICE, BOB, "reviewer"]).await,
        "set"
    );
    let role: String = sqlx::query_scalar::<_, String>(
        "SELECT role FROM workspace_member WHERE workspace_id = $1 AND principal = $2",
    )
    .bind("w_acme")
    .bind(BOB)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(role, "reviewer");

    // Demoting the SOLE confirmed owner refuses; after a second owner exists it lands.
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", ALICE, ALICE, "member"]).await,
        "sole_owner"
    );
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", ALICE, BOB, "owner"]).await,
        "set"
    );
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", ALICE, ALICE, "member"]).await,
        "set"
    );
}

/// `topos_leave_workspace`: the self-serve seat delete. The lapse-detach reconcile runs BEFORE the
/// seat delete — the person's detachment records land (proving the entitled set was still readable
/// when they were written; after the delete the membership-gated union reads empty) — and a sole
/// confirmed owner cannot leave.
#[sqlx::test]
async fn leave_writes_the_detach_records_before_the_seat_delete_and_locks_the_sole_owner(
    pool: PgPool,
) {
    let fx = Fixture::new(pool.clone(), "chl-leave").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member").await;
    gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("f", b"v0")],
        "Deploy",
    )
    .await;
    // bob receives it via `everyone` before leaving.
    assert!(find(&deliver(&fx, &w, "dk_bob").await, "s_deploy").is_some());

    // A stranger is the typed member_required; the sole confirmed owner cannot leave.
    let eve: String = sqlx::query_scalar("SELECT topos_leave_workspace($1, $2, $3, $4)")
        .bind("w_acme")
        .bind("eve@else.com")
        .bind(NOW)
        .bind(CREATED_AT)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(eve, "member_required");
    let alice: String = sqlx::query_scalar("SELECT topos_leave_workspace($1, $2, $3, $4)")
        .bind("w_acme")
        .bind(ALICE)
        .bind(NOW)
        .bind(CREATED_AT)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alice, "sole_owner");

    // bob leaves: the seat is gone AND the person-scoped detachment record exists — written by the
    // reconcile that ran BEFORE the delete (the union is membership-gated, so a post-delete
    // reconcile would have had nothing to record).
    let left: String = sqlx::query_scalar("SELECT topos_leave_workspace($1, $2, $3, $4)")
        .bind("w_acme")
        .bind(BOB)
        .bind(NOW)
        .bind(CREATED_AT)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, "left");
    let seated: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM workspace_member WHERE workspace_id = $1 AND principal = $2",
    )
    .bind("w_acme")
    .bind(BOB)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(seated.is_none(), "the seat is deleted");
    let detached: Option<String> = sqlx::query_scalar(
        "SELECT cause FROM skill_detachments \
         WHERE workspace_id = $1 AND principal = $2 AND skill_id = 's_deploy'",
    )
    .bind("w_acme")
    .bind(BOB)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(
        detached.as_deref(),
        Some("membership_removed"),
        "the detach record landed before the seat delete"
    );
}

/// `topos_channel_rename` / `topos_channel_delete`: owner existence-acts, both keyed on the
/// IMMUTABLE channel_id (a stale caller whose name was freed and reused misses; it never
/// retargets). `everyone` refuses typed (`builtin`) on both; a rename moves only the display key
/// (the channel_id survives); a delete cascades the references and memberships itself and writes
/// NO person-detach records (a deletion is an upstream withdrawal, never the person's own act).
#[sqlx::test]
async fn channel_rename_and_delete_are_owner_acts_that_spare_everyone_and_detach_nobody(
    pool: PgPool,
) {
    let fx = Fixture::new(pool.clone(), "chl-chadmin").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member").await;
    gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("f", b"v0")],
        "Deploy",
    )
    .await;
    // A channel with a reference and a member: place creates `tools`, bob joins + follows via it.
    assert_eq!(
        fn_outcome(
            &pool,
            "SELECT topos_channel_place($1, $2, $3, $4, $5)",
            &["w_acme", "tools", "s_deploy", ALICE, CREATED_AT],
        )
        .await,
        "created"
    );
    assert_eq!(
        fn_outcome(
            &pool,
            "SELECT topos_channel_join($1, $2, $3, $4)",
            &["w_acme", "tools", BOB, CREATED_AT],
        )
        .await,
        "joined"
    );

    // The gates: builtin refusals for `everyone`, the owner gate, name rules, unknown channel.
    let rename = "SELECT topos_channel_rename($1, $2, $3, $4, $5)";
    let delete = "SELECT topos_channel_delete($1, $2, $3, $4)";
    assert_eq!(
        fn_outcome(
            &pool,
            rename,
            &["w_acme", "everyone", "all", ALICE, CREATED_AT]
        )
        .await,
        "builtin"
    );
    assert_eq!(
        fn_outcome(&pool, delete, &["w_acme", "everyone", ALICE, CREATED_AT]).await,
        "builtin"
    );
    assert_eq!(
        fn_outcome(
            &pool,
            rename,
            &["w_acme", "tools", "toolbox", BOB, CREATED_AT]
        )
        .await,
        "owner_role_required"
    );
    assert_eq!(
        fn_outcome(
            &pool,
            rename,
            &["w_acme", "tools", "Bad Name!", ALICE, CREATED_AT]
        )
        .await,
        "bad_name"
    );
    assert_eq!(
        fn_outcome(
            &pool,
            rename,
            &["w_acme", "tools", "everyone", ALICE, CREATED_AT]
        )
        .await,
        "name_taken"
    );
    assert_eq!(
        fn_outcome(&pool, rename, &["w_acme", "nope", "x", ALICE, CREATED_AT]).await,
        "unknown_channel"
    );

    // Rename: only the display key moves — the channel_id, references, and memberships survive.
    assert_eq!(
        fn_outcome(
            &pool,
            rename,
            &["w_acme", "tools", "toolbox", ALICE, CREATED_AT]
        )
        .await,
        "renamed"
    );
    let (cid, refs, members): (String, i64, i64) = {
        use sqlx::Row as _;
        let r = sqlx::query(
            "SELECT ch.channel_id,
                    (SELECT COUNT(*)::int8 FROM channel_skills cs
                     WHERE cs.workspace_id = ch.workspace_id AND cs.channel_id = ch.channel_id),
                    (SELECT COUNT(*)::int8 FROM channel_members cm
                     WHERE cm.workspace_id = ch.workspace_id AND cm.channel_id = ch.channel_id)
             FROM channels ch WHERE ch.workspace_id = $1 AND ch.name = 'toolbox'",
        )
        .bind("w_acme")
        .fetch_one(&pool)
        .await
        .unwrap();
        (r.get(0), r.get(1), r.get(2))
    };
    assert_eq!(cid, "tools", "the immutable channel_id never moves");
    assert_eq!((refs, members), (1, 1), "references + memberships survive");

    // Delete addresses the IMMUTABLE id — the rename moved only the name, so the NEW name is not
    // a valid selector here (and a freed-then-reused name can never retarget a stale caller).
    assert_eq!(
        fn_outcome(&pool, delete, &["w_acme", "toolbox", ALICE, CREATED_AT]).await,
        "unknown_channel"
    );
    // ... and NOBODY gets a person-detach record once it lands (the skill still delivers via
    // `everyone`; a channel deletion is an upstream act).
    assert_eq!(
        fn_outcome(&pool, delete, &["w_acme", "tools", ALICE, CREATED_AT]).await,
        "deleted"
    );
    for probe in [
        "SELECT COUNT(*)::int8 FROM channels WHERE workspace_id = $1 AND channel_id = 'tools'",
        "SELECT COUNT(*)::int8 FROM channel_skills WHERE workspace_id = $1 AND channel_id = 'tools'",
        "SELECT COUNT(*)::int8 FROM channel_members WHERE workspace_id = $1 AND channel_id = 'tools'",
    ] {
        let n: i64 = sqlx::query_scalar(probe)
            .bind("w_acme")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0, "cascaded: {probe}");
    }
    let detachments: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::int8 FROM skill_detachments WHERE workspace_id = $1")
            .bind("w_acme")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(detachments, 0, "an upstream delete detaches nobody");
    assert!(
        find(&deliver(&fx, &w, "dk_bob").await, "s_deploy").is_some(),
        "the skill keeps flowing via everyone"
    );
}

/// `topos_revoke_device`: the session-side revoke — the device's own principal signs it out, an
/// owner revokes anyone's, another member is refused typed, and a re-revoke is idempotent.
#[sqlx::test]
async fn revoke_device_allows_self_and_owner_and_refuses_another_member(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chl-revoke").await;
    let w = ws("w_acme");
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member").await;
    seat(&fx, &w, "dk_carol", 13, "carol@acme.com", "member").await;
    let sql = "SELECT topos_revoke_device($1, $2, $3)";

    // The gates: a stranger, an unknown device, and another plain member all refuse typed.
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", "eve@else.com", "dk_bob"]).await,
        "member_required"
    );
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", ALICE, "dk_ghost"]).await,
        "unknown_device"
    );
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", "carol@acme.com", "dk_bob"]).await,
        "owner_or_self_required"
    );

    // Self sign-out and the owner's revoke both land; re-revoking answers the same.
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", BOB, "dk_bob"]).await,
        "revoked"
    );
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", ALICE, "dk_carol"]).await,
        "revoked"
    );
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", BOB, "dk_bob"]).await,
        "revoked"
    );
    let revoked: Vec<String> = sqlx::query_scalar(
        "SELECT device_key_id FROM device_registry \
         WHERE workspace_id = $1 AND revoked = 1 ORDER BY device_key_id",
    )
    .bind("w_acme")
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(revoked, vec!["dk_bob".to_owned(), "dk_carol".to_owned()]);
}

/// The sole-owner lockout is a READ-THEN-WRITE invariant, and the web tier calls these functions
/// from plain autocommit (READ COMMITTED) connections — where two racing owner-losing acts would
/// each read the pre-race snapshot and both commit, leaving the workspace OWNERLESS (a state no
/// serial order can produce). The functions therefore serialize the guard THEMSELVES (a `FOR
/// UPDATE` lock over the confirmed-owner seats, one deterministic order), so the second racer
/// waits, re-reads committed state, and refuses. This drives the race at the isolation the web
/// tier actually uses: two owners demoting each other, and two owners leaving at once. Exactly one
/// act may land in each pair, and a confirmed owner always survives.
#[sqlx::test]
async fn the_sole_owner_guard_holds_against_racing_web_lane_writers(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chl-race").await;
    let w = ws("w_acme");
    seat(&fx, &w, "dk_alice", 11, ALICE, "owner").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "owner").await;

    let confirmed_owners = |pool: PgPool| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::int8 FROM workspace_member \
             WHERE workspace_id = $1 AND role = 'owner' AND status = 'confirmed'",
        )
        .bind("w_acme")
        .fetch_one(&pool)
        .await
        .unwrap()
    };

    // RACE 1 — each owner demotes the other, concurrently, on separate connections.
    let (a, b) = tokio::join!(
        fn_outcome(
            &pool,
            "SELECT topos_set_member_role($1, $2, $3, $4)",
            &["w_acme", ALICE, BOB, "member"],
        ),
        fn_outcome(
            &pool,
            "SELECT topos_set_member_role($1, $2, $3, $4)",
            &["w_acme", BOB, ALICE, "member"],
        ),
    );
    // The LOSER's refusal code depends on the interleaving — it either took the fence first and
    // re-read committed state (`sole_owner`), or started after the winner committed and finds its
    // OWN seat demoted (`owner_role_required`). Both are correct refusals; what must never happen
    // is two landings, which would leave the workspace ownerless.
    let outcomes = [a.as_str(), b.as_str()];
    assert_eq!(
        outcomes.iter().filter(|o| **o == "set").count(),
        1,
        "exactly one demote may land — got {outcomes:?}"
    );
    assert_eq!(
        confirmed_owners(pool.clone()).await,
        1,
        "a confirmed owner always survives"
    );

    // RACE 2 — the survivor plus a fresh second owner both LEAVE at once.
    let survivor: String = sqlx::query_scalar(
        "SELECT principal FROM workspace_member \
         WHERE workspace_id = $1 AND role = 'owner' AND status = 'confirmed'",
    )
    .bind("w_acme")
    .fetch_one(&pool)
    .await
    .unwrap();
    let other = if survivor == ALICE { BOB } else { ALICE };
    sqlx::query(
        "UPDATE workspace_member SET role = 'owner' WHERE workspace_id = $1 AND principal = $2",
    )
    .bind("w_acme")
    .bind(other)
    .execute(&pool)
    .await
    .unwrap();
    let leave = "SELECT topos_leave_workspace($1, $2, $3::BIGINT, $4)";
    let survivor_binds: [&str; 4] = ["w_acme", &survivor, "1770000000000", CREATED_AT];
    let other_binds: [&str; 4] = ["w_acme", other, "1770000000000", CREATED_AT];
    let (a, b) = tokio::join!(
        fn_outcome(&pool, leave, &survivor_binds),
        fn_outcome(&pool, leave, &other_binds),
    );
    let outcomes = [a.as_str(), b.as_str()];
    assert_eq!(
        outcomes.iter().filter(|o| **o == "left").count(),
        1,
        "exactly one leave may land; the last owner cannot walk out — got {outcomes:?}"
    );
    assert_eq!(
        confirmed_owners(pool).await,
        1,
        "the workspace is never left ownerless"
    );
}

/// `topos_revoke_device`'s SELF-ONLY grade (`p_self_only = 1`): the account-level sign-out page
/// runs a lighter ceremony than the owner's step-up fleet revoke, so its calls must not be able to
/// exercise the OWNER arm of the matrix — even for a caller who happens to be an owner. The reach
/// of the act and the grade of the ceremony stay matched IN the database.
#[sqlx::test]
async fn revoke_device_self_only_refuses_even_an_owner_reaching_for_another_device(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chl-selfonly").await;
    let w = ws("w_acme");
    seat(&fx, &w, "dk_alice", 11, ALICE, "owner").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member").await;
    let sql = "SELECT topos_revoke_device($1, $2, $3, $4::BIGINT)";

    // The OWNER arm is unreachable under the self-only grade — even though the same owner may
    // revoke this very device through the step-up fleet ceremony (the default grade, below).
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", ALICE, "dk_bob", "1"]).await,
        "self_required"
    );
    let revoked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::int8 FROM device_registry WHERE workspace_id = $1 AND revoked = 1",
    )
    .bind("w_acme")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(revoked, 0, "the refusal wrote nothing");

    // Own device under the self-only grade: allowed. And the owner's default (step-up) grade
    // still reaches the same device — the matrix is unchanged where the ceremony earns it.
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", BOB, "dk_bob", "1"]).await,
        "revoked"
    );
    assert_eq!(
        fn_outcome(&pool, sql, &["w_acme", ALICE, "dk_bob", "0"]).await,
        "revoked"
    );
}
