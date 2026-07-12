//! WORKSPACE-STANDUP e2e — the full self-serve chain over loopback HTTP against the real plane.
//!
//! The genesis release-blocker proof: a workspace is born through each of its doors with the GENUINE
//! client (`topos::test_support::FollowHarness` driving the real `ureq` transports) against the GENUINE
//! plane (`topos_plane::router` over a real `Authority`), via the shared `common` harness.
//!
//! - **Door 1 (cloud):** an UN-ENROLLED direct `publish` goes PENDING (the sign-in envelope + the
//!   same-command resume argv), a web-verified email approves the standup (the authority op — the lib
//!   surface a composing web page calls; the OSS e2e drives it directly, the existing
//!   `confirm_external_identity` pattern), and re-invoking the SAME publish enrolls + lands the genesis
//!   in one invocation. The chain calls ZERO operator ops — asserted by construction (no mint, no
//!   admin-claim anywhere in the test) AND by the `admin_claim` table staying empty.
//! - **Door 2 (cloud):** `create_workspace` (the web door) seats the owner + mints the self-invite; the
//!   owner's agent enrolls through the web-approve leg, genesis-publishes, and invites a member whose
//!   redeem flips `invited → confirmed` and whose pull lands the bytes exactly.
//! - **Adversarial witnesses (cloud):** a leaked self-invite is inert off-roster (the client surfaces the
//!   REQUEST_ACCESS ask-an-owner guidance), approve-standup misses are the uniform NotFound, a
//!   double-approve is idempotent (ONE workspace), the 4th create hits the typed cap, and a standup
//!   session refuses every enroll identity leg (the intent guard) without being consumed.
//! - **Self-host chain:** the operator's one-time claim (`mint_admin_claim`, the lib op the bin's
//!   `mint-claim` subcommand drives) enrolls the first owner in ONE `follow <claim-link>` invocation (no
//!   web leg); publish → invite → a second client's BEARER redeem (no roster requirement) lands the bytes
//!   exactly. Plus the claim witnesses: a different device's redeem of the consumed claim is Denied, the
//!   SAME device replays Redeemed (lost-200 recovery), and an expired claim is Denied + `/i/` NotFound.
//! - **Cross-species isolation:** a claim token POSTed to the invite-authorize door and an invite token
//!   POSTed to `/v1/admin-claim` both fail exactly like an unknown token (uniform), consuming nothing.

mod common;

use common::{Plane, SKILL, WS, expected, start_plane_mode};
use plane_store::{
    ApproveStandupOutcome, Authority, AuthorityError, CreateWorkspaceOutcome, DeploymentMode,
    MintClaimOutcome, Principal, RedeemOutcome, WorkspaceId,
};
use topos::test_support::{Follow, FollowHarness, PublishResult, PullHarness, Scope};
use topos_types::Generation;
use topos_types::results::{PublishPendingStatus, PullAction};

// ── shared constants ──────────────────────────────────────────────────────────────────────────────

/// The draft every chain publishes: a doc + an EXECUTABLE script (the exec bit must survive end to end).
const DRAFT: &[(&str, bool, &[u8])] = &[
    ("SKILL.md", false, b"# deploy\nship it\n"),
    ("run.sh", true, b"#!/bin/sh\necho ship\n"),
];
/// A follower's local placeholder (NOT the genesis, so the first pull genuinely fast-forwards).
const PLACEHOLDER: &[(&str, bool, &[u8])] = &[("SKILL.md", false, b"# local placeholder\n")];
const AT: &str = "2026-07-03T00:00:00Z";
const FOUNDER: &str = "founder@newco.test";
const OWNER_EMAIL: &str = "owner@newco.test";
const MEMBER_EMAIL: &str = "member@newco.test";

/// The REAL wall clock (epoch ms) — the HTTP routes stamp real time, so any authority op whose result the
/// wire later compares against (claim expiry, session liveness) must be minted on the same clock.
fn wall_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the wall clock is past the epoch")
            .as_millis(),
    )
    .expect("epoch millis fit i64")
}

// ── row-level witnesses (direct reads on the per-test database; never a write path) ────────────────

/// The `workspace` row's `(display_name, deployment_mode)`.
fn workspace_row(plane: &Plane, ws: &str) -> (String, String) {
    plane
        .rt
        .block_on(
            sqlx::query_as::<_, (String, String)>(
                "SELECT display_name, deployment_mode FROM workspace WHERE workspace_id = $1",
            )
            .bind(ws)
            .fetch_one(&plane.pool),
        )
        .expect("the workspace row exists")
}

