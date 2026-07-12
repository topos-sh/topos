//! The verb-surface directory reads + the two member-lane guarded writes: the channels index, the
//! caller's membership, the review inbox, a skill's log, a skill's reach, the notices ack, and the
//! roster invite — plus the delivery read's staleness window + the N+1 → aggregate proposal fold.
//!
//! These drive the ops directly through `Authority` against a real Postgres + git store (no HTTP). The
//! front door is the ONE membership predicate the channel ops run, so every pre-gate miss is the
//! uniform `NotFound`; past it the reads disclose the member-entitled facts and the two writes route
//! through their guarded `topos_*` functions.

use super::*;

use crate::describe::InviteOutcome;

const ALICE: &str = "alice@acme.com";
const BOB: &str = "bob@acme.com";

// ── local seeding + driving helpers ─────────────────────────────────────────────────────────────────

/// Seat a person's device (holding its `(ws, dkid)` credential) + their `workspace_member` seat.
async fn seat(
    fx: &Fixture,
    w: &WorkspaceId,
    dkid: &str,
    seed: u8,
    principal: &str,
    role: &str,
    status: &str,
) {
    let p = prin(principal);
    fx.authority
        .db()
        .seed_device(w, dkid, &dev_key(seed), &p, false, &cred(w, dkid))
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_workspace_member(w, &p, role, status)
        .await
        .unwrap();
}

/// Genesis-publish `skill` (minting the catalog name from `display_name`, placing it into `channel`
/// else `everyone`) as the device presents it — the real pointer-move.
#[allow(clippy::too_many_arguments)]
async fn gpub(
    fx: &Fixture,
    w: &WorkspaceId,
    s: &SkillId,
    dkid: &str,
    op_id: &str,
    files: Vec<UploadedFile>,
    display_name: Option<&str>,
    channel: Option<&str>,
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
            display_name,
            channel,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap()
}

/// A `workspace` row WITH its address slug (the migration's backfill wrote one for every existing row;
/// `seed_workspace` predates the column, so the describe tests seed it directly).
async fn seed_ws_named(pool: &PgPool, w: &str, name: &str, display: &str) {
    sqlx::query(
        "INSERT INTO workspace (workspace_id, name, display_name, verified_domain_status, deployment_mode, created_at) \
         VALUES ($1, $2, $3, 'unverified', 'cloud', 'seed') \
         ON CONFLICT (workspace_id) DO UPDATE SET name = excluded.name, display_name = excluded.display_name",
    )
    .bind(w)
    .bind(name)
    .bind(display)
    .execute(pool)
    .await
    .unwrap();
}

/// Seed a plain (non-builtin) channel row.
async fn seed_channel(pool: &PgPool, w: &str, channel_id: &str, name: &str) {
    sqlx::query(
        "INSERT INTO channels (workspace_id, channel_id, name, mode, builtin, created_at) \
         VALUES ($1, $2, $3, 'open', 0, 'seed')",
    )
    .bind(w)
    .bind(channel_id)
    .bind(name)
    .execute(pool)
    .await
    .unwrap();
}

/// Ensure the structural `everyone` channel exists (its own guarded creator).
async fn ensure_everyone(pool: &PgPool, w: &str) {
    sqlx::query("SELECT topos_ensure_everyone($1, 'seed')")
        .bind(w)
        .execute(pool)
        .await
        .unwrap();
}

/// Seed an unacked person-scoped notice.
async fn seed_notice(pool: &PgPool, w: &str, id: &str, principal: &str, kind: &str) {
    sqlx::query(
        "INSERT INTO notices (workspace_id, id, principal, kind, created_at) VALUES ($1, $2, $3, $4, 'seed')",
    )
    .bind(w)
    .bind(id)
    .bind(principal)
    .bind(kind)
    .execute(pool)
    .await
    .unwrap();
}

/// One notice's `acked_at`.
async fn notice_acked_at(pool: &PgPool, w: &str, id: &str) -> Option<i64> {
    use sqlx::Row as _;
    sqlx::query("SELECT acked_at FROM notices WHERE workspace_id = $1 AND id = $2")
        .bind(w)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
        .get::<Option<i64>, _>("acked_at")
}

