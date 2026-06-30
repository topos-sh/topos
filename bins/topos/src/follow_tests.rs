//! End-to-end tests of the `follow` device-flow over a FAKE `EnrollSource` + a fixture plane (no HTTP):
//! the two-call resume (pending → granted → promote), the TOFU rules, the 0600 sidecar writers + secret
//! redaction, the merge-on-second-follow, the Redeemed-WAL recovery, and the first-receive baseline that
//! the existing pull engine offers (bare sweep) then places (explicit accept).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};

use topos_core::digest::{self, FileMode, ManifestEntry, to_hex};
use topos_core::sign::{self, Commit, CurrentPointer, EnrollFields, verify_enroll};
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::bootstrap::{
    BootstrapData, BootstrapInvite, BootstrapPlane, BootstrapSigningKey, BootstrapSkill,
    BootstrapWorkspace, ConsentMode, DeploymentMode, VerifiedDomainStatus,
};
use topos_types::persisted::SyncState;
use topos_types::results::PullAction;
use topos_types::{
    CurrencyKind, CurrentRecord, Generation, HarnessId, PointerScope, Signature, SignatureAlg,
    SignedCurrentRecord, TriggerReport, TriggerState,
};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    DeviceAuthorize, EnrollSource, FetchedFile, FetchedVersion, FollowContext, FollowMode,
    FollowSource, Grant, InertFollow, InertPlane, KnownCurrent, PlaneError, PlaneSource,
    PointerFetch, Redeem, RedeemedCred, TokenPoll,
};
use crate::plane_http::SkillCred;
use crate::sidecar::Layout;
use crate::{doc, enroll, identity, ops};

const WS: &str = "w_acme";
const BASE_URL: &str = "https://acme.topos.test";
const PLANE_SEED: [u8; 32] = [9u8; 32];

// ---------------------------------------------------------------------------------------------
// Scratch + the plane key.
// ---------------------------------------------------------------------------------------------

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-fol-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn plane_key() -> SigningKey {
    SigningKey::from_bytes(&PLANE_SEED)
}
fn plane_pubkey() -> [u8; 32] {
    plane_key().verifying_key().to_bytes()
}
fn plane_pubkey_b64() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(plane_pubkey())
}
/// `dk_<first 32 hex of sha256(pubkey)>` — the server-side derivation the device signer mirrors.
fn device_key_id_for(pubkey: &[u8; 32]) -> String {
    let hex = to_hex(&digest::sha256(pubkey));
    format!("dk_{}", &hex[..32])
}

// ---------------------------------------------------------------------------------------------
// A harness whose placement_for(None) returns an ABSOLUTE temp path (so the first-receive apply can
// first-install there). The engine reads the placement from map.json, so discover() stays empty.
// ---------------------------------------------------------------------------------------------