/// The `workspace` row's ADDRESS `name` (the slug the standup/create minted).
fn workspace_name(plane: &Plane, ws: &str) -> String {
    plane
        .rt
        .block_on(
            sqlx::query_scalar::<_, String>("SELECT name FROM workspace WHERE workspace_id = $1")
                .bind(ws)
                .fetch_one(&plane.pool),
        )
        .expect("the workspace row carries an address name")
}

/// The `workspace_member` row's `(role, status)`.
fn member_row(plane: &Plane, ws: &str, principal: &str) -> (String, String) {
    plane
        .rt
        .block_on(
            sqlx::query_as::<_, (String, String)>(
                "SELECT role, status FROM workspace_member \
                 WHERE workspace_id = $1 AND principal = $2",
            )
            .bind(ws)
            .bind(principal)
            .fetch_one(&plane.pool),
        )
        .expect("the member row exists")
}

/// A bare COUNT(*) witness.
fn count(plane: &Plane, sql: &str) -> i64 {
    plane
        .rt
        .block_on(sqlx::query_scalar::<_, i64>(sql).fetch_one(&plane.pool))
        .expect("count")
}

/// An enrollment-configured loopback plane with NOTHING seeded — every workspace in these chains is born
/// through a standup door, never a fixture.
fn empty_plane(tag: &str, mode: DeploymentMode) -> Plane {
    start_plane_mode(
        "topos-standup",
        tag,
        true,
        mode,
        async |_authority: &Authority| common::Seeded::default(),
    )
}

// ── door 1: the un-enrolled first publish stands the workspace up (cloud) ──────────────────────────

