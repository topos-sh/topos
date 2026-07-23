//! The reshaped verb surface over fakes (no HTTP): `remove` (per-device exclusion, local permanent delete),
//! `channel add`/`channel remove` (placement, create-on-first-place, role refusal), `protect` (dual-kind,
//! level validation, loosening), `invite` (the bare read, the two-phase describe, the applied roster
//! write), and the `review` inbox (inbox/outbox split). Each asserts the two-phase gate — a bare verb
//! describes and mutates nothing, `--yes` applies — plus the refusal spellings.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use topos_core::digest::{self, FileMode, ManifestEntry};
use topos_core::identity::{self, Commit};
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::requests::{
    InvitationData, InvitationRequest, ProposeRequest, PublishRequest, RevertRequest,
    ReviewRequest, WireCandidate, WireChannelEntry, WireChannelIndex, WireChannelSkill,
    WireFileMode, WireMe, WireProposalEntry, WireProposalIndex, WireReach, WireSkillIndex,
    WireSkillIndexEntry, WireSkillLog,
};
use topos_types::results::PublishGate;
use topos_types::{
    CurrencyKind, CurrentRecord, HarnessId, PointerScope, Receipt, TerminalOutcome, TriggerReport,
    TriggerState, WIRE_SCHEMA_VERSION, WireCurrentRecord,
};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    ContributeSource, DeliverySkill, DeliverySnapshot, DeliverySource, DirectorySource,
    FetchedVersion, FollowSource, GovernanceSource, InertFollow, InertPlane, KnownCurrent,
    PlaneError, PlaneSource, PointerFetch, ReconcileTransport, WriteReceipt,
};
use crate::plane_http::FileFollow;
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
            roots: None,
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
            },
        )
        .unwrap();
        let mut user = enroll::UserDoc {
            schema_version: 1,
            principal: Some(principal.to_owned()),
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
        enroll::write_credentials(&self.fs, &self.layout(), "wsc_secret", "dev_1").unwrap();
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
    /// The caller's role `me` answers (default `owner`; the describe-note tests set `member`).
    role: String,
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
            role: "owner".to_owned(),
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
        kind: "skill".to_owned(),
        status: "active".to_owned(),
        version_id: "a".repeat(64),
        bundle_digest: "b".repeat(64),
        generation: 1,
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
        // Default OMITTED — models a server predating the field, so these fixtures exercise the
        // principal-comparison COMPAT fallback; the split tests set `Some(..)` explicitly.
        yours: None,
    }
}

