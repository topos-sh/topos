//! HERO — the real client pull engine over loopback HTTP against the real plane.
//!
//! One real `plane-store` [`Authority`] (seeded through the feature-gated test-fixtures shims) is served by
//! the composed [`topos_plane::router`] on a real `127.0.0.1:0` socket on a background multi-threaded tokio
//! runtime. The client side is the GENUINE pull engine (`topos::ops::pull` via `topos::test_support`) over
//! the GENUINE [`topos::test_support`]-wrapped `ureq` transport — the SYNC client is driven from a plain
//! (non-async) thread, never inside the runtime.
//!
//! Three scenarios, each on its own freshly-seeded plane + client home:
//! 1. first pull fast-forwards onto the plane's genesis, byte-exact incl. the executable bit;
//! 2. an immediate second pull is a commit-sensitive **304 no-op** (nothing re-materialized);
//! 3. a v2 whose served signed record has a tampered signature is **refused** — the placement + floor retain
//!    last-known-good (the v1 genesis).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

mod common;
use std::sync::atomic::{AtomicU32, Ordering};

use ed25519_dalek::SigningKey;
use plane_store::{
    Authority, CommitId, FileMode, OpId, Principal, SkillId, UploadedFile, WorkspaceId,
};
use topos::test_support::{Follow, PullHarness, Scope};
use topos_plane::{PlaneState, router};
use topos_types::results::PullAction;
use topos_types::{Generation, TerminalOutcome};

// ── shared constants ──────────────────────────────────────────────────────────────────────────────
const WS: &str = "w_acme";
const SKILL: &str = "s_deploy";
const DKID: &str = "dk_a";
const PRINCIPAL: &str = "p_dev";
const READ_TOKEN: &str = "rt_hero_secret_value";
const AUTHOR: &str = "d_test";
const MESSAGE: &str = "topos publish";
const CREATED_AT: &str = "2026-06-29T00:00:00Z";
const NOW: i64 = 1_000_000;
/// The deterministic device signing seed; its public key is registered via `seed_device`.
const DEVICE_SEED: [u8; 32] = [7u8; 32];
const GENESIS_OP: &str = "a0000000-0000-4000-8000-000000000001";
const CHILD_OP: &str = "a0000000-0000-4000-8000-000000000002";

/// The plane's genesis bundle: a regular doc + an EXECUTABLE script (the exec bit must survive end to end).
fn genesis_files() -> Vec<UploadedFile> {
    vec![
        UploadedFile {
            path: "SKILL.md".to_owned(),
            mode: FileMode::Regular,
            bytes: b"# deploy\nDeploy the service.\n".to_vec(),
        },
        UploadedFile {
            path: "run.sh".to_owned(),
            mode: FileMode::Executable,
            bytes: b"#!/bin/sh\necho deploying\n".to_vec(),
        },
    ]
}

/// A v2 bundle (a forward child of genesis) — the move scenario 3 then tampers.
fn v2_files() -> Vec<UploadedFile> {
    vec![
        UploadedFile {
            path: "SKILL.md".to_owned(),
            mode: FileMode::Regular,
            bytes: b"# deploy v2\nDeploy the service, faster.\n".to_vec(),
        },
        UploadedFile {
            path: "run.sh".to_owned(),
            mode: FileMode::Executable,
            bytes: b"#!/bin/sh\necho deploying v2\n".to_vec(),
        },
    ]
}

/// The LOCAL placeholder a client adopts before any pull (intentionally NOT the plane's genesis, so the
/// first pull genuinely fast-forwards onto — and materializes — the plane's bytes).
const LOCAL_PLACEHOLDER: &[(&str, bool, &[u8])] = &[("SKILL.md", false, b"# local placeholder\n")];

// ── the loopback plane ──────────────────────────────────────────────────────────────────────────────

/// A self-cleaning temp dir (RAII).
struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("topos-hero-plane-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create plane scratch dir");
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A running loopback plane. Holds the runtime + authority handle alive for the test's duration; `_dir`
/// drops LAST so the served store outlives the runtime/authority.
struct Plane {
    rt: tokio::runtime::Runtime,
    authority: Arc<Authority>,
    base_url: String,
    plane_key: [u8; 32],
    /// The genesis version id the plane published at `(1,1)`.
    genesis: CommitId,
    _dir: Scratch,
}

impl Plane {
    fn ws(&self) -> WorkspaceId {
        WorkspaceId::parse(WS).unwrap()
    }
    fn skill(&self) -> SkillId {
        SkillId::parse(SKILL).unwrap()
    }
}

