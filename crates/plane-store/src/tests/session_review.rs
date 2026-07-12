//! The web-session REVIEW leg — browser-side approve / reject + the proposal-detail read.
//!
//! The release-blocker witnesses for the session-authorized review ops: the acting gate (ONE uniform,
//! never-persisted denial for a stranger / an invited seat / an unknown workspace / a self-host plane;
//! a DURABLE typed role denial for a confirmed plain member — the first enforcement of the reviewer
//! seat), the shared write body across lanes (the same CAS `CONFLICT`, the same ABA trap, four-eyes
//! over canonical principals with the proposer on the DEVICE lane and the approver on the SESSION
//! lane), byte-identical replay under the reason-inclusive request identity (a re-worded reject reason
//! fails closed as key reuse), the cross-lane op-id closure in BOTH directions (with the per-device
//! slot semantics preserved bit-for-bit), the reject resolution columns, the
//! synthesized-never-durable session pre-transaction misses, and the detail read's open-row preference.

use super::*;

const CLOUD: DeploymentMode = DeploymentMode::Cloud;

/// Seat a CONFIRMED workspace member (the session lane's entitlement; the ROLE decides the in-txn
/// review gate — deliberately no per-skill `roster` row anywhere in this suite unless a test says so).
async fn seat(fx: &Fixture, w: &WorkspaceId, email: &str, role: &str) {
    fx.authority
        .db()
        .seed_workspace_member(w, &prin(email), role, "confirmed")
        .await
        .unwrap();
}

/// Stand a workspace up to the review surface's standard shape: a genesis publish landing `current` at
/// `(1, 1)`, then a device-proposed child at base `(1, 1)`. The caller must have `register`ed the `dk`
/// device first; its principal is the proposal's proposer. Returns `(genesis, candidate, digest)`.
async fn open_proposal(
    fx: &Fixture,
    w: &WorkspaceId,
    s: &SkillId,
    key: &[u8; 32],
    op_publish: &str,
    op_propose: &str,
) -> (CommitId, CommitId, [u8; 32]) {
    publish(
        fx,
        key,
        "dk",
        w,
        s,
        op_publish,
        genesis(vec![file("SKILL.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(fx, w, s).await;
    let (r, cp, digest) = do_propose(
        fx,
        key,
        "dk",
        w,
        s,
        op_propose,
        child(
            g,
            vec![
                file("SKILL.md", b"v0"),
                file("NEW.md", b"proposed reference"),
            ],
        ),
        gn(1, 1),
    )
    .await;
    assert_eq!(r.outcome, TerminalOutcome::NeedsReview);
    (g, cp, digest)
}

/// A session approve on the CLOUD posture (the common case), panicking only on a store fault — every
/// protocol outcome comes back as the receipt.
async fn approve_session(
    fx: &Fixture,
    w: &WorkspaceId,
    s: &SkillId,
    candidate: CommitId,
    expected: Generation,
    rid: &str,
    email: &str,
) -> crate::SetCurrentReceipt {
    fx.authority
        .review_approve_session(
            w, s, candidate, expected, rid, email, CLOUD, CREATED_AT, NOW,
        )
        .await
        .unwrap()
}

/// A session reject on the CLOUD posture (`expected` is the proposal's base — reject moves no pointer).
#[allow(clippy::too_many_arguments)]
async fn reject_session(
    fx: &Fixture,
    w: &WorkspaceId,
    s: &SkillId,
    candidate: CommitId,
    expected: Generation,
    reason: &str,
    rid: &str,
    email: &str,
) -> crate::SetCurrentReceipt {
    fx.authority
        .review_reject_session(
            w, s, candidate, expected, reason, rid, email, CLOUD, CREATED_AT,
        )
        .await
        .unwrap()
}

/// The receipt's machine-branchable `details.code`.
fn code_of(r: &crate::SetCurrentReceipt) -> Option<String> {
    r.details.as_ref()?.get("code")?.as_str().map(str::to_owned)
}

/// The receipt's human-readable `details.message`.
fn msg_of(r: &crate::SetCurrentReceipt) -> Option<String> {
    r.details
        .as_ref()?
        .get("message")?
        .as_str()
        .map(str::to_owned)
}

/// The DURABLE receipt rows one `(ws, op id)` slot holds, as `(actor, method, request_sha256, outcome)`
/// — the recording-rule witness (an empty vec proves an outcome was synthesized, never persisted).
async fn receipts_for(
    pool: &PgPool,
    ws: &str,
    op_id: &str,
) -> Vec<(String, String, Option<Vec<u8>>, String)> {
    sqlx::query_as::<_, (String, String, Option<Vec<u8>>, String)>(
        "SELECT actor, method, request_sha256, outcome FROM op_receipts \
         WHERE workspace_id = $1 AND op_id = $2 ORDER BY actor",
    )
    .bind(ws)
    .bind(op_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A proposal row's stored facts for `(candidate, base)`, straight off the pool:
/// `(status, resolved_by, resolved_reason, resolved_at)`.
async fn resolution_of(
    pool: &PgPool,
    ws: &str,
    commit: CommitId,
    base: Generation,
) -> (String, Option<String>, Option<String>, Option<String>) {
    sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>)>(
        "SELECT status, resolved_by, resolved_reason, resolved_at FROM proposals \
         WHERE workspace_id = $1 AND commit_id = $2 AND base_epoch = $3 AND base_seq = $4",
    )
    .bind(ws)
    .bind(commit.0.to_vec())
    .bind(i64::try_from(base.epoch).unwrap())
    .bind(i64::try_from(base.seq).unwrap())
    .fetch_one(pool)
    .await
    .unwrap()
}

// ── the happy path + byte-identical replay ──────────────────────────────────────────────────────

#[sqlx::test]
async fn session_approve_promotes_stamps_the_resolution_and_replays_byte_identically(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "srv-approve").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(60);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let (_g, cp, _d) = open_proposal(
        &fx,
        &w,
        &s,
        &key,
        "60000000-0000-4000-8000-000000000001",
        "60000000-0000-4000-8000-000000000002",
    )
    .await;

    // Approve promotes sideways: current advances (1,1)->(1,2); the candidate becomes `current`.
    let rid = "60000000-0000-4000-8000-000000000003";
    let r = approve_session(&fx, &w, &s, cp, gn(1, 1), rid, "reviewer@acme.com").await;
    assert!(r.is_ok());
    assert_eq!(r.current, Some(gn(1, 2)));
    assert!(r.record.is_some());
    assert_eq!(current_commit(&fx, &w, &s).await, cp);

    // The proposal row is accepted with the full resolution stamp: WHO (the reviewer's canonical
    // email), WHEN (the op's created_at), and NO reason (an accept has no reason field).
    assert_eq!(
        resolution_of(&pool, "w_acme", cp, gn(1, 1)).await,
        (
            "accepted".to_owned(),
            Some("reviewer@acme.com".to_owned()),
            None,
            Some(CREATED_AT.to_owned()),
        )
    );
    // The approvals audit row names the session reviewer.
    let approvals = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM approvals WHERE workspace_id = $1 AND reviewer = $2",
    )
    .bind("w_acme")
    .bind("reviewer@acme.com")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(approvals, 1);
    // The durable receipt names the SESSION lane: the acting email as actor, `web_session`, and the
    // domain-tagged request identity beside the bound columns.
    let rows = receipts_for(&pool, "w_acme", rid).await;
    assert_eq!(rows.len(), 1);
    let (actor, method, sha, outcome) = &rows[0];
    assert_eq!(
        (actor.as_str(), method.as_str()),
        ("reviewer@acme.com", "web_session")
    );
    assert_eq!(sha.as_ref().map(Vec::len), Some(32));
    assert_eq!(outcome, "OK");

    // A same-request_id re-run returns the byte-identical receipt (record included) and the
    // pointer does NOT move twice: seq advanced exactly once, one durable row.
    let replayed = approve_session(&fx, &w, &s, cp, gn(1, 1), rid, "reviewer@acme.com").await;
    assert_eq!(replayed, r);
    assert_eq!(
        fx.authority
            .db()
            .read_current_generation(&w, &s)
            .await
            .unwrap(),
        Some(gn(1, 2))
    );
    assert_eq!(receipts_for(&pool, "w_acme", rid).await.len(), 1);
}

// ── staleness: the same CAS refusal the CLI gets ────────────────────────────────────────────────

#[sqlx::test]
async fn a_stale_session_approve_conflicts_with_the_live_generation(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "srv-stale").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(61);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let (g, cp, _d) = open_proposal(
        &fx,
        &w,
        &s,
        &key,
        "61000000-0000-4000-8000-000000000001",
        "61000000-0000-4000-8000-000000000002",
    )
    .await;
    // A direct publish advances `current` to (1,2): the proposal is now STALE.
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "61000000-0000-4000-8000-000000000003",
        child(g, vec![file("SKILL.md", b"v1")]),
        gn(1, 1),
    )
    .await;
    let live = current_commit(&fx, &w, &s).await;

    // The session approve at the OLD expected ⇒ CONFLICT carrying the live generation; pointer unmoved.
    let rid = "61000000-0000-4000-8000-000000000004";
    let r = approve_session(&fx, &w, &s, cp, gn(1, 1), rid, "reviewer@acme.com").await;
    assert_eq!(r.outcome, TerminalOutcome::Conflict);
    assert_eq!(r.current, Some(gn(1, 2)));
    assert_eq!(current_commit(&fx, &w, &s).await, live);
    // A past-the-gate protocol outcome is durable (the shared terminal writers).
    let rows = receipts_for(&pool, "w_acme", rid).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].3, "CONFLICT");
}

