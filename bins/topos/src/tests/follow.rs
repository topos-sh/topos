//! The `follow <workspace-address>` device-authorization flow over fakes (no HTTP): call 1 (card →
//! re-root → authorize → the pending WAL), the resume idiom (re-invoking IS the resume) across every
//! poll arm (pending / denied / expired / granted), the ONE-credential persist, the WAL ownership
//! split against `auth login`, and the retired-`/i/` refusal. The two-phase subscribe the granted
//! flow continues into is covered in the subscribe suite.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::requests::{
    WireChannelIndex, WireMe, WireProposalIndex, WireReach, WireSkillIndex, WireSkillLog,
};
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::enroll;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    DeliverySnapshot, DeliverySource, DeviceAuthPoll, DeviceAuthStart, DirectorySource,
    EnrollSource, EnrolledGrant, EnrolledWorkspace, FetchedVersion, InertFollow, InertPlane,
    KnownCurrent, PlaneError, PlaneSource, PointerFetch, ReconcileTransport,
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
        let dir = std::env::temp_dir().join(format!("topos-flw-{tag}-{}-{n}", std::process::id()));
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

/// A harness that recognizes nothing but RECORDS the auto-update-trigger arm (the enrollment persist
/// arms the session-start hook for a pure follower).
struct RecordingHarness {
    armed: AtomicU32,
}
impl RecordingHarness {
    fn new() -> Self {
        Self {
            armed: AtomicU32::new(0),
        }
    }
}
impl HarnessAdapter for RecordingHarness {
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
        CurrencyKind::SessionStart
    }
    fn install_currency_trigger(&self) -> TriggerReport {
        self.armed.fetch_add(1, Ordering::Relaxed);
        TriggerReport {
            harness: HarnessId::ClaudeCode,
            currency_kind: CurrencyKind::SessionStart,
            touched_path: None,
            marker_id: "test".into(),
            state: TriggerState::Active,
        }
    }
    fn remove_currency_trigger(&self) -> TriggerReport {
        TriggerReport {
            harness: HarnessId::ClaudeCode,
            currency_kind: CurrencyKind::SessionStart,
            touched_path: None,
            marker_id: "test".into(),
            state: TriggerState::Inactive,
        }
    }
    fn uninstall_footprint(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

struct Rig {
    home: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: RecordingHarness,
}
impl Rig {
    fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(tag),
            fs: RealFs,
            ids: SeqIds::new("t"),
            clock: FixedClock(FIXED_MILLIS),
            harness: RecordingHarness::new(),
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
}

type CallLog = Arc<Mutex<Vec<String>>>;

/// A scriptable enroll fake: the constant card, one recorded `device_auth_start`, and a scripted
/// queue of poll answers (the LAST script entry repeats — a granted flow re-answers the same grant).
#[derive(Clone)]
struct FakeEnroll {
    api_base: String,
    log: CallLog,
    polls: Arc<Mutex<VecDeque<DeviceAuthPoll>>>,
}
impl FakeEnroll {
    fn new(log: CallLog, polls: Vec<DeviceAuthPoll>) -> Self {
        Self {
            api_base: API.to_owned(),
            log,
            polls: Arc::new(Mutex::new(polls.into())),
        }
    }
    fn granted() -> DeviceAuthPoll {
        DeviceAuthPoll::Granted(EnrolledGrant {
            hint: None,
            credential: "devc_secret".into(),
            device_id: "dev_1".into(),
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
            api_base_url: self.api_base.clone(),
        })
    }
    fn device_auth_start(
        &self,
        workspace: &str,
        requested_name: &str,
        invite_token: Option<&str>,
    ) -> Result<DeviceAuthStart, ClientError> {
        self.log.lock().unwrap().push(match invite_token {
            // The log records the token VERBATIM so the suite can assert exactly what rode the
            // wire (a fake — nothing is secret here).
            Some(token) => format!("authorize {workspace} as {requested_name} + invite {token}"),
            None => format!("authorize {workspace} as {requested_name}"),
        });
        Ok(DeviceAuthStart {
            device_code: "dc_secret".into(),
            user_code: "WXYZ-1234".into(),
            verification_uri: format!("{}/verify", self.api_base),
            expires_in_secs: 900,
            interval_secs: 5,
        })
    }
    fn device_auth_poll(&self, device_code: &str) -> Result<DeviceAuthPoll, ClientError> {
        assert_eq!(device_code, "dc_secret", "the poll presents the WAL's code");
        self.log.lock().unwrap().push("poll".to_owned());
        let mut polls = self.polls.lock().unwrap();
        if polls.len() > 1 {
            Ok(polls.pop_front().expect("scripted"))
        } else {
            // The LAST answer repeats — an approved flow re-answers the same grant on a re-poll.
            Ok(polls.front().expect("scripted").clone())
        }
    }
}

/// A minimal directory serving the one-workspace universe the granted continuation describes over.
#[derive(Clone)]
struct FakeDirectory;
impl DirectorySource for FakeDirectory {
    fn me(&self, _ws: &str) -> Result<WireMe, ClientError> {
        Ok(WireMe {
            workspace_id: WS.into(),
            name: "acme".into(),
            display_name: "Acme Inc".into(),
            address: "https://topos.sh/acme".into(),
            principal: "alice@acme.com".into(),
            role: "member".into(),
            invited_by: None,
        })
    }
    fn channels_index(&self, _ws: &str) -> Result<WireChannelIndex, ClientError> {
        Ok(WireChannelIndex {
            channels: Vec::new(),
        })
    }
    fn skills_index(&self, _ws: &str) -> Result<WireSkillIndex, ClientError> {
        Ok(WireSkillIndex { skills: Vec::new() })
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

/// An empty reconcile transport (the granted continuation's describe reads one delivery snapshot).
#[derive(Clone)]
struct EmptyTransport;
impl PlaneSource for EmptyTransport {
    fn get_current(&self, _s: &str, _k: Option<KnownCurrent>) -> Result<PointerFetch, PlaneError> {
        Err(PlaneError::NotFound)
    }
    fn fetch_version(&self, _s: &str, _v: [u8; 32]) -> Result<FetchedVersion, PlaneError> {
        Err(PlaneError::NotFound)
    }
}
impl DeliverySource for EmptyTransport {
    fn workspaces(&self) -> Vec<String> {
        vec![WS.to_owned()]
    }
    fn fetch_delivery(&self, _ws: &str) -> Result<DeliverySnapshot, PlaneError> {
        Ok(DeliverySnapshot {
            skills: Vec::new(),
            detached: Vec::new(),
            excluded: Vec::new(),
            proposals_awaiting: 0,
            notices: Vec::new(),
            staleness_window_ms: 604_800_000,
        })
    }
    fn report_applied(&self, _ws: &str, _a: &[(String, [u8; 32])]) -> Result<(), PlaneError> {
        Ok(())
    }
}

fn run_follow(
    rig: &Rig,
    enroll_fake: &FakeEnroll,
    targets: Vec<String>,
    opts: ops::FollowOpts,
) -> Result<ops::FollowOutcome, ClientError> {
    crate::identity::load_or_create_device_id(&rig.fs, &rig.layout()).unwrap();
    // The start-of-command recovery sweep runs first in production; mirror it so an expired WAL is
    // reaped exactly as the binary would.
    sidecar::recover(&rig.fs, &rig.layout(), FIXED_MILLIS as i64).unwrap();
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let enroll_connect = |_b: &str| -> Box<dyn EnrollSource> { Box::new(enroll_fake.clone()) };
    let dir_connect = |_b: &str| -> Box<dyn DirectorySource> { Box::new(FakeDirectory) };
    let del_connect = |_b: &str| -> Box<dyn ReconcileTransport> { Box::new(EmptyTransport) };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        directory: &dir_connect,
        delivery: &del_connect,
        web_origin: "https://topos.sh".to_owned(),
        // The classic suites consent to a bareword enroll by construction (the guard has its
        // own suite).
        confirm_bareword: &|_, _| ops::BarewordDecision::Proceed,
    };
    ops::follow(&ctx, &connectors, targets, opts)
}

fn opts(yes: bool) -> ops::FollowOpts {
    ops::FollowOpts {
        manual: false,
        workspace: None,
        yes,
        prefix_dirname: false,
        channels: Vec::new(),
        skills: Vec::new(),
        agents: Vec::new(),
    }
}

// =================================================================================================
// The retired `/i/` door.
// =================================================================================================

#[test]
fn an_i_link_is_refused_typed_toward_the_workspace_address() {
    let rig = Rig::new("i-refusal");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![FakeEnroll::granted()]);
    let err = run_follow(
        &rig,
        &enroll_fake,
        vec!["https://topos.sh/i/tok_secret".to_owned()],
        opts(false),
    )
    .unwrap_err();
    assert!(matches!(err, ClientError::Enrollment(_)), "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains("invite links are retired"), "{msg}");
    assert!(msg.contains("topos follow"), "{msg}");
    // Nothing dialed, nothing persisted — the refusal is purely local.
    assert!(log.lock().unwrap().is_empty());
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
}

// =================================================================================================
// Call 1 — begin: card → re-root → authorize → the WAL + the pending disclosure.
// =================================================================================================

#[test]
fn begin_writes_the_single_phase_wal_and_discloses_the_pending_url() {
    let rig = Rig::new("begin");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![DeviceAuthPoll::Pending]);

    let out = run_follow(&rig, &enroll_fake, vec!["acme".to_owned()], opts(false)).unwrap();
    let ops::FollowOutcome::Data { data, .. } = out else {
        panic!("call 1 answers the pending wire payload");
    };
    assert!(!data.enrolled);
    let pending = data.pending.expect("a pending device flow");
    // The BARE approval page — the code rides its own field, never a URL.
    assert_eq!(pending.verification_uri, format!("{API}/verify"));
    assert_eq!(pending.user_code, "WXYZ-1234");
    assert_eq!(pending.interval_secs, Some(5));
    assert_eq!(data.plane_base_url.as_deref(), Some(API), "re-rooted");

    {
        let l = log.lock().unwrap();
        assert!(
            l.iter().any(|e| e == "card https://topos.sh/acme"),
            "the card was fetched at the workspace's own address: {l:?}"
        );
        assert!(
            l.iter()
                .any(|e| e.starts_with("authorize acme as topos CLI")),
            "the start names the workspace ADDRESS + a host-derived device name: {l:?}"
        );
    }

    // The WAL holds the flow (ONE phase — no fence phases exist) with the follow intent recorded.
    let wal = enroll::read_wal(&rig.fs, &rig.layout())
        .unwrap()
        .expect("the WAL is on disk");
    assert_eq!(wal.base_url, API);
    assert_eq!(wal.workspace_name, "acme");
    assert_eq!(wal.device_code, "dc_secret");
    assert!(matches!(
        wal.intent,
        enroll::EnrollIntentDoc::Follow {
            target: Some(enroll::FollowTargetDoc {
                kind: enroll::FollowKindDoc::Workspace,
                ..
            }),
            mode: enroll::FollowModeDoc::Auto,
        }
    ));
    // Nothing else persisted before approval.
    assert!(
        enroll::read_credentials(&rig.fs, &rig.layout())
            .unwrap()
            .is_none()
    );
    assert!(
        enroll::read_instance(&rig.fs, &rig.layout())
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_manual_begin_records_confirm_each_in_the_wal() {
    let rig = Rig::new("begin-manual");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log, vec![DeviceAuthPoll::Pending]);
    let mut o = opts(false);
    o.manual = true;
    run_follow(&rig, &enroll_fake, vec!["acme".to_owned()], o).unwrap();
    let wal = enroll::read_wal(&rig.fs, &rig.layout()).unwrap().unwrap();
    assert!(matches!(
        wal.intent,
        enroll::EnrollIntentDoc::Follow {
            mode: enroll::FollowModeDoc::ConfirmEach,
            ..
        }
    ));
}

// =================================================================================================
// The resume idiom — re-invoking `follow` polls once and routes each arm.
// =================================================================================================

#[test]
fn resume_pending_re_emits_the_persisted_url_without_restarting() {
    let rig = Rig::new("resume-pending");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![DeviceAuthPoll::Pending]);
    run_follow(&rig, &enroll_fake, vec!["acme".to_owned()], opts(false)).unwrap();

    // Re-invoking (with any target, or none) RESUMES — it polls; it never starts a second flow.
    let out = run_follow(&rig, &enroll_fake, Vec::new(), opts(false)).unwrap();
    let ops::FollowOutcome::Data { data, .. } = out else {
        panic!("a pending resume re-emits the wire payload");
    };
    let pending = data.pending.expect("still pending");
    assert_eq!(
        pending.verification_uri,
        format!("{API}/verify"),
        "the SERVER-built URL is re-emitted verbatim from the WAL"
    );
    let l = log.lock().unwrap();
    assert_eq!(
        l.iter().filter(|e| e.starts_with("authorize")).count(),
        1,
        "ONE start; the resume only polls: {l:?}"
    );
    assert_eq!(l.iter().filter(|e| *e == "poll").count(), 1);
}

#[test]
fn resume_denied_clears_the_wal_and_surfaces_the_ask_an_owner_guidance() {
    let rig = Rig::new("resume-denied");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log, vec![DeviceAuthPoll::Denied]);
    run_follow(&rig, &enroll_fake, vec!["acme".to_owned()], opts(false)).unwrap();

    let err = run_follow(&rig, &enroll_fake, Vec::new(), opts(false)).unwrap_err();
    assert!(matches!(err, ClientError::EnrollDenied), "{err:?}");
    assert_eq!(err.code(), "DENIED");
    assert!(err.to_string().contains("topos invite <your-email>"));
    // The dead flow is swept — the next follow starts fresh.
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
}

