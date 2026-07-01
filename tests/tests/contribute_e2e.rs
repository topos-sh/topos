//! CONTRIBUTE e2e — the client device-signed write verbs over loopback HTTP against the real plane.
//!
//! One real `plane-store` [`Authority`] (seeded through the feature-gated fixtures) served by the composed
//! [`topos_plane::router`] on a real loopback socket. A PUBLISHER drives the GENUINE write verbs
//! (`publish`/`review`/`revert`/`diff` via `topos::test_support::ContributeHarness`) over the GENUINE `ureq`
//! transport; a separate FOLLOWER drives the GENUINE pull engine ([`topos::test_support::PullHarness`]) and
//! must receive the shipped bytes byte-exact. The publisher's device key is minted by the harness and
//! registered on the plane (the realistic flow), so the plane verifies its device-op signatures against the
//! key it enrolled. These cover the review_required-OFF loop; the review_required gate + the proposals-list
//! route are exercised in their own tests.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

mod common;
use std::sync::atomic::{AtomicU32, Ordering};

use ed25519_dalek::SigningKey;
use plane_store::{
    Authority, CommitId, FileMode, OpId, Principal, SkillId, UploadedFile, WorkspaceId,
};
use topos::test_support::{ContributeHarness, Follow, PublishResult, PullHarness, Scope};
use topos_types::results::{DiffSource, PullAction};
use topos_types::{Generation, TerminalOutcome};

const WS: &str = "w_acme";
const SKILL: &str = "s_deploy";
const GENESIS_DKID: &str = "dk_genesis";
const PRINCIPAL: &str = "p_dev";
const READ_TOKEN: &str = "rt_contribute_secret";
const AUTHOR: &str = "d_genesis";
const MESSAGE: &str = "topos: add";
const CREATED_AT: &str = "2026-06-30T00:00:00Z";
const NOW: i64 = 1_000_000;
const GENESIS_SEED: [u8; 32] = [9u8; 32];
const GENESIS_OP: &str = "b0000000-0000-4000-8000-000000000001";

/// The placeholder a client adopts before its first pull (NOT the plane's genesis, so the first pull
/// genuinely fast-forwards onto the plane's bytes).
const PLACEHOLDER: &[(&str, bool, &[u8])] = &[("SKILL.md", false, b"# local placeholder\n")];

fn genesis_files() -> Vec<UploadedFile> {
    vec![
        UploadedFile {
            path: "SKILL.md".to_owned(),
            mode: FileMode::Regular,
            bytes: b"# deploy\nv1\n".to_vec(),
        },
        UploadedFile {
            path: "run.sh".to_owned(),
            mode: FileMode::Executable,
            bytes: b"#!/bin/sh\necho v1\n".to_vec(),
        },
    ]
}

/// The publisher's new draft (a forward child of genesis) — shipped via `publish` / `--propose`.
const DRAFT: &[(&str, bool, &[u8])] = &[
    ("SKILL.md", false, b"# deploy\nv2 faster\n"),
    ("run.sh", true, b"#!/bin/sh\necho v2\n"),
];

// ── the loopback plane ──────────────────────────────────────────────────────────────────────────────

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("topos-contrib-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create plane scratch");
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Plane {
    rt: tokio::runtime::Runtime,
    authority: Arc<Authority>,
    base_url: String,
    plane_key: [u8; 32],
    genesis: CommitId,
    _dir: Scratch,
}

impl Plane {
    fn ws(&self) -> WorkspaceId {
        WorkspaceId::parse(WS).unwrap()
    }
    /// Register a contribute client's minted device key under `principal` (rostered), so the plane verifies
    /// its device-op signatures (the realistic enrollment outcome).
    fn register_device(&self, device_key_id: &str, device_pubkey: &[u8; 32], principal: &str) {
        let ws = self.ws();
        let principal = Principal::parse(principal).unwrap();
        self.rt.block_on(async {
            self.authority
                .seed_device(&ws, device_key_id, device_pubkey, &principal, false)
                .await
                .expect("seed device");
        });
    }

