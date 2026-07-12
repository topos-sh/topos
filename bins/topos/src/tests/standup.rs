//! The workspace-standup CLIENT flows over fakes (no HTTP): the un-enrolled `publish` standup branch
//! (call 1 pending → the same command re-invoked → granted → redeem → promote → publish continuation, all
//! against a stub transport), the consent re-derivation between the two calls, the `--propose` refusal,
//! the enrolled-device gate, the one-shot `follow <claim-link>` (happy + uncertain-send retry), the
//! fail-closed unknown-enrollment-method branch, and the REQUEST_ACCESS mapping of a denied redeem.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use base64::Engine as _;

use topos_core::digest::{self, FileMode, ManifestEntry, to_hex};
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::bootstrap::{
    BootstrapData, BootstrapInvite, BootstrapPlane, BootstrapWorkspace, ConsentMode,
    DeploymentMode, VerifiedDomainStatus,
};
use topos_types::requests::{ProposeRequest, PublishRequest, RevertRequest, ReviewRequest};
use topos_types::results::{PublishPendingStatus, PullAction};
use topos_types::{
    ActionCode, CurrencyKind, CurrentRecord, Generation, HarnessId, PointerScope, Receipt,
    TerminalOutcome, TriggerReport, TriggerState, WireCurrentRecord,
};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    ContributeSource, DeviceAuthorize, EnrollSource, Grant, GrantedToken, GrantedWorkspace,
    InertFollow, InertPlane, Redeem, StandupAuthorize, TokenPoll, WriteReceipt,
};
use crate::plane_http::SkillCred;
use crate::sidecar::Layout;
use crate::{enroll, identity, ops};

const HOSTED: &str = "https://api.topos.test";
const CLAIM_BASE: &str = "https://plane.acme.test";
const STANDUP_WS: &str = "w_standup";
const USER_CODE: &str = "abcdefgh23456789";

// ---------------------------------------------------------------------------------------------
// Scratch + rig (mirrors the follow suite's shape).
// ---------------------------------------------------------------------------------------------

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-sup-{tag}-{}-{n}", std::process::id()));
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
    work: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: NullHarness,
}
impl Rig {
    fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(&format!("{tag}-home")),
            work: Scratch::new(&format!("{tag}-work")),
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1_700_000_000_000),
            harness: NullHarness,
        }
    }
    fn layout(&self) -> Layout {
        Layout::new(&self.home.0)
    }
    fn ctx(&self) -> Ctx<'_> {
        // The UN-ENROLLED composition: the inert plane pair + the all-zero placeholder key — exactly what
        // app.rs wires when instance.json is absent (the state the standup branch starts from).
        static INERT_PLANE: InertPlane = InertPlane;
        static INERT_FOLLOW: InertFollow = InertFollow;
        Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: "d_test".into(),
            layout: self.layout(),
            harness: &self.harness,
            plane: &INERT_PLANE,
            follow: &INERT_FOLLOW,
        }
    }
    /// Adopt a plain skill dir (tracked in place) and return `(name, digest_hex)` — the consent token's
    /// two halves.
    fn adopt(&self, name: &str, body: &str) -> (String, String) {
        identity::load_or_create_device_id(&self.fs, &self.layout()).unwrap();
        let src = self.work.0.join(name);
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("SKILL.md"), body).unwrap();
        let added = ops::add(&self.ctx(), &src).expect("adopt the draft skill");
        (added.name, added.bundle_digest)
    }
    fn edit(&self, name: &str, body: &str) {
        std::fs::write(self.work.0.join(name).join("SKILL.md"), body).unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// The standup EnrollSource fake: authorize-standup / poll / redeem (verifying the EMPTY-offered-set
// possession frame exactly as the authority would). fetch_bootstrap / device_authorize / admin_claim
// panic — the standup branch must never touch the invite or claim doors.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Poll {
    Pending,
    Denied,
    Expired,
    Granted,
}

#[derive(Clone)]
struct FakeStandup {
    poll: Poll,
    authorize_calls: std::rc::Rc<Cell<u32>>,
    poll_calls: std::rc::Rc<Cell<u32>>,
}
impl FakeStandup {
    fn new(poll: Poll) -> Self {
        Self {
            poll,
            authorize_calls: std::rc::Rc::new(Cell::new(0)),
            poll_calls: std::rc::Rc::new(Cell::new(0)),
        }
    }
}
impl EnrollSource for FakeStandup {
    fn fetch_bootstrap(&self, _t: &str) -> Result<BootstrapData, ClientError> {
        panic!("a standup publish never reads an /i/ bootstrap")
    }
    fn device_authorize(
        &self,
        _t: &str,
        _k: [u8; 32],
        _m: &str,
    ) -> Result<DeviceAuthorize, ClientError> {
        panic!("a standup publish never starts an invite-anchored authorization")
    }
    fn device_authorize_standup(
        &self,
        _device_public_key: [u8; 32],
        _machine_name: &str,
    ) -> Result<StandupAuthorize, ClientError> {
        self.authorize_calls.set(self.authorize_calls.get() + 1);
        Ok(StandupAuthorize {
            auth: DeviceAuthorize {
                device_code: "dc_standup_secret".to_owned(),
                user_code: USER_CODE.to_owned(),
                verification_uri: format!("{HOSTED}/verify"),
                // The SERVER-built complete URI — the client must persist + surface it VERBATIM.
                verification_uri_complete: Some(format!("https://topos.sh/verify/{USER_CODE}")),
                expires_in: 900,
                interval: 5,
            },
            plane: BootstrapPlane {
                base_url: HOSTED.to_owned(),
                deployment_mode: DeploymentMode::Cloud,
                enrollment_method: "device_code".to_owned(),
            },
        })
    }
    fn poll_token(&self, device_code: &str) -> Result<TokenPoll, ClientError> {
        self.poll_calls.set(self.poll_calls.get() + 1);
        assert_eq!(device_code, "dc_standup_secret", "polls the WAL's session");
        Ok(match self.poll {
            Poll::Pending => TokenPoll::Pending,
            Poll::Denied => TokenPoll::Denied,
            Poll::Expired => TokenPoll::Expired,
            Poll::Granted => TokenPoll::Granted(GrantedToken {
                grant: Grant::new("grant_standup_xyz".to_owned()),
                workspace: Some(GrantedWorkspace {
                    workspace_id: STANDUP_WS.to_owned(),
                    display_name: "robert's workspace".to_owned(),
                    // The workspace ADDRESS the standup receipt's share line rides.
                    address: Some(format!("{HOSTED}/roberts-workspace")),
                }),
            }),
        })
    }
    fn redeem(
        &self,
        workspace_id: &str,
        _grant: &str,
        device_public_key: [u8; 32],
    ) -> Result<Redeem, ClientError> {
        // The client no longer signs the redeem — the body's public key is the identity anchor and the
        // plane derives the device key id the same way.
        let dk = topos_core::identity::device_key_id(&device_public_key);
        assert_eq!(workspace_id, STANDUP_WS);
        Ok(Redeem {
            workspace_id: workspace_id.to_owned(),
            device_key_id: dk,
            principal: Some("robert@example.com".to_owned()),
            credential: format!("wsc_{workspace_id}"),
        })
    }
    fn admin_claim(&self, _c: &str, _k: [u8; 32], _d: &str) -> Result<Redeem, ClientError> {
        panic!("a standup publish never redeems an admin claim")
    }
}

