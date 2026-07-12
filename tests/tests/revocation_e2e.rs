//! REVOCATION e2e — a removed member fails closed, then re-enrollment recovers, over loopback HTTP.
//!
//! The credential-model revocation story end to end, on the GENUINE client (`FollowHarness` driving the
//! real `ureq` transports) against the GENUINE plane (`topos_plane::router` over a real `Authority`), via
//! the shared `common` harness. One workspace, one skill, an owner, and one invited follower.
//!
//! 1. The follower enrolls via the REAL two-call `follow` (device flow → confirm → redeem → the workspace
//!    credential lands in `credentials.json`), places the offered genesis (v1), and a bare sweep is
//!    up-to-date at v1.
//! 2. The owner REMOVES the member — `Authority::roster_remove` under the OWNER's Bearer credential (the
//!    device-lane governance op the `DELETE /v1/workspaces/{ws}/roster/{email}` route composes) — deleting
//!    the `workspace_member` row.
//! 3. The owner ships v2; the removed follower's next `pull` FAILS CLOSED. The plane answers the uniform
//!    404 for a non-member read; the pull engine maps that to a silent no-op and applies NOTHING — the
//!    removed member is frozen at v1 and never receives v2 (no bytes change).
//! 4. The owner RE-INVITES; the follower re-runs the REAL `follow` with the new `/i/` link. Its existing
//!    (non-revoked) device re-redeems, the server flips the invited seat back to CONFIRMED and ROTATES the
//!    credential — the registry `credential_sha256` changes, so the pre-rotation credential no longer
//!    resolves to any row (it can never authenticate again).
//! 5. The follower's next `pull` RECOVERS — it lands current (v2) byte-exact.
//!
//! CONVERSION-NOTE (the "old credential no longer authenticates" acceptance clause): the brief asks for
//! a raw HTTP current-read with the OLD plaintext credential -> 404. The real-`follow`-minted credential is a
//! server-side secret the `test-fixtures` facade deliberately never exposes to this crate (redacted from
//! `FollowHarness`; `credentials.json` has no accessor), and `test_support` is out of scope to change — so
//! the raw plaintext is unavailable. Rotation is instead witnessed at the database: the follower's
//! `device_registry.credential_sha256` is captured before removal and after re-enrollment and asserted to
//! have CHANGED (a fresh grant derived a fresh credential; the register upsert replaced the stored hash), so
//! the pre-rotation credential matches no row and cannot authenticate. Combined with step 3 (the credential
//! in-hand 404s the moment membership is revoked), this proves the full guarantee.

mod common;

use common::{NOW, Plane, SKILL, WS, expected_placement, genesis_files};
use plane_store::{
    Authority, CommitId, ConfirmOutcome, GovernanceOp, GovernanceOutcome, GovernanceRequest, OpId,
    Principal, SkillId, UploadedFile, WorkspaceId,
};
use topos::test_support::{FollowHarness, Scope};
use topos_types::results::PullAction;
use topos_types::{Generation, TerminalOutcome};

// ── shared constants ──────────────────────────────────────────────────────────────────────────────
const OWNER: &str = "p_owner";
const OWNER_DKID: &str = "dk_owner";
/// The owner device's registered 32-byte public key (a fixed test value; nothing verifies against it).
const OWNER_PUBKEY: [u8; 32] = [9u8; 32];
/// The owner's workspace Bearer credential — publishes v1/v2 and drives the removal + re-invite governance.
const OWNER_CRED: &str = "wc_owner_secret";
/// The follower is identified by an email — the cloud confirms it, and it becomes the seated principal.
const FOLLOWER: &str = "follower@newco.test";

const AUTHOR: &str = "d_test";
const MSG: &str = "topos publish";
const AT: &str = "2026-07-05T00:00:00Z";
const GENESIS_OP: &str = "a0000000-0000-4000-8000-000000000001";
const V2_OP: &str = "a0000000-0000-4000-8000-000000000002";
const INVITE_OP_1: &str = "b0000000-0000-4000-8000-000000000001";
const INVITE_OP_2: &str = "b0000000-0000-4000-8000-000000000002";
const REMOVE_OP: &str = "c0000000-0000-4000-8000-000000000001";

/// v2 — the owner's update the removed follower must NOT receive (then DOES, on recovery).
fn v2_files() -> Vec<UploadedFile> {
    use plane_store::FileMode;
    vec![
        UploadedFile {
            path: "SKILL.md".to_owned(),
            mode: FileMode::Regular,
            bytes: b"# deploy v2\nDeploy faster.\n".to_vec(),
        },
        UploadedFile {
            path: "run.sh".to_owned(),
            mode: FileMode::Executable,
            bytes: b"#!/bin/sh\necho deploying v2\n".to_vec(),
        },
    ]
}