/// A proposal the server marked as the CALLER's own (`yours: true`) — even when its `proposer`
/// display string differs from the local principal (a renamed email / a display name).
fn proposal_yours(
    skill_id: &str,
    name: &str,
    hash: &str,
    proposer: &str,
    msg: &str,
) -> WireProposalEntry {
    WireProposalEntry {
        yours: Some(true),
        ..proposal(skill_id, name, hash, proposer, msg)
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
            role: self.role.clone(),
            invited_by: None,
            link_status: "active".into(),
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
fn remove_followed_clean_skill_applies_the_exclusion_immediately() {
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

    // A followed CLEAN skill: the bare run APPLIES the exclusion immediately — the server row is
    // written, the receipt is undo-led, `--yes` stays an accepted no-op.
    let out = ops::remove(&ctx, &connectors, &["deploy".into()], &[], None, false).unwrap();
    match out {
        ops::RemoveOutcome::Applied(data) => {
            assert!(data.applied);
            assert_eq!(data.items.len(), 1);
            assert!(matches!(
                data.items[0].kind,
                topos_types::results::RemoveKind::FollowedExclusion
            ));
            assert!(data.items[0].bytes_kept);
            // QUALIFIED — the promised inverse must resolve even beside a same-named skill in a
            // second workspace.
            assert_eq!(data.undo, vec!["topos", "follow", "acme/skills/deploy"]);
        }
        _ => panic!("a followed clean skill applies immediately"),
    }
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
    let out = ops::invite(&ctx, &connectors, Vec::new(), None, None, None, false).unwrap();
    match out {
        ops::InviteOutcome::Read(data) => {
            assert_eq!(data.address, "https://topos.sh/acme");
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
        None,
        Some("eng".into()),
        None,
        false,
    )
    .unwrap();
    match out {
        ops::InviteOutcome::Described { describe, yes_argv } => {
            assert_eq!(describe.seat, vec!["bob@acme.com".to_owned()]);
            assert_eq!(describe.channel.as_deref(), Some("eng"));
            assert!(describe.skill.is_none());
            // The apply argv re-spells the hint.
            assert!(yes_argv.iter().any(|a| a == "--channel"));
        }
        _ => panic!("emails without --yes describe"),
    }
    assert!(captured.lock().unwrap().is_none());

    // Apply (--yes): the folded wire body reaches the transport.
    let out = ops::invite(
        &ctx,
        &connectors,
        emails,
        None,
        Some("eng".into()),
        None,
        true,
    )
    .unwrap();
    assert!(matches!(out, ops::InviteOutcome::Applied(_)));
    let (ws, body) = captured.lock().unwrap().clone().unwrap();
    assert_eq!(ws, WS);
    assert_eq!(body.emails, vec!["bob@acme.com".to_owned()]);
    assert_eq!(body.channel.as_deref(), Some("eng"));
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

    let out = ops::review_dispatch(
        &ctx,
        &connectors,
        None,
        None,
        None,
        ops::DiffBudget::resolve(None, false),
    )
    .unwrap();
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

#[test]
fn review_inbox_prefers_the_server_yours_flag_over_the_principal_string() {
    // The server marks a proposal `yours: true` from the resolved user id — the client must put it in the
    // OUTBOX even when its `proposer` DISPLAY string differs from the local principal (a renamed login, a
    // display name). The principal comparison stays only as the old-server compat fallback.
    let rig = Rig::new("rv-yours");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let mut fake = FakeDir::new(log);
    let mine = "a".repeat(64);
    let theirs = "d".repeat(64);
    fake.proposals = vec![
        proposal("s_docs", "docs", &theirs, "bob@acme.com", "improve docs"),
        // `proposer` is a DIFFERENT string than the local principal, but the server says it is yours.
        proposal_yours("s_deploy", "deploy", &mine, "Alice (she/her)", "my change"),
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

    let out = ops::review_dispatch(
        &ctx,
        &connectors,
        None,
        None,
        None,
        ops::DiffBudget::resolve(None, false),
    )
    .unwrap();
    match out {
        ops::ReviewOutcome::Inbox(data) => {
            assert_eq!(data.inbox.len(), 1, "only bob's proposal is to review");
            assert_eq!(data.inbox[0].proposer, "bob@acme.com");
            assert_eq!(
                data.outbox.len(),
                1,
                "the server-marked-yours proposal is the outbox"
            );
            assert_eq!(data.outbox[0].proposer, "Alice (she/her)");
        }
        _ => panic!("a bare review is the inbox"),
    }
}

#[test]
fn review_inbox_never_overrides_a_served_yours_false_with_the_principal_match() {
    // The served `yours` is authoritative in BOTH directions. Emails are mutable and
    // re-registrable: someone ELSE may now hold this install's old enrolled login as their
    // address, so their proposal's `proposer` string EQUALS the stored principal — but the server
    // (resolved user id) says `yours: false`. The proposal must land in the INBOX (approve/reject
    // offered), never mislabel into the outbox on the stale string match.
    let rig = Rig::new("rv-yoursfalse");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let mut fake = FakeDir::new(log);
    let theirs = "e".repeat(64);
    fake.proposals = vec![WireProposalEntry {
        yours: Some(false),
        ..proposal(
            "s_docs",
            "docs",
            &theirs,
            "alice@acme.com", // the re-registered address — string-equal to the principal
            "someone else's change",
        )
    }];
    let dir = dir_connect(&fake);
    let contribute = |_b: &str| -> Box<dyn ContributeSource> { Box::new(NullContribute) };
    let connectors = ops::ReviewConnectors {
        directory: &dir,
        contribute: &contribute,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);

    let out = ops::review_dispatch(
        &ctx,
        &connectors,
        None,
        None,
        None,
        ops::DiffBudget::resolve(None, false),
    )
    .unwrap();
    match out {
        ops::ReviewOutcome::Inbox(data) => {
            assert!(data.outbox.is_empty(), "a served false is never overridden");
            assert_eq!(data.inbox.len(), 1, "it is someone else's to review");
        }
        _ => panic!("a bare review is the inbox"),
    }
}

#[test]
fn review_target_falls_back_to_the_catalog_when_not_locally_followed() {
    // The device is enrolled and the workspace CATALOG knows the skill, but the device has NO local follow
    // entry for it (the genesis publisher, pre-`update`). Resolving the review target must fall back to the
    // catalog over the wire, so the exact `<name>@<hash>` command `topos review` printed resolves — instead
    // of failing "no tracked skill". With no matching open proposal, resolution SUCCEEDS and the verb
    // returns the uniform NOT_FOUND, proving it reached the proposal filter (a pre-fix run stops at
    // NO_SUCH_SKILL during resolution, never reading the proposals at all).
    let rig = Rig::new("rv-catalog");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let mut fake = FakeDir::new(log);
    fake.skills = vec![skill("s_release", "release-notes")];
    fake.proposals = Vec::new();
    let dir = dir_connect(&fake);
    let contribute = |_b: &str| -> Box<dyn ContributeSource> { Box::new(NullContribute) };
    let connectors = ops::ReviewConnectors {
        directory: &dir,
        contribute: &contribute,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);

    let target = format!("release-notes@{}", "a".repeat(64));
    let err = ops::review_dispatch(
        &ctx,
        &connectors,
        Some(&target),
        None,
        None,
        ops::DiffBudget::resolve(None, false),
    )
    .unwrap_err();
    assert_eq!(
        err.code(),
        "NOT_FOUND",
        "resolution fell back to the catalog and reached the proposal filter"
    );
}

#[test]
fn review_target_keeps_no_such_skill_when_absent_from_the_catalog() {
    // The catalog is reachable but does NOT hold the name (and there is no local copy): resolution stays
    // the local "no tracked skill" — the wire fallback never invents a target.
    let rig = Rig::new("rv-nocat");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let mut fake = FakeDir::new(log);
    fake.skills = vec![skill("s_deploy", "deploy")];
    let dir = dir_connect(&fake);
    let contribute = |_b: &str| -> Box<dyn ContributeSource> { Box::new(NullContribute) };
    let connectors = ops::ReviewConnectors {
        directory: &dir,
        contribute: &contribute,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);

    let target = format!("release-notes@{}", "a".repeat(64));
    let err = ops::review_dispatch(
        &ctx,
        &connectors,
        Some(&target),
        None,
        None,
        ops::DiffBudget::resolve(None, false),
    )
    .unwrap_err();
    assert_eq!(err.code(), "NO_SUCH_SKILL");
}

#[test]
fn a_catalog_resolved_describe_with_no_local_copy_degrades_to_the_clean_not_found() {
    // The verdictless DESCRIBE of a catalog-only skill (no local copy) resolves via the catalog and finds
    // the matching proposal, then the diff's LOCAL resolve has nothing to render — it must surface the
    // clean old-shaped NO_SUCH_SKILL, NEVER a confusing transport-shaped plane error.
    let rig = Rig::new("rv-desc-nolocal");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let mut fake = FakeDir::new(log);
    fake.skills = vec![skill("s_release", "release-notes")];
    let hash = "a".repeat(64);
    fake.proposals = vec![proposal(
        "s_release",
        "release-notes",
        &hash,
        "bob@acme.com",
        "notes",
    )];
    let dir = dir_connect(&fake);
    let contribute = |_b: &str| -> Box<dyn ContributeSource> { Box::new(NullContribute) };
    let connectors = ops::ReviewConnectors {
        directory: &dir,
        contribute: &contribute,
    };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);

    let target = format!("release-notes@{hash}");
    let err = ops::review_dispatch(
        &ctx,
        &connectors,
        Some(&target),
        None,
        None,
        ops::DiffBudget::resolve(None, false),
    )
    .unwrap_err();
    assert_eq!(
        err.code(),
        "NO_SUCH_SKILL",
        "the describe degrades to the clean local not-found, never a transport error",
    );
}

/// A read transport that GATES `get_current` on a prior `bind_skill` — exactly as the real `UreqPlane`
/// gates on its `follows.json`-derived cred map, answering the indistinguishable `NotFound` for a skill
/// it was never taught. Records the binding so the test can prove the review wiring applies it.
struct BindGatedPlane {
    bound: std::cell::RefCell<std::collections::HashSet<String>>,
}
impl BindGatedPlane {
    fn new() -> Self {
        Self {
            bound: std::cell::RefCell::new(std::collections::HashSet::new()),
        }
    }
}
impl PlaneSource for BindGatedPlane {
    fn get_current(
        &self,
        skill_id: &str,
        _k: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError> {
        if !self.bound.borrow().contains(skill_id) {
            // The pre-bind state: the per-skill cred map does not know this skill → "not served here".
            return Err(PlaneError::NotFound);
        }
        Ok(PointerFetch::Record(WireCurrentRecord {
            schema_version: WIRE_SCHEMA_VERSION,
            scope: PointerScope {
                workspace_id: WS.to_owned(),
                skill_id: skill_id.to_owned(),
            },
            record: CurrentRecord {
                version_id: "7".repeat(64),
                generation: 1,
            },
        }))
    }
    fn fetch_version(&self, _s: &str, _v: [u8; 32]) -> Result<FetchedVersion, PlaneError> {
        unreachable!("a reject verdict fetches no version bytes")
    }
    fn bind_skill(&self, _ws: &str, skill_id: &str) {
        self.bound.borrow_mut().insert(skill_id.to_owned());
    }
}

/// A contribute source whose `review` returns a canned OK receipt (the reject write's terminal outcome);
/// the other three verbs never fire in a review-reject flow.
struct OkReview;
impl ContributeSource for OkReview {
    fn publish(&self, _b: PublishRequest) -> Result<WriteReceipt, ClientError> {
        unreachable!("no publish in a review flow")
    }
    fn propose(&self, _b: ProposeRequest) -> Result<WriteReceipt, ClientError> {
        unreachable!("no propose in a review flow")
    }
    fn revert(&self, _b: RevertRequest) -> Result<WriteReceipt, ClientError> {
        unreachable!("no revert in a review flow")
    }
    fn review(&self, _b: ReviewRequest) -> Result<WriteReceipt, ClientError> {
        Ok(WriteReceipt {
            receipt: Some(Receipt {
                schema_version: 1,
                op_id: "op-1".into(),
                command: "review".into(),
                outcome: TerminalOutcome::Ok,
                workspace_id: WS.into(),
                skill_id: Some("s_release".into()),
                version_id: None,
                bundle_digest: None,
                expected_generation: None,
                current_generation: None,
                created_at: "2026-07-13T00:00:00Z".into(),
                details: None,
            }),
            error: None,
            wire_record: None,
        })
    }
}

#[test]
fn a_catalog_resolved_review_binds_the_skill_credential_for_the_downstream_reads() {
    // The genesis publisher: enrolled, the catalog knows the skill, but NO local follow entry
    // (pre-`update`). A review VERDICT resolves via the catalog, then reads `current` under the workspace
    // credential — which requires the read transport to be TAUGHT this skill first (`bind_skill`). Without
    // the bind (the pre-fix code) that read answers the transport-shaped "not served here" and the verdict
    // dies WORSE than the old clean NO_SUCH_SKILL; with it, the reject lands.
    let rig = Rig::new("rv-bind");
    rig.seed_enrolled("alice@acme.com");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let mut fake = FakeDir::new(log);
    fake.skills = vec![skill("s_release", "release-notes")];
    let dir = dir_connect(&fake);
    let contribute = |_b: &str| -> Box<dyn ContributeSource> { Box::new(OkReview) };
    let connectors = ops::ReviewConnectors {
        directory: &dir,
        contribute: &contribute,
    };
    let plane = BindGatedPlane::new();
    let inert_f = InertFollow;
    let ctx = rig.ctx(&plane, &inert_f);

    let target = format!("release-notes@{}", "a".repeat(64));
    let out = ops::review_dispatch(
        &ctx,
        &connectors,
        Some(&target),
        Some(ops::ReviewVerdict::Reject {
            reason: Some("not yet".into()),
        }),
        None,
        ops::DiffBudget::resolve(None, false),
    )
    .unwrap();
    assert!(
        matches!(out, ops::ReviewOutcome::Applied(_)),
        "the catalog-resolved reject landed (the downstream current read authenticated)",
    );
    assert!(
        plane.bound.borrow().contains("s_release"),
        "the review wiring bound the catalog-resolved skill's credential before the read",
    );
}

// ---------------------------------------------------------------------------------------------
// publish (describe gate) — the gate reads the FRESH server protection, not the cached follow-state
// ---------------------------------------------------------------------------------------------

/// A delivery transport whose `fetch_delivery` answers a canned snapshot (`Some`) or fails (`None` — the
/// offline path). `publish_describe` reads only `fetch_delivery`, so the pointer/byte/report lanes panic.
#[derive(Clone)]
struct FakeDelivery {
    snapshot: Option<DeliverySnapshot>,
}
impl PlaneSource for FakeDelivery {
    fn get_current(&self, _s: &str, _k: Option<KnownCurrent>) -> Result<PointerFetch, PlaneError> {
        unreachable!("publish_describe reads delivery, never the pointer")
    }
    fn fetch_version(&self, _s: &str, _v: [u8; 32]) -> Result<FetchedVersion, PlaneError> {
        unreachable!("publish_describe fetches no bytes")
    }
}
impl DeliverySource for FakeDelivery {
    fn workspaces(&self) -> Vec<String> {
        vec![WS.to_owned()]
    }
    fn fetch_delivery(&self, _ws: &str) -> Result<DeliverySnapshot, PlaneError> {
        self.snapshot
            .clone()
            .ok_or_else(|| PlaneError::Unreachable("offline".to_owned()))
    }
    fn report_applied(&self, _ws: &str, _a: &[(String, [u8; 32])]) -> Result<(), PlaneError> {
        unreachable!("publish_describe never reports")
    }
}

/// A one-skill delivery snapshot carrying `review_required` as the FRESH per-bundle protection.
fn delivery_with(skill_id: &str, review_required: bool) -> DeliverySnapshot {
    DeliverySnapshot {
        skills: vec![DeliverySkill {
            skill_id: skill_id.to_owned(),
            name: "pd-skill".to_owned(),
            review_required,
            version_id: [0u8; 32],
            generation: 1,
            bundle_digest: [0u8; 32],
            via_channels: vec!["everyone".to_owned()],
            via_direct: true,
        }],
        detached: Vec::new(),
        excluded: Vec::new(),
        proposals_awaiting: 0,
        notices: Vec::new(),
        staleness_window_ms: 604_800_000,
        link_status: crate::plane::LinkStatus::Active,
    }
}

/// Adopt + FOLLOW a real skill (recording `cached_review_required` in `follows.json`, as the last
/// delivery reconcile stamped it), edit its draft so it diverges from `current`, then run
/// `publish_describe` with the delivery connector answering `fresh` (a snapshot with that protection, or
/// `None` to make the delivery read fail — the offline fallback). Returns the described gate.
fn publish_describe_gate(cached_review_required: bool, fresh: Option<bool>) -> PublishGate {
    let rig = Rig::new("pubdesc");
    rig.seed_enrolled("alice@acme.com");

    // A real tracked skill, adopted in place (placement = the source dir).
    let src = Scratch::new("pubdesc-src");
    let skill_dir = src.0.join("pd-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: pd-skill\ndescription: base\n---\n# base\n",
    )
    .unwrap();
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let add = {
        let ctx = rig.ctx(&inert_p, &inert_f);
        ops::add(&ctx, &skill_dir).unwrap()
    };

    // Edit the draft so it diverges from `current` (a describe of an unchanged draft is NO_CHANGES).
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: pd-skill\ndescription: edited\n---\n# edited draft\n",
    )
    .unwrap();

    // The follow entry carries the CACHED protection + the workspace scope.
    enroll::write_follows_merged(
        &rig.fs,
        &rig.layout(),
        &[enroll::FollowEntry {
            skill_id: add.skill_id.clone(),
            workspace_id: WS.to_owned(),
            mode: enroll::FollowModeDoc::Auto,
            review_required: cached_review_required,
            following: true,
            excluded_here: false,
            agents: Vec::new(),
            excluded_agents: Vec::new(),
        }],
    )
    .unwrap();
    let follows = enroll::read_follows(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    let file_follow = FileFollow::new(enroll::follow_contexts(&follows));

    // The directory connector (reach + me — tolerated); the delivery connector carries the FRESH protection.
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let dir = FakeDir::new(log);
    let dir_c = dir_connect(&dir);
    let del = FakeDelivery {
        snapshot: fresh.map(|p| delivery_with(&add.skill_id, p)),
    };
    let del_c = |_b: &str| -> Box<dyn ReconcileTransport> { Box::new(del.clone()) };
    let connectors = ops::PublishDescribeConnectors {
        directory: &dir_c,
        delivery: &del_c,
    };

    let ctx = rig.ctx(&inert_p, &file_follow);
    ops::publish_describe(&ctx, &connectors, None, "pd-skill", false, None, None)
        .expect("describe succeeds")
        .gate
}

#[test]
fn publish_describe_gate_prefers_fresh_reviewed_over_cached_open() {
    // The device followed while the skill was OPEN (cached review_required=false); an owner has since run
    // `protect <skill> reviewed`. The describe must show the PROPOSAL gate the apply will be rerouted
    // into — never the stale "lands directly" the cached follow-state would claim (consent == the act).
    assert_eq!(
        publish_describe_gate(false, Some(true)),
        PublishGate::Proposal
    );
}

#[test]
fn publish_describe_gate_prefers_fresh_open_over_cached_reviewed() {
    // The reverse staleness: cached reviewed, loosened to open upstream since the last sync — the fresh
    // read lands it directly.
    assert_eq!(publish_describe_gate(true, Some(false)), PublishGate::Lands);
}

#[test]
fn publish_describe_gate_falls_back_to_cached_when_delivery_offline() {
    // The delivery read fails (offline): the describe keeps working and falls back to the CACHED
    // protection in either direction, so a bare describe still answers with no network.
    assert_eq!(publish_describe_gate(false, None), PublishGate::Lands);
    assert_eq!(publish_describe_gate(true, None), PublishGate::Proposal);
}

/// The publish describe's MERGE PREVIEW: a copy BEHIND the last-known observed `current` (whose
/// bytes a prior sweep already fetched into the local store) predicts the rebase merge purely from
/// local bytes — clean when the edits are disjoint, conflicted (naming the path) when they overlap.
/// An up-to-date copy carries NO preview, and the whole thing runs over INERT transports (the
/// describe gains no network call for it).
#[test]
fn publish_describe_previews_the_rebase_merge_for_a_behind_copy() {
    use topos_gitstore::{ImportFile, Store};
    use topos_types::persisted::SyncState;
    use topos_types::results::MergePreviewVerdict;

    // `overlap`: whether theirs edits the same SKILL.md the draft edits (→ conflicted) or adds a
    // disjoint file (→ clean). Returns the describe's preview.
    let run = |tag: &str, overlap: bool| {
        let rig = Rig::new(tag);
        rig.seed_enrolled("alice@acme.com");
        let src = Scratch::new(&format!("{tag}-src"));
        let skill_dir = src.0.join("pv-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: pv-skill\ndescription: base\n---\n# base\n",
        )
        .unwrap();
        let inert_p = InertPlane;
        let inert_f = InertFollow;
        let add = {
            let ctx = rig.ctx(&inert_p, &inert_f);
            ops::add(&ctx, &skill_dir).unwrap()
        };
        // The local draft edits SKILL.md.
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: pv-skill\ndescription: edited\n---\n# edited draft\n",
        )
        .unwrap();

        // "Theirs" — the observed current a prior sweep fetched: parented on the genesis, either
        // overlapping the draft's SKILL.md edit or a disjoint added file. Committed straight into
        // the skill's local store (exactly where a sweep's ensure_local lands it).
        let sid = crate::id::SkillId::parse(&add.skill_id).unwrap();
        let sp = rig.layout().published(&sid);
        let theirs_files: Vec<(&str, &[u8])> = if overlap {
            vec![(
                "SKILL.md",
                b"---\nname: pv-skill\ndescription: theirs\n---\n# theirs\n",
            )]
        } else {
            vec![
                (
                    "SKILL.md",
                    b"---\nname: pv-skill\ndescription: base\n---\n# base\n",
                ),
                ("notes.md", b"theirs\n"),
            ]
        };
        let entries: Vec<ManifestEntry> = theirs_files
            .iter()
            .map(|(p, b)| ManifestEntry {
                path: (*p).to_owned(),
                mode: FileMode::Regular,
                content_sha256: digest::sha256(b),
            })
            .collect();
        let tree_digest = digest::bundle_digest(&entries).unwrap();
        let genesis = ops::parse_hex32(&add.version_id).unwrap();
        let theirs_id = identity::commit_id(&Commit {
            parents: &[genesis],
            tree: tree_digest,
            author: "d_pub",
            message: "v1",
        })
        .unwrap();
        let store = Store::open(&sp.store).unwrap();
        let import: Vec<ImportFile<'_>> = theirs_files
            .iter()
            .map(|(p, b)| ImportFile {
                path: p,
                mode: FileMode::Regular,
                bytes: b,
            })
            .collect();
        let tree = store.write_bundle(&import).unwrap();
        store
            .commit(theirs_id, &[genesis], &tree, "d_pub", "v1")
            .unwrap();

        // The BEHIND state: a sweep observed generation 2 (theirs) but never applied it.
        let sync: SyncState = crate::doc::read_doc(&rig.fs, &sp.sync).unwrap().unwrap();
        crate::doc::write_doc(
            &rig.fs,
            &sp.sync,
            &SyncState {
                observed: 2,
                observed_version_id: topos_core::digest::to_hex(&theirs_id),
                ..sync
            },
        )
        .unwrap();

        enroll::write_follows_merged(
            &rig.fs,
            &rig.layout(),
            &[enroll::FollowEntry {
                skill_id: add.skill_id.clone(),
                workspace_id: WS.to_owned(),
                mode: enroll::FollowModeDoc::Auto,
                review_required: false,
                following: true,
                excluded_here: false,
                agents: Vec::new(),
                excluded_agents: Vec::new(),
            }],
        )
        .unwrap();
        let follows = enroll::read_follows(&rig.fs, &rig.layout())
            .unwrap()
            .unwrap();
        let file_follow = FileFollow::new(enroll::follow_contexts(&follows));
        let log: CallLog = Arc::new(Mutex::new(Vec::new()));
        let dir = FakeDir::new(log);
        let dir_c = dir_connect(&dir);
        let del = FakeDelivery { snapshot: None };
        let del_c = |_b: &str| -> Box<dyn ReconcileTransport> { Box::new(del.clone()) };
        let connectors = ops::PublishDescribeConnectors {
            directory: &dir_c,
            delivery: &del_c,
        };
        let ctx = rig.ctx(&inert_p, &file_follow);
        ops::publish_describe(&ctx, &connectors, None, "pv-skill", false, None, None)
            .expect("describe succeeds")
            .merge_preview
    };

    // Disjoint edits → a clean prediction.
    let clean = run("pvclean", false).expect("a behind copy previews");
    assert_eq!(clean.verdict, MergePreviewVerdict::Clean);
    assert!(clean.conflicts.is_empty());
    // Overlapping edits → conflicted, naming the path.
    let conflicted = run("pvconf", true).expect("a behind copy previews");
    assert_eq!(conflicted.verdict, MergePreviewVerdict::Conflicted);
    assert_eq!(conflicted.conflicts, vec!["SKILL.md".to_owned()]);

    // And the control: the gate tests above run the SAME describe with an up-to-date copy — their
    // envelopes carry no preview (absent = nothing to predict). Re-proven here structurally: an
    // up-to-date sync (observed == applied) short-circuits before any store read.
}

// ---------------------------------------------------------------------------------------------
// publish (apply + describe) — the NO_CHANGES no-op guard covers a genesis AUTHOR's repeat publish,
// not just a follower's. A skill adopted locally then published has NO follow entry, so the refusal
// keys on a published `current` existing (`observed` past GENESIS), never on follow-state.
// ---------------------------------------------------------------------------------------------

/// Recompute a candidate's `commit_id` (the `version_id` `current` moves to) from the wire bundle — the
/// SAME derivation the client and the plane run: the bundle digest (git's "tree") over the files, then the
/// length-prefixed commit frame over `(parents, tree, author, message)`. A faithful OK receipt's pointer
/// names exactly this, so `apply_publish_ok`'s scope + version check passes.
fn candidate_commit(candidate: &WireCandidate) -> [u8; 32] {
    use base64::Engine as _;
    let entries: Vec<ManifestEntry> = candidate
        .files
        .iter()
        .map(|f| ManifestEntry {
            path: f.path.clone(),
            mode: match f.mode {
                WireFileMode::Regular => FileMode::Regular,
                WireFileMode::Executable => FileMode::Executable,
            },
            content_sha256: digest::sha256(
                &base64::engine::general_purpose::STANDARD
                    .decode(&f.content_base64)
                    .unwrap(),
            ),
        })
        .collect();
    let tree = digest::bundle_digest(&entries).unwrap();
    let parents: Vec<[u8; 32]> = candidate
        .parents
        .iter()
        .map(|p| ops::parse_hex32(p).unwrap())
        .collect();
    identity::commit_id(&Commit {
        parents: &parents,
        tree,
        author: &candidate.author,
        message: &candidate.message,
    })
    .unwrap()
}

/// A contribute source that ACKS a direct publish with a byte-faithful OK receipt: it recomputes the
/// candidate's commit id from the wire bundle exactly as the plane would, and serves a `current` pointer
/// naming it at `generation` (the plane's genesis branch creates `current` at 1), so the caller's local
/// state fast-forwards past GENESIS. Records every op id it sent, so a test can assert a repeat identical
/// publish never reached the wire. Propose/revert/review are unreachable here — the no-op guard refuses an
/// identical `--propose` BEFORE the send, and these flows never revert/review.
#[derive(Clone)]
struct OkPublish {
    generation: u64,
    sent: Arc<Mutex<Vec<String>>>,
}
impl OkPublish {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            sent: Arc::new(Mutex::new(Vec::new())),
        }
    }
}
impl ContributeSource for OkPublish {
    fn publish(&self, b: PublishRequest) -> Result<WriteReceipt, ClientError> {
        self.sent.lock().unwrap().push(b.op_id.clone());
        let vid = digest::to_hex(&candidate_commit(&b.candidate));
        Ok(WriteReceipt {
            receipt: Some(Receipt {
                schema_version: 1,
                op_id: b.op_id,
                command: "publish".to_owned(),
                outcome: TerminalOutcome::Ok,
                workspace_id: b.workspace_id.clone(),
                skill_id: Some(b.skill_id.clone()),
                version_id: Some(vid.clone()),
                bundle_digest: None,
                expected_generation: None,
                current_generation: Some(self.generation),
                created_at: "2026-07-16T00:00:00Z".to_owned(),
                details: None,
            }),
            error: None,
            wire_record: Some(WireCurrentRecord {
                schema_version: 1,
                scope: PointerScope {
                    workspace_id: b.workspace_id,
                    skill_id: b.skill_id,
                },
                record: CurrentRecord {
                    version_id: vid,
                    generation: self.generation,
                },
            }),
        })
    }
    fn propose(&self, _b: ProposeRequest) -> Result<WriteReceipt, ClientError> {
        unreachable!("an identical --propose refuses NO_CHANGES before the send")
    }
    fn revert(&self, _b: RevertRequest) -> Result<WriteReceipt, ClientError> {
        unreachable!("no revert in these flows")
    }
    fn review(&self, _b: ReviewRequest) -> Result<WriteReceipt, ClientError> {
        unreachable!("no review in these flows")
    }
}

/// Genesis-AUTHOR setup: enroll, adopt a real skill dir in place (NO follow entry — the author case), and
/// run the FIRST `publish --yes` against a byte-faithful OK plane, landing the skill's `current` and
/// advancing local `observed` past GENESIS. Returns the adopted `skill_id` — the post-first-publish state
/// on which a REPEAT identical publish must refuse `NO_CHANGES` (the reported bug: a duplicate version).
fn genesis_author_first_publish(rig: &Rig, src: &Scratch) -> String {
    rig.seed_enrolled("alice@acme.com");
    let skill_dir = src.0.join("deploy");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: deploy\ndescription: base\n---\n# deploy\n",
    )
    .unwrap();

    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let add = ops::add(&ctx, &skill_dir).unwrap();

    let ok = OkPublish::new(1);
    let connect = |_b: &str| -> Box<dyn ContributeSource> { Box::new(ok.clone()) };
    let out = ops::publish(
        &ctx, &connect, None, None, "deploy", false, None, None, None,
    )
    .unwrap();
    assert!(
        matches!(out, ops::PublishOutcome::Published(_)),
        "the genesis author's first publish lands current"
    );
    assert_eq!(
        ok.sent.lock().unwrap().len(),
        1,
        "the first publish reached the wire exactly once"
    );
    add.skill_id
}

#[test]
fn a_genesis_authors_repeat_publish_refuses_no_changes_and_writes_no_wal() {
    // The reported bug (apply path): `publish --yes` twice with an unchanged draft used to mint a
    // duplicate version (same tree, new commit parented on the last). Now the second refuses NO_CHANGES —
    // even though the author has NO follow entry — because a published `current` exists (`observed` past
    // GENESIS).
    let rig = Rig::new("pub-noop-apply");
    let src = Scratch::new("pub-noop-apply-src");
    let skill_id = genesis_author_first_publish(&rig, &src);

    let ok = OkPublish::new(2);
    let connect = |_b: &str| -> Box<dyn ContributeSource> { Box::new(ok.clone()) };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let err = ops::publish(
        &ctx, &connect, None, None, "deploy", false, None, None, None,
    )
    .unwrap_err();
    assert!(
        matches!(err, ClientError::NoChanges { .. }),
        "a repeat identical publish is NO_CHANGES, got {err:?}"
    );
    assert!(
        ok.sent.lock().unwrap().is_empty(),
        "the refusal fired before any wire send"
    );

    // The guard fires BEFORE the op-WAL write (it lives in `build_publish_op`, ahead of the record), so no
    // pending publish op is left behind to replay.
    let pending = crate::op_wal::find_pending_for_skill(
        &rig.fs,
        &rig.layout(),
        WS,
        &skill_id,
        &[
            topos_types::persisted::OpKind::PublishDirect,
            topos_types::persisted::OpKind::PublishPropose,
        ],
    )
    .unwrap();
    assert!(
        pending.is_none(),
        "the no-op guard leaves no pending op-WAL record"
    );
}

#[test]
fn a_genesis_authors_repeat_publish_describe_is_no_changes() {
    // Describe path (bare publish), same unfollowed author: the skill has a published `current`
    // (`observed` past GENESIS) though no follow entry, so the describe refuses NO_CHANGES — before any
    // network read (reach / me / delivery).
    let rig = Rig::new("pub-noop-describe");
    let src = Scratch::new("pub-noop-describe-src");
    let _ = genesis_author_first_publish(&rig, &src);

    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let dir = FakeDir::new(log);
    let dir_c = dir_connect(&dir);
    let del = FakeDelivery { snapshot: None };
    let del_c = |_b: &str| -> Box<dyn ReconcileTransport> { Box::new(del.clone()) };
    let connectors = ops::PublishDescribeConnectors {
        directory: &dir_c,
        delivery: &del_c,
    };

    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let err =
        ops::publish_describe(&ctx, &connectors, None, "deploy", false, None, None).unwrap_err();
    assert!(
        matches!(err, ClientError::NoChanges { .. }),
        "the unfollowed author's repeat describe is NO_CHANGES, got {err:?}"
    );
    assert!(
        dir.log.lock().unwrap().is_empty(),
        "the no-op describe reads no network before refusing"
    );
}

#[test]
fn a_genesis_authors_identical_propose_refuses_no_changes() {
    // `--propose` with identical bytes also refuses (both op kinds flow through `build_publish_op`), so a
    // no-op proposal is never opened — matching the describe, which refuses regardless of `--propose`.
    let rig = Rig::new("pub-noop-propose");
    let src = Scratch::new("pub-noop-propose-src");
    let _ = genesis_author_first_publish(&rig, &src);

    let ok = OkPublish::new(2);
    let connect = |_b: &str| -> Box<dyn ContributeSource> { Box::new(ok.clone()) };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let err =
        ops::publish(&ctx, &connect, None, None, "deploy", true, None, None, None).unwrap_err();
    assert!(
        matches!(err, ClientError::NoChanges { .. }),
        "an identical --propose is NO_CHANGES, got {err:?}"
    );
    assert!(
        ok.sent.lock().unwrap().is_empty(),
        "the propose refusal fired before any wire send (propose is unreachable in the fake)"
    );
}

#[test]
fn editing_the_draft_lets_a_second_publish_land() {
    // Non-regression: the guard keys on byte-identity, so a genuinely changed draft still publishes. Edit
    // a file byte after the first publish → the second lands a new version (the wire is dialed again).
    let rig = Rig::new("pub-edit");
    let src = Scratch::new("pub-edit-src");
    let _ = genesis_author_first_publish(&rig, &src);

    std::fs::write(
        src.0.join("deploy").join("SKILL.md"),
        "---\nname: deploy\ndescription: edited\n---\n# deploy v2\n",
    )
    .unwrap();

    let ok = OkPublish::new(2);
    let connect = |_b: &str| -> Box<dyn ContributeSource> { Box::new(ok.clone()) };
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let out = ops::publish(
        &ctx, &connect, None, None, "deploy", false, None, None, None,
    )
    .unwrap();
    assert!(
        matches!(out, ops::PublishOutcome::Published(_)),
        "an edited draft publishes again (not a no-op)"
    );
    assert_eq!(
        ok.sent.lock().unwrap().len(),
        1,
        "the edited publish reached the wire"
    );
}

/// The exact teammate handoff the acme fixture address composes — the join line a non-member's
/// agent can follow (the share line's skill page answers only for members).
const ACME_INVITE_LINE: &str = "Ask your agent: \"Set up Topos for us: fetch \
                                https://topos.sh/agent and follow it. Our workspace: \
                                https://topos.sh/acme\"";

#[test]
fn a_landed_publish_carries_the_teammate_handoff_line() {
    // The APPLY receipt hands the author the join line — composed from the same `me.address` the
    // describe's share line reads, fetched best-effort after the publish settled.
    let rig = Rig::new("pub-invite");
    let src = Scratch::new("pub-invite-src");
    rig.seed_enrolled("alice@acme.com");
    let skill_dir = src.0.join("deploy");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: deploy\ndescription: base\n---\n# deploy\n",
    )
    .unwrap();

    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    ops::add(&ctx, &skill_dir).unwrap();

    let ok = OkPublish::new(1);
    let connect = |_b: &str| -> Box<dyn ContributeSource> { Box::new(ok.clone()) };
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let dir = FakeDir::new(log);
    let dir_c = dir_connect(&dir);
    let out = ops::publish(
        &ctx,
        &connect,
        Some(&dir_c),
        None,
        "deploy",
        false,
        None,
        None,
        None,
    )
    .unwrap();
    let ops::PublishOutcome::Published(data) = out else {
        panic!("the genesis publish lands current");
    };
    assert_eq!(data.invite_line.as_deref(), Some(ACME_INVITE_LINE));
}