    /// Turn the workspace `review_required` gate on/off (the anti-poisoning policy).
    fn set_review_required(&self, on: bool) {
        let ws = self.ws();
        self.rt.block_on(async {
            self.authority
                .seed_review_required(&ws, on)
                .await
                .expect("set review_required");
        });
    }

    /// Roster a second principal + mint its read token (a distinct reviewer for four-eyes).
    fn seed_reviewer_principal(&self) {
        let ws = self.ws();
        let skill = SkillId::parse(SKILL).unwrap();
        let principal = Principal::parse(P_REVIEWER).unwrap();
        self.rt.block_on(async {
            self.authority
                .seed_roster(&ws, &skill, &principal)
                .await
                .unwrap();
            self.authority
                .mint_read_token(&ws, &skill, &principal, RT_REVIEWER)
                .await
                .unwrap();
        });
    }
}

const P_REVIEWER: &str = "p_reviewer";
const RT_REVIEWER: &str = "rt_reviewer_secret";

/// An enrolled reviewer under a DISTINCT principal (four-eyes), with the skill adopted + its device
/// registered + a read token — able to `review` a proposal.
fn enrolled_reviewer(plane: &Plane, tag: &str) -> ContributeHarness {
    let mut h = ContributeHarness::new(tag);
    plane.seed_reviewer_principal();
    plane.register_device(&h.device_key_id(), &h.device_pubkey(), P_REVIEWER);
    h.enroll(
        &plane.base_url,
        plane.plane_key,
        WS,
        SKILL,
        RT_REVIEWER,
        true,
        PLACEHOLDER,
    );
    h
}

