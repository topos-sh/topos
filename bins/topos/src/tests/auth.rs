//! The `auth` group over fakes (no HTTP): login writes + the different-account confirm gate,
//! logout keeping every byte, and the status causes (healthy / gone / unreachable / no credential).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::requests::{
    WireChannelIndex, WireMe, WireProposalIndex, WireReach, WireSkillIndex, WireSkillLog,
};
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    Card, DeviceAuthorize, DirectorySource, EnrollSource, GovernanceSource, Grant, GrantedToken,
    InertFollow, InertPlane, LoginRedeem, LoginSeat, Redeem, StandupAuthorize, TokenPoll,
};
use crate::sidecar::Layout;
use crate::{enroll, ops};

const API: &str = "https://api.acme.test";

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-auth-{tag}-{}-{n}", std::process::id()));
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

struct NullHarness;
impl HarnessAdapter for NullHarness {
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
            dir: std::env::temp_dir().join(skill_id),
        }
    }
    fn currency_kind(&self) -> CurrencyKind {
        CurrencyKind::ExplicitPullOnly
    }
    fn install_currency_trigger(&self) -> TriggerReport {
        TriggerReport {
            harness: HarnessId::ClaudeCode,
            currency_kind: CurrencyKind::ExplicitPullOnly,
            touched_path: None,
            marker_id: "test".into(),
            state: TriggerState::Inactive,
        }
    }
    fn remove_currency_trigger(&self) -> TriggerReport {
        self.install_currency_trigger()
    }
    fn uninstall_footprint(&self) -> Vec<PathBuf> {
        // "Armed": auth status reads the hook state off this probe.
        vec![PathBuf::from("/tmp/settings.json")]
    }
}

struct Rig {
    home: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: NullHarness,
}
impl Rig {
    fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(tag),
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1_700_000_000_000),
            harness: NullHarness,
        }
    }
    fn layout(&self) -> Layout {
        Layout::new(&self.home.0)
    }
    fn ctx<'a>(
        &'a self,
        plane: &'a dyn crate::plane::PlaneSource,
        follow: &'a dyn crate::plane::FollowSource,
    ) -> Ctx<'a> {
        Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: "d_test".into(),
            layout: self.layout(),
            harness: &self.harness,
            plane,
            follow,
        }
    }
}

type CallLog = Arc<Mutex<Vec<String>>>;

/// The login-flow enroll fake: the card, the login device flow, and the login redeem's seats.
#[derive(Clone)]
struct FakeLogin {
    api_base: String,
    principal: String,
    seats: Vec<LoginSeat>,
    log: CallLog,
}

impl EnrollSource for FakeLogin {
    fn fetch_bootstrap(&self, _t: &str) -> Result<topos_types::BootstrapData, ClientError> {
        unreachable!("login reads the card, never an /i/ bootstrap")
    }
    fn fetch_card(&self, url: &str) -> Result<Card, ClientError> {
        self.log.lock().unwrap().push(format!("card {url}"));
        Ok(Card::Protocol(topos_types::requests::WireProtocolCard {
            schema_version: 1,
            card: "topos-protocol-card".to_owned(),
            api_base_url: self.api_base.clone(),
        }))
    }
    fn device_authorize(
        &self,
        _ws: &str,
        _pk: [u8; 32],
        _m: &str,
    ) -> Result<DeviceAuthorize, ClientError> {
        unreachable!("login never starts an enroll session")
    }
    fn device_authorize_login(
        &self,
        _pk: [u8; 32],
        _machine: &str,
    ) -> Result<DeviceAuthorize, ClientError> {
        self.log.lock().unwrap().push("authorize login".to_owned());
        Ok(DeviceAuthorize {
            device_code: "dc_login".into(),
            user_code: "LOGIN-CODE".into(),
            verification_uri: format!("{}/verify", self.api_base),
            verification_uri_complete: Some(format!("{}/verify/LOGIN-CODE", self.api_base)),
            expires_in: 900,
            interval: 5,
        })
    }
    fn device_authorize_standup(
        &self,
        _pk: [u8; 32],
        _m: &str,
    ) -> Result<StandupAuthorize, ClientError> {
        unreachable!("login never starts a standup")
    }
    fn poll_token(&self, _dc: &str) -> Result<TokenPoll, ClientError> {
        Ok(TokenPoll::Granted(GrantedToken {
            grant: Grant::new("grant_login".into()),
            workspace: None,
        }))
    }
    fn login_redeem(&self, _grant: &str, _pk: [u8; 32]) -> Result<LoginRedeem, ClientError> {
        self.log.lock().unwrap().push("login redeem".to_owned());
        Ok(LoginRedeem {
            principal: self.principal.clone(),
            seats: self.seats.clone(),
        })
    }
    fn redeem(&self, _ws: &str, _g: &str, _pk: [u8; 32]) -> Result<Redeem, ClientError> {
        unreachable!("login redeems at /v1/login, never the enroll redeem")
    }
    fn admin_claim(&self, _t: &str, _pk: [u8; 32], _d: &str) -> Result<Redeem, ClientError> {
        unreachable!("no claim in the login flow")
    }
}

