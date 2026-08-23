//! The composed-e2e fixture rig (feature `test-fixtures`; never in a production build) — the
//! SESSION-MODEL client, driven over the GENUINE `ureq` transports so an external e2e crate can
//! prove the whole loop against the real web app: `login` (the browser-approval flow), the
//! manifest verbs (`add`/`remove`), the manifest reconcile (`update`), the trust rail
//! (`status`), and the governance verbs (`publish` with the default governance transfer,
//! `review`, `protect`, `invite`) — each wired exactly as the composition root wires them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use topos_harness::triggers::{TriggerAdapter, TriggerArtifact};
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::results::{AddData, LoginData, PullData, StatusData};
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::{AgentRoots, Ctx};
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::{RealClock, RealIds};
use crate::ops;
use crate::plane_http::{UreqDeviceClient, UreqPlane};
use crate::sessions::Session;
use crate::sidecar::Layout;

/// A self-cleaning temp directory (RAII — a failed test still tidies).
#[derive(Debug)]
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-e2e-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        // Canonical from birth, as the composition root resolves its own roots: `$TMPDIR` sits
        // behind the macOS `/var` symlink, and the sidecar home and the detection roots are
        // compared lexically — a rig spelling them differently would not be the real shape.
        Self(dir.canonicalize().expect("canonical scratch dir"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A harness adapter whose person-scope placement is an ABSOLUTE dir under the install's work
/// root — deterministic for the suites, no machine detection involved.
#[derive(Debug)]
struct WorkHarness {
    work: PathBuf,
}

impl HarnessAdapter for WorkHarness {
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
            dir: self.work.join(skill_id),
        }
    }
}

impl TriggerAdapter for WorkHarness {
    fn slug(&self) -> &'static str {
        HarnessId::ClaudeCode.slug()
    }

    fn install(&self) -> TriggerReport {
        no_trigger()
    }

    fn remove(&self) -> TriggerReport {
        no_trigger()
    }

    fn artifacts(&self) -> Vec<TriggerArtifact> {
        Vec::new()
    }

    fn present(&self) -> bool {
        !self.artifacts().is_empty()
    }
}

fn no_trigger() -> TriggerReport {
    TriggerReport {
        agent: "claude-code".to_owned(),
        currency_kind: CurrencyKind::ExplicitPullOnly,
        touched_path: None,
        marker_id: "e2e".into(),
        state: TriggerState::Inactive,

        note: None,
    }
}

/// The PLACEMENT port every in-crate suite rigs its `Ctx` with — one adapter, parameterized on the
/// only thing those rigs ever differed on: where a bundle's bytes land. It discovers nothing (so a
/// plain temp source is never harness-tagged) and answers `placement_for` alone; the TRIGGER port is
/// the shared inert one (`crate::ops::INERT_TRIGGER`), which is why nothing here reports a trigger.
#[derive(Debug)]
pub struct MockHarness {
    dir: MockDir,
}

/// The two placement shapes the suites need.
#[derive(Debug)]
enum MockDir {
    /// `<base>/<skill id>` verbatim — an empty base gives the bare id (the suites that read the
    /// placement out of `map.json` and never write through the adapter), a temp dir gives an
    /// absolute one.
    Join(PathBuf),
    /// Through the REAL naming ladder under a fixture skills root — for the suites that place bytes
    /// and assert the dir a collision lands in.
    Ladder(PathBuf),
}

impl MockHarness {
    /// Placement is `<base>/<skill id>`, no ladder: `""` gives the bare id.
    pub fn joining(base: impl Into<PathBuf>) -> Self {
        Self {
            dir: MockDir::Join(base.into()),
        }
    }

    /// Placement resolves through the real naming ladder under a fixture skills `root`.
    pub fn ladder(root: impl Into<PathBuf>) -> Self {
        Self {
            dir: MockDir::Ladder(root.into()),
        }
    }
}

impl HarnessAdapter for MockHarness {
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
        PlacementTarget {
            dir: match &self.dir {
                MockDir::Join(base) => base.join(skill_id),
                MockDir::Ladder(root) => topos_harness::choose_skill_dir(
                    root,
                    skill_id,
                    naming,
                    &topos_harness::dir_taken,
                    &|_| false,
                ),
            },
        }
    }
}

