//! CATALOG e2e — `list --remote` end to end over loopback HTTP: the REAL client catalog transport
//! (device-signed) against the REAL composed plane route, on a real `127.0.0.1:0` socket.
//!
//! The one thing only a cross-crate loopback run can prove for the catalog read: that the client's
//! `UreqDeviceClient::fetch_catalog` — naming the install's genuine device credential (the `device_key_id`
//! its `DeviceSigner` derives) in the `Topos-Device-Key-Id` header, no signature — is accepted by the
//! plane's `GET /v1/workspaces/{ws}/skills` route, whose `list_skills_device` authority resolves the
//! non-revoked registered device, gates on confirmed workspace membership, and returns the `WireSkillIndex`.
//! Everything below drives the GENUINE transport (via the client's `test-fixtures` facade) against a
//! GENUINE `plane-store::Authority` seeded through its own `test-fixtures` shims.
//!
//! 1. **Happy path** — a confirmed-member device reads the catalog: both published skills come back with
//!    their exact `version_id`/`bundle_digest` (hex) and genesis generation, proving the whole
//!    presented-credential → `Topos-Device-Key-Id` header → registry lookup → confirmed-member → catalog
//!    round-trip.
//! 2. **Merge** — driving the real `topos::ops::list(.., Some(RemoteScope))` over the same live transport,
//!    a followed skill (local == catalog current, after a real pull) annotates `Following` and an
//!    unfollowed catalog skill annotates `Available`.
//! 3. **Negative** — a registered-but-non-member device AND a REVOKED device are gated: the plane's 404
//!    maps to an EMPTY index, while a confirmed member on the SAME plane still sees the full catalog (the
//!    gate actually gates; an unknown/revoked device 404s where a bad signature used to).
//! 4. **Self-host** — the device catalog lane consults NO deployment mode, so a member reads a self-host
//!    workspace's catalog exactly as on cloud.

mod common;

use common::{NOW, Seeded};
use plane_store::{
    Authority, DeploymentMode, FileMode, OpId, Principal, SkillId, UploadedFile, WorkspaceId,
};
use topos::test_support::ContributeHarness;
use topos_types::Generation;
use topos_types::TerminalOutcome;
use topos_types::results::RemoteFollowState;

// ── the one workspace + two published skills every scenario reads ────────────────────────────────────
const WS: &str = "w_acme";
const SA: &str = "s_alpha";
const SB: &str = "s_beacon";

/// The publisher — a distinct device, rostered on each skill so it can publish the genesis (its principal
/// need not be a workspace member; the READING device is the one that must be a confirmed member).
const PUBLISHER: &str = "p_author";
const PUB_DKID: &str = "dk_pub";
/// The publisher device's registered 32-byte public key (a fixed test value; nothing verifies against it).
const PUB_PUBKEY: [u8; 32] = [41u8; 32];

/// The reading principal (email-shaped — canonical-folded to lowercase), seated as a confirmed member.
const READER: &str = "reader@acme.test";
/// A registered device whose principal is NOT a workspace member (the negative-case witness).
const STRANGER: &str = "stranger@acme.test";

const GENESIS_OP_A: &str = "c0000000-0000-4000-8000-000000000001";
const GENESIS_OP_B: &str = "c0000000-0000-4000-8000-000000000002";
const AUTHOR: &str = "d_seed";
const MSG: &str = "topos publish";
const AT: &str = "2026-07-07T00:00:00Z";
/// The reader's per-skill read token for skill A (the merge case pulls A onto the plane's current).
const RT_A: &str = "rt_reader_alpha";

/// Skill A's genesis bundle — its own distinct bytes (so a cross-skill mix-up would be visible).
fn files_a() -> Vec<UploadedFile> {
    vec![
        UploadedFile {
            path: "SKILL.md".to_owned(),
            mode: FileMode::Regular,
            bytes: b"# alpha\nAcme deploy.\n".to_vec(),
        },
        UploadedFile {
            path: "run.sh".to_owned(),
            mode: FileMode::Executable,
            bytes: b"#!/bin/sh\necho alpha\n".to_vec(),
        },
    ]
}

/// Skill B's genesis bundle — DIFFERENT bytes from A's.
fn files_b() -> Vec<UploadedFile> {
    vec![UploadedFile {
        path: "SKILL.md".to_owned(),
        mode: FileMode::Regular,
        bytes: b"# beacon\nBeacon deploy.\n".to_vec(),
    }]
}