#[test]
fn e2e_door1_the_first_publish_stands_the_workspace_up() {
    let plane = empty_plane("door1", DeploymentMode::Cloud);
    let client = FollowHarness::new("standup-door1");
    client.adopt(SKILL, DRAFT);
    let digest = client.draft_digest(SKILL);
    let approve = format!("{SKILL}@{digest}");

    // Call 1 — the un-enrolled direct publish does not fail: it goes PENDING the human sign-in.
    let call1 = client
        .publish(&plane.base_url, &approve)
        .expect("publish call 1");
    let PublishResult::Pending { data, resume_argv } = call1 else {
        panic!("an un-enrolled publish must go PENDING, got {call1:?}");
    };
    let pending = data.pending.expect("the pending sign-in block");
    assert_eq!(pending.status, PublishPendingStatus::SigninRequired);
    assert!(
        pending.user_code.len() >= 40
            && pending
                .user_code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "standup codes are high-entropy opaque URL-safe tokens, got {:?}",
        pending.user_code
    );
    assert_eq!(
        pending.verification_uri_complete,
        format!("{}/verify/{}", plane.base_url, pending.user_code),
        "the SERVER-built complete URI rides verbatim"
    );
    assert!(pending.expires_at.is_some(), "the expiry is disclosed");
    assert_eq!(
        resume_argv,
        vec![
            "topos".to_owned(),
            "publish".to_owned(),
            approve.clone(),
            "--json".to_owned(),
        ],
        "the ENROLL_RESUME argv IS this same publish command (the `<skill>@<digest>` positional)"
    );
    assert!(
        data.version_id.is_none() && data.current_generation.is_none(),
        "nothing shipped at pending"
    );
    assert_eq!(data.bundle_digest, digest, "the consent digest is bound");
    assert!(client.wal_exists(), "the standup WAL is written");

    // The web leg, headless: the authority's approve op with an ALREADY-verified email (the lib surface a
    // composing web page calls). `None` display name takes the server's localpart default.
    let approved = plane
        .rt
        .block_on(plane.authority.approve_standup(
            &pending.user_code,
            FOUNDER,
            None,
            None,
            DeploymentMode::Cloud,
            wall_ms(),
            AT,
        ))
        .expect("approve the standup session");
    let ApproveStandupOutcome::Approved {
        workspace_id,
        display_name,
    } = approved
    else {
        panic!("expected Approved, got {approved:?}");
    };
    assert_eq!(
        display_name, "founder's workspace",
        "a None name takes the email-localpart server default"
    );
    let ws = workspace_id.as_str();

    // Call 2 — the SAME command re-invoked: poll → redeem → promote → the publish lands in ONE invocation.
    let call2 = client
        .publish(&plane.base_url, &approve)
        .expect("publish call 2 (the resume)");
    let PublishResult::Published(done) = call2 else {
        panic!("the resumed publish must land, got {call2:?}");
    };
    assert_eq!(
        done.current_generation,
        Some(Generation { epoch: 1, seq: 1 }),
        "the genesis publish landed"
    );
    assert_eq!(done.bundle_digest, digest);
    let version_id = done.version_id.clone().expect("the landed version id");
    let receipt = done
        .standup
        .expect("the standup disclosure rides the receipt");
    assert_eq!(receipt.workspace_display_name, "founder's workspace");
    assert_eq!(
        receipt.owner_principal.as_deref(),
        Some(FOUNDER),
        "hijack visibility: the receipt names who owns the workspace"
    );
    // The genesis no longer mints an `/i/` link (invites are roster rows now) — it discloses the
    // workspace ADDRESS as the share line: the receipt's `address` and the published skill's `share_line`.
    let receipt_address = receipt
        .address
        .clone()
        .expect("the standup receipt discloses the workspace address");
    assert!(
        done.share_line.is_some(),
        "the genesis publish discloses the skill's paste-able share line"
    );
    assert!(!client.wal_exists(), "the WAL is consumed at promote");
    // A standup-born workspace has its ADDRESS name minted (a slug), and the receipt address roots on it.
    let minted_name = workspace_name(&plane, ws);
    assert!(!minted_name.is_empty(), "the standup minted an address name");
    assert!(
        receipt_address.ends_with(&format!("/{minted_name}")),
        "the receipt address roots on the minted name: {receipt_address} vs {minted_name}"
    );

    // The enrolled state the standup wrote: instance.json committed, seated principal, workspace.
    assert!(client.instance_written());
    assert_eq!(client.user_principal().as_deref(), Some(FOUNDER));
    assert_eq!(client.user_workspace().as_deref(), Some(ws));

    // The workspace was born in CLOUD mode with the default name; the owner member is confirmed.
    assert_eq!(
        workspace_row(&plane, ws),
        ("founder's workspace".to_owned(), "cloud".to_owned())
    );
    assert_eq!(
        member_row(&plane, ws, FOUNDER),
        ("owner".to_owned(), "confirmed".to_owned())
    );
    assert_eq!(count(&plane, "SELECT COUNT(*) FROM workspace"), 1);

    // ZERO operator ops: this chain never called mint_admin_claim / admin_claim (by construction — grep
    // this test), and the admin_claim table is EMPTY.
    assert_eq!(
        count(&plane, "SELECT COUNT(*) FROM admin_claim"),
        0,
        "the cloud self-serve chain minted no operator claim"
    );

    // The landed genesis object is byte-exact OVER THE WIRE: a follower pulls it with a workspace credential
    // seeded on a witness device bound to FOUNDER (a confirmed owner from the standup — a confirmed member
    // reads every skill; per-skill read tokens are gone). The credential is a read-side witness aid; the
    // chain above is untouched by it.
    plane
        .rt
        .block_on(plane.authority.seed_device(
            &WorkspaceId::parse(ws).expect("the server-minted ws id parses"),
            "dk_standup_witness",
            &[55u8; 32],
            &Principal::parse(FOUNDER).expect("principal"),
            false,
            "wc_standup_witness",
        ))
        .expect("seed the witness device credential");
    let mut follower = PullHarness::new("standup-door1-f");
    follower.adopt_followed(SKILL, ws, "wc_standup_witness", Follow::Auto, PLACEHOLDER);
    let pulled = follower.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(pulled.skills[0].action, PullAction::FastForwarded);
    assert_eq!(pulled.skills[0].applied, Generation { epoch: 1, seq: 1 });
    assert_eq!(
        follower.placement_files(SKILL),
        expected(DRAFT),
        "the genesis object lands byte-exact (incl. the exec bit)"
    );
    assert_eq!(
        follower.sync_state(SKILL).base_commit,
        version_id,
        "the follower's applied version IS the published one"
    );
}

// ── door 1, auto-add: the first publish of an UNTRACKED directory adopts it, then stands up ─────────