/// One workspace_member row `(role, status, invited_by)`.
async fn member_row(
    pool: &PgPool,
    w: &str,
    principal: &str,
) -> Option<(String, String, Option<String>)> {
    use sqlx::Row as _;
    sqlx::query("SELECT role, status, invited_by FROM workspace_member WHERE workspace_id = $1 AND principal = $2")
        .bind(w)
        .bind(principal)
        .fetch_optional(pool)
        .await
        .unwrap()
        .map(|r| {
            (
                r.get::<String, _>("role"),
                r.get::<String, _>("status"),
                r.get::<Option<String>, _>("invited_by"),
            )
        })
}

/// Whether a channel_members row exists for `(channel_id, principal)`.
async fn channel_member_exists(pool: &PgPool, w: &str, channel_id: &str, principal: &str) -> bool {
    use sqlx::Row as _;
    sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM channel_members WHERE workspace_id = $1 AND channel_id = $2 AND principal = $3) AS present",
    )
    .bind(w)
    .bind(channel_id)
    .bind(principal)
    .fetch_one(pool)
    .await
    .unwrap()
    .get::<bool, _>("present")
}

/// Set the workspace invite policy via its guarded setter (owner acts); returns the outcome code.
async fn set_invite_policy(pool: &PgPool, w: &str, actor: &str, policy: &str) -> String {
    use sqlx::Row as _;
    sqlx::query("SELECT topos_set_invite_policy($1, $2, $3) AS outcome")
        .bind(w)
        .bind(actor)
        .bind(policy)
        .fetch_one(pool)
        .await
        .unwrap()
        .get::<String, _>("outcome")
}

/// Set the workspace staleness window via its guarded setter (owner acts); returns the outcome code.
async fn set_staleness(pool: &PgPool, w: &str, actor: &str, ms: i64) -> String {
    use sqlx::Row as _;
    sqlx::query("SELECT topos_set_staleness_window($1, $2, $3) AS outcome")
        .bind(w)
        .bind(actor)
        .bind(ms)
        .fetch_one(pool)
        .await
        .unwrap()
        .get::<String, _>("outcome")
}

// ── ack_notices ──────────────────────────────────────────────────────────────────────────────────────

/// A confirmed member acks their own notice (idempotent — a second ack is a no-op), and an unknown
/// credential (a non-member) reads the uniform `NotFound`.
#[sqlx::test]
async fn ack_notices_is_idempotent_and_non_member_is_notfound(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "vsd-ack").await;
    let w = ws("w_acme");
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    seed_notice(&pool, "w_acme", "n1", BOB, "verdict").await;
    let bob = cred(&w, "dk_bob");

    fx.authority
        .ack_notices(&w, &bob, &["n1".to_owned()], NOW)
        .await
        .unwrap();
    assert_eq!(notice_acked_at(&pool, "w_acme", "n1").await, Some(NOW));

    // A second ack changes nothing (only unacked rows move) — still Ok, still the first timestamp.
    fx.authority
        .ack_notices(&w, &bob, &["n1".to_owned()], NOW + 100)
        .await
        .unwrap();
    assert_eq!(
        notice_acked_at(&pool, "w_acme", "n1").await,
        Some(NOW),
        "an already-acked notice is not re-stamped"
    );

    // An unknown credential is the uniform miss.
    assert!(matches!(
        fx.authority
            .ack_notices(&w, &cred(&w, "dk_ghost"), &["n1".to_owned()], NOW)
            .await,
        Err(AuthorityError::NotFound)
    ));
}

// ── invite ─────────────────────────────────────────────────────────────────────────────────────────

/// Under the default `members` policy a plain member seats invited rows (recording `invited_by`),
/// pre-places into a named channel, and a re-invite of a CONFIRMED member never demotes them.
#[sqlx::test]
async fn invite_under_members_policy_seats_records_and_preplaces(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "vsd-invite-members").await;
    let w = ws("w_acme");
    seat(&fx, &w, "dk_alice", 11, ALICE, "member", "confirmed").await;
    // A confirmed member who must never be demoted by a re-invite.
    seat(
        &fx,
        &w,
        "dk_carol",
        13,
        "carol@acme.com",
        "reviewer",
        "confirmed",
    )
    .await;
    seed_channel(&pool, "w_acme", "ops", "ops").await;
    let alice = cred(&w, "dk_alice");

    // Seat a fresh email + pre-place into `ops`; the folded email is returned.
    let out = fx
        .authority
        .invite(
            &w,
            &alice,
            &[BOB.to_owned()],
            &["ops".to_owned()],
            CREATED_AT,
        )
        .await
        .unwrap();
    assert_eq!(
        out,
        InviteOutcome::Invited {
            invited: vec![BOB.to_owned()]
        }
    );
    assert_eq!(
        member_row(&pool, "w_acme", BOB).await,
        Some((
            "member".to_owned(),
            "invited".to_owned(),
            Some(ALICE.to_owned())
        )),
        "an invited member seat records role=member status=invited invited_by=actor"
    );
    assert!(
        channel_member_exists(&pool, "w_acme", "ops", BOB).await,
        "the channel pre-placement landed"
    );

    // Re-inviting a CONFIRMED reviewer never demotes.
    fx.authority
        .invite(&w, &alice, &["carol@acme.com".to_owned()], &[], CREATED_AT)
        .await
        .unwrap();
    let (role, status, _) = member_row(&pool, "w_acme", "carol@acme.com").await.unwrap();
    assert_eq!(
        (role.as_str(), status.as_str()),
        ("reviewer", "confirmed"),
        "a re-invite never demotes a confirmed seat"
    );
}