#[test]
fn resume_expired_clears_the_wal_typed() {
    let rig = Rig::new("resume-expired");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log, vec![DeviceAuthPoll::Expired]);
    run_follow(&rig, &enroll_fake, vec!["acme".to_owned()], opts(false)).unwrap();

    let err = run_follow(&rig, &enroll_fake, Vec::new(), opts(false)).unwrap_err();
    assert!(matches!(err, ClientError::Enrollment(_)), "{err:?}");
    assert!(err.to_string().contains("expired"), "{err}");
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
}

#[test]
fn resume_granted_persists_the_one_credential_and_continues_into_the_describe() {
    let rig = Rig::new("resume-granted");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![FakeEnroll::granted()]);
    run_follow(&rig, &enroll_fake, vec!["acme".to_owned()], opts(false)).unwrap();

    let out = run_follow(&rig, &enroll_fake, Vec::new(), opts(false)).unwrap();
    // The granted resume CONTINUES into the recorded follow intent's DESCRIBE (bare = consent still
    // pending; the enrollment itself already persisted).
    let ops::FollowOutcome::Described { describe, .. } = out else {
        panic!("a granted bare resume lands on the describe, got another outcome");
    };
    assert!(describe.enrolled_now);
    assert_eq!(describe.workspace.workspace_id, WS);

    // The ONE credential + the registered device id (0600, replaced wholesale).
    let creds = enroll::read_credentials(&rig.fs, &rig.layout())
        .unwrap()
        .expect("the credential persisted");
    assert_eq!(creds.credential, "devc_secret");
    assert_eq!(creds.device_id, "dev_1");
    // The plane pin + the membership from the poll's AUTHORITATIVE workspace context.
    assert_eq!(
        enroll::read_instance(&rig.fs, &rig.layout())
            .unwrap()
            .unwrap()
            .base_url,
        API
    );
    let user = enroll::read_user(&rig.fs, &rig.layout()).unwrap().unwrap();
    let m = user.membership(WS).expect("the joined membership");
    assert_eq!(m.name, "acme");
    assert_eq!(m.display_name, "Acme Inc");
    // The WAL is gone (the last durable step) and the auto-update hook armed.
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
    assert!(rig.harness.armed.load(Ordering::Relaxed) >= 1);
}