#[test]
fn a_publish_describe_carries_the_teammate_handoff_line() {
    // The DESCRIBE composes the same join line from the same `me` read as the share line — the
    // origin is the address minus its workspace path.
    let rig = Rig::new("pd-invite");
    let src = Scratch::new("pd-invite-src");
    rig.seed_enrolled("alice@acme.com");
    let skill_dir = src.0.join("deploy");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: deploy\ndescription: base\n---\n# deploy\n",
    )
    .unwrap();
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    ops::add(&ctx, &skill_dir).unwrap();

    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let dir = FakeDir::new(log);
    let dir_c = dir_connect(&dir);
    let del = FakeDelivery { snapshot: None };
    let del_c = |_b: &str| -> Box<dyn ReconcileTransport> { Box::new(del.clone()) };
    let connectors = ops::PublishDescribeConnectors {
        directory: &dir_c,
        delivery: &del_c,
    };
    let data = ops::publish_describe(&ctx, &connectors, None, "deploy", false, None, None)
        .expect("describe succeeds");
    assert_eq!(
        data.share_line.as_deref(),
        Some("https://topos.sh/acme/skills/deploy")
    );
    assert_eq!(data.invite_line.as_deref(), Some(ACME_INVITE_LINE));
}

// ---------------------------------------------------------------------------------------------
// publish (describe placements) — the placement line + its curated note track what the APPLY
// would actually touch: an explicit `--to` places on EVERY publish, the default `everyone` on a
// GENESIS only (a bare republish alters no placement); the note fires whenever the TARGET
// resolves curated against a member caller — never for an owner, an open channel, or a `--to`
// naming a channel absent from the index (create-on-first-use, born open).
// ---------------------------------------------------------------------------------------------