/// The `owners` policy gates inviting: a plain member is refused `OwnerRoleRequired` while an owner's
/// invite lands.
#[sqlx::test]
async fn invite_under_owners_policy_gates_by_role(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "vsd-invite-owners").await;
    let w = ws("w_acme");
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner", "confirmed").await;
    seat(&fx, &w, "dk_mem", 12, BOB, "member", "confirmed").await;
    assert_eq!(
        set_invite_policy(&pool, "w_acme", ALICE, "owners").await,
        "set"
    );

    // A plain member is now refused (typed — an authenticated member is entitled to the reason).
    assert_eq!(
        fx.authority
            .invite(
                &w,
                &cred(&w, "dk_mem"),
                &["x@acme.com".to_owned()],
                &[],
                CREATED_AT
            )
            .await
            .unwrap(),
        InviteOutcome::OwnerRoleRequired
    );
    assert!(
        member_row(&pool, "w_acme", "x@acme.com").await.is_none(),
        "a refused invite writes nothing"
    );
    // The owner's invite lands.
    assert_eq!(
        fx.authority
            .invite(
                &w,
                &cred(&w, "dk_owner"),
                &["x@acme.com".to_owned()],
                &[],
                CREATED_AT
            )
            .await
            .unwrap(),
        InviteOutcome::Invited {
            invited: vec!["x@acme.com".to_owned()]
        }
    );
}

/// An unknown channel refuses the WHOLE invite (resolve-all-or-apply-none) — nothing is written; and
/// pre-placing into the structural `everyone` succeeds as a silent no-op (its membership IS the
/// roster).
#[sqlx::test]
async fn invite_unknown_channel_is_all_or_none_and_everyone_is_a_noop(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "vsd-invite-channels").await;
    let w = ws("w_acme");
    seat(&fx, &w, "dk_alice", 11, ALICE, "member", "confirmed").await;
    ensure_everyone(&pool, "w_acme").await;
    let alice = cred(&w, "dk_alice");

    // Unknown channel ⇒ refused, and the email is NOT seated (the channels resolve before any write).
    assert_eq!(
        fx.authority
            .invite(
                &w,
                &alice,
                &[BOB.to_owned()],
                &["ghostchan".to_owned()],
                CREATED_AT
            )
            .await
            .unwrap(),
        InviteOutcome::UnknownChannel
    );
    assert!(
        member_row(&pool, "w_acme", BOB).await.is_none(),
        "an all-or-none refusal leaves no seat behind"
    );

    // Pre-placing into `everyone` seats the member but writes no channel_members row (roster-derived).
    assert_eq!(
        fx.authority
            .invite(
                &w,
                &alice,
                &[BOB.to_owned()],
                &["everyone".to_owned()],
                CREATED_AT
            )
            .await
            .unwrap(),
        InviteOutcome::Invited {
            invited: vec![BOB.to_owned()]
        }
    );
    assert!(
        member_row(&pool, "w_acme", BOB).await.is_some(),
        "the seat lands"
    );
    assert!(
        !channel_member_exists(&pool, "w_acme", "everyone", BOB).await,
        "everyone is structural — no explicit membership row"
    );
}