#[test]
fn a_second_workspace_grant_replaces_the_credential_and_adds_a_membership() {
    let rig = Rig::new("second-ws");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![FakeEnroll::granted()]);
    // First enrollment (granted immediately on the resume poll).
    run_follow(&rig, &enroll_fake, vec!["acme".to_owned()], opts(false)).unwrap();
    run_follow(&rig, &enroll_fake, Vec::new(), opts(false)).unwrap();

    // A second workspace on the SAME plane: a fresh flow whose grant carries a NEW credential.
    let second = FakeEnroll {
        polls: Arc::new(Mutex::new(
            vec![DeviceAuthPoll::Granted(EnrolledGrant {
                hint: None,
                credential: "devc_two".into(),
                device_id: "dev_2".into(),
                workspace: EnrolledWorkspace {
                    workspace_id: "w_beta".into(),
                    name: "beta".into(),
                    display_name: "Beta".into(),
                },
            })]
            .into(),
        )),
        ..FakeEnroll::new(log, Vec::new())
    };
    run_follow(&rig, &second, vec!["beta".to_owned()], opts(false)).unwrap();
    run_follow(&rig, &second, Vec::new(), opts(false)).unwrap();

    // The device holds exactly ONE credential (replaced wholesale)…
    let creds = enroll::read_credentials(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert_eq!(creds.credential, "devc_two");
    assert_eq!(creds.device_id, "dev_2");
    // …while the memberships ACCUMULATE (a second follow never drops the first).
    let user = enroll::read_user(&rig.fs, &rig.layout()).unwrap().unwrap();
    assert!(user.membership(WS).is_some());
    assert!(user.membership("w_beta").is_some());
}

