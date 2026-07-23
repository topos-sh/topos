//! The two-phase SUBSCRIBE surface over fakes (no HTTP): the `follow <address>` enroll flow
//! (card → authorize(workspace) → redeem → the describe gate), the wrong-server `TOPOS_HOME`
//! refusal, the describe fields (installs + the dirname outcomes — in-place adoptions and
//! auto-namespaced collisions — + direct-follow note), the `--yes` apply (row ops + the
//! batch-accepted reconcile), the dual-kind `unfollow` (workspace/`everyone` refusals; the skill
//! detach row + the local pause), and the hook posture (the staleness warning line; notices
//! fetched-without-ack vs narrated-then-acked).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use topos_core::digest::{self, FileMode, ManifestEntry};
use topos_core::identity::Commit;
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::requests::{
    WireChannelEntry, WireChannelIndex, WireChannelSkill, WireMe, WireNotice, WireProposalIndex,
    WireReach, WireSkillIndex, WireSkillIndexEntry, WireSkillLog,
};
use topos_types::results::PullAction;
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    DeliverySkill, DeliverySnapshot, DeliverySource, DeviceAuthPoll, DeviceAuthStart,
    DirectorySource, EnrollSource, EnrolledGrant, EnrolledWorkspace, FetchedFile, FetchedVersion,
    FollowSource, InertFollow, InertPlane, KnownCurrent, PlaneError, PlaneSource, PointerFetch,
    ReconcileTransport,
};
use crate::sidecar::Layout;
use crate::{enroll, ops, sync_status};

const WS: &str = "w_acme";
const API: &str = "https://api.acme.test";

// ---------------------------------------------------------------------------------------------
// Scratch + rig (mirrors tests/follow.rs's conventions).
// ---------------------------------------------------------------------------------------------

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-sub-{tag}-{}-{n}", std::process::id()));
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
    fn placement_for(
        &self,
        skill_id: &str,
        naming: topos_harness::PlacementNaming<'_>,
        _d: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget {
        // Mirror the real adapters: the ONE naming discipline — the (sanitized) display name,
        // workspace-suffixed on a collision, the id as the last resort. The collision machinery
        // is this suite's whole subject.
        PlacementTarget {
            dir: topos_harness::choose_skill_dir(
                &self.skills_root,
                skill_id,
                naming,
                &topos_harness::dir_taken,
                &|_| false,
            ),
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
            follow,
            roots: None,
        }
    }
    /// Seed the on-disk enrolled state a completed `follow <address>` leaves (instance + membership +
    /// credential; empty follows) — the subscribe flows start from here.
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
        enroll::write_credentials(&self.fs, &self.layout(), "wsc_secret", "dev_1").unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// The fakes: a recording directory, a fixture reconcile transport, and an enroll source that
// answers the card + the address device flow.
// ---------------------------------------------------------------------------------------------

/// The recorded row ops (`"join eng"`, `"follow s_deploy"`, `"leave eng"`, `"unfollow s_x"`,
/// `"ack n1,n2"`), shared across connector-built clones.
type CallLog = Arc<Mutex<Vec<String>>>;

#[derive(Clone)]
struct FakeDirectory {
    me: WireMe,
    channels: Vec<WireChannelEntry>,
    skills: Vec<WireSkillIndexEntry>,
    log: CallLog,
}

impl FakeDirectory {
    fn acme(log: CallLog) -> Self {
        Self {
            me: WireMe {
                workspace_id: WS.into(),
                name: "acme".into(),
                display_name: "Acme Inc".into(),
                address: "https://topos.sh/acme".into(),
                principal: "alice@acme.com".into(),
                role: "member".into(),
                invited_by: Some("robert@acme.com".into()),
                link_status: "active".into(),
            },
            channels: vec![
                channel_entry("everyone", true, true, &[]),
                channel_entry("eng", false, false, &[("s_deploy", "deploy")]),
                // A pre-placed channel (the inviter's placement): member = true.
                channel_entry("design", false, true, &[]),
            ],
            skills: vec![
                skill_entry("s_deploy", "deploy"),
                skill_entry("s_docs", "docs"),
            ],
            log,
        }
    }
    fn record(&self, line: String) {
        self.log.lock().unwrap().push(line);
    }
}

fn channel_entry(
    name: &str,
    builtin: bool,
    member: bool,
    skills: &[(&str, &str)],
) -> WireChannelEntry {
    WireChannelEntry {
        name: name.to_owned(),
        mode: "open".to_owned(),
        builtin,
        member,
        member_count: 3,
        skills: skills
            .iter()
            .map(|(id, n)| WireChannelSkill {
                skill_id: (*id).to_owned(),
                name: (*n).to_owned(),
            })
            .collect(),
    }
}

fn skill_entry(id: &str, name: &str) -> WireSkillIndexEntry {
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

impl DirectorySource for FakeDirectory {
    fn me(&self, workspace_id: &str) -> Result<WireMe, ClientError> {
        if workspace_id != self.me.workspace_id {
            return Err(ClientError::TargetNotFound {
                target: workspace_id.to_owned(),
            });
        }
        Ok(self.me.clone())
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
            proposals: Vec::new(),
        })
    }
    fn skill_log(&self, _ws: &str, _skill: &str) -> Result<WireSkillLog, ClientError> {
        unreachable!("no log read in these flows")
    }
    fn reach(&self, _ws: &str, _skill: &str) -> Result<WireReach, ClientError> {
        unreachable!("no reach read in these flows")
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
    fn channel_place(&self, _ws: &str, _ch: &str, _skill: &str) -> Result<(), ClientError> {
        unreachable!("no placement in these flows")
    }
    fn channel_unplace(&self, _ws: &str, _ch: &str, _skill: &str) -> Result<(), ClientError> {
        unreachable!("no placement in these flows")
    }
    fn exclude_device(&self, _ws: &str, skill_id: &str) -> Result<(), ClientError> {
        self.record(format!("exclude {skill_id}"));
        Ok(())
    }
    fn protect_skill(&self, _ws: &str, _skill: &str, _level: &str) -> Result<(), ClientError> {
        unreachable!("no protection in these flows")
    }
    fn protect_channel(&self, _ws: &str, _ch: &str, _level: &str) -> Result<(), ClientError> {
        unreachable!("no protection in these flows")
    }
    fn ack_notices(&self, _ws: &str, ids: &[String]) -> Result<(), ClientError> {
        self.record(format!("ack {}", ids.join(",")));
        Ok(())
    }
}

/// A version whose bytes reproduce a REAL commit id (the engine re-verifies on apply).
struct Version {
    id: [u8; 32],
    digest: [u8; 32],
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
        digest: tree,
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

/// The reconcile transport fake: one workspace's delivery snapshot + the version bytes the engine
/// fetches, plus a recorded ack log (shared with the directory's).
#[derive(Clone)]
struct FakeTransport {
    snapshot: DeliverySnapshot,
    versions: HashMap<String, FetchedVersion>,
    log: CallLog,
}

impl FakeTransport {
    fn empty(log: CallLog) -> Self {
        Self {
            snapshot: DeliverySnapshot {
                skills: Vec::new(),
                detached: Vec::new(),
                excluded: Vec::new(),
                proposals_awaiting: 0,
                notices: Vec::new(),
                staleness_window_ms: 604_800_000,
                link_status: crate::plane::LinkStatus::Active,
            },
            versions: HashMap::new(),
            log,
        }
    }
}

impl PlaneSource for FakeTransport {
    fn get_current(
        &self,
        _skill_id: &str,
        _known: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError> {
        Err(PlaneError::NotFound)
    }
    fn fetch_version(
        &self,
        skill_id: &str,
        version_id: [u8; 32],
    ) -> Result<FetchedVersion, PlaneError> {
        let _ = version_id;
        self.versions
            .get(skill_id)
            .cloned()
            .ok_or(PlaneError::NotFound)
    }
}

impl DeliverySource for FakeTransport {
    fn workspaces(&self) -> Vec<String> {
        vec![WS.to_owned()]
    }
    fn fetch_delivery(&self, _ws: &str) -> Result<DeliverySnapshot, PlaneError> {
        Ok(self.snapshot.clone())
    }
    fn report_applied(&self, _ws: &str, applied: &[(String, [u8; 32])]) -> Result<(), PlaneError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("report {}", applied.len()));
        Ok(())
    }
    fn ack_notices(&self, _ws: &str, ids: &[String]) -> Result<(), PlaneError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("ack {}", ids.join(",")));
        Ok(())
    }
}

/// The address-flow enroll fake: the protocol card + the device-authorization flow (recording the
/// requested workspace NAME); the poll answers granted with the ONE device credential.
#[derive(Clone)]
struct FakeAddressEnroll {
    api_base: String,
    log: CallLog,
}

impl EnrollSource for FakeAddressEnroll {
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
        _requested_name: &str,
        _invite_token: Option<&str>,
    ) -> Result<DeviceAuthStart, ClientError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("authorize {workspace}"));
        Ok(DeviceAuthStart {
            device_code: "dc_secret".into(),
            user_code: "CODE".into(),
            verification_uri: format!("{}/verify", self.api_base),
            expires_in_secs: 900,
            interval_secs: 5,
        })
    }
    fn device_auth_poll(&self, _dc: &str) -> Result<DeviceAuthPoll, ClientError> {
        self.log.lock().unwrap().push("poll".to_owned());
        Ok(DeviceAuthPoll::Granted(EnrolledGrant {
            hint: None,
            link_status: crate::plane::LinkStatus::Active,
            credential: "devc_secret".into(),
            device_id: "dev_1".into(),
            workspace: EnrolledWorkspace {
                workspace_id: WS.into(),
                name: "acme".into(),
                display_name: "Acme Inc".into(),
            },
        }))
    }
}

