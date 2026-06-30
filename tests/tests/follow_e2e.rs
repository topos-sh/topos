//! E2E — the real `topos follow` over loopback HTTP against the real plane.
//!
//! Proves the whole enrollment loop end to end, replacing the fixture-seeded follow: an owner mints an `/i/`
//! invite (a governance-signed op the plane re-derives + verifies), then a fresh client `follow`s the link —
//! fetching the bootstrap, TOFU-pinning the plane key, minting a `0600` device seed, device-authorizing,
//! confirming the identity (the human's verification, driven in-process via the authority's external-confirm
//! op so the flow is headless), resuming to sign the **enroll possession proof** + redeem OVER THE WIRE (the
//! server `verify_enroll`s it — the two-halves wire proof), and finally placing the first-received bundle
//! byte-exact (incl. the executable bit).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use ed25519_dalek::{Signer as _, SigningKey};
use plane_store::{
    Authority, CommitId, ConfirmOutcome, CreateInviteOutcome, DeploymentMode, EnrollmentConfig,
    FileMode, GovernanceOp, GovernanceSignedOp, OpId, Principal, Role, SkillId, UploadedFile,
    WorkspaceId,
};
use topos::test_support::FollowHarness;
use topos_core::sign::{GovernanceOpFields, GovernanceOpKind, governance_op_preimage};
use topos_plane::{PlaneState, router};
use topos_types::{Generation, TerminalOutcome};

// ── shared constants ──────────────────────────────────────────────────────────────────────────────
const WS: &str = "w_acme";
const SKILL: &str = "s_deploy";
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
const NOW: i64 = 1_000_000;
const GENESIS_OP: &str = "a0000000-0000-4000-8000-000000000001";
const INVITE_OP: &str = "b0000000-0000-4000-8000-000000000001";

/// The plane's genesis bundle: a regular doc + an EXECUTABLE script (the exec bit must survive to placement).
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

/// A self-cleaning temp dir (RAII).
struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("topos-enroll-e2e-{tag}-{}-{n}", std::process::id()));
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

/// A running loopback plane configured for enrollment + seeded with a workspace, an owner, a published skill,
/// a pre-rostered invitee, and a minted `/i/` invite link.
struct Plane {
    rt: tokio::runtime::Runtime,
    authority: Arc<Authority>,
    base_url: String,
    plane_key: [u8; 32],
    genesis: CommitId,
    invite_link: String,
    _dir: Scratch,
}

/// Mint the `/i/` invite the owner issues — a governance-signed `Invite` op the plane re-derives + verifies.
/// The owner signs the SAME `topos-core` frame the plane rebuilds (role byte 3 = Member, `expires_at = 0`, the
/// invited-email set, the offered-skill-id set), so this in-process mint cannot drift from production.
async fn mint_invite(authority: &Authority, ws: &WorkspaceId, skill: &SkillId) -> String {
    let hyphenless: String = INVITE_OP.chars().filter(|c| *c != '-').collect();
    let mut op_id_bytes = [0u8; 16];
    hex::decode_to_slice(&hyphenless, &mut op_id_bytes).expect("op_id is 16 hex bytes");

    let fields = GovernanceOpFields {
        workspace_id: WS,
        op_id: op_id_bytes,
        device_key_id: OWNER_DKID,
        op: GovernanceOpKind::Invite {
            role: 3, // Member
            expires_at: 0,
            emails: &[INVITEE],
            skills: &[SKILL],
        },
    };
    let preimage = governance_op_preimage(&fields).expect("governance preimage");
    let signature = SigningKey::from_bytes(&OWNER_SEED)
        .sign(&preimage)
        .to_bytes();
    let signed = GovernanceSignedOp {
        device_key_id: OWNER_DKID.to_owned(),
        op: GovernanceOp::Invite {
            role: Role::Member,
            expires_at: None,
            emails: vec![Principal::parse(INVITEE).unwrap()],
            skills: vec![(skill.clone(), None)],
        },
        signature,
    };
    match authority
        .create_invite(ws, INVITE_OP, signed, AT)
        .await
        .expect("create_invite")
    {
        CreateInviteOutcome::Created(invite) => invite.link,
        CreateInviteOutcome::Denied(reason) => panic!("invite denied: {reason}"),
    }
}

/// Stand the plane up: bind the loopback socket FIRST (the bootstrap echoes this `base_url`), then open +
/// enroll-configure + seed the authority, mint the invite, and serve `router(state)`.
fn start_plane(tag: &str) -> Plane {
    let dir = Scratch::new(tag);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    // Bind before configuring so the enrollment base_url is the real loopback address.
    let listener = rt
        .block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await })
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("local addr");
    let base_url = format!("http://{addr}");

    let (authority, genesis, plane_key, invite_link) = rt.block_on(async {
        let authority =
            Authority::open_sqlite(&dir.0.join("db"), &dir.0.join("git"), &dir.0.join("large"))
                .await
                .expect("open authority")
                .with_plane_key(&dir.0.join("plane.key"))
                .expect("load plane key")
                .with_enrollment_config(EnrollmentConfig {
                    secret_path: dir.0.join("enroll.key"),
                    base_url: base_url.clone(),
                    deployment_mode: DeploymentMode::Cloud,
                    enrollment_method: "device_code".to_owned(),
                })
                .expect("load enrollment secret");

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

        let invite_link = mint_invite(&authority, &ws, &skill).await;
        let plane_key = authority.plane_public_key().expect("plane public key");
        (authority, genesis, plane_key, invite_link)
    });

    let authority = Arc::new(authority);
    let state = PlaneState::new(authority.clone());
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
        base_url,
        plane_key,
        genesis,
        invite_link,
        _dir: dir,
    }
}

// ── the keystone: a real follow lands the first skill ──────────────────────────────────────────────────

#[test]
fn e2e_real_follow_enrolls_and_lands_the_first_skill() {
    let plane = start_plane("follow");
    let client = FollowHarness::new("follow");

    // Call 1: `topos follow <link>` — fetch the bootstrap, TOFU-pin, mint the device seed, device-authorize.
    let pending = client
        .follow(&plane.invite_link, plane.plane_key)
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
    let target = format!("{SKILL}@{}", hex::encode(plane.genesis.0));
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
        .follow(&plane.invite_link, plane.plane_key)
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

// ── helpers ───────────────────────────────────────────────────────────────────────────────────────

/// The placement-snapshot shape (`(path, mode & 0o777, bytes)`, sorted): regular files 0o644, executable 0o755.
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