/// A standup connector that panics on ANY use — proving a code path never reaches for the network.
fn panicking_standup_connect(_b: &str) -> Box<dyn EnrollSource> {
    panic!("this code path must not build a standup transport")
}

// ---------------------------------------------------------------------------------------------
// The contribute-side fakes: a plane that rehashes the candidate like the real server and answers OK
// with the moved (UNSIGNED) `current` pointer, and a governance stub whose invite fold-in fails (the
// publish must still succeed — the fold is best-effort).
// ---------------------------------------------------------------------------------------------

struct SigningPlane {
    publishes: RefCell<Vec<String>>,
}
impl SigningPlane {
    fn new() -> Self {
        Self {
            publishes: RefCell::new(Vec::new()),
        }
    }
}
impl ContributeSource for SigningPlane {
    fn publish(&self, body: PublishRequest) -> Result<WriteReceipt, ClientError> {
        // Rehash the candidate the way the server does: decode every file, digest, commit id.
        let entries: Vec<ManifestEntry> = body
            .candidate
            .files
            .iter()
            .map(|f| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&f.content_base64)
                    .expect("candidate bytes are standard base64");
                ManifestEntry {
                    path: f.path.clone(),
                    mode: FileMode::Regular,
                    content_sha256: digest::sha256(&bytes),
                }
            })
            .collect();
        let tree = digest::bundle_digest(&entries).expect("candidate digest");
        let parents: Vec<[u8; 32]> = body
            .candidate
            .parents
            .iter()
            .map(|p| {
                let mut out = [0u8; 32];
                hex::decode_to_slice(p, &mut out).unwrap();
                out
            })
            .collect();
        let commit_id = topos_core::identity::commit_id(&topos_core::identity::Commit {
            parents: &parents,
            tree,
            author: &body.candidate.author,
            message: &body.candidate.message,
        })
        .unwrap();
        self.publishes.borrow_mut().push(body.op_id.clone());
        // The UNSIGNED pointer at (1,1) — the genesis move; authority is the row, integrity the version id.
        Ok(WriteReceipt {
            receipt: Receipt {
                schema_version: 1,
                op_id: body.op_id,
                command: "publish-direct".to_owned(),
                outcome: TerminalOutcome::Ok,
                workspace_id: body.workspace_id.clone(),
                skill_id: Some(body.skill_id.clone()),
                version_id: Some(to_hex(&commit_id)),
                bundle_digest: Some(to_hex(&tree)),
                expected_generation: Some(body.expected),
                current_generation: Some(Generation { epoch: 1, seq: 1 }),
                created_at: "2026-07-03T00:00:00Z".to_owned(),
                details: None,
            },
            error: None,
            wire_record: Some(WireCurrentRecord {
                schema_version: 1,
                scope: PointerScope {
                    workspace_id: body.workspace_id,
                    skill_id: body.skill_id,
                },
                record: CurrentRecord {
                    version_id: to_hex(&commit_id),
                    generation: Generation { epoch: 1, seq: 1 },
                },
            }),
        })
    }
    fn propose(&self, _b: ProposeRequest) -> Result<WriteReceipt, ClientError> {
        panic!("the standup continuation is a direct publish, never a proposal")
    }
    fn revert(&self, _b: RevertRequest) -> Result<WriteReceipt, ClientError> {
        panic!("not a revert")
    }
    fn review(&self, _b: ReviewRequest) -> Result<WriteReceipt, ClientError> {
        panic!("not a review")
    }
}

/// Drive `ops::publish` with the standup connectors over the fakes (never the compiled-in base). `target`
/// is the single positional — `<skill>` or `<skill>@<digest>`.
fn run_publish(
    rig: &Rig,
    fake: &FakeStandup,
    target: &str,
    propose: bool,
) -> Result<ops::PublishOutcome, ClientError> {
    let ctx = rig.ctx();
    let contribute = |_b: &str| -> Box<dyn ContributeSource> { Box::new(SigningPlane::new()) };
    let standup_enroll = |_b: &str| -> Box<dyn EnrollSource> { Box::new(fake.clone()) };
    let standup = ops::StandupConnectors {
        enroll: &standup_enroll,
        base_url: HOSTED.to_owned(),
    };
    ops::publish(
        &ctx,
        &contribute,
        &standup,
        None, // roots — the standup tests act on an already-tracked skill (no auto-add)
        target,
        propose,
        None,
        None,
        None,
    )
}