/// Build the follow connectors over the fakes. Every connector clones the shared fakes, so the
/// recorded call logs span connector rebuilds exactly as the production fresh-credential reads do.
fn run_follow(
    rig: &Rig,
    enroll_fake: &FakeAddressEnroll,
    directory: &FakeDirectory,
    transport: &FakeTransport,
    targets: Vec<String>,
    opts: ops::FollowOpts,
) -> Result<ops::FollowOutcome, ClientError> {
    crate::identity::load_or_create_device_id(&rig.fs, &rig.layout()).unwrap();
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let enroll_connect = |_b: &str| -> Box<dyn EnrollSource> { Box::new(enroll_fake.clone()) };
    let dir_connect = |_b: &str| -> Box<dyn DirectorySource> { Box::new(directory.clone()) };
    let del_connect = |_b: &str| -> Box<dyn ReconcileTransport> { Box::new(transport.clone()) };
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
        channels: Vec::new(),
        skills: Vec::new(),
        agents: Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// The enroll flow: card → authorize(workspace) → redeem → the describe gate.
// ---------------------------------------------------------------------------------------------

#[test]
fn an_address_follow_enrolls_then_lands_on_the_describe_never_the_apply() {
    let rig = Rig::new("addr-enroll");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let directory = FakeDirectory::acme(log.clone());
    let transport = FakeTransport::empty(log.clone());

    // Call 1: the bare address starts the device flow (card → authorize toward the NAME) and goes
    // pending — the WAL holds the follow intent.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Data { data, .. } = out else {
        panic!("call 1 answers the pending wire payload");
    };
    assert!(data.pending.is_some(), "the device flow is pending");
    assert_eq!(data.plane_base_url.as_deref(), Some(API), "re-rooted");
    {
        let l = log.lock().unwrap();
        assert!(
            l.iter().any(|e| e == "card https://topos.sh/acme"),
            "the card was fetched at the workspace's own address: {l:?}"
        );
        assert!(
            l.iter().any(|e| e == "authorize acme"),
            "the enroll intent named the workspace ADDRESS: {l:?}"
        );
    }

    // Call 2 (re-invoke = resume): granted → redeem at the GRANTED workspace id → promote →
    // continue into the DESCRIBE (bare = nothing subscribed, nothing installed).
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        Vec::new(),
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Described {
        describe,
        next_argvs,
    } = out
    else {
        panic!("a bare resumed follow lands on the describe");
    };
    assert!(describe.enrolled_now, "this invocation enrolled");
    assert_eq!(describe.workspace.name, "acme");
    assert_eq!(describe.role, "member");
    assert_eq!(describe.principal, "alice@acme.com");
    assert_eq!(describe.invited_by.as_deref(), Some("robert@acme.com"));
    // The inviter's pre-placement is disclosed; the structural everyone is not.
    assert_eq!(describe.preplaced_channels, vec!["design".to_owned()]);
    // Nothing is delivered and a workspace target writes no row — the honest standing receipt: no
    // `--yes` to offer, the standing note carries the fact instead.
    assert!(next_argvs.is_empty(), "{next_argvs:?}");
    let note = describe.standing_note.as_deref().expect("standing note");
    assert!(note.starts_with("nothing new to install"), "{note}");
    // The grant was polled for (the credential rides the granted poll — no redeem round-trip).
    assert!(log.lock().unwrap().iter().any(|e| e == "poll"));
    // The enrollment itself promoted (identity, reversible): the credential + membership are on
    // disk, the WAL is gone — but NOTHING was subscribed and nothing installed.
    let creds = enroll::read_credentials(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert_eq!(creds.credential, "devc_secret");
    assert_eq!(creds.device_id, "dev_1");
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
    assert!(
        !log.lock()
            .unwrap()
            .iter()
            .any(|e| e.starts_with("join") || e.starts_with("follow ") || e.starts_with("report")),
        "nothing mutates before --yes except the enrollment itself: {:?}",
        log.lock().unwrap()
    );
}

#[test]
fn the_wrong_server_refusal_names_the_topos_home_hatch() {
    let rig = Rig::new("wrong-server");
    rig.seed_enrolled(); // pinned to API
    let log: CallLog = Arc::default();
    // A card declaring a DIFFERENT plane than the pinned one.
    let enroll_fake = FakeAddressEnroll {
        api_base: "https://other.plane.test".to_owned(),
        log: log.clone(),
    };
    let directory = FakeDirectory::acme(log.clone());
    let transport = FakeTransport::empty(log.clone());

    let err = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["https://other.plane.test/beta".to_owned()],
        opts(false),
    )
    .unwrap_err();
    assert_eq!(err.code(), "PLACEMENT_UNSUPPORTED");
    let msg = err.to_string();
    assert!(msg.contains("TOPOS_HOME"), "the hatch is named: {msg}");
    assert!(msg.contains(API), "the pinned plane is named: {msg}");
}

// ---------------------------------------------------------------------------------------------
// The describe fields: installs + via, the collision choice, the direct-follow note.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_channel_describe_lists_installs_with_digests_and_the_auto_namespaced_collision() {
    let rig = Rig::new("describe");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let directory = FakeDirectory::acme(log.clone());
    let transport = FakeTransport::empty(log.clone());

    // A LOCAL tracked skill (a different identity, DIFFERENT bytes) already occupies the by-name
    // dir under the harness skills root — the incoming channel skill collides on the dirname.
    let local = rig.work.0.join("skills").join("deploy");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("SKILL.md"), b"# local deploy\n").unwrap();
    {
        let inert_p = InertPlane;
        let inert_f = InertFollow;
        let ctx = rig.ctx(&inert_p, &inert_f);
        ops::add_with_name(&ctx, &local, Some("deploy")).unwrap();
    }

    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme/channels/eng".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Described {
        describe,
        next_argvs,
    } = out
    else {
        panic!("bare = describe");
    };
    assert!(!describe.enrolled_now, "already enrolled");
    assert_eq!(describe.targets.len(), 1);
    assert_eq!(describe.targets[0].kind, "channel");
    // The install list carries the catalog digest + the via attribution.
    assert_eq!(describe.installs.len(), 1);
    let install = &describe.installs[0];
    assert_eq!(install.name, "deploy");
    assert_eq!(
        install.bundle_digest.as_deref(),
        Some("b".repeat(64).as_str())
    );
    assert_eq!(install.via_channels, vec!["eng".to_owned()]);
    assert!(!install.via_direct);
    // The collision is disclosed with the auto-namespaced dirname (skill first, workspace suffix);
    // there is exactly ONE apply argv — no opt-in flag exists any more.
    assert_eq!(describe.collisions.len(), 1);
    assert_eq!(describe.collisions[0].name, "deploy");
    assert_eq!(describe.collisions[0].installs_as, "deploy-acme");
    assert!(
        describe.collisions[0].existing.ends_with("skills/deploy"),
        "{}",
        describe.collisions[0].existing
    );
    assert!(describe.adoptions.is_empty(), "different bytes never adopt");
    assert_eq!(next_argvs.len(), 1, "one apply argv, no collision variant");
    assert!(
        next_argvs[0].iter().all(|a| a != "--prefix-dirname"),
        "{next_argvs:?}"
    );
    // Nothing was mutated by the describe.
    assert!(log.lock().unwrap().iter().all(|e| !e.starts_with("join")));
}

#[test]
fn a_direct_skill_follow_on_a_channel_delivered_skill_explains_why_it_is_not_redundant() {
    let rig = Rig::new("direct-note");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let directory = FakeDirectory::acme(log.clone());
    // The skill is ALREADY delivered via #eng (the person joined it earlier).
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n")]);
    let mut transport = FakeTransport::empty(log.clone());
    transport.snapshot.skills.push(DeliverySkill {
        skill_id: "s_deploy".into(),
        name: "deploy".into(),
        review_required: false,
        version_id: v.id,
        generation: 1,
        bundle_digest: v.digest,
        via_channels: vec!["eng".into()],
        via_direct: false,
    });

    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme/skills/deploy".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Described { describe, .. } = out else {
        panic!("bare = describe");
    };
    let note = describe.direct_follow_note.expect("the note is present");
    assert!(note.contains("already arrives via #eng"), "{note}");
    assert!(note.contains("keeps it"), "{note}");
}

// ---------------------------------------------------------------------------------------------
// The dirname outcomes: a byte-identical occupant is ADOPTED in place; a genuine conflict
// auto-namespaces `<skill>-<workspace>` (the ADDRESS slug, never the `w_…` id); an unknown slug
// falls back to the validated skill id.
// ---------------------------------------------------------------------------------------------

/// A transport whose delivery carries `s_deploy` at a REAL version (the engine re-verifies bytes).
fn transport_with_deploy(log: CallLog) -> (FakeTransport, Version) {
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n")]);
    let mut transport = FakeTransport::empty(log);
    transport.snapshot.skills.push(DeliverySkill {
        skill_id: "s_deploy".into(),
        name: "deploy".into(),
        review_required: false,
        version_id: v.id,
        generation: 1,
        bundle_digest: v.digest,
        via_channels: vec!["eng".into()],
        via_direct: false,
    });
    transport
        .versions
        .insert("s_deploy".into(), v.fetched.clone());
    (transport, v)
}

#[test]
fn a_byte_identical_occupant_is_adopted_in_place_never_duplicated() {
    let rig = Rig::new("adopt");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let directory = FakeDirectory::acme(log.clone());
    let (transport, v) = transport_with_deploy(log.clone());

    // An UNTRACKED byte-identical copy already sits at the by-name dir (e.g. hand-installed from
    // the same source) — the untracked-occupant policy is identical to a tracked one's.
    let occupant = rig.work.0.join("skills").join("deploy");
    std::fs::create_dir_all(&occupant).unwrap();
    std::fs::write(occupant.join("SKILL.md"), b"# deploy\n").unwrap();

    // The workspace describe discloses the ADOPTION — no collision, no namespaced sibling.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Described { describe, .. } = out else {
        panic!("bare = describe");
    };
    assert_eq!(describe.adoptions.len(), 1, "{:?}", describe.adoptions);
    assert_eq!(describe.adoptions[0].name, "deploy");
    assert!(
        describe.adoptions[0].path.ends_with("skills/deploy"),
        "{}",
        describe.adoptions[0].path
    );
    assert!(
        describe.collisions.is_empty(),
        "identical bytes are an adoption, never a collision: {:?}",
        describe.collisions
    );

    // `--yes` manages THAT dir: no `deploy-acme` sibling, the recorded placement IS the adopted
    // dir, and the sync landed applied over the occupant's (identical) bytes.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme".to_owned()],
        opts(true),
    )
    .unwrap();
    let ops::FollowOutcome::Applied(applied) = out else {
        panic!("--yes = apply");
    };
    assert_eq!(
        applied
            .installed
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        vec!["deploy"]
    );
    assert!(
        !rig.work.0.join("skills").join("deploy-acme").exists(),
        "never a second copy beside an identical occupant"
    );
    assert_eq!(
        std::fs::read(occupant.join("SKILL.md")).unwrap(),
        b"# deploy\n"
    );
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    let map = crate::doc::read_map(&rig.fs, &rig.layout().published(&sid).map)
        .unwrap()
        .unwrap();
    assert_eq!(
        map.placements,
        vec![occupant.to_string_lossy().into_owned()],
        "the adopted dir IS the placement"
    );
    assert_eq!(
        map.placement_state[0].materialized_sha.as_deref(),
        Some(topos_core::digest::to_hex(&v.digest).as_str()),
        "the apply advanced over the adopted dir"
    );
}