#[test]
fn a_login_owned_wal_refuses_toward_auth_login() {
    let rig = Rig::new("login-owned");
    // A pending LOGIN flow owns the shared WAL slot.
    enroll::write_wal(
        &rig.fs,
        &rig.layout(),
        &enroll::PendingEnrollment {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            base_url: API.to_owned(),
            workspace_name: "acme".to_owned(),
            intent: enroll::EnrollIntentDoc::Login,
            device_code: "dc_login".to_owned(),
            user_code: "CODE".to_owned(),
            verification_uri: format!("{API}/verify"),
            interval_secs: 5,
            expires_at_millis: i64::MAX,
        },
    )
    .unwrap();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![FakeEnroll::granted()]);
    let err = run_follow(&rig, &enroll_fake, vec!["acme".to_owned()], opts(false)).unwrap_err();
    assert!(matches!(err, ClientError::Enrollment(_)), "{err:?}");
    assert!(err.to_string().contains("auth login"), "{err}");
    // The login flow's secret was never touched (no poll, no restart).
    assert!(log.lock().unwrap().is_empty());
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_some());
}

#[test]
fn the_recovery_sweep_reaps_an_expired_wal_so_follow_starts_fresh() {
    let rig = Rig::new("sweep");
    // An EXPIRED flow (the fixed clock sits far past it).
    enroll::write_wal(
        &rig.fs,
        &rig.layout(),
        &enroll::PendingEnrollment {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            base_url: API.to_owned(),
            workspace_name: "acme".to_owned(),
            intent: enroll::EnrollIntentDoc::Follow {
                target: None,
                mode: enroll::FollowModeDoc::Auto,
            },
            device_code: "dc_dead".to_owned(),
            user_code: "DEAD".to_owned(),
            verification_uri: format!("{API}/verify"),
            interval_secs: 5,
            expires_at_millis: 1_000,
        },
    )
    .unwrap();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![DeviceAuthPoll::Pending]);
    // The sweep (run_follow mirrors the start-of-command recovery) reaps the dead WAL, so this
    // begins a FRESH flow instead of polling the dead code.
    let out = run_follow(&rig, &enroll_fake, vec!["acme".to_owned()], opts(false)).unwrap();
    let ops::FollowOutcome::Data { data, .. } = out else {
        panic!("a fresh begin");
    };
    assert!(data.pending.is_some());
    let l = log.lock().unwrap();
    assert!(
        l.iter().any(|e| e.starts_with("authorize acme")),
        "a fresh start replaced the dead flow: {l:?}"
    );
    assert!(
        !l.iter().any(|e| e == "poll"),
        "the dead code was never polled"
    );
}

