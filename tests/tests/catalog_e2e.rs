//! CATALOG e2e — `list --remote` end to end over loopback HTTP: the REAL client catalog transport
//! (workspace-credential Bearer) against the REAL composed plane route, on a real `127.0.0.1:0` socket.
//!
//! The one thing only a cross-crate loopback run can prove for the catalog read: that the client's
//! `UreqDeviceClient::fetch_catalog` — presenting the install's workspace **Bearer credential** (from
//! `credentials.json`), no signature — is accepted by the plane's `GET /v1/workspaces/{ws}/skills` route,
//! whose `list_skills_device` authority resolves the credential to its non-revoked registry row, gates on
//! confirmed workspace membership, and returns the `WireSkillIndex`. Everything below drives the GENUINE
//! transport (via the client's `test-fixtures` facade — each reading rig `enroll`s with its credential so
//! the Bearer lands in `credentials.json`) against a GENUINE `plane-store::Authority` seeded through its own
//! `test-fixtures` shims.
//!
//! 1. **Happy path** — a confirmed-member device reads the catalog: both published skills come back with
//!    their exact `version_id`/`bundle_digest` (hex) and genesis generation, proving the whole presented
//!    Bearer credential → registry lookup → confirmed-member → catalog round-trip.
//! 2. **Merge** — driving the real `topos::ops::list(.., Some(RemoteScope))` over the same live transport,
//!    a followed skill (local == catalog current, after a real pull) annotates `Following` and an
//!    unfollowed catalog skill annotates `Available`.
//! 3. **Negative** — a credential resolving to a non-member device AND a credential on a REVOKED device are
//!    gated: the plane's 404 maps to an EMPTY index, while a confirmed member on the SAME plane still sees
//!    the full catalog (the gate actually gates; an unknown/revoked credential 404s where a bad signature
//!    used to).
//! 4. **Self-host** — the device catalog lane consults NO deployment mode, so a member reads a self-host
//!    workspace's catalog exactly as on cloud.

mod common;

use common::{NOW, Seeded};
use plane_store::{
    Authority, BundleId, DeploymentMode, FileMode, OpId, Principal, UploadedFile, WorkspaceId,
};
use topos::test_support::ContributeHarness;
use topos_types::Generation;
use topos_types::TerminalOutcome;
use topos_types::results::RemoteFollowState;

// ── the one workspace + two published skills every scenario reads ────────────────────────────────────
const WS: &str = "w_acme";
const SA: &str = "s_alpha";
const SB: &str = "s_beacon";

/// The local placeholder each reading rig adopts when it enrolls (the catalog read is follow-independent;
/// enrollment is only how the workspace credential lands in `credentials.json`).
const PLACEHOLDER: &[(&str, bool, &[u8])] = &[("SKILL.md", false, b"# placeholder\n")];

/// The publisher — a confirmed member holding [`PUB_CRED`] (a write now needs confirmed membership; the
/// per-skill roster grants nothing). The READING principal below is a DISTINCT member.
const PUBLISHER: &str = "p_author";
const PUB_DKID: &str = "dk_pub";
/// The publisher device's registered 32-byte public key (a fixed test value; nothing verifies against it).
const PUB_PUBKEY: [u8; 32] = [41u8; 32];
/// The publisher's workspace Bearer credential (used only to seed the two genesis publishes).
const PUB_CRED: &str = "wc_pub_secret";

/// The reading principal (email-shaped — canonical-folded to lowercase), seated as a confirmed member.
const READER: &str = "reader@acme.test";
const MEMBER_DKID: &str = "dk_reader";
/// The reader device's registered 32-byte public key (a fixed test value; the credential authenticates).
const MEMBER_PUBKEY: [u8; 32] = [42u8; 32];
/// The reader's workspace Bearer credential — resolves to a confirmed member.
const MEMBER_CRED: &str = "wc_reader_secret";