#[test]
fn a_conflicting_occupant_auto_namespaces_by_the_address_slug_and_stays_untouched() {
    let rig = Rig::new("conflict");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let directory = FakeDirectory::acme(log.clone());
    let (transport, v) = transport_with_deploy(log.clone());

    // An UNTRACKED occupant with DIFFERENT bytes holds the by-name dir.
    let occupant = rig.work.0.join("skills").join("deploy");
    std::fs::create_dir_all(&occupant).unwrap();
    std::fs::write(occupant.join("SKILL.md"), b"# mine\n").unwrap();

    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Described { describe, .. } = out else {
        panic!("bare = describe");
    };
    assert_eq!(describe.collisions.len(), 1, "{:?}", describe.collisions);
    assert_eq!(describe.collisions[0].installs_as, "deploy-acme");
    assert!(describe.adoptions.is_empty(), "{:?}", describe.adoptions);

    // `--yes`: the namespaced dir lands the incoming bytes; the occupant is byte-untouched; the
    // suffix is the workspace's ADDRESS slug — a `w_…` id never reaches a dir name.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme".to_owned()],
        opts(true),
    )
    .unwrap();
    let ops::FollowOutcome::Applied(applied) = out else {
        panic!("--yes = apply");
    };
    assert_eq!(
        applied
            .installed
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        vec!["deploy"]
    );
    let namespaced = rig.work.0.join("skills").join("deploy-acme");
    assert_eq!(
        std::fs::read(namespaced.join("SKILL.md")).unwrap(),
        b"# deploy\n"
    );
    assert_eq!(
        std::fs::read(occupant.join("SKILL.md")).unwrap(),
        b"# mine\n",
        "the occupant is never written"
    );
    for entry in std::fs::read_dir(rig.work.0.join("skills")).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with("w_"),
            "a workspace ID must never reach a dir name: {name}"
        );
    }
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    let map = crate::doc::read_map(&rig.fs, &rig.layout().published(&sid).map)
        .unwrap()
        .unwrap();
    assert_eq!(
        map.placements,
        vec![namespaced.to_string_lossy().into_owned()]
    );
    let _ = v;
}

#[test]
fn an_unknown_membership_slug_falls_back_to_the_skill_id_never_the_workspace_id() {
    // Enrolled shape WITHOUT a membership record (instance + credential only): the delivered
    // workspace's ADDRESS slug is unknowable, so a colliding dirname skips the namespace attempt
    // and lands under the validated skill id.
    let rig = Rig::new("slug-fallback");
    enroll::write_instance(
        &rig.fs,
        &rig.layout(),
        &enroll::Instance {
            schema_version: 1,
            base_url: API.to_owned(),
        },
    )
    .unwrap();
    enroll::write_credentials(&rig.fs, &rig.layout(), "wsc_secret", "dev_1").unwrap();
    crate::identity::load_or_create_device_id(&rig.fs, &rig.layout()).unwrap();
    let log: CallLog = Arc::default();
    let (transport, _v) = transport_with_deploy(log.clone());
    let occupant = rig.work.0.join("skills").join("deploy");
    std::fs::create_dir_all(&occupant).unwrap();
    std::fs::write(occupant.join("SKILL.md"), b"# mine\n").unwrap();

    let inert_f = InertFollow;
    // The transport doubles as ctx.plane so the accepted first receive can fetch its bytes.
    let ctx = rig.ctx(&transport, &inert_f);
    let out = ops::pull_reconcile_with(
        &ctx,
        &transport,
        &ops::ReconcileOpts {
            accept_first_receive: true,
            ..ops::ReconcileOpts::default()
        },
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let id_dir = rig.work.0.join("skills").join("s_deploy");
    assert_eq!(
        std::fs::read(id_dir.join("SKILL.md")).unwrap(),
        b"# deploy\n",
        "the fallback dirname is the validated skill id"
    );
    assert!(
        !rig.work.0.join("skills").join("deploy-acme").exists()
            && !rig.work.0.join("skills").join("w_acme-deploy").exists()
            && !rig.work.0.join("skills").join("deploy-w_acme").exists(),
        "no namespace attempt without a membership slug"
    );
    assert_eq!(
        std::fs::read(occupant.join("SKILL.md")).unwrap(),
        b"# mine\n"
    );
}

#[test]
fn a_moved_target_lapses_the_adoption_and_the_accept_lands_namespaced() {
    // The adoption reservation was laid for version A (the sweep's offer); the served current
    // moves to B before the accept. The A-recorded adoption must not be reused for B — the engine
    // clears the lapsed record, re-plans, and lands B under the suffixed dir in the SAME
    // invocation: the occupant stays byte-untouched and nothing wedges.
    let rig = Rig::new("adopt-lapse");
    rig.seed_enrolled();
    crate::identity::load_or_create_device_id(&rig.fs, &rig.layout()).unwrap();
    let log: CallLog = Arc::default();
    let v_a = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy v1\n")]);
    let v_b = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy v2\n")]);
    let mut transport = FakeTransport::empty(log.clone());
    transport.snapshot.skills.push(DeliverySkill {
        skill_id: "s_deploy".into(),
        name: "deploy".into(),
        review_required: false,
        version_id: v_a.id,
        generation: 1,
        bundle_digest: v_a.digest,
        via_channels: vec!["eng".into()],
        via_direct: false,
    });
    transport
        .versions
        .insert("s_deploy".into(), v_a.fetched.clone());

    // An UNTRACKED occupant byte-identical to VERSION A sits at the by-name dir.
    let occupant = rig.work.0.join("skills").join("deploy");
    std::fs::create_dir_all(&occupant).unwrap();
    std::fs::write(occupant.join("SKILL.md"), b"# deploy v1\n").unwrap();

    // The bare sweep lays the baseline + the A-adoption and OFFERS (no bytes move).
    let inert_f = InertFollow;
    {
        let ctx = rig.ctx(&transport, &inert_f);
        ops::pull_reconcile_with(&ctx, &transport, &ops::ReconcileOpts::default()).unwrap();
    }
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    let map = crate::doc::read_map(&rig.fs, &rig.layout().published(&sid).map)
        .unwrap()
        .unwrap();
    assert_eq!(
        map.placement_state[0].pre_existing_sha.as_deref(),
        Some(topos_core::digest::to_hex(&v_a.digest).as_str()),
        "the sweep recorded the A-adoption"
    );

    // The served current MOVES to B before any accept.
    let mut transport_b = transport.clone();
    transport_b.snapshot.skills[0].version_id = v_b.id;
    transport_b.snapshot.skills[0].bundle_digest = v_b.digest;
    transport_b.snapshot.skills[0].generation = 2;
    transport_b
        .versions
        .insert("s_deploy".into(), v_b.fetched.clone());

    // ONE accept: B lands under the suffixed dir; the occupant untouched; no wedge, no retry.
    let follows = enroll::read_follows(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    let seam = crate::plane_http::FileFollow::new(enroll::follow_contexts(&follows));
    let ctx = rig.ctx(&transport_b, &seam);
    let out = ops::pull_reconcile_with(
        &ctx,
        &transport_b,
        &ops::ReconcileOpts {
            accept_first_receive: true,
            ..ops::ReconcileOpts::default()
        },
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(
        matches!(out.data.skills[0].action, PullAction::FastForwarded),
        "{:?}",
        out.data.skills[0].action
    );
    assert_eq!(
        std::fs::read(occupant.join("SKILL.md")).unwrap(),
        b"# deploy v1\n",
        "the raced occupant is never written"
    );
    let namespaced = rig.work.0.join("skills").join("deploy-acme");
    assert_eq!(
        std::fs::read(namespaced.join("SKILL.md")).unwrap(),
        b"# deploy v2\n"
    );
    let map = crate::doc::read_map(&rig.fs, &rig.layout().published(&sid).map)
        .unwrap()
        .unwrap();
    assert_eq!(
        map.placements,
        vec![namespaced.to_string_lossy().into_owned()],
        "the lapsed reservation was swapped for the suffixed dir"
    );
}

#[test]
fn a_dir_recorded_by_another_skill_is_never_adopted_even_when_identical() {
    // The strongest confusion case: ANOTHER tracked skill already owns the by-name dir with bytes
    // IDENTICAL to the incoming version. Adoption must refuse (two records must never own one
    // dir) — the plan suffixes instead, and each record keeps exactly its own dir.
    let rig = Rig::new("adopt-owned");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let directory = FakeDirectory::acme(log.clone());
    let (transport, _v) = transport_with_deploy(log.clone());

    let owned = rig.work.0.join("skills").join("deploy");
    std::fs::create_dir_all(&owned).unwrap();
    std::fs::write(owned.join("SKILL.md"), b"# deploy\n").unwrap(); // identical to the incoming
    let other_id = {
        let inert_p = InertPlane;
        let inert_f = InertFollow;
        let ctx = rig.ctx(&inert_p, &inert_f);
        ops::add_with_name(&ctx, &owned, Some("deploy"))
            .unwrap()
            .skill_id
    };

    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Described { describe, .. } = out else {
        panic!("bare = describe");
    };
    assert!(
        describe.adoptions.is_empty(),
        "another record's dir is never adopted: {:?}",
        describe.adoptions
    );
    assert_eq!(describe.collisions.len(), 1);
    assert_eq!(describe.collisions[0].installs_as, "deploy-acme");

    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme".to_owned()],
        opts(true),
    )
    .unwrap();
    let ops::FollowOutcome::Applied(applied) = out else {
        panic!("--yes = apply");
    };
    assert_eq!(
        applied
            .installed
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        vec!["deploy"]
    );
    let namespaced = rig.work.0.join("skills").join("deploy-acme");
    assert_eq!(
        std::fs::read(namespaced.join("SKILL.md")).unwrap(),
        b"# deploy\n"
    );
    // Each record owns exactly its own dir.
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    let map = crate::doc::read_map(&rig.fs, &rig.layout().published(&sid).map)
        .unwrap()
        .unwrap();
    assert_eq!(
        map.placements,
        vec![namespaced.to_string_lossy().into_owned()]
    );
    let other_sid = crate::id::SkillId::parse(&other_id).unwrap();
    let other_map = crate::doc::read_map(&rig.fs, &rig.layout().published(&other_sid).map)
        .unwrap()
        .unwrap();
    // The record stores the CANONICALIZED adopt source (macOS temp roots are symlinked).
    assert_eq!(
        other_map.placements,
        vec![owned.canonicalize().unwrap().to_string_lossy().into_owned()],
        "the other skill's record is untouched"
    );
}

#[test]
fn a_deleted_dir_recorded_by_another_skill_stays_reserved() {
    // Skill A's record owns the by-name dir but the dir was manually DELETED: the free-LOOKING
    // path must stay A's — a same-named arrival lands suffixed (never claims it), and A's later
    // converge re-lands A's bytes there without touching the arrival's dir.
    let rig = Rig::new("recorded-free");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let directory = FakeDirectory::acme(log.clone());
    let (transport, _v) = transport_with_deploy(log.clone());

    // Skill A: tracked at the by-name dir; then its dir is deleted on disk.
    let owned = rig.work.0.join("skills").join("deploy");
    std::fs::create_dir_all(&owned).unwrap();
    std::fs::write(owned.join("SKILL.md"), b"# A's deploy\n").unwrap();
    let a_id = {
        let inert_p = InertPlane;
        let inert_f = InertFollow;
        let ctx = rig.ctx(&inert_p, &inert_f);
        ops::add_with_name(&ctx, &owned, Some("deploy"))
            .unwrap()
            .skill_id
    };
    std::fs::remove_dir_all(&owned).unwrap();

    // The arrival's `--yes` lands under the suffix — the deleted dir is still A's.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme".to_owned()],
        opts(true),
    )
    .unwrap();
    let ops::FollowOutcome::Applied(applied) = out else {
        panic!("--yes = apply");
    };
    assert_eq!(
        applied
            .installed
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        vec!["deploy"]
    );
    let namespaced = rig.work.0.join("skills").join("deploy-acme");
    assert_eq!(
        std::fs::read(namespaced.join("SKILL.md")).unwrap(),
        b"# deploy\n"
    );
    assert!(!owned.exists(), "the reserved path is never claimed");
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    let map = crate::doc::read_map(&rig.fs, &rig.layout().published(&sid).map)
        .unwrap()
        .unwrap();
    assert_eq!(
        map.placements,
        vec![namespaced.to_string_lossy().into_owned()]
    );

    // A's record is untouched; its converge re-lands A's bytes at the reserved path without
    // touching the arrival's dir.
    let a_sid = crate::id::SkillId::parse(&a_id).unwrap();
    let a_lock: topos_types::persisted::Lock =
        crate::doc::read_doc(&rig.fs, &rig.layout().published(&a_sid).lock)
            .unwrap()
            .unwrap();
    {
        let inert_p = InertPlane;
        let inert_f = InertFollow;
        let ctx = rig.ctx(&inert_p, &inert_f);
        ops::apply_scope_change(
            &ctx,
            &a_sid,
            &a_lock,
            crate::placement::AgentScope::default(),
        )
        .unwrap();
    }
    assert_eq!(
        std::fs::read(owned.join("SKILL.md")).unwrap(),
        b"# A's deploy\n",
        "A's bytes re-land at A's recorded dir"
    );
    assert_eq!(
        std::fs::read(namespaced.join("SKILL.md")).unwrap(),
        b"# deploy\n",
        "the arrival's dir is untouched"
    );
    let a_map = crate::doc::read_map(&rig.fs, &rig.layout().published(&a_sid).map)
        .unwrap()
        .unwrap();
    assert_eq!(
        a_map.placements,
        vec![owned.canonicalize().unwrap().to_string_lossy().into_owned()]
    );
}

// ---------------------------------------------------------------------------------------------
// The --yes apply: the row op + the batch-accepted reconcile + the report.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_yes_apply_joins_then_lands_the_delivered_set_and_reports() {
    let rig = Rig::new("apply");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let directory = FakeDirectory::acme(log.clone());
    // Post-join, the delivery serves the channel's skill; the transport serves its REAL bytes.
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n")]);
    let mut transport = FakeTransport::empty(log.clone());
    transport.snapshot.skills.push(DeliverySkill {
        skill_id: "s_deploy".into(),
        name: "deploy".into(),
        review_required: false,
        version_id: v.id,
        generation: 1,
        bundle_digest: v.digest,
        via_channels: vec!["eng".into()],
        via_direct: false,
    });
    transport.versions.insert("s_deploy".into(), v.fetched);

    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme/channels/eng".to_owned()],
        opts(true),
    )
    .unwrap();
    let ops::FollowOutcome::Applied(applied) = out else {
        panic!("--yes = apply");
    };
    assert_eq!(applied.subscribed.len(), 1);
    assert_eq!(applied.subscribed[0].name, "eng");
    // The first receive was BATCH-ACCEPTED this invocation: the bytes are on disk under the
    // catalog name, and the applied report went out.
    assert_eq!(applied.installed.len(), 1);
    assert_eq!(applied.installed[0].name, "deploy");
    assert_eq!(
        std::fs::read(rig.work.0.join("skills").join("deploy").join("SKILL.md")).unwrap(),
        b"# deploy\n"
    );
    let l = log.lock().unwrap();
    assert!(l.iter().any(|e| e == "join eng"), "{l:?}");
    assert!(l.iter().any(|e| e == "report 1"), "the fleet report: {l:?}");
}

