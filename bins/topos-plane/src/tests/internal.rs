//! The INTERNAL session lane (`/internal/v1/*`) — the internal-token auth shape + a full cloud happy path
//! over the session wrappers, driven through `router(state)` with `oneshot` (no socket).
//!
//! The lane fronts the lib-only session wrappers for a downstream session-authenticated composing surface:
//! the internal bearer token gates the whole lane (404-invisible until configured), the acting principal
//! rides the `x-topos-acting-email` header, and the wrappers' own in-transaction gates re-verify the roster
//! rows — this suite pins the auth ordering, the read/write outcome shapes, and the miss uniformity.

use super::*;

// ── lane constants ─────────────────────────────────────────────────────────────────────────────────────

const INTERNAL_TOKEN: &str = "int_secret";
const ACTING_HEADER: &str = "x-topos-acting-email";
/// The workspace seats (all confirmed): the owner (also the proposer + the acting owner), a reviewer (the
/// approver + the reader), a plain member (the role-gate witness), and a stranger with no seat.
const OWNER_EMAIL: &str = "owner@acme.com";
const REVIEWER_EMAIL: &str = "reviewer@acme.com";
const MEMBER_EMAIL: &str = "member@acme.com";
const STRANGER_EMAIL: &str = "stranger@acme.com";
/// The owner device's workspace credential — it publishes the genesis + opens the seed proposal.
const INT_OWNER_CRED: &str = "cred_int_owner";
const OP_GENESIS: &str = "41000000-0000-4000-8000-000000000001";
const OP_PROPOSE: &str = "41000000-0000-4000-8000-000000000002";

// ── fixture ────────────────────────────────────────────────────────────────────────────────────────────

/// A seeded CLOUD plane for the internal lane: a workspace with owner/reviewer/member seats, a published
/// genesis at `(1,1)`, and one open proposal (a child of genesis). `internal_token = Some` configures the
/// lane; `None` leaves it 404-invisible.
struct InternalCtx {
    dir: PathBuf,
    state: PlaneState,
    /// The test's own handle on the database — direct asserts on the guarded SQL functions.
    pool: PgPool,
    genesis_vid: [u8; 32],
    proposal_vid: [u8; 32],
}

impl Drop for InternalCtx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl InternalCtx {
    fn app(&self) -> axum::Router {
        router(self.state.clone())
    }

    fn pool(&self) -> &PgPool {
        &self.pool
    }
}