/// Stand the plane up via the shared harness (bind-first, enrollment-configured): the workspace + owner (a
/// confirmed owner holding [`OWNER_CRED`]), the published genesis (v1), the invited follower, and the `/i/`
/// invite link.
fn start_plane(tag: &str) -> Plane {
    common::start_plane(
        "topos-revoke-e2e",
        tag,
        true,
        async |authority: &Authority| {
            let ws = WorkspaceId::parse(WS).unwrap();
            let skill = SkillId::parse(SKILL).unwrap();
            let follower = Principal::parse(FOLLOWER).unwrap();

            authority
                .seed_workspace(&ws, "Acme", "verified", "cloud")
                .await
                .expect("seed workspace");
            // The owner: a confirmed owner holding OWNER_CRED — it publishes v1/v2 and drives governance.
            common::seed_member(
                authority,
                &ws,
                OWNER_DKID,
                &OWNER_PUBKEY,
                OWNER,
                "owner",
                OWNER_CRED,
            )
            .await;
            let receipt = authority
                .seed_published_genesis(
                    &ws,
                    &skill,
                    OWNER_CRED,
                    &OpId::parse(GENESIS_OP).unwrap(),
                    genesis_files(),
                    AUTHOR,
                    MSG,
                    AT,
                    NOW,
                )
                .await
                .expect("seed genesis");
            assert_eq!(receipt.outcome, TerminalOutcome::Ok);
            assert_eq!(receipt.current, Some(Generation { epoch: 1, seq: 1 }));
            let genesis = receipt.version_id.expect("genesis version id");

            // Pre-seat the follower invited — the cloud redeem gate flips them invited → confirmed.
            authority
                .seed_workspace_member(&ws, &follower, "member", "invited")
                .await
                .expect("pre-roster the follower");
            let invite =
                common::mint_invite(authority, &ws, OWNER_CRED, INVITE_OP_1, FOLLOWER, SKILL, AT)
                    .await;
            common::Seeded {
                genesis: Some(genesis),
                invites: vec![invite],
            }
        },
    )
}

// ── row-level witnesses (direct reads on the per-test database; never a write path) ─────────────────

/// The follower's `device_registry.credential_sha256` (the rotation witness — one device, one row).
fn credential_hash(plane: &Plane, principal: &str) -> Vec<u8> {
    plane
        .rt
        .block_on(
            sqlx::query_scalar::<_, Option<Vec<u8>>>(
                "SELECT credential_sha256 FROM device_registry WHERE workspace_id = $1 AND principal = $2",
            )
            .bind(WS)
            .bind(principal)
            .fetch_one(&plane.pool),
        )
        .expect("the follower's device row exists after enrollment")
        .expect("the device carries a credential hash")
}

/// The `workspace_member` row's `(role, status)` for `principal`, or `None` when no row exists.
fn member_row(plane: &Plane, principal: &str) -> Option<(String, String)> {
    plane
        .rt
        .block_on(
            sqlx::query_as::<_, (String, String)>(
                "SELECT role, status FROM workspace_member WHERE workspace_id = $1 AND principal = $2",
            )
            .bind(WS)
            .bind(principal)
            .fetch_optional(&plane.pool),
        )
        .expect("query workspace_member")
}

/// The owner publishes a 1-parent child of `parent` through the real pointer-move (authenticated by
/// [`OWNER_CRED`]) — the author's next version. Returns its commit id.
fn publish_child(plane: &Plane, parent: CommitId, files: Vec<UploadedFile>, op: &str) -> CommitId {
    let (ws, skill) = (plane.ws(), plane.skill());
    plane.rt.block_on(async {
        let receipt = plane
            .authority
            .seed_published_child(
                &ws,
                &skill,
                OWNER_CRED,
                &OpId::parse(op).unwrap(),
                parent,
                files,
                AUTHOR,
                MSG,
                AT,
                NOW,
            )
            .await
            .expect("publish child");
        assert_eq!(receipt.outcome, TerminalOutcome::Ok);
        receipt.version_id.expect("child version id")
    })
}

/// The real two-call `follow`: begin (bootstrap → device-authorize), confirm the follower's identity
/// headless (the authority op a web verification page composes), resume (redeem + promote). Used for both
/// the first enrollment and the post-removal re-enrollment (the same device re-redeems).
fn follow_and_confirm(plane: &Plane, client: &FollowHarness, invite: &str) {
    let pending = client.follow(invite).expect("follow call 1");
    assert!(!pending.enrolled, "call 1 only begins enrollment");
    let user_code = pending
        .pending
        .as_ref()
        .expect("the pending arm carries the verification handle")
        .user_code
        .clone();
    let confirm = plane
        .rt
        .block_on(
            plane
                .authority
                .confirm_external_identity(&user_code, FOLLOWER, NOW),
        )
        .expect("confirm the session identity");
    assert!(matches!(confirm, ConfirmOutcome::Confirmed));
    let done = client.resume().expect("follow --resume");
    assert!(done.enrolled, "enrolled after the resume redeem");
}

// ── the keystone: fail-closed on removal, recover on re-enrollment ──────────────────────────────────

