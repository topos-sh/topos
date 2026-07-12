//! End-to-end tests of the `follow` device-flow over a FAKE `EnrollSource` + a fixture plane (no HTTP):
//! the two-call resume (pending → granted → promote), the first-receive consent rules, the 0600 sidecar writers + secret
//! redaction, the merge-on-second-follow, the Redeemed-WAL recovery, and the first-receive baseline that
//! the existing pull engine offers (bare sweep) then places (explicit accept).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use topos_core::digest::{self, FileMode, ManifestEntry, to_hex};
use topos_core::identity::Commit;
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::bootstrap::{
    BootstrapData, BootstrapInvite, BootstrapPlane, BootstrapSkill, BootstrapWorkspace,
    ConsentMode, DeploymentMode, VerifiedDomainStatus,
};
use topos_types::persisted::SyncState;
use topos_types::results::PullAction;
use topos_types::{
    CurrencyKind, CurrentRecord, Generation, HarnessId, PointerScope, TriggerReport, TriggerState,
    WireCurrentRecord,
};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::{FaultFs, FsOps, RealFs};
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    DeviceAuthorize, EnrollSource, FetchedFile, FetchedVersion, FollowContext, FollowMode,
    FollowSource, Grant, GrantedToken, InertFollow, InertPlane, KnownCurrent, PlaneError,
    PlaneSource, PointerFetch, Redeem, StandupAuthorize, TokenPoll,
};
use crate::plane_http::SkillCred;
use crate::sidecar::Layout;
use crate::{doc, enroll, identity, ops};

