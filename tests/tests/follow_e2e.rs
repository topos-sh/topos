//! E2E — the real `topos follow` over loopback HTTP against the real plane.
//!
//! Proves the whole enrollment loop end to end, replacing the fixture-seeded follow: an owner mints an `/i/`
//! invite (a governance-signed op the plane re-derives + verifies), then a fresh client `follow`s the link —
//! fetching the bootstrap, TOFU-pinning the plane key, minting a `0600` device seed, device-authorizing,
//! confirming the identity (the human's verification, driven in-process via the authority's external-confirm
//! op so the flow is headless), resuming to sign the **enroll possession proof** + redeem OVER THE WIRE (the
//! server `verify_enroll`s it — the two-halves wire proof), and finally placing the first-received bundle
//! byte-exact (incl. the executable bit).

mod common;

use common::{NOW, Plane, SKILL, WS, expected_placement, genesis_files};
use ed25519_dalek::SigningKey;
use plane_store::{Authority, ConfirmOutcome, OpId, Principal, SkillId, WorkspaceId};
use topos::test_support::FollowHarness;
use topos_types::{Generation, TerminalOutcome};

// ── shared constants ──────────────────────────────────────────────────────────────────────────────
const OWNER: &str = "p_owner";
const OWNER_DKID: &str = "dk_owner";
const OWNER_SEED: [u8; 32] = [9u8; 32];
/// The invitee is identified by an email — the cloud confirms it, and it becomes the rostered principal.
const INVITEE: &str = "alice@acme.test";
/// The publisher of the offered skill (a distinct principal — the invitee reads it through the granted roster).
const PUB_SEED: [u8; 32] = [7u8; 32];
const PUB_DKID: &str = "dk_pub";
const PUB_PRINCIPAL: &str = "p_pub";
const AUTHOR: &str = "d_test";
const MSG: &str = "topos publish";
const AT: &str = "2026-06-30T00:00:00Z";
const GENESIS_OP: &str = "a0000000-0000-4000-8000-000000000001";
const INVITE_OP: &str = "b0000000-0000-4000-8000-000000000001";