/// Seed a real authority (device → roster → signed genesis → read token), then serve `router(state)` on a
/// real loopback socket on a background runtime. Returns the live [`Plane`].
fn start_plane(tag: &str) -> Plane {
    let dir = Scratch::new(tag);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let (authority, genesis, plane_key) = rt.block_on(async {
        let authority = Authority::from_pool(
            common::provision_pg().await,
            &dir.0.join("git"),
            &dir.0.join("large"),
        )
        .expect("open authority")
        .with_plane_key(&dir.0.join("plane.key"))
        .expect("load plane key");

        let ws = WorkspaceId::parse(WS).unwrap();
        let skill = SkillId::parse(SKILL).unwrap();
        let principal = Principal::parse(PRINCIPAL).unwrap();
        let device_pubkey = SigningKey::from_bytes(&DEVICE_SEED)
            .verifying_key()
            .to_bytes();

        authority
            .seed_device(&ws, DKID, &device_pubkey, &principal, false)
            .await
            .expect("seed device");
        authority
            .seed_roster(&ws, &skill, &principal)
            .await
            .expect("seed roster");
        let receipt = authority
            .seed_published_genesis(
                &ws,
                &skill,
                DKID,
                &DEVICE_SEED,
                &OpId::parse(GENESIS_OP).unwrap(),
                genesis_files(),
                AUTHOR,
                MESSAGE,
                CREATED_AT,
                NOW,
            )
            .await
            .expect("seed genesis");
        assert_eq!(receipt.outcome, TerminalOutcome::Ok);
        assert_eq!(receipt.current, Some(Generation { epoch: 1, seq: 1 }));
        let genesis = receipt.version_id.expect("genesis version id");
        authority
            .mint_read_token(&ws, &skill, &principal, READ_TOKEN)
            .await
            .expect("mint read token");
        let plane_key = authority.plane_public_key().expect("plane public key");
        (authority, genesis, plane_key)
    });

    let authority = Arc::new(authority);
    let state = PlaneState::new(authority.clone());

    // Bind (and listen) BEFORE spawning serve, so a client connect queues in the backlog with no race.
    let listener = rt
        .block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await })
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("local addr");
    rt.spawn(async move {
        let _ = axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    Plane {
        rt,
        authority,
        base_url: format!("http://{addr}"),
        plane_key,
        genesis,
        _dir: dir,
    }
}

// ── scenario 1: first pull fast-forwards + byte-exact (incl. the executable bit) ──────────────────────

#[test]
fn first_pull_fast_forwards_byte_exact() {
    let plane = start_plane("ff");
    let mut client = PullHarness::new("ff");
    client.adopt_followed(SKILL, WS, READ_TOKEN, Follow::Auto, LOCAL_PLACEHOLDER);

    let data = client.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);

    // The engine fast-forwarded onto the plane's genesis at (1,1).
    assert_eq!(data.skills.len(), 1, "exactly one followed skill");
    let row = &data.skills[0];
    assert_eq!(row.action, PullAction::FastForwarded, "row: {row:?}");
    assert_eq!(row.applied, Generation { epoch: 1, seq: 1 });
    assert_eq!(row.observed, Generation { epoch: 1, seq: 1 });

    // The placement now holds the EXACT genesis bytes — path/mode/content byte-for-byte, incl. the exec bit.
    let got = client.placement_files(SKILL);
    let want = expected_placement(&genesis_files());
    assert_eq!(got, want, "placement must be the genesis bundle byte-exact");
    // Spell the executable-bit guarantee out explicitly.
    let run_sh = got
        .iter()
        .find(|(p, _, _)| p == "run.sh")
        .expect("run.sh present");
    assert_eq!(run_sh.1 & 0o111, 0o111, "run.sh keeps its executable bit");
    let skill_md = got
        .iter()
        .find(|(p, _, _)| p == "SKILL.md")
        .expect("SKILL.md present");
    assert_eq!(skill_md.1 & 0o111, 0, "SKILL.md is not executable");

    // sync.json advanced: applied == observed == (1,1), base == the genesis version id.
    let sync = client.sync_state(SKILL);
    assert_eq!(sync.applied, Generation { epoch: 1, seq: 1 });
    assert_eq!(sync.observed, Generation { epoch: 1, seq: 1 });
    assert_eq!(sync.base_commit, hex::encode(plane.genesis.0));
}

// ── scenario 2: an immediate second pull is a 304 no-op ───────────────────────────────────────────────