/// Adopt a fresh `deploy` skill (genesis author — no follow entry), optionally land its FIRST
/// publish and edit the draft (`republish` — the non-genesis bare case), then run the bare
/// publish describe with the caller's `role` and the default `everyone` channel in
/// `everyone_mode`. Returns the described `(placements, placement_note)`.
fn describe_placements(
    tag: &str,
    role: &str,
    everyone_mode: &str,
    channel: Option<&str>,
    republish: bool,
) -> (Vec<String>, Option<String>) {
    let rig = Rig::new(tag);
    let src = Scratch::new(&format!("{tag}-src"));
    if republish {
        let _ = genesis_author_first_publish(&rig, &src);
        // Edit the draft so the second describe is not NO_CHANGES.
        std::fs::write(
            src.0.join("deploy").join("SKILL.md"),
            "---\nname: deploy\ndescription: edited\n---\n# deploy v2\n",
        )
        .unwrap();
    } else {
        rig.seed_enrolled("alice@acme.com");
        let skill_dir = src.0.join("deploy");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: deploy\ndescription: base\n---\n# deploy\n",
        )
        .unwrap();
        let inert_p = InertPlane;
        let inert_f = InertFollow;
        let ctx = rig.ctx(&inert_p, &inert_f);
        ops::add(&ctx, &skill_dir).unwrap();
    }

    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let mut dir = FakeDir::new(log);
    dir.role = role.to_owned();
    dir.channels = vec![
        chan("everyone", true, true, everyone_mode, &[]),
        chan("secure", false, false, "curated", &[]),
    ];
    let dir_c = dir_connect(&dir);
    let del = FakeDelivery { snapshot: None };
    let del_c = |_b: &str| -> Box<dyn ReconcileTransport> { Box::new(del.clone()) };
    let connectors = ops::PublishDescribeConnectors {
        directory: &dir_c,
        delivery: &del_c,
    };

    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let data = ops::publish_describe(&ctx, &connectors, None, "deploy", false, channel, None)
        .expect("describe succeeds");
    (data.placements, data.placement_note)
}

