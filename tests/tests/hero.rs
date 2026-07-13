//! HERO — the real client pull engine over loopback HTTP against the real plane.
//!
//! One real `plane-store` [`Authority`] (seeded through the feature-gated test-fixtures shims) is served by
//! the composed [`topos_plane::router`] on a real `127.0.0.1:0` socket on a background multi-threaded tokio
//! runtime — all through the shared `common` harness. The client side is the GENUINE pull engine
//! (`topos::ops::pull` via `topos::test_support`) over the GENUINE [`topos::test_support`]-wrapped `ureq`
//! transport — the SYNC client is driven from a plain (non-async) thread, never inside the runtime.
//!
//! Three scenarios, each on its own freshly-seeded plane + client home:
//! 1. first pull fast-forwards onto the plane's genesis, byte-exact incl. the executable bit;
//! 2. an immediate second pull is a commit-sensitive **304 no-op** (nothing re-materialized);
//! 3. a forward move to a v2 (an ordinary UNSIGNED advanced record) applies byte-exact on the next pull —
//!    no signature, no verification, just the served record + the content-addressed digest re-check on apply.

mod common;

use common::{NOW, Plane, SKILL, WS, expected_placement, genesis_files};
use plane_store::{Authority, FileMode, OpId, UploadedFile};
use topos::test_support::{Follow, PullHarness, Scope};
use topos_types::results::PullAction;
use topos_types::{Generation, TerminalOutcome};

// ── shared constants ──────────────────────────────────────────────────────────────────────────────
const DKID: &str = "dk_a";
const PRINCIPAL: &str = "p_dev";
/// The publisher device's workspace Bearer credential — and the one the follower presents to read (a
/// confirmed member reads every skill; per-skill read tokens are gone).
const CRED: &str = "wc_hero_secret_value";
const AUTHOR: &str = "d_test";
const MESSAGE: &str = "topos publish";
const CREATED_AT: &str = "2026-06-29T00:00:00Z";
/// The device's registered 32-byte public key (a fixed test value; nothing verifies against it).
const DEVICE_PUBKEY: [u8; 32] = [7u8; 32];
const GENESIS_OP: &str = "a0000000-0000-4000-8000-000000000001";
const CHILD_OP: &str = "a0000000-0000-4000-8000-000000000002";

/// A v2 bundle (a forward child of genesis) — the forward-move scenario 3 applies it byte-exact.
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

/// Seed a real authority (device+credential+confirmed-member → genesis), then stand the COMPOSED
/// stack up — the web app in front, the vault internal — via the shared harness. The pull engine
/// below dials the APP's `/api` base, exactly like a real client after the door cutover.
fn start_plane(tag: &str) -> Plane {
    common::start_stack(
        "topos-hero-plane",
        tag,
        false,
        async |authority: &Authority| {
            let genesis = common::seed_genesis_plane(
                authority,
                common::GenesisSpec {
                    dkid: DKID,
                    device_pubkey: &DEVICE_PUBKEY,
                    op_id: GENESIS_OP,
                    files: genesis_files(),
                    principal: PRINCIPAL,
                    author: AUTHOR,
                    message: MESSAGE,
                    created_at: CREATED_AT,
                    credential: CRED,
                },
            )
            .await;
            common::Seeded {
                genesis: Some(genesis),
                ..Default::default()
            }
        },
    )
}

// ── scenario 1: first pull fast-forwards + byte-exact (incl. the executable bit) ──────────────────────

#[test]
fn first_pull_fast_forwards_byte_exact() {
    let plane = start_plane("ff");
    let mut client = PullHarness::new("ff");
    client.adopt_followed(SKILL, WS, CRED, Follow::Auto, LOCAL_PLACEHOLDER);

    let data = client.run_pull(&plane.base_url, Scope::AllFollowed);

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
    assert_eq!(sync.base_commit, hex::encode(plane.genesis().0));
}

// ── scenario 2: an immediate second pull is a 304 no-op ───────────────────────────────────────────────

#[test]
fn second_pull_is_a_304_no_op() {
    let plane = start_plane("304");
    let mut client = PullHarness::new("304");
    client.adopt_followed(SKILL, WS, CRED, Follow::Auto, LOCAL_PLACEHOLDER);

    // First pull fast-forwards to (1,1).
    let first = client.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(first.skills[0].action, PullAction::FastForwarded);
    let placement_after_ff = client.placement_files(SKILL);
    let sync_after_ff = client.sync_state(SKILL);

    // Second pull: the client sends If-None-Match: "1.1" + Topos-Known-Version-Id: <genesis>; the plane,
    // unchanged at (1,1)/<genesis>, answers 304. The engine maps that to UpToDate and re-materializes nothing.
    let second = client.run_pull(&plane.base_url, Scope::AllFollowed);
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

// ── scenario 3: a forward move to v2 applies byte-exact (an unsigned advanced record, no ceremony) ─────

#[test]
fn a_forward_move_to_v2_applies_byte_exact() {
    let plane = start_plane("v2");
    let mut client = PullHarness::new("v2");
    client.adopt_followed(SKILL, WS, CRED, Follow::Auto, LOCAL_PLACEHOLDER);

    // Reach the genesis (1,1).
    let ff = client.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(ff.skills[0].action, PullAction::FastForwarded);
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&genesis_files())
    );

    // Move `current` FORWARD to a v2 (a 1-parent publish on genesis) — an ordinary UNSIGNED advanced record.
    let authority = plane.authority.clone();
    let ws = plane.ws();
    let skill = plane.skill();
    let genesis = plane.genesis();
    plane.rt.block_on(async {
        let receipt = authority
            .seed_published_child(
                &ws,
                &skill,
                CRED,
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
    });

    // The next pull fetches the advanced record and fast-forwards onto v2 byte-exact — there is no
    // signature and no client-side verification, only the served record plus the content-addressed digest
    // re-check on apply (a mismatch would be a loud integrity error; a clean move just lands).
    let applied = client.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(
        applied.skills[0].action,
        PullAction::FastForwarded,
        "an unsigned advanced record applies with no ceremony: {:?}",
        applied.skills[0]
    );
    assert_eq!(applied.skills[0].applied, Generation { epoch: 1, seq: 2 });
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&v2_files()),
        "the v2 bundle materializes byte-exact"
    );
    let sync = client.sync_state(SKILL);
    assert_eq!(sync.applied, Generation { epoch: 1, seq: 2 });
    assert_eq!(sync.observed, Generation { epoch: 1, seq: 2 });
}