// =================================================================================================
// The bareword-enroll consent guard: on an UNENROLLED install a bare `follow <name>` (no slash —
// a workspace shorthand for the DEFAULT server) never starts a device flow silently. A TTY asks
// first (the composition root's prompt rides the confirm seam); `--yes` is the headless consent;
// an unconfirmable run refuses typed toward the two deliberate spellings. Enrolled installs and
// full addresses keep their exact prior behavior.
// =================================================================================================

/// [`run_follow`] with an injected consent answer (the guard's own seam).
fn run_follow_confirm(
    rig: &Rig,
    enroll_fake: &FakeEnroll,
    targets: Vec<String>,
    opts: ops::FollowOpts,
    confirm: &dyn Fn(&str, &str) -> ops::BarewordDecision,
) -> Result<ops::FollowOutcome, ClientError> {
    crate::identity::load_or_create_device_id(&rig.fs, &rig.layout()).unwrap();
    sidecar::recover(&rig.fs, &rig.layout(), FIXED_MILLIS as i64).unwrap();
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let enroll_connect = |_b: &str| -> Box<dyn EnrollSource> { Box::new(enroll_fake.clone()) };
    let dir_connect = |_b: &str| -> Box<dyn DirectorySource> { Box::new(FakeDirectory) };
    let del_connect = |_b: &str| -> Box<dyn ReconcileTransport> { Box::new(EmptyTransport) };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        directory: &dir_connect,
        delivery: &del_connect,
        web_origin: "https://topos.sh".to_owned(),
        confirm_bareword: confirm,
    };
    ops::follow(&ctx, &connectors, targets, opts)
}

#[test]
fn a_headless_bareword_enroll_refuses_typed_with_both_spellings_and_dials_nothing() {
    let rig = Rig::new("bareword-headless");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![DeviceAuthPoll::Pending]);
    let err = run_follow_confirm(
        &rig,
        &enroll_fake,
        vec!["acme".to_owned()],
        opts(false),
        &|_, _| ops::BarewordDecision::Headless,
    )
    .unwrap_err();
    assert!(
        matches!(err, ClientError::BarewordEnrollUnconfirmed { .. }),
        "{err:?}"
    );
    assert_eq!(err.code(), "ENROLL_CONFIRM_REQUIRED");
    let msg = err.to_string();
    assert!(msg.contains("--yes"), "{msg}");
    assert!(msg.contains("https://topos.sh/acme"), "{msg}");
    // Nothing dialed, nothing persisted — the refusal fires BEFORE the card fetch.
    assert!(log.lock().unwrap().is_empty());
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
}