#[test]
fn e2e_door1_first_publish_of_an_untracked_dir_auto_adds_and_stands_up() {
    let plane = empty_plane("door1-autoadd", DeploymentMode::Cloud);
    let client = FollowHarness::new("standup-autoadd");
    // The publish target is a RAW directory the client has never adopted — the auto-add convenience.
    let dir = client.write_skill_dir("deploy", DRAFT);
    let dir_arg = dir.to_str().expect("a utf-8 work path");

    // Call 1 — auto-adopts the dir (offline, before any network) THEN goes PENDING the human sign-in.
    let call1 = client
        .publish(&plane.base_url, dir_arg)
        .expect("publish call 1");
    let PublishResult::Pending { data, resume_argv } = call1 else {
        panic!("an un-enrolled publish of an untracked dir must go PENDING, got {call1:?}");
    };
    // The folded-in add is disclosed on the pending receipt (a plain dir → no harness slug).
    let added = data.added.expect("the auto-add is disclosed at pending");
    assert_eq!(added.name, "deploy");
    assert_eq!(added.harness_slug, None);
    // The resume argv SELF-HEALS from the raw dir path to the adopted `<name>@<digest>` — so call 2
    // tracked-resolves fast and never re-adopts.
    let resume_target = resume_argv.get(2).cloned().expect("a resume target token");
    assert!(
        resume_target.starts_with("deploy@"),
        "the resume self-heals to the adopted name, got {resume_target}"
    );
    assert!(client.wal_exists(), "the standup WAL is written");
    let pending = data.pending.expect("the pending sign-in block");

    // The web approve leg — the owner signs in and approves the standup.
    let approved = plane
        .rt
        .block_on(plane.authority.approve_standup(
            &pending.user_code,
            FOUNDER,
            None,
            None,
            DeploymentMode::Cloud,
            wall_ms(),
            AT,
        ))
        .expect("approve the standup session");
    assert!(
        matches!(approved, ApproveStandupOutcome::Approved { .. }),
        "expected Approved, got {approved:?}"
    );

    // Call 2 — the resume target (already tracked from call 1): enroll + land the genesis in one
    // invocation, WITHOUT re-adopting.
    let call2 = client
        .publish(&plane.base_url, &resume_target)
        .expect("publish call 2 (the resume)");
    let PublishResult::Published(done) = call2 else {
        panic!("the resumed publish must land, got {call2:?}");
    };
    assert!(
        done.added.is_none(),
        "call 2 publishes the already-tracked skill — no second add"
    );
    assert_eq!(
        done.current_generation,
        Some(Generation { epoch: 1, seq: 1 }),
        "the auto-added skill's genesis landed over the wire"
    );
    assert!(
        done.standup.is_some(),
        "the standup disclosure rides the receipt"
    );
    assert!(!client.wal_exists(), "the WAL is consumed at promote");
    assert_eq!(client.user_principal().as_deref(), Some(FOUNDER));
}

// ── door 2: the web create → owner enrollment → distribute to an invited member (cloud) ────────────

#[test]
fn e2e_door2_web_create_enrolls_the_owner_and_distributes_to_an_invited_member() {
    let plane = empty_plane("door2", DeploymentMode::Cloud);

    // The web door: create_workspace for an already-verified email (the authority op the cloud page calls).
    // A `None` address name derives the slug from the display name; the outcome carries the ADDRESS (no link
    // — the roster is the lock).
    let created = plane
        .rt
        .block_on(plane.authority.create_workspace(
            "req-door2-1",
            Some("Newco"),
            None,
            OWNER_EMAIL,
            DeploymentMode::Cloud,
            AT,
        ))
        .expect("create workspace");
    let CreateWorkspaceOutcome::Created(c) = created else {
        panic!("expected Created, got {created:?}");
    };
    let ws = c.workspace_id.as_str().to_owned();
    assert_eq!(c.display_name, "Newco");
    assert_eq!(
        workspace_row(&plane, &ws),
        ("Newco".to_owned(), "cloud".to_owned())
    );
    assert_eq!(
        member_row(&plane, &ws, OWNER_EMAIL),
        ("owner".to_owned(), "confirmed".to_owned())
    );
    assert!(
        c.address.ends_with(&format!("/{}", c.name)),
        "the address roots on the minted name: {} vs {}",
        c.address,
        c.name
    );
    let owner_address = c.address.clone();

    // A web retry (the SAME request id + owner) replays ONE workspace + the identical ADDRESS.
    let replayed = plane
        .rt
        .block_on(plane.authority.create_workspace(
            "req-door2-1",
            Some("Newco"),
            None,
            OWNER_EMAIL,
            DeploymentMode::Cloud,
            AT,
        ))
        .expect("replay the same request");
    let CreateWorkspaceOutcome::Replayed(r) = replayed else {
        panic!("expected Replayed, got {replayed:?}");
    };
    assert_eq!(r.workspace_id.as_str(), ws);
    assert_eq!(r.address, c.address, "the address replays byte-identically");
    assert_eq!(
        count(&plane, "SELECT COUNT(*) FROM workspace"),
        1,
        "one request = one workspace"
    );

    // The owner's agent enrolls by ADDRESS: card → device flow → the web-approve leg → resume redeems
    // (the confirmed-owner roster row admits it). No self-invite, no link.
    let owner = FollowHarness::new("standup-door2-owner");
    common::begin_address_enroll(&plane, &owner, &owner_address, OWNER_EMAIL);
    let describe = owner.resume_describe().expect("owner resume enrolls");
    assert!(describe.enrolled_now, "the confirmed owner's redeem is admitted");
    assert_eq!(describe.workspace_id, ws);
    assert!(owner.instance_written());
    assert_eq!(owner.user_principal().as_deref(), Some(OWNER_EMAIL));

    // The owner adopts + genesis-publishes over the wire (into the structural `everyone`).
    owner.adopt(SKILL, DRAFT);
    let digest = owner.draft_digest(SKILL);
    let published = owner
        .publish(&plane.base_url, &format!("{SKILL}@{digest}"))
        .expect("the owner's genesis publish");
    let PublishResult::Published(genesis) = published else {
        panic!("expected a direct publish, got {published:?}");
    };
    assert_eq!(
        genesis.current_generation,
        Some(Generation { epoch: 1, seq: 1 })
    );
    assert!(
        genesis.standup.is_none(),
        "an address-rooted enrollment carries no standup receipt"
    );

    // The owner invites a member (the REAL invitation op — the owner's device credential authenticates it,
    // nothing signed). The invite seats the roster row and returns the workspace ADDRESS the member joins at.
    let member_address = owner.invite(MEMBER_EMAIL, &[SKILL]).expect("invite");
    assert_eq!(
        member_row(&plane, &ws, MEMBER_EMAIL),
        ("member".to_owned(), "invited".to_owned()),
        "the invite seats the roster row"
    );

    // The member joins by ADDRESS: pending → the web-approve leg (member) → redeem flips invited →
    // confirmed → the `--yes` apply lands the `everyone` genesis byte-exact (a batch-accepted first receive).
    let member = FollowHarness::new("standup-door2-member");
    common::begin_address_enroll(&plane, &member, &member_address, MEMBER_EMAIL);
    let applied = member.resume_apply().expect("member resume enrolls + applies");
    assert!(applied.enrolled_now);
    assert_eq!(
        member_row(&plane, &ws, MEMBER_EMAIL),
        ("member".to_owned(), "confirmed".to_owned()),
        "the redeem flips invited → confirmed"
    );
    assert_eq!(
        member.placement_files(SKILL),
        expected(DRAFT),
        "the bytes land exactly on the second client"
    );
}