// ---------------------------------------------------------------------------------------------
// Target-scoped consent: a targeted `--yes` lands ONLY its named target's set — waiting arrivals
// (delivered-but-never-received skills outside the target) are neither described nor installed.
// ---------------------------------------------------------------------------------------------

/// A transport whose delivery carries a WAITING ARRIVAL — `s_extra` (delivered via a standing
/// direct follow, e.g. the person's own publish from another device; never received here) — beside
/// the channel skill `s_deploy` (via #eng). Both serve real bytes.
fn transport_with_waiting_arrival(log: CallLog) -> FakeTransport {
    let v_deploy = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n")]);
    let v_extra = mk_version(&[("SKILL.md", FileMode::Regular, b"# extra\n")]);
    let mut transport = FakeTransport::empty(log);
    transport.snapshot.skills.push(DeliverySkill {
        skill_id: "s_deploy".into(),
        name: "deploy".into(),
        review_required: false,
        version_id: v_deploy.id,
        generation: 1,
        bundle_digest: v_deploy.digest,
        via_channels: vec!["eng".into()],
        via_direct: false,
    });
    transport.snapshot.skills.push(DeliverySkill {
        skill_id: "s_extra".into(),
        name: "extra".into(),
        review_required: false,
        version_id: v_extra.id,
        generation: 1,
        bundle_digest: v_extra.digest,
        via_channels: Vec::new(),
        via_direct: true,
    });
    transport
        .versions
        .insert("s_deploy".into(), v_deploy.fetched);
    transport.versions.insert("s_extra".into(), v_extra.fetched);
    transport
}

/// The `acme` directory extended with the waiting arrival's catalog entry, so `extra` resolves as
/// a skill target too.
fn directory_with_extra(log: CallLog) -> FakeDirectory {
    let mut directory = FakeDirectory::acme(log);
    directory.skills.push(skill_entry("s_extra", "extra"));
    directory
}

#[test]
fn a_targeted_channel_yes_never_sweeps_in_a_waiting_arrival() {
    // The union regression: bare describes (a skill's, a channel's), then a targeted `--yes` on the
    // CHANNEL. The `--yes` must land exactly the channel's set — the waiting arrival `extra`
    // (delivered via a standing direct follow, never consented here) stays un-listed, un-followed,
    // and un-materialized; a LATER targeted `--yes` on it still lands it.
    let rig = Rig::new("scoped-channel");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let directory = directory_with_extra(log.clone());
    let transport = transport_with_waiting_arrival(log.clone());

    // Exploratory BARE describes first (the observed shape): the skill's, then the channel's.
    // Neither mutates anything, and neither's disclosure leaks into the other's consent.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme/skills/extra".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Described { describe, .. } = out else {
        panic!("bare = describe");
    };
    assert_eq!(
        describe
            .installs
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        vec!["extra"],
        "the skill describe lists ONLY its named target"
    );
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme/channels/eng".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Described { describe, .. } = out else {
        panic!("bare = describe");
    };
    assert_eq!(
        describe
            .installs
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        vec!["deploy"],
        "the channel describe lists ONLY the channel's set — never the waiting arrival"
    );

    // The targeted `--yes` on the CHANNEL: exactly the channel's set lands.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme/channels/eng".to_owned()],
        opts(true),
    )
    .unwrap();
    let ops::FollowOutcome::Applied(applied) = out else {
        panic!("--yes = apply");
    };
    assert_eq!(applied.subscribed.len(), 1);
    assert_eq!(applied.subscribed[0].name, "eng");
    assert_eq!(
        applied
            .installed
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        vec!["deploy"],
        "the channel's set landed and NOTHING else"
    );
    // The waiting arrival stayed waiting: no follow entry, no sidecar baseline, no bytes.
    let follows = enroll::read_follows(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert!(
        follows.follows.iter().all(|e| e.skill_id != "s_extra"),
        "no subscription state for the un-named arrival: {:?}",
        follows.follows
    );
    assert!(
        !rig.work.0.join("skills").join("extra").exists(),
        "the un-named arrival's bytes never materialized"
    );
    assert!(
        log.lock().unwrap().iter().all(|e| e != "follow s_extra"),
        "no direct-follow row was written for the un-named arrival: {:?}",
        log.lock().unwrap()
    );

    // A LATER targeted `--yes` on the waiting arrival still works — individually consentable.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme/skills/extra".to_owned()],
        opts(true),
    )
    .unwrap();
    let ops::FollowOutcome::Applied(applied) = out else {
        panic!("--yes = apply");
    };
    assert_eq!(
        applied
            .installed
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        vec!["extra"],
        "the later targeted consent lands exactly its target"
    );
    assert_eq!(
        std::fs::read(rig.work.0.join("skills").join("extra").join("SKILL.md")).unwrap(),
        b"# extra\n"
    );
}

#[test]
fn a_targeted_skill_yes_with_several_waiting_arrivals_installs_exactly_one() {
    // Several waiting catalog arrivals; `follow <one-skill> --yes` installs exactly the named one.
    let rig = Rig::new("scoped-skill");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let directory = directory_with_extra(log.clone());
    // BOTH skills are waiting arrivals (delivered via #everyone, never received here).
    let mut transport = transport_with_waiting_arrival(log.clone());
    for ds in &mut transport.snapshot.skills {
        ds.via_channels = vec!["everyone".into()];
        ds.via_direct = false;
    }

    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme/skills/extra".to_owned()],
        opts(true),
    )
    .unwrap();
    let ops::FollowOutcome::Applied(applied) = out else {
        panic!("--yes = apply");
    };
    assert_eq!(
        applied
            .installed
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        vec!["extra"],
        "exactly the named skill landed"
    );
    // The attribution stays honest: the named skill's existing delivery lane is disclosed.
    assert_eq!(
        applied.installed[0].via_channels,
        vec!["everyone".to_owned()]
    );
    assert!(
        applied.installed[0].via_direct,
        "a skill target follows direct"
    );
    assert!(
        !rig.work.0.join("skills").join("deploy").exists(),
        "the OTHER waiting arrival stayed waiting"
    );
    let follows = enroll::read_follows(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert!(
        follows.follows.iter().all(|e| e.skill_id != "s_deploy"),
        "no subscription state for the un-named arrival: {:?}",
        follows.follows
    );
}