#[sqlx::test]
async fn aba_a_stale_session_approve_conflicts_even_when_the_live_tree_matches_the_base(
    pool: PgPool,
) {
    // …X(beta)->Y(gamma); revert --to X makes current.tree == X.tree == the proposal's base tree, yet
    // the generation advanced. A late SESSION approve at the stale base must CONFLICT exactly as the
    // device lane's does — the whole-(epoch,seq) CAS is shared, not re-implemented per lane.
    let fx = Fixture::new(pool, "srv-aba").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(62);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    // X (beta) at (1,1).
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "62000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"beta")]),
        gn(0, 0),
    )
    .await;
    let x = current_commit(&fx, &w, &s).await;
    // Propose Q on base X (1,1).
    let (_, q, _dq) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "62000000-0000-4000-8000-000000000002",
        child(x, vec![file("a.md", b"q-change")]),
        gn(1, 1),
    )
    .await;
    // Publish Y (gamma) -> (1,2).
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "62000000-0000-4000-8000-000000000003",
        child(x, vec![file("a.md", b"gamma")]),
        gn(1, 1),
    )
    .await;
    // Revert --to X -> (1,3). Now current.tree == beta == Q's base tree, but the generation moved on.
    let rop = op("62000000-0000-4000-8000-000000000004");
    let rdev = revert_request(&w, "dk", gn(1, 2));
    let rev = fx
        .authority
        .revert(
            &w,
            &s,
            x,
            rdev,
            "d_test",
            "topos revert",
            &rop,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(rev.current, Some(gn(1, 3)));

    // The session approve of Q at its stale base (1,1) ⇒ CONFLICT (live (1,3)), tree match or not.
    let r = approve_session(
        &fx,
        &w,
        &s,
        q,
        gn(1, 1),
        "62000000-0000-4000-8000-000000000005",
        "reviewer@acme.com",
    )
    .await;
    assert_eq!(r.outcome, TerminalOutcome::Conflict);
    assert_eq!(r.current, Some(gn(1, 3)));
}

// ── the acting gate: the durable role denial vs the uniform synthesized one ─────────────────────

#[sqlx::test]
async fn a_confirmed_plain_members_role_denial_is_durable_typed_and_replayable(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "srv-role").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(63);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "member@acme.com", "member").await;
    let (g, cp, _d) = open_proposal(
        &fx,
        &w,
        &s,
        &key,
        "63000000-0000-4000-8000-000000000001",
        "63000000-0000-4000-8000-000000000002",
    )
    .await;

    // A confirmed plain member is entitled to a RECORDED, replayable answer: the typed role denial.
    let rid = "63000000-0000-4000-8000-000000000003";
    let r = approve_session(&fx, &w, &s, cp, gn(1, 1), rid, "member@acme.com").await;
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    assert_eq!(code_of(&r).as_deref(), Some("REVIEWER_ROLE_REQUIRED"));
    assert_eq!(
        msg_of(&r).as_deref(),
        Some("approving or rejecting needs an owner or reviewer seat")
    );
    let rows = receipts_for(&pool, "w_acme", rid).await;
    assert_eq!(rows.len(), 1);
    let (actor, method, sha, outcome) = &rows[0];
    assert_eq!(
        (actor.as_str(), method.as_str()),
        ("member@acme.com", "web_session")
    );
    assert!(
        sha.is_some(),
        "the durable denial binds the request identity"
    );
    assert_eq!(outcome, "DENIED");
    // The same request_id replays the byte-identical receipt.
    let replayed = approve_session(&fx, &w, &s, cp, gn(1, 1), rid, "member@acme.com").await;
    assert_eq!(replayed, r);
    assert_eq!(receipts_for(&pool, "w_acme", rid).await.len(), 1);

    // The reject verb runs the SAME gate through its own transaction — durable there too.
    let rid2 = "63000000-0000-4000-8000-000000000004";
    let r2 = reject_session(
        &fx,
        &w,
        &s,
        cp,
        gn(1, 1),
        "too vague",
        rid2,
        "member@acme.com",
    )
    .await;
    assert_eq!(r2.outcome, TerminalOutcome::Denied);
    assert_eq!(code_of(&r2).as_deref(), Some("REVIEWER_ROLE_REQUIRED"));
    assert_eq!(receipts_for(&pool, "w_acme", rid2).await.len(), 1);

    // Nothing moved, nothing resolved: the proposal is still open, `current` still at genesis.
    assert_eq!(current_commit(&fx, &w, &s).await, g);
    assert_eq!(resolution_of(&pool, "w_acme", cp, gn(1, 1)).await.0, "open");
}