const WS: &str = "w_acme";
const BASE_URL: &str = "https://acme.topos.test";

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
// Scratch.
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
// The fake EnrollSource — canned bootstrap / authorize / poll / redeem (nothing is signed or verified; the
// grant is an opaque bearer credential the redeem exchanges for per-skill read creds).
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
        _grant: &str,
        device_public_key: [u8; 32],
    ) -> Result<Redeem, ClientError> {
        // The device pubkey is the identity anchor; the server derives the key id from it, and the client
        // mirrors that derivation. Nothing is signed or verified. The redeem mints the ONE workspace
        // credential (the followed set now comes from the bootstrap's offered skills, carried in the WAL).
        let dk = topos_core::identity::device_key_id(&device_public_key);
        Ok(Redeem {
            workspace_id: workspace_id.to_owned(),
            device_key_id: dk,
            principal: Some("alice@acme.com".to_owned()),
            credential: format!("wsc_secret_{workspace_id}"),
        })
    }
    fn admin_claim(
        &self,
        _claim_token: &str,
        device_public_key: [u8; 32],
        _display_name: &str,
    ) -> Result<Redeem, ClientError> {
        // The LIVE `/i/` door is the claim door: enroll in ONE call, minting the workspace credential.
        // `Poll::Denied` models a refused claim (consumed / expired), which sweeps the ClaimPending WAL.
        if matches!(self.poll, Poll::Denied) {
            return Err(ClientError::Enrollment("the claim link was refused".into()));
        }
        let dk = topos_core::identity::device_key_id(&device_public_key);
        Ok(Redeem {
            workspace_id: WS.to_owned(),
            device_key_id: dk,
            principal: Some("alice@acme.com".to_owned()),
            credential: format!("wsc_secret_{WS}"),
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
            // The live `/i/` door is the admin CLAIM door (the device-code invite enrollment is retired).
            enrollment_method: "admin_claim".into(),
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
    records: HashMap<String, WireCurrentRecord>,
    versions: HashMap<(String, String), FetchedVersion>,
}
impl FixturePlane {
    fn serve_genesis(&mut self, skill: &str, files: &[(&str, FileMode, &[u8])]) {
        let v = mk_version(files);
        self.versions
            .insert((skill.to_owned(), to_hex(&v.id)), v.fetched);
        self.records
            .insert(skill.to_owned(), served(skill, v.id, 1, 1));
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
    let id = topos_core::identity::commit_id(&Commit {
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
/// An UNSIGNED `current` record for the given skill + version + generation (the plane serves these; the
/// engine scope-checks them and re-verifies the fetched bytes against the content-addressed version id).
fn served(skill: &str, version_id: [u8; 32], epoch: u64, seq: u64) -> WireCurrentRecord {
    WireCurrentRecord {
        schema_version: 1,
        scope: PointerScope {
            workspace_id: WS.into(),
            skill_id: skill.into(),
        },
        record: CurrentRecord {
            version_id: to_hex(&version_id),
            generation: Generation { epoch, seq },
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
    let dir_connect = |_b: &str| -> Box<dyn crate::plane::DirectorySource> {
        panic!("the classic follow flows never build a directory transport")
    };
    let del_connect = |_b: &str| -> Box<dyn crate::plane::ReconcileTransport> {
        panic!("the classic follow flows never build a delivery transport")
    };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        plane: &plane_connect,
        directory: &dir_connect,
        delivery: &del_connect,
        web_origin: "https://topos.sh".to_owned(),
    };
    let outcome = ops::follow(
        &ctx,
        &connectors,
        link.map(str::to_owned).into_iter().collect(),
        opts,
    )?;
    match outcome {
        ops::FollowOutcome::Data { data, .. } => Ok(data),
        _ => panic!("the classic follow flows answer the wire payload"),
    }
}

fn opts(manual: bool) -> ops::FollowOpts {
    ops::FollowOpts {
        manual,
        workspace: None,
        yes: false,
        prefix_dirname: false,
        channels: Vec::new(),
        skills: Vec::new(),
    }
}

/// Enroll a workspace the way the live flow's lockout fence leaves it: write a `Redeemed` WAL (the
/// single-use grant already spent, its minted credential + offered skills persisted), then re-invoke
/// `follow` to PROMOTE from it. This drives the SAME `promote` machinery the claim door, the standup, and
/// the (later) address follow all share — no device-flow poll (the invite device flow is retired). Returns
/// the promoted `FollowData`.
fn enroll_via_redeemed_wal(
    rig: &Rig,
    plane: &FixturePlane,
    ws: &str,
    display: &str,
    offered: &[(&str, &str)],
) -> topos_types::results::FollowData {
    rig.mint_identity();
    let context = enroll::EnrollContext {
        base_url: BASE_URL.into(),
        deployment_mode: DeploymentMode::Cloud,
        enrollment_method: "admin_claim".into(),
        workspace_id: ws.into(),
        workspace_display_name: display.into(),
        verified_domain: Some("acme.com".into()),
        verified_domain_status: VerifiedDomainStatus::Verified,
        offered_skills: offered
            .iter()
            .map(|(id, name)| enroll::OfferedSkill {
                skill_id: (*id).into(),
                name: Some((*name).into()),
            })
            .collect(),
        mode: enroll::FollowModeDoc::Auto,
        // Joining an existing workspace is invite-rooted (the address-follow leg drives this path live).
        root: enroll::EnrollRoot::Invite,
        follow_target: None,
    };
    let wal = enroll::PendingEnrollment {
        schema_version: 1,
        state: enroll::EnrollPhase::Redeemed {
            context,
            credential: format!("wsc_secret_{ws}"),
            device_key_id: device_key_id_for(&device_pubkey(rig)),
            principal: Some("alice@acme.com".into()),
            enrolled_at_millis: 1,
        },
    };
    enroll::write_wal(&rig.fs, &rig.layout(), &wal).unwrap();
    // A pending WAL → the follow dispatch RESUMES it (rule 1), promoting from the persisted credential.
    run_follow(rig, &fake(offered, Poll::Granted), plane, None, opts(false)).unwrap()
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
fn a_redeemed_wal_promote_writes_all_docs_records_the_key_and_clears_the_wal() {
    let rig = Rig::new("granted");
    let mut plane = FixturePlane::default();
    plane.serve_genesis("s_deploy", GENESIS_FILES);

    // Promote from the lockout-fence `Redeemed` WAL (the live flow's shared promote path — the device-code
    // invite enrollment that once produced this two-call is retired).
    let data = enroll_via_redeemed_wal(&rig, &plane, WS, "Acme Inc", &[("s_deploy", "deploy")]);

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
    // instance.json — the plane the device enrolled with (base URL + posture). No trust root is stored:
    // the `current` pointer is unsigned, its authority the database row. The per-workspace disclosure
    // lives on the user.json membership asserted below.
    let instance = enroll::read_instance(&rig.fs, &layout)
        .unwrap()
        .expect("instance.json");
    assert_eq!(instance.base_url, BASE_URL);
    assert_eq!(instance.deployment_mode, DeploymentMode::Cloud);
    // instance.json is PUBLIC (ordinary perms — plane metadata, no secret).
    assert_eq!(mode_of(&layout.instance_path()), 0o644);

    // follows.json — 0600, the skill following=true.
    assert_eq!(
        mode_of(&layout.follows_path()),
        0o600,
        "follows.json is 0600 for perm hygiene (pure subscription state now)"
    );
    let follows = enroll::read_follows(&rig.fs, &layout)
        .unwrap()
        .expect("follows.json");
    assert_eq!(follows.follows.len(), 1);
    assert_eq!(follows.follows[0].skill_id, "s_deploy");
    assert!(follows.follows[0].following);
    // The load_enrollment Some-condition holds (instance present AND ≥1 following skill).
    assert!(follows.follows.iter().any(|f| f.following));

    // credentials.json — 0600, holds the workspace credential the redeem minted (the followed skill's
    // Bearer). The secret lives HERE now, not on the follow entry.
    assert_eq!(
        mode_of(&layout.credentials_path()),
        0o600,
        "credentials.json holds the secret workspace credential"
    );
    let creds = enroll::read_credentials(&rig.fs, &layout)
        .unwrap()
        .expect("credentials.json")
        .into_map();
    assert_eq!(creds.get(WS).map(String::as_str), Some("wsc_secret_w_acme"));

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
        follow_target: None,
    };
    let wal = enroll::PendingEnrollment {
        schema_version: 1,
        state: enroll::EnrollPhase::Redeemed {
            context,
            credential: "wsc_secret_w_acme".into(),
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
    let dir_connect = |_b: &str| -> Box<dyn crate::plane::DirectorySource> {
        panic!("an invite-rooted promote never builds a directory transport")
    };
    let del_connect = |_b: &str| -> Box<dyn crate::plane::ReconcileTransport> {
        panic!("an invite-rooted promote never builds a delivery transport")
    };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        plane: &plane_connect,
        directory: &dir_connect,
        delivery: &del_connect,
        web_origin: "https://topos.sh".to_owned(),
    };
    let data = match ops::follow(&ctx, &connectors, Vec::new(), opts(false)).unwrap() {
        ops::FollowOutcome::Data { data, .. } => data,
        _ => panic!("an invite-rooted promote answers the wire payload"),
    };

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
    fn redeem(&self, _w: &str, _g: &str, _k: [u8; 32]) -> Result<Redeem, ClientError> {
        panic!("a Redeemed-WAL resume must NOT re-redeem the single-use grant")
    }
    fn admin_claim(&self, _c: &str, _k: [u8; 32], _d: &str) -> Result<Redeem, ClientError> {
        panic!("a Redeemed-WAL resume must not redeem an admin claim")
    }
}

#[test]
fn a_denied_claim_is_a_typed_error_and_sweeps_the_wal() {
    // A refused claim (consumed / expired) at the `/i/` door: one call, a typed error, and the
    // ClaimPending WAL swept so a later `follow <other-link>` is never wedged behind it.
    let rig = Rig::new("denied");
    let plane = FixturePlane::default();
    let link = format!("{BASE_URL}/i/tok");
    let err = run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Denied),
        &plane,
        Some(&link),
        opts(false),
    )
    .unwrap_err();
    assert!(matches!(err, ClientError::Enrollment(_)), "got {err:?}");
    // The dead claim's WAL is swept.
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
}

#[test]
fn a_cross_base_url_follow_is_refused() {
    let rig = Rig::new("crossplane");
    let mut plane = FixturePlane::default();
    plane.serve_genesis("s_deploy", GENESIS_FILES);
    // Enroll with the first plane (the one-call claim door).
    run_follow(
        &rig,
        &fake(&[("s_deploy", "deploy")], Poll::Granted),
        &plane,
        Some(&format!("{BASE_URL}/i/t")),
        opts(false),
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
        opts(false),
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
        opts(false),
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
    let f = fake(&[("s_deploy", "deploy")], Poll::Granted);

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
    let dir_connect = |_b: &str| -> Box<dyn crate::plane::DirectorySource> {
        panic!("this flow never builds a directory transport")
    };
    let del_connect = |_b: &str| -> Box<dyn crate::plane::ReconcileTransport> {
        panic!("this flow never builds a delivery transport")
    };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        plane: &plane_connect,
        directory: &dir_connect,
        delivery: &del_connect,
        web_origin: "https://topos.sh".to_owned(),
    };
    // The claim door enrolls in ONE call, re-rooting first.
    let data = match ops::follow(
        &ctx,
        &connectors,
        vec!["https://links.example/i/tok_abc".to_owned()],
        opts(false),
    )
    .expect("the claim door enrolls in one call after re-rooting")
    {
        ops::FollowOutcome::Data { data, .. } => data,
        _ => panic!("the claim door answers the wire payload"),
    };

    assert!(data.enrolled);
    // The receipt disclosed the plane the device actually enrolls against (not the share host).
    assert_eq!(data.plane_base_url.as_deref(), Some(BASE_URL));
    // The bootstrap GET rode the link base; the claim redeem rode the re-rooted API base.
    assert_eq!(
        bases.borrow().as_slice(),
        ["https://links.example", BASE_URL]
    );
    // The pinned instance records the API base — the share host is nowhere.
    let instance = enroll::read_instance(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert_eq!(instance.base_url, BASE_URL);
}

#[test]
fn the_workspace_credential_never_leaks_in_debug() {
    // The secret is the WORKSPACE credential now — redacted everywhere it could surface (the transport
    // credential, credentials.json's entry, the WAL's redeemed credential). A follow entry carries no
    // secret any more.
    let cred = SkillCred::new(WS.into(), "wsc_super_secret".into());
    assert!(!format!("{cred:?}").contains("wsc_super_secret"));
    let entry = enroll::CredentialEntry {
        workspace_id: WS.into(),
        credential: "wsc_super_secret".into(),
    };
    assert!(!format!("{entry:?}").contains("wsc_super_secret"));
    let wal = enroll::PendingEnrollment {
        schema_version: 1,
        state: enroll::EnrollPhase::Redeemed {
            context: tiny_context(),
            credential: "wsc_super_secret".into(),
            device_key_id: "dk_x".into(),
            principal: None,
            enrolled_at_millis: 1,
        },
    };
    let dbg = format!("{wal:?}");
    assert!(dbg.contains("<redacted>"));
    assert!(
        !dbg.contains("wsc_super_secret"),
        "WAL Debug leaked the workspace credential"
    );
    // The redeem result + the grant + device-code domain types redact too.
    let redeem = Redeem {
        workspace_id: WS.into(),
        device_key_id: "dk_x".into(),
        principal: None,
        credential: "wsc_super_secret".into(),
    };
    assert!(!format!("{redeem:?}").contains("wsc_super_secret"));
    assert!(!format!("{:?}", Grant::new("grant_x".into())).contains("grant_x"));
}

fn tiny_context() -> enroll::EnrollContext {
    enroll::EnrollContext {
        base_url: BASE_URL.into(),
        deployment_mode: DeploymentMode::Cloud,
        enrollment_method: "device_code".into(),
        workspace_id: WS.into(),
        workspace_display_name: "Acme".into(),
        verified_domain: None,
        verified_domain_status: VerifiedDomainStatus::Unverified,
        offered_skills: Vec::new(),
        mode: enroll::FollowModeDoc::Auto,
        root: enroll::EnrollRoot::Invite,
        follow_target: None,
    }
}

#[test]
fn the_first_receive_baseline_is_laid_then_a_fixture_plane_pull_offers_then_places() {
    let rig = Rig::new("firstrecv");
    let mut plane = FixturePlane::default();
    plane.serve_genesis("s_deploy", GENESIS_FILES);

    // Enroll + follow s_deploy (the live promote path lays the first-receive baseline from the WAL's
    // offered skills).
    enroll_via_redeemed_wal(&rig, &plane, WS, "Acme Inc", &[("s_deploy", "deploy")]);

    // The first-receive baseline is laid: a NEVER-RECEIVED sync (empty recorded, genesis floor), a store,
    // and a map carrying the harness placement target.
    let layout = rig.layout();
    // Find the laid skill dir (the baseline is keyed by the plane's skill id).
    let sp = layout.published(&sid("s_deploy"));
    let sync: SyncState = doc::read_doc(&rig.fs, &sp.sync)
        .unwrap()
        .expect("baseline sync.json");
    assert_eq!(
        sync.observed_version_id,
        "0".repeat(64),
        "never-received: the all-zero version-id sentinel"
    );
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

    // A SECOND bare sweep (the first-receive-consent regression guard): the first sweep moved `observed` to
    // the served target, but the still-unapproved first-receive baseline is STILL offered, NEVER auto-landed — even for
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
            workspace: None,
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
    enroll_via_redeemed_wal(&rig, &plane, WS, "Acme Inc", &[("s_deploy", "deploy")]);

    // `follow deploy` drives the engine through ctx.plane (a separate, post-enroll
    // invocation where the plane is live).
    let inert_f = InertFollow;
    let ctx = rig.ctx(&plane, &inert_f);
    let enroll_connect = |_b: &str| -> Box<dyn EnrollSource> { Box::new(PanicEnroll) };
    let plane_connect = |_b: &str, _c: HashMap<String, SkillCred>| -> Box<dyn PlaneSource> {
        Box::new(plane.clone())
    };
    let dir_connect = |_b: &str| -> Box<dyn crate::plane::DirectorySource> {
        panic!("the skill path never builds a directory transport")
    };
    let del_connect = |_b: &str| -> Box<dyn crate::plane::ReconcileTransport> {
        panic!("the skill path never builds a delivery transport")
    };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        plane: &plane_connect,
        directory: &dir_connect,
        delivery: &del_connect,
        web_origin: "https://topos.sh".to_owned(),
    };
    let (data, resumed) =
        match ops::follow(&ctx, &connectors, vec!["deploy".to_owned()], opts(false)).unwrap() {
            ops::FollowOutcome::Data { data, resumed } => (data, resumed),
            _ => panic!("the skill path answers the wire payload"),
        };

    assert!(data.enrolled);
    assert_eq!(data.skills.len(), 1);
    assert!(
        resumed.is_empty(),
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
// The re-invoke-resume interleave: with a pending enrollment WAL on disk, re-invoking `follow` (with any
// target, or none) RESUMES the in-flight session rather than clobbering it — a live Authorizing session is
// re-polled; a Redeemed-but-unpromoted grant is completed from its single-use creds; a dead (expired)
// session is superseded by the start-of-command recovery sweep, then a fresh follow begins.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_follow_while_a_redeemed_wal_is_unpromoted_completes_the_promotion() {
    // A Redeemed-but-unpromoted grant is single-use (spent server-side) and its minted workspace credential
    // lives ONLY in this WAL. Re-invoking `follow` (with any target) COMPLETES the promotion from the
    // persisted credential + the WAL's offered skills — it never loses them and never clobbers the WAL.
    let rig = Rig::new("interleave-redeemed");
    let plane = FixturePlane::default();
    rig.mint_identity();

    // The followed set comes from `context.offered_skills` now — the WAL carries s_deploy.
    let context = enroll::EnrollContext {
        offered_skills: vec![enroll::OfferedSkill {
            skill_id: "s_deploy".into(),
            name: Some("deploy".into()),
        }],
        ..tiny_context()
    };
    let wal = enroll::PendingEnrollment {
        schema_version: 1,
        state: enroll::EnrollPhase::Redeemed {
            context,
            credential: "wsc_secret_w_acme".into(),
            device_key_id: device_key_id_for(&device_pubkey(&rig)),
            principal: None,
            enrolled_at_millis: 1,
        },
    };
    enroll::write_wal(&rig.fs, &rig.layout(), &wal).unwrap();

    let data = run_follow(
        &rig,
        &fake(&[("s_review", "review")], Poll::Pending),
        &plane,
        Some(&format!("{BASE_URL}/i/tok_b")),
        opts(false),
    )
    .expect("a re-invoked follow promotes the redeemed grant from its persisted credential");
    assert!(data.enrolled, "the redeemed enrollment completed");
    // The single-use creds were USED (never lost): follows.json now carries the skill, and the WAL is gone.
    let follows = enroll::read_follows(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert!(
        follows.follows.iter().any(|f| f.skill_id == "s_deploy"),
        "the redeemed read cred landed in follows.json"
    );
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
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

/// Fully enroll workspace A (skill `s_deploy`) through the live promote path (the shared `Redeemed`-WAL
/// resume the claim door / standup / address follow all use), so instance.json is written, follows.json +
/// user.json carry A, host.json holds the device key, and A's first-receive baseline is laid. Leaves no
/// WAL (A's own promote deleted it).
fn enroll_workspace_a(rig: &Rig, plane: &FixturePlane) {
    enroll_via_redeemed_wal(rig, plane, WS, "Acme Inc", &[("s_deploy", "deploy")]);
}

/// Hand-write workspace B's `Redeemed` WAL exactly as the lockout fence records it BEFORE promotion — the
/// same install/device (so the device key id + principal match), a DIFFERENT `workspace_id` on the SAME
/// base URL + pinned key. A re-invoked `follow` promotes from this without re-redeeming.
fn write_workspace_b_redeemed_wal(rig: &Rig) {
    let context = enroll::EnrollContext {
        base_url: BASE_URL.into(),
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
        follow_target: None,
    };
    let wal = enroll::PendingEnrollment {
        schema_version: 1,
        state: enroll::EnrollPhase::Redeemed {
            context,
            credential: format!("wsc_secret_{WS_B}"),
            device_key_id: device_key_id_for(&device_pubkey(rig)),
            principal: Some("alice@acme.com".into()),
            enrolled_at_millis: 2,
        },
    };
    enroll::write_wal(&rig.fs, &rig.layout(), &wal).unwrap();
}

/// Drive a re-invoked `follow` (resume) over an arbitrary fs (the crash gate injects a [`FaultFs`]). A Redeemed-WAL
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
    let dir_connect = |_b: &str| -> Box<dyn crate::plane::DirectorySource> {
        panic!("an invite-rooted promote never builds a directory transport")
    };
    let del_connect = |_b: &str| -> Box<dyn crate::plane::ReconcileTransport> {
        panic!("an invite-rooted promote never builds a delivery transport")
    };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        plane: &plane_connect,
        directory: &dir_connect,
        delivery: &del_connect,
        web_origin: "https://topos.sh".to_owned(),
    };
    match ops::follow(&ctx, &connectors, Vec::new(), opts(false))? {
        ops::FollowOutcome::Data { data, .. } => Ok(data),
        _ => panic!("an invite-rooted promote answers the wire payload"),
    }
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