#[test]
fn a_workspace_yes_still_lands_the_whole_delivered_set() {
    // The enrollment/workspace consent is the one place the whole delivered set is disclosed —
    // and its `--yes` still lands all of it (the two-call enrollment flow depends on this).
    let rig = Rig::new("scoped-workspace");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let directory = directory_with_extra(log.clone());
    let transport = transport_with_waiting_arrival(log.clone());

    // The bare workspace describe lists BOTH pending deliveries.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Described { describe, .. } = out else {
        panic!("bare = describe");
    };
    let mut described: Vec<&str> = describe.installs.iter().map(|i| i.name.as_str()).collect();
    described.sort_unstable();
    assert_eq!(described, vec!["deploy", "extra"]);

    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme".to_owned()],
        opts(true),
    )
    .unwrap();
    let ops::FollowOutcome::Applied(applied) = out else {
        panic!("--yes = apply");
    };
    let mut landed: Vec<&str> = applied.installed.iter().map(|i| i.name.as_str()).collect();
    landed.sort_unstable();
    assert_eq!(
        landed,
        vec!["deploy", "extra"],
        "the workspace-consented apply lands the whole disclosed set"
    );
    assert!(rig.work.0.join("skills").join("deploy").exists());
    assert!(rig.work.0.join("skills").join("extra").exists());
}

// ---------------------------------------------------------------------------------------------
// The standing no-op receipt + the enrolled receipt: a describe that would change nothing offers
// no apply argv (the standing note carries the fact); an enrolling describe names the principal
// and never claims "nothing has changed".
// ---------------------------------------------------------------------------------------------

#[test]
fn a_standing_follow_with_nothing_new_offers_no_apply_argv() {
    let rig = Rig::new("standing");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    // An extra channel with NO skills: `design` is already a member (no new row), `ops` is not
    // (a join row would be new even with nothing to install).
    let mut directory = FakeDirectory::acme(log.clone());
    directory
        .channels
        .push(channel_entry("ops", false, false, &[]));
    let transport = FakeTransport::empty(log.clone());

    // The workspace target: no installs, no new rows → the standing note, no `--yes` offered.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Described {
        describe,
        next_argvs,
    } = out
    else {
        panic!("bare = describe");
    };
    assert!(next_argvs.is_empty(), "{next_argvs:?}");
    let note = describe.standing_note.as_deref().expect("standing note");
    assert!(note.starts_with("nothing new to install"), "{note}");
    assert!(
        note.contains("every device you've linked to this workspace"),
        "{note}"
    );
    // The TTY carries the standing fact and neither the classic no-change line nor an argv.
    let text = crate::render::follow_describe_tty(&describe, &next_argvs);
    assert!(text.contains("nothing new to install"), "{text}");
    assert!(!text.contains("Nothing has changed yet"), "{text}");
    assert!(!text.contains("--yes"), "{text}");
    assert!(
        crate::render::describe_next_actions(Vec::new()).is_empty(),
        "an empty argv list yields no next actions"
    );

    // An ALREADY-MEMBER channel target with nothing to install: same suppression.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme/channels/design".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Described {
        describe,
        next_argvs,
    } = out
    else {
        panic!("bare = describe");
    };
    assert!(next_argvs.is_empty(), "{next_argvs:?}");
    assert!(describe.standing_note.is_some());

    // A NOT-YET-JOINED channel writes a row even with nothing to install → the argv is offered.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme/channels/ops".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Described {
        describe,
        next_argvs,
    } = out
    else {
        panic!("bare = describe");
    };
    assert_eq!(next_argvs.len(), 1, "{next_argvs:?}");
    assert!(describe.standing_note.is_none());
}

#[test]
fn the_enrolled_receipt_names_the_principal_and_never_claims_nothing_changed() {
    // The two-call enroll against an EMPTY delivery: the receipt leads with WHO this device now
    // acts as, and folds the standing fact into the Following line.
    let rig = Rig::new("enroll-receipt");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let directory = FakeDirectory::acme(log.clone());
    let transport = FakeTransport::empty(log.clone());
    run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme".to_owned()],
        opts(false),
    )
    .unwrap();
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        Vec::new(),
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Described {
        describe,
        next_argvs,
    } = out
    else {
        panic!("resume lands on the describe");
    };
    assert!(describe.enrolled_now);
    let text = crate::render::follow_describe_tty(&describe, &next_argvs);
    assert!(
        text.contains(
            "Enrolled this device as alice@acme.com — role: member — invited by robert@acme.com."
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "Following: workspace acme — nothing new to install; new team skills arrive \
             automatically on every device you've linked to this workspace."
        ),
        "{text}"
    );
    assert!(!text.contains("Your role:"), "{text}");
    assert!(
        !text.contains("Nothing has changed yet"),
        "the enrollment persisted a credential and armed the trigger: {text}"
    );

    // With an install waiting, the enrolled describe offers the apply under the honest heading.
    let rig = Rig::new("enroll-receipt-installs");
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let directory = FakeDirectory::acme(log.clone());
    let (transport, _v) = transport_with_deploy(log.clone());
    run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme".to_owned()],
        opts(false),
    )
    .unwrap();
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        Vec::new(),
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::Described {
        describe,
        next_argvs,
    } = out
    else {
        panic!("resume lands on the describe");
    };
    assert!(describe.enrolled_now && describe.standing_note.is_none());
    assert_eq!(next_argvs.len(), 1);
    let text = crate::render::follow_describe_tty(&describe, &next_argvs);
    assert!(text.contains("Apply the follow with:"), "{text}");
    assert!(!text.contains("Nothing has changed yet"), "{text}");
}

// ---------------------------------------------------------------------------------------------
// unfollow: the workspace / everyone refusals, the skill detach row + the local pause.
// ---------------------------------------------------------------------------------------------

fn run_unfollow(
    rig: &Rig,
    directory: &FakeDirectory,
    transport: &FakeTransport,
    targets: &[String],
    yes: bool,
) -> Result<ops::UnfollowOutcome, ClientError> {
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let dir_connect = |_b: &str| -> Box<dyn DirectorySource> { Box::new(directory.clone()) };
    let del_connect = |_b: &str| -> Box<dyn ReconcileTransport> { Box::new(transport.clone()) };
    let connectors = ops::UnfollowConnectors {
        directory: &dir_connect,
        delivery: &del_connect,
    };
    ops::unfollow(&ctx, &connectors, targets, &[], &[], yes)
}

#[test]
fn unfollow_refuses_a_workspace_toward_the_web_and_everyone_with_alternatives() {
    let rig = Rig::new("unf-refusals");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let directory = FakeDirectory::acme(log.clone());
    let transport = FakeTransport::empty(log.clone());

    // The workspace target: recognized, and refused toward the web — never "not found".
    let err = run_unfollow(&rig, &directory, &transport, &["acme".to_owned()], true).unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    assert!(err.to_string().contains("web action"), "{err}");

    // The structural everyone: refused with the alternatives spelled.
    let err =
        run_unfollow(&rig, &directory, &transport, &["everyone".to_owned()], true).unwrap_err();
    assert!(err.to_string().contains("structural"), "{err}");
    assert!(err.to_string().contains("topos remove"), "{err}");
    // Neither refusal wrote a row.
    assert!(log.lock().unwrap().iter().all(|e| !e.starts_with("leave")));
}

#[test]
fn unfollow_skill_writes_the_detach_row_and_flips_the_local_pause() {
    let rig = Rig::new("unf-skill");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let directory = FakeDirectory::acme(log.clone());
    let transport = FakeTransport::empty(log.clone());
    // The skill is followed locally (install-state), as a reconcile would have recorded it.
    enroll::write_follows_merged(
        &rig.fs,
        &rig.layout(),
        &[enroll::FollowEntry {
            skill_id: "s_docs".to_owned(),
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

    // A bare SKILL unfollow APPLIES immediately: the server row + the local pause flip; bytes
    // stay (nothing else on disk moves); the receipt leads with its undo.
    let out = run_unfollow(&rig, &directory, &transport, &["docs".to_owned()], false).unwrap();
    let ops::UnfollowOutcome::Applied(applied) = out else {
        panic!("a skill unfollow applies immediately");
    };
    assert!(applied.bytes_kept);
    assert_eq!(applied.items.len(), 1);
    assert_eq!(applied.items[0].kind, "skill");
    assert_eq!(applied.items[0].stops, vec!["docs".to_owned()]);
    // The undo target rides QUALIFIED — a same-named skill in a second workspace must not turn
    // the promised inverse into an ambiguity refusal.
    assert_eq!(applied.undo, vec!["topos", "follow", "acme/skills/docs"]);
    assert!(
        log.lock().unwrap().iter().any(|e| e == "unfollow s_docs"),
        "the person-scoped detach row was written"
    );
    let follows = enroll::read_follows(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert!(
        !follows
            .follows
            .iter()
            .find(|e| e.skill_id == "s_docs")
            .unwrap()
            .following,
        "the local pause flipped alongside the row"
    );
}

// ---------------------------------------------------------------------------------------------
// The hook posture: the staleness warning line, the soft-failure classifier, and the
// narrated-then-acked vs fetch-without-ack notices split.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_quiet_hook_warns_once_stale_and_unreachable_and_stays_silent_otherwise() {
    let rig = Rig::new("quiet-stale");
    let window = 10_000u64;
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            WS.to_owned(),
            sync_status::WorkspaceSync {
                last_delivery_at: Some(1_000),
                last_report_at: Some(1_000),
                staleness_window_ms: window,
                ..Default::default()
            },
        )],
    )
    .unwrap();

    let out = ops::PullOutcome {
        data: topos_types::results::PullData {
            skills: Vec::new(),
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
        },
        warnings: Vec::new(),
        access_gone: Vec::new(),
        unreachable: vec![WS.to_owned()],
    };
    // Unreachable AND stale → exactly one warning line naming the age.
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), 1_000 + 60_000 * 90, &out);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("topos: w_acme last synced 1h ago"),
        "{lines:?}"
    );
    assert!(lines[0].contains("server unreachable"), "{lines:?}");
    // Unreachable but INSIDE the window → silent (a blip must not spam every session).
    assert!(ops::quiet_hook_lines(&rig.fs, &rig.layout(), 2_000, &out).is_empty());
    // The access-gone freeze always earns its line.
    let gone = ops::PullOutcome {
        data: topos_types::results::PullData {
            skills: Vec::new(),
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
        },
        warnings: Vec::new(),
        access_gone: vec![WS.to_owned()],
        unreachable: Vec::new(),
    };
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), 2_000, &gone);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("frozen in place"), "{lines:?}");

    // The soft-failure classifier: auth/transport exit 0; local corruption still fails loud.
    assert!(ops::quiet_soft_failure(&ClientError::Plane("dial".into())));
    assert!(ops::quiet_soft_failure(&ClientError::Enrollment(
        "no creds".into()
    )));
    assert!(ops::quiet_soft_failure(&ClientError::TargetNotFound {
        target: "w".into()
    }));
    assert!(!ops::quiet_soft_failure(&ClientError::Corrupt(
        "sidecar".into()
    )));
    assert!(!ops::quiet_soft_failure(&ClientError::Io("disk".into())));
}