struct TmpHarness {
    skills_root: PathBuf,
}
impl HarnessAdapter for TmpHarness {
    fn id(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }
    fn discover(&self) -> Vec<DiscoveredPlacement> {
        Vec::new()
    }
    fn placement_for(&self, skill_id: &str, _d: Option<&DiscoveredPlacement>) -> PlacementTarget {
        PlacementTarget {
            dir: self.skills_root.join(skill_id),
        }
    }
    fn currency_kind(&self) -> CurrencyKind {
        CurrencyKind::ExplicitPullOnly
    }
    fn install_currency_trigger(&self) -> TriggerReport {
        no_trigger()
    }
    fn remove_currency_trigger(&self) -> TriggerReport {
        no_trigger()
    }
    fn uninstall_footprint(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}
fn no_trigger() -> TriggerReport {
    TriggerReport {
        harness: HarnessId::ClaudeCode,
        currency_kind: CurrencyKind::ExplicitPullOnly,
        touched_path: None,
        marker_id: "test".into(),
        state: TriggerState::Inactive,
    }
}

// ---------------------------------------------------------------------------------------------
// The fake EnrollSource — canned bootstrap / authorize / poll / redeem (the redeem VERIFIES the op's
// enroll signature, proving the client signs the right frame: device_auth_id = user_code, the offered
// set, grant_hash binding).
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Poll {
    Pending,
    Denied,
    Granted,
}

#[derive(Clone)]
struct FakeEnroll {
    bootstrap: BootstrapData,
    user_code: String,
    device_code: String,
    grant: String,
    poll: Poll,
}

impl EnrollSource for FakeEnroll {
    fn fetch_bootstrap(&self, _token: &str) -> Result<BootstrapData, ClientError> {
        Ok(self.bootstrap.clone())
    }
    fn device_authorize(
        &self,
        _token: &str,
        _device_public_key: [u8; 32],
        _machine_name: &str,
    ) -> Result<DeviceAuthorize, ClientError> {
        Ok(DeviceAuthorize {
            device_code: self.device_code.clone(),
            user_code: self.user_code.clone(),
            verification_uri: format!("{BASE_URL}/device"),
            expires_in: 900,
            interval: 5,
        })
    }
    fn poll_token(&self, _device_code: &str) -> Result<TokenPoll, ClientError> {
        Ok(match self.poll {
            Poll::Pending => TokenPoll::Pending,
            Poll::Denied => TokenPoll::Denied,
            Poll::Granted => TokenPoll::Granted(Grant::new(self.grant.clone())),
        })
    }
    fn redeem(
        &self,
        workspace_id: &str,
        grant: &str,
        device_public_key: [u8; 32],
        enroll_sig: [u8; 64],
    ) -> Result<Redeem, ClientError> {
        // Reconstruct + verify the enroll possession proof exactly as the authority would.
        let dk = device_key_id_for(&device_public_key);
        let grant_hash = digest::sha256(grant.as_bytes());
        let offered: Vec<&str> = self
            .bootstrap
            .offered_skills
            .iter()
            .map(|s| s.skill_id.as_str())
            .collect();
        let fields = EnrollFields {
            workspace_id,
            grant_hash,
            device_auth_id: &self.user_code,
            device_key_id: &dk,
            device_public_key,
            offered_skill_ids: &offered,
        };
        assert!(
            verify_enroll(&fields, &enroll_sig, &device_public_key),
            "the client's enroll possession signature must verify on the framed fields"
        );
        Ok(Redeem {
            workspace_id: workspace_id.to_owned(),
            device_key_id: dk,
            read_creds: self
                .bootstrap
                .offered_skills
                .iter()
                .map(|s| RedeemedCred {
                    skill_id: s.skill_id.clone(),
                    read_token: format!("rt_secret_{}", s.skill_id),
                    expires_at: None,
                })
                .collect(),
        })
    }
}

fn bootstrap(offered: &[(&str, &str)]) -> BootstrapData {
    BootstrapData {
        schema_version: 1,
        invite: BootstrapInvite {
            token_id: "tok_1".into(),
            expires_at: None,
            consent: ConsentMode::DirectHumanFirstReceive,
            first_receive_auto_land: false,
        },
        plane: BootstrapPlane {
            base_url: BASE_URL.into(),
            deployment_mode: DeploymentMode::Cloud,
            enrollment_method: "device_code".into(),
            signing_key: BootstrapSigningKey {
                alg: SignatureAlg::Ed25519,
                key_id: "pk_acme".into(),
                value: plane_pubkey_b64(),
            },
        },
        workspace: BootstrapWorkspace {
            workspace_id: WS.into(),
            display_name: "Acme Inc".into(),
            verified_domain: Some("acme.com".into()),
            verified_domain_status: VerifiedDomainStatus::Verified,
        },
        offered_skills: offered
            .iter()
            .map(|(id, name)| BootstrapSkill {
                skill_id: (*id).into(),
                name: Some((*name).into()),
            })
            .collect(),
    }
}

fn fake(offered: &[(&str, &str)], poll: Poll) -> FakeEnroll {
    FakeEnroll {
        bootstrap: bootstrap(offered),
        user_code: "WXYZ-1234".into(),
        device_code: "dc_secret_abc".into(),
        grant: "grant_secret_xyz".into(),
        poll,
    }
}

// ---------------------------------------------------------------------------------------------
// The fixture plane (the offer disclosure + the first-receive pull source).
// ---------------------------------------------------------------------------------------------

#[derive(Default, Clone)]
struct FixturePlane {
    records: HashMap<String, SignedCurrentRecord>,
    versions: HashMap<(String, String), FetchedVersion>,
}
impl FixturePlane {
    fn serve_genesis(&mut self, skill: &str, files: &[(&str, FileMode, &[u8])]) {
        let v = mk_version(files);
        self.versions
            .insert((skill.to_owned(), to_hex(&v.id)), v.fetched);
        self.records
            .insert(skill.to_owned(), signed(skill, v.id, 1, 1));
    }
}
impl PlaneSource for FixturePlane {
    fn get_current(
        &self,
        skill_id: &str,
        known: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError> {
        let Some(rec) = self.records.get(skill_id) else {
            return Err(PlaneError::NotFound);
        };
        if let Some(k) = known
            && k.generation.epoch == rec.record.generation.epoch
            && k.generation.seq == rec.record.generation.seq
            && to_hex(&k.version_id) == rec.record.version_id
        {
            return Ok(PointerFetch::NotModified);
        }
        Ok(PointerFetch::Record(rec.clone()))
    }
    fn fetch_version(
        &self,
        skill_id: &str,
        version_id: [u8; 32],
    ) -> Result<FetchedVersion, PlaneError> {
        self.versions
            .get(&(skill_id.to_owned(), to_hex(&version_id)))
            .cloned()
            .ok_or(PlaneError::NotFound)
    }
}

struct FixtureFollow {
    entries: Vec<(String, FollowContext)>,
}
impl FollowSource for FixtureFollow {
    fn followed(&self) -> Vec<(String, FollowContext)> {
        self.entries.clone()
    }
    fn proposals_awaiting(&self) -> u32 {
        0
    }
}

struct Version {
    id: [u8; 32],
    fetched: FetchedVersion,
}
fn mk_version(files: &[(&str, FileMode, &[u8])]) -> Version {
    let entries: Vec<ManifestEntry> = files
        .iter()
        .map(|(p, m, b)| ManifestEntry {
            path: (*p).to_owned(),
            mode: *m,
            content_sha256: digest::sha256(b),
        })
        .collect();
    let tree = digest::bundle_digest(&entries).unwrap();
    let id = sign::commit_id(&Commit {
        parents: &[],
        tree,
        author: "d_pub",
        message: "genesis",
    })
    .unwrap();
    Version {
        id,
        fetched: FetchedVersion {
            parents: Vec::new(),
            author: "d_pub".into(),
            message: "genesis".into(),
            files: files
                .iter()
                .map(|(p, m, b)| FetchedFile {
                    path: (*p).to_owned(),
                    mode: *m,
                    bytes: b.to_vec(),
                })
                .collect(),
        },
    }
}
fn signed(skill: &str, version_id: [u8; 32], epoch: u64, seq: u64) -> SignedCurrentRecord {
    let pointer = CurrentPointer {
        workspace_id: WS,
        skill_id: skill,
        version_id,
        epoch,
        seq,
    };
    let msg = sign::pointer_preimage(&pointer).unwrap();
    let value = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(plane_key().sign(msg.as_bytes()).to_bytes());
    SignedCurrentRecord {
        schema_version: 1,
        scope: PointerScope {
            workspace_id: WS.into(),
            skill_id: skill.into(),
        },
        record: CurrentRecord {
            version_id: to_hex(&version_id),
            generation: Generation { epoch, seq },
        },
        signature: Signature {
            alg: SignatureAlg::Ed25519,
            key_id: "pk_acme".into(),
            value,
        },
    }
}

// ---------------------------------------------------------------------------------------------
// The rig.
// ---------------------------------------------------------------------------------------------

struct Rig {
    home: Scratch,
    work: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: TmpHarness,
}
impl Rig {
    fn new(tag: &str) -> Self {
        let work = Scratch::new(&format!("{tag}-work"));
        let harness = TmpHarness {
            skills_root: work.0.join("skills"),
        };
        Self {
            home: Scratch::new(&format!("{tag}-home")),
            work,
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1_700_000_000_000),
            harness,
        }
    }
    fn layout(&self) -> Layout {
        Layout::new(&self.home.0)
    }
    fn ctx<'a>(&'a self, plane: &'a dyn PlaneSource, follow: &'a dyn FollowSource) -> Ctx<'a> {
        Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: "d_test".into(),
            layout: self.layout(),
            harness: &self.harness,
            plane,
            plane_key: plane_pubkey(),
            follow,
        }
    }
    /// Mint host.json (app.rs does this in the `follow` device-id arm) so `set_device_key` can record into it.
    fn mint_identity(&self) {
        identity::load_or_create_device_id(&self.fs, &self.layout()).unwrap();
    }
    fn placement(&self, skill: &str) -> PathBuf {
        self.work.0.join("skills").join(skill)
    }
}