/// The active harness's USER skills root under a fixture `home`, resolved the one way the placement
/// engine resolves it (the registry row through the registry's resolver).
///
/// It asserts the answer stays inside the fixture, which is a real fence and not a formality: the
/// row's root honors `$CLAUDE_CONFIG_DIR`, and the suites write actual bytes into the dir it
/// computes. With that variable set, a silent pass would mean a test had just written into the
/// developer's own skills folder — so a rig that calls this requires it unset, and says so here
/// rather than finding out afterwards.
pub fn native_skills_root(home: &Path) -> PathBuf {
    let root = topos_harness::registry::skills_root(
        HarnessId::ClaudeCode.slug(),
        topos_harness::registry::SkillScope::User,
        home,
        None,
    )
    .expect("claude-code has a user skills root");
    assert!(
        root.starts_with(home),
        "this rig needs $CLAUDE_CONFIG_DIR unset — the skills root resolved to {root:?}, \
         outside the fixture home {home:?}"
    );
    root
}

/// One error surface for the suites: `"<CODE>: <display>"` — matchable on either half.
fn err_str(e: ClientError) -> String {
    format!("{}: {e}", e.code())
}

/// The simplified publish outcome the suites assert on.
#[derive(Debug)]
pub enum PublishView {
    Published {
        version_id: String,
        /// The governance-transfer receipt half (the manifest whose path line was rewritten).
        manifest: Option<String>,
        reference: Option<String>,
        converted_from: Option<String>,
    },
    Proposed {
        proposal: String,
    },
    /// The copy already matched `current` — a success with nothing to ship.
    NoChanges {
        /// The other scope's copy, when that is where the edits are.
        other_scope_draft: Option<topos_types::results::ScopeDraft>,
    },
}

/// One SESSION-MODEL installation: a fresh `~/.topos` + a work root for person-scope placements.
/// Every verb method wires the real seams exactly as the composition root does.
#[derive(Debug)]
pub struct SessionInstall {
    root: Scratch,
}

impl SessionInstall {
    pub fn new(tag: &str) -> Self {
        let root = Scratch::new(tag);
        std::fs::create_dir_all(root.0.join("home")).unwrap();
        std::fs::create_dir_all(root.0.join("work")).unwrap();
        Self { root }
    }

    /// The install's root dir (suites build project checkouts under it).
    pub fn root(&self) -> &Path {
        &self.root.0
    }

    /// The person-scope placement dir for a skill NAME — the active harness's registry row
    /// resolved under the install's own home, exactly as the planner resolves it (the row, not
    /// the adapter, names the dir now that the table is data; the `WorkHarness` answer is the
    /// planner's fallback only where no row can resolve). Asserted inside the fixture so a set
    /// `$CLAUDE_CONFIG_DIR` fails loudly instead of pointing an assertion at a real machine.
    pub fn skills_dir(&self, name: &str) -> PathBuf {
        self.skills_root().join(name)
    }

    /// The person-scope skills ROOT itself — for a suite that scans placements rather than
    /// naming one.
    pub fn skills_root(&self) -> PathBuf {
        native_skills_root(&self.root.0.join("home"))
    }

    fn layout(&self) -> Layout {
        Layout::new(&self.root.0.join(".topos"))
    }

    /// Whether the login WAL is on disk (a pending browser approval).
    pub fn wal_exists(&self) -> bool {
        self.layout().enrollment_path().exists()
    }