// ── cloud adversarial witnesses ─────────────────────────────────────────────────────────────────────

#[test]
fn e2e_a_leaked_address_is_inert_off_roster_and_surfaces_request_access() {
    let plane = empty_plane("leak", DeploymentMode::Cloud);
    let created = plane
        .rt
        .block_on(plane.authority.create_workspace(
            "req-leak-1",
            Some("Newco"),
            None,
            OWNER_EMAIL,
            DeploymentMode::Cloud,
            AT,
        ))
        .expect("create workspace");
    let CreateWorkspaceOutcome::Created(c) = created else {
        panic!("expected Created, got {created:?}");
    };
    let ws = c.workspace_id.as_str().to_owned();
    let address = c.address.clone();

    // The leak: a stranger's agent starts fine (the workspace ADDRESS is a public enrollment START) and
    // signs in as an identity NOT on the roster (`begin_address_enroll` drives call 1 + the confirm).
    let stranger = FollowHarness::new("standup-leak");
    common::begin_address_enroll(&plane, &stranger, &address, "mallory@evil.test");

    // The redeem is DENIED — and the client surfaces the REQUEST_ACCESS ask-an-owner guidance, exactly as
    // the production error envelope carries it (the ONE uniform membership refusal — the address resolved,
    // the identity proved, but no seat).
    let denial = stranger.resume_expect_denied();
    assert_eq!(denial.code, "DENIED");
    assert_eq!(
        denial.next_action_codes,
        vec!["REQUEST_ACCESS".to_owned()],
        "the denied redeem carries the REQUEST_ACCESS next-action"
    );
    assert!(
        denial.message.contains("ask a workspace owner"),
        "the ask-an-owner guidance: {}",
        denial.message
    );
    assert!(!stranger.enrolled(), "no enrollment state lands");
    assert!(!stranger.instance_written(), "nothing was promoted");

    // The workspace is untouched: the owner seat stands, the stranger holds no membership.
    assert_eq!(
        member_row(&plane, &ws, OWNER_EMAIL),
        ("owner".to_owned(), "confirmed".to_owned())
    );
    let mallory: i64 = plane
        .rt
        .block_on(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM workspace_member WHERE principal = $1",
            )
            .bind("mallory@evil.test")
            .fetch_one(&plane.pool),
        )
        .expect("count");
    assert_eq!(mallory, 0, "a leaked address seats no one off-roster");
}