#[test]
fn notices_are_returned_and_acked_interactively_but_fetched_without_ack_by_the_hook() {
    let rig = Rig::new("notices");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let mut transport = FakeTransport::empty(log.clone());
    transport.snapshot.notices = vec![WireNotice {
        id: "ntc_1".into(),
        kind: "verdict".into(),
        skill_id: Some("s_docs".into()),
        skill_name: Some("docs".into()),
        version_id: None,
        actor: Some("robert@acme.com".into()),
        outcome: Some("approve".into()),
        reason: Some("looks good".into()),
        message: None,
        created_at: "2026-07-11T00:00:00Z".into(),
    }];
    let inert_p = InertPlane;
    let inert_f = InertFollow;

    // The interactive/`--json` posture: notices ride the data AND the ack goes out for exactly them.
    let ctx = rig.ctx(&inert_p, &inert_f);
    let out = ops::pull_reconcile_with(
        &ctx,
        &transport,
        &ops::ReconcileOpts {
            ack_notices: true,
            ..ops::ReconcileOpts::default()
        },
    )
    .unwrap();
    assert_eq!(out.data.notices.len(), 1);
    assert_eq!(out.data.notices[0].kind, "verdict");
    assert!(
        log.lock().unwrap().iter().any(|e| e == "ack ntc_1"),
        "the narrated ids were acked: {:?}",
        log.lock().unwrap()
    );
    // The sweep also recorded the freshness doc (delivery + report + the window).
    let status = sync_status::read(&rig.fs, &rig.layout()).unwrap();
    let entry = status.workspaces.get(WS).expect("the workspace entry");
    assert!(entry.last_delivery_at.is_some() && entry.last_report_at.is_some());
    assert_eq!(entry.staleness_window_ms, 604_800_000);
    assert_eq!(
        out.data.sync.len(),
        1,
        "the freshness mirrors onto the payload"
    );

    // The quiet hook posture: fetched, never acked.
    log.lock().unwrap().clear();
    let out = ops::pull_reconcile_with(&ctx, &transport, &ops::ReconcileOpts::default()).unwrap();
    assert_eq!(out.data.notices.len(), 1, "still fetched");
    assert!(
        log.lock().unwrap().iter().all(|e| !e.starts_with("ack")),
        "the hook never acks: {:?}",
        log.lock().unwrap()
    );
}

#[test]
fn a_pending_link_delivery_is_skipped_quietly_and_marks_the_wait() {
    // No data flows over a PENDING device↔workspace link: the sweep skips the workspace QUIETLY —
    // no rows, no warnings, no report PUT, no freshness stamp. A `status`-visible fact, not an
    // error; the local membership records the wait.
    let rig = Rig::new("pending-link");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let mut transport = FakeTransport::empty(log.clone());
    transport.snapshot.link_status = crate::plane::LinkStatus::Pending;
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let out = ops::pull_reconcile_with(&ctx, &transport, &ops::ReconcileOpts::default()).unwrap();
    assert!(out.data.skills.is_empty());
    assert!(out.warnings.is_empty(), "quiet: {:?}", out.warnings);
    assert!(out.access_gone.is_empty() && out.unreachable.is_empty());
    assert!(
        !log.lock().unwrap().iter().any(|e| e.starts_with("report")),
        "no report PUT rides a pending link: {:?}",
        log.lock().unwrap()
    );
    assert!(
        sync_status::read(&rig.fs, &rig.layout())
            .unwrap()
            .workspaces
            .is_empty(),
        "no freshness stamp for a pending workspace"
    );
    let user = enroll::read_user(&rig.fs, &rig.layout()).unwrap().unwrap();
    assert_eq!(
        user.membership(WS).unwrap().link_status,
        enroll::LINK_PENDING
    );

    // Approval lands: the next delivering sweep SELF-HEALS the local record back to active.
    transport.snapshot.link_status = crate::plane::LinkStatus::Active;
    let out = ops::pull_reconcile_with(&ctx, &transport, &ops::ReconcileOpts::default()).unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let user = enroll::read_user(&rig.fs, &rig.layout()).unwrap().unwrap();
    assert_eq!(
        user.membership(WS).unwrap().link_status,
        enroll::LINK_ACTIVE
    );
}

#[test]
fn a_pending_workspace_that_404s_types_once_and_ends_the_membership() {
    // The link request wasn't approved (or the workspace is gone): ONE typed line — never the
    // freeze (nothing was ever delivered) — and the membership is marked ENDED so it prints once.
    // An ACTIVE workspace that 404s keeps the freeze, with the generic-causes copy.
    let rig = Rig::new("pending-404");
    rig.seed_enrolled();
    enroll::set_membership_link_status(&rig.fs, &rig.layout(), WS, enroll::LINK_PENDING).unwrap();
    struct GoneDelivery;
    impl DeliverySource for GoneDelivery {
        fn workspaces(&self) -> Vec<String> {
            vec![WS.to_owned()]
        }
        fn fetch_delivery(&self, _ws: &str) -> Result<DeliverySnapshot, PlaneError> {
            Err(PlaneError::NotFound)
        }
        fn report_applied(&self, _ws: &str, _a: &[(String, [u8; 32])]) -> Result<(), PlaneError> {
            unreachable!("a 404'd workspace never reports")
        }
    }
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let out =
        ops::pull_reconcile_with(&ctx, &GoneDelivery, &ops::ReconcileOpts::default()).unwrap();
    assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
    assert!(
        out.warnings[0].starts_with("LINK_ENDED w_acme"),
        "{:?}",
        out.warnings
    );
    assert!(
        out.warnings[0].contains("topos follow acme"),
        "the relink hint names the ADDRESS: {:?}",
        out.warnings
    );
    assert!(
        out.access_gone.is_empty(),
        "nothing was delivered, so nothing freezes"
    );
    let user = enroll::read_user(&rig.fs, &rig.layout()).unwrap().unwrap();
    assert_eq!(user.membership(WS).unwrap().link_status, enroll::LINK_ENDED);

    // The second sweep stays SILENT — the line printed once (and the production fan-out drops an
    // ended membership entirely).
    let out =
        ops::pull_reconcile_with(&ctx, &GoneDelivery, &ops::ReconcileOpts::default()).unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(out.access_gone.is_empty());

    // An ACTIVE link that 404s keeps the FREEZE (unlinked, removed, or gone — indistinguishable).
    enroll::set_membership_link_status(&rig.fs, &rig.layout(), WS, enroll::LINK_ACTIVE).unwrap();
    let out =
        ops::pull_reconcile_with(&ctx, &GoneDelivery, &ops::ReconcileOpts::default()).unwrap();
    assert_eq!(out.access_gone, vec![WS.to_owned()]);
    assert!(
        out.warnings[0].contains("unlinked, removed, or gone"),
        "{:?}",
        out.warnings
    );
}

#[test]
fn an_excluded_skills_unreadable_sync_doc_warns_and_never_aborts_the_quiet_sweep() {
    // The quiet hook must never die on one poisoned sidecar doc. An excluded skill whose sync.json is
    // unreadable (e.g. a downgrade past a bumped doc schema) is isolated to a per-skill warning — the
    // sweep still runs to completion and writes its freshness/report, exactly like the freeze and
    // withdraw arms already do.
    let rig = Rig::new("excluded-corrupt");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    // The skill is followed + excluded here, and NOT in the delivered set → the undelivered classifier
    // hits the excluded arm, which reads its sync.json.
    let follow = crate::plane_http::FileFollow::new(vec![(
        "s_docs".to_owned(),
        crate::plane::FollowContext {
            workspace_id: WS.to_owned(),
            mode: crate::plane::FollowMode::Auto,
            review_required: false,
            following: true,
            agents: Vec::new(),
            excluded_agents: Vec::new(),
        },
    )]);
    // Poison the skill's sync.json with an unknown schema version — the fail-closed doc load refuses it.
    let sp = rig
        .layout()
        .published(&crate::id::SkillId::parse("s_docs").unwrap());
    std::fs::create_dir_all(sp.sync.parent().unwrap()).unwrap();
    std::fs::write(&sp.sync, br#"{"schema_version":9999}"#).unwrap();

    let mut transport = FakeTransport::empty(log.clone());
    transport.snapshot.excluded = vec!["s_docs".to_owned()];

    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_p, &follow);
    // The sweep completes (Ok, not Err): the corrupt doc is a per-skill warning, and the freshness doc
    // still lands — the hook exits 0.
    let out = ops::pull_reconcile_with(&ctx, &transport, &ops::ReconcileOpts::default()).unwrap();
    assert!(
        out.warnings.iter().any(|w| w.contains("s_docs")),
        "the poisoned skill is a warning, not a fatal abort: {:?}",
        out.warnings
    );
    let status = sync_status::read(&rig.fs, &rig.layout()).unwrap();
    assert!(
        status.workspaces.contains_key(WS),
        "the sweep ran to completion and recorded freshness despite the poisoned skill"
    );
}

#[test]
fn the_pull_action_vocabulary_covers_the_applied_filter() {
    // A guard for the apply's landed filter: the actions it treats as "landed" exist and the
    // detached/excluded ones stay out (a rename in the vocabulary should break THIS test, not the
    // filter silently).
    for landed in [
        PullAction::FastForwarded,
        PullAction::UpToDate,
        PullAction::Merged,
    ] {
        assert!(matches!(
            landed,
            PullAction::FastForwarded | PullAction::UpToDate | PullAction::Merged
        ));
    }
}

// ---------------------------------------------------------------------------------------------
// The per-device exclusion LIFT — `follow <skill>` re-attaches a skill `remove` excluded here.
// ---------------------------------------------------------------------------------------------

/// One workspace's transport whose delivery serves `s_deploy` (real bytes) — a `follow` installs it,
/// `remove` excludes it, and a later `follow` re-attaches.
fn deploy_transport(log: CallLog) -> FakeTransport {
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n")]);
    let mut transport = FakeTransport::empty(log);
    transport.snapshot.skills.push(DeliverySkill {
        skill_id: "s_deploy".into(),
        name: "deploy".into(),
        review_required: false,
        version_id: v.id,
        generation: 1,
        bundle_digest: v.digest,
        via_channels: vec!["eng".into()],
        via_direct: false,
    });
    transport.versions.insert("s_deploy".into(), v.fetched);
    transport
}