// ---------------------------------------------------------------------------------------------
// The publish standup branch.
// ---------------------------------------------------------------------------------------------

#[test]
fn unenrolled_publish_call1_emits_pending_and_writes_the_standup_wal() {
    let rig = Rig::new("call1");
    let (name, digest_hex) = rig.adopt("deploy", "# deploy v1\n");
    let approve = format!("{name}@{digest_hex}");
    let fake = FakeStandup::new(Poll::Pending);

    let outcome = run_publish(&rig, &fake, &approve, false).expect("call 1 is ok-pending");
    let ops::PublishOutcome::Pending { data, resume_argv } = outcome else {
        panic!("an un-enrolled direct publish returns the PENDING standup outcome");
    };

    // The pending block: the SERVER-built complete URI verbatim, the code, an RFC-3339 expiry.
    let pending = data.pending.expect("a pending block");
    assert!(matches!(
        pending.status,
        PublishPendingStatus::SigninRequired
    ));
    assert_eq!(
        pending.verification_uri_complete,
        format!("https://topos.sh/verify/{USER_CODE}"),
        "the server-provided complete URI is used verbatim (never reconstructed)"
    );
    assert_eq!(pending.user_code, USER_CODE);
    assert_eq!(pending.expires_at.as_deref(), Some("2023-11-14T22:28:20Z"));
    // Honest at pending: no version, no generation; the consent digest is the approved one.
    assert!(data.version_id.is_none());
    assert!(data.current_generation.is_none());
    assert_eq!(data.bundle_digest, digest_hex);
    // The resume argv IS this same command, canonically spelled — the `<skill>@<digest>` positional pin.
    assert_eq!(
        resume_argv,
        vec![
            "topos".to_owned(),
            "publish".to_owned(),
            approve.clone(),
            "--json".to_owned(),
        ]
    );

    // The WAL: 0600, AuthorizingStandup, carrying the verbatim complete URI.
    let layout = rig.layout();
    let mode = std::fs::metadata(layout.enrollment_path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the standup WAL is a 0600 secret");
    let wal = enroll::read_wal(&rig.fs, &layout).unwrap().expect("a WAL");
    let enroll::EnrollPhase::AuthorizingStandup {
        base_url,
        verification_uri_complete,
        ..
    } = &wal.state
    else {
        panic!("the WAL is the standup phase");
    };
    assert_eq!(base_url, HOSTED);
    assert_eq!(
        *verification_uri_complete,
        format!("https://topos.sh/verify/{USER_CODE}")
    );
    // NOT enrolled yet — nothing promoted, nothing published.
    assert!(enroll::read_instance(&rig.fs, &layout).unwrap().is_none());
    assert_eq!(fake.authorize_calls.get(), 1);
}

#[test]
fn unenrolled_publish_of_an_untracked_dir_auto_adds_then_stands_up() {
    // The auto-add composition: an un-enrolled `publish <dir>` of a skill topos has never seen adopts it in
    // place FIRST (offline, before any network), then stands the workspace up — one command, disclosed.
    let rig = Rig::new("autoadd");
    let src = rig.work.0.join("newskill");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), "# newskill v1\n").unwrap();
    let fake = FakeStandup::new(Poll::Pending);

    let outcome =
        run_publish(&rig, &fake, src.to_str().unwrap(), false).expect("call 1 is ok-pending");
    let ops::PublishOutcome::Pending { data, resume_argv } = outcome else {
        panic!(
            "an un-enrolled direct publish of an untracked dir returns the PENDING standup outcome"
        );
    };

    // The add was folded in and DISCLOSED (a plain dir under no harness → no slug).
    let added = data
        .added
        .expect("the auto-add is disclosed on the pending receipt");
    assert_eq!(added.name, "newskill");
    assert_eq!(added.harness_slug, None);
    // The standup actually started (the network was reached only AFTER the local adopt).
    assert!(data.pending.is_some());
    assert_eq!(fake.authorize_calls.get(), 1);
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_some());

    // The skill is now tracked under its adopted NAME — the resume argv carries `<name>@<digest>` (never the
    // original dir path), so call 2 tracked-resolves fast and never re-adopts.
    assert_eq!(resume_argv.first().map(String::as_str), Some("topos"));
    assert!(
        resume_argv.iter().any(|a| a.starts_with("newskill@")),
        "the resume argv self-heals to the adopted name: {resume_argv:?}"
    );
    // A second adopt of the same dir is now refused — proof it is genuinely tracked.
    assert!(matches!(
        ops::add(&rig.ctx(), &src).unwrap_err(),
        ClientError::AlreadyTracked { .. }
    ));
}

#[test]
fn unenrolled_bare_publish_bakes_the_disclosed_digest_into_the_resume() {
    // A bare `publish <skill>` (no `@<digest>` pin) still BINDS across the sign-in gap: the pending
    // receipt discloses `bundle_digest`, and the resume argv it emits carries that computed digest as the
    // pin — so the resume's pre-flight refuses drift and nothing lands that the pending receipt did not
    // disclose. (topos self-supplies the pin it computed; the caller never typed it.)
    let rig = Rig::new("call1_bare");
    let (name, digest_hex) = rig.adopt("deploy", "# deploy v1\n");
    let fake = FakeStandup::new(Poll::Pending);

    let outcome = run_publish(&rig, &fake, &name, false).expect("call 1 is ok-pending");
    let ops::PublishOutcome::Pending { data, resume_argv } = outcome else {
        panic!("an un-enrolled direct publish returns the PENDING standup outcome");
    };
    // The disclosed digest and the resume's pin are the SAME value — the binding is end-to-end.
    assert_eq!(data.bundle_digest, digest_hex);
    assert_eq!(
        resume_argv,
        vec![
            "topos".to_owned(),
            "publish".to_owned(),
            format!("{name}@{digest_hex}"),
            "--json".to_owned(),
        ],
        "a bare publish bakes the computed digest into the resume so the disclosure stays binding"
    );
}