/// A published skill's server-trusted catalog facts (the exact hex the catalog must echo).
struct SkillFacts {
    version_id: String,
    bundle_digest: String,
}

/// The seed the loopback plane stands its scenario on — every catalog test seeds post-startup through
/// `plane.authority` (so the test holds the receipts), so the `start_plane` closure stands nothing up.
async fn empty_seed(_authority: &Authority) -> Seeded {
    Seeded::default()
}

// ── the shared seeding (run post-startup on `plane.rt` against the live authority) ───────────────────

/// Seed the workspace at `deployment_mode` (`"cloud"` / `"self_host"`), the publisher device rostered on
/// both skills, and a signed genesis for each at `(1,1)`. Returns the two skills' catalog facts.
async fn seed_two_published_skills(
    authority: &Authority,
    deployment_mode: &str,
) -> (SkillFacts, SkillFacts) {
    let ws = WorkspaceId::parse(WS).unwrap();
    let sa = SkillId::parse(SA).unwrap();
    let sb = SkillId::parse(SB).unwrap();
    let publisher = Principal::parse(PUBLISHER).unwrap();

    authority
        .seed_workspace(&ws, "Acme", "verified", deployment_mode)
        .await
        .expect("seed workspace");
    authority
        .seed_device(&ws, PUB_DKID, &PUB_PUBKEY, &publisher, false)
        .await
        .expect("seed publisher device");
    authority
        .seed_roster(&ws, &sa, &publisher)
        .await
        .expect("roster publisher on A");
    authority
        .seed_roster(&ws, &sb, &publisher)
        .await
        .expect("roster publisher on B");

    let a = publish_genesis(authority, &ws, &sa, GENESIS_OP_A, files_a()).await;
    let b = publish_genesis(authority, &ws, &sb, GENESIS_OP_B, files_b()).await;
    (a, b)
}

/// Drive one signed genesis publish and return its server-trusted hex facts.
async fn publish_genesis(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &SkillId,
    op_id: &str,
    files: Vec<UploadedFile>,
) -> SkillFacts {
    let receipt = authority
        .seed_published_genesis(
            ws,
            skill,
            PUB_DKID,
            &OpId::parse(op_id).unwrap(),
            files,
            AUTHOR,
            MSG,
            AT,
            NOW,
        )
        .await
        .expect("seed genesis");
    assert_eq!(receipt.outcome, TerminalOutcome::Ok);
    assert_eq!(receipt.current, Some(Generation { epoch: 1, seq: 1 }));
    SkillFacts {
        version_id: hex::encode(receipt.version_id.expect("genesis version id").0),
        bundle_digest: hex::encode(receipt.bundle_digest.expect("genesis bundle digest")),
    }
}

/// Register `device_key_id` (holding `public_key`, bound to `principal`) and seat that principal as a
/// CONFIRMED workspace member — the exact shape the catalog read authorizes.
async fn seat_member_device(
    authority: &Authority,
    device_key_id: &str,
    public_key: [u8; 32],
    principal: &str,
) {
    let ws = WorkspaceId::parse(WS).unwrap();
    let p = Principal::parse(principal).unwrap();
    authority
        .seed_device(&ws, device_key_id, &public_key, &p, false)
        .await
        .expect("seed member device");
    authority
        .seed_workspace_member(&ws, &p, "member", "confirmed")
        .await
        .expect("seat confirmed member");
}

/// Register a non-revoked device with a valid principal but NO workspace-member seat — a resolvable device
/// with a valid credential that must still be gated out of the catalog.
async fn register_non_member_device(
    authority: &Authority,
    device_key_id: &str,
    public_key: [u8; 32],
    principal: &str,
) {
    let ws = WorkspaceId::parse(WS).unwrap();
    let p = Principal::parse(principal).unwrap();
    authority
        .seed_device(&ws, device_key_id, &public_key, &p, false)
        .await
        .expect("seed non-member device");
}

// ── 1. the happy path: the REAL device-credential catalog round-trip over loopback HTTP ──────────────