/// Build the connectors over a fake enroll source + a fixture plane.
fn run_follow(
    rig: &Rig,
    fake: &FakeEnroll,
    plane: &FixturePlane,
    link: Option<&str>,
    opts: ops::FollowOpts,
) -> Result<topos_types::results::FollowData, ClientError> {
    rig.mint_identity();
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let enroll_connect = |_b: &str| -> Box<dyn EnrollSource> { Box::new(fake.clone()) };
    let plane_connect = |_b: &str, _c: HashMap<String, SkillCred>| -> Box<dyn PlaneSource> {
        Box::new(plane.clone())
    };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        plane: &plane_connect,
    };
    ops::follow(&ctx, &connectors, link.map(str::to_owned), opts)
}

fn opts(manual: bool, resume: bool, approve: &[&str]) -> ops::FollowOpts {
    ops::FollowOpts {
        manual,
        resume,
        approve: approve.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn mode_of(p: &std::path::Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

const GENESIS_FILES: &[(&str, FileMode, &[u8])] = &[
    ("SKILL.md", FileMode::Regular, b"# deploy\n"),
    ("run.sh", FileMode::Executable, b"#!/bin/sh\necho deploy\n"),
];

// ---------------------------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------------------------

#[test]
fn follow_link_returns_pending_writes_a_0600_wal_and_discloses_provenance() {
    let rig = Rig::new("pending");
    let fk = fake(&[("s_deploy", "deploy")], Poll::Pending);
    let plane = FixturePlane::default();
    let link = format!("{BASE_URL}/i/tok_abc");

    let data = run_follow(&rig, &fk, &plane, Some(&link), opts(false, false, &[])).unwrap();

    // The pending arm: not enrolled, the verification URL + provenance disclosed.
    assert!(!data.enrolled);
    let pending = data.pending.expect("a pending enrollment");
    assert!(
        pending
            .verification_uri_complete
            .contains("user_code=WXYZ-1234")
    );
    assert_eq!(pending.user_code, "WXYZ-1234");
    assert_eq!(data.workspace_display_name.as_deref(), Some("Acme Inc"));
    assert_eq!(data.verified_domain.as_deref(), Some("acme.com"));
    assert_eq!(
        data.verified_domain_status,
        Some(VerifiedDomainStatus::Verified)
    );

    // The WAL is on disk at 0600.
    let wal_path = rig.layout().enrollment_path();
    assert_eq!(mode_of(&wal_path), 0o600, "the enrollment WAL must be 0600");
    // No enrollment is finalized yet.
    assert!(
        enroll::read_instance(&rig.fs, &rig.layout())
            .unwrap()
            .is_none()
    );
}

#[test]
fn resume_granted_promotes_writes_all_docs_records_the_key_and_clears_the_wal() {
    let rig = Rig::new("granted");
    let fk = fake(&[("s_deploy", "deploy")], Poll::Granted);
    let mut plane = FixturePlane::default();
    plane.serve_genesis("s_deploy", GENESIS_FILES);
    let link = format!("{BASE_URL}/i/tok_abc");

    // Call 1: begin (pending), then call 2: resume (granted) — two op invocations sharing the WAL.
    let _ = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Pending),
        &plane,
        Some(&link),
        opts(false, false, &[]),
    )
    .unwrap();
    let data = run_follow(&rig, &fk, &plane, None, opts(false, true, &[])).unwrap();

    assert!(data.enrolled);
    assert_eq!(data.workspace_id, WS);
    // The offer is disclosed (the read-only metadata fetch), with a real version + digest.
    assert_eq!(data.skills.len(), 1);
    assert_eq!(data.skills[0].skill_id, "s_deploy");
    assert_eq!(data.skills[0].name, "deploy");
    assert_eq!(data.skills[0].offer.version_id.len(), 64);
    assert_eq!(data.skills[0].offer.bundle_digest.len(), 64);

    let layout = rig.layout();
    // instance.json — TOFU-pinned to the bootstrap key, carrying the new disclosure fields.
    let instance = enroll::read_instance(&rig.fs, &layout)
        .unwrap()
        .expect("instance.json");
    assert_eq!(instance.base_url, BASE_URL);
    assert_eq!(instance.plane_key, to_hex(&plane_pubkey()));
    assert_eq!(instance.deployment_mode, DeploymentMode::Cloud);
    assert_eq!(instance.workspace_display_name.as_deref(), Some("Acme Inc"));
    assert_eq!(
        instance.verified_domain_status,
        VerifiedDomainStatus::Verified
    );
    // instance.json is PUBLIC (ordinary perms — the plane key is a public key).
    assert_eq!(mode_of(&layout.instance_path()), 0o644);

    // follows.json — 0600, the skill following=true.
    assert_eq!(
        mode_of(&layout.follows_path()),
        0o600,
        "follows.json holds secret read tokens"
    );
    let follows = enroll::read_follows(&rig.fs, &layout)
        .unwrap()
        .expect("follows.json");
    assert_eq!(follows.follows.len(), 1);
    assert_eq!(follows.follows[0].skill_id, "s_deploy");
    assert!(follows.follows[0].following);
    assert_eq!(follows.follows[0].read_token, "rt_secret_s_deploy");
    // The load_enrollment Some-condition holds (instance present AND ≥1 following skill).
    assert!(follows.follows.iter().any(|f| f.following));

    // user.json — metadata only, ordinary perms, NO secret.
    assert_eq!(mode_of(&layout.user_path()), 0o644);
    let user: enroll::UserDoc = doc::read_doc(&rig.fs, &layout.user_path())
        .unwrap()
        .unwrap();
    assert_eq!(user.workspace_id, WS);
    assert!(user.invite_rooted);
    assert!(user.email.is_none());

    // host.json records the device key (the PUBLIC ref); the id matches the server derivation.
    let host_bytes = std::fs::read(layout.host_path()).unwrap();
    let host_text = String::from_utf8(host_bytes).unwrap();
    assert!(host_text.contains("\"device_key\""));
    assert!(host_text.contains("\"private_key_ref\""));
    assert!(host_text.contains("device.key"));

    // The WAL is gone.
    assert!(enroll::read_wal(&rig.fs, &layout).unwrap().is_none());
}

#[test]
fn a_second_follow_to_another_skill_merges_and_preserves_the_first() {
    let rig = Rig::new("merge");
    let mut plane = FixturePlane::default();
    plane.serve_genesis("s_deploy", GENESIS_FILES);
    plane.serve_genesis("s_review", GENESIS_FILES);
    let link = format!("{BASE_URL}/i/tok");

    // First enrollment: s_deploy.
    let _ = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Pending),
        &plane,
        Some(&link),
        opts(false, false, &[]),
    )
    .unwrap();
    let _ = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Granted),
        &plane,
        None,
        opts(false, true, &[]),
    )
    .unwrap();

    // Second enrollment to the SAME plane (bare token), offering s_review.
    let _ = run_follow(
        &rig,
        &fake(&[("s_review", "review")], Poll::Pending),
        &plane,
        Some("tok2"),
        opts(false, false, &[]),
    )
    .unwrap();
    let _ = run_follow(
        &rig,
        &fake(&[("s_review", "review")], Poll::Granted),
        &plane,
        None,
        opts(false, true, &[]),
    )
    .unwrap();

    // follows.json now carries BOTH — the first was not clobbered.
    let follows = enroll::read_follows(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    let ids: Vec<&str> = follows
        .follows
        .iter()
        .map(|f| f.skill_id.as_str())
        .collect();
    assert!(ids.contains(&"s_deploy"), "first follow preserved: {ids:?}");
    assert!(ids.contains(&"s_review"), "second follow merged: {ids:?}");
    assert_eq!(follows.follows.len(), 2);
}