#[test]
fn reinvoke_while_pending_reemits_and_keeps_the_wal() {
    let rig = Rig::new("pending2");
    let (name, digest_hex) = rig.adopt("deploy", "# deploy v1\n");
    let approve = format!("{name}@{digest_hex}");

    let fake = FakeStandup::new(Poll::Pending);
    let _ = run_publish(&rig, &fake, &approve, false).unwrap();
    // The SAME command re-invoked: one poll, the pending envelope re-emitted, the WAL kept.
    let outcome = run_publish(&rig, &fake, &approve, false).unwrap();
    let ops::PublishOutcome::Pending { data, .. } = outcome else {
        panic!("still pending");
    };
    assert_eq!(data.pending.unwrap().user_code, USER_CODE);
    assert_eq!(fake.authorize_calls.get(), 1, "no second authorization");
    assert_eq!(fake.poll_calls.get(), 1, "exactly ONE poll per invocation");
    assert!(
        enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_some(),
        "the WAL survives a pending poll"
    );
}

#[test]
fn reinvoke_granted_redeems_promotes_and_publishes_in_one_invocation() {
    let rig = Rig::new("granted");
    let (name, digest_hex) = rig.adopt("deploy", "# deploy v1\n");
    let approve = format!("{name}@{digest_hex}");

    // Call 1: pending.
    let _ = run_publish(&rig, &FakeStandup::new(Poll::Pending), &approve, false).unwrap();
    // Call 2 (same argv): granted → redeem → promote → the publish continues in THIS invocation.
    let outcome = run_publish(&rig, &FakeStandup::new(Poll::Granted), &approve, false).unwrap();
    let ops::PublishOutcome::Published(data) = outcome else {
        panic!("a granted standup continues into the publish");
    };
    assert_eq!(data.bundle_digest, digest_hex);
    assert!(data.version_id.is_some(), "a real version was published");
    assert_eq!(
        data.current_generation,
        Some(Generation { epoch: 1, seq: 1 }),
        "the genesis publish moved current to (1,1)"
    );
    // The hijack-visibility disclosure: workspace + owner, verbatim from the plane.
    let standup = data.standup.expect("the standup receipt");
    assert_eq!(standup.workspace_display_name, "robert's workspace");
    assert_eq!(
        standup.owner_principal.as_deref(),
        Some("robert@example.com")
    );
    // The genesis standup disclosed the workspace address, so the publish folds in the paste-able share
    // line for the shipped skill (the client-side invite mint is retired — the address IS the share surface).
    assert!(
        data.share_line
            .as_deref()
            .is_some_and(|l| l.starts_with(HOSTED) && l.contains("/skills/")),
        "the share line rides the standup address: {:?}",
        data.share_line
    );

    // The enrollment was promoted: instance.json at the standup plane, user.json carries the
    // principal + the NON-invite root, the WAL is gone.
    let layout = rig.layout();
    let instance = enroll::read_instance(&rig.fs, &layout).unwrap().unwrap();
    assert_eq!(instance.base_url, HOSTED);
    let user = enroll::read_user(&rig.fs, &layout).unwrap().unwrap();
    let m = user
        .membership(STANDUP_WS)
        .expect("the standup workspace membership");
    assert_eq!(m.display_name.as_deref(), Some("robert's workspace"));
    assert_eq!(user.principal.as_deref(), Some("robert@example.com"));
    assert_eq!(user.email.as_deref(), Some("robert@example.com"));
    assert!(!m.invite_rooted, "a standup is not invite-rooted");
    assert!(enroll::read_wal(&rig.fs, &layout).unwrap().is_none());
}

#[test]
fn consent_rederives_on_resume_so_drifted_bytes_are_refused() {
    let rig = Rig::new("drift");
    let (name, digest_hex) = rig.adopt("deploy", "# deploy v1\n");
    let approve = format!("{name}@{digest_hex}");
    let _ = run_publish(&rig, &FakeStandup::new(Poll::Pending), &approve, false).unwrap();

    // The bytes drift between call 1 and call 2 — the SAME approve token no longer matches the scan, so
    // the resume is refused BEFORE any poll (the existing digest-mismatch refusal), and the WAL stays.
    rig.edit("deploy", "# deploy v2 — drifted\n");
    let fake = FakeStandup::new(Poll::Granted);
    let err = run_publish(&rig, &fake, &approve, false).unwrap_err();
    assert!(
        matches!(err, ClientError::ApprovalMismatch { .. }),
        "got {err:?}"
    );
    assert_eq!(fake.poll_calls.get(), 0, "consent binds BEFORE any network");
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_some());
}

#[test]
fn a_denied_or_expired_signin_clears_the_wal_typed() {
    for (poll, needle) in [(Poll::Denied, "denied"), (Poll::Expired, "expired")] {
        let rig = Rig::new("deny");
        let (name, digest_hex) = rig.adopt("deploy", "# deploy v1\n");
        let approve = format!("{name}@{digest_hex}");
        let _ = run_publish(&rig, &FakeStandup::new(Poll::Pending), &approve, false).unwrap();
        let err = run_publish(&rig, &FakeStandup::new(poll), &approve, false).unwrap_err();
        assert!(matches!(err, ClientError::Enrollment(_)), "got {err:?}");
        assert!(err.to_string().contains(needle), "{err}");
        assert!(
            enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none(),
            "a terminal sign-in outcome sweeps the WAL"
        );
    }
}