async fn internal_setup(pool: PgPool, tag: &str, internal_token: Option<&str>) -> InternalCtx {
    let dir = unique_dir(tag);
    let db_pool = pool.clone();
    let authority = Authority::from_pool(pool, &dir.join("git"), &dir.join("large"))
        .expect("open authority")
        .with_enrollment_config(EnrollmentConfig {
            secret_path: dir.join("enroll.secret"),
            base_url: "https://plane.test".to_owned(),
            verify_base_url: None,
            link_base_url: None,
            deployment_mode: DeploymentMode::Cloud,
            enrollment_method: "passcode".to_owned(),
        })
        .expect("enrollment config");
    let ws = WorkspaceId::parse(WS).unwrap();
    authority
        .seed_workspace(&ws, "Acme", "unverified", "cloud")
        .await
        .unwrap();
    let owner = Principal::parse(OWNER_EMAIL).unwrap();
    authority
        .seed_workspace_member(&ws, &owner, "owner", "confirmed")
        .await
        .unwrap();
    authority
        .seed_workspace_member(
            &ws,
            &Principal::parse(REVIEWER_EMAIL).unwrap(),
            "reviewer",
            "confirmed",
        )
        .await
        .unwrap();
    authority
        .seed_workspace_member(
            &ws,
            &Principal::parse(MEMBER_EMAIL).unwrap(),
            "member",
            "confirmed",
        )
        .await
        .unwrap();
    // The owner's device carries a credential so it can publish the genesis + open the proposal.
    authority
        .seed_device(
            &ws,
            "dk_int_owner",
            &dev_pubkey(41),
            &owner,
            false,
            INT_OWNER_CRED,
        )
        .await
        .unwrap();

    let mut state = PlaneState::new(Arc::new(authority))
        .with_rate_limit(crate::Limits {
            burst: 1.0,
            refill_per_sec: 1.0,
            enabled: false,
        })
        .with_enroll_config(crate::state::EnrollConfig {
            base_url: "https://plane.test".to_owned(),
            verify_base_url: "https://plane.test".to_owned(),
            link_base_url: "https://plane.test".to_owned(),
            strict_deployment_mode: Some(DeploymentMode::Cloud),
            deployment_mode: DeploymentMode::Cloud,
            enrollment_method: "passcode".to_owned(),
            smtp: None,
        });
    if let Some(token) = internal_token {
        state = state.with_internal_token(token);
    }

    // Genesis publish (owner device) → current at (1,1).
    let receipt = state
        .authority()
        .seed_published_genesis(
            &ws,
            &SkillId::parse(SKILL).unwrap(),
            INT_OWNER_CRED,
            &OpId::parse(OP_GENESIS).unwrap(),
            vec![file("SKILL.md", b"genesis v0\n")],
            AUTHOR,
            MESSAGE,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .expect("seed genesis");
    let genesis_vid = receipt.version_id.unwrap().0;

    // Open a proposal (a child of genesis) over the device propose route, presented with the owner credential.
    let files = vec![file("SKILL.md", b"a proposed change\n")];
    let (proposal_vid, _digest) = compute_ids(&[genesis_vid], &files);
    let (s, _, _) = send(
        router(state.clone()),
        post_as(
            "/v1/proposals",
            candidate_body(OP_PROPOSE, gn(1, 1), &[genesis_vid], &files),
            Some(INT_OWNER_CRED),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "open the seed proposal");

    InternalCtx {
        dir,
        state,
        pool: db_pool,
        genesis_vid,
        proposal_vid,
    }
}

// ── request builders ─────────────────────────────────────────────────────────────────────────────────

/// A lane request for any method: an optional internal bearer token + an optional acting-email header.
fn int_req(
    method: &str,
    uri: &str,
    token: Option<&str>,
    acting: Option<&str>,
    body: Vec<u8>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(acting) = acting {
        builder = builder.header(ACTING_HEADER, acting);
    }
    builder.body(Body::from(body)).unwrap()
}

fn int_get(uri: &str, token: Option<&str>, acting: Option<&str>) -> Request<Body> {
    int_req("GET", uri, token, acting, vec![])
}

fn json_body(value: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}

/// The JSON `outcome` body of a lane WRITE (a `serde_json::Value` so a test reads `["outcome"]`/`["reason"]`).
fn outcome(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("a JSON outcome body")
}

fn current_uri() -> String {
    format!("/internal/v1/workspaces/{WS}/skills/{SKILL}/current")
}

// ══ 1. the lane is invisible without a configured internal token ═══════════════════════════════════════

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn the_internal_lane_is_invisible_without_a_token(pool: PgPool) {
    // No internal token configured ⇒ 404 on every representative route, even with a bearer + a valid acting
    // email + a valid body — a composition that never sets the token can't expose the lane. A malformed body
    // must NOT make the disabled route observable (auth is decided before any parse): still a 404, never 400.
    let ctx = internal_setup(pool, "internal-off", None).await;

    // A representative READ (GET current).
    let (status, _h, bytes) = send(
        ctx.app(),
        int_get(&current_uri(), Some("whatever"), Some(REVIEWER_EMAIL)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let env = envelope(&bytes);
    assert!(!env.ok);
    assert!(env.receipt.is_none(), "the wrapper was never reached");

    // A representative WRITE (POST approve) with a valid body.
    let approve = format!(
        "/internal/v1/workspaces/{WS}/skills/{SKILL}/proposals/{}/approve",
        hex::encode(ctx.proposal_vid)
    );
    let body = json_body(serde_json::json!({
        "request_id": "41000000-0000-4000-8000-0000000000a0",
        "expected_epoch": 1, "expected_seq": 1,
    }));
    let (status, _h, _b) = send(
        ctx.app(),
        int_req(
            "POST",
            &approve,
            Some("whatever"),
            Some(REVIEWER_EMAIL),
            body,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The SAME write with a MALFORMED body → still the indistinguishable 404 (never a 400 body-parse oracle).
    let (status, _h, _b) = send(
        ctx.app(),
        int_req(
            "POST",
            &approve,
            Some("whatever"),
            Some(REVIEWER_EMAIL),
            b"not json".to_vec(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // And a roster-remove POST with a malformed body → still 404.
    let remove = format!("/internal/v1/workspaces/{WS}/roster/remove");
    let (status, _h, _b) = send(
        ctx.app(),
        int_req(
            "POST",
            &remove,
            Some("whatever"),
            Some(REVIEWER_EMAIL),
            b"not json".to_vec(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ══ 2. configured lane: wrong/missing bearer → 401; missing acting-email → 400 (only past a good bearer) ══

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn configured_lane_gates_the_bearer_then_the_acting_principal(pool: PgPool) {
    let ctx = internal_setup(pool, "internal-auth", Some(INTERNAL_TOKEN)).await;
    let uri = current_uri();

    // No Authorization header ⇒ an honest 401 (the configured internal secret, not an existence oracle).
    let (status, _h, _b) = send(ctx.app(), int_get(&uri, None, Some(REVIEWER_EMAIL))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A wrong bearer ⇒ 401 (even WITH a valid acting email).
    let (status, _h, _b) = send(
        ctx.app(),
        int_get(&uri, Some("wrong"), Some(REVIEWER_EMAIL)),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The correct bearer but NO acting-email header ⇒ 400 — decided ONLY past a correct bearer (never an
    // oracle), and the message says which input is missing (proving the acting check runs before any body/id
    // parse).
    let (status, _h, bytes) = send(ctx.app(), int_get(&uri, Some(INTERNAL_TOKEN), None)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let env = envelope(&bytes);
    let message = env
        .error
        .and_then(|e| {
            e.context
                .get("message")
                .and_then(|m| m.as_str().map(str::to_owned))
        })
        .unwrap_or_default();
    assert!(
        message.contains("acting principal"),
        "the 400 names the missing acting principal: {message:?}"
    );

    // An empty acting-email header is the same 400 (blank folds to missing).
    let (status, _h, _b) = send(ctx.app(), int_get(&uri, Some(INTERNAL_TOKEN), Some("   "))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ══ 3. the full cloud happy path over the session wrappers ═════════════════════════════════════════════

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn the_full_cloud_happy_path(pool: PgPool) {
    let ctx = internal_setup(pool, "internal-happy", Some(INTERNAL_TOKEN)).await;
    let g_hex = hex::encode(ctx.genesis_vid);
    let p_hex = hex::encode(ctx.proposal_vid);

    // ── reads (member-scoped; `no-store`) ──────────────────────────────────────────────────────────────

    // current → 200 + the stored WireCurrentRecord JSON verbatim (generation (1,1), the genesis version).
    let (status, headers, bytes) = send(
        ctx.app(),
        int_get(&current_uri(), Some(INTERNAL_TOKEN), Some(REVIEWER_EMAIL)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap(),
        "application/json"
    );
    assert_eq!(
        headers
            .get(header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap(),
        "no-store"
    );
    let record: topos_types::WireCurrentRecord =
        serde_json::from_slice(&bytes).expect("the body is a WireCurrentRecord");
    assert_eq!(record.record.generation, gn(1, 1));
    assert_eq!(record.record.version_id, g_hex);

    // version → 200 + the wire version metadata (the genesis commit's metadata).
    let version_uri = format!("/internal/v1/workspaces/{WS}/skills/{SKILL}/versions/{g_hex}");
    let (status, _h, bytes) = send(
        ctx.app(),
        int_get(&version_uri, Some(INTERNAL_TOKEN), Some(REVIEWER_EMAIL)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let meta: topos_types::requests::WireVersionMeta =
        serde_json::from_slice(&bytes).expect("the body is a WireVersionMeta");
    assert_eq!(meta.version_id, g_hex);

    // proposals list → 200 + the one open proposal (its @hash + base).
    let list_uri = format!("/internal/v1/workspaces/{WS}/skills/{SKILL}/proposals");
    let (status, _h, bytes) = send(
        ctx.app(),
        int_get(&list_uri, Some(INTERNAL_TOKEN), Some(REVIEWER_EMAIL)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let list: topos_types::requests::WireProposalList =
        serde_json::from_slice(&bytes).expect("the body is a WireProposalList");
    assert_eq!(list.proposals.len(), 1);
    assert_eq!(list.proposals[0].version_id, p_hex);
    assert_eq!(list.proposals[0].base_generation, gn(1, 1));

    // proposal detail → 200 + the stored facts (open, the owner is the proposer, review_required off).
    let detail_uri = format!("/internal/v1/workspaces/{WS}/skills/{SKILL}/proposals/{p_hex}");
    let (status, _h, bytes) = send(
        ctx.app(),
        int_get(&detail_uri, Some(INTERNAL_TOKEN), Some(REVIEWER_EMAIL)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let detail = outcome(&bytes);
    assert_eq!(detail["version_id"].as_str(), Some(p_hex.as_str()));
    assert_eq!(detail["status"].as_str(), Some("open"));
    assert_eq!(detail["proposer"].as_str(), Some(OWNER_EMAIL));
    assert_eq!(detail["review_required"].as_bool(), Some(false));
    assert_eq!(detail["base_epoch"].as_u64(), Some(1));
    assert_eq!(detail["base_seq"].as_u64(), Some(1));

    // ── writes ──────────────────────────────────────────────────────────────────────────────────────────

    let approve_uri =
        format!("/internal/v1/workspaces/{WS}/skills/{SKILL}/proposals/{p_hex}/approve");
    let reject_uri =
        format!("/internal/v1/workspaces/{WS}/skills/{SKILL}/proposals/{p_hex}/reject");

    // reject with an EMPTY reason ⇒ a typed DENIED (REASON_REQUIRED); the proposal stays open.
    let (status, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &reject_uri,
            Some(INTERNAL_TOKEN),
            Some(REVIEWER_EMAIL),
            json_body(serde_json::json!({
                "request_id": "41000000-0000-4000-8000-0000000000b0",
                "expected_epoch": 1, "expected_seq": 1, "reason": "",
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v = outcome(&bytes);
    assert_eq!(v["outcome"].as_str(), Some("denied"));
    assert!(
        v["reason"].as_str().unwrap_or_default().contains("reason"),
        "the empty-reason reject denies with a reason-required message: {v:?}"
    );

    // approve with a plain MEMBER acting email ⇒ a typed DENIED (the reviewer role gate).
    let (status, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &approve_uri,
            Some(INTERNAL_TOKEN),
            Some(MEMBER_EMAIL),
            json_body(serde_json::json!({
                "request_id": "41000000-0000-4000-8000-0000000000b1",
                "expected_epoch": 1, "expected_seq": 1,
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v = outcome(&bytes);
    assert_eq!(
        v["outcome"].as_str(),
        Some("denied"),
        "member is role-gated: {v:?}"
    );
    assert!(!v["reason"].as_str().unwrap_or_default().is_empty());

    // approve with the REVIEWER acting email + the correct expected generation ⇒ APPROVED, pointer moves.
    let (status, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &approve_uri,
            Some(INTERNAL_TOKEN),
            Some(REVIEWER_EMAIL),
            json_body(serde_json::json!({
                "request_id": "41000000-0000-4000-8000-0000000000b2",
                "expected_epoch": 1, "expected_seq": 1,
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(outcome(&bytes)["outcome"].as_str(), Some("approved"));

    // The pointer moved: current is now (1,2) at the accepted candidate.
    let (_s, _h, bytes) = send(
        ctx.app(),
        int_get(&current_uri(), Some(INTERNAL_TOKEN), Some(REVIEWER_EMAIL)),
    )
    .await;
    let record: topos_types::WireCurrentRecord = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(record.record.generation, gn(1, 2));
    assert_eq!(record.record.version_id, p_hex);

    // revert to the prior good version (the genesis) from the OWNER ⇒ REVERTED (a forward promote to (1,3)).
    let revert_uri = format!("/internal/v1/workspaces/{WS}/skills/{SKILL}/reverts");
    let (status, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &revert_uri,
            Some(INTERNAL_TOKEN),
            Some(OWNER_EMAIL),
            json_body(serde_json::json!({
                "request_id": "41000000-0000-4000-8000-0000000000b3",
                "good_version_id": g_hex,
                "expected_epoch": 1, "expected_seq": 2,
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(outcome(&bytes)["outcome"].as_str(), Some("reverted"));

    // remove_member on the LAST OWNER ⇒ DENIED (the last-owner lockout).
    let remove_uri = format!("/internal/v1/workspaces/{WS}/roster/remove");
    let (status, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &remove_uri,
            Some(INTERNAL_TOKEN),
            Some(OWNER_EMAIL),
            json_body(serde_json::json!({
                "request_id": "41000000-0000-4000-8000-0000000000b4",
                "email": OWNER_EMAIL,
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v = outcome(&bytes);
    assert_eq!(
        v["outcome"].as_str(),
        Some("denied"),
        "last owner is locked in: {v:?}"
    );

    // The review-required DEFAULT is a database policy decision now (`topos_set_review_default`,
    // owner-gated in the function itself — no route on this lane): a member is refused, the owner
    // sets it, and the flip is OBSERVABLE via a fresh proposal-detail read.
    let denied: (String,) = sqlx::query_as("SELECT topos_set_review_default($1, $2, 1)")
        .bind(WS)
        .bind(REVIEWER_EMAIL)
        .fetch_one(ctx.pool())
        .await
        .expect("the guarded setter answers");
    assert_eq!(
        denied.0, "owner_role_required",
        "the role gate runs IN the function"
    );
    let set: (String,) = sqlx::query_as("SELECT topos_set_review_default($1, $2, 1)")
        .bind(WS)
        .bind(OWNER_EMAIL)
        .fetch_one(ctx.pool())
        .await
        .expect("the guarded setter answers");
    assert_eq!(set.0, "set");
    let (_s, _h, bytes) = send(
        ctx.app(),
        int_get(&detail_uri, Some(INTERNAL_TOKEN), Some(REVIEWER_EMAIL)),
    )
    .await;
    assert_eq!(
        outcome(&bytes)["review_required"].as_bool(),
        Some(true),
        "the policy flip is observable on the proposal detail"
    );
}

// ══ 4. miss uniformity: reads answer 404, writes answer their 200 not_found — same for both miss causes ══

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn unknown_target_and_non_member_answer_the_same_shape(pool: PgPool) {
    let ctx = internal_setup(pool, "internal-miss", Some(INTERNAL_TOKEN)).await;
    let p_hex = hex::encode(ctx.proposal_vid);

    // READ routes: an unknown skill (valid member acting) and a non-member acting email (valid skill) both
    // answer the SAME uniform 404.
    let unknown_skill = format!("/internal/v1/workspaces/{WS}/skills/s_nope/current");
    let (s1, _h, b1) = send(
        ctx.app(),
        int_get(&unknown_skill, Some(INTERNAL_TOKEN), Some(REVIEWER_EMAIL)),
    )
    .await;
    let (s2, _h, b2) = send(
        ctx.app(),
        int_get(&current_uri(), Some(INTERNAL_TOKEN), Some(STRANGER_EMAIL)),
    )
    .await;
    assert_eq!(s1, StatusCode::NOT_FOUND);
    assert_eq!(s2, StatusCode::NOT_FOUND);
    for bytes in [&b1, &b2] {
        let env = envelope(bytes);
        assert!(!env.ok);
        assert!(env.receipt.is_none());
    }

    // WRITE routes: an unknown version (valid reviewer acting) and a non-member acting email (valid version)
    // both answer the SAME 200 `not_found` outcome (the composing page renders the uniform miss itself).
    let unknown_version = "ab".repeat(32);
    let approve_unknown =
        format!("/internal/v1/workspaces/{WS}/skills/{SKILL}/proposals/{unknown_version}/approve");
    let (s3, _h, b3) = send(
        ctx.app(),
        int_req(
            "POST",
            &approve_unknown,
            Some(INTERNAL_TOKEN),
            Some(REVIEWER_EMAIL),
            json_body(serde_json::json!({
                "request_id": "41000000-0000-4000-8000-0000000000c0",
                "expected_epoch": 1, "expected_seq": 1,
            })),
        ),
    )
    .await;
    let approve_known =
        format!("/internal/v1/workspaces/{WS}/skills/{SKILL}/proposals/{p_hex}/approve");
    let (s4, _h, b4) = send(
        ctx.app(),
        int_req(
            "POST",
            &approve_known,
            Some(INTERNAL_TOKEN),
            Some(STRANGER_EMAIL),
            json_body(serde_json::json!({
                "request_id": "41000000-0000-4000-8000-0000000000c1",
                "expected_epoch": 1, "expected_seq": 1,
            })),
        ),
    )
    .await;
    assert_eq!(s3, StatusCode::OK);
    assert_eq!(s4, StatusCode::OK);
    assert_eq!(outcome(&b3)["outcome"].as_str(), Some("not_found"));
    assert_eq!(outcome(&b4)["outcome"].as_str(), Some("not_found"));
}

// ══ 5. the skill-lifecycle ceremonies (archive / unarchive / delete / purge / rename) ═══════════════════

/// One lifecycle POST (the ceremonies are id-keyed: `{skill}` carries the immutable skill id).
fn lifecycle_uri(skill_id: &str, act: &str) -> String {
    format!("/internal/v1/workspaces/{WS}/skills/{skill_id}/{act}")
}

/// The skill's catalog `(status, name)` row (raw `sqlx` against the test's own pool handle).
async fn catalog_row(pool: &PgPool, skill_id: &str) -> Option<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT status, name FROM catalog WHERE workspace_id = $1 AND skill_id = $2",
    )
    .bind(WS)
    .bind(skill_id)
    .fetch_optional(pool)
    .await
    .expect("the catalog probe answers")
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn the_lifecycle_ceremonies_land_and_refuse_typed(pool: PgPool) {
    let ctx = internal_setup(pool, "internal-lifecycle", Some(INTERNAL_TOKEN)).await;
    let g_hex = hex::encode(ctx.genesis_vid);
    let p_hex = hex::encode(ctx.proposal_vid);

    // Approve the seed proposal (reviewer) so the pointer sits at (1,2) = the proposal's version —
    // giving purge a non-current target (the genesis) and a current one (the candidate).
    let approve_uri =
        format!("/internal/v1/workspaces/{WS}/skills/{SKILL}/proposals/{p_hex}/approve");
    let (status, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &approve_uri,
            Some(INTERNAL_TOKEN),
            Some(REVIEWER_EMAIL),
            json_body(serde_json::json!({
                "request_id": "42000000-0000-4000-8000-0000000000a1",
                "expected_epoch": 1, "expected_seq": 1,
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(outcome(&bytes)["outcome"].as_str(), Some("approved"));

    // ── purge ───────────────────────────────────────────────────────────────────────────────────────
    // A malformed version id is a malformed BODY (400) — parsed AFTER the guard, never an oracle.
    let (status, _h, _b) = send(
        ctx.app(),
        int_req(
            "POST",
            &lifecycle_uri(SKILL, "purge"),
            Some(INTERNAL_TOKEN),
            Some(OWNER_EMAIL),
            json_body(serde_json::json!({ "version_id": "not-hex" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Purging the CURRENT version refuses typed (`is_current` — the SQL outcome code verbatim).
    let (status, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &lifecycle_uri(SKILL, "purge"),
            Some(INTERNAL_TOKEN),
            Some(OWNER_EMAIL),
            json_body(serde_json::json!({ "version_id": p_hex })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v = outcome(&bytes);
    assert_eq!(v["outcome"].as_str(), Some("denied"));
    assert_eq!(v["reason"].as_str(), Some("is_current"));

    // Purging the non-current genesis lands.
    let (status, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &lifecycle_uri(SKILL, "purge"),
            Some(INTERNAL_TOKEN),
            Some(OWNER_EMAIL),
            json_body(serde_json::json!({ "version_id": g_hex })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(outcome(&bytes)["outcome"].as_str(), Some("purged"));

    // ── rename ──────────────────────────────────────────────────────────────────────────────────────
    // A plain member is refused typed; the owner renames; the OLD name keeps resolving as a hint.
    let (_s, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &lifecycle_uri(SKILL, "rename"),
            Some(INTERNAL_TOKEN),
            Some(MEMBER_EMAIL),
            json_body(serde_json::json!({ "new_name": "ship" })),
        ),
    )
    .await;
    let v = outcome(&bytes);
    assert_eq!(v["outcome"].as_str(), Some("denied"));
    assert_eq!(v["reason"].as_str(), Some("owner_role_required"));

    let old_name = catalog_row(ctx.pool(), SKILL).await.expect("cataloged").1;
    let (status, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &lifecycle_uri(SKILL, "rename"),
            Some(INTERNAL_TOKEN),
            Some(OWNER_EMAIL),
            json_body(serde_json::json!({ "new_name": "ship" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v = outcome(&bytes);
    assert_eq!(v["outcome"].as_str(), Some("renamed"));
    assert_eq!(v["name"].as_str(), Some("ship"));
    assert_eq!(
        catalog_row(ctx.pool(), SKILL).await,
        Some(("active".to_owned(), "ship".to_owned()))
    );
    let (hint_id, hint_name, via): (String, String, String) =
        sqlx::query_as("SELECT skill_id, name, via FROM topos_resolve_skill($1, $2)")
            .bind(WS)
            .bind(&old_name)
            .fetch_one(ctx.pool())
            .await
            .expect("the old name still resolves");
    assert_eq!(
        (hint_id.as_str(), hint_name.as_str(), via.as_str()),
        (SKILL, "ship", "hint"),
        "the old name resolves as a hint carrying the live spelling"
    );

    // A name another identity holds refuses typed (`name_taken`).
    ctx.state
        .authority()
        .seed_catalog(
            &WorkspaceId::parse(WS).unwrap(),
            &SkillId::parse("s_docs").unwrap(),
            "docs",
        )
        .await
        .unwrap();
    let (_s, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &lifecycle_uri(SKILL, "rename"),
            Some(INTERNAL_TOKEN),
            Some(OWNER_EMAIL),
            json_body(serde_json::json!({ "new_name": "docs" })),
        ),
    )
    .await;
    let v = outcome(&bytes);
    assert_eq!(v["outcome"].as_str(), Some("denied"));
    assert_eq!(v["reason"].as_str(), Some("name_taken"));

    // ── delete is archive-first ─────────────────────────────────────────────────────────────────────
    let (_s, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &lifecycle_uri(SKILL, "delete"),
            Some(INTERNAL_TOKEN),
            Some(OWNER_EMAIL),
            json_body(serde_json::json!({})),
        ),
    )
    .await;
    let v = outcome(&bytes);
    assert_eq!(v["outcome"].as_str(), Some("denied"));
    assert_eq!(v["reason"].as_str(), Some("not_archived"));

    // ── archive → unarchive → archive → delete ──────────────────────────────────────────────────────
    // A non-owner archive is the typed role refusal.
    let (_s, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &lifecycle_uri(SKILL, "archive"),
            Some(INTERNAL_TOKEN),
            Some(MEMBER_EMAIL),
            json_body(serde_json::json!({})),
        ),
    )
    .await;
    let v = outcome(&bytes);
    assert_eq!(v["outcome"].as_str(), Some("denied"));
    assert_eq!(v["reason"].as_str(), Some("owner_role_required"));

    // The owner archives: the body carries the archived spelling and the catalog row moved to it.
    let (status, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &lifecycle_uri(SKILL, "archive"),
            Some(INTERNAL_TOKEN),
            Some(OWNER_EMAIL),
            json_body(serde_json::json!({})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v = outcome(&bytes);
    assert_eq!(v["outcome"].as_str(), Some("archived"));
    let archived_name = v["archived_name"]
        .as_str()
        .expect("archived_name")
        .to_owned();
    assert!(
        archived_name.starts_with("ship-archived-"),
        "the archived spelling carries the freed base name: {archived_name}"
    );
    assert_eq!(
        catalog_row(ctx.pool(), SKILL).await,
        Some(("archived".to_owned(), archived_name))
    );

    // Unarchive renames back.
    let (status, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &lifecycle_uri(SKILL, "unarchive"),
            Some(INTERNAL_TOKEN),
            Some(OWNER_EMAIL),
            json_body(serde_json::json!({})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v = outcome(&bytes);
    assert_eq!(v["outcome"].as_str(), Some("unarchived"));
    assert_eq!(v["name"].as_str(), Some("ship"));
    assert_eq!(
        catalog_row(ctx.pool(), SKILL).await,
        Some(("active".to_owned(), "ship".to_owned()))
    );

    // Archive again, then delete (archive-first satisfied): the catalog row is the tombstone.
    let (_s, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &lifecycle_uri(SKILL, "archive"),
            Some(INTERNAL_TOKEN),
            Some(OWNER_EMAIL),
            json_body(serde_json::json!({})),
        ),
    )
    .await;
    assert_eq!(outcome(&bytes)["outcome"].as_str(), Some("archived"));
    let (status, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &lifecycle_uri(SKILL, "delete"),
            Some(INTERNAL_TOKEN),
            Some(OWNER_EMAIL),
            json_body(serde_json::json!({})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(outcome(&bytes)["outcome"].as_str(), Some("deleted"));
    assert_eq!(
        catalog_row(ctx.pool(), SKILL).await.map(|(s, _)| s),
        Some("deleted".to_owned())
    );
}

/// The ceremonies' miss uniformity: a stranger acting on a real skill, an owner acting on an unknown
/// skill id, and an owner acting in an unknown workspace all answer the SAME 200 `not_found` body —
/// the composing page renders the uniform miss itself, and nothing distinguishes the causes.
#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn lifecycle_misses_are_the_one_uniform_not_found(pool: PgPool) {
    let ctx = internal_setup(pool, "internal-lc-miss", Some(INTERNAL_TOKEN)).await;
    let cases: [(String, &str); 3] = [
        (lifecycle_uri(SKILL, "archive"), STRANGER_EMAIL),
        (lifecycle_uri("s_nope", "archive"), OWNER_EMAIL),
        (
            format!("/internal/v1/workspaces/w_nope/skills/{SKILL}/archive"),
            OWNER_EMAIL,
        ),
    ];
    for (uri, acting) in cases {
        let (status, _h, bytes) = send(
            ctx.app(),
            int_req(
                "POST",
                &uri,
                Some(INTERNAL_TOKEN),
                Some(acting),
                json_body(serde_json::json!({})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(
            outcome(&bytes)["outcome"].as_str(),
            Some("not_found"),
            "{uri} as {acting}"
        );
    }
    // The rename twin of the unknown-id miss (the body parses, the target does not resolve).
    let (status, _h, bytes) = send(
        ctx.app(),
        int_req(
            "POST",
            &lifecycle_uri("s_nope", "rename"),
            Some(INTERNAL_TOKEN),
            Some(OWNER_EMAIL),
            json_body(serde_json::json!({ "new_name": "ship" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(outcome(&bytes)["outcome"].as_str(), Some("not_found"));
}