#[test]
fn a_redeemed_wal_resume_promotes_without_re_redeeming() {
    let rig = Rig::new("recover");
    let mut plane = FixturePlane::default();
    plane.serve_genesis("s_deploy", GENESIS_FILES);

    // Reach the granted state once, then HAND-WRITE a Redeemed WAL (as the lockout fence records it) and
    // re-resume: it must promote from the persisted creds WITHOUT calling redeem again. A fake whose
    // redeem PANICS proves no re-redeem happens.
    rig.mint_identity();
    let context = enroll::EnrollContext {
        base_url: BASE_URL.into(),
        pinned_plane_key: to_hex(&plane_pubkey()),
        plane_key_id: "pk_acme".into(),
        deployment_mode: DeploymentMode::Cloud,
        enrollment_method: "device_code".into(),
        workspace_id: WS.into(),
        workspace_display_name: "Acme Inc".into(),
        verified_domain: Some("acme.com".into()),
        verified_domain_status: VerifiedDomainStatus::Verified,
        offered_skills: vec![enroll::OfferedSkill {
            skill_id: "s_deploy".into(),
            name: Some("deploy".into()),
        }],
        mode: enroll::FollowModeDoc::Auto,
    };
    let wal = enroll::PendingEnrollment {
        schema_version: 1,
        state: enroll::EnrollPhase::Redeemed {
            context,
            read_creds: vec![enroll::RedeemedCredDoc {
                skill_id: "s_deploy".into(),
                read_token: "rt_secret_s_deploy".into(),
                expires_at: None,
            }],
            device_key_id: device_key_id_for(&device_pubkey(&rig)),
            enrolled_at_millis: 1,
        },
    };
    enroll::write_wal(&rig.fs, &rig.layout(), &wal).unwrap();

    // A fake whose poll/redeem would panic — proving the Redeemed-WAL path never calls them.
    let panicky = PanicEnroll;
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let enroll_connect = |_b: &str| -> Box<dyn EnrollSource> { Box::new(panicky) };
    let plane_connect = |_b: &str, _c: HashMap<String, SkillCred>| -> Box<dyn PlaneSource> {
        Box::new(plane.clone())
    };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        plane: &plane_connect,
    };
    let data = ops::follow(&ctx, &connectors, None, opts(false, true, &[])).unwrap();

    assert!(data.enrolled);
    let follows = enroll::read_follows(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert_eq!(follows.follows[0].skill_id, "s_deploy");
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
}