#[test]
fn unenrolled_propose_keeps_the_typed_error_and_never_touches_the_network() {
    let rig = Rig::new("propose");
    let (name, digest_hex) = rig.adopt("deploy", "# deploy v1\n");
    let approve = format!("{name}@{digest_hex}");
    let ctx = rig.ctx();
    let contribute = |_b: &str| -> Box<dyn ContributeSource> { Box::new(SigningPlane::new()) };
    let standup = ops::StandupConnectors {
        enroll: &panicking_standup_connect,
        base_url: HOSTED.to_owned(),
    };
    let err = ops::publish(
        &ctx,
        &contribute,
        &standup,
        None, // roots — already-tracked skill, no auto-add
        &approve,
        true,
        None,
        None,
        None,
    )
    .unwrap_err();
    assert!(matches!(err, ClientError::Enrollment(_)), "got {err:?}");
    assert!(err.to_string().contains("not enrolled"), "{err}");
    assert!(
        enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none(),
        "no standup session was started for a proposal"
    );
    let _ = name;
}

#[test]
fn an_enrolled_device_never_hits_the_standup_branch() {
    let rig = Rig::new("enrolled");
    let (name, digest_hex) = rig.adopt("deploy", "# deploy v1\n");
    let approve = format!("{name}@{digest_hex}");
    // Enrolled = instance.json present. user.json deliberately absent, so the ENROLLED path fails with
    // its own typed error — while the panicking standup connector proves the branch was never taken.
    enroll::write_instance(
        &rig.fs,
        &rig.layout(),
        &enroll::Instance {
            schema_version: 1,
            base_url: "https://acme.topos.test".to_owned(),
            deployment_mode: DeploymentMode::Cloud,
            enrollment_method: "device_code".to_owned(),
        },
    )
    .unwrap();
    let ctx = rig.ctx();
    let contribute = |_b: &str| -> Box<dyn ContributeSource> { Box::new(SigningPlane::new()) };
    let standup = ops::StandupConnectors {
        enroll: &panicking_standup_connect,
        base_url: HOSTED.to_owned(),
    };
    let err = ops::publish(
        &ctx,
        &contribute,
        &standup,
        None, // roots — already-tracked skill, no auto-add
        &approve,
        false,
        None,
        None,
        None,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("workspace"),
        "the ENROLLED path's error, not a standup: {err}"
    );
}

// ---------------------------------------------------------------------------------------------
// The one-shot `follow <claim-link>` door.
// ---------------------------------------------------------------------------------------------

fn claim_bootstrap() -> BootstrapData {
    BootstrapData {
        schema_version: 1,
        invite: BootstrapInvite {
            token_id: "claim_1".into(),
            expires_at: None,
            consent: ConsentMode::DirectHumanFirstReceive,
            first_receive_auto_land: false,
        },
        plane: BootstrapPlane {
            base_url: CLAIM_BASE.into(),
            deployment_mode: DeploymentMode::SelfHost,
            enrollment_method: "admin_claim".into(),
        },
        workspace: BootstrapWorkspace {
            workspace_id: "w_acme".into(),
            display_name: "Acme".into(),
            verified_domain: None,
            verified_domain_status: VerifiedDomainStatus::Unverified,
        },
        offered_skills: Vec::new(),
    }
}

#[derive(Clone)]
struct FakeClaim {
    bootstrap: BootstrapData,
    /// The `/i/` token this fake expects (asserted on the bootstrap read and the claim POST).
    token: &'static str,
    /// Fail the first N admin-claim POSTs with a transport fault (the uncertain send).
    fail_claims: std::rc::Rc<Cell<u32>>,
    /// Answer every admin-claim POST with the transport's TERMINAL 200+DENIED mapping (a dead claim).
    deny_claims: bool,
    claim_calls: std::rc::Rc<Cell<u32>>,
    bootstrap_calls: std::rc::Rc<Cell<u32>>,
}
impl FakeClaim {
    fn new(fail_first: u32) -> Self {
        Self::build("claimtok", fail_first, false)
    }
    /// A fake whose claim door always returns the terminal dead-claim denial.
    fn denying() -> Self {
        Self::build("claimtok", 0, true)
    }
    fn build(token: &'static str, fail_first: u32, deny_claims: bool) -> Self {
        Self {
            bootstrap: claim_bootstrap(),
            token,
            fail_claims: std::rc::Rc::new(Cell::new(fail_first)),
            deny_claims,
            claim_calls: std::rc::Rc::new(Cell::new(0)),
            bootstrap_calls: std::rc::Rc::new(Cell::new(0)),
        }
    }
}
impl EnrollSource for FakeClaim {
    fn fetch_bootstrap(&self, token: &str) -> Result<BootstrapData, ClientError> {
        self.bootstrap_calls.set(self.bootstrap_calls.get() + 1);
        assert_eq!(token, self.token);
        Ok(self.bootstrap.clone())
    }
    fn device_authorize(
        &self,
        _t: &str,
        _k: [u8; 32],
        _m: &str,
    ) -> Result<DeviceAuthorize, ClientError> {
        panic!("a claim follow never runs the device-auth flow")
    }
    fn device_authorize_standup(
        &self,
        _k: [u8; 32],
        _m: &str,
    ) -> Result<StandupAuthorize, ClientError> {
        panic!("a claim follow never starts a standup")
    }
    fn poll_token(&self, _d: &str) -> Result<TokenPoll, ClientError> {
        panic!("a claim follow never polls")
    }
    fn redeem(&self, _w: &str, _g: &str, _k: [u8; 32]) -> Result<Redeem, ClientError> {
        panic!("a claim follow never redeems a grant")
    }
    fn admin_claim(
        &self,
        claim_token: &str,
        device_public_key: [u8; 32],
        display_name: &str,
    ) -> Result<Redeem, ClientError> {
        self.claim_calls.set(self.claim_calls.get() + 1);
        assert_eq!(claim_token, self.token);
        assert_eq!(display_name, "Acme", "disclosure-only display name");
        if self.fail_claims.get() > 0 {
            self.fail_claims.set(self.fail_claims.get() - 1);
            return Err(ClientError::Plane("timed out mid-send".into()));
        }
        if self.deny_claims {
            // The transport's terminal mapping of a 200+DENIED claim envelope (`map_redeem_envelope`).
            return Err(ClientError::Enrollment(
                "the claim link was refused — it may be consumed, expired, or the workspace may \
                 already exist; ask the plane operator for a fresh claim link"
                    .into(),
            ));
        }
        let dk = topos_core::identity::device_key_id(&device_public_key);
        Ok(Redeem {
            workspace_id: "w_acme".to_owned(),
            device_key_id: dk.clone(),
            principal: Some(format!("dev.{dk}")),
            credential: "wsc_acme".to_owned(),
        })
    }
}

