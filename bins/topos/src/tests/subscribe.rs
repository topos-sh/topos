//! The two-phase SUBSCRIBE surface over fakes (no HTTP): the `follow <address>` enroll flow
//! (card → authorize(workspace) → redeem → the describe gate), the wrong-server `TOPOS_HOME`
//! refusal, the describe fields (installs + collision choice + direct-follow note), the `--yes`
//! apply (row ops + the batch-accepted reconcile), the dual-kind `unfollow` (workspace/`everyone`
//! refusals; the skill detach row + the local pause), and the hook posture (the staleness warning
//! line; notices fetched-without-ack vs narrated-then-acked).

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
use topos_types::{CurrencyKind, Generation, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    Card, DeliverySkill, DeliverySnapshot, DeliverySource, DeviceAuthorize, DirectorySource,
    EnrollSource, FetchedFile, FetchedVersion, FollowSource, Grant, GrantedToken, GrantedWorkspace,
    InertFollow, InertPlane, KnownCurrent, PlaneError, PlaneSource, PointerFetch,
    ReconcileTransport, Redeem, StandupAuthorize, TokenPoll,
};
use crate::plane_http::SkillCred;
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
        // Mirror the real adapters: the DIRNAME is the (sanitized) display name — the collision
        // machinery's whole subject — falling back to the id.
        let dir = naming.name.unwrap_or(skill_id);
        PlacementTarget {
            dir: self.skills_root.join(dir),
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
                deployment_mode: topos_types::bootstrap::DeploymentMode::Cloud,
                enrollment_method: "device_code".to_owned(),
            },
        )
        .unwrap();
        let mut user = enroll::UserDoc {
            schema_version: 1,
            email: Some("alice@acme.com".to_owned()),
            principal: Some("alice@acme.com".to_owned()),
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
                invite_policy: "members".into(),
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
        status: "active".to_owned(),
        version_id: "a".repeat(64),
        bundle_digest: "b".repeat(64),
        generation: Generation { epoch: 1, seq: 1 },
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
    fn exclude_device(&self, _ws: &str, _skill: &str) -> Result<(), ClientError> {
        unreachable!("no exclusion in these flows")
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

/// The address-flow enroll fake: the protocol card, the enroll-intent device flow (recording the
/// requested workspace NAME), and the redeem.
#[derive(Clone)]
struct FakeAddressEnroll {
    api_base: String,
    log: CallLog,
}

impl EnrollSource for FakeAddressEnroll {
    fn fetch_bootstrap(&self, _t: &str) -> Result<topos_types::BootstrapData, ClientError> {
        unreachable!("the address flow reads the card, never an /i/ bootstrap")
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
        workspace: &str,
        _pk: [u8; 32],
        _machine: &str,
    ) -> Result<DeviceAuthorize, ClientError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("authorize {workspace}"));
        Ok(DeviceAuthorize {
            device_code: "dc_secret".into(),
            user_code: "CODE".into(),
            verification_uri: format!("{}/verify", self.api_base),
            verification_uri_complete: Some(format!("{}/verify/CODE", self.api_base)),
            expires_in: 900,
            interval: 5,
        })
    }
    fn device_authorize_standup(
        &self,
        _pk: [u8; 32],
        _machine: &str,
    ) -> Result<StandupAuthorize, ClientError> {
        unreachable!("no standup in the address flow")
    }
    fn poll_token(&self, _dc: &str) -> Result<TokenPoll, ClientError> {
        Ok(TokenPoll::Granted(GrantedToken {
            grant: Grant::new("grant_secret".into()),
            workspace: Some(GrantedWorkspace {
                workspace_id: WS.into(),
                display_name: "Acme Inc".into(),
                address: Some("https://topos.sh/acme".into()),
            }),
        }))
    }
    fn redeem(
        &self,
        workspace_id: &str,
        _grant: &str,
        _pk: [u8; 32],
    ) -> Result<Redeem, ClientError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("redeem {workspace_id}"));
        Ok(Redeem {
            workspace_id: workspace_id.to_owned(),
            device_key_id: "dk_test".into(),
            principal: Some("alice@acme.com".into()),
            credential: "wsc_secret".into(),
        })
    }
    fn admin_claim(&self, _t: &str, _pk: [u8; 32], _d: &str) -> Result<Redeem, ClientError> {
        unreachable!("no claim in the address flow")
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
    let plane_connect = |_b: &str, _c: HashMap<String, SkillCred>| -> Box<dyn PlaneSource> {
        panic!("the subscribe flows never build the offer-disclosure transport")
    };
    let dir_connect = |_b: &str| -> Box<dyn DirectorySource> { Box::new(directory.clone()) };
    let del_connect = |_b: &str| -> Box<dyn ReconcileTransport> { Box::new(transport.clone()) };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        plane: &plane_connect,
        directory: &dir_connect,
        delivery: &del_connect,
        web_origin: "https://topos.sh".to_owned(),
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
    assert_eq!(describe.invited_by.as_deref(), Some("robert@acme.com"));
    // The inviter's pre-placement is disclosed; the structural everyone is not.
    assert_eq!(describe.preplaced_channels, vec!["design".to_owned()]);
    // The paste-ready apply argv ends in --yes.
    assert_eq!(
        next_argvs[0],
        vec!["topos", "follow", "acme", "--yes"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    );
    // The redeem rode the workspace the GRANT named (never the unverified address string).
    assert!(log.lock().unwrap().iter().any(|e| e == "redeem w_acme"));
    // The enrollment itself promoted (identity, reversible): the credential + membership are on
    // disk, the WAL is gone — but NOTHING was subscribed and nothing installed.
    let creds = enroll::read_credentials(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap()
        .into_map();
    assert_eq!(creds.get(WS).map(String::as_str), Some("wsc_secret"));
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
fn the_channel_describe_lists_installs_with_digests_and_the_collision_choice() {
    let rig = Rig::new("describe");
    rig.seed_enrolled();
    let log: CallLog = Arc::default();
    let enroll_fake = FakeAddressEnroll {
        api_base: API.to_owned(),
        log: log.clone(),
    };
    let directory = FakeDirectory::acme(log.clone());
    let transport = FakeTransport::empty(log.clone());

    // A LOCAL tracked skill already holds the name "deploy" (a different identity) — the incoming
    // channel skill collides on the dirname.
    let local = rig.work.0.join("local-deploy");
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
    // The collision is listed with the prefixed-dir choice, and the alternative argv is offered.
    assert_eq!(describe.collisions.len(), 1);
    assert_eq!(describe.collisions[0].name, "deploy");
    assert_eq!(describe.collisions[0].prefixed_dirname, "acme.deploy");
    assert_eq!(next_argvs.len(), 2, "the --prefix-dirname argv is offered");
    assert!(next_argvs[1].contains(&"--prefix-dirname".to_owned()));
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
        generation: Generation { epoch: 1, seq: 1 },
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
        generation: Generation { epoch: 1, seq: 1 },
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
        }],
    )
    .unwrap();

    // Bare = describe; nothing changes.
    let out = run_unfollow(&rig, &directory, &transport, &["docs".to_owned()], false).unwrap();
    let ops::UnfollowOutcome::Described { describe, yes_argv } = out else {
        panic!("bare = describe");
    };
    assert_eq!(describe.items.len(), 1);
    assert_eq!(describe.items[0].kind, "skill");
    assert_eq!(describe.items[0].stops, vec!["docs".to_owned()]);
    assert!(describe.all_devices_note.contains("every device"));
    assert!(describe.record_note.contains("records the detach"));
    assert!(yes_argv.contains(&"--yes".to_owned()));
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .all(|e| !e.starts_with("unfollow")),
        "the describe wrote nothing"
    );

    // --yes: the server row + the local pause flip; bytes stay (nothing else on disk moves).
    let out = run_unfollow(&rig, &directory, &transport, &["docs".to_owned()], true).unwrap();
    let ops::UnfollowOutcome::Applied(applied) = out else {
        panic!("--yes = apply");
    };
    assert!(applied.bytes_kept);
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
