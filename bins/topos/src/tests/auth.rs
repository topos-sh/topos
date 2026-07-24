//! The `auth` group over fakes (no HTTP): `login` re-runs the device-authorization flow toward an
//! enrolled membership's ADDRESS and REPLACES the ONE device credential wholesale; `logout` is
//! two-phase and keeps every byte (best-effort self-revoke naming the STORED device id); `status`
//! probes access per workspace (healthy / gone / unreachable / no credential).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::requests::{
    InvitationData, InvitationRequest, WireChannelIndex, WireMe, WireProposalIndex, WireReach,
    WireSkillIndex, WireSkillLog,
};
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::enroll;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    DeviceAuthPoll, DeviceAuthStart, DirectorySource, EnrollSource, EnrolledGrant,
    EnrolledWorkspace, GovernanceSource, InertFollow, InertPlane,
};
use crate::sidecar::Layout;
use crate::{ops, sidecar};

const WS: &str = "w_acme";
const API: &str = "https://api.acme.test";
const FIXED_MILLIS: u64 = 1_700_000_000_000;

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
            dir: PathBuf::from("/nonexistent").join(skill_id),
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
            ids: SeqIds::new("a"),
            clock: FixedClock(FIXED_MILLIS),
            harness: NullHarness,
        }
    }
    fn layout(&self) -> Layout {
        Layout::new(&self.home.0)
    }
    fn ctx<'a>(&'a self, plane: &'a InertPlane, follow: &'a InertFollow) -> Ctx<'a> {
        Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: "d_test".into(),
            layout: self.layout(),
            harness: &self.harness,
            plane,
            follow,
            roots: None,
        }
    }
    /// Seed the enrolled state a completed `follow <address>` leaves: the pinned plane, one
    /// membership (with its ADDRESS name), and the ONE device credential.
    fn seed_enrolled(&self) {
        enroll::write_instance(
            &self.fs,
            &self.layout(),
            &enroll::Instance {
                schema_version: 1,
                base_url: API.to_owned(),
            },
        )
        .unwrap();
        let mut user = enroll::UserDoc {
            schema_version: 1,
            principal: Some("alice@acme.com".to_owned()),
            workspaces: Vec::new(),
        };
        enroll::upsert_membership(
            &mut user,
            enroll::Membership {
                workspace_id: WS.to_owned(),
                name: "acme".to_owned(),
                display_name: "Acme Inc".to_owned(),
                enrolled_at: 1,
                link_status: enroll::LINK_ACTIVE.to_owned(),
            },
        );
        enroll::write_user(&self.fs, &self.layout(), &user).unwrap();
        enroll::write_credentials(&self.fs, &self.layout(), "devc_old", "dev_old").unwrap();
    }
}

type CallLog = Arc<Mutex<Vec<String>>>;

/// A scriptable enroll fake (the same shape as the follow suite's).
#[derive(Clone)]
struct FakeEnroll {
    log: CallLog,
    polls: Arc<Mutex<VecDeque<DeviceAuthPoll>>>,
}
impl FakeEnroll {
    fn new(log: CallLog, polls: Vec<DeviceAuthPoll>) -> Self {
        Self {
            log,
            polls: Arc::new(Mutex::new(polls.into())),
        }
    }
    fn granted() -> DeviceAuthPoll {
        DeviceAuthPoll::Granted(EnrolledGrant {
            hint: None,
            link_status: crate::plane::LinkStatus::Active,
            credential: "devc_new".into(),
            session_id: None,
            device_id: "dev_new".into(),
            workspace: EnrolledWorkspace {
                workspace_id: WS.into(),
                name: "acme".into(),
                display_name: "Acme Inc".into(),
            },
        })
    }
}
impl EnrollSource for FakeEnroll {
    fn fetch_card(
        &self,
        url: &str,
    ) -> Result<topos_types::requests::WireProtocolCard, ClientError> {
        self.log.lock().unwrap().push(format!("card {url}"));
        Ok(topos_types::requests::WireProtocolCard {
            schema_version: 1,
            card: "topos-protocol-card".to_owned(),
            api_base_url: API.to_owned(),
        })
    }
    fn device_auth_start(
        &self,
        workspace: &str,
        _requested_name: &str,
        _invite_token: Option<&str>,
    ) -> Result<DeviceAuthStart, ClientError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("authorize {workspace}"));
        Ok(DeviceAuthStart {
            device_code: "dc_login".into(),
            user_code: "LOGN-1234".into(),
            verification_uri: format!("{API}/verify"),
            expires_in_secs: 900,
            interval_secs: 7,
        })
    }
    fn device_auth_poll(&self, device_code: &str) -> Result<DeviceAuthPoll, ClientError> {
        assert_eq!(device_code, "dc_login");
        self.log.lock().unwrap().push("poll".to_owned());
        let mut polls = self.polls.lock().unwrap();
        if polls.len() > 1 {
            Ok(polls.pop_front().expect("scripted"))
        } else {
            Ok(polls.front().expect("scripted").clone())
        }
    }
}

