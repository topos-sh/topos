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
    common::start_plane("topos-enroll-e2e", tag, true, seed_follow_plane)
}

/// [`start_plane`] on the SPLIT link base (links ride `http://localhost:<port>`, the API stays
/// `http://127.0.0.1:<port>` — one listener): the hosted main-domain-links shape.
fn start_plane_split(tag: &str) -> Plane {
    common::start_plane_split("topos-enroll-e2e", tag, seed_follow_plane)
}

async fn seed_follow_plane(authority: &Authority) -> common::Seeded {
    {
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
    }
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

// ── the hosted main-domain-link shape: re-root + the agent instruction document, over real HTTP ───────

/// The link rides one host (`localhost` — standing in for the hosted web origin) while the plane's API
/// base is another (`127.0.0.1`). What this proves over a REAL socket: (1) the minted invite link rides
/// the configured PUBLIC link base; (2) `GET /i/{token}` without a JSON Accept serves the markdown
/// agent-instruction document (the paste-a-link-to-your-agent door) carrying the install line and the
/// full follow command; (3) the real client re-roots — the bootstrap GET hits the link host, everything
/// after (device flow, redeem, the pinned `instance.json`, the placing pull) rides the declared API
/// base; and (4) the landed bytes are the genesis, byte-exact.
#[test]
fn e2e_a_share_host_link_re_roots_and_serves_the_agent_doc() {
    let plane = start_plane_split("splitbase");
    let client = FollowHarness::new("splitbase");

    // (1) The minted link rides the public link base, not the API base.
    let link = plane.invite(0).to_owned();
    assert!(
        link.starts_with(&format!("{}/i/", plane.link_base_url)),
        "the invite link rides the link base: {link}"
    );
    assert_ne!(plane.link_base_url, plane.base_url);

    // (2) A non-JSON fetch of the link — what a browserless agent does first — is the instruction
    // document (text/plain, so a browser displays the same face inline), served over the real socket.
    let doc = http_get_markdown(&link);
    assert!(doc.contains("text/plain"), "content-type: {doc}");
    assert!(doc.contains("releases/latest/download/install.sh"));
    assert!(doc.contains(&format!("topos follow '{link}' --json")));

    // (3) The real two-call follow: bootstrap on the link host, then re-root.
    let pending = client
        .follow(&link, plane.plane_key)
        .expect("follow call 1");
    assert!(!pending.enrolled);
    assert_eq!(
        pending.plane_base_url.as_deref(),
        Some(plane.base_url.as_str()),
        "the receipt disclosed the re-rooted API base"
    );
    let user_code = pending.pending.as_ref().expect("pending").user_code.clone();
    let confirm = plane
        .rt
        .block_on(
            plane
                .authority
                .confirm_external_identity(&user_code, INVITEE, NOW),
        )
        .expect("confirm the session identity");
    assert!(matches!(confirm, ConfirmOutcome::Confirmed));
    let done = client.resume(plane.plane_key).expect("follow --resume");
    assert!(done.enrolled);
    assert_eq!(
        done.plane_base_url.as_deref(),
        Some(plane.base_url.as_str())
    );
    assert_eq!(
        client.instance_pinned_key(),
        Some(plane.plane_key),
        "the TOFU pin committed under the API base"
    );

    // (4) The placing pull rides the API base and lands the genesis byte-exact.
    let target = format!("{SKILL}@{}", hex::encode(plane.genesis().0));
    client
        .approve(&plane.base_url, plane.plane_key, &[target])
        .expect("follow --approve");
    let got = client.placement_files(SKILL);
    assert_eq!(got, expected_placement(&genesis_files()));
}

/// A minimal HTTP/1.1 GET over a plain TcpStream (no client library — the e2e crate carries none): send
/// a browserless-agent-shaped request (`Accept: */*`) and return the raw response (headers + body).
fn http_get_markdown(link: &str) -> String {
    use std::io::{Read as _, Write as _};

    let rest = link.strip_prefix("http://").expect("a loopback http link");
    let (host, path) = rest.split_once('/').expect("a path");
    let mut stream = std::net::TcpStream::connect(host).expect("connect the link host");
    write!(
        stream,
        "GET /{path} HTTP/1.1\r\nHost: {host}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    )
    .expect("send the request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read the response");
    response
}