/// Drive the CANONICAL post-`remove` state a device holds when it excluded a followed skill: first a
/// `follow acme/channels/eng --yes` installs `deploy` (sidecar + agent dir + follow entry), then
/// `remove deploy --yes` writes the server exclusion, cleans the agent dir, resets the sync state to
/// the never-received baseline, and marks `excluded_here`. Leaves the tracked sidecar in place.
fn seed_excluded_deploy(rig: &Rig, directory: &FakeDirectory, transport: &FakeTransport) {
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: Arc::default(),
    };
    // Install deploy fresh (a channel follow — a delivered first-receive lands under `--yes`).
    let out = run_follow(
        rig,
        &enroll_fake,
        directory,
        transport,
        vec!["acme/channels/eng".to_owned()],
        opts(true),
    )
    .unwrap();
    assert!(
        matches!(&out, ops::FollowOutcome::Applied(a) if a.installed.iter().any(|i| i.name == "deploy")),
        "the seed install landed deploy",
    );
    assert!(
        rig.work
            .0
            .join("skills")
            .join("deploy")
            .join("SKILL.md")
            .exists(),
        "the seed left deploy in the agent dir",
    );
    // Exclude it on THIS device — the exact `remove` path (server row + clean + reset + marker).
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let dir_connect = |_b: &str| -> Box<dyn DirectorySource> { Box::new(directory.clone()) };
    let connectors = ops::RemoveConnectors {
        directory: &dir_connect,
    };
    ops::remove(&ctx, &connectors, &["deploy".to_owned()], &[], None, true).unwrap();
    assert!(
        !rig.work.0.join("skills").join("deploy").exists(),
        "remove cleaned the agent dir",
    );
    let e = follow_entry(rig, "s_deploy");
    assert!(
        e.following && e.excluded_here,
        "excluded but still followed"
    );
}

fn follow_entry(rig: &Rig, skill_id: &str) -> enroll::FollowEntry {
    enroll::read_follows(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap()
        .follows
        .into_iter()
        .find(|e| e.skill_id == skill_id)
        .unwrap_or_else(|| panic!("no follow entry for {skill_id}"))
}

/// The first half of [`seed_excluded_deploy`]: install `deploy` fresh through a consented channel
/// follow, leaving it followed + materialized (no exclusion).
fn seed_installed_deploy(rig: &Rig, directory: &FakeDirectory, transport: &FakeTransport) {
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: Arc::default(),
    };
    let out = run_follow(
        rig,
        &enroll_fake,
        directory,
        transport,
        vec!["acme/channels/eng".to_owned()],
        opts(true),
    )
    .unwrap();
    assert!(
        matches!(&out, ops::FollowOutcome::Applied(a) if a.installed.iter().any(|i| i.name == "deploy")),
        "the seed install landed deploy",
    );
}

#[test]
fn a_bare_follow_of_an_unfollowed_skill_clears_the_stance_and_applies() {
    // Arm: previously-followed-then-unfollowed. The bare `follow <skill>` on an ENROLLED install
    // clears the person's unfollowed stance SERVER-side (the same `follow_skill` row op), resumes
    // the local entry, reinstalls, and answers the undo-led receipt — no describe phase.
    let rig = Rig::new("reattach-unfollowed");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let directory = FakeDirectory::acme(log.clone());
    let transport = deploy_transport(log.clone());
    seed_installed_deploy(&rig, &directory, &transport);

    // Unfollow it (a bare skill unfollow applies immediately now) — the paused stance.
    let out = run_unfollow(&rig, &directory, &transport, &["deploy".to_owned()], false).unwrap();
    assert!(matches!(out, ops::UnfollowOutcome::Applied(_)));
    assert!(!follow_entry(&rig, "s_deploy").following, "paused");
    log.lock().unwrap().clear();

    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["deploy".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::ReattachApplied(reattach) = out else {
        panic!("a previously-unfollowed skill re-attaches immediately");
    };
    assert_eq!(reattach.cause, "unfollowed");
    assert_eq!(
        reattach.undo,
        vec!["topos", "unfollow", "acme/skills/deploy"]
    );
    // The stance cleared SERVER-side (not just the local flag) via the follow_skill row op.
    assert!(
        log.lock().unwrap().iter().any(|e| e == "follow s_deploy"),
        "clearing the unfollowed stance rides `follow_skill`: {:?}",
        log.lock().unwrap()
    );
    assert!(
        follow_entry(&rig, "s_deploy").following,
        "the local entry resumed"
    );
    // The TTY receipt words the resume and leads with its undo (the qualified target).
    let text = crate::render::reattach_applied_tty(&reattach);
    assert!(text.contains("Following deploy again"), "{text}");
    assert!(
        text.contains("Undo: topos unfollow acme/skills/deploy"),
        "{text}"
    );
}

#[test]
fn remove_with_a_draft_holds_the_loss_guard_then_yes_applies() {
    // The loss-guard: a followed skill WITH local edits ahead keeps the two-phase describe (the
    // draft would leave every agent dir), with loss-led copy; `--yes` then applies snapshot-first.
    let rig = Rig::new("rm-loss-guard");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let directory = FakeDirectory::acme(log.clone());
    let transport = deploy_transport(log.clone());
    seed_installed_deploy(&rig, &directory, &transport);
    log.lock().unwrap().clear();

    // Edit the placed copy — a draft ahead of the followed version.
    let placed = rig.work.0.join("skills").join("deploy").join("SKILL.md");
    std::fs::write(&placed, "# deploy\nmy local edit\n").unwrap();

    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let ctx = rig.ctx(&inert_p, &inert_f);
    let dir_connect = |_b: &str| -> Box<dyn DirectorySource> { Box::new(directory.clone()) };
    let connectors = ops::RemoveConnectors {
        directory: &dir_connect,
    };

    // Bare = the loss-guard DESCRIBE: loss-led copy, nothing mutated.
    let out = ops::remove(&ctx, &connectors, &["deploy".to_owned()], &[], None, false).unwrap();
    let ops::RemoveOutcome::Described { data, yes_argv } = out else {
        panic!("a draft holds the gate");
    };
    assert!(!data.applied);
    let note = data.items[0].note.as_deref().unwrap_or_default();
    assert!(
        note.contains("local edits ahead"),
        "the describe leads with the loss: {note}"
    );
    assert!(yes_argv.contains(&"--yes".to_owned()));
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .all(|e| !e.starts_with("exclude")),
        "the describe wrote no row: {:?}",
        log.lock().unwrap()
    );
    assert!(placed.exists(), "the describe cleaned nothing");

    // `--yes` applies: the exclusion row lands, the dir is cleaned (draft snapshotted first).
    let out = ops::remove(&ctx, &connectors, &["deploy".to_owned()], &[], None, true).unwrap();
    assert!(matches!(out, ops::RemoveOutcome::Applied(d) if d.applied));
    assert!(
        log.lock().unwrap().iter().any(|e| e == "exclude s_deploy"),
        "the exclusion row was written"
    );
    assert!(!placed.exists(), "the dirs are cleaned on apply");
    assert!(follow_entry(&rig, "s_deploy").excluded_here);
}

#[test]
fn a_bare_follow_of_an_excluded_skill_applies_the_reattach_immediately() {
    let rig = Rig::new("reattach-bare");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let directory = FakeDirectory::acme(log.clone());
    let transport = deploy_transport(log.clone());
    seed_excluded_deploy(&rig, &directory, &transport);
    log.lock().unwrap().clear();

    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    // Bare `follow deploy` (no `--yes`): the skill was on this device's trust surface, so the
    // re-attach APPLIES immediately — the receipt leads with its undo.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["deploy".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::ReattachApplied(reattach) = out else {
        panic!("an excluded skill re-attaches immediately, never a first-receive offer");
    };
    assert_eq!(reattach.name, "deploy");
    assert_eq!(reattach.skill_id, "s_deploy");
    assert_eq!(reattach.cause, "excluded-here");
    assert!(reattach.installed, "the current bytes landed back");
    assert_eq!(reattach.undo, vec!["topos", "remove", "acme/skills/deploy"]);
    // The mutation is real: the server row op fired, the marker cleared, the bytes are back.
    assert!(
        log.lock().unwrap().iter().any(|e| e == "follow s_deploy"),
        "the lift rides `follow_skill`: {:?}",
        log.lock().unwrap()
    );
    assert!(
        !follow_entry(&rig, "s_deploy").excluded_here,
        "marker cleared"
    );
    assert!(rig.work.0.join("skills").join("deploy").exists());
    // The TTY receipt is undo-led (the qualified target).
    let text = crate::render::reattach_applied_tty(&reattach);
    assert!(text.contains("Re-attached deploy"), "{text}");
    assert!(
        text.contains("Undo: topos remove acme/skills/deploy"),
        "{text}"
    );
}

#[test]
fn a_yes_follow_of_an_excluded_skill_lifts_the_row_clears_the_marker_and_reinstalls() {
    let rig = Rig::new("reattach-apply");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let directory = FakeDirectory::acme(log.clone());
    let transport = deploy_transport(log.clone());
    seed_excluded_deploy(&rig, &directory, &transport);
    log.lock().unwrap().clear();

    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["deploy".to_owned()],
        opts(true),
    )
    .unwrap();
    let ops::FollowOutcome::ReattachApplied(reattach) = out else {
        panic!("--yes on an excluded skill applies the re-attach");
    };
    assert_eq!(reattach.name, "deploy");
    assert!(
        reattach.installed,
        "the current bytes landed back on this device"
    );
    // (a) the SERVER exclusion was lifted via the `follow_skill` row op (the same op the web speaks).
    let l = log.lock().unwrap();
    assert!(
        l.iter().any(|e| e == "follow s_deploy"),
        "the lift rides `follow_skill` (PUT follows): {l:?}",
    );
    drop(l);
    // (b) the local marker cleared; (c) the current bytes are back on disk.
    assert!(
        !follow_entry(&rig, "s_deploy").excluded_here,
        "marker cleared"
    );
    assert_eq!(
        std::fs::read(rig.work.0.join("skills").join("deploy").join("SKILL.md")).unwrap(),
        b"# deploy\n",
    );
}