fn run_claim_follow(
    rig: &Rig,
    fake: &FakeClaim,
    target: Option<&str>,
) -> Result<topos_types::results::FollowData, ClientError> {
    identity::load_or_create_device_id(&rig.fs, &rig.layout()).unwrap();
    let ctx = rig.ctx();
    let enroll_connect = |_b: &str| -> Box<dyn EnrollSource> { Box::new(fake.clone()) };
    let plane_connect =
        |_b: &str, _c: HashMap<String, SkillCred>| -> Box<dyn crate::plane::PlaneSource> {
            panic!("a skill-less claim follow discloses no offers")
        };
    let dir_connect = |_b: &str| -> Box<dyn crate::plane::DirectorySource> {
        panic!("the claim door never builds a directory transport")
    };
    let del_connect = |_b: &str| -> Box<dyn crate::plane::ReconcileTransport> {
        panic!("the claim door never builds a delivery transport")
    };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        plane: &plane_connect,
        directory: &dir_connect,
        delivery: &del_connect,
        web_origin: "https://topos.sh".to_owned(),
    };
    let opts = ops::FollowOpts {
        manual: false,
        workspace: None,
        yes: false,
        prefix_dirname: false,
        channels: Vec::new(),
        skills: Vec::new(),
    };
    match ops::follow(
        &ctx,
        &connectors,
        target.map(str::to_owned).into_iter().collect(),
        opts,
    )? {
        ops::FollowOutcome::Data { data, .. } => Ok(data),
        _ => panic!("the claim door answers the wire payload"),
    }
}

#[test]
fn a_claim_link_enrolls_in_one_invocation() {
    let rig = Rig::new("claim");
    let fake = FakeClaim::new(0);
    let data = run_claim_follow(&rig, &fake, Some(&format!("{CLAIM_BASE}/i/claimtok")))
        .expect("the one-shot claim follow succeeds");

    assert!(data.enrolled, "ONE invocation — no re-invoke needed");
    assert!(data.pending.is_none());
    assert_eq!(data.workspace_id, "w_acme");
    assert_eq!(data.workspace_display_name.as_deref(), Some("Acme"));
    assert_eq!(fake.claim_calls.get(), 1);

    let layout = rig.layout();
    let instance = enroll::read_instance(&rig.fs, &layout).unwrap().unwrap();
    assert_eq!(instance.base_url, CLAIM_BASE);
    assert_eq!(instance.enrollment_method, "admin_claim");
    let user = enroll::read_user(&rig.fs, &layout).unwrap().unwrap();
    assert!(user.principal.as_deref().unwrap().starts_with("dev.dk_"));
    assert!(
        user.email.is_none(),
        "a device-rooted principal is not an email"
    );
    let m = user.workspaces.first().expect("one claim membership");
    assert!(!m.invite_rooted, "a claim is not invite-rooted");
    assert!(enroll::read_wal(&rig.fs, &layout).unwrap().is_none());
}

#[test]
fn an_uncertain_claim_send_retries_the_post_directly_without_refetching_the_link() {
    let rig = Rig::new("claim-retry");
    let fake = FakeClaim::new(1);
    let link = format!("{CLAIM_BASE}/i/claimtok");

    // The first send is UNCERTAIN — the error surfaces, but the pre-send WAL is on disk.
    let err = run_claim_follow(&rig, &fake, Some(&link)).unwrap_err();
    assert!(matches!(err, ClientError::Plane(_)), "got {err:?}");
    let wal = enroll::read_wal(&rig.fs, &rig.layout()).unwrap().unwrap();
    assert!(
        matches!(wal.state, enroll::EnrollPhase::ClaimPending { .. }),
        "the pre-send claim WAL survives an uncertain send"
    );
    assert_eq!(fake.bootstrap_calls.get(), 1);
    assert_eq!(fake.claim_calls.get(), 1);

    // Re-running the SAME link retries the POST directly — the (possibly consumed) /i/ link is NEVER
    // refetched; the server's same-device replay answers Redeemed.
    let data = run_claim_follow(&rig, &fake, Some(&link)).expect("the retry settles");
    assert!(data.enrolled);
    assert_eq!(
        fake.bootstrap_calls.get(),
        1,
        "the retry never refetched the /i/ bootstrap"
    );
    assert_eq!(fake.claim_calls.get(), 2, "the POST was re-sent");
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
}

#[test]
fn a_claim_link_re_pasted_from_another_host_still_retries_without_refetch() {
    // The claim's identity is its TOKEN, not the host the link string rode: the WAL records the
    // re-rooted API base, and a human may re-paste the same claim as a share-host link. The retry must
    // still go straight to the POST — never refetching the (possibly consumed) /i/ bootstrap.
    let rig = Rig::new("claim-rehost");
    let fake = FakeClaim::new(1);
    let _ = run_claim_follow(
        &rig,
        &fake,
        Some(&format!("https://share.example/i/{}", fake.token)),
    )
    .unwrap_err();
    // The pre-send WAL recorded the DECLARED API base (the bootstrap's), not the share host.
    let wal = enroll::read_wal(&rig.fs, &rig.layout()).unwrap().unwrap();
    let enroll::EnrollPhase::ClaimPending { base_url, .. } = &wal.state else {
        panic!("expected a ClaimPending WAL");
    };
    assert_eq!(base_url, CLAIM_BASE);

    // The retry rides the ORIGINAL canonical link this time — same token, different host string.
    let data = run_claim_follow(&rig, &fake, Some(&format!("{CLAIM_BASE}/i/{}", fake.token)))
        .expect("the re-pasted claim settles");
    assert!(data.enrolled);
    assert_eq!(fake.bootstrap_calls.get(), 1, "never refetched");
    assert_eq!(fake.claim_calls.get(), 2);
}