    /// The stored sessions as `(host, workspace_name, status)` rows.
    pub fn sessions(&self) -> Vec<(String, String, String)> {
        crate::sessions::read_sessions(&RealFs, &self.layout())
            .map(|all| {
                all.sessions
                    .into_iter()
                    .map(|s| (s.host, s.workspace_name, s.status))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every file under a placement dir as `(rel_path, exec_bits, bytes)`, sorted.
    pub fn dir_files(dir: &Path) -> Vec<(String, u32, Vec<u8>)> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u32, Vec<u8>)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    #[cfg(unix)]
                    let mode = {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::metadata(&path).unwrap().permissions().mode() & 0o111
                    };
                    #[cfg(not(unix))]
                    let mode = 0;
                    out.push((
                        path.strip_prefix(root).unwrap().display().to_string(),
                        mode,
                        std::fs::read(&path).unwrap(),
                    ));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    /// Run `f` over a freshly wired ctx (the composition-root wiring: session-routed plane +
    /// cache-backed follow seam + the work harness), with `cwd` as the manifest walk's start.
    fn with_ctx<R>(&self, cwd: Option<&Path>, f: impl FnOnce(&Ctx<'_>) -> R) -> R {
        let fs = RealFs;
        let ids = RealIds;
        let clock = RealClock;
        let layout = self.layout();
        let harness = WorkHarness {
            work: self.root.0.join("work"),
        };
        let device_id = crate::identity::load_or_create_device_id(&fs, &layout).expect("device id");
        let connect = connect_session();
        let routed = ops::SessionRoutedPlane::load(&fs, &layout, &connect);
        let cache = ops::CacheFollow::load(&fs, &layout);
        let ctx = Ctx {
            progress: crate::progress::silent(),
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id,
            layout,
            harness: &harness,
            // The same fixture on both ports: the e2e suites prove the loop, never a harness config.
            triggers: ops::Triggers::active_only(&harness),
            plane: &routed,
            follow: &cache,
            roots: Some(AgentRoots::new(
                self.root.0.join("home"),
                cwd.map(Path::to_path_buf),
            )),
        };
        f(&ctx)
    }

    // ---- sessions -----------------------------------------------------------------------------

    /// `topos login <address>` — start (or resume with `None`) the flow. A pending answer
    /// carries `data.pending`; the suites approve in the browser then call this again.
    pub fn login(&self, address: Option<&str>) -> Result<LoginData, String> {
        self.with_ctx(None, |ctx| {
            let enroll_connect = |base: &str| -> Box<dyn crate::plane::EnrollSource> {
                Box::new(UreqDeviceClient::new(base.to_owned(), None))
            };
            let delivery_connect =
                |base: &str, cred: &str, ws: &str| -> Box<dyn crate::plane::DeliverySource> {
                    Box::new(
                        UreqPlane::new(base.to_owned(), Some(cred.to_owned()), Default::default())
                            .with_workspaces(vec![ws.to_owned()]),
                    )
                };
            let lane_connect =
                |base: &str, cred: &str| -> Box<dyn crate::plane::GovernanceSource> {
                    Box::new(UreqDeviceClient::new(
                        base.to_owned(),
                        Some(cred.to_owned()),
                    ))
                };
            let directory_connect =
                |base: &str, cred: &str| -> Box<dyn crate::plane::DirectorySource> {
                    Box::new(UreqDeviceClient::new(
                        base.to_owned(),
                        Some(cred.to_owned()),
                    ))
                };
            let connectors = ops::LoginConnectors {
                enroll: &enroll_connect,
                delivery: &delivery_connect,
                lane: &lane_connect,
                directory: &directory_connect,
                web_origin: "https://topos.sh".to_owned(),
            };
            ops::session_login(ctx, &connectors, address, false).map_err(err_str)
        })
    }

    /// `topos logout --all` — the ended workspace names.
    pub fn logout_all(&self) -> Result<Vec<String>, String> {
        self.with_ctx(None, |ctx| {
            let revoke = |base: &str, cred: &str| -> Box<dyn crate::plane::GovernanceSource> {
                Box::new(UreqDeviceClient::new(
                    base.to_owned(),
                    Some(cred.to_owned()),
                ))
            };
            ops::session_logout(ctx, &revoke, None, true)
                .map(|d| d.ended)
                .map_err(err_str)
        })
    }

    // ---- manifests + reconcile ----------------------------------------------------------------

    /// `topos add <reference> --yes` (a workspace/catalog/channel/feed reference; `global` =
    /// `-g`). The suites drive the APPLIED arm; a describe surfacing here is a failure (only a
    /// git source describes, and these are workspace references). A project-scoped add records
    /// into a file that
    /// already exists (none in reach refuses), so the rig runs the idempotent `topos init` first,
    /// exactly as a person would.
    pub fn add_reference(
        &self,
        reference: &str,
        global: bool,
        cwd: Option<&Path>,
    ) -> Result<AddData, String> {
        self.with_ctx(cwd, |ctx| {
            if !global {
                ops::init(ctx, false, None).map_err(err_str)?;
            }
            let connect = connect_session();
            match ops::add_reference(
                ctx,
                &connect,
                None,
                reference,
                global,
                true,
                &Default::default(),
                None,
            )
            .map_err(err_str)?
            {
                ops::AddRefOutcome::Applied { data, .. } => Ok(*data),
                ops::AddRefOutcome::Described { data, .. } => {
                    Err(format!("unexpected describe: {data:?}"))
                }
            }
        })
    }

    /// `topos init` then `topos add ./dir` — adopt a local folder into THIS folder's manifest (the
    /// line rides the receipt). The `init` is not incidental: `add` records rows in a file that
    /// already exists and refuses when none covers the folder, so the rig mints the project
    /// manifest first, exactly as a person would.
    pub fn adopt_dir(&self, dir: &Path, cwd: Option<&Path>) -> Result<AddData, String> {
        self.with_ctx(cwd, |ctx| {
            ops::init(ctx, false, None).map_err(err_str)?;
            let mut data = ops::add(ctx, dir).map_err(err_str)?;
            ops::note_added_path(ctx, &mut data, dir, false).map_err(err_str)?;
            Ok(data)
        })
    }

    /// `topos add <dir> --as <bundle>` — the identity claim, in the scope `cwd` stands in: a folder
    /// that already holds a copy of a bundle this scope manages becomes one of its places.
    pub fn claim(
        &self,
        dir: &Path,
        as_bundle: &str,
        cwd: Option<&Path>,
    ) -> Result<AddData, String> {
        self.with_ctx(cwd, |ctx| {
            let scope = ops::add_scope(ctx, false).map_err(err_str)?;
            ops::claim(ctx, &scope, dir, as_bundle).map_err(err_str)
        })
    }

    /// An add receipt exactly as a terminal prints it — for a suite asserting the LINE a person
    /// reads, not just the typed outcome behind it.
    #[must_use]
    pub fn add_receipt_tty(data: &AddData) -> String {
        crate::render::add_tty(data)
    }

    /// `topos remove <targets…> --yes` — the manifest arm; `Ok(true)` when it claimed the tokens.
    pub fn remove(&self, targets: &[&str], cwd: Option<&Path>) -> Result<bool, String> {
        self.with_ctx(cwd, |ctx| {
            let connect = connect_session();
            let owned: Vec<String> = targets.iter().map(|t| (*t).to_owned()).collect();
            ops::remove_project(ctx, &connect, &owned, None, true, &Default::default())
                .map(|r| r.is_some())
                .map_err(err_str)
        })
    }

    /// `topos remove -g <reference> --yes` — `(kind, note)` from the applied receipt.
    pub fn remove_global(&self, reference: &str) -> Result<(String, Option<String>), String> {
        self.with_ctx(None, |ctx| {
            let connect = connect_session();
            match ops::remove_global(
                ctx,
                &connect,
                &[reference.to_owned()],
                None,
                true,
                &Default::default(),
            )
            .map_err(err_str)?
            {
                ops::RemoveOutcome::Applied(d) => {
                    let item = d.items.into_iter().next().expect("one item");
                    Ok((format!("{:?}", item.kind), item.note))
                }
                ops::RemoveOutcome::Described { data, .. } => {
                    Err(format!("unexpected describe under --yes: {:?}", data.items))
                }
            }
        })
    }

    /// `topos update [targets…]` — the manifest reconcile, from `cwd`.
    pub fn update(
        &self,
        targets: &[&str],
        cwd: Option<&Path>,
    ) -> Result<(PullData, Vec<String>), String> {
        self.with_ctx(cwd, |ctx| {
            let connect = connect_session();
            ops::manifest_update(
                ctx,
                &connect,
                None,
                &ops::ManifestUpdateOpts {
                    targets: targets.iter().map(|t| (*t).to_owned()).collect(),
                    ack_notices: true,
                    rebuild: false,
                    // A hand-run `topos update`: the scope rule applies, so `cwd` inside a
                    // checkout converges that project and a `cwd`-less call converges the machine.
                    scope: ops::UpdateScope::Here,
                    // No forge lane is wired in these composed fixtures (`git` is `None` above),
                    // so the cadence never comes into play; the hand-run posture is the honest one.
                    forge: ops::ForgeCadence::Now,
                    // The fixture's `update` IS the typed verb — follow rows re-resolve and the
                    // project lock rewrites, exactly as `topos update` runs it.
                    lock: ops::LockMode::Update,
                },
            )
            // The lines a sweep emits, merged exactly as the `--json` envelope merges them:
            // failures first, then the disclosures (a settled-draft fan-out, a version split) —
            // and rendered through the SAME legacy derivation the envelope's `warnings` array
            // uses, so a composed fixture reads exactly what a consumer of that array reads.
            .map(|out| {
                let mut messages = out.warnings;
                messages.extend(out.disclosures);
                (out.data, crate::message::legacy_lines(&messages))
            })
            .map_err(err_str)
        })
    }

    /// `topos status` — the offline health panel, from `cwd` (the here-scope view).
    pub fn status(&self, cwd: Option<&Path>) -> Result<StatusData, String> {
        self.with_ctx(cwd, |ctx| {
            ops::status_snapshot(ctx, ops::ScopeView::Here).map_err(err_str)
        })
    }

    // ---- governance ---------------------------------------------------------------------------

    /// `topos publish <target> [--propose] [--to <channel>] [-m <msg>]` (the `--yes` apply).
    pub fn publish(
        &self,
        target: &str,
        propose: bool,
        to: Option<&str>,
        message: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<PublishView, String> {
        self.with_ctx(cwd, |ctx| {
            let connect = connect_session();
            match ops::publish(
                ctx,
                Some(&connect),
                None,
                target,
                propose,
                to,
                None,
                message,
                &ops::Selection::default(),
                ops::StoreScope::Here,
            )
            .map_err(err_str)?
            {
                ops::PublishOutcome::Published(d) => Ok(PublishView::Published {
                    version_id: d.version_id,
                    manifest: d.manifest,
                    reference: d.reference,
                    converted_from: d.converted_from,
                }),
                ops::PublishOutcome::Proposed(d) => Ok(PublishView::Proposed {
                    proposal: d.proposal,
                }),
                ops::PublishOutcome::NoChanges(d) => Ok(PublishView::NoChanges {
                    other_scope_draft: d.other_scope_draft,
                }),
            }
        })
    }

    /// `topos review <skill>@<hash> --approve|--reject|--withdraw` — a Debug view of the outcome.
    pub fn review(
        &self,
        target: &str,
        verdict: &str,
        message: Option<&str>,
    ) -> Result<String, String> {
        self.with_ctx(None, |ctx| {
            let connect = connect_session();
            let connectors = ops::ReviewConnectors { session: &connect };
            let verdict = match verdict {
                "approve" => ops::ReviewVerdict::Approve,
                "reject" => ops::ReviewVerdict::Reject {
                    reason: message.map(str::to_owned),
                },
                "withdraw" => ops::ReviewVerdict::Withdraw,
                other => panic!("unknown verdict {other}"),
            };
            ops::review_dispatch(
                ctx,
                &connectors,
                Some(target),
                Some(verdict),
                None,
                ops::DiffBudget::unlimited(),
            )
            .map(|o| format!("{o:?}"))
            .map_err(err_str)
        })
    }

    /// `topos protect <target> [<level>] --yes`.
    pub fn protect(&self, target: &str, level: Option<&str>) -> Result<(), String> {
        self.with_ctx(None, |ctx| {
            let connect = connect_session();
            let connectors = ops::ProtectConnectors { session: &connect };
            ops::protect(ctx, &connectors, target, level, None, true)
                .map(|_| ())
                .map_err(err_str)
        })
    }

    /// `topos invite <emails…> --yes`.
    pub fn invite(&self, emails: &[&str]) -> Result<(), String> {
        self.with_ctx(None, |ctx| {
            let connect = connect_session();
            let connectors = ops::InviteConnectors { session: &connect };
            let owned: Vec<String> = emails.iter().map(|e| (*e).to_owned()).collect();
            ops::invite(ctx, &connectors, owned, None, None, None, true)
                .map(|_| ())
                .map_err(err_str)
        })
    }
}

/// The per-session transports, wired exactly as the composition root wires them (one
/// byte/delivery lane + one directory/write lane per session, each under that session's OWN
/// workspace-scoped credential).
fn connect_session() -> impl Fn(&Session) -> ops::SessionTransports {
    |s: &Session| ops::SessionTransports {
        plane: Box::new(
            UreqPlane::new(
                s.base_url.clone(),
                Some(s.credential.clone()),
                Default::default(),
            )
            .with_workspaces(vec![s.workspace_id.clone()]),
        ),
        directory: Box::new(UreqDeviceClient::new(
            s.base_url.clone(),
            Some(s.credential.clone()),
        )),
        contribute: Box::new(UreqDeviceClient::new(
            s.base_url.clone(),
            Some(s.credential.clone()),
        )),
        governance: Box::new(UreqDeviceClient::new(
            s.base_url.clone(),
            Some(s.credential.clone()),
        )),
    }
}