#[sqlx::test]
async fn unproven_callers_get_one_uniform_denial_and_a_confirmed_owner_passes(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "srv-gate").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(64);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    fx.authority
        .db()
        .seed_workspace_member(&w, &prin("invited@acme.com"), "member", "invited")
        .await
        .unwrap();
    let (g, cp, _d) = open_proposal(
        &fx,
        &w,
        &s,
        &key,
        "64000000-0000-4000-8000-000000000001",
        "64000000-0000-4000-8000-000000000002",
    )
    .await;
    const UNIFORM: &str = "session review ops require a confirmed workspace member";

    // A stranger and a merely-invited seat: BOTH verbs, the ONE uniform message, nothing durable.
    let mut n = 0u64;
    for acting in ["stranger@evil.com", "invited@acme.com"] {
        n += 1;
        let rid_a = format!("64000000-0000-4000-8000-0000000001{n:02}");
        let ra = approve_session(&fx, &w, &s, cp, gn(1, 1), &rid_a, acting).await;
        assert_eq!(ra.outcome, TerminalOutcome::Denied, "{acting} approve");
        assert_eq!(msg_of(&ra).as_deref(), Some(UNIFORM), "{acting} approve");
        assert!(receipts_for(&pool, "w_acme", &rid_a).await.is_empty());
        let rid_r = format!("64000000-0000-4000-8000-0000000002{n:02}");
        let rr = reject_session(&fx, &w, &s, cp, gn(1, 1), "nope", &rid_r, acting).await;
        assert_eq!(rr.outcome, TerminalOutcome::Denied, "{acting} reject");
        assert_eq!(msg_of(&rr).as_deref(), Some(UNIFORM), "{acting} reject");
        assert!(receipts_for(&pool, "w_acme", &rid_r).await.is_empty());
    }

    // An UNKNOWN workspace reads exactly the same — even for an email seated elsewhere.
    let ghost_rid = "64000000-0000-4000-8000-000000000301";
    let rg = a
        .review_approve_session(
            &ws("w_ghost"),
            &s,
            cp,
            gn(1, 1),
            ghost_rid,
            "reviewer@acme.com",
            CLOUD,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(rg.outcome, TerminalOutcome::Denied);
    assert_eq!(msg_of(&rg).as_deref(), Some(UNIFORM));
    assert!(receipts_for(&pool, "w_ghost", ghost_rid).await.is_empty());

    // Nothing moved through the denials — the proposal is still open at genesis.
    assert_eq!(current_commit(&fx, &w, &s).await, g);

    // A confirmed OWNER passes the gate like a reviewer — and a SELF-HOST plane ANSWERS the op exactly
    // like a hosted one: the acting gate is the confirmed-seat role check, identical on both postures
    // (no blanket self-host denial). The owner approving on self-host lands the pointer.
    seat(&fx, &w, "owner2@acme.com", "owner").await;
    let ok = a
        .review_approve_session(
            &w,
            &s,
            cp,
            gn(1, 1),
            "64000000-0000-4000-8000-000000000303",
            "owner2@acme.com",
            DeploymentMode::SelfHost,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert!(ok.is_ok());
    assert_eq!(current_commit(&fx, &w, &s).await, cp);
}

// ── four-eyes across lanes (the proposer principal is the compare, not the lane) ────────────────

#[sqlx::test]
async fn four_eyes_blocks_a_device_proposer_from_session_approving_under_review_required(
    pool: PgPool,
) {
    let fx = Fixture::new(pool.clone(), "srv-4eyes-on").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(66);
    // The author's DEVICE proposes; the SAME mailbox holds a confirmed reviewer seat, so the role gate
    // admits it and four-eyes is the discriminator — over canonical principals, across the two lanes.
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "author@acme.com", "reviewer").await;
    seat(&fx, &w, "reviewer2@acme.com", "reviewer").await;
    fx.authority
        .db()
        .set_review_required(&w, true)
        .await
        .unwrap();
    let (g, cp, _d) = open_proposal(
        &fx,
        &w,
        &s,
        &key,
        "66000000-0000-4000-8000-000000000001",
        "66000000-0000-4000-8000-000000000002",
    )
    .await;

    // The device-lane proposer may NOT session-approve their own proposal under review-required.
    let rid = "66000000-0000-4000-8000-000000000003";
    let denied = approve_session(&fx, &w, &s, cp, gn(1, 1), rid, "author@acme.com").await;
    assert_eq!(denied.outcome, TerminalOutcome::Denied);
    assert_eq!(
        msg_of(&denied).as_deref(),
        Some("the proposer may not approve their own proposal on a reviewed bundle")
    );
    assert_eq!(current_commit(&fx, &w, &s).await, g);
    // A past-the-gate denial of a CONFIRMED reviewer is durable.
    assert_eq!(receipts_for(&pool, "w_acme", rid).await.len(), 1);

    // A DIFFERENT session reviewer approves ⇒ OK.
    let ok = approve_session(
        &fx,
        &w,
        &s,
        cp,
        gn(1, 1),
        "66000000-0000-4000-8000-000000000004",
        "reviewer2@acme.com",
    )
    .await;
    assert!(ok.is_ok());
    assert_eq!(current_commit(&fx, &w, &s).await, cp);
}

#[sqlx::test]
async fn a_device_proposer_may_session_approve_their_own_proposal_with_review_off(pool: PgPool) {
    let fx = Fixture::new(pool, "srv-4eyes-off").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(67);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "author@acme.com", "reviewer").await;
    // review_required is OFF (the default) — a deferred self-publish is allowed across lanes too.
    let (_g, cp, _d) = open_proposal(
        &fx,
        &w,
        &s,
        &key,
        "67000000-0000-4000-8000-000000000001",
        "67000000-0000-4000-8000-000000000002",
    )
    .await;
    let ok = approve_session(
        &fx,
        &w,
        &s,
        cp,
        gn(1, 1),
        "67000000-0000-4000-8000-000000000003",
        "author@acme.com",
    )
    .await;
    assert!(
        ok.is_ok(),
        "self-approve is allowed with review_required off"
    );
    assert_eq!(current_commit(&fx, &w, &s).await, cp);
}

// ── session reject: the reason column + the target classification ───────────────────────────────

#[sqlx::test]
async fn session_reject_flips_open_to_rejected_and_records_the_reason_verbatim(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "srv-reject").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(68);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let (g, cp, _d) = open_proposal(
        &fx,
        &w,
        &s,
        &key,
        "68000000-0000-4000-8000-000000000001",
        "68000000-0000-4000-8000-000000000002",
    )
    .await;

    // Reject ⇒ OK (no pointer data, no pointer move); the reason lands verbatim with WHO and WHEN.
    let reason = "too broad for our deploy flow";
    let rid = "68000000-0000-4000-8000-000000000003";
    let r = reject_session(&fx, &w, &s, cp, gn(1, 1), reason, rid, "reviewer@acme.com").await;
    assert_eq!(r.outcome, TerminalOutcome::Ok);
    assert_eq!(code_of(&r).as_deref(), Some("PROPOSAL_REJECTED"));
    assert!(r.record.is_none());
    assert_eq!(current_commit(&fx, &w, &s).await, g);
    assert_eq!(
        resolution_of(&pool, "w_acme", cp, gn(1, 1)).await,
        (
            "rejected".to_owned(),
            Some("reviewer@acme.com".to_owned()),
            Some(reason.to_owned()),
            Some(CREATED_AT.to_owned()),
        )
    );
    let rows = receipts_for(&pool, "w_acme", rid).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1.as_str(), rows[0].3.as_str()),
        ("reviewer@acme.com", "web_session", "OK")
    );

    // A reject of the already-rejected proposal under a NEW request_id is an idempotent OK — and the
    // FIRST resolution stands (the retry's different reason overwrites nothing).
    let again = reject_session(
        &fx,
        &w,
        &s,
        cp,
        gn(1, 1),
        "some other wording",
        "68000000-0000-4000-8000-000000000004",
        "reviewer@acme.com",
    )
    .await;
    assert_eq!(again.outcome, TerminalOutcome::Ok);
    assert_eq!(
        code_of(&again).as_deref(),
        Some("PROPOSAL_ALREADY_REJECTED")
    );
    assert_eq!(
        resolution_of(&pool, "w_acme", cp, gn(1, 1)).await.2,
        Some(reason.to_owned())
    );
}