#[test]
fn a_member_device_reads_the_workspace_catalog_over_loopback() {
    let plane = common::start_plane("topos-catalog-e2e", "member-ok", false, empty_seed);
    let rig = ContributeHarness::new("cat-member");
    // The plane must register THIS install's genuine device key + id (the signer the round-trip uses).
    let reader_pk = rig.device_pubkey();
    let reader_dkid = rig.device_key_id();

    let (fa, fb) = plane.rt.block_on(async {
        let authority: &Authority = &plane.authority;
        let facts = seed_two_published_skills(authority, "cloud").await;
        seat_member_device(authority, &reader_dkid, reader_pk, READER).await;
        facts
    });

    // presented credential → Topos-Device-Key-Id header → registry lookup → confirmed-member gate.
    let idx = rig
        .fetch_catalog(&plane.base_url, WS)
        .expect("the real catalog round-trip succeeds");

    let by_id = |id: &str| {
        idx.skills
            .iter()
            .find(|e| e.skill_id == id)
            .unwrap_or_else(|| panic!("{id} present in the catalog: {:?}", idx.skills))
    };
    assert_eq!(
        idx.skills.len(),
        2,
        "both published skills: {:?}",
        idx.skills
    );
    // The exact server-trusted ids survive the JSON round-trip (hex), for BOTH skills.
    assert_eq!(by_id(SA).version_id, fa.version_id, "s_alpha version id");
    assert_eq!(by_id(SA).bundle_digest, fa.bundle_digest, "s_alpha digest");
    assert_eq!(by_id(SB).version_id, fb.version_id, "s_beacon version id");
    assert_eq!(by_id(SB).bundle_digest, fb.bundle_digest, "s_beacon digest");
    // Each sits at its genesis generation, with no open proposals.
    assert_eq!(by_id(SA).generation, Generation { epoch: 1, seq: 1 });
    assert_eq!(by_id(SB).generation, Generation { epoch: 1, seq: 1 });
    assert_eq!(by_id(SA).open_proposals, 0);
    // The two skills are genuinely distinct on the wire.
    assert_ne!(by_id(SA).version_id, by_id(SB).version_id);
}

// ── 2. the merge, end to end: the real `list --remote` over the same live transport ──────────────────

#[test]
fn list_remote_merges_the_catalog_with_local_follow_state() {
    let plane = common::start_plane("topos-catalog-e2e", "merge", false, empty_seed);
    let mut rig = ContributeHarness::new("cat-merge");
    let reader_pk = rig.device_pubkey();
    let reader_dkid = rig.device_key_id();

    plane.rt.block_on(async {
        let authority: &Authority = &plane.authority;
        seed_two_published_skills(authority, "cloud").await;
        seat_member_device(authority, &reader_dkid, reader_pk, READER).await;
        // The reader will FOLLOW skill A: roster it + mint its read token so a real pull lands A's current.
        let ws = WorkspaceId::parse(WS).unwrap();
        let sa = SkillId::parse(SA).unwrap();
        let reader = Principal::parse(READER).unwrap();
        authority
            .seed_roster(&ws, &sa, &reader)
            .await
            .expect("roster reader on A");
        authority
            .mint_read_token(&ws, &sa, &reader, RT_A)
            .await
            .expect("mint reader read token for A");
    });

    // Enroll following ONLY skill A (a placeholder bundle), then pull so local A == the plane's current.
    rig.enroll(
        &plane.base_url,
        WS,
        SA,
        RT_A,
        false,
        &[("SKILL.md", false, b"placeholder\n")],
    );
    let pulled = rig.pull();
    assert_eq!(
        pulled.skills.len(),
        1,
        "the sweep pulled the one followed skill"
    );

    // The real merge: the device-signed catalog annotated with this install's on-disk follow-state.
    let (data, warnings) = rig.list_remote(
        &plane.base_url,
        vec![(WS.to_owned(), "Acme".to_owned())],
        None,
    );
    assert!(
        warnings.is_empty(),
        "no per-workspace catalog fault: {warnings:?}"
    );

    let entry = |id: &str| {
        data.remote_available
            .iter()
            .find(|e| e.skill_id == id)
            .unwrap_or_else(|| panic!("{id} in remote_available: {:?}", data.remote_available))
    };
    assert_eq!(
        data.remote_available.len(),
        2,
        "{:?}",
        data.remote_available
    );
    // A: followed AND the local applied version matches the catalog current → Following.
    assert_eq!(
        entry(SA).state,
        RemoteFollowState::Following,
        "A is followed"
    );
    assert_eq!(entry(SA).workspace_id, WS);
    // B: in the catalog but not followed by this install → Available (what to follow next).
    assert_eq!(
        entry(SB).state,
        RemoteFollowState::Available,
        "B is available"
    );
}