/// A malformed email is a typed argument error (`InvalidId`), never a silent drop or the uniform miss.
#[sqlx::test]
async fn invite_rejects_a_malformed_email(pool: PgPool) {
    let fx = Fixture::new(pool, "vsd-invite-badmail").await;
    let w = ws("w_acme");
    seat(&fx, &w, "dk_alice", 11, ALICE, "member", "confirmed").await;
    assert!(matches!(
        fx.authority
            .invite(
                &w,
                &cred(&w, "dk_alice"),
                &["not a valid email".to_owned()],
                &[],
                CREATED_AT
            )
            .await,
        Err(AuthorityError::InvalidId(_))
    ));
}

// ── membership_describe ──────────────────────────────────────────────────────────────────────────────

/// `me` returns the workspace identity + its share address (`<link_base>/<name>`), the caller's seat,
/// and the invite policy.
#[sqlx::test]
async fn membership_describe_returns_the_address_and_seat(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "vsd-me").await;
    let w = ws("w_acme");
    seed_ws_named(&pool, "w_acme", "acme-team", "Acme Team").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;

    let me = fx
        .authority
        .membership_describe(&w, &cred(&w, "dk_bob"))
        .await
        .unwrap();
    assert_eq!(me.name, "acme-team");
    assert_eq!(me.display_name, "Acme Team");
    // The address is the link base (the fixture's `base_url`) joined with the slug.
    assert_eq!(me.address, "https://plane.test/acme-team");
    assert_eq!(me.principal, BOB);
    assert_eq!(me.role, "member");
    assert_eq!(me.invited_by, None);
    assert_eq!(me.invite_policy, "members", "the default policy");

    // A non-member is the uniform miss.
    assert!(matches!(
        fx.authority
            .membership_describe(&w, &cred(&w, "dk_ghost"))
            .await,
        Err(AuthorityError::NotFound)
    ));
}

// ── channels_index ─────────────────────────────────────────────────────────────────────────────────

/// The channels index shows the structural `everyone` (member=true, the confirmed-roster count, the
/// placed skills) alongside a joined ordinary channel with its own membership + count.
#[sqlx::test]
async fn channels_index_shows_everyone_and_a_joined_channel(pool: PgPool) {
    let fx = Fixture::new(pool, "vsd-channels-index").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_alice", 11, ALICE, "owner", "confirmed").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    gpub(
        &fx,
        &w,
        &s,
        "dk_alice",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        Some("Deploy"),
        None,
    )
    .await;
    let alice = cred(&w, "dk_alice");
    let bob = cred(&w, "dk_bob");
    // alice creates `ops` (placing deploy into it); bob joins.
    fx.authority
        .channel_place(&w, &alice, "ops", s.as_str(), CREATED_AT)
        .await
        .unwrap();
    fx.authority
        .channel_join(&w, &bob, "ops", CREATED_AT)
        .await
        .unwrap();

    let idx = fx.authority.channels_index(&w, &bob).await.unwrap();
    let names: Vec<&str> = idx.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["everyone", "ops"],
        "channels name-sorted, everyone included"
    );

    let everyone = &idx[0];
    assert!(everyone.builtin);
    assert!(everyone.member, "membership IS the roster for everyone");
    assert_eq!(everyone.member_count, 2, "both confirmed members");
    assert_eq!(
        everyone
            .skills
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        vec!["deploy"],
        "the genesis skill lives in everyone"
    );

    let ops = &idx[1];
    assert!(!ops.builtin);
    assert!(ops.member, "bob joined ops");
    assert_eq!(ops.member_count, 1, "only bob is a channel_members row");
    assert_eq!(
        ops.skills
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        vec!["deploy"]
    );
    // A skill reference carries the immutable id + the catalog name.
    assert_eq!(ops.skills[0].skill_id, "s_deploy");

    // A non-member reads the uniform miss.
    assert!(matches!(
        fx.authority.channels_index(&w, &cred(&w, "dk_ghost")).await,
        Err(AuthorityError::NotFound)
    ));
}

// ── proposals_index ──────────────────────────────────────────────────────────────────────────────────