#[test]
fn e2e_approve_standup_misses_are_uniform_and_double_approve_is_idempotent() {
    let plane = empty_plane("approve", DeploymentMode::Cloud);
    let client = FollowHarness::new("standup-approve");
    client.adopt(SKILL, DRAFT);
    let approve = format!("{SKILL}@{}", client.draft_digest(SKILL));
    let call1 = client
        .publish(&plane.base_url, &approve)
        .expect("publish call 1");
    let PublishResult::Pending { data, .. } = call1 else {
        panic!("expected Pending, got {call1:?}");
    };
    let code = data.pending.expect("pending").user_code;

    // An unknown/guessed user code is the single indistinguishable NotFound (entropy is the anti-hijack
    // dial; a miss discloses nothing).
    let unknown = plane.rt.block_on(plane.authority.approve_standup(
        "XXXX-XXXX-XXXX-XXXX",
        FOUNDER,
        None,
        None,
        DeploymentMode::Cloud,
        wall_ms(),
        AT,
    ));
    assert!(
        matches!(unknown, Err(AuthorityError::NotFound)),
        "an unknown code is the uniform miss: {unknown:?}"
    );

    // The real approval…
    let approved = plane
        .rt
        .block_on(plane.authority.approve_standup(
            &code,
            FOUNDER,
            None,
            None,
            DeploymentMode::Cloud,
            wall_ms(),
            AT,
        ))
        .expect("approve");
    let ApproveStandupOutcome::Approved { workspace_id, .. } = approved else {
        panic!("expected Approved, got {approved:?}");
    };

    // …a same-email re-click is the idempotent AlreadyApproved (the SAME workspace, no second creation)…
    let again = plane
        .rt
        .block_on(plane.authority.approve_standup(
            &code,
            FOUNDER,
            None,
            None,
            DeploymentMode::Cloud,
            wall_ms(),
            AT,
        ))
        .expect("re-approve");
    let ApproveStandupOutcome::AlreadyApproved { workspace_id: w2 } = again else {
        panic!("expected AlreadyApproved, got {again:?}");
    };
    assert_eq!(w2, workspace_id);

    // …and a DIFFERENT email's re-approve is the uniform miss (first-writer-wins, never an overwrite).
    let hijack = plane.rt.block_on(plane.authority.approve_standup(
        &code,
        "other@evil.test",
        None,
        None,
        DeploymentMode::Cloud,
        wall_ms(),
        AT,
    ));
    assert!(
        matches!(hijack, Err(AuthorityError::NotFound)),
        "a different email's re-approve is the uniform miss: {hijack:?}"
    );

    assert_eq!(
        count(&plane, "SELECT COUNT(*) FROM workspace"),
        1,
        "exactly ONE workspace exists after the double-approve"
    );
}

#[test]
fn e2e_the_fourth_create_for_one_identity_is_the_typed_cap_denial() {
    let plane = empty_plane("cap", DeploymentMode::Cloud);
    for i in 1..=3 {
        let created = plane
            .rt
            .block_on(plane.authority.create_workspace(
                &format!("req-cap-{i}"),
                None,
                None,
                FOUNDER,
                DeploymentMode::Cloud,
                AT,
            ))
            .expect("create");
        assert!(
            matches!(created, CreateWorkspaceOutcome::Created(_)),
            "create #{i} is admitted: {created:?}"
        );
    }
    let fourth = plane
        .rt
        .block_on(plane.authority.create_workspace(
            "req-cap-4",
            None,
            None,
            FOUNDER,
            DeploymentMode::Cloud,
            AT,
        ))
        .expect("the op itself runs");
    let CreateWorkspaceOutcome::Denied(reason) = fourth else {
        panic!("the 4th create must be the typed cap denial, got {fourth:?}");
    };
    assert!(
        reason.contains("limit"),
        "the denial names the cap: {reason}"
    );
    assert_eq!(count(&plane, "SELECT COUNT(*) FROM workspace"), 3);
}

#[test]
fn e2e_create_workspace_refuses_a_reserved_address_name() {
    let plane = empty_plane("reserved", DeploymentMode::Cloud);
    // An explicit RESERVED slug is a typed refusal at the genesis door — nothing is created.
    let denied = plane
        .rt
        .block_on(plane.authority.create_workspace(
            "req-reserved-1",
            Some("Acme"),
            Some("api"),
            OWNER_EMAIL,
            DeploymentMode::Cloud,
            AT,
        ))
        .expect("the op runs");
    assert!(
        matches!(denied, CreateWorkspaceOutcome::Denied(_)),
        "a reserved address name is refused: {denied:?}"
    );
    assert_eq!(
        count(&plane, "SELECT COUNT(*) FROM workspace"),
        0,
        "a refused create makes no workspace"
    );
}