#[sqlx::test]
async fn session_reject_refuses_empty_reasons_accepted_targets_and_wrong_bases_never_conflict(
    pool: PgPool,
) {
    let fx = Fixture::new(pool.clone(), "srv-reject-cls").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(69);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let (_g, cp, _d) = open_proposal(
        &fx,
        &w,
        &s,
        &key,
        "69000000-0000-4000-8000-000000000001",
        "69000000-0000-4000-8000-000000000002",
    )
    .await;

    // An empty (or whitespace-only) reason is a SYNTHESIZED refusal — a belt, never a durable row.
    for (n, reason) in [(1u8, ""), (2, "   \t ")] {
        let rid = format!("69000000-0000-4000-8000-0000000001{n:02}");
        let r = reject_session(&fx, &w, &s, cp, gn(1, 1), reason, &rid, "reviewer@acme.com").await;
        assert_eq!(r.outcome, TerminalOutcome::Denied);
        assert_eq!(code_of(&r).as_deref(), Some("REASON_REQUIRED"));
        assert!(receipts_for(&pool, "w_acme", &rid).await.is_empty());
    }

    // A WRONG (stale-shaped) base: reject has no CAS, so a mismatched base is the typed not-open
    // denial — NEVER a CONFLICT (there is no pointer move to conflict with).
    let wrong = reject_session(
        &fx,
        &w,
        &s,
        cp,
        gn(1, 5),
        "stale form",
        "69000000-0000-4000-8000-000000000201",
        "reviewer@acme.com",
    )
    .await;
    assert_eq!(wrong.outcome, TerminalOutcome::Denied);
    assert_ne!(wrong.outcome, TerminalOutcome::Conflict);
    assert_eq!(
        msg_of(&wrong).as_deref(),
        Some("no open proposal for this candidate and base")
    );
    assert_eq!(resolution_of(&pool, "w_acme", cp, gn(1, 1)).await.0, "open");

    // Accept it, then a late reject is the typed already-accepted denial.
    let ok = approve_session(
        &fx,
        &w,
        &s,
        cp,
        gn(1, 1),
        "69000000-0000-4000-8000-000000000202",
        "reviewer@acme.com",
    )
    .await;
    assert!(ok.is_ok());
    let late = reject_session(
        &fx,
        &w,
        &s,
        cp,
        gn(1, 1),
        "changed my mind",
        "69000000-0000-4000-8000-000000000203",
        "reviewer@acme.com",
    )
    .await;
    assert_eq!(late.outcome, TerminalOutcome::Denied);
    assert_eq!(
        msg_of(&late).as_deref(),
        Some("the proposal is already accepted")
    );
}

// ── the cross-lane op-id space (one workspace-global slot; per-device semantics preserved) ───────

#[sqlx::test]
async fn cross_lane_op_id_reuse_fails_closed_in_both_directions(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "srv-cross").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(65);
    let key2 = dev_key(75);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    register(&fx, &w, &s, "dk2", &key2, "reviewer2@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    seat(&fx, &w, "reviewer2@acme.com", "reviewer").await;
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "65000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    let op_propose_1 = "65000000-0000-4000-8000-000000000002";
    let (_, p1, _d1) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        op_propose_1,
        child(g, vec![file("a.md", b"p1")]),
        gn(1, 1),
    )
    .await;

    // DEVICE → SESSION: the propose's op_id already holds a device receipt; a session request_id
    // reusing it is a permanent key reuse (any existing row closes the session slot).
    let r = approve_session(&fx, &w, &s, p1, gn(1, 1), op_propose_1, "reviewer@acme.com").await;
    assert_eq!(r.outcome, TerminalOutcome::PermanentFailure);
    assert_eq!(code_of(&r).as_deref(), Some("OP_ID_REUSED"));

    // A fresh session id approves p1 ⇒ OK at (1,2).
    let session_rid = "65000000-0000-4000-8000-000000000003";
    let ok = approve_session(&fx, &w, &s, p1, gn(1, 1), session_rid, "reviewer@acme.com").await;
    assert!(ok.is_ok());

    // SESSION → DEVICE: a device review op reusing the session's request_id as its op_id fails
    // closed too (the two identities can never match — the session slot is method+actor-keyed).
    let (_, p2, d2) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "65000000-0000-4000-8000-000000000004",
        child(p1, vec![file("a.md", b"p2")]),
        gn(1, 2),
    )
    .await;
    let dr = do_approve(&fx, &key, "dk", &w, &s, session_rid, p2, d2, gn(1, 2)).await;
    assert_eq!(dr.outcome, TerminalOutcome::PermanentFailure);
    assert_eq!(code_of(&dr).as_deref(), Some("OP_ID_REUSED"));
    assert_eq!(
        current_commit(&fx, &w, &s).await,
        p1,
        "the reuse moved nothing"
    );

    // SESSION → SESSION, two DIFFERENT emails: the session slot is GLOBAL per (ws, op_id) — the
    // second actor's reuse is refused even though the first actor could replay it byte-identically.
    let shared_rid = "65000000-0000-4000-8000-000000000005";
    let first = approve_session(&fx, &w, &s, p2, gn(1, 2), shared_rid, "reviewer@acme.com").await;
    assert!(first.is_ok());
    let second = approve_session(&fx, &w, &s, p2, gn(1, 2), shared_rid, "reviewer2@acme.com").await;
    assert_eq!(second.outcome, TerminalOutcome::PermanentFailure);
    assert_eq!(code_of(&second).as_deref(), Some("OP_ID_REUSED"));

    // DEVICE → DEVICE (no web_session row anywhere near the id): the pre-lane per-device slot
    // semantics hold bit-for-bit — a DIFFERENT device reusing a device op_id proceeds FRESH.
    let op_propose_3 = "65000000-0000-4000-8000-000000000006";
    let (_, p3, d3) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        op_propose_3,
        child(p2, vec![file("a.md", b"p3")]),
        gn(1, 3),
    )
    .await;
    let fresh = do_approve(&fx, &key2, "dk2", &w, &s, op_propose_3, p3, d3, gn(1, 3)).await;
    assert!(
        fresh.is_ok(),
        "another device's op_id is not this device's slot"
    );
    assert_eq!(current_commit(&fx, &w, &s).await, p3);

    // DEVICE PROPOSE reusing a session-held id: the refusal must also RELEASE the incoming
    // candidate's own migrate lease — the propose staged real bytes under this op_id before the
    // transaction could classify the reuse, and a key-reuse refusal abandons that candidate exactly
    // like a receipted terminal does (without the release its objects would stay GC-rooted forever).
    let (reused, _, _) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        shared_rid,
        child(p3, vec![file("a.md", b"p4")]),
        gn(1, 4),
    )
    .await;
    assert_eq!(reused.outcome, TerminalOutcome::PermanentFailure);
    assert_eq!(code_of(&reused).as_deref(), Some("OP_ID_REUSED"));
    let leases = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM promotion_lease WHERE workspace_id = $1 AND op_id = $2",
    )
    .bind(w.as_str())
    .bind(shared_rid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(leases, 0, "the abandoned candidate's lease is released");
}

