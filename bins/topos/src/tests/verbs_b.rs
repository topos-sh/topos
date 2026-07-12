//! The reshaped verb surface over fakes (no HTTP): `remove` (per-device exclusion, local permanent delete),
//! `channel add`/`channel remove` (placement, create-on-first-place, role refusal), `protect` (dual-kind,
//! level validation, loosening), `invite` (the bare read, the two-phase describe, the applied roster
//! write), and the `review` inbox (inbox/outbox split). Each asserts the two-phase gate — a bare verb
//! describes and mutates nothing, `--yes` applies — plus the refusal spellings.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::requests::{
    InvitationData, InvitationRequest, ProposeRequest, PublishRequest, RevertRequest,
    ReviewRequest, WireChannelEntry, WireChannelIndex, WireChannelSkill, WireMe, WireProposalEntry,
    WireProposalIndex, WireReach, WireSkillIndex, WireSkillIndexEntry, WireSkillLog,
};
use topos_types::{CurrencyKind, Generation, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    ContributeSource, DirectorySource, FollowSource, GovernanceSource, InertFollow, InertPlane,
    PlaneSource, WriteReceipt,
};
use crate::sidecar::Layout;
use crate::{enroll, ops};

const WS: &str = "w_acme";
const API: &str = "https://api.acme.test";