#[test]
fn e2e_removed_member_fails_closed_then_reenrollment_recovers() {
    let plane = start_plane("revoke");
    let client = FollowHarness::new("revoke");

    // ── 1 · The follower enrolls via the REAL two-call follow, places v1, and is up-to-date. ──
    follow_and_confirm(&plane, &client, plane.invite(0));
    client
        .approve(
            &plane.base_url,
            &[format!("{SKILL}@{}", hex::encode(plane.genesis().0))],
        )
        .expect("first-receive approve places the genesis (v1)");
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&genesis_files()),
        "v1 lands byte-exact"
    );
    let sweep = client.pull(Scope::AllFollowed);
    assert_eq!(sweep.skills[0].action, PullAction::UpToDate, "at v1");
    assert_eq!(
        client.sync_state(SKILL).applied,
        Generation { epoch: 1, seq: 1 }
    );

    // The pre-rotation credential hash (the rotation witness — captured while the follower is a member).
    let hash_before = credential_hash(&plane, FOLLOWER);
    assert_eq!(
        member_row(&plane, FOLLOWER),
        Some(("member".to_owned(), "confirmed".to_owned())),
        "the redeem confirmed the seat"
    );

    // ── 2 · The owner REMOVES the member (the device-lane governance op the DELETE route composes). ──
    let removed = plane
        .rt
        .block_on(plane.authority.roster_remove(
            &plane.ws(),
            REMOVE_OP,
            GovernanceRequest {
                credential: OWNER_CRED.to_owned(),
                op: GovernanceOp::RosterRemove {
                    target: Principal::parse(FOLLOWER).unwrap(),
                },
            },
            AT,
        ))
        .expect("roster_remove runs");
    assert!(
        matches!(removed, GovernanceOutcome::Ok),
        "the owner's removal is admitted: {removed:?}"
    );
    assert_eq!(
        member_row(&plane, FOLLOWER),
        None,
        "the member row is deleted — access dies with it"
    );

    // ── 3 · The owner ships v2; the REMOVED follower's next pull FAILS CLOSED — no bytes change. ──
    let v2 = publish_child(&plane, plane.genesis(), v2_files(), V2_OP);
    let sweep = client.pull(Scope::AllFollowed);
    // CONVERSION-NOTE: the brief expected the fail-closed read to "surface a per-skill error/warning". The
    // production pull engine instead maps a 404 on the `current` read (a removed member is a non-member, so
    // the plane returns the uniform not-found) to `PullAction::UpToDate` — a SILENT no-op, not a warning
    // (`ops::sync_engine`'s `Err(PlaneError::NotFound) => UpToDate` arm). That is still fail-closed: the
    // removed follower observes/fetches/applies NOTHING and is frozen at v1. This asserts the actual
    // fail-closed behavior (no-op + no bytes change), not a warning the engine does not emit.
    assert_eq!(
        sweep.skills[0].action,
        PullAction::UpToDate,
        "a removed member's read 404s -> a fail-closed no-op: {:?}",
        sweep.skills[0]
    );
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&genesis_files()),
        "no bytes change — still v1, NEVER v2"
    );
    assert_eq!(
        client.sync_state(SKILL).applied,
        Generation { epoch: 1, seq: 1 },
        "applied never advanced toward v2"
    );

    // ── 4 · The owner RE-INVITES; the follower re-runs the REAL follow (same device) and is re-admitted. ──
    let invite2 = plane.rt.block_on(common::mint_invite(
        &plane.authority,
        &plane.ws(),
        OWNER_CRED,
        INVITE_OP_2,
        FOLLOWER,
        SKILL,
        AT,
    ));
    follow_and_confirm(&plane, &client, &invite2);
    assert_eq!(
        member_row(&plane, FOLLOWER),
        Some(("member".to_owned(), "confirmed".to_owned())),
        "the existing device re-redeems; the seat flips invited → confirmed"
    );

    // The credential ROTATED: the stored hash changed, so the pre-rotation credential matches no registry
    // row and can never authenticate again (see the module CONVERSION-NOTE for why this is the witness).
    let hash_after = credential_hash(&plane, FOLLOWER);
    assert_ne!(
        hash_before, hash_after,
        "re-enrollment rotated the workspace credential (the old one is dead)"
    );

    // ── 5 · The follower's next pull RECOVERS — lands current (v2) byte-exact. ──
    let sweep = client.pull(Scope::AllFollowed);
    assert_eq!(
        sweep.skills[0].action,
        PullAction::FastForwarded,
        "the re-admitted member fast-forwards to v2: {:?}",
        sweep.skills[0]
    );
    assert_eq!(sweep.skills[0].applied, Generation { epoch: 1, seq: 2 });
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&v2_files()),
        "v2 lands byte-exact on recovery"
    );
    assert_eq!(
        client.sync_state(SKILL).base_commit,
        hex::encode(v2.as_bytes())
    );
}