#[sqlx::test]
async fn a_divergent_payload_or_a_reworded_reason_under_a_reused_request_id_is_key_reuse(
    pool: PgPool,
) {
    let fx = Fixture::new(pool, "srv-diverge").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(70);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let (_g, p1, _d1) = open_proposal(
        &fx,
        &w,
        &s,
        &key,
        "6a000000-0000-4000-8000-000000000001",
        "6a000000-0000-4000-8000-000000000002",
    )
    .await;

    // Approve p1 under request_id A ⇒ OK; the SAME id + email against a DIFFERENT candidate is a
    // divergent payload — refused closed, never re-executed.
    let rid_a = "6a000000-0000-4000-8000-000000000003";
    let ok = approve_session(&fx, &w, &s, p1, gn(1, 1), rid_a, "reviewer@acme.com").await;
    assert!(ok.is_ok());
    let (_, p2, _d2) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "6a000000-0000-4000-8000-000000000004",
        child(p1, vec![file("SKILL.md", b"v2")]),
        gn(1, 2),
    )
    .await;
    let diverged = approve_session(&fx, &w, &s, p2, gn(1, 2), rid_a, "reviewer@acme.com").await;
    assert_eq!(diverged.outcome, TerminalOutcome::PermanentFailure);
    assert_eq!(code_of(&diverged).as_deref(), Some("OP_ID_REUSED"));
    assert_eq!(current_commit(&fx, &w, &s).await, p1);

    // The reason-inclusive request identity: the SAME reject replays byte-identically, but a
    // RE-WORDED reason under the same request_id mismatches the stored identity and fails closed —
    // the bound-identity columns alone would have replayed it (they carry no reason).
    let rid_b = "6a000000-0000-4000-8000-000000000005";
    let rejected = reject_session(
        &fx,
        &w,
        &s,
        p2,
        gn(1, 2),
        "too broad",
        rid_b,
        "reviewer@acme.com",
    )
    .await;
    assert_eq!(rejected.outcome, TerminalOutcome::Ok);
    let replayed = reject_session(
        &fx,
        &w,
        &s,
        p2,
        gn(1, 2),
        "too broad",
        rid_b,
        "reviewer@acme.com",
    )
    .await;
    assert_eq!(replayed, rejected);
    let reworded = reject_session(
        &fx,
        &w,
        &s,
        p2,
        gn(1, 2),
        "too narrow",
        rid_b,
        "reviewer@acme.com",
    )
    .await;
    assert_eq!(reworded.outcome, TerminalOutcome::PermanentFailure);
    assert_eq!(code_of(&reworded).as_deref(), Some("OP_ID_REUSED"));
}

// ── canonical principals (one mailbox, one identity, however the session cased it) ──────────────

#[sqlx::test]
async fn a_mixed_case_acting_email_folds_to_its_canonical_reviewer_seat(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "srv-canon").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(71);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let (_g, cp, _d) = open_proposal(
        &fx,
        &w,
        &s,
        &key,
        "6b000000-0000-4000-8000-000000000001",
        "6b000000-0000-4000-8000-000000000002",
    )
    .await;

    // The session's email arrives cased however the IdP rendered it; the fold finds the canonical
    // reviewer seat, and everything recorded — receipt actor, resolved_by — is the canonical form.
    let rid = "6b000000-0000-4000-8000-000000000003";
    let r = approve_session(&fx, &w, &s, cp, gn(1, 1), rid, "Reviewer@Acme.COM").await;
    assert!(r.is_ok());
    let rows = receipts_for(&pool, "w_acme", rid).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1.as_str()),
        ("reviewer@acme.com", "web_session")
    );
    assert_eq!(
        resolution_of(&pool, "w_acme", cp, gn(1, 1)).await.1,
        Some("reviewer@acme.com".to_owned())
    );
}

// ── concurrency (the shared serializable write must converge, not double-execute) ───────────────

#[sqlx::test]
async fn raced_identical_session_approves_converge_to_one_byte_identical_ok(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "srv-race-same").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(72);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let (_g, cp, _d) = open_proposal(
        &fx,
        &w,
        &s,
        &key,
        "6c000000-0000-4000-8000-000000000001",
        "6c000000-0000-4000-8000-000000000002",
    )
    .await;

    // Two concurrent approves with the SAME request_id + email: `op_receipts_pkey` (in the
    // serializable runner's convergent-23505 set) makes the loser retry into a replay hit — both
    // callers end holding the byte-identical OK and the pointer advanced exactly once.
    let rid = "6c000000-0000-4000-8000-000000000003";
    let (ra, rb) = tokio::join!(
        approve_session(&fx, &w, &s, cp, gn(1, 1), rid, "reviewer@acme.com"),
        approve_session(&fx, &w, &s, cp, gn(1, 1), rid, "reviewer@acme.com"),
    );
    assert!(ra.is_ok());
    assert_eq!(ra, rb, "both racers converge to the one stored receipt");
    assert_eq!(
        fx.authority
            .db()
            .read_current_generation(&w, &s)
            .await
            .unwrap(),
        Some(gn(1, 2))
    );
    assert_eq!(receipts_for(&pool, "w_acme", rid).await.len(), 1);
}

#[sqlx::test]
async fn a_raced_device_and_session_approve_sharing_one_op_id_executes_once(pool: PgPool) {
    let fx = Fixture::new(pool, "srv-race-cross").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(73);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let (_g, cp, _digest_) = open_proposal(
        &fx,
        &w,
        &s,
        &key,
        "6d000000-0000-4000-8000-000000000001",
        "6d000000-0000-4000-8000-000000000002",
    )
    .await;

    // One op-id STRING, raced across the two lanes: whichever lane commits first owns the slot; the
    // other's lane-blind replay probe classifies the foreign row as key reuse — never a second
    // promote, never a CONFLICT (replay runs before the CAS on both lanes).
    let shared = "6d000000-0000-4000-8000-000000000003";
    let op_shared = op(shared);
    let device = DeviceOpAuth {
        credential: cred(&w, "dk"),
        op: DeviceOp::ReviewApprove,
        expected: gn(1, 1),
    };
    let (rd, rs) = tokio::join!(
        fx.authority
            .review_approve(&w, &s, cp, device, &op_shared, CREATED_AT, NOW),
        fx.authority.review_approve_session(
            &w,
            &s,
            cp,
            gn(1, 1),
            shared,
            "reviewer@acme.com",
            CLOUD,
            CREATED_AT,
            NOW
        ),
    );
    let (rd, rs) = (rd.unwrap(), rs.unwrap());
    let winners = [&rd, &rs].iter().filter(|r| r.is_ok()).count();
    assert_eq!(winners, 1, "exactly one lane executes the shared op id");
    let loser = if rd.is_ok() { &rs } else { &rd };
    assert_eq!(loser.outcome, TerminalOutcome::PermanentFailure);
    assert_eq!(code_of(loser).as_deref(), Some("OP_ID_REUSED"));
    assert_eq!(current_commit(&fx, &w, &s).await, cp);
    assert_eq!(
        fx.authority
            .db()
            .read_current_generation(&w, &s)
            .await
            .unwrap(),
        Some(gn(1, 2))
    );
}

// ── the session proposal-detail read (the four-eyes disclosure surface) ─────────────────────────