// ---------------------------------------------------------------------------------------------
// Scratch + rig.
// ---------------------------------------------------------------------------------------------

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-vb-{tag}-{}-{n}", std::process::id()));
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
            ids: SeqIds::new("s"),
            clock: FixedClock(1_700_000_000_000),
            harness: NullHarness,
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
            follow,
        }
    }
    /// Seed the enrolled state a completed `follow` leaves (instance + one membership + credential),
    /// with `principal` as the acting identity.
    fn seed_enrolled(&self, principal: &str) {
        enroll::write_instance(
            &self.fs,
            &self.layout(),
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
            email: Some(principal.to_owned()),
            principal: Some(principal.to_owned()),
            workspaces: Vec::new(),
        };
        enroll::upsert_membership(
            &mut user,
            enroll::Membership {
                workspace_id: WS.to_owned(),
                display_name: Some("Acme Inc".to_owned()),
                roles: Vec::new(),
                verified_domain: None,
                verified_domain_status: topos_types::bootstrap::VerifiedDomainStatus::Unverified,
                invite_rooted: false,
                enrolled_at: 1,
            },
        );
        enroll::write_user(&self.fs, &self.layout(), &user).unwrap();
        enroll::write_credential(&self.fs, &self.layout(), WS, "wsc_secret").unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// The recording fake directory.
// ---------------------------------------------------------------------------------------------

type CallLog = Arc<Mutex<Vec<String>>>;

#[derive(Clone)]
struct FakeDir {
    channels: Vec<WireChannelEntry>,
    skills: Vec<WireSkillIndexEntry>,
    proposals: Vec<WireProposalEntry>,
    reach_persons: u64,
    /// A code to fail curated placements / protection writes with (e.g. `CURATED_ROLE_REQUIRED`).
    place_refusal: Option<String>,
    protect_refusal: Option<String>,
    log: CallLog,
}
impl FakeDir {
    fn new(log: CallLog) -> Self {
        Self {
            channels: vec![
                chan("everyone", true, true, "open", &[]),
                chan("eng", false, false, "open", &[("s_deploy", "deploy")]),
                chan("secure", false, false, "curated", &[]),
            ],
            skills: vec![skill("s_deploy", "deploy"), skill("s_docs", "docs")],
            proposals: Vec::new(),
            reach_persons: 7,
            place_refusal: None,
            protect_refusal: None,
            log,
        }
    }
    fn record(&self, line: String) {
        self.log.lock().unwrap().push(line);
    }
}

fn chan(
    name: &str,
    builtin: bool,
    member: bool,
    mode: &str,
    skills: &[(&str, &str)],
) -> WireChannelEntry {
    WireChannelEntry {
        name: name.to_owned(),
        mode: mode.to_owned(),
        builtin,
        member,
        member_count: 5,
        skills: skills
            .iter()
            .map(|(id, n)| WireChannelSkill {
                skill_id: (*id).to_owned(),
                name: (*n).to_owned(),
            })
            .collect(),
    }
}
fn skill(id: &str, name: &str) -> WireSkillIndexEntry {
    WireSkillIndexEntry {
        skill_id: id.to_owned(),
        name: name.to_owned(),
        status: "active".to_owned(),
        version_id: "a".repeat(64),
        bundle_digest: "b".repeat(64),
        generation: Generation { epoch: 1, seq: 1 },
        display_name: None,
        updated_at: 0,
        open_proposals: 0,
    }
}
fn proposal(
    skill_id: &str,
    name: &str,
    hash: &str,
    proposer: &str,
    msg: &str,
) -> WireProposalEntry {
    WireProposalEntry {
        skill_id: skill_id.to_owned(),
        skill_name: name.to_owned(),
        version_id: hash.to_owned(),
        base_version_id: "c".repeat(64),
        proposer: proposer.to_owned(),
        message: msg.to_owned(),
        created_at: "2026-07-12T00:00:00Z".to_owned(),
        stale: false,
    }
}

impl DirectorySource for FakeDir {
    fn me(&self, _ws: &str) -> Result<WireMe, ClientError> {
        Ok(WireMe {
            workspace_id: WS.into(),
            name: "acme".into(),
            display_name: "Acme Inc".into(),
            address: "https://topos.sh/acme".into(),
            principal: "alice@acme.com".into(),
            role: "owner".into(),
            invited_by: None,
            invite_policy: "members".into(),
        })
    }
    fn channels_index(&self, _ws: &str) -> Result<WireChannelIndex, ClientError> {
        Ok(WireChannelIndex {
            channels: self.channels.clone(),
        })
    }
    fn skills_index(&self, _ws: &str) -> Result<WireSkillIndex, ClientError> {
        Ok(WireSkillIndex {
            skills: self.skills.clone(),
        })
    }
    fn proposals_index(&self, _ws: &str) -> Result<WireProposalIndex, ClientError> {
        Ok(WireProposalIndex {
            proposals: self.proposals.clone(),
        })
    }
    fn skill_log(&self, _ws: &str, _skill: &str) -> Result<WireSkillLog, ClientError> {
        unreachable!("no log read in these flows")
    }
    fn reach(&self, _ws: &str, skill_id: &str) -> Result<WireReach, ClientError> {
        self.record(format!("reach {skill_id}"));
        Ok(WireReach {
            persons: self.reach_persons,
            devices: self.reach_persons * 2,
        })
    }
    fn follow_skill(&self, _ws: &str, skill_id: &str) -> Result<(), ClientError> {
        self.record(format!("follow {skill_id}"));
        Ok(())
    }
    fn unfollow_skill(&self, _ws: &str, skill_id: &str) -> Result<(), ClientError> {
        self.record(format!("unfollow {skill_id}"));
        Ok(())
    }
    fn channel_join(&self, _ws: &str, channel: &str) -> Result<(), ClientError> {
        self.record(format!("join {channel}"));
        Ok(())
    }
    fn channel_leave(&self, _ws: &str, channel: &str) -> Result<(), ClientError> {
        self.record(format!("leave {channel}"));
        Ok(())
    }
    fn channel_place(&self, _ws: &str, ch: &str, skill_id: &str) -> Result<(), ClientError> {
        if let Some(code) = &self.place_refusal {
            return Err(ClientError::PlaneTerminal {
                outcome: topos_types::TerminalOutcome::Denied,
                code: code.clone(),
                retryable: false,
            });
        }
        self.record(format!("place {ch} {skill_id}"));
        Ok(())
    }
    fn channel_unplace(&self, _ws: &str, ch: &str, skill_id: &str) -> Result<(), ClientError> {
        self.record(format!("unplace {ch} {skill_id}"));
        Ok(())
    }
    fn exclude_device(&self, _ws: &str, skill_id: &str) -> Result<(), ClientError> {
        self.record(format!("exclude {skill_id}"));
        Ok(())
    }
    fn protect_skill(&self, _ws: &str, skill_id: &str, level: &str) -> Result<(), ClientError> {
        if let Some(code) = &self.protect_refusal {
            return Err(ClientError::PlaneTerminal {
                outcome: topos_types::TerminalOutcome::Denied,
                code: code.clone(),
                retryable: false,
            });
        }
        self.record(format!("protect_skill {skill_id} {level}"));
        Ok(())
    }
    fn protect_channel(&self, _ws: &str, ch: &str, level: &str) -> Result<(), ClientError> {
        if let Some(code) = &self.protect_refusal {
            return Err(ClientError::PlaneTerminal {
                outcome: topos_types::TerminalOutcome::Denied,
                code: code.clone(),
                retryable: false,
            });
        }
        self.record(format!("protect_channel {ch} {level}"));
        Ok(())
    }
    fn ack_notices(&self, _ws: &str, ids: &[String]) -> Result<(), ClientError> {
        self.record(format!("ack {}", ids.join(",")));
        Ok(())
    }
}

/// A contribute source that panics if used — the review inbox/describe reads never write.
struct NullContribute;
impl ContributeSource for NullContribute {
    fn publish(&self, _b: PublishRequest) -> Result<WriteReceipt, ClientError> {
        unreachable!("no write in these flows")
    }
    fn propose(&self, _b: ProposeRequest) -> Result<WriteReceipt, ClientError> {
        unreachable!("no write in these flows")
    }
    fn revert(&self, _b: RevertRequest) -> Result<WriteReceipt, ClientError> {
        unreachable!("no write in these flows")
    }
    fn review(&self, _b: ReviewRequest) -> Result<WriteReceipt, ClientError> {
        unreachable!("no write in these flows")
    }
}

/// A governance source that captures the invite body + returns a canned outcome.
#[derive(Clone)]
struct FakeGov {
    captured: Arc<Mutex<Option<(String, InvitationRequest)>>>,
}
impl GovernanceSource for FakeGov {
    fn invite(&self, ws: &str, body: InvitationRequest) -> Result<InvitationData, ClientError> {
        *self.captured.lock().unwrap() = Some((ws.to_owned(), body.clone()));
        Ok(InvitationData {
            address: "https://topos.sh/acme".into(),
            invited: body.emails,
            mailed: false,
        })
    }
}

/// Build a `DirectoryConnect` closure over a shared fake (each connector build clones it).
fn dir_connect(fake: &FakeDir) -> impl Fn(&str) -> Box<dyn DirectorySource> + '_ {
    move |_b: &str| Box::new(fake.clone())
}