#[test]
fn e2e_a_standup_session_refuses_every_enroll_identity_leg_yet_stays_live() {
    let plane = empty_plane("intent", DeploymentMode::Cloud);
    let client = FollowHarness::new("standup-intent");
    client.adopt(SKILL, DRAFT);
    let approve = format!("{SKILL}@{}", client.draft_digest(SKILL));
    let call1 = client
        .publish(&plane.base_url, &approve)
        .expect("publish call 1");
    let PublishResult::Pending { data, .. } = call1 else {
        panic!("expected Pending, got {call1:?}");
    };
    let code = data.pending.expect("pending").user_code;

    // The intent guard: a standup session is only ever advanced by approve_standup — the passcode
    // start/complete and the external-identity confirm are all the uniform miss.
    let passcode = plane.rt.block_on(plane.authority.start_passcode(
        &code,
        "x@y.test",
        wall_ms(),
        AT,
    ));
    assert!(
        matches!(passcode, Err(AuthorityError::NotFound)),
        "start_passcode refuses a standup session: {passcode:?}"
    );
    let complete = plane.rt.block_on(plane.authority.complete_passcode(
        &code,
        "x@y.test",
        "000000",
        wall_ms(),
    ));
    assert!(
        matches!(complete, Err(AuthorityError::NotFound)),
        "complete_passcode refuses a standup session: {complete:?}"
    );
    let confirm = plane.rt.block_on(plane.authority.confirm_external_identity(
        &code,
        "x@y.test",
        wall_ms(),
    ));
    assert!(
        matches!(confirm, Err(AuthorityError::NotFound)),
        "confirm_external_identity refuses a standup session: {confirm:?}"
    );

    // The refused legs consumed NOTHING: the real approval still lands and the resume still publishes.
    let approved = plane
        .rt
        .block_on(plane.authority.approve_standup(
            &code,
            FOUNDER,
            None,
            None,
            DeploymentMode::Cloud,
            wall_ms(),
            AT,
        ))
        .expect("approve");
    assert!(matches!(approved, ApproveStandupOutcome::Approved { .. }));
    let call2 = client
        .publish(&plane.base_url, &approve)
        .expect("the resume still lands");
    assert!(
        matches!(call2, PublishResult::Published(_)),
        "the session stayed live through the refused legs: {call2:?}"
    );
}

// ── the self-host chain: the operator's one-time claim ─────────────────────────────────────────────

#[test]
fn e2e_selfhost_claim_chain_enrolls_publishes_and_distributes() {
    let plane = empty_plane("selfhost", DeploymentMode::SelfHost);

    // The operator mints ONE claim — the lib op the bin's `mint-claim` subcommand drives. No owner email:
    // on self-host the claiming device roots the owner.
    let minted = plane
        .rt
        .block_on(plane.authority.mint_admin_claim(
            &WorkspaceId::parse(WS).expect("ws id"),
            Some("Acme"),
            None,
            DeploymentMode::SelfHost,
            3_600_000,
            wall_ms(),
            AT,
        ))
        .expect("mint the claim");
    let MintClaimOutcome::Minted(claim) = minted else {
        panic!("expected Minted, got {minted:?}");
    };
    let claim_link = format!("{}/i/{}", plane.base_url, claim.token);

    // `follow <claim-link>` — ONE invocation, no web leg, no --resume.
    let owner = FollowHarness::new("standup-claim-owner");
    let done = owner
        .follow(&claim_link)
        .expect("the one-shot claim follow");
    assert!(done.enrolled, "enrolled in one invocation");
    assert_eq!(done.workspace_id, WS);
    assert!(!owner.wal_exists(), "the claim WAL is consumed at promote");
    assert!(owner.instance_written());
    let owner_principal = owner.user_principal().expect("the seated principal");
    assert!(
        owner_principal.starts_with("dev."),
        "no mint-time email ⇒ the owner is device-rooted: {owner_principal}"
    );
    assert_eq!(
        member_row(&plane, WS, &owner_principal),
        ("owner".to_owned(), "confirmed".to_owned())
    );
    assert_eq!(
        workspace_row(&plane, WS),
        ("Acme".to_owned(), "self_host".to_owned()),
        "the workspace is born at THE PLANE'S mode with the mint-time name"
    );

    // The enrolled owner genesis-publishes (the ordinary enrolled path — never the standup branch).
    owner.adopt(SKILL, DRAFT);
    let digest = owner.draft_digest(SKILL);
    let published = owner
        .publish(&plane.base_url, &format!("{SKILL}@{digest}"))
        .expect("the owner's genesis publish");
    let PublishResult::Published(genesis) = published else {
        panic!("expected a direct publish, got {published:?}");
    };
    assert_eq!(
        genesis.current_generation,
        Some(Generation { epoch: 1, seq: 1 })
    );

    // invite → the member joins by ADDRESS. Self-host now gates on the ROSTER exactly like cloud (the
    // born-confirmed device-rooted shortcut died with the invite bearer token), so there IS an identity
    // leg: the invited email proves itself, the redeem flips invited → confirmed, and the `--yes` apply
    // lands the `everyone` genesis byte-exact.
    let member_address = owner
        .invite("anyone@else.test", &[SKILL])
        .expect("the owner's invite");
    let member = FollowHarness::new("standup-claim-member");
    common::begin_address_enroll(&plane, &member, &member_address, "anyone@else.test");
    let applied = member.resume_apply().expect("member resume enrolls + applies");
    assert!(applied.enrolled_now, "the invited identity joins on self-host");
    assert_eq!(
        member_row(&plane, WS, "anyone@else.test"),
        ("member".to_owned(), "confirmed".to_owned()),
        "the redeem flips invited → confirmed on self-host too"
    );
    assert_eq!(
        member.placement_files(SKILL),
        expected(DRAFT),
        "the pull lands byte-exact on the second client"
    );
}

