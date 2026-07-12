//! E2E — the real `topos follow <address>` over loopback HTTP against the real plane.
//!
//! Proves the whole ADDRESS enrollment loop end to end, replacing the retired invite-link door: an owner
//! seats a member through the REAL invitation op (a member-lane roster write — no `/i/` link), then a
//! fresh client `follow`s the workspace ADDRESS — fetching the constant protocol card over the real socket
//! (JSON face carries the API base; the markdown face is the constant agent hand-off), re-rooting onto the
//! declared `api_base_url`, minting a `0600` device keypair, device-authorizing toward the address NAME
//! (intent enroll), confirming the identity in-process (the human's verification, the authority's
//! external-confirm op), resuming to redeem OVER THE WIRE (the grant is the bearer credential; the server
//! checks the redeem body's device public key against the grant's bound key), DESCRIBING what `--yes`
//! would land, and finally applying it byte-exact (`everyone`'s genesis, incl. the executable bit).
//!
//! The off-roster case now asserts the UNIFORM denial (the address resolves, the identity proves, but no
//! seat ⇒ the one `ENROLL_UNAVAILABLE`-shaped `REQUEST_ACCESS` refusal). The split-origin test proves the
//! card on a WEB-base listener + the API re-root: the card rides the link host, the API rides the declared
//! base.

mod common;

use common::{NOW, Plane, SKILL, WS, WS_NAME, expected_placement, genesis_files, ws_address};
use plane_store::{Authority, OpId, SkillId, WorkspaceId};
use topos::test_support::FollowHarness;
use topos_types::Generation;
use topos_types::TerminalOutcome;
use topos_types::requests::WireProtocolCard;

// ── shared constants ──────────────────────────────────────────────────────────────────────────────
const OWNER: &str = "p_owner";
const OWNER_DKID: &str = "dk_owner";
/// The owner device's registered 32-byte public key (a fixed test value; nothing verifies against it).
const OWNER_PUBKEY: [u8; 32] = [9u8; 32];
/// The owner's workspace Bearer credential — publishes the genesis and drives the invitation op.
const OWNER_CRED: &str = "wc_owner_secret";
/// The invitee is identified by an email — the plane confirms it, and it becomes the rostered principal.
const INVITEE: &str = "alice@acme.test";
const AUTHOR: &str = "d_test";
const MSG: &str = "topos publish";
const AT: &str = "2026-06-30T00:00:00Z";
const GENESIS_OP: &str = "a0000000-0000-4000-8000-000000000001";

/// Stand the plane up via the shared harness (bind-first, so the enrollment `base_url` is the real
/// loopback address): the workspace + owner, the genesis published into `everyone`, and the invitee
/// seated as an INVITED member through the real invitation op (the redeem flips it to confirmed).
fn start_plane(tag: &str) -> Plane {
    common::start_plane("topos-enroll-e2e", tag, true, seed_follow_plane)
}

/// [`start_plane`] on the SPLIT link base (the card + links ride `http://localhost:<port>`, the API stays
/// `http://127.0.0.1:<port>` — one listener): the hosted main-domain-address shape.
fn start_plane_split(tag: &str) -> Plane {
    common::start_plane_split("topos-enroll-e2e", tag, seed_follow_plane)
}

async fn seed_follow_plane(authority: &Authority) -> common::Seeded {
    let ws = WorkspaceId::parse(WS).unwrap();
    let skill = SkillId::parse(SKILL).unwrap();

    // The workspace + a confirmed owner holding OWNER_CRED (publishes the genesis, drives the invite).
    authority
        .seed_workspace(&ws, "Acme", "verified", "cloud")
        .await
        .expect("seed workspace");
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

    // The genesis lands in `everyone` (a registering publish places it there) — so a fresh member is
    // entitled to it the instant they join. A real publish carries the folder name, so the offer +
    // the follower's local skill name match SKILL.
    let receipt = authority
        .seed_published_genesis(
            &ws,
            &skill,
            OWNER_CRED,
            &OpId::parse(GENESIS_OP).unwrap(),
            genesis_files(),
            AUTHOR,
            MSG,
            Some(SKILL),
            AT,
            NOW,
        )
        .await
        .expect("seed genesis");
    assert_eq!(receipt.outcome, TerminalOutcome::Ok);
    assert_eq!(receipt.current, Some(Generation { epoch: 1, seq: 1 }));
    let genesis = receipt.version_id.expect("genesis version id");

    // Seat the invitee through the REAL invitation op (the member-lane roster write) — no `/i/` link;
    // the address IS the join target, the roster is the lock. The redeem flips invited → confirmed.
    let invited = common::invite_member(authority, &ws, OWNER_CRED, &[INVITEE], &[], AT).await;
    assert_eq!(
        invited,
        vec![INVITEE.to_owned()],
        "the invite seats the member"
    );

    common::Seeded {
        genesis: Some(genesis),
        invites: Vec::new(),
    }
}