// ---------------------------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------------------------

#[test]
fn remove_followed_skill_describe_then_exclude() {
    let rig = Rig::new("rm-followed");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeDir::new(log.clone());
    let connect = dir_connect(&fake);
    let connectors = ops::RemoveConnectors {
        directory: &connect,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);

    // Bare = describe (nothing mutated): the boundary is the followed exclusion.
    let out = ops::remove(&ctx, &connectors, &["deploy".into()], &[], None, false).unwrap();
    match out {
        ops::RemoveOutcome::Described { data, yes_argv } => {
            assert!(!data.applied);
            assert_eq!(data.items.len(), 1);
            assert!(matches!(
                data.items[0].kind,
                topos_types::results::RemoveKind::FollowedExclusion
            ));
            assert!(data.items[0].bytes_kept);
            assert!(yes_argv.contains(&"--yes".to_owned()));
        }
        _ => panic!("bare remove should describe"),
    }
    assert!(
        log.lock().unwrap().is_empty(),
        "a describe mutates nothing: {:?}",
        log.lock().unwrap()
    );

    // --yes applies: the server exclusion row is written.
    let out = ops::remove(&ctx, &connectors, &["deploy".into()], &[], None, true).unwrap();
    assert!(matches!(out, ops::RemoveOutcome::Applied(d) if d.applied));
    assert_eq!(*log.lock().unwrap(), vec!["exclude s_deploy".to_owned()]);
}