fn seat(ws: &str, name: &str, credential: Option<&str>, blocked: Option<&str>) -> LoginSeat {
    LoginSeat {
        workspace_id: ws.to_owned(),
        name: name.to_owned(),
        display_name: name.to_owned(),
        role: "member".to_owned(),
        device_key_id: "dk_test".to_owned(),
        credential: credential.map(str::to_owned),
        blocked: blocked.map(str::to_owned),
    }
}

/// A directory whose `me` answers a fixed verdict per workspace (the status probe's causes).
#[derive(Clone)]
struct VerdictDirectory {
    verdicts: HashMap<String, &'static str>,
}

impl DirectorySource for VerdictDirectory {
    fn me(&self, workspace_id: &str) -> Result<WireMe, ClientError> {
        match self.verdicts.get(workspace_id).copied() {
            Some("healthy") => Ok(WireMe {
                workspace_id: workspace_id.to_owned(),
                name: "acme".into(),
                display_name: "Acme".into(),
                address: "https://topos.sh/acme".into(),
                principal: "alice@acme.com".into(),
                role: "reviewer".into(),
                invited_by: None,
                invite_policy: "members".into(),
            }),
            Some("gone") => Err(ClientError::TargetNotFound {
                target: workspace_id.to_owned(),
            }),
            _ => Err(ClientError::Plane("dial: connection refused".into())),
        }
    }
    fn channels_index(&self, _ws: &str) -> Result<WireChannelIndex, ClientError> {
        unreachable!("status probes only /me")
    }
    fn skills_index(&self, _ws: &str) -> Result<WireSkillIndex, ClientError> {
        unreachable!("status probes only /me")
    }
    fn proposals_index(&self, _ws: &str) -> Result<WireProposalIndex, ClientError> {
        unreachable!("status probes only /me")
    }
    fn skill_log(&self, _ws: &str, _s: &str) -> Result<WireSkillLog, ClientError> {
        unreachable!()
    }
    fn reach(&self, _ws: &str, _s: &str) -> Result<WireReach, ClientError> {
        unreachable!()
    }
    fn follow_skill(&self, _ws: &str, _s: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn unfollow_skill(&self, _ws: &str, _s: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn channel_join(&self, _ws: &str, _c: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn channel_leave(&self, _ws: &str, _c: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn channel_place(&self, _ws: &str, _c: &str, _s: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn channel_unplace(&self, _ws: &str, _c: &str, _s: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn exclude_device(&self, _ws: &str, _s: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn protect_skill(&self, _ws: &str, _s: &str, _l: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn protect_channel(&self, _ws: &str, _c: &str, _l: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn ack_notices(&self, _ws: &str, _ids: &[String]) -> Result<(), ClientError> {
        unreachable!()
    }
}

/// A governance fake recording the self-revokes.
#[derive(Clone)]
struct FakeGovernance {
    log: CallLog,
    fail_ws: Option<String>,
}
impl GovernanceSource for FakeGovernance {
    fn invite(
        &self,
        _ws: &str,
        _body: topos_types::requests::InvitationRequest,
    ) -> Result<topos_types::requests::InvitationData, ClientError> {
        unreachable!("logout never invites")
    }
    fn revoke_device(&self, ws: &str, target: &str, _op_id: &str) -> Result<(), ClientError> {
        if self.fail_ws.as_deref() == Some(ws) {
            return Err(ClientError::Plane("dial: refused".into()));
        }
        self.log
            .lock()
            .unwrap()
            .push(format!("revoke {ws} {target}"));
        Ok(())
    }
}

fn login_connectors<'a>(
    enroll_fake: &'a FakeLogin,
    directory: &'a VerdictDirectory,
    governance: &'a FakeGovernance,
    enroll_connect: &'a dyn Fn(&str) -> Box<dyn EnrollSource>,
    dir_connect: &'a dyn Fn(&str) -> Box<dyn DirectorySource>,
    gov_connect: &'a dyn Fn(&str) -> Box<dyn GovernanceSource>,
) -> ops::AuthConnectors<'a> {
    let _ = (enroll_fake, directory, governance);
    ops::AuthConnectors {
        enroll: enroll_connect,
        directory: dir_connect,
        governance: gov_connect,
        web_origin: "https://topos.sh".to_owned(),
    }
}

/// Run login end to end over the fakes: call 1 (pending) then the resume (granted → redeem).
fn run_login(rig: &Rig, fake: &FakeLogin, yes: bool) -> Result<ops::AuthLoginOutcome, ClientError> {
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let enroll_connect = |_b: &str| -> Box<dyn EnrollSource> { Box::new(fake.clone()) };
    let dir_connect = |_b: &str| -> Box<dyn DirectorySource> {
        panic!("login never builds a directory transport")
    };
    let gov_connect = |_b: &str| -> Box<dyn GovernanceSource> {
        panic!("login never builds a governance transport")
    };
    let directory = VerdictDirectory {
        verdicts: HashMap::new(),
    };
    let governance = FakeGovernance {
        log: Arc::default(),
        fail_ws: None,
    };
    let connectors = login_connectors(
        fake,
        &directory,
        &governance,
        &enroll_connect,
        &dir_connect,
        &gov_connect,
    );
    let first = ops::login(&ctx, &connectors, None, yes)?;
    match first {
        // The BIN's re-invoke loop resumes the pending session; drive one resume here.
        ops::AuthLoginOutcome::Pending(p) => {
            assert!(p.verification_uri_complete.contains("LOGIN-CODE"));
            ops::login(&ctx, &connectors, None, yes)
        }
        done => Ok(done),
    }
}

#[test]
fn login_mints_per_seat_credentials_and_reports_blocked_ones() {
    let rig = Rig::new("login");
    crate::identity::load_or_create_device_id(&rig.fs, &rig.layout()).unwrap();
    let fake = FakeLogin {
        api_base: API.to_owned(),
        principal: "alice@acme.com".to_owned(),
        seats: vec![
            seat("w_acme", "acme", Some("wsc_a"), None),
            seat("w_beta", "beta", None, Some("device revoked")),
        ],
        log: Arc::default(),
    };
    let out = run_login(&rig, &fake, false).unwrap();
    let ops::AuthLoginOutcome::Done(data) = out else {
        panic!("the resumed login settles");
    };
    assert_eq!(data.principal, "alice@acme.com");
    assert_eq!(data.server, API);
    assert!(data.replaced_principal.is_none());
    assert_eq!(data.memberships.len(), 2);
    assert!(data.memberships[0].minted);
    assert!(!data.memberships[1].minted);
    assert_eq!(
        data.memberships[1].blocked.as_deref(),
        Some("device revoked")
    );

    // The writes: instance pinned, the MINTED seat's credential stored (the blocked one absent),
    // user.json carrying the principal + both memberships, the WAL gone.
    let layout = rig.layout();
    assert_eq!(
        enroll::read_instance(&rig.fs, &layout)
            .unwrap()
            .unwrap()
            .base_url,
        API
    );
    let creds = enroll::read_credentials(&rig.fs, &layout)
        .unwrap()
        .unwrap()
        .into_map();
    assert_eq!(creds.get("w_acme").map(String::as_str), Some("wsc_a"));
    assert!(!creds.contains_key("w_beta"));
    let user = enroll::read_user(&rig.fs, &layout).unwrap().unwrap();
    assert_eq!(user.principal.as_deref(), Some("alice@acme.com"));
    assert_eq!(user.email.as_deref(), Some("alice@acme.com"));
    assert_eq!(user.workspaces.len(), 2);
    assert!(enroll::read_wal(&rig.fs, &layout).unwrap().is_none());

    // A SAME-account re-login is an idempotent re-mint (no confirm gate).
    let out = run_login(&rig, &fake, false).unwrap();
    assert!(matches!(out, ops::AuthLoginOutcome::Done(d) if d.replaced_principal.is_none()));
}

#[test]
fn a_different_account_login_needs_yes_and_then_replaces_wholesale() {
    let rig = Rig::new("login-switch");
    crate::identity::load_or_create_device_id(&rig.fs, &rig.layout()).unwrap();
    // Sign in as alice with a seat in w_acme…
    let alice = FakeLogin {
        api_base: API.to_owned(),
        principal: "alice@acme.com".to_owned(),
        seats: vec![seat("w_acme", "acme", Some("wsc_alice"), None)],
        log: Arc::default(),
    };
    run_login(&rig, &alice, false).unwrap();

    // …then bob WITHOUT --yes: a typed CONFIRM_REQUIRED naming both accounts; nothing replaced.
    let bob = FakeLogin {
        api_base: API.to_owned(),
        principal: "bob@acme.com".to_owned(),
        seats: vec![seat("w_beta", "beta", Some("wsc_bob"), None)],
        log: Arc::default(),
    };
    let err = run_login(&rig, &bob, false).unwrap_err();
    assert_eq!(err.code(), "CONFIRM_REQUIRED");
    let msg = err.to_string();
    assert!(
        msg.contains("alice@acme.com") && msg.contains("bob@acme.com"),
        "{msg}"
    );
    assert!(msg.contains("--yes"), "{msg}");
    let creds = enroll::read_credentials(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap()
        .into_map();
    assert_eq!(creds.get("w_acme").map(String::as_str), Some("wsc_alice"));
    assert!(
        !creds.contains_key("w_beta"),
        "nothing written without --yes"
    );
    // The refused login cleared its spent WAL (the re-run starts a fresh sign-in).
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());

    // With --yes the credentials are replaced WHOLESALE — alice's are gone.
    let out = run_login(&rig, &bob, true).unwrap();
    let ops::AuthLoginOutcome::Done(data) = out else {
        panic!("settles");
    };
    assert_eq!(data.replaced_principal.as_deref(), Some("alice@acme.com"));
    let creds = enroll::read_credentials(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap()
        .into_map();
    assert!(
        !creds.contains_key("w_acme"),
        "the old account's credential is gone"
    );
    assert_eq!(creds.get("w_beta").map(String::as_str), Some("wsc_bob"));
    let user = enroll::read_user(&rig.fs, &rig.layout()).unwrap().unwrap();
    assert_eq!(user.principal.as_deref(), Some("bob@acme.com"));
    assert_eq!(
        user.workspaces.len(),
        1,
        "the membership list was replaced too"
    );
}

#[test]
fn logout_revokes_best_effort_deletes_credentials_and_keeps_every_byte() {
    let rig = Rig::new("logout");
    crate::identity::load_or_create_device_id(&rig.fs, &rig.layout()).unwrap();
    let layout = rig.layout();
    // A signed-in install with two workspaces + a skill's sidecar bytes on disk.
    enroll::write_instance(
        &rig.fs,
        &layout,
        &enroll::Instance {
            schema_version: 1,
            base_url: API.to_owned(),
            deployment_mode: topos_types::bootstrap::DeploymentMode::Cloud,
            enrollment_method: "device_code".to_owned(),
        },
    )
    .unwrap();
    enroll::write_credential(&rig.fs, &layout, "w_acme", "wsc_a").unwrap();
    enroll::write_credential(&rig.fs, &layout, "w_beta", "wsc_b").unwrap();
    enroll::write_user(
        &rig.fs,
        &layout,
        &enroll::UserDoc {
            schema_version: 1,
            email: Some("alice@acme.com".into()),
            principal: Some("alice@acme.com".into()),
            workspaces: Vec::new(),
        },
    )
    .unwrap();
    enroll::write_follows_merged(
        &rig.fs,
        &layout,
        &[enroll::FollowEntry {
            skill_id: "s_docs".to_owned(),
            workspace_id: "w_acme".to_owned(),
            mode: enroll::FollowModeDoc::Auto,
            review_required: false,
            following: true,
            excluded_here: false,
        }],
    )
    .unwrap();
    let skill_bytes = rig.home.0.join("skills-probe");
    std::fs::create_dir_all(&skill_bytes).unwrap();
    std::fs::write(skill_bytes.join("SKILL.md"), b"# kept\n").unwrap();

    let log: CallLog = Arc::default();
    let governance = FakeGovernance {
        log: log.clone(),
        // One revoke fails (the plane is down for w_beta) — best-effort, never a blocker.
        fail_ws: Some("w_beta".to_owned()),
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let enroll_connect = |_b: &str| -> Box<dyn EnrollSource> { panic!("logout never enrolls") };
    let dir_connect = |_b: &str| -> Box<dyn DirectorySource> {
        panic!("logout never builds a directory transport")
    };
    let gov_connect = |_b: &str| -> Box<dyn GovernanceSource> { Box::new(governance.clone()) };
    let connectors = ops::AuthConnectors {
        enroll: &enroll_connect,
        directory: &dir_connect,
        governance: &gov_connect,
        web_origin: "https://topos.sh".to_owned(),
    };

    // Bare = describe; nothing changes.
    let out = ops::logout(&ctx, &connectors, false).unwrap();
    let ops::AuthLogoutOutcome::Described { describe, yes_argv } = out else {
        panic!("bare = describe");
    };
    assert_eq!(describe.principal.as_deref(), Some("alice@acme.com"));
    assert_eq!(describe.workspaces.len(), 2);
    assert!(yes_argv.contains(&"--yes".to_owned()));
    assert!(
        enroll::read_credentials(&rig.fs, &layout)
            .unwrap()
            .is_some(),
        "the describe deleted nothing"
    );

    // --yes: revoke best-effort, delete the credentials, keep everything else.
    let out = ops::logout(&ctx, &connectors, true).unwrap();
    let ops::AuthLogoutOutcome::Applied(data) = out else {
        panic!("--yes = apply");
    };
    assert!(data.credentials_deleted);
    assert_eq!(data.revoked, vec!["w_acme".to_owned()]);
    assert_eq!(data.revoke_failed, vec!["w_beta".to_owned()]);
    // The self-revoke targeted THIS device's own key id.
    let l = log.lock().unwrap();
    assert!(
        l.iter().any(|e| e.starts_with("revoke w_acme dk_")),
        "{l:?}"
    );
    drop(l);
    // Credentials gone; skills, follows, and the principal stay.
    assert!(
        enroll::read_credentials(&rig.fs, &layout)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        std::fs::read(skill_bytes.join("SKILL.md")).unwrap(),
        b"# kept\n"
    );
    assert!(enroll::read_follows(&rig.fs, &layout).unwrap().is_some());
    assert_eq!(
        enroll::read_user(&rig.fs, &layout)
            .unwrap()
            .unwrap()
            .principal
            .as_deref(),
        Some("alice@acme.com")
    );

    // Idempotent: a second logout --yes is a clean "already signed out".
    let out = ops::logout(&ctx, &connectors, true).unwrap();
    assert!(
        matches!(out, ops::AuthLogoutOutcome::Applied(d) if !d.credentials_deleted && d.revoked.is_empty())
    );
}

#[test]
fn status_reports_the_probe_causes_and_the_reporting_posture() {
    let rig = Rig::new("status");
    let layout = rig.layout();
    enroll::write_instance(
        &rig.fs,
        &layout,
        &enroll::Instance {
            schema_version: 1,
            base_url: API.to_owned(),
            deployment_mode: topos_types::bootstrap::DeploymentMode::Cloud,
            enrollment_method: "device_code".to_owned(),
        },
    )
    .unwrap();
    let mut user = enroll::UserDoc {
        schema_version: 1,
        email: Some("alice@acme.com".into()),
        principal: Some("alice@acme.com".into()),
        workspaces: Vec::new(),
    };
    for ws in ["w_ok", "w_gone", "w_down", "w_nocred"] {
        enroll::upsert_membership(
            &mut user,
            enroll::Membership {
                workspace_id: ws.to_owned(),
                display_name: None,
                roles: Vec::new(),
                verified_domain: None,
                verified_domain_status: topos_types::bootstrap::VerifiedDomainStatus::Unverified,
                invite_rooted: false,
                enrolled_at: 1,
            },
        );
    }
    enroll::write_user(&rig.fs, &layout, &user).unwrap();
    for ws in ["w_ok", "w_gone", "w_down"] {
        enroll::write_credential(&rig.fs, &layout, ws, "wsc").unwrap();
    }
    // The reporting posture: one fresh, one stale.
    crate::sync_status::record(
        &rig.fs,
        &layout,
        &[
            (
                "w_ok".to_owned(),
                crate::sync_status::WorkspaceSync {
                    last_delivery_at: Some(1_700_000_000_000 - 1_000),
                    last_report_at: Some(1_700_000_000_000 - 1_000),
                    staleness_window_ms: 604_800_000,
                },
            ),
            (
                "w_gone".to_owned(),
                crate::sync_status::WorkspaceSync {
                    last_delivery_at: Some(1_000),
                    last_report_at: Some(1_000),
                    staleness_window_ms: 10_000,
                },
            ),
        ],
    )
    .unwrap();

    let directory = VerdictDirectory {
        verdicts: HashMap::from([
            ("w_ok".to_owned(), "healthy"),
            ("w_gone".to_owned(), "gone"),
            ("w_down".to_owned(), "down"),
        ]),
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let enroll_connect = |_b: &str| -> Box<dyn EnrollSource> { panic!("status never enrolls") };
    let dir_connect = |_b: &str| -> Box<dyn DirectorySource> { Box::new(directory.clone()) };
    let gov_connect = |_b: &str| -> Box<dyn GovernanceSource> { panic!("status never governs") };
    let connectors = ops::AuthConnectors {
        enroll: &enroll_connect,
        directory: &dir_connect,
        governance: &gov_connect,
        web_origin: "https://topos.sh".to_owned(),
    };

    let data = ops::status(&ctx, &connectors).unwrap();
    assert_eq!(data.server.as_deref(), Some(API));
    assert_eq!(data.principal.as_deref(), Some("alice@acme.com"));
    assert!(data.signed_in);
    assert!(data.hook_armed, "the adapter probe reads armed");
    let by_ws: HashMap<&str, &str> = data
        .workspaces
        .iter()
        .map(|w| (w.workspace_id.as_str(), w.health.as_str()))
        .collect();
    assert_eq!(by_ws["w_ok"], "healthy");
    assert_eq!(by_ws["w_gone"], "no access — revoked or removed");
    assert_eq!(by_ws["w_down"], "unreachable");
    assert_eq!(by_ws["w_nocred"], "no credential");
    // The healthy probe carries the role.
    assert_eq!(
        data.workspaces
            .iter()
            .find(|w| w.workspace_id == "w_ok")
            .unwrap()
            .role
            .as_deref(),
        Some("reviewer")
    );
    // The reporting posture: the stale workspace reads stale, the fresh one does not.
    let stale: HashMap<&str, bool> = data
        .reporting
        .iter()
        .map(|r| (r.workspace_id.as_str(), r.stale))
        .collect();
    assert!(!stale["w_ok"]);
    assert!(stale["w_gone"]);
}
