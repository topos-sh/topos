//! The composed-e2e fixture rig (feature `test-fixtures`; never in a production build) — the
//! SESSION-MODEL client, driven over the GENUINE `ureq` transports so an external e2e crate can
//! prove the whole loop against the real web app: `login` (the browser-approval flow), the
//! manifest verbs (`add`/`remove`), the manifest reconcile (`update`), the trust rail
//! (`status`), and the governance verbs (`publish` with the default governance transfer,
//! `review`, `protect`, `invite`) — each wired exactly as the composition root wires them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

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
        Self(dir)
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
        marker_id: "e2e".into(),
        state: TriggerState::Inactive,
    }
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

    /// The person-scope placement dir for a skill id (the work-harness layout).
    pub fn work_dir(&self, skill_id: &str) -> PathBuf {
        self.root.0.join("work").join(skill_id)
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
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id,
            layout,
            harness: &harness,
            plane: &routed,
            follow: &cache,
            roots: Some(AgentRoots {
                home: self.root.0.join("home"),
                cwd: cwd.map(Path::to_path_buf),
            }),
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
            let connectors = ops::LoginConnectors {
                enroll: &enroll_connect,
                delivery: &delivery_connect,
                web_origin: "https://topos.sh".to_owned(),
            };
            ops::session_login(ctx, &connectors, address).map_err(err_str)
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

    /// `topos add <reference>` (a workspace/catalog/channel reference; `global` = `-g`).
    pub fn add_reference(
        &self,
        reference: &str,
        global: bool,
        cwd: Option<&Path>,
    ) -> Result<AddData, String> {
        self.with_ctx(cwd, |ctx| {
            let connect = connect_session();
            ops::add_reference(ctx, &connect, None, reference, global).map_err(err_str)
        })
    }

    /// `topos add ./dir` — adopt a local folder (the manifest line rides the receipt).
    pub fn adopt_dir(&self, dir: &Path, cwd: Option<&Path>) -> Result<AddData, String> {
        self.with_ctx(cwd, |ctx| {
            let mut data = ops::add(ctx, dir).map_err(err_str)?;
            ops::note_added_path(ctx, &mut data, dir, false).map_err(err_str)?;
            Ok(data)
        })
    }

    /// `topos remove <targets…>` — the manifest arm; `Ok(true)` when it claimed the tokens.
    pub fn remove(&self, targets: &[&str], cwd: Option<&Path>) -> Result<bool, String> {
        self.with_ctx(cwd, |ctx| {
            let provided = ops::profile_provided_names(ctx);
            let owned: Vec<String> = targets.iter().map(|t| (*t).to_owned()).collect();
            ops::remove_from_manifests(ctx, &owned, &provided)
                .map(|r| r.is_some())
                .map_err(err_str)
        })
    }

    /// `topos remove -g <reference>` — `(kind, note)` from the receipt.
    pub fn remove_global(&self, reference: &str) -> Result<(String, Option<String>), String> {
        self.with_ctx(None, |ctx| {
            let connect = connect_session();
            ops::remove_reference_global(ctx, &connect, reference)
                .map(|d| {
                    let item = d.items.into_iter().next().expect("one item");
                    (format!("{:?}", item.kind), item.note)
                })
                .map_err(err_str)
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
                },
            )
            .map(|out| (out.data, out.warnings))
            .map_err(err_str)
        })
    }

    /// `topos status` — the offline trust rail, from `cwd`.
    pub fn status(&self, cwd: Option<&Path>) -> Result<StatusData, String> {
        self.with_ctx(cwd, |ctx| ops::status_snapshot(ctx).map_err(err_str))
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
            let legacy = |_b: &str, _c: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
                unreachable!("the session lane carries every publish")
            };
            match ops::publish(
                ctx,
                &legacy,
                None,
                Some(&connect),
                None,
                target,
                propose,
                to,
                None,
                message,
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
            let dir_legacy = |_b: &str| -> Box<dyn crate::plane::DirectorySource> {
                unreachable!("the session lane carries every review read")
            };
            let contrib_legacy =
                |_b: &str, _c: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
                    unreachable!("the session lane carries every review write")
                };
            let connectors = ops::ReviewConnectors {
                directory: &dir_legacy,
                contribute: &contrib_legacy,
                session: &connect,
            };
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
            let dir_legacy = |_b: &str| -> Box<dyn crate::plane::DirectorySource> {
                unreachable!("the session lane carries every protect write")
            };
            let connectors = ops::ProtectConnectors {
                directory: &dir_legacy,
                session: &connect,
            };
            ops::protect(ctx, &connectors, target, level, None, true)
                .map(|_| ())
                .map_err(err_str)
        })
    }

    /// `topos invite <emails…> --yes`.
    pub fn invite(&self, emails: &[&str]) -> Result<(), String> {
        self.with_ctx(None, |ctx| {
            let connect = connect_session();
            let gov_legacy = |_b: &str| -> Box<dyn crate::plane::GovernanceSource> {
                unreachable!("the session lane carries every invite")
            };
            let dir_legacy = |_b: &str| -> Box<dyn crate::plane::DirectorySource> {
                unreachable!("the session lane carries every invite read")
            };
            let connectors = ops::InviteConnectors {
                governance: &gov_legacy,
                directory: &dir_legacy,
                session: &connect,
            };
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