/// The review inbox carries the proposed version's commit message and flips `stale` the moment
/// `current` moves past the proposal's base.
#[sqlx::test]
async fn proposals_index_carries_the_message_and_flips_stale(pool: PgPool) {
    let fx = Fixture::new(pool, "vsd-proposals-index").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_alice", 11, ALICE, "owner", "confirmed").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    gpub(
        &fx,
        &w,
        &s,
        "dk_alice",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        Some("Deploy"),
        None,
    )
    .await;

    // bob proposes a candidate with a distinctive message, based on current (1,1).
    let parent = current_commit(&fx, &w, &s).await;
    let candidate = CandidateUpload {
        files: vec![file("SKILL.md", b"v1-fix")],
        parents: vec![parent],
        author: "d_bob".to_owned(),
        message: "fix the deploy step".to_owned(),
    };
    let (_r, commit, _digest) = do_propose(
        &fx,
        &dev_key(12),
        "dk_bob",
        &w,
        &s,
        "bbbbbbbb-0000-4000-8000-000000000001",
        candidate,
        gn(1, 1),
    )
    .await;

    let idx = fx
        .authority
        .proposals_index(&w, &cred(&w, "dk_bob"))
        .await
        .unwrap();
    assert_eq!(idx.len(), 1);
    let e = &idx[0];
    assert_eq!(e.skill_name, "deploy");
    assert_eq!(e.skill_id, "s_deploy");
    assert_eq!(e.version_id, commit.0);
    assert_eq!(e.proposer, BOB);
    assert!(
        e.message.contains("fix the deploy step"),
        "the proposed version's commit message is read from the store: {:?}",
        e.message
    );
    assert!(!e.stale, "non-stale while current is still the base");

    // Move current past the base ⇒ the proposal stales.
    fx.authority
        .db()
        .force_current_generation(&w, &s, 1, 2)
        .await
        .unwrap();
    let idx = fx
        .authority
        .proposals_index(&w, &cred(&w, "dk_bob"))
        .await
        .unwrap();
    assert!(idx[0].stale, "the base no longer equals current ⇒ stale");
}

// ── skill_log ──────────────────────────────────────────────────────────────────────────────────────

/// `log <skill>` lists a purged version's who/when tombstone and marks the current pointer.
#[sqlx::test]
async fn skill_log_lists_a_purged_tombstone_and_marks_current(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "vsd-log-purged").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    fx.authority
        .db()
        .seed_catalog(&w, &s, "deploy")
        .await
        .unwrap();
    let (cv1, cv2) = (CommitId([0xA1; 32]), CommitId([0xA2; 32]));
    fx.authority
        .db()
        .seed_commit(&w, &s, cv1, &[])
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_commit(&w, &s, cv2, &[])
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_current(&w, &s, cv2, 1, 1)
        .await
        .unwrap();
    // Purge cv1 (who/when tombstone on the provenance row).
    sqlx::query("UPDATE skill_commit SET purged_at = 500, purged_by = $3 WHERE workspace_id = $1 AND commit_id = $2")
        .bind("w_acme")
        .bind(cv1.0.as_slice())
        .bind(ALICE)
        .execute(&pool)
        .await
        .unwrap();

    let log = fx
        .authority
        .skill_log(&w, &cred(&w, "dk_bob"), "deploy")
        .await
        .unwrap();
    assert_eq!(log.skill_id, "s_deploy");
    assert_eq!(log.name, "deploy");
    assert_eq!(log.status, "active");
    let v1 = log
        .versions
        .iter()
        .find(|v| v.version_id == cv1.0)
        .expect("cv1 listed");
    assert_eq!(v1.purged_at, Some(500));
    assert_eq!(v1.purged_by.as_deref(), Some(ALICE));
    assert!(!v1.current);
    let v2 = log
        .versions
        .iter()
        .find(|v| v.version_id == cv2.0)
        .expect("cv2 listed");
    assert!(v2.current, "cv2 is the current pointer");
    assert_eq!(v2.purged_at, None);
}

/// `log <old-name>` resolves an archived skill by its FREED base name (the archived-successor hint).
#[sqlx::test]
async fn skill_log_resolves_an_archived_skill_by_its_freed_base_name(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "vsd-log-archived").await;
    let w = ws("w_acme");
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    // An archived catalog row whose name was suffixed and whose freed base name is `deploy`.
    sqlx::query(
        "INSERT INTO catalog (workspace_id, skill_id, name, status, base_name, archived_at, created_at) \
         VALUES ($1, 's_old', 'deploy-archived-2026-07-12', 'archived', 'deploy', 0, 'seed')",
    )
    .bind("w_acme")
    .execute(&pool)
    .await
    .unwrap();

    let log = fx
        .authority
        .skill_log(&w, &cred(&w, "dk_bob"), "deploy")
        .await
        .unwrap();
    assert_eq!(log.skill_id, "s_old", "resolved via the freed base name");
    assert_eq!(log.name, "deploy-archived-2026-07-12");
    assert_eq!(log.status, "archived");
    assert_eq!(log.base_name.as_deref(), Some("deploy"));

    // An entirely unknown name is the uniform miss.
    assert!(matches!(
        fx.authority
            .skill_log(&w, &cred(&w, "dk_bob"), "nope")
            .await,
        Err(AuthorityError::NotFound)
    ));
}

