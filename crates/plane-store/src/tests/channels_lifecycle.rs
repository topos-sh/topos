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
use crate::delivery::{Delivery, DeliveredSkill};

const ALICE: &str = "alice@acme.com";
const BOB: &str = "bob@acme.com";

/// Seat a person's device + workspace_member seat.
async fn seat(
    fx: &Fixture,
    w: &WorkspaceId,
    dkid: &str,
    seed: u8,
    principal: &str,
    role: &str,
) {
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
        .publish(w, s, &op(op_id), genesis(files), auth, Some(display_name), None, CREATED_AT, NOW)
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
    let r = sqlx::query("SELECT purged_at, purged_by FROM skill_commit WHERE workspace_id = $1 AND commit_id = $2")
        .bind(w)
        .bind(commit)
        .fetch_one(pool)
        .await
        .unwrap();
    (r.get::<Option<i64>, _>("purged_at"), r.get::<Option<String>, _>("purged_by"))
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
    let expected = fx.authority.db().read_current_generation(w, s).await.unwrap().unwrap();
    let auth = DeviceOpAuth {
        credential: cred(w, dkid),
        op: DeviceOp::PublishDirect,
        expected,
    };
    fx.authority
        .publish(w, s, &op(op_id), child(parent, files), auth, None, None, CREATED_AT, NOW)
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
        .archive_skill_session(&w, ALICE, "deploy", DeploymentMode::Cloud, CREATED_AT, NOW)
        .await
        .unwrap();
    let LifecycleOutcome::Archived { archived_name } = out else {
        panic!("expected Archived, got {out:?}");
    };
    assert_eq!(archived_name, "deploy-archived-2026-06-28", "renamed with the date label");
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
    assert_eq!(catalog(&pool, "w_acme", "s_deploy2").await.unwrap().1, "deploy");

    // The OLD skill is out of delivery; the NEW one is in (via everyone).
    let d = deliver(&fx, &w, "dk_bob").await;
    assert!(find(&d, "s_deploy").is_none(), "the archived skill is out of delivery");
    assert!(find(&d, "s_deploy2").is_some(), "the fresh skill delivers");

    // The proposal is CLOSED (not rejected) with the circumstantial reason, and bob has a notice.
    assert_eq!(
        proposal(&pool, "w_acme", "0f0f0f0f-0000-4000-8000-000000000001").await,
        Some(("closed".to_owned(), Some("skill archived".to_owned())))
    );
    assert!(
        d.notices.iter().any(|n| n.kind == "proposal_closed" && n.outcome.as_deref() == Some("closed")),
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
    gpub(&fx, &w, &skill("s_one"), "dk_owner", "aaaaaaaa-0000-4000-8000-000000000001", vec![file("f", b"1")], "Deploy").await;
    fx.authority
        .archive_skill_session(&w, ALICE, "deploy", DeploymentMode::Cloud, CREATED_AT, NOW)
        .await
        .unwrap();
    // skill 2 claims the freed "deploy", then archives SAME day → the "-2" suffix.
    gpub(&fx, &w, &skill("s_two"), "dk_owner", "aaaaaaaa-0000-4000-8000-000000000002", vec![file("f", b"2")], "Deploy").await;
    let out = fx
        .authority
        .archive_skill_session(&w, ALICE, "deploy", DeploymentMode::Cloud, CREATED_AT, NOW)
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
/// `NotActive`, an unknown name is the uniform `NotFound`, and a self-host plane denies the whole
/// session op (`NotFound`) — the typed reasons a confirmed member is entitled to, and the uniform
/// misses that leak nothing.
#[sqlx::test]
async fn archive_refusals_non_owner_not_active_unknown_and_self_host(pool: PgPool) {
    let fx = Fixture::new(pool, "chl-archive-refuse").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;
    seat(&fx, &w, "dk_mem", 12, BOB, "member").await;
    gpub(&fx, &w, &s, "dk_owner", "aaaaaaaa-0000-4000-8000-000000000001", vec![file("f", b"v0")], "Deploy").await;

    // A confirmed non-owner member gets the typed role refusal.
    assert_eq!(
        fx.authority
            .archive_skill_session(&w, BOB, "deploy", DeploymentMode::Cloud, CREATED_AT, NOW)
            .await
            .unwrap(),
        LifecycleOutcome::OwnerRoleRequired
    );
    // An unknown skill name is the uniform NotFound.
    assert!(matches!(
        fx.authority
            .archive_skill_session(&w, ALICE, "nope", DeploymentMode::Cloud, CREATED_AT, NOW)
            .await,
        Err(AuthorityError::NotFound)
    ));
    // A self-host plane denies the whole session op uniformly.
    assert!(matches!(
        fx.authority
            .archive_skill_session(&w, ALICE, "deploy", DeploymentMode::SelfHost, CREATED_AT, NOW)
            .await,
        Err(AuthorityError::NotFound)
    ));
    // Archiving twice: the second is NotActive.
    assert!(matches!(
        fx.authority
            .archive_skill_session(&w, ALICE, "deploy", DeploymentMode::Cloud, CREATED_AT, NOW)
            .await
            .unwrap(),
        LifecycleOutcome::Archived { .. }
    ));
    assert_eq!(
        fx.authority
            .archive_skill_session(&w, ALICE, "deploy-archived-2026-06-28", DeploymentMode::Cloud, CREATED_AT, NOW)
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
    gpub(&fx, &w, &s, "dk_owner", "aaaaaaaa-0000-4000-8000-000000000001", vec![file("f", b"v0")], "Deploy").await;
    // Both receive it before the archive (ALICE direct+everyone, bob everyone).
    assert!(find(&deliver(&fx, &w, "dk_owner").await, "s_deploy").is_some());
    assert!(find(&deliver(&fx, &w, "dk_bob").await, "s_deploy").is_some());

    fx.authority
        .archive_skill_session(&w, ALICE, "deploy", DeploymentMode::Cloud, CREATED_AT, NOW)
        .await
        .unwrap();
    let out = fx
        .authority
        .unarchive_skill_session(&w, ALICE, "deploy-archived-2026-06-28", DeploymentMode::Cloud)
        .await
        .unwrap();
    assert_eq!(out, LifecycleOutcome::Unarchived { name: "deploy".to_owned() });

    // Placements were NOT restored.
    assert_eq!(placements(&pool, "w_acme", "s_deploy").await, 0, "curation is not restored");
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
    gpub(&fx, &w, &skill("s_old"), "dk_owner", "aaaaaaaa-0000-4000-8000-000000000001", vec![file("f", b"old")], "Deploy").await;
    fx.authority
        .archive_skill_session(&w, ALICE, "deploy", DeploymentMode::Cloud, CREATED_AT, NOW)
        .await
        .unwrap();
    // A fresh identity claims "deploy".
    gpub(&fx, &w, &skill("s_new"), "dk_owner", "aaaaaaaa-0000-4000-8000-000000000002", vec![file("f", b"new")], "Deploy").await;
    // Unarchiving the OLD skill now collides on the base name.
    assert_eq!(
        fx.authority
            .unarchive_skill_session(&w, ALICE, "deploy-archived-2026-06-28", DeploymentMode::Cloud)
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
    gpub(&fx, &w, &s, "dk_owner", "aaaaaaaa-0000-4000-8000-000000000001", vec![file("SKILL.md", body)], "Deploy").await;
    let obj = object_id(body);
    // The object is readable while the skill lives.
    assert!(
        fx.authority.read_object(&prin(ALICE), &w, &s, obj).await.is_ok(),
        "readable before delete"
    );

    // Delete of an ACTIVE skill is refused (archive-first).
    assert_eq!(
        fx.authority
            .delete_skill_session(&w, ALICE, "deploy", DeploymentMode::Cloud, NOW)
            .await
            .unwrap(),
        LifecycleOutcome::NotArchived
    );
    // Archive, then delete.
    fx.authority
        .archive_skill_session(&w, ALICE, "deploy", DeploymentMode::Cloud, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(
        fx.authority
            .delete_skill_session(&w, ALICE, "deploy-archived-2026-06-28", DeploymentMode::Cloud, NOW)
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
        fx.authority.db().read_current_commit(&w, &s).await.unwrap().is_none(),
        "the current pointer is dropped"
    );
    // The object is un-rooted (read_object → NotFound) and the GC reclaims it.
    assert!(matches!(
        fx.authority.read_object(&prin(ALICE), &w, &s, obj).await,
        Err(AuthorityError::NotFound)
    ));
    let reclaimed = fx.authority.run_gc(&w, NOW + 1_000_000).await.unwrap();
    assert!(reclaimed >= 1, "the deleted skill's bytes are reclaimed: {reclaimed}");

    // A publish onto the deleted skill id is DENIED "deleted".
    let r = fx
        .authority
        .publish(
            &w,
            &s,
            &op("aaaaaaaa-0000-4000-8000-000000000009"),
            genesis(vec![file("SKILL.md", b"resurrect")]),
            DeviceOpAuth { credential: cred(&w, "dk_owner"), op: DeviceOp::PublishDirect, expected: gn(0, 0) },
            Some("Deploy"),
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    assert_eq!(
        r.details.as_ref().and_then(|d| d.get("message")).and_then(serde_json::Value::as_str),
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
    let g = gpub(&fx, &w, &s, "dk_owner", "aaaaaaaa-0000-4000-8000-000000000001", vec![file("f", b"v0")], "Deploy").await;
    let c0 = fx.authority.db().read_current_commit(&w, &s).await.unwrap().unwrap();
    fx.authority
        .archive_skill_session(&w, ALICE, "deploy", DeploymentMode::Cloud, CREATED_AT, NOW)
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
            DeviceOpAuth { credential: cred(&w, "dk_owner"), op: DeviceOp::PublishDirect, expected: g.current.unwrap() },
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    assert_eq!(
        r.details.as_ref().and_then(|d| d.get("message")).and_then(serde_json::Value::as_str),
        Some("the skill is archived"),
    );
    // A follow of the archived name is SkillNotActive.
    assert_eq!(
        fx.authority
            .follow_skill(&w, &cred(&w, "dk_bob"), "deploy-archived-2026-06-28", CREATED_AT)
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
    let c1 = fx.authority.db().read_current_commit(&w, &s).await.unwrap().unwrap();
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
            .purge_version_session(&w, ALICE, "deploy", v2_commit, DeploymentMode::Cloud, CREATED_AT, NOW)
            .await
            .unwrap(),
        PurgeOutcome::IsCurrent
    );
    // Purge v1: un-rooted + tombstoned.
    assert_eq!(
        fx.authority
            .purge_version_session(&w, ALICE, "deploy", v1_commit, DeploymentMode::Cloud, CREATED_AT, NOW)
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
            fx.authority.read_object(&prin(ALICE), &w, &s, obj_secret).await,
            Err(AuthorityError::NotFound)
        ),
        "v1's unique blob is unreadable after the purge + GC"
    );
    assert_eq!(
        fx.authority.read_object(&prin(ALICE), &w, &s, obj_shared).await.unwrap(),
        shared.to_vec(),
        "the shared blob stays readable via v2"
    );
    // v1's version-metadata read is now the uniform NotFound (un-rooted ⇒ not reachable).
    assert!(matches!(
        fx.authority
            .read_version_metadata_session(&w, "deploy", &digest::to_hex(&v1_commit.0), ALICE, DeploymentMode::Cloud)
            .await,
        Err(AuthorityError::NotFound)
    ));

    // The dependent proposal is closed with the circumstantial reason + a notice to bob.
    assert_eq!(
        proposal(&pool, "w_acme", "0f0f0f0f-0000-4000-8000-000000000002").await,
        Some(("closed".to_owned(), Some("a version it rests on was purged".to_owned())))
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
            .purge_version_session(&w, ALICE, "deploy", v1_commit, DeploymentMode::Cloud, CREATED_AT, NOW)
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
            let c1 = fx.authority.db().read_current_commit(&w, s).await.unwrap().unwrap();
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
    let a_current = fx.authority.db().read_current_generation(&w, &sa).await.unwrap().unwrap();
    let mr = fx
        .authority
        .revert(
            &w,
            &sa,
            a_genesis,
            DeviceOpAuth { credential: cred(&w, "dk_mem"), op: DeviceOp::Revert, expected: a_current },
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
        mr.details.as_ref().and_then(|d| d.get("downgraded")).and_then(serde_json::Value::as_bool),
        Some(true),
        "a member's revert downgrades to a proposal"
    );
    assert_eq!(
        fx.authority.db().read_current_generation(&w, &sa).await.unwrap(),
        Some(a_current),
        "the pointer is frozen"
    );

    // A reviewer's revert on skill B LANDS (the pointer advances).
    let b_current = fx.authority.db().read_current_generation(&w, &sb).await.unwrap().unwrap();
    let rr = fx
        .authority
        .revert(
            &w,
            &sb,
            b_genesis,
            DeviceOpAuth { credential: cred(&w, "dk_rev"), op: DeviceOp::Revert, expected: b_current },
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
        fx.authority.db().read_current_generation(&w, &sb).await.unwrap().unwrap().seq
            > b_current.seq,
        "the pointer advanced"
    );
}