#[test]
fn a_declined_prompt_refuses_and_dials_nothing() {
    let rig = Rig::new("bareword-declined");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![DeviceAuthPoll::Pending]);
    let asked = std::sync::atomic::AtomicBool::new(false);
    let err = run_follow_confirm(
        &rig,
        &enroll_fake,
        vec!["acme".to_owned()],
        opts(false),
        &|name, server| {
            assert_eq!(name, "acme");
            assert_eq!(server, "https://topos.sh");
            asked.store(true, std::sync::atomic::Ordering::SeqCst);
            ops::BarewordDecision::Declined
        },
    )
    .unwrap_err();
    assert!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        "the prompt ran"
    );
    assert!(
        matches!(err, ClientError::BarewordEnrollDeclined { .. }),
        "{err:?}"
    );
    assert_eq!(err.code(), "ENROLL_CONFIRM_REQUIRED");
    assert!(err.to_string().contains("nothing changed"), "{}", err);
    assert!(log.lock().unwrap().is_empty());
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
}

#[test]
fn yes_is_the_headless_consent_and_a_full_address_never_prompts() {
    // `--yes` short-circuits the prompt entirely (a panicking confirm proves it is never asked).
    let rig = Rig::new("bareword-yes");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![DeviceAuthPoll::Pending]);
    let out = run_follow_confirm(
        &rig,
        &enroll_fake,
        vec!["acme".to_owned()],
        opts(true),
        &|_, _| panic!("--yes must never prompt"),
    )
    .unwrap();
    let ops::FollowOutcome::Data { data, .. } = out else {
        panic!("the consented bareword begins the flow");
    };
    assert!(data.pending.is_some());

    // A full `<server>/<workspace>` address is already explicit — no prompt either.
    let rig = Rig::new("address-no-prompt");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![DeviceAuthPoll::Pending]);
    let out = run_follow_confirm(
        &rig,
        &enroll_fake,
        vec!["https://topos.sh/acme".to_owned()],
        opts(false),
        &|_, _| panic!("a full address must never prompt"),
    )
    .unwrap();
    let ops::FollowOutcome::Data { data, .. } = out else {
        panic!("the address begins the flow");
    };
    assert!(data.pending.is_some());
}

#[test]
fn an_enrolled_install_keeps_its_prior_bareword_behavior() {
    // Enrolled (instance.json pinned): a bareword that matches nothing locally still begins the
    // device flow against the PINNED plane — the guard is for the fresh-machine default-server
    // surprise only.
    let rig = Rig::new("bareword-enrolled");
    enroll::write_instance(
        &rig.fs,
        &rig.layout(),
        &enroll::Instance {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            base_url: API.to_owned(),
        },
    )
    .unwrap();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![DeviceAuthPoll::Pending]);
    let out = run_follow_confirm(
        &rig,
        &enroll_fake,
        vec!["ghost".to_owned()],
        opts(false),
        &|_, _| panic!("an enrolled install must never prompt"),
    )
    .unwrap();
    let ops::FollowOutcome::Data { data, .. } = out else {
        panic!("the enrolled bareword begins the flow toward the pinned plane");
    };
    assert!(data.pending.is_some());
    let l = log.lock().unwrap();
    assert!(
        l.iter().any(|e| e.starts_with("authorize ghost")),
        "the flow ran against the pinned plane: {l:?}"
    );
}

// =================================================================================================
// The tokened invitation URL — `follow <invite-url>`: an unenrolled install starts the device flow
// CARRYING the token (the browser destination becomes the invitation page + the flow challenge);
// a granted flow's HINT steers the post-enrollment subscribe; an enrolled install accepts directly
// over the device lane with no browser at all.
// =================================================================================================