#[test]
fn e2e_claim_replay_expiry_and_refetch_witnesses() {
    let plane = empty_plane("claimwit", DeploymentMode::SelfHost);
    let minted = plane
        .rt
        .block_on(plane.authority.mint_admin_claim(
            &WorkspaceId::parse(WS).expect("ws id"),
            Some("Acme"),
            None,
            DeploymentMode::SelfHost,
            3_600_000,
            wall_ms(),
            AT,
        ))
        .expect("mint");
    let MintClaimOutcome::Minted(claim) = minted else {
        panic!("expected Minted, got {minted:?}");
    };
    let claim_link = format!("{}/i/{}", plane.base_url, claim.token);

    // The owner consumes the claim (the real one-shot follow).
    let owner = FollowHarness::new("standup-claimwit-owner");
    let done = owner.follow(&claim_link).expect("claim follow");
    assert!(done.enrolled);

    // A DIFFERENT device redeeming the consumed claim is Denied (a distinct 32-byte public key ⇒ a
    // distinct server-derived device key id).
    let other_device = [42u8; 32];
    let denied = plane
        .rt
        .block_on(
            plane
                .authority
                .admin_claim(&claim.token, other_device, wall_ms(), AT),
        )
        .expect("the op runs");
    assert!(
        matches!(denied, RedeemOutcome::Denied(_)),
        "a different device's redeem of a consumed claim is Denied: {denied:?}"
    );

    // The SAME device's replay deterministically re-answers Redeemed (lost-200 recovery).
    let replay = plane
        .rt
        .block_on(
            plane
                .authority
                .admin_claim(&claim.token, owner.device_pubkey(), wall_ms(), AT),
        )
        .expect("the op runs");
    let RedeemOutcome::Redeemed(r) = replay else {
        panic!("the same device replays Redeemed, got {replay:?}");
    };
    assert_eq!(r.workspace_id.as_str(), WS);

    // A consumed claim's /i/ refetch is the uniform NotFound (which is why the client's retry POSTs from
    // its WAL instead of refetching — proven in-crate; the wire-visible half is asserted here).
    let refetch = FollowHarness::new("standup-claimwit-refetch").follow(&claim_link);
    assert!(
        refetch
            .as_ref()
            .is_err_and(|e| e.contains("invalid or has expired")),
        "a consumed claim bootstraps the uniform NotFound: {refetch:?}"
    );

    // An EXPIRED claim (minted a minute in the past with ttl 0, for a different workspace): Denied at
    // redeem + /i/ NotFound. Expiry gates only the FIRST consumption.
    let expired = plane
        .rt
        .block_on(plane.authority.mint_admin_claim(
            &WorkspaceId::parse("w_expired").expect("ws id"),
            None,
            None,
            DeploymentMode::SelfHost,
            0,
            wall_ms() - 60_000,
            AT,
        ))
        .expect("mint the expired claim");
    let MintClaimOutcome::Minted(expired) = expired else {
        panic!("expected Minted, got {expired:?}");
    };
    let fresh_device = [43u8; 32];
    let dead = plane
        .rt
        .block_on(
            plane
                .authority
                .admin_claim(&expired.token, fresh_device, wall_ms(), AT),
        )
        .expect("the op runs");
    assert!(
        matches!(dead, RedeemOutcome::Denied(_)),
        "an expired claim's first consumption is Denied: {dead:?}"
    );
    let dead_link = format!("{}/i/{}", plane.base_url, expired.token);
    let dead_follow = FollowHarness::new("standup-claimwit-expired").follow(&dead_link);
    assert!(
        dead_follow
            .as_ref()
            .is_err_and(|e| e.contains("invalid or has expired")),
        "an expired claim bootstraps the uniform NotFound: {dead_follow:?}"
    );
}