// ── 3. the negative: the confirmed-member gate actually gates ────────────────────────────────────────

#[test]
fn a_non_member_device_is_gated_to_an_empty_catalog() {
    let plane = common::start_plane("topos-catalog-e2e", "gate", false, empty_seed);
    let member = ContributeHarness::new("cat-gate-member");
    let stranger = ContributeHarness::new("cat-gate-stranger");
    let revoked = ContributeHarness::new("cat-gate-revoked");
    let member_pk = member.device_pubkey();
    let member_dkid = member.device_key_id();
    let stranger_pk = stranger.device_pubkey();
    let stranger_dkid = stranger.device_key_id();
    let revoked_pk = revoked.device_pubkey();
    let revoked_dkid = revoked.device_key_id();

    plane.rt.block_on(async {
        let authority: &Authority = &plane.authority;
        seed_two_published_skills(authority, "cloud").await;
        seat_member_device(authority, &member_dkid, member_pk, READER).await;
        // A resolvable, non-revoked device with a valid credential — but no confirmed workspace seat.
        register_non_member_device(authority, &stranger_dkid, stranger_pk, STRANGER).await;
        // A REVOKED device bound to the confirmed member READER — revocation short-circuits the resolve, so
        // even a member's revoked device reads nothing (the 404-shaped denial that replaced bad-signature).
        authority
            .seed_device(
                &WorkspaceId::parse(WS).unwrap(),
                &revoked_dkid,
                &revoked_pk,
                &Principal::parse(READER).unwrap(),
                true,
            )
            .await
            .expect("seed revoked member device");
    });

    // The confirmed member sees the full catalog...
    let member_idx = member
        .fetch_catalog(&plane.base_url, WS)
        .expect("member catalog round-trip");
    assert_eq!(member_idx.skills.len(), 2, "the member sees both skills");

    // ...while the registered-but-non-member device is gated: the plane's 404 → an EMPTY index (the
    // transport's degradation contract), NOT a leaked catalog and NOT a hard error.
    let stranger_idx = stranger
        .fetch_catalog(&plane.base_url, WS)
        .expect("the 404→empty mapping is not an error");
    assert!(
        stranger_idx.skills.is_empty(),
        "a non-member gets nothing: {:?}",
        stranger_idx.skills
    );

    // ...and a REVOKED device (even one bound to a confirmed member) is likewise gated to an empty catalog.
    let revoked_idx = revoked
        .fetch_catalog(&plane.base_url, WS)
        .expect("the 404→empty mapping is not an error");
    assert!(
        revoked_idx.skills.is_empty(),
        "a revoked device gets nothing: {:?}",
        revoked_idx.skills
    );
}

// ── 4. self-host: the device catalog lane consults no deployment mode ─────────────────────────────────

#[test]
fn the_catalog_serves_a_member_on_a_self_host_plane() {
    // The device lane authorizes catalog visibility by membership on BOTH cloud and self-host (device auth
    // IS the self-host membership story) — it never consults a deployment mode. The workspace is born
    // self-host (the honest signal the lane would read if it ever did), and the catalog still serves.
    let plane = common::start_plane_mode(
        "topos-catalog-e2e",
        "selfhost",
        false,
        DeploymentMode::SelfHost,
        empty_seed,
    );
    let rig = ContributeHarness::new("cat-selfhost");
    let reader_pk = rig.device_pubkey();
    let reader_dkid = rig.device_key_id();

    let (fa, fb) = plane.rt.block_on(async {
        let authority: &Authority = &plane.authority;
        let facts = seed_two_published_skills(authority, "self_host").await;
        seat_member_device(authority, &reader_dkid, reader_pk, READER).await;
        facts
    });

    let idx = rig
        .fetch_catalog(&plane.base_url, WS)
        .expect("the self-host catalog round-trip succeeds");
    let by_id = |id: &str| {
        idx.skills
            .iter()
            .find(|e| e.skill_id == id)
            .unwrap_or_else(|| panic!("{id} present in the catalog: {:?}", idx.skills))
    };
    assert_eq!(idx.skills.len(), 2, "self-host member sees both skills");
    assert_eq!(by_id(SA).version_id, fa.version_id);
    assert_eq!(by_id(SB).version_id, fb.version_id);
}