#[test]
fn a_member_genesis_describe_notes_a_curated_default_everyone() {
    // The bare genesis default: a member's describe under a CURATED `everyone` carries the note
    // (the apply withholds the placement, catalog-only); an OPEN `everyone`, or an owner caller,
    // does not — their placement lands.
    let (placements, note) = describe_placements("pd-note-mem", "member", "curated", None, false);
    assert_eq!(placements, vec!["everyone".to_owned()]);
    assert_eq!(
        note.as_deref(),
        Some("curated: lands catalog-only; a curator places it afterwards")
    );
    let (_, open_note) = describe_placements("pd-note-open", "member", "open", None, false);
    assert!(open_note.is_none(), "an open everyone needs no note");
    let (_, owner_note) = describe_placements("pd-note-own", "owner", "curated", None, false);
    assert!(owner_note.is_none(), "an owner's placement lands — no note");
}

#[test]
fn an_explicit_to_describe_notes_a_curated_target_for_a_member() {
    // `--to everyone` rides the SAME gated path as any named channel at the apply (no string-match
    // bypass), so the describe notes it too — same for a named curated channel; a `--to` naming a
    // channel ABSENT from the index is create-on-first-use (born open), so no note.
    let (placements, note) =
        describe_placements("pd-to-evr", "member", "curated", Some("everyone"), false);
    assert_eq!(placements, vec!["everyone".to_owned()]);
    assert!(
        note.is_some(),
        "an explicit --to everyone under curation carries the note"
    );
    let (placements, note) =
        describe_placements("pd-to-sec", "member", "open", Some("secure"), false);
    assert_eq!(placements, vec!["secure".to_owned()]);
    assert!(
        note.is_some(),
        "a named curated channel carries the note for a member"
    );
    let (_, fresh_note) =
        describe_placements("pd-to-new", "member", "curated", Some("brand-new"), false);
    assert!(
        fresh_note.is_none(),
        "an unknown --to channel is created open — no note"
    );
}