#[test]
fn a_qualified_follow_path_reaches_the_same_reattach_arm() {
    let rig = Rig::new("reattach-qual");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let directory = FakeDirectory::acme(log.clone());
    let transport = deploy_transport(log.clone());
    seed_excluded_deploy(&rig, &directory, &transport);
    log.lock().unwrap().clear();

    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    // The qualified `<ws>/skills/<name>` form must reach the SAME re-attach arm, not replay a
    // person-scope subscribe that leaves the device exclusion standing.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme/skills/deploy".to_owned()],
        opts(true),
    )
    .unwrap();
    let ops::FollowOutcome::ReattachApplied(reattach) = out else {
        panic!("the qualified path re-attaches an excluded skill too");
    };
    assert!(reattach.installed);
    assert!(
        !follow_entry(&rig, "s_deploy").excluded_here,
        "marker cleared"
    );
    assert!(
        log.lock().unwrap().iter().any(|e| e == "follow s_deploy"),
        "the qualified lift rides the same row op",
    );
}

#[test]
fn a_bare_follow_of_a_never_followed_name_still_takes_the_offer_path() {
    // The boundary: a bare name that is NOT a followed-and-excluded skill is unaffected by the
    // re-attach arm — a never-followed catalog name still takes the ordinary offer/subscribe describe
    // (`FollowOutcome::Described`), never a re-attach.
    let rig = Rig::new("reattach-boundary");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let directory = FakeDirectory::acme(log.clone());
    let transport = FakeTransport::empty(log.clone());
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    // `docs` is a catalog skill this device has never followed — bare `follow docs` describes the
    // ordinary subscribe, not a re-attach.
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["docs".to_owned()],
        opts(false),
    )
    .unwrap();
    assert!(
        matches!(out, ops::FollowOutcome::Described { .. }),
        "a never-followed name still takes the subscribe/offer describe, not the re-attach arm",
    );
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .all(|e| !e.starts_with("follow ")),
        "a describe writes no row: {:?}",
        log.lock().unwrap()
    );
}

#[test]
fn a_qualified_bare_follow_of_an_excluded_skill_applies_immediately_too() {
    // The qualified `<ws>/skills/<name>` path reaches the SAME immediate re-attach arm as the bare
    // positional — one gate policy per stance, not per spelling.
    let rig = Rig::new("reattach-qual-bare");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let directory = FakeDirectory::acme(log.clone());
    let transport = deploy_transport(log.clone());
    seed_excluded_deploy(&rig, &directory, &transport);
    log.lock().unwrap().clear();

    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["acme/skills/deploy".to_owned()],
        opts(false),
    )
    .unwrap();
    let ops::FollowOutcome::ReattachApplied(reattach) = out else {
        panic!("the qualified excluded target re-attaches immediately");
    };
    assert_eq!(reattach.cause, "excluded-here");
    assert!(reattach.installed);
    assert!(
        log.lock().unwrap().iter().any(|e| e == "follow s_deploy"),
        "the lift rides the same row op: {:?}",
        log.lock().unwrap()
    );
}

#[test]
fn a_yes_reattach_installs_only_its_subject_never_a_teammates_new_skill() {
    // P1: after this device excluded `deploy`, a teammate shares a BRAND-NEW skill into #everyone. The
    // re-attach describe named only `deploy`, so `--yes` must install ONLY `deploy` — the never-disclosed
    // arrival gets no follow entry, no baseline, no bytes (it lands on the next full `update`/`follow`).
    let rig = Rig::new("reattach-only-subject");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let directory = FakeDirectory::acme(log.clone());
    let mut transport = deploy_transport(log.clone());
    seed_excluded_deploy(&rig, &directory, &transport);
    log.lock().unwrap().clear();

    // NOW a teammate shares `newthing` into #everyone (delivered after the exclusion, never received here).
    let znew = mk_version(&[("SKILL.md", FileMode::Regular, b"# secret\n")]);
    transport.snapshot.skills.push(DeliverySkill {
        skill_id: "s_new".into(),
        name: "newthing".into(),
        review_required: false,
        version_id: znew.id,
        generation: 1,
        bundle_digest: znew.digest,
        via_channels: vec!["everyone".into()],
        via_direct: false,
    });
    transport.versions.insert("s_new".into(), znew.fetched);

    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["deploy".to_owned()],
        opts(true),
    )
    .unwrap();
    let ops::FollowOutcome::ReattachApplied(reattach) = out else {
        panic!("--yes re-attaches the subject");
    };
    assert!(reattach.installed, "the subject `deploy` landed");
    assert!(
        rig.work
            .0
            .join("skills")
            .join("deploy")
            .join("SKILL.md")
            .exists(),
        "the subject's bytes are back on this device",
    );
    // The undisclosed arrival was NOT installed: no follow entry, no materialized bytes.
    let follows = enroll::read_follows(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert!(
        follows.follows.iter().all(|e| e.skill_id != "s_new"),
        "the re-attach wrote no follow entry for the undisclosed skill: {:?}",
        follows.follows,
    );
    assert!(
        !rig.work.0.join("skills").join("newthing").exists(),
        "the undisclosed skill's bytes never materialized",
    );
}

#[test]
fn a_yes_reattach_of_a_paused_and_excluded_skill_converges_to_installed() {
    // P3: `remove deploy` then `unfollow deploy` leaves the entry PAUSED as well as excluded here. The
    // re-attach must re-affirm the local follow (not just clear the marker) — else the reconcile's
    // `!following` guard skips the skill and "lands on next update" is a lie.
    let rig = Rig::new("reattach-paused");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let directory = FakeDirectory::acme(log.clone());
    let transport = deploy_transport(log.clone());
    seed_excluded_deploy(&rig, &directory, &transport);
    // The `unfollow` half: pause the entry (it stays excluded here too).
    enroll::set_following(&rig.fs, &rig.layout(), "s_deploy", false).unwrap();
    assert!(
        !follow_entry(&rig, "s_deploy").following,
        "seeded as paused",
    );
    log.lock().unwrap().clear();

    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["deploy".to_owned()],
        opts(true),
    )
    .unwrap();
    let ops::FollowOutcome::ReattachApplied(reattach) = out else {
        panic!("--yes re-attaches a paused+excluded skill too");
    };
    assert!(
        reattach.installed,
        "a paused+excluded re-attach still lands the bytes",
    );
    let e = follow_entry(&rig, "s_deploy");
    assert!(
        e.following && !e.excluded_here,
        "converged: following again AND the marker cleared (following={}, excluded_here={})",
        e.following,
        e.excluded_here,
    );
    assert!(
        rig.work
            .0
            .join("skills")
            .join("deploy")
            .join("SKILL.md")
            .exists(),
        "the current bytes are back on disk",
    );
}

#[test]
fn a_multi_target_subscribe_clears_a_swept_in_excluded_skills_marker() {
    // P3: the single-excluded-target case re-attaches, but `follow <x> <y> --yes` (multi) takes the
    // classic subscribe apply. Its `follow_skill` PUT lifts x's SERVER exclusion — the local
    // `excluded_here` marker must be cleared to match, or `list` lies. Both targets converge.
    let rig = Rig::new("multi-excluded");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let directory = FakeDirectory::acme(log.clone());
    let mut transport = deploy_transport(log.clone());
    // A second delivered skill, so the subscribe is genuinely multi-target.
    let vdocs = mk_version(&[("SKILL.md", FileMode::Regular, b"# docs\n")]);
    transport.snapshot.skills.push(DeliverySkill {
        skill_id: "s_docs".into(),
        name: "docs".into(),
        review_required: false,
        version_id: vdocs.id,
        generation: 1,
        bundle_digest: vdocs.digest,
        via_channels: vec!["everyone".into()],
        via_direct: false,
    });
    transport.versions.insert("s_docs".into(), vdocs.fetched);
    seed_excluded_deploy(&rig, &directory, &transport);
    log.lock().unwrap().clear();

    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    // Two targets → the classic subscribe apply (NOT the single-target re-attach arm).
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec![
            "acme/skills/deploy".to_owned(),
            "acme/skills/docs".to_owned(),
        ],
        opts(true),
    )
    .unwrap();
    assert!(
        matches!(out, ops::FollowOutcome::Applied(_)),
        "a multi-target subscribe applies via the classic path, never the re-attach arm",
    );
    // deploy's stale local marker is cleared, and its bytes are reinstalled.
    assert!(
        !follow_entry(&rig, "s_deploy").excluded_here,
        "the swept-in excluded skill's stale marker is cleared",
    );
    assert!(
        rig.work
            .0
            .join("skills")
            .join("deploy")
            .join("SKILL.md")
            .exists(),
        "deploy converged (bytes reinstalled)",
    );
}

#[test]
fn a_batch_sweeping_an_excluded_skill_keeps_the_describe() {
    // `remove deploy` (this device's exclusion) + `unfollow docs` (the person's pause): the bare
    // TWO-target `follow deploy docs` must DESCRIBE — the excluded-here stance is never widened
    // (its inverse is `remove`, and the batch's single `unfollow` undo could not restore the
    // device opt-out), and the stance check runs BEFORE the snapshot's `detached` evidence, so a
    // skill both excluded here and detached stays gated too. Docs alone would re-attach
    // immediately; the excluded member gates the whole batch.
    let rig = Rig::new("no-widen-excluded");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let directory = FakeDirectory::acme(log.clone());
    let transport = deploy_transport(log.clone());
    seed_excluded_deploy(&rig, &directory, &transport);
    // docs: followed locally, then unfollowed — the paused stance beside the exclusion. The
    // person's unfollow ALSO lists deploy detached server-side in the codex scenario; the local
    // excluded-here marker must win regardless of the snapshot.
    enroll::write_follows_merged(
        &rig.fs,
        &rig.layout(),
        &[enroll::FollowEntry {
            skill_id: "s_docs".to_owned(),
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
    let out = run_unfollow(&rig, &directory, &transport, &["docs".to_owned()], false).unwrap();
    assert!(matches!(out, ops::UnfollowOutcome::Applied(_)));
    // Deploy carries BOTH stances now: excluded here AND unfollowed (paused) — the remove-then-
    // unfollow shape whose batch re-follow must stay gated.
    let out = run_unfollow(&rig, &directory, &transport, &["deploy".to_owned()], false).unwrap();
    assert!(matches!(out, ops::UnfollowOutcome::Applied(_)));
    log.lock().unwrap().clear();

    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let out = run_follow(
        &rig,
        &enroll_fake,
        &directory,
        &transport,
        vec!["deploy".to_owned(), "docs".to_owned()],
        opts(false),
    )
    .unwrap();
    assert!(
        matches!(out, ops::FollowOutcome::Described { .. }),
        "an excluded-here member gates the whole batch: {out:?}"
    );
    // Nothing mutated: no follow_skill row op fired, both stances stand.
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .all(|e| e != "follow s_deploy" && e != "follow s_docs"),
        "no row op on a describe: {:?}",
        log.lock().unwrap()
    );
    let e = follow_entry(&rig, "s_deploy");
    assert!(e.excluded_here, "the device exclusion stands");
    assert!(!follow_entry(&rig, "s_docs").following, "the pause stands");
}