// ── reach ────────────────────────────────────────────────────────────────────────────────────────────

/// `reach <skill>` counts the confirmed members entitled to the skill and their non-revoked devices;
/// an unfollow subtracts the person (and their devices).
#[sqlx::test]
async fn reach_counts_persons_and_devices_with_an_unfollow_subtracted(pool: PgPool) {
    let fx = Fixture::new(pool, "vsd-reach").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_alice", 11, ALICE, "owner", "confirmed").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    seat(
        &fx,
        &w,
        "dk_carol",
        13,
        "carol@acme.com",
        "member",
        "confirmed",
    )
    .await;
    // A second device for carol, plus a revoked one that must not count.
    fx.authority
        .db()
        .seed_device(
            &w,
            "dk_carol2",
            &dev_key(23),
            &prin("carol@acme.com"),
            false,
            &cred(&w, "dk_carol2"),
        )
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_device(
            &w,
            "dk_bob_revoked",
            &dev_key(24),
            &prin(BOB),
            true,
            &cred(&w, "dk_bob_revoked"),
        )
        .await
        .unwrap();
    // deploy is born in everyone ⇒ every confirmed member is entitled.
    gpub(
        &fx,
        &w,
        &s,
        "dk_alice",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        Some("Deploy"),
        None,
    )
    .await;

    // 3 persons (alice, bob, carol); devices = alice(1) + bob(1, the revoked excluded) + carol(2) = 4.
    let r = fx
        .authority
        .reach(&w, &cred(&w, "dk_alice"), s.as_str())
        .await
        .unwrap();
    assert_eq!(r.persons, 3);
    assert_eq!(r.devices, 4, "the revoked device is excluded");

    // alice unfollows ⇒ she (and her device) drop out of reach.
    fx.authority
        .unfollow_skill(&w, &cred(&w, "dk_alice"), s.as_str(), NOW, CREATED_AT)
        .await
        .unwrap();
    let r = fx
        .authority
        .reach(&w, &cred(&w, "dk_alice"), s.as_str())
        .await
        .unwrap();
    assert_eq!(r.persons, 2, "alice unfollowed");
    assert_eq!(r.devices, 3, "and her device is gone");

    // An unknown skill name is the uniform miss.
    assert!(matches!(
        fx.authority.reach(&w, &cred(&w, "dk_alice"), "nope").await,
        Err(AuthorityError::NotFound)
    ));
}

// ── delivery: staleness window + the N+1 → aggregate proposal fold ─────────────────────────────────────

/// The delivery response carries the workspace staleness window (default, then a set value) and its
/// `proposals_awaiting` aggregate equals the OLD per-skill `open_proposal_rows` sum (the N+1 fold
/// parity) across a two-skill seeded case.
#[sqlx::test]
async fn delivery_carries_staleness_and_folds_the_proposal_count(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "vsd-delivery-fold").await;
    let (w, s1, s2) = (ws("w_acme"), skill("s_alpha"), skill("s_beacon"));
    seat(&fx, &w, "dk_alice", 11, ALICE, "owner", "confirmed").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    // Two real skills in everyone (bob is entitled to both).
    gpub(
        &fx,
        &w,
        &s1,
        "dk_alice",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"alpha")],
        Some("Alpha"),
        None,
    )
    .await;
    gpub(
        &fx,
        &w,
        &s2,
        "dk_alice",
        "bbbbbbbb-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"beacon")],
        Some("Beacon"),
        None,
    )
    .await;

    // Default window, no proposals yet.
    let d = fx
        .authority
        .delivery(&w, &cred(&w, "dk_bob"))
        .await
        .unwrap();
    assert_eq!(
        d.staleness_window_ms, 604_800_000,
        "the default one-week window"
    );
    assert_eq!(d.proposals_awaiting, 0);

    // Open one proposal on each skill (bob proposes off each current).
    for (i, s) in [(1u8, &s1), (2u8, &s2)] {
        let parent = current_commit(&fx, &w, s).await;
        let candidate = CandidateUpload {
            files: vec![file("SKILL.md", b"vX")],
            parents: vec![parent],
            author: "d_bob".to_owned(),
            message: "propose".to_owned(),
        };
        do_propose(
            &fx,
            &dev_key(12),
            "dk_bob",
            &w,
            s,
            &format!("cccccccc-0000-4000-8000-00000000000{i}"),
            candidate,
            gn(1, 1),
        )
        .await;
    }

    // The aggregate equals the OLD per-skill sum over the entitled skills (the N+1 fold parity).
    let old_sum = fx
        .authority
        .db()
        .open_proposal_rows(&w, &s1)
        .await
        .unwrap()
        .len()
        + fx.authority
            .db()
            .open_proposal_rows(&w, &s2)
            .await
            .unwrap()
            .len();
    assert_eq!(old_sum, 2, "one open non-stale proposal per skill");
    let d = fx
        .authority
        .delivery(&w, &cred(&w, "dk_bob"))
        .await
        .unwrap();
    assert_eq!(
        d.proposals_awaiting as usize, old_sum,
        "the folded aggregate matches the per-skill sum"
    );

    // The owner sets a shorter window; delivery reflects it.
    assert_eq!(
        set_staleness(&pool, "w_acme", ALICE, 3_600_000).await,
        "set"
    );
    let d = fx
        .authority
        .delivery(&w, &cred(&w, "dk_bob"))
        .await
        .unwrap();
    assert_eq!(d.staleness_window_ms, 3_600_000);
}