/// A directory whose `me` answers are scripted per workspace: `Ok(role)`, the uniform 404, or a
/// transport fault — the `auth status` probe matrix.
#[derive(Clone, Default)]
struct FakeDirectory {
    gone: bool,
    unreachable: bool,
}
impl DirectorySource for FakeDirectory {
    fn me(&self, ws: &str) -> Result<WireMe, ClientError> {
        if self.gone {
            return Err(ClientError::TargetNotFound {
                target: ws.to_owned(),
            });
        }
        if self.unreachable {
            return Err(ClientError::Plane("connect refused".into()));
        }
        Ok(WireMe {
            workspace_id: ws.to_owned(),
            name: "acme".into(),
            display_name: "Acme Inc".into(),
            address: "https://topos.sh/acme".into(),
            principal: "alice@acme.com".into(),
            role: "owner".into(),
            invited_by: None,
            link_status: "active".into(),
        })
    }
    fn channels_index(&self, _ws: &str) -> Result<WireChannelIndex, ClientError> {
        unreachable!()
    }
    fn skills_index(&self, _ws: &str) -> Result<WireSkillIndex, ClientError> {
        unreachable!()
    }
    fn proposals_index(&self, _ws: &str) -> Result<WireProposalIndex, ClientError> {
        unreachable!()
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

/// A governance fake recording the ONE global self-revoke; `fail` makes it a transport fault
/// (logout stays best-effort); `already_revoked` answers the uniform 404 (= already signed out).
#[derive(Clone)]
struct FakeGovernance {
    log: CallLog,
    fail: bool,
    already_revoked: bool,
}
impl GovernanceSource for FakeGovernance {
    fn invite(&self, _ws: &str, _body: InvitationRequest) -> Result<InvitationData, ClientError> {
        unreachable!("auth never invites")
    }
    fn revoke_device(&self) -> Result<(), ClientError> {
        self.log.lock().unwrap().push("revoke device".to_owned());
        if self.fail {
            Err(ClientError::Plane("connect refused".into()))
        } else if self.already_revoked {
            Err(ClientError::TargetNotFound {
                target: "device".to_owned(),
            })
        } else {
            Ok(())
        }
    }
}

struct AuthFakes {
    enroll: FakeEnroll,
    directory: FakeDirectory,
    governance: FakeGovernance,
}

fn with_connectors<R>(
    rig: &Rig,
    fakes: &AuthFakes,
    f: impl FnOnce(&Ctx<'_>, &ops::AuthConnectors<'_>) -> R,
) -> R {
    sidecar::recover(&rig.fs, &rig.layout(), FIXED_MILLIS as i64).unwrap();
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let enroll_fake = fakes.enroll.clone();
    let dir_fake = fakes.directory.clone();
    let gov_fake = fakes.governance.clone();
    let enroll_connect = move |_b: &str| -> Box<dyn EnrollSource> { Box::new(enroll_fake.clone()) };
    let dir_connect = move |_b: &str| -> Box<dyn DirectorySource> { Box::new(dir_fake.clone()) };
    let gov_connect = move |_b: &str| -> Box<dyn GovernanceSource> { Box::new(gov_fake.clone()) };
    let connectors = ops::AuthConnectors {
        enroll: &enroll_connect,
        directory: &dir_connect,
        governance: &gov_connect,
        web_origin: "https://topos.sh".to_owned(),
    };
    f(&ctx, &connectors)
}

fn fakes(log: &CallLog, polls: Vec<DeviceAuthPoll>) -> AuthFakes {
    AuthFakes {
        enroll: FakeEnroll::new(log.clone(), polls),
        directory: FakeDirectory::default(),
        governance: FakeGovernance {
            log: log.clone(),
            fail: false,
            already_revoked: false,
        },
    }
}

// =================================================================================================
// login
// =================================================================================================

#[test]
fn login_needs_an_enrolled_membership() {
    let rig = Rig::new("login-fresh");
    let log: CallLog = Arc::default();
    let fk = fakes(&log, vec![FakeEnroll::granted()]);
    let err = with_connectors(&rig, &fk, |ctx, c| ops::login(ctx, c, None, None)).unwrap_err();
    assert!(matches!(err, ClientError::Enrollment(_)), "{err:?}");
    assert!(err.to_string().contains("topos follow"), "{err}");
    assert!(log.lock().unwrap().is_empty(), "nothing dialed");
}

#[test]
fn login_begins_toward_the_membership_address_and_owns_a_login_wal() {
    let rig = Rig::new("login-begin");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let fk = fakes(&log, vec![DeviceAuthPoll::Pending]);
    let out = with_connectors(&rig, &fk, |ctx, c| ops::login(ctx, c, None, None)).unwrap();
    let ops::AuthLoginOutcome::Pending(p) = out else {
        panic!("call 1 is pending");
    };
    assert_eq!(p.server, API);
    assert_eq!(p.user_code, "LOGN-1234");
    assert_eq!(p.interval_secs, 7);
    // The flow authorized toward the MEMBERSHIP's address name.
    assert!(log.lock().unwrap().iter().any(|e| e == "authorize acme"));
    // The WAL is LOGIN-owned (a `follow` refuses it; `auth login` resumes it).
    let wal = enroll::read_wal(&rig.fs, &rig.layout()).unwrap().unwrap();
    assert!(matches!(wal.intent, enroll::EnrollIntentDoc::Login));
}

#[test]
fn login_granted_replaces_the_one_credential_wholesale() {
    let rig = Rig::new("login-granted");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let fk = fakes(&log, vec![FakeEnroll::granted()]);
    with_connectors(&rig, &fk, |ctx, c| ops::login(ctx, c, None, None)).unwrap();

    // Re-invoking `auth login` IS the resume: poll → granted → the credential replaces wholesale.
    let out = with_connectors(&rig, &fk, |ctx, c| ops::login(ctx, c, None, None)).unwrap();
    let ops::AuthLoginOutcome::Done(done) = out else {
        panic!("the resume settles");
    };
    assert_eq!(done.workspace_id, WS);
    assert_eq!(done.device_id, "dev_new");
    let creds = enroll::read_credentials(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert_eq!(creds.credential, "devc_new", "replaced wholesale");
    assert_eq!(creds.device_id, "dev_new");
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
}

#[test]
fn login_denied_and_expired_clear_the_wal_typed() {
    for (poll, needle) in [
        (DeviceAuthPoll::Denied, "denied"),
        (DeviceAuthPoll::Expired, "expired"),
    ] {
        let rig = Rig::new("login-terminal");
        rig.seed_enrolled();
        let log: CallLog = Arc::default();
        let fk = fakes(&log, vec![poll]);
        with_connectors(&rig, &fk, |ctx, c| ops::login(ctx, c, None, None)).unwrap();
        let err = with_connectors(&rig, &fk, |ctx, c| ops::login(ctx, c, None, None)).unwrap_err();
        assert!(matches!(err, ClientError::Enrollment(_)), "{err:?}");
        assert!(err.to_string().contains(needle), "{err}");
        assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
        // The OLD credential survives a failed re-login (nothing was replaced).
        let creds = enroll::read_credentials(&rig.fs, &rig.layout())
            .unwrap()
            .unwrap();
        assert_eq!(creds.credential, "devc_old");
    }
}

#[test]
fn a_follow_owned_wal_refuses_toward_follow() {
    let rig = Rig::new("login-follow-owned");
    rig.seed_enrolled();
    enroll::write_wal(
        &rig.fs,
        &rig.layout(),
        &enroll::PendingEnrollment {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            host: String::new(),
            base_url: API.to_owned(),
            workspace_name: "acme".to_owned(),
            intent: enroll::EnrollIntentDoc::Follow {
                target: None,
                mode: enroll::FollowModeDoc::Auto,
            },
            device_code: "dc_follow".to_owned(),
            user_code: "CODE".to_owned(),
            verification_uri: format!("{API}/verify"),
            interval_secs: 5,
            expires_at_millis: i64::MAX,
        },
    )
    .unwrap();
    let log: CallLog = Arc::default();
    let fk = fakes(&log, vec![FakeEnroll::granted()]);
    let err = with_connectors(&rig, &fk, |ctx, c| ops::login(ctx, c, None, None)).unwrap_err();
    assert!(err.to_string().contains("topos follow"), "{err}");
    assert!(
        log.lock().unwrap().is_empty(),
        "the follow flow's secret was never touched"
    );
}

// =================================================================================================
// logout
// =================================================================================================

#[test]
fn logout_is_two_phase_and_runs_the_one_global_revoke() {
    let rig = Rig::new("logout");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let fk = fakes(&log, vec![DeviceAuthPoll::Pending]);

    // Bare = DESCRIBE (nothing changes).
    let out = with_connectors(&rig, &fk, |ctx, c| ops::logout(ctx, c, false)).unwrap();
    let ops::AuthLogoutOutcome::Described { describe, yes_argv } = out else {
        panic!("bare logout describes");
    };
    assert_eq!(describe.principal.as_deref(), Some("alice@acme.com"));
    assert_eq!(describe.workspaces, vec![WS.to_owned()]);
    assert_eq!(yes_argv.last().map(String::as_str), Some("--yes"));
    assert!(
        enroll::read_credentials(&rig.fs, &rig.layout())
            .unwrap()
            .is_some(),
        "nothing changed on the describe"
    );

    // `--yes` = ONE global self-revoke (`DELETE /v1/device`) + the credential delete.
    let out = with_connectors(&rig, &fk, |ctx, c| ops::logout(ctx, c, true)).unwrap();
    let ops::AuthLogoutOutcome::Applied(applied) = out else {
        panic!("--yes applies");
    };
    assert!(applied.credentials_deleted);
    assert!(applied.revoked, "the one global revoke landed");
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|e| e.as_str() == "revoke device")
            .count(),
        1,
        "exactly ONE global revoke — the per-workspace loop is gone: {:?}",
        log.lock().unwrap()
    );
    // Signed out = no credential; the memberships stay for the re-login UX.
    assert!(
        enroll::read_credentials(&rig.fs, &rig.layout())
            .unwrap()
            .is_none()
    );
    assert!(
        enroll::read_user(&rig.fs, &rig.layout())
            .unwrap()
            .unwrap()
            .membership(WS)
            .is_some()
    );

    // A second logout is idempotent (already signed out — nothing to revoke or delete).
    let out = with_connectors(&rig, &fk, |ctx, c| ops::logout(ctx, c, true)).unwrap();
    let ops::AuthLogoutOutcome::Applied(applied) = out else {
        panic!("--yes applies");
    };
    assert!(!applied.credentials_deleted);
    assert!(!applied.revoked, "no credential ⇒ nothing dialed");
}

#[test]
fn logout_treats_the_uniform_404_as_already_revoked() {
    // The design fact: after a server-side revoke, a retry answers the uniform 404 — logout treats
    // that as already-signed-out and still proceeds with the local delete, reporting revoked.
    let rig = Rig::new("logout-already-revoked");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let mut fk = fakes(&log, vec![DeviceAuthPoll::Pending]);
    fk.governance.already_revoked = true;
    let out = with_connectors(&rig, &fk, |ctx, c| ops::logout(ctx, c, true)).unwrap();
    let ops::AuthLogoutOutcome::Applied(applied) = out else {
        panic!("--yes applies");
    };
    assert!(applied.revoked, "the uniform 404 = already revoked");
    assert!(applied.credentials_deleted);
}

#[test]
fn logout_deletes_the_credential_even_when_the_revoke_fails() {
    let rig = Rig::new("logout-besteffort");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let mut fk = fakes(&log, vec![DeviceAuthPoll::Pending]);
    fk.governance.fail = true;
    let out = with_connectors(&rig, &fk, |ctx, c| ops::logout(ctx, c, true)).unwrap();
    let ops::AuthLogoutOutcome::Applied(applied) = out else {
        panic!("--yes applies");
    };
    assert!(
        applied.credentials_deleted,
        "the local sign-out never blocks on the network"
    );
    assert!(!applied.revoked, "the transport fault is reported honestly");
    assert!(
        enroll::read_credentials(&rig.fs, &rig.layout())
            .unwrap()
            .is_none()
    );
}

// =================================================================================================
// status
// =================================================================================================

#[test]
fn status_probes_me_and_reports_the_access_causes() {
    // Healthy: the probe answers the role AND refreshes the principal disclosure.
    let rig = Rig::new("status-healthy");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let fk = fakes(&log, vec![DeviceAuthPoll::Pending]);
    let s = with_connectors(&rig, &fk, ops::status).unwrap();
    assert!(s.signed_in);
    assert_eq!(s.server.as_deref(), Some(API));
    assert_eq!(s.principal.as_deref(), Some("alice@acme.com"));
    assert_eq!(s.device_id.as_deref(), Some("dev_old"));
    assert_eq!(s.workspaces.len(), 1);
    assert_eq!(s.workspaces[0].health, "healthy");
    assert_eq!(s.workspaces[0].role.as_deref(), Some("owner"));

    // The uniform 404: this device (or the person) lost the workspace.
    let mut fk_gone = fakes(&log, vec![DeviceAuthPoll::Pending]);
    fk_gone.directory.gone = true;
    let s = with_connectors(&rig, &fk_gone, ops::status).unwrap();
    assert_eq!(
        s.workspaces[0].health,
        "no access — unlinked, removed, or gone"
    );

    // A transport fault: unreachable, never a false "revoked".
    let mut fk_down = fakes(&log, vec![DeviceAuthPoll::Pending]);
    fk_down.directory.unreachable = true;
    let s = with_connectors(&rig, &fk_down, ops::status).unwrap();
    assert_eq!(s.workspaces[0].health, "unreachable");

    // Signed out: no credential — the probe never dials.
    std::fs::remove_file(rig.layout().credentials_path()).unwrap();
    let s = with_connectors(&rig, &fk, ops::status).unwrap();
    assert!(!s.signed_in);
    assert_eq!(s.workspaces[0].health, "no credential");
    assert!(s.device_id.is_none());
}