/// A directory whose catalog carries the hinted skill and whose `accept_invitation` is armed —
/// the invite-URL suites' universe (the classic [`FakeDirectory`] serves the plain flows).
#[derive(Clone)]
struct InviteDirectory {
    log: CallLog,
    hint: Option<crate::plane::GrantHint>,
}
impl DirectorySource for InviteDirectory {
    fn me(&self, ws: &str) -> Result<WireMe, ClientError> {
        FakeDirectory.me(ws)
    }
    fn accept_invitation(&self, token: &str) -> Result<crate::plane::InviteAccepted, ClientError> {
        self.log.lock().unwrap().push(format!("accept {token}"));
        Ok(crate::plane::InviteAccepted {
            workspace: EnrolledWorkspace {
                workspace_id: WS.into(),
                name: "acme".into(),
                display_name: "Acme Inc".into(),
            },
            hint: self.hint.clone(),
        })
    }
    fn channels_index(&self, ws: &str) -> Result<WireChannelIndex, ClientError> {
        FakeDirectory.channels_index(ws)
    }
    fn skills_index(&self, _ws: &str) -> Result<WireSkillIndex, ClientError> {
        Ok(WireSkillIndex {
            skills: vec![topos_types::requests::WireSkillIndexEntry {
                skill_id: "s_deploy".into(),
                name: "deploy".into(),
                kind: "skill".into(),
                display_name: None,
                status: "active".into(),
                version_id: "a".repeat(64),
                bundle_digest: "b".repeat(64),
                generation: 1,
                updated_at: 0,
                open_proposals: 0,
            }],
        })
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
        self.log.lock().unwrap().push("follow_skill".into());
        Ok(())
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

fn run_follow_invite(
    rig: &Rig,
    enroll_fake: &FakeEnroll,
    directory: &InviteDirectory,
    targets: Vec<String>,
    opts: ops::FollowOpts,
) -> Result<ops::FollowOutcome, ClientError> {
    crate::identity::load_or_create_device_id(&rig.fs, &rig.layout()).unwrap();
    sidecar::recover(&rig.fs, &rig.layout(), FIXED_MILLIS as i64).unwrap();
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let enroll_connect = |_b: &str| -> Box<dyn EnrollSource> { Box::new(enroll_fake.clone()) };
    let dir_connect = |_b: &str| -> Box<dyn DirectorySource> { Box::new(directory.clone()) };
    let del_connect = |_b: &str| -> Box<dyn ReconcileTransport> { Box::new(EmptyTransport) };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        directory: &dir_connect,
        delivery: &del_connect,
        web_origin: "https://topos.sh".to_owned(),
        confirm_bareword: &|_, _| panic!("an invite URL never prompts the bareword guard"),
    };
    ops::follow(&ctx, &connectors, targets, opts)
}

#[test]
fn an_invite_url_starts_the_flow_carrying_the_token_and_weaves_the_browser_destination() {
    let rig = Rig::new("inv-url-begin");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![DeviceAuthPoll::Pending]);
    let directory = InviteDirectory {
        log: log.clone(),
        hint: None,
    };
    let out = run_follow_invite(
        &rig,
        &enroll_fake,
        &directory,
        vec!["https://topos.sh/invite/tok_abc123".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Data { data, .. } = out else {
        panic!("an unenrolled invite URL answers the pending wire payload");
    };
    let pending = data.pending.expect("a pending device flow");
    // The browser destination is the INVITATION page (the one-visit weave), carrying the flow's
    // device-code CHALLENGE — never the code, never the server's bare /verify.
    let challenge = ops::device_challenge("dc_secret");
    assert_eq!(
        pending.verification_uri,
        format!("https://topos.sh/invite/tok_abc123?device={challenge}")
    );
    {
        let l = log.lock().unwrap();
        // The card probe hits the BARE origin — the token never rides a card fetch.
        assert!(l.iter().any(|e| e == "card https://topos.sh"), "{l:?}");
        // The token rode the authorize start (the flow row records it server-side); the
        // origin-rooted form names no workspace slug.
        assert!(
            l.iter()
                .any(|e| e.starts_with("authorize  as topos CLI")
                    && e.ends_with("+ invite tok_abc123")),
            "{l:?}"
        );
    }
    // The WAL records the weave destination; the multi-shape slug variant parses too.
    let wal = enroll::read_wal(&rig.fs, &rig.layout()).unwrap().unwrap();
    assert_eq!(wal.workspace_name, "");
    assert!(wal.verification_uri.contains("/invite/tok_abc123?device="));
}

#[test]
fn a_multi_tenant_invite_url_names_its_workspace_slug() {
    let rig = Rig::new("inv-url-multi");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![DeviceAuthPoll::Pending]);
    let directory = InviteDirectory {
        log: log.clone(),
        hint: None,
    };
    run_follow_invite(
        &rig,
        &enroll_fake,
        &directory,
        vec!["https://topos.sh/acme/invite/tok_xyz".to_owned()],
        opts(false),
    )
    .unwrap();
    let l = log.lock().unwrap();
    assert!(
        l.iter().any(
            |e| e.starts_with("authorize acme as topos CLI") && e.ends_with("+ invite tok_xyz")
        ),
        "{l:?}"
    );
}

#[test]
fn a_granted_invite_flow_continues_into_the_hinted_skill() {
    let rig = Rig::new("inv-url-hint");
    let log: CallLog = Arc::default();
    let granted_with_hint = DeviceAuthPoll::Granted(EnrolledGrant {
        hint: Some(crate::plane::GrantHint {
            kind: "skill".into(),
            name: "deploy".into(),
        }),
        credential: "devc_secret".into(),
        device_id: "dev_1".into(),
        workspace: EnrolledWorkspace {
            workspace_id: WS.into(),
            name: "acme".into(),
            display_name: "Acme Inc".into(),
        },
    });
    let enroll_fake = FakeEnroll::new(log.clone(), vec![granted_with_hint]);
    let directory = InviteDirectory {
        log: log.clone(),
        hint: None,
    };
    // Call 1: begin (pending WAL). The fake's scripted poll then grants on the resume.
    let pending_fake = FakeEnroll::new(log.clone(), vec![DeviceAuthPoll::Pending]);
    run_follow_invite(
        &rig,
        &pending_fake,
        &directory,
        vec!["https://topos.sh/invite/tok_hint".to_owned()],
        opts(false),
    )
    .unwrap();
    // Call 2: the resume persists the grant and continues INTO THE HINT — the describe targets
    // the invited-to skill, not the whole workspace.
    let out = run_follow_invite(&rig, &enroll_fake, &directory, Vec::new(), opts(false)).unwrap();
    let ops::FollowOutcome::Described { describe, .. } = out else {
        panic!("a granted resume continues into the two-phase describe, got {out:?}");
    };
    assert_eq!(describe.targets.len(), 1);
    assert_eq!(describe.targets[0].kind, "skill");
    assert_eq!(describe.targets[0].name, "deploy");
    assert!(describe.enrolled_now);
}

#[test]
fn an_enrolled_install_accepts_directly_and_continues_into_the_hint() {
    let rig = Rig::new("inv-url-direct2");
    let log: CallLog = Arc::default();
    let directory = InviteDirectory {
        log: log.clone(),
        hint: Some(crate::plane::GrantHint {
            kind: "skill".into(),
            name: "deploy".into(),
        }),
    };
    // Seed the enrolled state directly (instance + credentials + membership), as a granted
    // flow's persist would have.
    crate::identity::load_or_create_device_id(&rig.fs, &rig.layout()).unwrap();
    enroll::write_instance(
        &rig.fs,
        &rig.layout(),
        &enroll::Instance {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            base_url: API.to_owned(),
        },
    )
    .unwrap();
    enroll::write_credentials(&rig.fs, &rig.layout(), "devc_secret", "dev_1").unwrap();

    let enroll_fake = FakeEnroll::new(log.clone(), vec![DeviceAuthPoll::Pending]);
    let out = run_follow_invite(
        &rig,
        &enroll_fake,
        &directory,
        vec!["https://topos.sh/invite/tok_direct".to_owned()],
        opts(false),
    )
    .unwrap();
    // No device flow started — the accept rode the device lane, and the describe targets the hint.
    let ops::FollowOutcome::Described { describe, .. } = out else {
        panic!("the direct accept continues into the two-phase describe, got {out:?}");
    };
    assert_eq!(describe.targets[0].name, "deploy");
    {
        let l = log.lock().unwrap();
        assert!(l.iter().any(|e| e == "accept tok_direct"), "{l:?}");
        assert!(
            !l.iter().any(|e| e.starts_with("authorize")),
            "no device flow starts on an enrolled install: {l:?}"
        );
    }
    // The membership persisted (the credential now reaches the joined workspace).
    let user = enroll::read_user(&rig.fs, &rig.layout()).unwrap().unwrap();
    assert!(user.workspaces.iter().any(|m| m.workspace_id == WS));
    // No WAL exists — nothing to resume.
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
}

#[test]
fn an_invite_url_for_a_different_plane_refuses_toward_the_second_install_hatch() {
    let rig = Rig::new("inv-url-wrong");
    let log: CallLog = Arc::default();
    let directory = InviteDirectory {
        log: log.clone(),
        hint: None,
    };
    crate::identity::load_or_create_device_id(&rig.fs, &rig.layout()).unwrap();
    enroll::write_instance(
        &rig.fs,
        &rig.layout(),
        &enroll::Instance {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            base_url: "https://api.other.test".to_owned(),
        },
    )
    .unwrap();
    let enroll_fake = FakeEnroll::new(log.clone(), vec![DeviceAuthPoll::Pending]);
    let err = run_follow_invite(
        &rig,
        &enroll_fake,
        &directory,
        vec!["https://topos.sh/invite/tok_wrong".to_owned()],
        opts(false),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("TOPOS_HOME"), "{msg}");
    assert!(
        !log.lock().unwrap().iter().any(|e| e.starts_with("accept")),
        "nothing was accepted across planes"
    );
}