/// The device lane keys on the immutable skill ID, never the mutable catalog NAME: the client always
/// resolved a resource address to its id, and a rename must never break a pending op. Passing the NAME
/// where the id belongs is the same uniform miss any unknown token is. (A regression guard for the
/// name!=id case the slug-clean suites cannot catch.)
#[sqlx::test]
async fn device_lane_ops_key_on_the_skill_id_not_the_catalog_name(pool: PgPool) {
    use crate::channels::{ProtectKind, ProtectLevel};
    let fx = Fixture::new(pool.clone(), "vsd-id-not-name").await;
    let w = ws("w_acme");
    seat(&fx, &w, "dk_alice", 11, ALICE, "owner", "confirmed").await;
    // Genesis a skill whose id (`s_deploy`) DIFFERS from its catalog name (`deploy`, folded from the
    // display name) — the production shape the slug-clean suites deliberately avoid.
    let s = skill("s_deploy");
    gpub(
        &fx,
        &w,
        &s,
        "dk_alice",
        "d1000000-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v1")],
        Some("Deploy"),
        None,
    )
    .await;
    let alice = cred(&w, "dk_alice");

    // follow_skill by the ID works; by the NAME it is the uniform NotFound.
    assert_eq!(
        fx.authority
            .follow_skill(&w, &alice, "s_deploy", CREATED_AT)
            .await
            .unwrap(),
        crate::channels::SubscriptionOutcome::Followed,
    );
    assert!(matches!(
        fx.authority
            .follow_skill(&w, &alice, "deploy", CREATED_AT)
            .await,
        Err(AuthorityError::NotFound),
    ));

    // channel_place likewise keys on the id.
    assert!(matches!(
        fx.authority
            .channel_place(&w, &alice, "ops", "deploy", CREATED_AT)
            .await,
        Err(AuthorityError::NotFound),
    ));
    assert_eq!(
        fx.authority
            .channel_place(&w, &alice, "ops", "s_deploy", CREATED_AT)
            .await
            .unwrap(),
        crate::channels::CurationOutcome::Created,
    );

    // protect (skill kind) and reach both take the id.
    assert!(matches!(
        fx.authority
            .protect(
                &w,
                &alice,
                ProtectKind::Skill,
                "deploy",
                ProtectLevel::Protected,
                CREATED_AT
            )
            .await,
        Err(AuthorityError::NotFound),
    ));
    assert_eq!(
        fx.authority
            .protect(
                &w,
                &alice,
                ProtectKind::Skill,
                "s_deploy",
                ProtectLevel::Protected,
                CREATED_AT
            )
            .await
            .unwrap(),
        crate::channels::ProtectOutcome::Set,
    );
    assert!(matches!(
        fx.authority.reach(&w, &alice, "deploy").await,
        Err(AuthorityError::NotFound),
    ));
    // The author self-follows at genesis, so the audience is at least one.
    assert!(
        fx.authority
            .reach(&w, &alice, "s_deploy")
            .await
            .unwrap()
            .persons
            >= 1
    );
}