/// A principal whose device is registered but is NOT a workspace member (the negative-case witness).
const STRANGER: &str = "stranger@acme.test";
const STRANGER_DKID: &str = "dk_stranger";
/// The stranger device's registered 32-byte public key (a fixed test value).
const STRANGER_PUBKEY: [u8; 32] = [43u8; 32];
/// The stranger's workspace Bearer credential — resolves to a real device with NO confirmed seat.
const STRANGER_CRED: &str = "wc_stranger_secret";

const REVOKED_DKID: &str = "dk_revoked";
/// The revoked device's registered 32-byte public key (a fixed test value).
const REVOKED_PUBKEY: [u8; 32] = [44u8; 32];
/// A workspace Bearer credential on a REVOKED device (bound to the confirmed member READER) — the resolve
/// short-circuits on the revoked row, so even a member's revoked device reads nothing.
const REVOKED_CRED: &str = "wc_revoked_secret";

const GENESIS_OP_A: &str = "c0000000-0000-4000-8000-000000000001";
const GENESIS_OP_B: &str = "c0000000-0000-4000-8000-000000000002";
const AUTHOR: &str = "d_seed";
const MSG: &str = "topos publish";
const AT: &str = "2026-07-07T00:00:00Z";

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

/// Seed the workspace at `deployment_mode` (`"cloud"` / `"self_host"`), the publisher as a confirmed member
/// holding [`PUB_CRED`], and a genesis for each skill at `(1,1)`. Returns the two skills' catalog facts.
async fn seed_two_published_skills(
    authority: &Authority,
    deployment_mode: &str,
) -> (SkillFacts, SkillFacts) {
    let ws = WorkspaceId::parse(WS).unwrap();
    let sa = BundleId::parse(SA).unwrap();
    let sb = BundleId::parse(SB).unwrap();

    authority
        .seed_workspace(&ws, "Acme", "verified", deployment_mode)
        .await
        .expect("seed workspace");
    // The publisher must be a confirmed member to publish (per-skill roster grants nothing now); its
    // credential authenticates the two genesis writes.
    common::seed_member(
        authority,
        &ws,
        PUB_DKID,
        &PUB_PUBKEY,
        PUBLISHER,
        "member",
        PUB_CRED,
    )
    .await;

    let a = publish_genesis(authority, &ws, &sa, GENESIS_OP_A, files_a()).await;
    let b = publish_genesis(authority, &ws, &sb, GENESIS_OP_B, files_b()).await;
    (a, b)
}