#[test]
fn follow_resume_also_settles_an_unsettled_claim() {
    let rig = Rig::new("claim-resume");
    let fake = FakeClaim::new(1);
    let link = format!("{CLAIM_BASE}/i/claimtok");
    let _ = run_claim_follow(&rig, &fake, Some(&link)).unwrap_err();
    // Re-invoking `follow` (no link) retries from the WAL too.
    let data = run_claim_follow(&rig, &fake, None).expect("a re-invoked follow settles the claim");
    assert!(data.enrolled);
    assert_eq!(fake.bootstrap_calls.get(), 1, "never refetched");
    assert_eq!(fake.claim_calls.get(), 2);
}

#[test]
fn a_terminal_claim_denial_clears_the_wal_and_unwedges_follow() {
    let rig = Rig::new("claim-deny");
    let fake = FakeClaim::denying();
    let err = run_claim_follow(&rig, &fake, Some(&format!("{CLAIM_BASE}/i/claimtok"))).unwrap_err();
    assert!(matches!(err, ClientError::Enrollment(_)), "got {err:?}");
    assert_eq!(fake.claim_calls.get(), 1);
    // A definitive plane rejection is not retryable — the ClaimPending WAL is GONE (contrast the
    // uncertain transport fault, which KEEPS it: `an_uncertain_claim_send_retries_…` above).
    assert!(
        enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none(),
        "a terminal claim denial clears the ClaimPending WAL"
    );
    // …so a fresh follow of a DIFFERENT claim link is not wedged behind a leftover ClaimPending WAL: the
    // terminal denial cleared it, so the re-invoke begins the new claim instead of resuming the dead one.
    let fresh = FakeClaim::build("claimtok2", 0, false);
    let data = run_claim_follow(&rig, &fresh, Some(&format!("{CLAIM_BASE}/i/claimtok2")))
        .expect("a fresh claim link enrolls after the dead one cleared");
    assert!(data.enrolled);
}

#[test]
fn a_malformed_link_base_is_refused_before_any_network_and_never_echoes_the_token() {
    // A syntactically-bad base must be refused BEFORE the secret token is spliced into a request URL:
    // downstream it would surface as a ureq BadUri transport error whose message carries the full URI
    // (token included) — and every transport-error detail is persisted to ~/.topos/log.jsonl.
    let rig = Rig::new("bad-base");
    identity::load_or_create_device_id(&rig.fs, &rig.layout()).unwrap();
    let ctx = rig.ctx();
    let enroll_connect = |_b: &str| -> Box<dyn EnrollSource> {
        panic!("a malformed base is refused before any transport is built")
    };
    let plane_connect =
        |_b: &str, _c: HashMap<String, SkillCred>| -> Box<dyn crate::plane::PlaneSource> {
            panic!("no offers on a refused link")
        };
    let dir_connect = |_b: &str| -> Box<dyn crate::plane::DirectorySource> {
        panic!("no directory transport on a refused link")
    };
    let del_connect = |_b: &str| -> Box<dyn crate::plane::ReconcileTransport> {
        panic!("no delivery transport on a refused link")
    };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        plane: &plane_connect,
        directory: &dir_connect,
        delivery: &del_connect,
        web_origin: "https://topos.sh".to_owned(),
    };
    for bad in [
        "http://[bad]/i/claim_secret_token",
        "ftp://plane.acme.test/i/claim_secret_token",
        "http:///i/claim_secret_token",
    ] {
        let err = ops::follow(
            &ctx,
            &connectors,
            vec![bad.to_owned()],
            ops::FollowOpts {
                manual: false,
                workspace: None,
                yes: false,
                prefix_dirname: false,
                channels: Vec::new(),
                skills: Vec::new(),
            },
        )
        .map(|_| ())
        .unwrap_err();
        assert!(matches!(err, ClientError::Enrollment(_)), "got {err:?}");
        for surface in [err.to_string(), err.detail()] {
            assert!(
                !surface.contains("claim_secret_token"),
                "the token must never ride an error surface: {surface}"
            );
        }
    }
    // A well-formed base still parses (the validation must not over-reject the legit shapes).
    let fake = FakeClaim::new(0);
    let data = run_claim_follow(&rig, &fake, Some(&format!("{CLAIM_BASE}/i/claimtok")))
        .expect("a well-formed https base still enrolls");
    assert!(data.enrolled);
}