// ── the keystone: a real address follow enrolls, describes, and lands `everyone`'s set ──────────────────

#[test]
fn e2e_real_follow_enrolls_describes_and_lands_the_first_skill() {
    let plane = start_plane("follow");
    let client = FollowHarness::new("follow");
    let address = ws_address(&plane.base_url);

    // The constant protocol card, over the REAL socket, BOTH faces once:
    //  - JSON carries the API base the client re-roots onto;
    //  - the markdown face is the constant agent hand-off (no path echo — no existence oracle).
    let card = fetch_card_json(&address);
    assert_eq!(card.schema_version, 1);
    assert_eq!(card.card, "topos-protocol-card");
    assert_eq!(
        card.api_base_url, plane.base_url,
        "the JSON card discloses the API base (the machine bootstrap)"
    );
    let markdown = http_get(&address, "*/*");
    assert!(
        markdown.contains("text/plain"),
        "the markdown face: {markdown}"
    );
    assert!(markdown.contains("A Topos resource address"));
    assert!(markdown.contains("releases/latest/download/install.sh"));

    // Call 1 — `topos follow <address>`: fetch the card, re-root, mint the device keypair, device-authorize.
    // The identity leg is completed in-process (the authority's external-confirm op — the flow is headless).
    common::begin_address_enroll(&plane, &client, &address, INVITEE);
    assert!(
        client.wal_exists(),
        "the pending WAL is written (0600 resume journal)"
    );
    assert_eq!(
        client.device_key_mode(),
        Some(0o600),
        "the device private seed is a SEPARATE 0600 file"
    );

    // Call 2 — re-invoke `topos follow <address>`: poll (granted) → redeem OVER THE WIRE → promote →
    // continue into the two-phase DESCRIBE (the enrollment landed; the subscription still awaits `--yes`).
    let describe = client.resume_describe().expect("the resume describes");
    assert!(
        client.instance_written(),
        "instance.json committed at promote"
    );
    assert!(
        !client.wal_exists(),
        "the WAL is consumed once promotion completes"
    );
    assert!(describe.enrolled_now, "THIS invocation enrolled the device");
    assert_eq!(describe.workspace_id, WS);
    assert_eq!(describe.workspace_name, WS_NAME);
    assert_eq!(describe.role, "member", "the confirmed seat is a member");
    // The describe lists exactly the `everyone`-delivered genesis, with its consent digest + `via`.
    assert_eq!(
        describe.installs.len(),
        1,
        "one install: {:?}",
        describe.installs
    );
    let install = &describe.installs[0];
    assert_eq!(install.skill_id, SKILL);
    assert_eq!(
        install.version_id.as_deref(),
        Some(hex::encode(plane.genesis().0).as_str()),
        "the install pins the genesis version"
    );
    assert!(
        install.bundle_digest.is_some(),
        "the consent digest is disclosed"
    );
    assert_eq!(install.via_channels, vec!["everyone".to_owned()]);
    assert!(
        !install.via_direct,
        "it arrives via the everyone channel, not a direct follow"
    );
    assert!(
        !describe.all_devices_note.is_empty(),
        "the person-scoped disclosure"
    );
    assert!(
        !describe.reporting_note.is_empty(),
        "the fleet-reporting disclosure"
    );

    // Call 3 — `topos follow <address> --yes`: apply. The reconcile lands `everyone`'s set this invocation.
    let applied = client.follow_apply(&address).expect("the --yes apply");
    assert!(!applied.enrolled_now, "already enrolled by call 2");
    assert_eq!(
        applied.installed.len(),
        1,
        "the genesis landed: {:?}",
        applied.installed
    );
    assert_eq!(applied.installed[0].skill_id, SKILL);
    assert!(
        applied.warnings.is_empty(),
        "a clean apply: {:?}",
        applied.warnings
    );

    // The placement holds the EXACT genesis bytes — path/mode/content byte-for-byte, incl. the exec bit.
    let got = client.placement_files(SKILL);
    assert_eq!(
        got,
        expected_placement(&genesis_files()),
        "the genesis is placed byte-exact"
    );
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

// ── an off-roster identity is denied at the redeem: the UNIFORM denial ──────────────────────────────────

#[test]
fn e2e_off_roster_identity_gets_the_uniform_denial() {
    let plane = start_plane("offroster");
    let client = FollowHarness::new("offroster");
    let address = ws_address(&plane.base_url);

    // The agent fetches the card + device-authorizes fine (the address is a public enrollment START), and
    // the confirmed identity is a stranger NOT on the workspace roster.
    common::begin_address_enroll(&plane, &client, &address, "mallory@evil.test");

    // The resume polls + attempts the redeem; the off-roster identity is DENIED — the ONE uniform
    // membership refusal (an unresolved address, a wrong workspace, and an off-roster identity are
    // byte-indistinguishable), surfaced by the client as the ask-an-owner `REQUEST_ACCESS` guidance.
    let denial = client.resume_expect_denied();
    assert_eq!(denial.code, "DENIED");
    assert_eq!(
        denial.next_action_codes,
        vec!["REQUEST_ACCESS".to_owned()],
        "the denied redeem carries the REQUEST_ACCESS next-action"
    );
    assert!(
        !client.enrolled(),
        "no enrollment state lands for a denied redeem"
    );
    assert!(!client.instance_written(), "nothing was promoted");
}

// ── the hosted main-domain-address shape: the card on a WEB base + the API re-root ─────────────────────

/// The card + address ride one host (`localhost` — standing in for the hosted web origin) while the
/// plane's API base is another (`127.0.0.1`). What this proves over a REAL socket: (1) the JSON card at
/// the WEB base declares the API base; (2) the client re-roots — the card GET hits the link host,
/// everything after (device flow, redeem, `instance.json`, the reconcile) rides the declared API base;
/// and (3) the landed bytes are the genesis, byte-exact.
#[test]
fn e2e_a_web_base_address_re_roots_and_lands_the_genesis() {
    let plane = start_plane_split("splitbase");
    let client = FollowHarness::new("splitbase");
    assert_ne!(plane.link_base_url, plane.base_url);
    let web_address = ws_address(&plane.link_base_url);

    // (1) The JSON card at the WEB base declares the API base (not the web base).
    let card = fetch_card_json(&web_address);
    assert_eq!(
        card.api_base_url, plane.base_url,
        "the card on the web origin re-roots the client onto the API base"
    );

    // (2) The real follow: card + device flow via the web address, then re-root — enroll + describe.
    common::begin_address_enroll(&plane, &client, &web_address, INVITEE);
    let describe = client.resume_describe().expect("the resume describes");
    assert!(describe.enrolled_now);
    assert_eq!(describe.workspace_id, WS);
    assert!(
        client.instance_written(),
        "instance.json committed under the API base"
    );

    // (3) The `--yes` apply rides the API base and lands the genesis byte-exact.
    let applied = client.follow_apply(&web_address).expect("the --yes apply");
    assert_eq!(applied.installed[0].skill_id, SKILL);
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&genesis_files())
    );
}

// ── raw HTTP helpers (the e2e crate carries no client library) ──────────────────────────────────────────

/// A minimal HTTP/1.1 GET over a plain `TcpStream` with an explicit `Accept`, returning the raw response
/// (status line + headers + body).
fn http_get(url: &str, accept: &str) -> String {
    use std::io::{Read as _, Write as _};

    let rest = url.strip_prefix("http://").expect("a loopback http url");
    let (host, path) = rest.split_once('/').expect("a path");
    let mut stream = std::net::TcpStream::connect(host).expect("connect the host");
    write!(
        stream,
        "GET /{path} HTTP/1.1\r\nHost: {host}\r\nAccept: {accept}\r\nConnection: close\r\n\r\n"
    )
    .expect("send the request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read the response");
    response
}

/// GET `url` asking for JSON and parse the [`WireProtocolCard`] body (the fallback card's machine face).
fn fetch_card_json(url: &str) -> WireProtocolCard {
    let raw = http_get(url, "application/json");
    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .expect("a response body follows the headers");
    serde_json::from_str(body.trim()).unwrap_or_else(|e| panic!("a WireProtocolCard: {e}\n{raw}"))
}