/// The device public key the rig's signer would mint — load_or_generate is idempotent, so this reads the
/// already-persisted seed (minted by `mint_identity`'s sibling device key write on first follow).
fn device_pubkey(rig: &Rig) -> [u8; 32] {
    crate::device_signer::DeviceSigner::load_or_generate(&rig.fs, &rig.layout())
        .unwrap()
        .public_key()
}

#[derive(Clone, Copy)]
struct PanicEnroll;
impl EnrollSource for PanicEnroll {
    fn fetch_bootstrap(&self, _t: &str) -> Result<BootstrapData, ClientError> {
        panic!("a Redeemed-WAL resume must not re-contact the plane")
    }
    fn device_authorize(
        &self,
        _t: &str,
        _k: [u8; 32],
        _m: &str,
    ) -> Result<DeviceAuthorize, ClientError> {
        panic!("a Redeemed-WAL resume must not re-authorize")
    }
    fn poll_token(&self, _d: &str) -> Result<TokenPoll, ClientError> {
        panic!("a Redeemed-WAL resume must not re-poll")
    }
    fn redeem(
        &self,
        _w: &str,
        _g: &str,
        _k: [u8; 32],
        _s: [u8; 64],
    ) -> Result<Redeem, ClientError> {
        panic!("a Redeemed-WAL resume must NOT re-redeem the single-use grant")
    }
}