#[test]
fn second_pull_is_a_304_no_op() {
    let plane = start_plane("304");
    let mut client = PullHarness::new("304");
    client.adopt_followed(SKILL, WS, READ_TOKEN, Follow::Auto, LOCAL_PLACEHOLDER);

    // First pull fast-forwards to (1,1).
    let first = client.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(first.skills[0].action, PullAction::FastForwarded);
    let placement_after_ff = client.placement_files(SKILL);
    let sync_after_ff = client.sync_state(SKILL);

    // Second pull: the client sends If-None-Match: "1.1" + Topos-Known-Version-Id: <genesis>; the plane,
    // unchanged at (1,1)/<genesis>, answers 304. The engine maps that to UpToDate and re-materializes nothing.
    let second = client.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(
        second.skills[0].action,
        PullAction::UpToDate,
        "an unchanged pointer (304) is a no-op: {:?}",
        second.skills[0]
    );

    // Nothing re-materialized: the placement bytes/modes and sync floor are byte-identical to post-FF.
    assert_eq!(
        client.placement_files(SKILL),
        placement_after_ff,
        "a 304 must not rewrite the placement"
    );
    let sync_now = client.sync_state(SKILL);
    assert_eq!(sync_now.applied, sync_after_ff.applied);
    assert_eq!(sync_now.observed, sync_after_ff.observed);
    assert_eq!(sync_now.applied, Generation { epoch: 1, seq: 1 });
}

// ── scenario 3: a tampered v2 signature ⇒ refuse + retain last-known-good ──────────────────────────────

#[test]
fn tampered_signature_is_refused_and_retains_last_known_good() {
    let plane = start_plane("tamper");
    let mut client = PullHarness::new("tamper");
    client.adopt_followed(SKILL, WS, READ_TOKEN, Follow::Auto, LOCAL_PLACEHOLDER);

    // Reach a clean last-known-good state at the genesis (1,1).
    let ff = client.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(ff.skills[0].action, PullAction::FastForwarded);
    let good_placement = client.placement_files(SKILL);
    assert_eq!(good_placement, expected_placement(&genesis_files()));

    // Move `current` FORWARD to a v2 (a 1-parent publish on genesis), then corrupt its served signed record
    // so the signature no longer verifies — generation (1,2) + version_id stay intact.
    let authority = plane.authority.clone();
    let ws = plane.ws();
    let skill = plane.skill();
    let genesis = plane.genesis;
    plane.rt.block_on(async {
        let receipt = authority
            .seed_published_child(
                &ws,
                &skill,
                DKID,
                &DEVICE_SEED,
                &OpId::parse(CHILD_OP).unwrap(),
                genesis,
                v2_files(),
                AUTHOR,
                MESSAGE,
                CREATED_AT,
                NOW,
            )
            .await
            .expect("publish v2");
        assert_eq!(receipt.outcome, TerminalOutcome::Ok);
        assert_eq!(receipt.current, Some(Generation { epoch: 1, seq: 2 }));
        authority
            .tamper_current_signature(&ws, &skill)
            .await
            .expect("tamper current signature");
    });

    // The third pull fetches the advanced-but-forged record; the engine authenticates it against the pinned
    // plane key, the signature fails, and it REFUSES (an alarm) — applying nothing.
    let refused = client.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(
        refused.skills[0].action,
        PullAction::Alarm,
        "a tampered signature must refuse, not apply: {:?}",
        refused.skills[0]
    );

    // Last-known-good retained: the placement still holds the v1 genesis bytes, and the floor never advanced.
    assert_eq!(
        client.placement_files(SKILL),
        good_placement,
        "the refused v2 must never clobber the v1 placement"
    );
    let sync = client.sync_state(SKILL);
    assert_eq!(
        sync.applied,
        Generation { epoch: 1, seq: 1 },
        "applied stays at the last-known-good genesis"
    );
    assert_eq!(
        sync.observed,
        Generation { epoch: 1, seq: 1 },
        "the floor is never raised by an unverifiable record"
    );
    assert_eq!(sync.base_commit, hex::encode(genesis.0));
}

// ── helpers ───────────────────────────────────────────────────────────────────────────────────────

/// The placement-snapshot shape (`(path, mode & 0o777, bytes)`, sorted) the plane's bundle should
/// materialize to: regular files at 0o644, executable files at 0o755.
fn expected_placement(files: &[UploadedFile]) -> Vec<(String, u32, Vec<u8>)> {
    let mut out: Vec<(String, u32, Vec<u8>)> = files
        .iter()
        .map(|f| {
            let mode = match f.mode {
                FileMode::Executable => 0o755,
                FileMode::Regular => 0o644,
            };
            (f.path.clone(), mode, f.bytes.clone())
        })
        .collect();
    out.sort();
    out
}