#[sqlx::test]
async fn the_detail_read_discloses_proposer_and_resolution_to_confirmed_members_only(pool: PgPool) {
    let fx = Fixture::new(pool, "srv-detail").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(74);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "member@acme.com", "member").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let (_g, cp, _d) = open_proposal(
        &fx,
        &w,
        &s,
        &key,
        "6e000000-0000-4000-8000-000000000001",
        "6e000000-0000-4000-8000-000000000002",
    )
    .await;
    let cp_hex = digest::to_hex(&cp.0);

    // Any confirmed member — a plain member included — reads the proposer, the base, the status, and
    // the workspace policy at read time.
    let d = a
        .read_proposal_detail_session(&w, "s_deploy", &cp_hex, "member@acme.com", CLOUD)
        .await
        .unwrap()
        .expect("an opened proposal has a detail row");
    assert_eq!(d.version_id, cp.0);
    assert_eq!(d.status, "open");
    assert_eq!(d.base, gn(1, 1));
    assert_eq!(d.proposer, "author@acme.com");
    assert_eq!(d.created_at, CREATED_AT);
    assert!(!d.review_required);
    assert_eq!(
        (d.resolved_by, d.resolved_reason, d.resolved_at),
        (None, None, None)
    );
    // The policy is read live (display-only; the in-txn gate stays the authority).
    fx.authority
        .db()
        .set_review_required(&w, true)
        .await
        .unwrap();
    let d = a
        .read_proposal_detail_session(&w, "s_deploy", &cp_hex, "member@acme.com", CLOUD)
        .await
        .unwrap()
        .unwrap();
    assert!(d.review_required);

    // After a session reject the resolution facts are disclosed in full.
    let r = reject_session(
        &fx,
        &w,
        &s,
        cp,
        gn(1, 1),
        "needs a runbook link",
        "6e000000-0000-4000-8000-000000000003",
        "reviewer@acme.com",
    )
    .await;
    assert_eq!(r.outcome, TerminalOutcome::Ok);
    let d = a
        .read_proposal_detail_session(&w, "s_deploy", &cp_hex, "member@acme.com", CLOUD)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(d.status, "rejected");
    assert_eq!(d.resolved_by.as_deref(), Some("reviewer@acme.com"));
    assert_eq!(d.resolved_reason.as_deref(), Some("needs a runbook link"));
    assert_eq!(d.resolved_at.as_deref(), Some(CREATED_AT));

    // Every pre-gate miss is the ONE uniform NotFound: a stranger, an invited seat.
    fx.authority
        .db()
        .seed_workspace_member(&w, &prin("invited@acme.com"), "member", "invited")
        .await
        .unwrap();
    for acting in ["stranger@evil.com", "invited@acme.com"] {
        assert!(matches!(
            a.read_proposal_detail_session(&w, "s_deploy", &cp_hex, acting, CLOUD)
                .await,
            Err(AuthorityError::NotFound)
        ));
    }
    // A self-host plane ANSWERS this read for a confirmed member exactly like a hosted one — the acting
    // gate is the confirmed-seat check, identical on both postures.
    let sh_detail = a
        .read_proposal_detail_session(
            &w,
            "s_deploy",
            &cp_hex,
            "member@acme.com",
            DeploymentMode::SelfHost,
        )
        .await
        .unwrap()
        .expect("self-host serves the confirmed member the same detail");
    assert_eq!(sh_detail.status, "rejected");
    // A malformed version id is the same member-lane miss; an UNKNOWN candidate is the member-
    // entitled `Ok(None)` (the composing wrapper folds it into its uniform miss).
    assert!(matches!(
        a.read_proposal_detail_session(&w, "s_deploy", "not-hex", "member@acme.com", CLOUD)
            .await,
        Err(AuthorityError::NotFound)
    ));
    let unknown = digest::to_hex(&[0xAB; 32]);
    assert!(
        a.read_proposal_detail_session(&w, "s_deploy", &unknown, "member@acme.com", CLOUD)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test]
async fn after_a_reject_and_a_re_propose_the_open_row_wins_the_detail(pool: PgPool) {
    // Two rows CAN coexist for one (skill, candidate) — a rejected attempt plus a re-propose on a
    // newer base — and the OPEN row must never be shadowed by the terminal one, whatever the
    // timestamps say. The newer base is minted the one way a re-propose of the SAME candidate is
    // possible: an epoch bump that moves the generation while `current` keeps the same commit.
    let fx = Fixture::new(pool.clone(), "srv-detail-pref").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(76);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "member@acme.com", "member").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let files = vec![
        file("SKILL.md", b"v0"),
        file("NEW.md", b"re-proposed bytes"),
    ];
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "6f000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    let (_, cp, _d) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "6f000000-0000-4000-8000-000000000002",
        child(g, files.clone()),
        gn(1, 1),
    )
    .await;
    reject_session(
        &fx,
        &w,
        &s,
        cp,
        gn(1, 1),
        "not yet",
        "6f000000-0000-4000-8000-000000000003",
        "reviewer@acme.com",
    )
    .await;
    let cp_hex = digest::to_hex(&cp.0);
    let d = a
        .read_proposal_detail_session(&w, "s_deploy", &cp_hex, "member@acme.com", CLOUD)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(d.status, "rejected");

    // The epoch bump re-signs `current` at (2,1) over the SAME commit, so the identical candidate
    // (same content, same first parent) can re-propose at a genuinely NEWER base.
    fx.authority
        .restore_bump_epochs(None, None, NOW + 1)
        .await
        .unwrap();
    let (_, cp2, _d2) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "6f000000-0000-4000-8000-000000000004",
        child(g, files),
        gn(2, 1),
    )
    .await;
    assert_eq!(
        cp2, cp,
        "identical bytes are the same content-derived candidate"
    );
    let rows = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM proposals WHERE workspace_id = $1 AND commit_id = $2",
    )
    .bind("w_acme")
    .bind(cp.0.to_vec())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 2, "the rejected row and the open re-propose coexist");

    // The detail shows the OPEN row — fresh base, no resolution — not the older rejected one.
    let d = a
        .read_proposal_detail_session(&w, "s_deploy", &cp_hex, "member@acme.com", CLOUD)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(d.status, "open");
    assert_eq!(d.base, gn(2, 1));
    assert_eq!(
        (d.resolved_by, d.resolved_reason, d.resolved_at),
        (None, None, None)
    );
}

// ── session pre-transaction misses (the recording rule, upstream of the txn) ────────────────────

#[sqlx::test]
async fn session_pretxn_misses_are_synthesized_never_durable(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "srv-pretxn").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(77);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "70000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;

    // A commit this skill never recorded: a typed permanent failure, synthesized — a proper reviewer
    // must NOT grow op_receipts through a pre-txn miss (deterministic re-run is owed, not replay).
    let rid_unknown = "70000000-0000-4000-8000-000000000002";
    let r = approve_session(
        &fx,
        &w,
        &s,
        CommitId([0xAB; 32]),
        gn(1, 1),
        rid_unknown,
        "reviewer@acme.com",
    )
    .await;
    assert_eq!(r.outcome, TerminalOutcome::PermanentFailure);
    assert_eq!(
        msg_of(&r).as_deref(),
        Some("no such proposal commit for this skill")
    );
    assert!(receipts_for(&pool, "w_acme", rid_unknown).await.is_empty());

    // A KNOWN commit that was never proposed: the same synthesized posture, one step deeper.
    let rid_never = "70000000-0000-4000-8000-000000000003";
    let r = approve_session(&fx, &w, &s, g, gn(1, 1), rid_never, "reviewer@acme.com").await;
    assert_eq!(r.outcome, TerminalOutcome::PermanentFailure);
    assert_eq!(
        msg_of(&r).as_deref(),
        Some("no proposal for this candidate and base")
    );
    assert!(receipts_for(&pool, "w_acme", rid_never).await.is_empty());

    // The reject twin of the unknown-commit miss.
    let rid_reject = "70000000-0000-4000-8000-000000000004";
    let r = reject_session(
        &fx,
        &w,
        &s,
        CommitId([0xAB; 32]),
        gn(1, 1),
        "never existed",
        rid_reject,
        "reviewer@acme.com",
    )
    .await;
    assert_eq!(r.outcome, TerminalOutcome::PermanentFailure);
    assert_eq!(
        msg_of(&r).as_deref(),
        Some("no such proposal commit for this skill")
    );
    assert!(receipts_for(&pool, "w_acme", rid_reject).await.is_empty());

    // The malformed-request_id belt refuses before everything (and cannot mint a row either).
    let r = fx
        .authority
        .review_approve_session(
            &w,
            &s,
            g,
            gn(1, 1),
            "not-a-uuid",
            "reviewer@acme.com",
            CLOUD,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    assert_eq!(
        msg_of(&r).as_deref(),
        Some("request_id is not a canonical UUID")
    );
    assert!(receipts_for(&pool, "w_acme", "not-a-uuid").await.is_empty());
}

