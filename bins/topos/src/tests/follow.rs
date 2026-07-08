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
use crate::fs_seam::{FaultFs, FsOps, RealFs};
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    DeviceAuthorize, EnrollSource, FetchedFile, FetchedVersion, FollowContext, FollowMode,
    FollowSource, Grant, GrantedToken, InertFollow, InertPlane, KnownCurrent, PlaneError,
    PlaneSource, PointerFetch, Redeem, RedeemedCred, StandupAuthorize, TokenPoll,
};
use crate::plane_http::SkillCred;
use crate::sidecar::Layout;
use crate::{doc, enroll, identity, ops};

const WS: &str = "w_acme";
const BASE_URL: &str = "https://acme.topos.test";
const PLANE_SEED: [u8; 32] = [9u8; 32];

/// Parse a fixture skill id through the validated newtype (always charset-clean here).
fn sid(id: &str) -> crate::id::SkillId {
    crate::id::SkillId::parse(id).expect("fixture skill id is charset-clean")
}

/// The test shim over [`ops::pull`]: project the schema payload (warnings have dedicated tests).
fn pull_data(
    ctx: &Ctx<'_>,
    scope: ops::PullScope,
) -> Result<topos_types::results::PullData, ClientError> {
    ops::pull(ctx, scope).map(|o| o.data)
}

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
    /// Counts `install_currency_trigger` calls — the promote path must arm the follower's hook.
    installs: std::sync::atomic::AtomicU32,
}
impl HarnessAdapter for TmpHarness {
    fn id(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }
    fn discover(&self) -> Vec<DiscoveredPlacement> {
        Vec::new()
    }
    fn placement_for(
        &self,
        skill_id: &str,
        _n: topos_harness::PlacementNaming<'_>,
        _d: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget {
        PlacementTarget {
            dir: self.skills_root.join(skill_id),
        }
    }
    fn currency_kind(&self) -> CurrencyKind {
        CurrencyKind::ExplicitPullOnly
    }
    fn install_currency_trigger(&self) -> TriggerReport {
        self.installs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
            verification_uri: format!("{BASE_URL}/verify"),
            verification_uri_complete: Some(format!("{BASE_URL}/verify/{}", self.user_code)),
            expires_in: 900,
            interval: 5,
        })
    }
    fn device_authorize_standup(
        &self,
        _device_public_key: [u8; 32],
        _machine_name: &str,
    ) -> Result<StandupAuthorize, ClientError> {
        panic!("the invite follow flow never starts a standup authorization")
    }
    fn poll_token(&self, _device_code: &str) -> Result<TokenPoll, ClientError> {
        Ok(match self.poll {
            Poll::Pending => TokenPoll::Pending,
            Poll::Denied => TokenPoll::Denied,
            Poll::Granted => TokenPoll::Granted(GrantedToken {
                grant: Grant::new(self.grant.clone()),
                workspace: None,
            }),
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
            principal: Some("alice@acme.com".to_owned()),
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
    fn admin_claim(
        &self,
        _claim_token: &str,
        _device_public_key: [u8; 32],
        _display_name: &str,
    ) -> Result<Redeem, ClientError> {
        panic!("the invite follow flow never redeems an admin claim")
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
            installs: std::sync::atomic::AtomicU32::new(0),
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
        self.ctx_fs(&self.fs, plane, follow)
    }
    /// A [`Ctx`] over an arbitrary [`FsOps`] (the crash gate injects a [`FaultFs`] to fault the Nth
    /// durable op of a promotion).
    fn ctx_fs<'a>(
        &'a self,
        fs: &'a dyn FsOps,
        plane: &'a dyn PlaneSource,
        follow: &'a dyn FollowSource,
    ) -> Ctx<'a> {
        Ctx {
            fs,
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
    ops::follow(&ctx, &connectors, link.map(str::to_owned), opts).map(|o| o.data)
}

fn opts(manual: bool, resume: bool, approve: &[&str]) -> ops::FollowOpts {
    ops::FollowOpts {
        manual,
        resume,
        approve: approve.iter().map(|s| (*s).to_owned()).collect(),
        workspace: None,
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
    // The SERVER-built complete URI is surfaced verbatim (the client-side reconstruction is only the
    // fallback for an older plane that omits the field).
    assert_eq!(
        pending.verification_uri_complete,
        format!("{BASE_URL}/verify/WXYZ-1234")
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
    // The promote armed the follower's session-start currency hook (a pure follower never runs `add`,
    // so this is their only arm point) and disclosed the outcome on the result.
    assert_eq!(
        rig.harness
            .installs
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert!(data.currency.is_some());
    // The offer is disclosed (the read-only metadata fetch), with a real version + digest.
    assert_eq!(data.skills.len(), 1);
    assert_eq!(data.skills[0].skill_id, "s_deploy");
    assert_eq!(data.skills[0].name, "deploy");
    assert_eq!(data.skills[0].offer.version_id.len(), 64);
    assert_eq!(data.skills[0].offer.bundle_digest.len(), 64);

    let layout = rig.layout();
    // instance.json — TOFU-pinned to the bootstrap key; the PLANE record only now (the per-workspace
    // disclosure moved to the user.json membership asserted below).
    let instance = enroll::read_instance(&rig.fs, &layout)
        .unwrap()
        .expect("instance.json");
    assert_eq!(instance.base_url, BASE_URL);
    assert_eq!(instance.plane_key, to_hex(&plane_pubkey()));
    assert_eq!(instance.deployment_mode, DeploymentMode::Cloud);
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
    // The workspace disclosure now lives on the per-workspace membership.
    let m = user.membership(WS).expect("the workspace membership");
    assert_eq!(m.display_name.as_deref(), Some("Acme Inc"));
    assert_eq!(m.verified_domain_status, VerifiedDomainStatus::Verified);
    assert!(m.invite_rooted);
    // The redeem now discloses the seated principal; an email-shaped one also fills `email`.
    assert_eq!(user.principal.as_deref(), Some("alice@acme.com"));
    assert_eq!(user.email.as_deref(), Some("alice@acme.com"));

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
        root: enroll::EnrollRoot::Invite,
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
            principal: None,
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
    let data = ops::follow(&ctx, &connectors, None, opts(false, true, &[]))
        .unwrap()
        .data;

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
    fn device_authorize_standup(
        &self,
        _k: [u8; 32],
        _m: &str,
    ) -> Result<StandupAuthorize, ClientError> {
        panic!("a Redeemed-WAL resume must not start a standup")
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
    fn admin_claim(&self, _c: &str, _k: [u8; 32], _d: &str) -> Result<Redeem, ClientError> {
        panic!("a Redeemed-WAL resume must not redeem an admin claim")
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

    // A follow whose bootstrap DECLARES a different plane API base is refused (v0 is one plane per
    // install). The link's own host is NOT the discriminator — a share link may legitimately ride
    // another host — the plane the bootstrap re-roots onto is.
    let mut other_plane = fake(&[("s_other", "other")], Poll::Pending);
    other_plane.bootstrap.plane.base_url = "https://other.plane.test".into();
    let other = run_follow(
        &rig,
        &other_plane,
        &plane,
        Some("https://evil.test/i/t2"),
        opts(false, false, &[]),
    )
    .unwrap_err();
    assert!(
        matches!(other, ClientError::PlacementUnsupported { .. }),
        "got {other:?}"
    );

    // The mirror case: a link riding ANOTHER host whose bootstrap declares the ALREADY-PINNED plane
    // matches the pin — the enrolled install accepts its own team's share links from any host.
    let same_plane = run_follow(
        &rig,
        &fake(&[("s_other", "other")], Poll::Pending),
        &plane,
        Some("https://links.example/i/t3"),
        opts(false, false, &[]),
    )
    .expect("a share-host link onto the pinned plane is accepted");
    assert_eq!(same_plane.plane_base_url.as_deref(), Some(BASE_URL));
}

/// The re-root itself: a link on a share host (a hosted team's web origin) fetches the bootstrap there,
/// then EVERYTHING else — the device flow, the pin, the persisted `instance.json` — rides the API base
/// the bootstrap declared. The share host never appears in the sidecar.
#[test]
fn a_share_host_link_re_roots_onto_the_declared_api_base() {
    use std::cell::RefCell;

    let rig = Rig::new("reroot");
    let mut plane = FixturePlane::default();
    plane.serve_genesis("s_deploy", GENESIS_FILES);
    let f = fake(&[("s_deploy", "deploy")], Poll::Pending);

    // A recording connector: which bases the op built enrollment transports for, in order.
    let bases = RefCell::new(Vec::<String>::new());
    rig.mint_identity();
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let enroll_connect = |b: &str| -> Box<dyn EnrollSource> {
        bases.borrow_mut().push(b.to_owned());
        Box::new(f.clone())
    };
    let plane_connect = |_b: &str, _c: HashMap<String, SkillCred>| -> Box<dyn PlaneSource> {
        Box::new(plane.clone())
    };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        plane: &plane_connect,
    };
    let data = ops::follow(
        &ctx,
        &connectors,
        Some("https://links.example/i/tok_abc".to_owned()),
        opts(false, false, &[]),
    )
    .expect("begin re-roots")
    .data;

    // The receipt disclosed the plane the device actually enrolls against (not the share host).
    assert_eq!(data.plane_base_url.as_deref(), Some(BASE_URL));
    // The bootstrap GET rode the link base; the device authorization rode the re-rooted API base.
    assert_eq!(
        bases.borrow().as_slice(),
        ["https://links.example", BASE_URL]
    );

    // Complete the enrollment; the pinned instance records the API base — the share host is nowhere.
    let granted = fake(&[("s_deploy", "deploy")], Poll::Granted);
    let done = run_follow(&rig, &granted, &plane, None, opts(false, true, &[])).unwrap();
    assert!(done.enrolled);
    assert_eq!(done.plane_base_url.as_deref(), Some(BASE_URL));
    let instance = enroll::read_instance(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert_eq!(instance.base_url, BASE_URL);
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
            principal: None,
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
        root: enroll::EnrollRoot::Invite,
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
    let sp = layout.published(&sid("s_deploy"));
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
    let swept = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
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
    let swept2 = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
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
    let accepted = pull_data(
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
    let out = ops::follow(&ctx, &connectors, None, opts(false, false, &["deploy"])).unwrap();

    assert!(out.data.enrolled);
    assert_eq!(out.data.skills.len(), 1);
    assert!(
        out.resumed.is_empty(),
        "an active follow's approve is not a resume"
    );
    // The named bytes were placed.
    let placement = rig.placement("s_deploy");
    assert_eq!(
        std::fs::read(placement.join("SKILL.md")).unwrap(),
        b"# deploy\n"
    );
}

// ---------------------------------------------------------------------------------------------
// The begin-guard interleave: a second `follow <link>` must never CLOBBER an in-progress enrollment WAL
// (following workspace B while A is still mid-enrollment). A live/redeemed session refuses; a dead
// (expired) one is superseded. The data-loss case is a Redeemed WAL — its single-use read creds live ONLY
// in that WAL.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_second_follow_while_the_first_is_still_pending_refuses_and_keeps_the_wal() {
    let rig = Rig::new("interleave-live");
    let plane = FixturePlane::default();

    // Begin A: writes a LIVE Authorizing WAL (expires_at = now + 900s, so >= the rig's fixed clock).
    let _ = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Pending),
        &plane,
        Some(&format!("{BASE_URL}/i/tok_a")),
        opts(false, false, &[]),
    )
    .unwrap();
    let wal_before = enroll::read_wal(&rig.fs, &rig.layout())
        .unwrap()
        .expect("A left a live Authorizing WAL");
    assert!(matches!(
        &wal_before.state,
        enroll::EnrollPhase::Authorizing { .. }
    ));

    // Begin B (a DIFFERENT link) while A is still pending → refused, and A's WAL is byte-for-byte intact
    // (B's fake is never even consulted — the guard fires before the bootstrap fetch).
    let err = run_follow(
        &rig,
        &fake(&[("s_review", "review")], Poll::Pending),
        &plane,
        Some(&format!("{BASE_URL}/i/tok_b")),
        opts(false, false, &[]),
    )
    .unwrap_err();
    assert!(matches!(err, ClientError::Enrollment(_)), "got {err:?}");
    let wal_after = enroll::read_wal(&rig.fs, &rig.layout()).unwrap().unwrap();
    assert_eq!(
        wal_before, wal_after,
        "the live enrollment WAL must NOT be clobbered by a second follow"
    );
}

#[test]
fn a_follow_while_a_redeemed_wal_is_unpromoted_refuses_and_keeps_the_creds() {
    // The data-loss case: a Redeemed-but-unpromoted grant is single-use (spent server-side) and its minted
    // read creds live ONLY in this WAL. A second `follow <link>` must REFUSE, never overwrite it.
    let rig = Rig::new("interleave-redeemed");
    let plane = FixturePlane::default();
    rig.mint_identity();

    let wal = enroll::PendingEnrollment {
        schema_version: 1,
        state: enroll::EnrollPhase::Redeemed {
            context: tiny_context(),
            read_creds: vec![enroll::RedeemedCredDoc {
                skill_id: "s_deploy".into(),
                read_token: "rt_secret_s_deploy".into(),
                expires_at: None,
            }],
            device_key_id: "dk_abc".into(),
            principal: None,
            enrolled_at_millis: 1,
        },
    };
    enroll::write_wal(&rig.fs, &rig.layout(), &wal).unwrap();

    let err = run_follow(
        &rig,
        &fake(&[("s_review", "review")], Poll::Pending),
        &plane,
        Some(&format!("{BASE_URL}/i/tok_b")),
        opts(false, false, &[]),
    )
    .unwrap_err();
    assert!(matches!(err, ClientError::Enrollment(_)), "got {err:?}");
    let after = enroll::read_wal(&rig.fs, &rig.layout()).unwrap().unwrap();
    assert_eq!(
        after, wal,
        "the redeemed WAL (the only copy of its single-use creds) must survive a second follow"
    );
}

#[test]
fn a_follow_supersedes_an_expired_authorizing_wal() {
    // An EXPIRED authorizing session is dead (the human never approved in time). A fresh `follow <link>`
    // must SUPERSEDE it (the recovery path) — begin falls through the guard and writes its own live WAL.
    let rig = Rig::new("interleave-expired");
    let plane = FixturePlane::default();
    rig.mint_identity();

    let expired = enroll::PendingEnrollment {
        schema_version: 1,
        state: enroll::EnrollPhase::Authorizing {
            context: tiny_context(),
            device_code: "dc_old".into(),
            user_code: "OLD-CODE".into(),
            verification_uri_complete: None,
            interval: 5,
            // Far in the past vs the rig's FixedClock(1_700_000_000_000) → expired.
            expires_at_millis: 1_000,
        },
    };
    enroll::write_wal(&rig.fs, &rig.layout(), &expired).unwrap();

    let data = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Pending),
        &plane,
        Some(&format!("{BASE_URL}/i/tok_fresh")),
        opts(false, false, &[]),
    )
    .expect("a fresh follow supersedes the expired WAL");
    assert!(!data.enrolled);
    assert!(
        data.pending.is_some(),
        "the superseding follow is itself pending"
    );

    // The WAL is now the FRESH session (the fake's user_code + a live expiry), not the dead one.
    let wal = enroll::read_wal(&rig.fs, &rig.layout()).unwrap().unwrap();
    let enroll::EnrollPhase::Authorizing {
        user_code,
        expires_at_millis,
        ..
    } = wal.state
    else {
        panic!("expected a fresh Authorizing WAL after the supersede");
    };
    assert_eq!(
        user_code, "WXYZ-1234",
        "the fresh session's user code replaced the dead one"
    );
    assert!(
        expires_at_millis > 1_000,
        "the fresh session carries a live expiry, not the dead one"
    );
}

// ---------------------------------------------------------------------------------------------
// Multi-workspace promotion crash gate.
//
// A SECOND same-plane enrollment (workspace B: a DIFFERENT `workspace_id` on the SAME `base_url` + SAME
// pinned plane key) must promote crash-coherently — atomic-per-file + idempotent replay from the
// `Redeemed` WAL — and never drop the first workspace (A). These exercise `promote_core` from the
// Redeemed-WAL recovery arm (no re-poll / re-redeem — `PanicEnroll` proves it).
// ---------------------------------------------------------------------------------------------

/// Workspace B: a second workspace on the same plane, offering its own skill.
const WS_B: &str = "w_beta";
const B_SKILL: &str = "s_report";

/// Fully enroll workspace A (skill `s_deploy`) through the real two-call device flow, so instance.json is
/// TOFU-pinned, follows.json + user.json carry A, host.json holds the device key, and A's first-receive
/// baseline is laid. Leaves no WAL (A's own promote deleted it).
fn enroll_workspace_a(rig: &Rig, plane: &FixturePlane) {
    let link = format!("{BASE_URL}/i/tok_a");
    run_follow(
        rig,
        &fake(&[("s_deploy", "deploy")], Poll::Pending),
        plane,
        Some(&link),
        opts(false, false, &[]),
    )
    .expect("A: begin");
    run_follow(
        rig,
        &fake(&[("s_deploy", "deploy")], Poll::Granted),
        plane,
        None,
        opts(false, true, &[]),
    )
    .expect("A: resume promotes");
}

/// Hand-write workspace B's `Redeemed` WAL exactly as the lockout fence records it BEFORE promotion — the
/// same install/device (so the device key id + principal match), a DIFFERENT `workspace_id` on the SAME
/// base URL + pinned key. A re-`follow --resume` promotes from this without re-redeeming.
fn write_workspace_b_redeemed_wal(rig: &Rig) {
    let context = enroll::EnrollContext {
        base_url: BASE_URL.into(),
        pinned_plane_key: to_hex(&plane_pubkey()),
        plane_key_id: "pk_acme".into(),
        deployment_mode: DeploymentMode::Cloud,
        enrollment_method: "device_code".into(),
        workspace_id: WS_B.into(),
        workspace_display_name: "Beta Team".into(),
        verified_domain: None,
        verified_domain_status: VerifiedDomainStatus::Unverified,
        offered_skills: vec![enroll::OfferedSkill {
            skill_id: B_SKILL.into(),
            name: Some("report".into()),
        }],
        mode: enroll::FollowModeDoc::Auto,
        root: enroll::EnrollRoot::Invite,
    };
    let wal = enroll::PendingEnrollment {
        schema_version: 1,
        state: enroll::EnrollPhase::Redeemed {
            context,
            read_creds: vec![enroll::RedeemedCredDoc {
                skill_id: B_SKILL.into(),
                read_token: format!("rt_secret_{B_SKILL}"),
                expires_at: None,
            }],
            device_key_id: device_key_id_for(&device_pubkey(rig)),
            principal: Some("alice@acme.com".into()),
            enrolled_at_millis: 2,
        },
    };
    enroll::write_wal(&rig.fs, &rig.layout(), &wal).unwrap();
}

/// Drive `follow --resume` over an arbitrary fs (the crash gate injects a [`FaultFs`]). A Redeemed-WAL
/// resume never re-contacts the plane for enrollment (`PanicEnroll` proves it); the plane connector serves
/// only the post-promote, best-effort offer disclosure.
fn resume_over_fs(
    rig: &Rig,
    fs: &dyn FsOps,
    plane: &FixturePlane,
) -> Result<topos_types::results::FollowData, ClientError> {
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx_fs(fs, &inert_p, &inert_f);
    let enroll_connect = |_b: &str| -> Box<dyn EnrollSource> { Box::new(PanicEnroll) };
    let plane_connect = |_b: &str, _c: HashMap<String, SkillCred>| -> Box<dyn PlaneSource> {
        Box::new(plane.clone())
    };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        plane: &plane_connect,
    };
    ops::follow(&ctx, &connectors, None, opts(false, true, &[])).map(|o| o.data)
}

/// Assert the fully converged state: BOTH memberships (each with the right id + display name), BOTH follows
/// (each tagged with its own workspace, A never dropped), BOTH first-receive baselines, and no WAL.
fn assert_ab_converged(rig: &Rig) {
    let layout = rig.layout();

    let user = enroll::read_user(&rig.fs, &layout)
        .unwrap()
        .expect("user.json");
    assert_eq!(user.workspaces.len(), 2, "both memberships present");
    let a = user
        .membership(WS)
        .expect("workspace A membership survived");
    assert_eq!(a.display_name.as_deref(), Some("Acme Inc"));
    let b = user
        .membership(WS_B)
        .expect("workspace B membership landed");
    assert_eq!(b.display_name.as_deref(), Some("Beta Team"));

    let follows = enroll::read_follows(&rig.fs, &layout)
        .unwrap()
        .expect("follows.json");
    assert_eq!(follows.follows.len(), 2, "both follows present");
    let fa = follows
        .follows
        .iter()
        .find(|f| f.skill_id == "s_deploy")
        .expect("A's follow survived");
    assert_eq!(fa.workspace_id, WS);
    assert!(fa.following);
    let fb = follows
        .follows
        .iter()
        .find(|f| f.skill_id == B_SKILL)
        .expect("B's follow landed");
    assert_eq!(fb.workspace_id, WS_B);
    assert!(fb.following);

    assert!(
        rig.fs.exists(&layout.skill_dir(&sid("s_deploy"))),
        "A's first-receive baseline exists"
    );
    assert!(
        rig.fs.exists(&layout.skill_dir(&sid(B_SKILL))),
        "B's first-receive baseline exists"
    );

    assert!(
        enroll::read_wal(&rig.fs, &layout).unwrap().is_none(),
        "the WAL is cleared once the promotion completes"
    );
}

#[test]
fn a_second_same_plane_workspace_enrollment_adds_a_membership_and_merges_follows() {
    // The happy path: enrolled in A, then follow into a DIFFERENT workspace B on the SAME plane. The
    // second promote UPSERTS a membership + MERGES a follow — it never overwrites A.
    let rig = Rig::new("mws-happy");
    let mut plane = FixturePlane::default();
    plane.serve_genesis("s_deploy", GENESIS_FILES);
    enroll_workspace_a(&rig, &plane);
    write_workspace_b_redeemed_wal(&rig);

    resume_over_fs(&rig, &rig.fs, &plane).expect("B resume promotes");

    assert_ab_converged(&rig);
}

/// Fault EACH durable step of workspace B's promotion, then replay the Redeemed WAL to completion and
/// assert the converged state is coherent — A always intact, B either fully landed or cleanly resumable,
/// never a stuck partial and never a dropped first workspace.
#[test]
fn second_enrollment_promote_is_crash_coherent_and_never_drops_the_first() {
    // Count the promote's durable ops from a clean run (fail_at == 0 never faults) and confirm convergence.
    let n_ops = {
        let rig = Rig::new("mws-count");
        let mut plane = FixturePlane::default();
        plane.serve_genesis("s_deploy", GENESIS_FILES);
        enroll_workspace_a(&rig, &plane);
        write_workspace_b_redeemed_wal(&rig);
        let fs = FaultFs::new(0);
        resume_over_fs(&rig, &fs, &plane).expect("clean B resume promotes");
        assert_ab_converged(&rig);
        fs.ops_attempted()
    };
    assert!(
        n_ops > 5,
        "expected several durable promote ops, got {n_ops}"
    );

    for fail_at in 1..=n_ops {
        let rig = Rig::new(&format!("mws-{fail_at}"));
        let mut plane = FixturePlane::default();
        plane.serve_genesis("s_deploy", GENESIS_FILES);
        enroll_workspace_a(&rig, &plane);
        write_workspace_b_redeemed_wal(&rig);
        let layout = rig.layout();

        // Fault the Nth durable op of B's promote (it may error mid-sequence).
        let fs = FaultFs::new(fail_at);
        let _ = resume_over_fs(&rig, &fs, &plane);

        // DURING-CRASH coherence (before any heal). Both docs are atomic, so they always load and A is
        // never lost; B is either fully landed or cleanly resumable — never a stuck partial.
        let user = enroll::read_user(&rig.fs, &layout)
            .unwrap()
            .expect("user.json still loads after the crash");
        assert!(
            user.membership(WS).is_some(),
            "fail_at={fail_at}: A's membership survives the crash"
        );
        let follows = enroll::read_follows(&rig.fs, &layout)
            .unwrap()
            .expect("follows.json still loads after the crash");
        assert!(
            follows.follows.iter().any(|f| f.skill_id == "s_deploy"),
            "fail_at={fail_at}: A's follow survives the crash"
        );

        let b_member = user.membership(WS_B).is_some();
        let wal_present = enroll::read_wal(&rig.fs, &layout).unwrap().is_some();
        // Ordering invariant: follows.json (step 2) lands before the membership (step 3), so a committed B
        // membership ALWAYS implies B's read creds are already on disk (an ambient write that resolves to B
        // can never find a membership without its follow credential).
        if b_member {
            assert!(
                follows.follows.iter().any(|f| f.skill_id == B_SKILL),
                "fail_at={fail_at}: a B membership must imply a B follow (follows written first)"
            );
        }
        // The WAL is the transaction log, deleted LAST: a not-yet-written B membership proves the crash
        // landed before the WAL was cleared, so the enrollment is still resumable (never a lost partial).
        if !b_member {
            assert!(
                wal_present,
                "fail_at={fail_at}: an incomplete B promote must leave the Redeemed WAL for resume"
            );
        }

        // Heal: replay the Redeemed WAL to completion (idempotent). If the WAL is already gone, B fully
        // landed on the faulted run (the WAL is deleted only after every other step), so there is nothing
        // left to resume.
        if wal_present {
            resume_over_fs(&rig, &rig.fs, &plane)
                .unwrap_or_else(|e| panic!("fail_at={fail_at}: healing resume failed: {e:?}"));
        }

        // Converged: both memberships, both follows (A intact), both baselines, WAL gone.
        assert_ab_converged(&rig);
    }
}