/// Drive one genesis publish (authenticated by [`PUB_CRED`]) and return its server-trusted hex facts.
async fn publish_genesis(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &BundleId,
    op_id: &str,
    files: Vec<UploadedFile>,
) -> SkillFacts {
    let receipt = authority
        .seed_published_genesis(
            ws,
            skill,
            PUB_CRED,
            &OpId::parse(op_id).unwrap(),
            files,
            AUTHOR,
            MSG,
            None,
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

/// Seat the READER as a CONFIRMED member holding [`MEMBER_CRED`] on the [`MEMBER_DKID`] device — the exact
/// shape the catalog read authorizes (a confirmed member's non-revoked credential).
async fn seat_reader(authority: &Authority) {
    common::seed_member(
        authority,
        &WorkspaceId::parse(WS).unwrap(),
        MEMBER_DKID,
        &MEMBER_PUBKEY,
        READER,
        "member",
        MEMBER_CRED,
    )
    .await;
}

// ── 1. the happy path: the REAL device-credential catalog round-trip over loopback HTTP ──────────────

#[test]
fn a_member_device_reads_the_workspace_catalog_over_loopback() {
    let plane = common::start_stack("topos-catalog-e2e", "member-ok", false, empty_seed);
    let mut rig = ContributeHarness::new("cat-member");

    let (fa, fb) = plane.rt.block_on(async {
        let authority: &Authority = &plane.authority;
        let facts = seed_two_published_skills(authority, "cloud").await;
        seat_reader(authority).await;
        facts
    });

    // Enroll so the member's workspace credential lands in credentials.json (the Bearer the catalog presents).
    rig.enroll(&plane.base_url, WS, SA, MEMBER_CRED, false, PLACEHOLDER);

    // presented Bearer credential → registry lookup → confirmed-member gate → the catalog.
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
    let plane = common::start_stack("topos-catalog-e2e", "merge", false, empty_seed);
    let mut rig = ContributeHarness::new("cat-merge");

    plane.rt.block_on(async {
        let authority: &Authority = &plane.authority;
        seed_two_published_skills(authority, "cloud").await;
        // The reader is a confirmed member holding MEMBER_CRED — that alone lets it read A on the real pull
        // (a confirmed member reads every skill; no per-skill roster / read token exists anymore).
        seat_reader(authority).await;
    });

    // Enroll following ONLY skill A (a placeholder bundle), then pull so local A == the plane's current.
    rig.enroll(&plane.base_url, WS, SA, MEMBER_CRED, false, PLACEHOLDER);
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
    let plane = common::start_stack("topos-catalog-e2e", "gate", false, empty_seed);
    let mut member = ContributeHarness::new("cat-gate-member");
    let mut stranger = ContributeHarness::new("cat-gate-stranger");
    let mut revoked = ContributeHarness::new("cat-gate-revoked");

    plane.rt.block_on(async {
        let authority: &Authority = &plane.authority;
        seed_two_published_skills(authority, "cloud").await;
        seat_reader(authority).await;
        let ws = WorkspaceId::parse(WS).unwrap();
        // A resolvable, non-revoked device with a valid credential — but NO confirmed workspace seat.
        authority
            .seed_device(
                &ws,
                STRANGER_DKID,
                &STRANGER_PUBKEY,
                &Principal::parse(STRANGER).unwrap(),
                false,
                STRANGER_CRED,
            )
            .await
            .expect("seed non-member device");
        // A REVOKED device bound to the confirmed member READER — revocation short-circuits the resolve, so
        // even a member's revoked credential reads nothing (the 404-shaped denial that replaced bad-signature).
        authority
            .seed_device(
                &ws,
                REVOKED_DKID,
                &REVOKED_PUBKEY,
                &Principal::parse(READER).unwrap(),
                true,
                REVOKED_CRED,
            )
            .await
            .expect("seed revoked member device");
    });

    // Each rig enrolls with its own credential so the presented Bearer is exactly the one under test.
    member.enroll(&plane.base_url, WS, SA, MEMBER_CRED, false, PLACEHOLDER);
    stranger.enroll(&plane.base_url, WS, SA, STRANGER_CRED, false, PLACEHOLDER);
    revoked.enroll(&plane.base_url, WS, SA, REVOKED_CRED, false, PLACEHOLDER);

    // The confirmed member sees the full catalog...
    let member_idx = member
        .fetch_catalog(&plane.base_url, WS)
        .expect("member catalog round-trip");
    assert_eq!(member_idx.skills.len(), 2, "the member sees both skills");

    // ...while the credential resolving to a non-member device is gated: the plane's 404 → an EMPTY index
    // (the transport's degradation contract), NOT a leaked catalog and NOT a hard error.
    let stranger_idx = stranger
        .fetch_catalog(&plane.base_url, WS)
        .expect("the 404→empty mapping is not an error");
    assert!(
        stranger_idx.skills.is_empty(),
        "a non-member gets nothing: {:?}",
        stranger_idx.skills
    );

    // ...and a REVOKED device's credential (even one bound to a confirmed member) is likewise gated to empty.
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
    let plane = common::start_stack_mode(
        "topos-catalog-e2e",
        "selfhost",
        false,
        DeploymentMode::SelfHost,
        empty_seed,
    );
    let mut rig = ContributeHarness::new("cat-selfhost");

    let (fa, fb) = plane.rt.block_on(async {
        let authority: &Authority = &plane.authority;
        let facts = seed_two_published_skills(authority, "self_host").await;
        seat_reader(authority).await;
        facts
    });

    rig.enroll(&plane.base_url, WS, SA, MEMBER_CRED, false, PLACEHOLDER);

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