// ── the web-session REVERT leg — one-click "roll back to this version" ───────────────────────────
//
// Revert is a FORWARD promote that bypasses the review gate (the safety net); on the session lane it
// carries the SAME confirmed owner|reviewer gate as approve, plus a cheap pre-stage fence so a plain
// member never triggers the forward-commit staging. These witness: the reviewer happy path + byte-
// identical replay, the owner|reviewer/member/stranger role matrix (a member's refusal is SYNTHESIZED,
// unlike approve's durable one — the pre-stage recording rule), the not-accepted-target refusal, the
// stale CAS CONFLICT (no credential to fail), cross-lane op-id closure, and self-host denial.

/// A session revert on the CLOUD posture, panicking only on a store fault.
async fn revert_session(
    fx: &Fixture,
    w: &WorkspaceId,
    s: &SkillId,
    good: CommitId,
    expected: Generation,
    rid: &str,
    email: &str,
) -> crate::SetCurrentReceipt {
    fx.authority
        .revert_session(w, s, good, expected, rid, email, CLOUD, CREATED_AT, NOW)
        .await
        .unwrap()
}

/// Stand two ACCEPTED trunk versions: genesis (`v0`, bytes `b"v0"`) landing `current` at (1,1), then a
/// direct-publish child (`v1`, bytes `b"v1"`) at (1,2). Returns `(v0, v1, v0_digest)`; the caller must
/// have `register`ed the `dk` device.
async fn two_versions(
    fx: &Fixture,
    w: &WorkspaceId,
    s: &SkillId,
    key: &[u8; 32],
    op0: &str,
    op1: &str,
) -> (CommitId, CommitId, [u8; 32]) {
    let r0 = publish(
        fx,
        key,
        "dk",
        w,
        s,
        op0,
        genesis(vec![file("SKILL.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let v0 = current_commit(fx, w, s).await;
    publish(
        fx,
        key,
        "dk",
        w,
        s,
        op1,
        child(v0, vec![file("SKILL.md", b"v1")]),
        gn(1, 1),
    )
    .await;
    let v1 = current_commit(fx, w, s).await;
    (v0, v1, r0.bundle_digest.expect("genesis records a digest"))
}

#[sqlx::test]
async fn a_session_revert_by_a_reviewer_moves_current_forward_and_replays_byte_identically(
    pool: PgPool,
) {
    let fx = Fixture::new(pool.clone(), "srv-rev-ok").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(70);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let (v0, v1, v0_digest) = two_versions(
        &fx,
        &w,
        &s,
        &key,
        "70000000-0000-4000-8000-000000000001",
        "70000000-0000-4000-8000-000000000002",
    )
    .await;

    // current is v1 at (1,2). A reviewer rolls back to v0 from the browser ⇒ a FORWARD move to (1,3)
    // carrying v0's bytes (a NEW commit, nothing deleted); the pointer never moves backward.
    let rid = "70000000-0000-4000-8000-000000000003";
    let r = revert_session(&fx, &w, &s, v0, gn(1, 2), rid, "reviewer@acme.com").await;
    assert_eq!(r.outcome, TerminalOutcome::Ok);
    assert_eq!(r.current, Some(gn(1, 3)));
    assert_eq!(
        r.bundle_digest,
        Some(v0_digest),
        "the forward commit restores v0's bytes"
    );
    let forward = r.version_id.expect("an OK revert names its forward commit");
    assert_ne!(forward, v0, "revert is a NEW forward commit, not v0 itself");
    assert_eq!(current_commit(&fx, &w, &s).await, forward);

    // The receipt is recorded on the SESSION lane (method + acting email), not a device-lane write.
    let rows = receipts_for(&pool, "w_acme", rid).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1.as_str(), rows[0].3.as_str()),
        ("reviewer@acme.com", "web_session", "OK")
    );
    assert!(
        rows[0].2.is_some(),
        "the session receipt binds a request_sha256"
    );

    // A lost-ack retry (same request id) replays the byte-identical OK — the pre-txn stable replay,
    // keyed on the acting email + request identity (NOT the forward commit, which re-parents on current).
    let replayed = revert_session(&fx, &w, &s, v0, gn(1, 2), rid, "reviewer@acme.com").await;
    assert_eq!(replayed, r);
    assert_eq!(
        current_commit(&fx, &w, &s).await,
        forward,
        "no double-apply"
    );
    assert_eq!(receipts_for(&pool, "w_acme", rid).await.len(), 1);
    let _ = v1;
}

#[sqlx::test]
async fn session_revert_needs_owner_or_reviewer_and_a_members_refusal_is_synthesized(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "srv-rev-role").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(71);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "owner@acme.com", "owner").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    seat(&fx, &w, "member@acme.com", "member").await;
    let (v0, v1, _d) = two_versions(
        &fx,
        &w,
        &s,
        &key,
        "71000000-0000-4000-8000-000000000001",
        "71000000-0000-4000-8000-000000000002",
    )
    .await;

    // A confirmed plain MEMBER: the pre-stage fence refuses with a machine-branchable role denial that
    // is SYNTHESIZED, never persisted — a revert stages a forward commit before the txn, so an
    // unauthorized member is turned away before that git work and never grows the ledger (the lane's
    // gate-before-reach posture; deliberately unlike approve's durable member denial, which stages
    // nothing).
    let rid_m = "71000000-0000-4000-8000-000000000003";
    let rm = revert_session(&fx, &w, &s, v0, gn(1, 2), rid_m, "member@acme.com").await;
    assert_eq!(rm.outcome, TerminalOutcome::Denied);
    assert_eq!(code_of(&rm).as_deref(), Some("REVIEWER_ROLE_REQUIRED"));
    assert!(
        receipts_for(&pool, "w_acme", rid_m).await.is_empty(),
        "a member's pre-stage revert refusal is synthesized, never persisted"
    );

    // A stranger with no seat: the ONE uniform acting denial, likewise synthesized.
    let rid_x = "71000000-0000-4000-8000-000000000004";
    let rx = revert_session(&fx, &w, &s, v0, gn(1, 2), rid_x, "stranger@acme.com").await;
    assert_eq!(rx.outcome, TerminalOutcome::Denied);
    assert!(receipts_for(&pool, "w_acme", rid_x).await.is_empty());

    // Neither refusal moved `current`.
    assert_eq!(current_commit(&fx, &w, &s).await, v1);

    // An OWNER rolls back to v0 ⇒ (1,3); then a REVIEWER rolls forward to v1 ⇒ (1,4). Both promote.
    let ro = revert_session(
        &fx,
        &w,
        &s,
        v0,
        gn(1, 2),
        "71000000-0000-4000-8000-000000000005",
        "owner@acme.com",
    )
    .await;
    assert_eq!(ro.outcome, TerminalOutcome::Ok);
    assert_eq!(ro.current, Some(gn(1, 3)));
    let rr = revert_session(
        &fx,
        &w,
        &s,
        v1,
        gn(1, 3),
        "71000000-0000-4000-8000-000000000006",
        "reviewer@acme.com",
    )
    .await;
    assert_eq!(rr.outcome, TerminalOutcome::Ok);
    assert_eq!(rr.current, Some(gn(1, 4)));
}