#[test]
fn a_denied_poll_is_a_typed_error_and_sweeps_the_wal() {
    let rig = Rig::new("denied");
    let plane = FixturePlane::default();
    let link = format!("{BASE_URL}/i/tok");
    let _ = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Pending),
        &plane,
        Some(&link),
        opts(false, false, &[]),
    )
    .unwrap();

    let err = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Denied),
        &plane,
        None,
        opts(false, true, &[]),
    )
    .unwrap_err();
    assert!(matches!(err, ClientError::Enrollment(_)), "got {err:?}");
    // The dead session's WAL is swept.
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
}

#[test]
fn a_cross_base_url_follow_is_refused() {
    let rig = Rig::new("crossplane");
    let mut plane = FixturePlane::default();
    plane.serve_genesis("s_deploy", GENESIS_FILES);
    // Enroll with the first plane.
    let _ = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Pending),
        &plane,
        Some(&format!("{BASE_URL}/i/t")),
        opts(false, false, &[]),
    )
    .unwrap();
    let _ = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Granted),
        &plane,
        None,
        opts(false, true, &[]),
    )
    .unwrap();

    // A follow against a DIFFERENT base URL is refused (v0 is one plane per install).
    let other = run_follow(
        &rig,
        &fake(&[("s_other", "other")], Poll::Pending),
        &plane,
        Some("https://evil.test/i/t2"),
        opts(false, false, &[]),
    )
    .unwrap_err();
    assert!(
        matches!(other, ClientError::PlacementUnsupported { .. }),
        "got {other:?}"
    );
}