/// Seed a real authority (genesis device → roster → signed genesis → read token) + serve `router(state)` on
/// a loopback socket. The publisher registers its OWN device key afterward via [`Plane::register_device`].
fn start_plane(tag: &str) -> Plane {
    let dir = Scratch::new(tag);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let (authority, genesis, plane_key) = rt.block_on(async {
        let authority = Authority::from_pool(
            common::provision_pg().await,
            &dir.0.join("git"),
            &dir.0.join("large"),
        )
        .expect("open authority")
        .with_plane_key(&dir.0.join("plane.key"))
        .expect("plane key");
        let ws = WorkspaceId::parse(WS).unwrap();
        let skill = SkillId::parse(SKILL).unwrap();
        let principal = Principal::parse(PRINCIPAL).unwrap();
        let genesis_pubkey = SigningKey::from_bytes(&GENESIS_SEED)
            .verifying_key()
            .to_bytes();
        authority
            .seed_device(&ws, GENESIS_DKID, &genesis_pubkey, &principal, false)
            .await
            .expect("seed genesis device");
        authority
            .seed_roster(&ws, &skill, &principal)
            .await
            .expect("seed roster");
        let receipt = authority
            .seed_published_genesis(
                &ws,
                &skill,
                GENESIS_DKID,
                &GENESIS_SEED,
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
        let genesis = receipt.version_id.expect("genesis id");
        authority
            .mint_read_token(&ws, &skill, &principal, READ_TOKEN)
            .await
            .expect("mint read token");
        let plane_key = authority.plane_public_key().expect("plane key");
        (authority, genesis, plane_key)
    });

    let authority = Arc::new(authority);
    let state = topos_plane::PlaneState::new(authority.clone());
    let listener = rt
        .block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await })
        .expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    rt.spawn(async move {
        let _ = axum::serve(
            listener,
            topos_plane::router(state).into_make_service_with_connect_info::<SocketAddr>(),
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

/// An enrolled publisher with its device key registered on `plane`, sitting at `current` (pulled), with
/// `DRAFT` staged as a local edit.
fn drafting_publisher(plane: &Plane, tag: &str) -> ContributeHarness {
    let mut pub_h = ContributeHarness::new(tag);
    plane.register_device(&pub_h.device_key_id(), &pub_h.device_pubkey(), PRINCIPAL);
    pub_h.enroll(
        &plane.base_url,
        plane.plane_key,
        WS,
        SKILL,
        READ_TOKEN,
        false,
        PLACEHOLDER,
    );
    // Reach the plane's current (1,1), then stage the draft.
    let pulled = pub_h.pull(plane.plane_key);
    assert_eq!(
        pulled.skills[0].action,
        PullAction::FastForwarded,
        "publisher reaches current"
    );
    pub_h.edit_placement(DRAFT);
    pub_h
}

fn approve_token(skill: &str, digest: &str) -> String {
    format!("{skill}@{digest}")
}

/// A fresh follower that has adopted + follows the skill (a placeholder); a pull lands `current`.
fn follower(tag: &str) -> PullHarness {
    let mut f = PullHarness::new(tag);
    f.adopt_followed(SKILL, WS, READ_TOKEN, Follow::Auto, PLACEHOLDER);
    f
}

/// The expected placement snapshot for a `(path, exec, bytes)` set: regular 0o644 / executable 0o755.
fn expected(files: &[(&str, bool, &[u8])]) -> Vec<(String, u32, Vec<u8>)> {
    let mut out: Vec<(String, u32, Vec<u8>)> = files
        .iter()
        .map(|(p, exec, b)| {
            (
                (*p).to_owned(),
                if *exec { 0o755 } else { 0o644 },
                b.to_vec(),
            )
        })
        .collect();
    out.sort();
    out
}

// ── scenario 1: publish-direct → the follower auto-applies byte-exact ──────────────────────────────────

#[test]
fn publish_direct_lands_on_a_follower_byte_exact() {
    let plane = start_plane("pubdirect");
    let pub_h = drafting_publisher(&plane, "pubdirect");

    let digest = pub_h.draft_digest();
    let outcome = pub_h
        .publish(plane.plane_key, false, &approve_token(SKILL, &digest))
        .expect("publish succeeds");
    let data = match outcome {
        PublishResult::Published(d) => d,
        other => panic!("expected a direct publish, got {other:?}"),
    };
    assert_eq!(
        data.current_generation,
        Generation { epoch: 1, seq: 2 },
        "current moved +1"
    );
    assert_eq!(
        data.bundle_digest, digest,
        "the published digest is the disclosed one"
    );

    // A separate follower pulls and auto-applies the EXACT shipped bytes (incl. the exec bit).
    let follower = follower("pubdirect-f");
    let pulled = follower.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(pulled.skills[0].action, PullAction::FastForwarded);
    assert_eq!(pulled.skills[0].applied, Generation { epoch: 1, seq: 2 });
    assert_eq!(
        follower.placement_files(SKILL),
        expected(DRAFT),
        "the follower holds the published draft byte-exact"
    );
}

// ── scenario 2: publish --propose → self-approve (review_required OFF) → follower applies ───────────────

#[test]
fn propose_then_approve_lands_on_a_follower() {
    let plane = start_plane("propose");
    let pub_h = drafting_publisher(&plane, "propose");

    let digest = pub_h.draft_digest();
    let proposed = pub_h
        .publish(plane.plane_key, true, &approve_token(SKILL, &digest))
        .expect("propose succeeds");
    let proposal = match proposed {
        PublishResult::Proposed(d) => d.proposal,
        other => panic!("expected a proposal, got {other:?}"),
    };

    // The proposer self-approves (allowed with review_required OFF) — current moves to the candidate.
    let review = pub_h
        .review(plane.plane_key, &proposal, true)
        .expect("approve succeeds");
    assert_eq!(
        review.current_generation,
        Some(Generation { epoch: 1, seq: 2 }),
        "approving the proposal moved current"
    );

    let follower = follower("propose-f");
    let pulled = follower.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(pulled.skills[0].applied, Generation { epoch: 1, seq: 2 });
    assert_eq!(
        follower.placement_files(SKILL),
        expected(DRAFT),
        "the follower applies the approved candidate (delegated consent, no prompt)"
    );
}

// ── scenario 3: revert → the follower rolls FORWARD to the good (genesis) bytes ─────────────────────────

#[test]
fn revert_rolls_a_follower_forward_to_the_good_bytes() {
    let plane = start_plane("revert");
    let pub_h = drafting_publisher(&plane, "revert");

    // Publish v2 (current → (1,2)).
    let digest = pub_h.draft_digest();
    pub_h
        .publish(plane.plane_key, false, &approve_token(SKILL, &digest))
        .expect("publish v2");

    // Revert to the GOOD genesis version — a forward move (current → (1,3)) restoring the v1 bytes.
    let good = hex::encode(plane.genesis.0);
    let reverted = pub_h
        .revert(plane.plane_key, &good, &approve_token(SKILL, &good), false)
        .expect("revert succeeds");
    assert_eq!(reverted.reverted_to, good);
    assert_eq!(
        reverted.current_generation,
        Generation { epoch: 1, seq: 3 },
        "forward, +1"
    );

    // A follower pulls and lands the restored genesis bytes (NOT the v2 it never saw).
    let follower = follower("revert-f");
    let pulled = follower.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(pulled.skills[0].applied, Generation { epoch: 1, seq: 3 });
    assert_eq!(
        follower.placement_files(SKILL),
        expected(&[
            ("SKILL.md", false, b"# deploy\nv1\n"),
            ("run.sh", true, b"#!/bin/sh\necho v1\n"),
        ]),
        "the follower rolls forward to the restored good (genesis) bytes"
    );
}

// ── scenario 4: a plane diff renders a proposal (`current..<hash>`) ─────────────────────────────────────

#[test]
fn diff_renders_a_proposal() {
    let plane = start_plane("diff");
    let pub_h = drafting_publisher(&plane, "diff");

    let digest = pub_h.draft_digest();
    let proposal = match pub_h
        .publish(plane.plane_key, true, &approve_token(SKILL, &digest))
        .expect("propose")
    {
        PublishResult::Proposed(d) => d.proposal,
        other => panic!("expected a proposal, got {other:?}"),
    };
    // `<skill>@<hash>` → just the hash for the diff ref.
    let hash = proposal
        .split_once('@')
        .expect("proposal is skill@hash")
        .1
        .to_owned();

    let diff = pub_h
        .diff(plane.plane_key, Some(&format!("current..{hash}")))
        .expect("plane diff renders");
    assert_eq!(
        diff.source,
        DiffSource::Plane,
        "a proposal review is a plane diff"
    );
    assert_eq!(
        diff.version_id, hash,
        "the diff targets the proposal version"
    );
    assert!(
        diff.diff.contains("v2"),
        "the diff shows the proposed change: {}",
        diff.diff
    );
}

// ── scenario 5: a mismatched --approve digest is refused BEFORE any send ────────────────────────────────

#[test]
fn a_mismatched_approve_digest_is_refused() {
    let plane = start_plane("mismatch");
    let pub_h = drafting_publisher(&plane, "mismatch");

    // Approve a digest that does NOT match the staged draft → refused locally (never signed/sent).
    let wrong = "0".repeat(64);
    let err = pub_h
        .publish(plane.plane_key, false, &approve_token(SKILL, &wrong))
        .expect_err("a digest mismatch must be refused");
    assert!(
        err.contains("--approve") || err.contains("digest"),
        "got: {err}"
    );

    // current never moved — still the genesis (1,1).
    let follower = follower("mismatch-f");
    let pulled = follower.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(
        pulled.skills[0].applied,
        Generation { epoch: 1, seq: 1 },
        "a refused publish never moved current"
    );
}

// ── scenario 6: under review_required, a DIRECT publish fails typed (APPROVAL_REQUIRED) ─────────────────

#[test]
fn a_direct_publish_under_review_required_is_typed_refused() {
    let plane = start_plane("approvalreq");
    plane.set_review_required(true);
    let pub_h = drafting_publisher(&plane, "approvalreq");

    // A direct publish is refused closed — the verb surfaces the `publish --propose` next-action; it never
    // auto-flips to a proposal.
    let digest = pub_h.draft_digest();
    let err = pub_h
        .publish(plane.plane_key, false, &approve_token(SKILL, &digest))
        .expect_err("review_required refuses a direct publish");
    assert!(
        err.contains("review") || err.contains("propose"),
        "APPROVAL_REQUIRED surfaces the propose next-action: {err}"
    );

    // Nothing ingested / moved.
    let follower = follower("approvalreq-f");
    let pulled = follower.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(pulled.skills[0].applied, Generation { epoch: 1, seq: 1 });
}

// ── scenario 7: four-eyes — a proposer cannot self-approve under review_required ────────────────────────

#[test]
fn four_eyes_blocks_a_self_approve_under_review_required() {
    let plane = start_plane("foureyes");
    plane.set_review_required(true);
    let pub_h = drafting_publisher(&plane, "foureyes");

    let digest = pub_h.draft_digest();
    let proposal = match pub_h
        .publish(plane.plane_key, true, &approve_token(SKILL, &digest))
        .expect("propose is allowed under review_required")
    {
        PublishResult::Proposed(d) => d.proposal,
        other => panic!("expected a proposal, got {other:?}"),
    };

    // The SAME identity approving its own proposal under review_required ⇒ DENIED (four-eyes).
    let err = pub_h
        .review(plane.plane_key, &proposal, true)
        .expect_err("four-eyes blocks self-approve");
    assert!(err.to_lowercase().contains("denied"), "got: {err}");

    // current never moved.
    let follower = follower("foureyes-f");
    let pulled = follower.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(pulled.skills[0].applied, Generation { epoch: 1, seq: 1 });
}

// ── scenario 8: delegated consent — a DIFFERENT reviewer approves; the follower applies, no prompt ──────

#[test]
fn delegated_consent_lands_on_a_follower_under_review_required() {
    let plane = start_plane("delegated");
    plane.set_review_required(true);
    let pub_h = drafting_publisher(&plane, "delegated");

    let digest = pub_h.draft_digest();
    let proposal = match pub_h
        .publish(plane.plane_key, true, &approve_token(SKILL, &digest))
        .expect("propose")
    {
        PublishResult::Proposed(d) => d.proposal,
        other => panic!("expected a proposal, got {other:?}"),
    };

    // A DISTINCT reviewer approves (four-eyes satisfied) — current moves to the candidate.
    let reviewer = enrolled_reviewer(&plane, "delegated-rev");
    let review = reviewer
        .review(plane.plane_key, &proposal, true)
        .expect("a different reviewer may approve");
    assert_eq!(
        review.current_generation,
        Some(Generation { epoch: 1, seq: 2 })
    );

    // The follower applies the reviewed candidate with no prompt (delegated consent).
    let follower = follower("delegated-f");
    let pulled = follower.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(pulled.skills[0].applied, Generation { epoch: 1, seq: 2 });
    assert_eq!(follower.placement_files(SKILL), expected(DRAFT));
}

// ── scenario 9: the proposals read route — pull's count + list's enumeration ────────────────────────────

#[test]
fn an_open_proposal_surfaces_in_pull_count_and_list() {
    let plane = start_plane("route");
    let pub_h = drafting_publisher(&plane, "route");

    // Before any proposal: zero.
    assert_eq!(
        pub_h.proposals_awaiting(plane.plane_key),
        0,
        "no proposals yet"
    );

    let digest = pub_h.draft_digest();
    let proposal = match pub_h
        .publish(plane.plane_key, true, &approve_token(SKILL, &digest))
        .expect("propose")
    {
        PublishResult::Proposed(d) => d.proposal,
        other => panic!("expected a proposal, got {other:?}"),
    };

    // `pull --json` reports a real count; `list <skill>` enumerates the proposal by `<skill>@<hash>`.
    assert_eq!(
        pub_h.proposals_awaiting(plane.plane_key),
        1,
        "one open proposal on the followed skill"
    );
    let pending = pub_h.list_pending_proposals(plane.plane_key);
    assert_eq!(
        pending,
        vec![proposal.clone()],
        "list enumerates the open proposal's @hash"
    );
}