#[test]
fn remove_unresolvable_is_uniform_not_found() {
    let rig = Rig::new("rm-miss");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeDir::new(log);
    let connect = dir_connect(&fake);
    let connectors = ops::RemoveConnectors {
        directory: &connect,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    // A name that is neither a plane skill, a tracked local, nor a discovered untracked copy.
    let err = ops::remove(&ctx, &connectors, &["ghost".into()], &[], None, true).unwrap_err();
    assert_eq!(err.code(), "NOT_FOUND");
}

// ---------------------------------------------------------------------------------------------
// channel add / remove
// ---------------------------------------------------------------------------------------------

#[test]
fn channel_add_describe_then_place() {
    let rig = Rig::new("ch-add");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeDir::new(log.clone());
    let connect = dir_connect(&fake);
    let connectors = ops::ChannelConnectors {
        directory: &connect,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);

    let args = vec!["add".into(), "eng".into(), "deploy".into()];
    let out = ops::channel(&ctx, &connectors, &args, None, false).unwrap();
    match out {
        ops::ChannelOutcome::Described { data, .. } => {
            assert!(!data.creates, "eng exists");
            assert_eq!(data.mode, "open");
            assert_eq!(data.items.len(), 1);
        }
        _ => panic!("bare channel add should describe"),
    }
    // The describe reads the channel index but writes nothing.
    assert!(!log.lock().unwrap().iter().any(|l| l.starts_with("place")));

    let out = ops::channel(&ctx, &connectors, &args, None, true).unwrap();
    assert!(matches!(out, ops::ChannelOutcome::Applied(_)));
    assert!(
        log.lock()
            .unwrap()
            .contains(&"place eng s_deploy".to_owned())
    );
}

#[test]
fn channel_remove_unplaces() {
    let rig = Rig::new("ch-rm");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeDir::new(log.clone());
    let connect = dir_connect(&fake);
    let connectors = ops::ChannelConnectors {
        directory: &connect,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let args = vec!["remove".into(), "eng".into(), "deploy".into()];
    let out = ops::channel(&ctx, &connectors, &args, None, true).unwrap();
    assert!(matches!(out, ops::ChannelOutcome::Applied(_)));
    assert!(
        log.lock()
            .unwrap()
            .contains(&"unplace eng s_deploy".to_owned())
    );
}

#[test]
fn channel_remove_missing_channel_is_not_found() {
    let rig = Rig::new("ch-rm-miss");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeDir::new(log);
    let connect = dir_connect(&fake);
    let connectors = ops::ChannelConnectors {
        directory: &connect,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let args = vec!["remove".into(), "ghostchannel".into(), "deploy".into()];
    let err = ops::channel(&ctx, &connectors, &args, None, true).unwrap_err();
    assert_eq!(err.code(), "NOT_FOUND");
}

#[test]
fn reset_without_a_named_skill_is_refused() {
    // `update --reset` throws away edits — it must never blanket-reset every followed skill.
    let rig = Rig::new("reset-bare");
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let err = ops::reset(&ctx, &[], false).unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    assert!(err.to_string().contains("needs a skill name"), "{err}");
}

#[test]
fn channel_add_to_new_channel_says_it_creates() {
    let rig = Rig::new("ch-new");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeDir::new(log);
    let connect = dir_connect(&fake);
    let connectors = ops::ChannelConnectors {
        directory: &connect,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let args = vec!["add".into(), "brandnew".into(), "deploy".into()];
    let out = ops::channel(&ctx, &connectors, &args, None, false).unwrap();
    match out {
        ops::ChannelOutcome::Described { data, .. } => assert!(data.creates),
        _ => panic!("describe"),
    }
}

#[test]
fn channel_add_curated_role_refusal_is_typed() {
    let rig = Rig::new("ch-curated");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let mut fake = FakeDir::new(log);
    fake.place_refusal = Some("CURATED_ROLE_REQUIRED".into());
    let connect = dir_connect(&fake);
    let connectors = ops::ChannelConnectors {
        directory: &connect,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let args = vec!["add".into(), "secure".into(), "deploy".into()];
    let err = ops::channel(&ctx, &connectors, &args, None, true).unwrap_err();
    assert_eq!(err.code(), "DENIED");
    assert!(err.to_string().contains("reviewer"), "{err}");
}

// ---------------------------------------------------------------------------------------------
// protect
// ---------------------------------------------------------------------------------------------

#[test]
fn protect_skill_bare_tightens_to_reviewed_with_reach() {
    let rig = Rig::new("pr-skill");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeDir::new(log.clone());
    let connect = dir_connect(&fake);
    let connectors = ops::ProtectConnectors {
        directory: &connect,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);

    let out = ops::protect(&ctx, &connectors, "deploy", None, None, false).unwrap();
    match out {
        ops::ProtectOutcome::Described { data, .. } => {
            assert_eq!(data.level, "reviewed");
            assert!(!data.loosening);
            assert_eq!(data.audience, Some(7));
        }
        _ => panic!("describe"),
    }
    assert!(
        !log.lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("protect_skill"))
    );

    let out = ops::protect(&ctx, &connectors, "deploy", None, None, true).unwrap();
    assert!(matches!(out, ops::ProtectOutcome::Applied(_)));
    assert!(
        log.lock()
            .unwrap()
            .contains(&"protect_skill s_deploy reviewed".to_owned())
    );
}

#[test]
fn protect_channel_bare_tightens_to_curated() {
    let rig = Rig::new("pr-chan");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeDir::new(log.clone());
    let connect = dir_connect(&fake);
    let connectors = ops::ProtectConnectors {
        directory: &connect,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let out = ops::protect(&ctx, &connectors, "eng", None, None, true).unwrap();
    match out {
        ops::ProtectOutcome::Applied(data) => assert_eq!(data.level, "curated"),
        _ => panic!("apply"),
    }
    assert!(
        log.lock()
            .unwrap()
            .contains(&"protect_channel eng curated".to_owned())
    );
}

#[test]
fn protect_skill_open_loosens_and_notes_pending_proposals() {
    let rig = Rig::new("pr-open");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeDir::new(log);
    let connect = dir_connect(&fake);
    let connectors = ops::ProtectConnectors {
        directory: &connect,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let out = ops::protect(&ctx, &connectors, "deploy", Some("open"), None, false).unwrap();
    match out {
        ops::ProtectOutcome::Described { data, .. } => {
            assert!(data.loosening);
            assert!(data.note.as_deref().unwrap().contains("pending proposals"));
        }
        _ => panic!("describe"),
    }
}

#[test]
fn protect_invalid_level_for_kind_is_a_usage_error() {
    let rig = Rig::new("pr-bad");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeDir::new(log);
    let connect = dir_connect(&fake);
    let connectors = ops::ProtectConnectors {
        directory: &connect,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    // `curated` is a channel level, not a skill level.
    let err = ops::protect(&ctx, &connectors, "deploy", Some("curated"), None, true).unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");
}

#[test]
fn protect_owner_role_refusal_is_typed() {
    let rig = Rig::new("pr-role");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let mut fake = FakeDir::new(log);
    fake.protect_refusal = Some("OWNER_ROLE_REQUIRED".into());
    let connect = dir_connect(&fake);
    let connectors = ops::ProtectConnectors {
        directory: &connect,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let err = ops::protect(&ctx, &connectors, "deploy", Some("open"), None, true).unwrap_err();
    assert_eq!(err.code(), "DENIED");
    assert!(err.to_string().contains("owner"), "{err}");
}

// ---------------------------------------------------------------------------------------------
// invite
// ---------------------------------------------------------------------------------------------

#[test]
fn invite_bare_reads_address_and_policy_and_changes_nothing() {
    let rig = Rig::new("inv-read");
    rig.seed_enrolled("alice@acme.com");
    let captured = Arc::new(Mutex::new(None));
    let gov = FakeGov {
        captured: captured.clone(),
    };
    let gov_connect = move |_b: &str| -> Box<dyn GovernanceSource> { Box::new(gov.clone()) };
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeDir::new(log);
    let dir = dir_connect(&fake);
    let connectors = ops::InviteConnectors {
        governance: &gov_connect,
        directory: &dir,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let out = ops::invite(&ctx, &connectors, Vec::new(), Vec::new(), None, false).unwrap();
    match out {
        ops::InviteOutcome::Read(data) => {
            assert_eq!(data.address, "https://topos.sh/acme");
            assert_eq!(data.invite_policy, "members");
            assert!(!data.changed);
        }
        _ => panic!("a bare invite is a read"),
    }
    assert!(captured.lock().unwrap().is_none(), "nothing was sent");
}

#[test]
fn invite_with_emails_describes_then_applies_and_folds() {
    let rig = Rig::new("inv-apply");
    rig.seed_enrolled("alice@acme.com");
    let captured = Arc::new(Mutex::new(None));
    let gov = FakeGov {
        captured: captured.clone(),
    };
    let gov_connect = move |_b: &str| -> Box<dyn GovernanceSource> { Box::new(gov.clone()) };
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeDir::new(log);
    let dir = dir_connect(&fake);
    let connectors = ops::InviteConnectors {
        governance: &gov_connect,
        directory: &dir,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);

    // Describe (no --yes): mixed-case emails already fold; nothing is sent.
    let emails = vec!["Bob@Acme.COM".to_owned()];
    let out = ops::invite(
        &ctx,
        &connectors,
        emails.clone(),
        vec!["eng".into()],
        None,
        false,
    )
    .unwrap();
    match out {
        ops::InviteOutcome::Described { describe, .. } => {
            assert_eq!(describe.seat, vec!["bob@acme.com".to_owned()]);
            assert_eq!(describe.channels, vec!["eng".to_owned()]);
        }
        _ => panic!("emails without --yes describe"),
    }
    assert!(captured.lock().unwrap().is_none());

    // Apply (--yes): the folded wire body reaches the transport.
    let out = ops::invite(&ctx, &connectors, emails, vec!["eng".into()], None, true).unwrap();
    assert!(matches!(out, ops::InviteOutcome::Applied(_)));
    let (ws, body) = captured.lock().unwrap().clone().unwrap();
    assert_eq!(ws, WS);
    assert_eq!(body.emails, vec!["bob@acme.com".to_owned()]);
    assert_eq!(body.channels, vec!["eng".to_owned()]);
}

// ---------------------------------------------------------------------------------------------
// review inbox
// ---------------------------------------------------------------------------------------------

#[test]
fn review_inbox_splits_others_from_yours_by_principal() {
    let rig = Rig::new("rv-inbox");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let mut fake = FakeDir::new(log);
    let mine = "a".repeat(64);
    let theirs = "d".repeat(64);
    fake.proposals = vec![
        proposal("s_docs", "docs", &theirs, "bob@acme.com", "improve docs"),
        proposal("s_deploy", "deploy", &mine, "alice@acme.com", "my change"),
    ];
    let dir = dir_connect(&fake);
    let contribute = |_b: &str| -> Box<dyn ContributeSource> { Box::new(NullContribute) };
    let connectors = ops::ReviewConnectors {
        directory: &dir,
        contribute: &contribute,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);

    let out = ops::review_dispatch(&ctx, &connectors, None, None, None).unwrap();
    match out {
        ops::ReviewOutcome::Inbox(data) => {
            assert_eq!(data.inbox.len(), 1, "bob's proposal is in the inbox");
            assert_eq!(data.inbox[0].proposer, "bob@acme.com");
            // The author message rides the entry (rendered first).
            assert_eq!(data.inbox[0].message, "improve docs");
            assert_eq!(data.outbox.len(), 1, "alice's own proposal is the outbox");
            assert_eq!(data.outbox[0].proposer, "alice@acme.com");
        }
        _ => panic!("a bare review is the inbox"),
    }
}