#[test]
fn same_url_different_key_is_key_repin_required() {
    let rig = Rig::new("repin");
    let mut plane = FixturePlane::default();
    plane.serve_genesis("s_deploy", GENESIS_FILES);
    let _ = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Pending),
        &plane,
        Some(&format!("{BASE_URL}/i/t")),
        opts(false, false, &[]),
    )
    .unwrap();
    let _ = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Granted),
        &plane,
        None,
        opts(false, true, &[]),
    )
    .unwrap();

    // The SAME base URL but a DIFFERENT signing key → a typed re-pin error (never a silent trust).
    let mut bad = fake(&[("s_deploy", "deploy")], Poll::Pending);
    bad.bootstrap.plane.signing_key.value =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0x55u8; 32]);
    let err = run_follow(
        &rig,
        &bad,
        &plane,
        Some(&format!("{BASE_URL}/i/t2")),
        opts(false, false, &[]),
    )
    .unwrap_err();
    assert!(matches!(err, ClientError::KeyRepinRequired), "got {err:?}");
}

#[test]
fn a_non_ed25519_alg_fails_closed_at_the_bootstrap_boundary() {
    // The bootstrap's `alg` is the CLOSED `SignatureAlg` enum — a non-Ed25519 value never deserializes
    // (so a downgrade/confusion attack on the trust root is refused at the edge).
    let json = serde_json::json!({
        "schema_version": 1,
        "invite": { "token_id": "t", "consent": "direct_human_first_receive", "first_receive_auto_land": false },
        "plane": {
            "base_url": BASE_URL,
            "deployment_mode": "cloud",
            "enrollment_method": "device_code",
            "signing_key": { "alg": "RSA", "key_id": "k", "value": "AAAA" }
        },
        "workspace": { "workspace_id": WS, "display_name": "Acme", "verified_domain_status": "verified" }
    });
    assert!(
        serde_json::from_value::<BootstrapData>(json).is_err(),
        "a non-Ed25519 alg must fail to deserialize"
    );
}

#[test]
fn the_read_token_never_leaks_in_debug() {
    // The secret read tokens are redacted everywhere they could surface (follows.json's entry, the
    // transport credential, the WAL's redeemed creds).
    let entry = enroll::FollowEntry {
        skill_id: "s".into(),
        workspace_id: WS.into(),
        read_token: "rt_super_secret".into(),
        mode: enroll::FollowModeDoc::Auto,
        review_required: false,
        following: true,
    };
    assert!(!format!("{entry:?}").contains("rt_super_secret"));
    let cred = SkillCred::new(WS.into(), "rt_super_secret".into());
    assert!(!format!("{cred:?}").contains("rt_super_secret"));
    let wal = enroll::PendingEnrollment {
        schema_version: 1,
        state: enroll::EnrollPhase::Redeemed {
            context: tiny_context(),
            read_creds: vec![enroll::RedeemedCredDoc {
                skill_id: "s".into(),
                read_token: "rt_super_secret".into(),
                expires_at: None,
            }],
            device_key_id: "dk_x".into(),
            enrolled_at_millis: 1,
        },
    };
    let dbg = format!("{wal:?}");
    assert!(dbg.contains("<redacted>"));
    assert!(
        !dbg.contains("rt_super_secret"),
        "WAL Debug leaked a read token"
    );
    // The grant + device-code domain types redact too.
    assert!(!format!("{:?}", Grant::new("grant_x".into())).contains("grant_x"));
}

fn tiny_context() -> enroll::EnrollContext {
    enroll::EnrollContext {
        base_url: BASE_URL.into(),
        pinned_plane_key: to_hex(&plane_pubkey()),
        plane_key_id: "pk".into(),
        deployment_mode: DeploymentMode::Cloud,
        enrollment_method: "device_code".into(),
        workspace_id: WS.into(),
        workspace_display_name: "Acme".into(),
        verified_domain: None,
        verified_domain_status: VerifiedDomainStatus::Unverified,
        offered_skills: Vec::new(),
        mode: enroll::FollowModeDoc::Auto,
    }
}