#[test]
fn a_bare_republish_describe_claims_no_placement() {
    // A locally-authored skill's SECOND bare publish (non-genesis, still no follow entry) alters
    // no placement server-side — the describe lists none and carries no note, even for a member
    // under a curated `everyone`; an explicit `--to` still places (and notes) on a republish.
    let (placements, note) = describe_placements("pd-repub", "member", "curated", None, true);
    assert!(
        placements.is_empty(),
        "a bare republish lists no placement, got {placements:?}"
    );
    assert!(note.is_none(), "no placement, no note");
    let (placements, note) =
        describe_placements("pd-repub-to", "member", "curated", Some("everyone"), true);
    assert_eq!(placements, vec!["everyone".to_owned()]);
    assert!(
        note.is_some(),
        "an explicit --to on a republish still notes a curated target"
    );
}

#[test]
fn a_qualified_agent_remove_refuses_instead_of_widening_to_the_device() {
    // `remove acme/skills/deploy --agent cursor`: the qualified spelling misses the per-agent
    // route (it takes bare names only), and falling through would perform the WHOLE-DEVICE
    // exclusion — strictly more than the caller named. The verb refuses typed toward the
    // supported spelling; nothing mutates.
    let rig = Rig::new("rm-qual-agent");
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

    let err = ops::remove(
        &ctx,
        &connectors,
        &["acme/skills/deploy".into()],
        &["cursor".into()],
        None,
        false,
    )
    .unwrap_err();
    assert!(
        matches!(&err, ClientError::InvalidArgument(m) if m.contains("--agent")),
        "the fall-through refuses typed: {err:?}"
    );
    assert!(
        log.lock().unwrap().is_empty(),
        "no server row moved: {:?}",
        log.lock().unwrap()
    );
}