#[sqlx::test]
async fn a_recorded_session_revert_replays_even_after_the_actor_is_demoted(pool: PgPool) {
    // The idempotency contract survives a role change: a reviewer whose successful revert's ack was lost,
    // then demoted to a plain member, retries the SAME request_id and is owed the stored `Reverted` — NOT a
    // fresh role denial. The pre-stage replay runs BEFORE the role fence (mirroring the in-txn path's
    // replay-before-authz), so a recorded result always replays.
    let fx = Fixture::new(pool.clone(), "srv-rev-demote").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(76);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let (v0, _v1, _d) = two_versions(
        &fx,
        &w,
        &s,
        &key,
        "76000000-0000-4000-8000-000000000001",
        "76000000-0000-4000-8000-000000000002",
    )
    .await;

    // The reviewer rolls back to v0 ⇒ a durable web_session OK on slot R.
    let rid = "76000000-0000-4000-8000-000000000003";
    let ok = revert_session(&fx, &w, &s, v0, gn(1, 2), rid, "reviewer@acme.com").await;
    assert_eq!(ok.outcome, TerminalOutcome::Ok);

    // Demote her to a plain member (an owner reroled her seat between the lost ack and the retry).
    sqlx::query(
        "UPDATE workspace_member SET role = 'member' WHERE workspace_id = $1 AND principal = $2",
    )
    .bind("w_acme")
    .bind("reviewer@acme.com")
    .execute(&pool)
    .await
    .unwrap();

    // The SAME request replays the byte-identical OK — the pre-stage replay precedes the role fence, so the
    // demotion never turns a recorded success into a denial.
    let replayed = revert_session(&fx, &w, &s, v0, gn(1, 2), rid, "reviewer@acme.com").await;
    assert_eq!(replayed, ok);
    assert_eq!(
        code_of(&replayed),
        None,
        "a replayed OK carries no denial code"
    );

    // A FRESH request from the now-demoted member IS refused (the fence still gates a new revert).
    let fresh = revert_session(
        &fx,
        &w,
        &s,
        _v1,
        ok.current.unwrap(),
        "76000000-0000-4000-8000-000000000004",
        "reviewer@acme.com",
    )
    .await;
    assert_eq!(fresh.outcome, TerminalOutcome::Denied);
    assert_eq!(code_of(&fresh).as_deref(), Some("REVIEWER_ROLE_REQUIRED"));
}

#[sqlx::test]
async fn a_session_revert_to_a_non_accepted_version_is_refused_without_a_durable_row(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "srv-rev-notacc").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(72);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    // genesis + an OPEN proposal (never accepted): its candidate is rooted via proposal_object, NEVER
    // commit_object — so it is not an accepted trunk version.
    let (g, cp, _d) = open_proposal(
        &fx,
        &w,
        &s,
        &key,
        "72000000-0000-4000-8000-000000000001",
        "72000000-0000-4000-8000-000000000002",
    )
    .await;

    // Rolling back to the un-accepted proposal candidate ⇒ PERMANENT_FAILURE (forward-promoting its tree
    // would smuggle un-reviewed bytes past the gate). Synthesized on the session lane — no durable row.
    let rid = "72000000-0000-4000-8000-000000000003";
    let r = revert_session(&fx, &w, &s, cp, gn(1, 1), rid, "reviewer@acme.com").await;
    assert_eq!(r.outcome, TerminalOutcome::PermanentFailure);
    assert_eq!(
        msg_of(&r).as_deref(),
        Some("revert target is not an accepted version")
    );
    assert!(
        receipts_for(&pool, "w_acme", rid).await.is_empty(),
        "a session pre-transaction target refusal is synthesized, never persisted"
    );
    assert_eq!(current_commit(&fx, &w, &s).await, g, "current is unmoved");
}

#[sqlx::test]
async fn a_stale_session_revert_conflicts_with_the_live_generation(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "srv-rev-stale").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(73);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let (v0, v1, _d) = two_versions(
        &fx,
        &w,
        &s,
        &key,
        "73000000-0000-4000-8000-000000000001",
        "73000000-0000-4000-8000-000000000002",
    )
    .await;
    // Another publish advances `current` to (1,3): the reviewer's page rendered against (1,2) is stale.
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "73000000-0000-4000-8000-000000000003",
        child(v1, vec![file("SKILL.md", b"v2")]),
        gn(1, 2),
    )
    .await;

    // The session revert at the STALE expected (1,2) ⇒ a clean CONFLICT carrying the live (1,3). Unlike
    // the device lane (where a stale parent surfaces as a DENIED), the keyless session lane
    // falls straight through to the whole-(epoch,seq) CAS — the same CONFLICT approve gets.
    let rid = "73000000-0000-4000-8000-000000000004";
    let r = revert_session(&fx, &w, &s, v0, gn(1, 2), rid, "reviewer@acme.com").await;
    assert_eq!(r.outcome, TerminalOutcome::Conflict);
    assert_eq!(r.current, Some(gn(1, 3)));
}

#[sqlx::test]
async fn a_device_revert_cannot_reuse_a_session_reverts_request_id(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "srv-rev-xlane").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(74);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let (v0, v1, _d) = two_versions(
        &fx,
        &w,
        &s,
        &key,
        "74000000-0000-4000-8000-000000000001",
        "74000000-0000-4000-8000-000000000002",
    )
    .await;

    // A reviewer rolls back to v0 with request id R ⇒ (1,3), a web_session receipt on slot R.
    let shared = "74000000-0000-4000-8000-000000000003";
    let sr = revert_session(&fx, &w, &s, v0, gn(1, 2), shared, "reviewer@acme.com").await;
    assert_eq!(sr.outcome, TerminalOutcome::Ok);

    // A DEVICE revert that reuses R as its op id fails closed: the lane-blind (ws, op_id) replay probe
    // sees the session's web_session row and refuses the device caller as OP_ID_REUSED (its own staged
    // forward-commit lease released on the Mismatch arm — no strand). The two lanes never replay each
    // other.
    let dop = op(shared);
    let ddev = revert_request(&w, "dk", gn(1, 3));
    let rd = fx
        .authority
        .revert(
            &w,
            &s,
            v1,
            ddev,
            "d_test",
            "topos revert",
            &dop,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(rd.outcome, TerminalOutcome::PermanentFailure);
    assert_eq!(code_of(&rd).as_deref(), Some("OP_ID_REUSED"));
    // Only the original session receipt lives on slot R.
    let rows = receipts_for(&pool, "w_acme", shared).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1.as_str(), "web_session");
}

#[sqlx::test]
async fn revert_session_answers_a_reviewer_on_self_host(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "srv-rev-selfhost").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(75);
    register(&fx, &w, &s, "dk", &key, "author@acme.com").await;
    seat(&fx, &w, "reviewer@acme.com", "reviewer").await;
    let (v0, v1, v0_digest) = two_versions(
        &fx,
        &w,
        &s,
        &key,
        "75000000-0000-4000-8000-000000000001",
        "75000000-0000-4000-8000-000000000002",
    )
    .await;

    // A self-host plane ANSWERS the session revert for a confirmed reviewer exactly like a hosted one:
    // the acting gate is the confirmed owner|reviewer seat, identical on both postures (the product app
    // serves self-hosted deployments through this session lane). The revert lands a FORWARD commit
    // carrying v0's bytes — the pointer never moves backward.
    let rid = "75000000-0000-4000-8000-000000000003";
    let r = fx
        .authority
        .revert_session(
            &w,
            &s,
            v0,
            gn(1, 2),
            rid,
            "reviewer@acme.com",
            DeploymentMode::SelfHost,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Ok);
    assert_eq!(r.current, Some(gn(1, 3)));
    assert_eq!(
        r.bundle_digest,
        Some(v0_digest),
        "the forward commit restores v0's bytes"
    );
    let forward = r.version_id.expect("an OK revert names its forward commit");
    assert_ne!(forward, v0, "revert is a NEW forward commit, not v0 itself");
    assert_ne!(forward, v1, "current moved off v1");
    assert_eq!(current_commit(&fx, &w, &s).await, forward);
    assert_eq!(receipts_for(&pool, "w_acme", rid).await.len(), 1);
}