#[test]
fn the_first_receive_baseline_is_laid_then_a_fixture_plane_pull_offers_then_places() {
    let rig = Rig::new("firstrecv");
    let mut plane = FixturePlane::default();
    plane.serve_genesis("s_deploy", GENESIS_FILES);

    // Enroll + follow s_deploy.
    let _ = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Pending),
        &plane,
        Some(&format!("{BASE_URL}/i/t")),
        opts(false, false, &[]),
    )
    .unwrap();
    let _ = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Granted),
        &plane,
        None,
        opts(false, true, &[]),
    )
    .unwrap();

    // The first-receive baseline is laid: a NEVER-RECEIVED sync (empty recorded, genesis floor), a store,
    // and a map carrying the harness placement target.
    let layout = rig.layout();
    // Find the laid skill dir (the baseline is keyed by the plane's skill id).
    let sp = layout.published("s_deploy");
    let sync: SyncState = doc::read_doc(&rig.fs, &sp.sync)
        .unwrap()
        .expect("baseline sync.json");
    assert!(sync.recorded.is_empty(), "never-received: empty recorded");
    assert_eq!(sync.observed, Generation { epoch: 0, seq: 0 });
    assert_eq!(sync.applied, Generation { epoch: 0, seq: 0 });
    assert!(
        sp.store.join("HEAD").exists() || sp.store.exists(),
        "an embedded store is initialized"
    );

    // Now a fixture-plane pull. A BARE SWEEP offers (never auto-lands a first receive — I-TOFU), even Auto.
    let follow = FixtureFollow {
        entries: vec![(
            "s_deploy".to_owned(),
            FollowContext {
                workspace_id: WS.into(),
                mode: FollowMode::Auto,
                review_required: false,
                following: true,
            },
        )],
    };
    let ctx = rig.ctx(&plane, &follow);
    let swept = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(swept.skills.len(), 1);
    assert_eq!(
        swept.skills[0].action,
        PullAction::Offered,
        "bare sweep OFFERS a first receive"
    );
    assert!(
        !rig.placement("s_deploy").exists(),
        "nothing placed by the offer"
    );

    // A SECOND bare sweep (the I-TOFU regression guard): the first sweep raised the floor + recorded the
    // tuple, but the still-unapproved first-receive baseline is STILL offered, NEVER auto-landed — even for
    // the default Auto follower.
    let swept2 = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(
        swept2.skills[0].action,
        PullAction::Offered,
        "a SECOND auto sweep still offers — the first version is never auto-landed"
    );
    assert!(
        !rig.placement("s_deploy").exists(),
        "still nothing placed after two consecutive auto sweeps"
    );

    // An EXPLICIT accept places the first bytes.
    let accepted = ops::pull(
        &ctx,
        ops::PullScope::One {
            name: "deploy".to_owned(),
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap();
    assert_eq!(
        accepted.skills[0].action,
        PullAction::FastForwarded,
        "explicit accept PLACES"
    );

    // The placement now holds the genesis bytes, exec bit preserved.
    let placement = rig.placement("s_deploy");
    assert_eq!(
        std::fs::read(placement.join("SKILL.md")).unwrap(),
        b"# deploy\n"
    );
    assert_eq!(
        mode_of(&placement.join("run.sh")) & 0o111,
        0o111,
        "exec bit preserved"
    );
}

#[test]
fn approve_places_the_named_first_receive_offer() {
    let rig = Rig::new("approve");
    let mut plane = FixturePlane::default();
    plane.serve_genesis("s_deploy", GENESIS_FILES);
    let _ = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Pending),
        &plane,
        Some(&format!("{BASE_URL}/i/t")),
        opts(false, false, &[]),
    )
    .unwrap();
    let _ = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Granted),
        &plane,
        None,
        opts(false, true, &[]),
    )
    .unwrap();

    // `follow --approve deploy@<digest>` drives the engine through ctx.plane (a separate, post-enroll
    // invocation where the plane is live).
    let inert_f = InertFollow;
    let ctx = rig.ctx(&plane, &inert_f);
    let enroll_connect = |_b: &str| -> Box<dyn EnrollSource> { Box::new(PanicEnroll) };
    let plane_connect = |_b: &str, _c: HashMap<String, SkillCred>| -> Box<dyn PlaneSource> {
        Box::new(plane.clone())
    };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        plane: &plane_connect,
    };
    let data = ops::follow(&ctx, &connectors, None, opts(false, false, &["deploy"])).unwrap();

    assert!(data.enrolled);
    assert_eq!(data.skills.len(), 1);
    // The named bytes were placed.
    let placement = rig.placement("s_deploy");
    assert_eq!(
        std::fs::read(placement.join("SKILL.md")).unwrap(),
        b"# deploy\n"
    );
}