#[test]
fn a_crash_between_instance_and_user_json_recovers_on_the_next_publish() {
    let rig = Rig::new("torn-promote");
    let (name, digest_hex) = rig.adopt("deploy", "# deploy v1\n");
    let approve = format!("{name}@{digest_hex}");

    // Reproduce the torn state a crash inside promote_core leaves: instance.json written (its FIRST
    // durable step), user.json absent (never reached), and the standup Redeemed WAL still on disk.
    let layout = rig.layout();
    enroll::write_instance(
        &rig.fs,
        &layout,
        &enroll::Instance {
            schema_version: 1,
            base_url: HOSTED.to_owned(),
            deployment_mode: DeploymentMode::Cloud,
            enrollment_method: "device_code".to_owned(),
        },
    )
    .unwrap();
    enroll::write_wal(
        &rig.fs,
        &layout,
        &enroll::PendingEnrollment {
            schema_version: 1,
            state: enroll::EnrollPhase::Redeemed {
                context: enroll::EnrollContext {
                    base_url: HOSTED.to_owned(),
                    deployment_mode: DeploymentMode::Cloud,
                    enrollment_method: "device_code".to_owned(),
                    workspace_id: STANDUP_WS.to_owned(),
                    workspace_display_name: "robert's workspace".to_owned(),
                    verified_domain: None,
                    verified_domain_status: VerifiedDomainStatus::Unverified,
                    offered_skills: Vec::new(),
                    mode: enroll::FollowModeDoc::Auto,
                    root: enroll::EnrollRoot::Standup,
                    follow_target: None,
                },
                credential: "wsc_torn".to_owned(),
                device_key_id: "dk_torn".to_owned(),
                principal: Some("robert@example.com".to_owned()),
                enrolled_at_millis: 1,
            },
        },
    )
    .unwrap();

    // Re-running the SAME publish command must heal: before the fix, instance.json's presence routed it
    // to the enrolled path, which failed "could not determine your workspace" without consulting the
    // WAL — and the standup receipt's own next-action is to re-run this exact command (a wedge). The
    // standup device-auth seam is never touched (nothing to poll) — the panicking connector proves it.
    let ctx = rig.ctx();
    let contribute = |_b: &str| -> Box<dyn ContributeSource> { Box::new(SigningPlane::new()) };
    let standup = ops::StandupConnectors {
        enroll: &panicking_standup_connect,
        base_url: HOSTED.to_owned(),
    };
    let outcome = ops::publish(
        &ctx,
        &contribute,
        &standup,
        None, // roots — already-tracked skill, no auto-add
        &approve,
        false,
        None,
        None,
        None,
    )
    .expect("the torn promotion heals and the genesis publish lands");
    let ops::PublishOutcome::Published(data) = outcome else {
        panic!("expected Published, got a pending/proposed outcome");
    };
    assert_eq!(
        data.current_generation,
        Some(Generation { epoch: 1, seq: 1 }),
        "the genesis publish completed"
    );
    let receipt = data
        .standup
        .expect("the surviving WAL still carried the standup disclosure");
    assert_eq!(
        receipt.owner_principal.as_deref(),
        Some("robert@example.com")
    );

    // The promotion completed: user.json healed, the WAL gone; a THIRD run is the ordinary enrolled
    // publish (idempotent op-WAL replay territory), not another heal.
    let user = enroll::read_user(&rig.fs, &layout)
        .unwrap()
        .expect("user.json was written");
    let m = user
        .membership(STANDUP_WS)
        .expect("the standup workspace membership");
    assert!(!m.invite_rooted);
    assert!(enroll::read_wal(&rig.fs, &layout).unwrap().is_none());
}

#[test]
fn an_unknown_enrollment_method_fails_closed() {
    let rig = Rig::new("unknown-method");
    let mut fake = FakeClaim::new(0);
    fake.bootstrap.plane.enrollment_method = "quantum_handshake".to_owned();
    let err = run_claim_follow(&rig, &fake, Some(&format!("{CLAIM_BASE}/i/claimtok"))).unwrap_err();
    assert!(matches!(err, ClientError::Enrollment(_)), "got {err:?}");
    assert!(err.to_string().contains("quantum_handshake"), "{err}");
    // Fail-CLOSED: no WAL, no pin, no enrollment.
    assert!(enroll::read_wal(&rig.fs, &rig.layout()).unwrap().is_none());
    assert!(
        enroll::read_instance(&rig.fs, &rig.layout())
            .unwrap()
            .is_none()
    );
    assert_eq!(fake.claim_calls.get(), 0, "nothing was redeemed");
}

// ---------------------------------------------------------------------------------------------
// The denied-redeem guidance (REQUEST_ACCESS) mapping.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_denied_redeem_maps_to_request_access_with_the_ask_an_owner_message() {
    let err = ClientError::RedeemDenied {
        code: "DENIED".to_owned(),
    };
    let envelope = crate::render::err_envelope("follow", &err);
    assert!(!envelope.ok);
    let wire = envelope.error.expect("a wire error");
    assert_eq!(wire.code, "DENIED");
    assert_eq!(wire.outcome, TerminalOutcome::Denied);
    assert_eq!(
        envelope
            .next_actions
            .iter()
            .map(|a| a.code.as_str())
            .collect::<Vec<_>>(),
        vec![ActionCode::RequestAccess.as_str()],
        "the denied redeem carries the existing REQUEST_ACCESS action code"
    );
    let msg = crate::render::safe_message(&err);
    assert!(
        msg.contains("topos invite <your-email>") && msg.contains("re-run `topos follow`"),
        "the ask-an-owner guidance names the exact commands: {msg}"
    );
}

// ---------------------------------------------------------------------------------------------
// A standup-promoted follower keeps working with the ordinary machinery (sanity: the enrolled state a
// standup writes is the SAME enrolled state follow writes — the pull sweep stays an honest no-op with
// nothing followed).
// ---------------------------------------------------------------------------------------------

#[test]
fn after_a_standup_the_pull_sweep_is_an_honest_no_op() {
    let rig = Rig::new("post-standup");
    let (name, digest_hex) = rig.adopt("deploy", "# deploy v1\n");
    let approve = format!("{name}@{digest_hex}");
    let _ = run_publish(&rig, &FakeStandup::new(Poll::Pending), &approve, false).unwrap();
    let outcome = run_publish(&rig, &FakeStandup::new(Poll::Granted), &approve, false).unwrap();
    assert!(matches!(outcome, ops::PublishOutcome::Published(_)));

    // The standup enrolled with ZERO followed skills — a pull sweep finds nothing to do.
    let ctx = rig.ctx();
    let swept = ops::pull(&ctx, ops::PullScope::AllFollowed)
        .expect("a bare sweep after a standup is a clean no-op");
    assert!(swept.data.skills.is_empty());
    assert_eq!(
        swept
            .data
            .skills
            .iter()
            .filter(|s| s.action != PullAction::UpToDate)
            .count(),
        0
    );
}