/// Stand the plane up via the shared harness (bind-first, so the enrollment `base_url` is the real
/// loopback address): the workspace + owner, the published skill, a pre-rostered invitee, and the minted
/// `/i/` invite link.
fn start_plane(tag: &str) -> Plane {
    common::start_plane(
        "topos-enroll-e2e",
        tag,
        true,
        async |authority: &Authority| {
            let ws = WorkspaceId::parse(WS).unwrap();
            let skill = SkillId::parse(SKILL).unwrap();
            let owner = Principal::parse(OWNER).unwrap();
            let publisher = Principal::parse(PUB_PRINCIPAL).unwrap();
            let invitee = Principal::parse(INVITEE).unwrap();

            // The workspace + the owner (with a registered device so the owner can sign the invite).
            authority
                .seed_workspace(&ws, "Acme", "verified", "cloud")
                .await
                .expect("seed workspace");
            authority
                .seed_workspace_member(&ws, &owner, "owner", "confirmed")
                .await
                .expect("seed owner");
            let owner_pk = SigningKey::from_bytes(&OWNER_SEED)
                .verifying_key()
                .to_bytes();
            authority
                .seed_device(&ws, OWNER_DKID, &owner_pk, &owner, false)
                .await
                .expect("seed owner device");

            // The published skill the invite offers.
            let pub_pk = SigningKey::from_bytes(&PUB_SEED).verifying_key().to_bytes();
            authority
                .seed_device(&ws, PUB_DKID, &pub_pk, &publisher, false)
                .await
                .expect("seed publisher device");
            authority
                .seed_roster(&ws, &skill, &publisher)
                .await
                .expect("seed publisher roster");
            let receipt = authority
                .seed_published_genesis(
                    &ws,
                    &skill,
                    PUB_DKID,
                    &PUB_SEED,
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

            // Pre-roster the invitee on the workspace — the cloud redeem gate requires it (a leaked link is inert
            // to anyone NOT on this list).
            authority
                .seed_workspace_member(&ws, &invitee, "member", "invited")
                .await
                .expect("pre-roster invitee");

            let invite_link = common::mint_invite(
                authority,
                &ws,
                (OWNER_DKID, &OWNER_SEED),
                INVITE_OP,
                INVITEE,
                SKILL,
                AT,
            )
            .await;
            common::Seeded {
                genesis: Some(genesis),
                invites: vec![invite_link],
            }
        },
    )
}

// ── the keystone: a real follow lands the first skill ──────────────────────────────────────────────────

#[test]
fn e2e_real_follow_enrolls_and_lands_the_first_skill() {
    let plane = start_plane("follow");
    let client = FollowHarness::new("follow");

    // Call 1: `topos follow <link>` — fetch the bootstrap, TOFU-pin, mint the device seed, device-authorize.
    let pending = client
        .follow(plane.invite(0), plane.plane_key)
        .expect("follow call 1");
    assert!(!pending.enrolled, "call 1 only begins enrollment");
    let user_code = pending
        .pending
        .as_ref()
        .expect("the pending arm carries the verification handle")
        .user_code
        .clone();
    assert!(
        client.wal_exists(),
        "the pending WAL is written (0600 resume journal)"
    );
    assert_eq!(
        client.device_key_mode(),
        Some(0o600),
        "the device private seed is a SEPARATE 0600 file, never in host.json"
    );

    // The human's verification, headless: the external-confirm op sets the session's confirmed identity (so the
    // device's next poll yields a grant). The agent only ever polls — it never holds a user token.
    let confirm = plane
        .rt
        .block_on(
            plane
                .authority
                .confirm_external_identity(&user_code, INVITEE, NOW),
        )
        .expect("confirm the session identity");
    assert!(matches!(confirm, ConfirmOutcome::Confirmed));

    // Call 2: `topos follow --resume` — poll (granted), sign the enroll possession proof, redeem OVER THE WIRE.
    // The server `verify_enroll`s the proof against the grant's bound device key — the two-halves wire proof.
    let done = client.resume(plane.plane_key).expect("follow --resume");
    assert!(done.enrolled, "enrolled after the resume redeem");
    assert!(
        !client.wal_exists(),
        "the WAL is consumed once promotion completes"
    );
    assert_eq!(
        client.instance_pinned_key(),
        Some(plane.plane_key),
        "the plane key (TOFU-pinned from the unauthenticated bootstrap) is committed at promote"
    );
    assert!(
        client.follows_count() >= 1,
        "the offered skill is now followed"
    );
    assert!(
        client.enrolled(),
        "load_enrollment now lights up (instance.json pinned + a followed skill)"
    );

    // `topos follow --approve` — place the first-received bytes (a never-received skill is an OFFER until this).
    let target = format!("{SKILL}@{}", hex::encode(plane.genesis().0));
    client
        .approve(&plane.base_url, plane.plane_key, &[target])
        .expect("follow --approve");

    // The placement holds the EXACT genesis bytes — path/mode/content byte-for-byte, incl. the exec bit.
    let got = client.placement_files(SKILL);
    let want = expected_placement(&genesis_files());
    assert_eq!(got, want, "the genesis bundle is placed byte-exact");
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
}

// ── a leaked invite is inert to an off-roster identity ────────────────────────────────────────────────

#[test]
fn e2e_off_roster_identity_cannot_redeem_a_leaked_invite() {
    let plane = start_plane("offroster");
    let client = FollowHarness::new("offroster");

    // The agent fetches the bootstrap + device-authorizes fine (the /i/ link is a public enrollment START).
    let pending = client
        .follow(plane.invite(0), plane.plane_key)
        .expect("follow call 1");
    let user_code = pending.pending.expect("pending arm").user_code;

    // But the confirmed identity is NOT on the workspace roster — the cloud gate makes redemption inert.
    let stranger = "mallory@evil.test";
    let confirm = plane
        .rt
        .block_on(
            plane
                .authority
                .confirm_external_identity(&user_code, stranger, NOW),
        )
        .expect("confirm sets the session identity to the stranger");
    assert!(matches!(confirm, ConfirmOutcome::Confirmed));

    // The resume polls + attempts the redeem; the off-roster identity is DENIED — a leaked link enrolls no one.
    let outcome = client.resume(plane.plane_key);
    assert!(
        outcome.is_err(),
        "an off-roster identity must be denied at redeem: {outcome:?}"
    );
    assert!(
        !client.enrolled(),
        "no enrollment state lands for a denied redeem"
    );
}
