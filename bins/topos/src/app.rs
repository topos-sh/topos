//! The composition root for the binary: parse argv, wire the real seams, run recovery, dispatch a verb,
//! and emit either the `--json` envelope (stdout) or thin TTY text.

use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use clap::Parser;
use serde::Serialize;
use topos_harness::{ClaudeCode, ConfigStore, HarnessAdapter, OpenClaw};
use topos_types::HarnessId;

use crate::cli::{AuthCmd, Cli, Command};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::{FsOps, RealFs};
use crate::ids::{Clock, RealClock, RealIds};
use crate::plane::{
    ContributeSource, DirectorySource, EnrollSource, GovernanceSource, ReconcileTransport,
};
use crate::plane_http::{UreqDeviceClient, UreqPlane};
use crate::sidecar::{Layout, recover};
use crate::{identity, logfile, ops, render};

/// Run the CLI; returns the process exit code. A thin wrapper over the dispatch: AFTER a successful
/// eligible command it runs the passive version check (stderr-only, self-throttled to at most one
/// probe per day, silent on every failure — `ops::version_check` owns the policy). The check never
/// runs on the quiet sweep (the session-start hook path gains zero latency and zero noise), on
/// `self-update`/`upgrade` (they ARE the release surface), on `uninstall` (it would recreate the
/// state dir the command just deleted), or on a failed command.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    // The invocation AS TYPED, past the binary name. The composition root is the only place that
    // legitimately holds it, and the error surfaces need it: a refusal that offers a rebuilt
    // command must first know whether the rebuild would be the whole of what was asked (see
    // `render::err_hint_tty`). Read through `args_os` — `std::env::args()` PANICS on any
    // non-UTF-8 element (even a launcher's argv[0], which `skip` still has to step over). A
    // non-UTF-8 argument is dropped instead — a lossy token must never become part of an offered
    // command, and its absence only makes the surfaces more conservative.
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .filter_map(|a| a.into_string().ok())
        .collect();
    // No subcommand: on a TTY, orient (the status snapshot / the unenrolled welcome, exit 0);
    // piped or under `--json`, keep the classic usage error (stderr, exit 2) so scripts fail
    // loudly. The version nag never rides the bare invocation — orientation stays offline.
    let Some(command) = cli.command else {
        return run_bare(cli.json, cli.workspace, &argv);
    };
    let check = version_check_applies(&command) && ops::version_check_env_allows();
    let code = run_command(cli.json, cli.workspace, command, false, &argv);
    if check && code == ExitCode::SUCCESS {
        let fs = RealFs;
        let clock = RealClock;
        let layout = Layout::new(&resolve_home());
        let now_ms = i64::try_from(clock.now_unix_millis()).unwrap_or(i64::MAX);
        let probe = crate::plane_http::UreqVersionProbe::new();
        if let Some(line) = ops::version_nag(&fs, &layout, now_ms, &probe) {
            // stderr ONLY — stdout already carries the command's document (`--json` stays byte-clean).
            eprintln!("{line}");
        }
    }
    code
}

/// Whether the passive version check may run after `command`. Excluded: the quiet sweep (any
/// `update --quiet` — hooks fire on every session event and must stay silent + latency-free),
/// `self-update` and its `upgrade` disambiguation (that surface just spoke to releases itself),
/// and `uninstall` (the check's stamp would recreate `~/.topos/state` after the teardown).
fn version_check_applies(command: &Command) -> bool {
    !matches!(
        command,
        Command::SelfUpdate { .. }
            | Command::Upgrade
            | Command::Uninstall { .. }
            | Command::Update { quiet: true, .. }
            // `status` promises "offline, nothing dialed" — the passive probe would break it.
            | Command::Status { .. }
    )
}

/// The bare `topos` invocation (no subcommand). A TTY gets the orientation render — the same
/// snapshot `topos status` computes, with the unenrolled welcome as its short form — and exit 0.
/// Anything scripted (piped stdout, or `--json`) keeps the classic clap usage error on stderr with
/// exit 2, so automation that forgot a verb still fails loudly.
fn run_bare(json: bool, workspace: Option<String>, argv: &[String]) -> ExitCode {
    use std::io::IsTerminal;
    if json || !std::io::stdout().is_terminal() {
        let mut cmd = crate::cli::cli_command();
        let err = cmd.error(
            clap::error::ErrorKind::MissingSubcommand,
            "'topos' requires a subcommand but one was not provided",
        );
        let _ = err.print();
        return ExitCode::from(2);
    }
    run_command(
        false,
        workspace,
        Command::Status {
            global: false,
            all: false,
        },
        true,
        argv,
    )
}

/// Parse-free dispatch: wire the real seams, run recovery, dispatch the verb, emit the outcome.
/// `bare` marks the no-subcommand TTY orientation (it only softens `status`'s render on a fresh
/// machine — the welcome instead of the full snapshot).
fn run_command(
    json: bool,
    workspace: Option<String>,
    command: Command,
    bare: bool,
    argv: &[String],
) -> ExitCode {
    // The global `--workspace` — which workspace the ambient write verbs act in (and the filter that
    // disambiguates a skill name shared across workspaces). Optional; inferred with a single workspace.
    // Canonicalized below (name → id) once the layout exists, so every consumer keeps id semantics.
    let cmd_name = command.name();

    let fs = RealFs;
    let ids = RealIds;
    let clock = RealClock;
    let layout = Layout::new(&resolve_home());
    // `--workspace` accepts the ADDRESS name as well as the opaque id — canonicalized ONCE here
    // (name → joined id, best-effort), so every downstream consumer keeps id semantics.
    let workspace = crate::sessions::canonicalize_workspace_flag(&fs, &layout, workspace);
    // The error-side diagnostics channel: every failure that reaches a finisher below lands its full
    // detail in the append-only log the redacted user surfaces point at.
    let diag = Diag {
        fs: &fs,
        clock: &clock,
        log_path: layout.log_path(),
        argv,
    };
    // The harness adapter, selected through the one dispatch seam. v0 wires Claude Code only; the
    // adapter touches its config home only when adopting a recognized skill, arming auto-updates, or on
    // uninstall.
    let harness = adapter_for(HarnessId::ClaudeCode, &fs, &fs);
    // The ACTIVITY channel (stderr only — stdout stays the one document). Animated on a terminal,
    // one plain line per phase when piped, and byte-silent for the harness hook sweep, which fires
    // on every session-start-shaped event and must cost nothing on either stream. Held as an `Rc`
    // because the transports built below are boxed `'static` trait objects: they cannot borrow the
    // binding, so each one clones a handle to the same sink.
    let quiet_sweep = matches!(&command, Command::Update { quiet: true, .. });
    let progress = crate::progress::select(quiet_sweep);

    // `uninstall` dispatches BEFORE state recovery and enrollment loading: its whole point is to
    // remove `~/.topos/` even when that state is corrupt — an unreadable/newer credentials doc or a
    // torn sidecar must never block the one command that deletes them. It needs no sign-in, mints no
    // identity, and touches no plane; the binary path is disclosed but never self-deleted (a package
    // manager may own it).
    if let Command::Uninstall { yes } = &command {
        let inert_plane = crate::plane::InertPlane;
        let inert_follow = crate::plane::InertFollow;
        let ctx = Ctx {
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: String::new(),
            layout: layout.clone(),
            harness: harness.as_ref(),
            plane: &inert_plane,
            follow: &inert_follow,
            roots: None,
            // The teardown is entirely local (delete `~/.topos/`, scrub the triggers) — nothing to
            // report activity about.
            progress: crate::progress::silent(),
        };
        let binary = std::env::current_exe().ok();
        // The breadth scrub rides the applied receipt: after the active adapter's hook scrub,
        // every OTHER agent's trigger artifact is removed too (or its survival disclosed —
        // OpenClaw's gateway may be down). Swept over the SUPPORTED set, not detection: an
        // artifact must be scrubbed even when its harness's detect dir has since vanished.
        let result = ops::uninstall(&ctx, binary, *yes).map(|outcome| match outcome {
            ops::UninstallOutcome::Applied(mut applied) => {
                if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
                    applied.triggers = ops::scrub_all(&home, harness.id().slug(), &fs, &fs);
                }
                ops::UninstallOutcome::Applied(applied)
            }
            other => other,
        });
        return finish_uninstall(json, cmd_name, result, &diag);
    }

    // `status` (and the bare-`topos` orientation that reuses it) also dispatches BEFORE the
    // recovery sweep: the verb promises offline AND read-only, and recovery WRITES (it reaps an
    // expired enrollment WAL, removes torn staging, repairs logs). Its finisher likewise never
    // appends to the diagnostics log — a status run leaves the sidecar byte-identical, proven by
    // `ops::status`'s pending-recovery-fixture test.
    if let Command::Status { global, all } = &command {
        let view = match (global, all) {
            (true, _) => ops::ScopeView::Machine,
            (_, true) => ops::ScopeView::All,
            _ => ops::ScopeView::Here,
        };
        let inert_plane = crate::plane::InertPlane;
        let inert_follow = crate::plane::InertFollow;
        let ctx = Ctx {
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: String::new(),
            layout: layout.clone(),
            harness: harness.as_ref(),
            plane: &inert_plane,
            follow: &inert_follow,
            roots: std::env::var_os("HOME").map(|h| crate::ctx::AgentRoots {
                home: PathBuf::from(h),
                cwd: std::env::current_dir().ok(),
            }),
            // `status` (and the bare-`topos` orientation over it) promises OFFLINE: it dials
            // nothing, so it has no activity to report. The silent sink makes that structural
            // rather than a thing every code path must remember not to do.
            progress: crate::progress::silent(),
        };
        // The snapshot from local state; the trigger rows from the read-only probe at this root
        // (the one layer holding the real config port + $HOME) — the same layering the arming
        // receipts use, minus every write.
        let result = ops::status_snapshot(&ctx, view).map(|mut data| {
            if let Some(r) = &ctx.roots {
                data.triggers =
                    ops::probe_detected(&r.home, r.cwd.as_deref(), harness.as_ref(), &fs);
            }
            data
        });
        return finish_status(json, cmd_name, result, bare, argv);
    }

    // Recovery runs at the start of every command (it also abandons an expired, never-redeemed
    // enrollment WAL against the real wall clock).
    let now_millis = i64::try_from(clock.now_unix_millis()).unwrap_or(i64::MAX);
    let mut recovery_warnings = Vec::new();
    if let Err(e) = recover(&fs, &layout, now_millis, &mut recovery_warnings) {
        return emit_err(json, cmd_name, &e, &diag);
    }
    // Recovery's typed disclosures (a refused park-journal entry) ride stderr — diagnostics,
    // never the envelope, so `--json` stdout stays the one document.
    for warning in &recovery_warnings {
        eprintln!("topos: {warning}");
    }

    // The plane + follow-state sources — SESSIONS are the identity: the routed plane sends each
    // per-skill read down the session lane its workspace names (the delivery cache supplies the
    // map), and the cache-backed follow seam answers workspace provenance. With no session both
    // stay truthful no-ops (nothing delivered, nothing served).
    let connect_for_wiring = |s: &crate::sessions::Session| -> ops::SessionTransports {
        ops::SessionTransports {
            plane: Box::new(
                UreqPlane::new(
                    s.base_url.clone(),
                    Some(s.credential.clone()),
                    Default::default(),
                )
                .with_workspaces(vec![s.workspace_id.clone()])
                .with_progress(Rc::clone(&progress)),
            ),
            directory: Box::new(
                UreqDeviceClient::new(s.base_url.clone(), Some(s.credential.clone()))
                    .with_progress(Rc::clone(&progress)),
            ),
            contribute: Box::new(
                UreqDeviceClient::new(s.base_url.clone(), Some(s.credential.clone()))
                    .with_progress(Rc::clone(&progress)),
            ),
            governance: Box::new(
                UreqDeviceClient::new(s.base_url.clone(), Some(s.credential.clone()))
                    .with_progress(Rc::clone(&progress)),
            ),
        }
    };
    let routed_plane = ops::SessionRoutedPlane::load(&fs, &layout, &connect_for_wiring);
    let cache_follow = ops::CacheFollow::load(&fs, &layout);
    let (plane, follow): (
        &dyn crate::plane::PlaneSource,
        &dyn crate::plane::FollowSource,
    ) = (&routed_plane, &cache_follow);

    // `add` and `pull` author commits (adoption / a draft snapshot before a divergence), so both load
    // (and on first use, mint) the device identity — `uninstall` must never create or require it before
    // tearing the home down.
    let device_id = match &command {
        // `follow` also loads (and on first use mints) the device identity: its skill path authors a
        // draft snapshot through the pull engine. `publish` authors the candidate commit and `revert`
        // the forward-revert commit, so both load (and on first use mint) the device id.
        Command::Add { .. }
        | Command::Update { .. }
        | Command::Publish { .. }
        | Command::Revert { .. } => match identity::load_or_create_device_id(&fs, &layout) {
            Ok(d) => d,
            Err(e) => return emit_err(json, cmd_name, &e, &diag),
        },
        _ => String::new(),
    };
    let ctx = Ctx {
        fs: &fs,
        ids: &ids,
        clock: &clock,
        device_id,
        layout,
        harness: harness.as_ref(),
        plane,
        follow,
        // The machine roots the placement engine detects agents against — the same `$HOME` + cwd
        // resolution untracked discovery uses. Absent `$HOME` degrades to the classic single-dir
        // placement (the active adapter's), never an error.
        roots: std::env::var_os("HOME").map(|h| crate::ctx::AgentRoots {
            home: PathBuf::from(h),
            cwd: std::env::current_dir().ok(),
        }),
        progress: &*progress,
    };

    // The credentialed device connectors — every credentialed route presents the device's ONE Bearer
    // credential. Built as closures so `credentials.json` is re-read FRESH on each build: a `follow
    // <address>` mints it mid-invocation (at the granted poll), and the continued describe/apply must
    // see the freshly-minted credential.
    let connect_governance = |base_url: &str| -> Box<dyn GovernanceSource> {
        Box::new(
            UreqDeviceClient::new(
                base_url.to_owned(),
                load_device_credential(ctx.fs, &ctx.layout),
            )
            .with_progress(Rc::clone(&progress)),
        )
    };
    let connect_contribute =
        |base_url: &str, credential: Option<&str>| -> Box<dyn ContributeSource> {
            Box::new(
                UreqDeviceClient::new(base_url.to_owned(), credential.map(str::to_owned))
                    .with_progress(Rc::clone(&progress)),
            )
        };
    // The DIRECTORY connector (describe reads + subscription/curation/notice row ops) and the
    // RECONCILE connector (delivery + fleet report + the per-skill read lane on one object) — both
    // re-read the on-disk credential fresh per build, for the same mid-invocation reason as above.
    let connect_directory = |base_url: &str| -> Box<dyn DirectorySource> {
        Box::new(
            UreqDeviceClient::new(
                base_url.to_owned(),
                load_device_credential(ctx.fs, &ctx.layout),
            )
            .with_progress(Rc::clone(&progress)),
        )
    };
    // LEGACY connector shape kept for the mid-migration op signatures — creds-less (the session
    // lanes carry the real credentials).
    let connect_delivery = |base_url: &str| -> Box<dyn ReconcileTransport> {
        Box::new(
            UreqPlane::new(base_url.to_owned(), None, Default::default())
                .with_progress(Rc::clone(&progress)),
        )
    };
    // The per-SESSION transports (the manifest model): one byte/delivery lane + one directory
    // lane per logged-in workspace, each under that session's OWN workspace-scoped credential.
    let connect_session_transports = |s: &crate::sessions::Session| -> ops::SessionTransports {
        ops::SessionTransports {
            plane: Box::new(
                UreqPlane::new(
                    s.base_url.clone(),
                    Some(s.credential.clone()),
                    Default::default(),
                )
                .with_workspaces(vec![s.workspace_id.clone()])
                .with_progress(Rc::clone(&progress)),
            ),
            directory: Box::new(
                UreqDeviceClient::new(s.base_url.clone(), Some(s.credential.clone()))
                    .with_progress(Rc::clone(&progress)),
            ),
            contribute: Box::new(
                UreqDeviceClient::new(s.base_url.clone(), Some(s.credential.clone()))
                    .with_progress(Rc::clone(&progress)),
            ),
            governance: Box::new(
                UreqDeviceClient::new(s.base_url.clone(), Some(s.credential.clone()))
                    .with_progress(Rc::clone(&progress)),
            ),
        }
    };
    // The ENROLLMENT connector: `UreqDeviceClient` with NO credential — the device-flow routes are
    // unauthenticated (they mint the credential the other connectors then present). A closure like
    // its siblings, so it carries this invocation's activity sink.
    let connect_enroll = |base_url: &str| -> Box<dyn EnrollSource> {
        Box::new(
            UreqDeviceClient::new(base_url.to_owned(), None).with_progress(Rc::clone(&progress)),
        )
    };
    // The default WEB origin the enrollment doors dial on a fresh install (`follow <bare-ws>`,
    // `auth login`): the env override, else the hosted web origin (the card re-roots onto the API).
    let web_origin = resolve_web_origin(std::env::var("TOPOS_PLANE_URL").ok());

    match command {
        // Dispatched BEFORE state recovery above — the read-only promise admits no sweep write.
        Command::Status { .. } => unreachable!("status dispatches before state recovery"),
        // `init` — create this folder's `topos.toml`, or (with `-g`) materialize the global
        // manifest (idempotent; a no-op receipt when the file exists).
        Command::Init { global } => finish(
            json,
            cmd_name,
            ops::init(&ctx, global),
            render::init_tty,
            &diag,
        ),
        // `fmt [-g]` — rewrite the target manifest into the normal form (atomic write on change).
        Command::Fmt { global } => finish(
            json,
            cmd_name,
            ops::fmt_manifest(&ctx, global),
            render::fmt_tty,
            &diag,
        ),
        // `login [<address>]` — mint ONE workspace-scoped session against a SERVER (the workspace
        // is chosen in the browser; a machine already logged into that server takes the
        // browser-free lane). The same blocking idiom as every device-auth verb: a TTY (or
        // `--wait`) run re-polls until the approval settles; piped/`--json` without `--wait`
        // never hangs.
        Command::Login { address, wait } => {
            let connect_session_delivery =
                |base: &str, cred: &str, ws: &str| -> Box<dyn crate::plane::DeliverySource> {
                    Box::new(
                        UreqPlane::new(base.to_owned(), Some(cred.to_owned()), Default::default())
                            .with_workspaces(vec![ws.to_owned()])
                            .with_progress(Rc::clone(&progress)),
                    )
                };
            let connect_session_lane = |base: &str, cred: &str| -> Box<dyn GovernanceSource> {
                Box::new(
                    UreqDeviceClient::new(base.to_owned(), Some(cred.to_owned()))
                        .with_progress(Rc::clone(&progress)),
                )
            };
            let connectors = ops::LoginConnectors {
                enroll: &connect_enroll,
                delivery: &connect_session_delivery,
                lane: &connect_session_lane,
                web_origin: web_origin.clone(),
            };
            let stdout_tty = {
                use std::io::IsTerminal;
                std::io::stdout().is_terminal()
            };
            let policy = WaitPolicy::resolve(json, wait, stdout_tty, &clock);
            // THE BINDING IS DECIDED BEFORE THE FLOW IS STARTED, because it is what the flow is
            // started AS: the approval page pre-arms from the URL-borne challenge and redirects
            // to this listener, so the listener must already exist when we declare it — and the
            // server records the binding write-once. Bind first, declare second: if the bind
            // fails we say so and run the classic typed-code flow, rather than promising a
            // hand-off we cannot receive.
            let opener = if policy.block && !json {
                let interactive = {
                    use std::io::IsTerminal;
                    std::io::stderr().is_terminal()
                };
                ops::loopback::choose_browser(&ops::loopback::BrowserEnv::detect(
                    interactive,
                    stdout_tty,
                    wait.is_some(),
                ))
            } else {
                None
            };
            let bound = opener.and_then(|opener| {
                let state = uuid::Uuid::new_v4().simple().to_string();
                let listener = ops::loopback::LoopbackListener::bind(state.clone()).ok()?;
                Some((opener, listener, state))
            });
            if opener.is_some() && bound.is_none() {
                // Never a silent downgrade to the phishable path: a person who expected the
                // browser hand-off should know they are now typing a code for a reason.
                eprintln!(
                    "note: could not open a local listener — falling back to the typed-code login."
                );
            }
            let holds_listener = bound.is_some();
            // A FRESH start declares the flow loopback-bound exactly when this invocation holds
            // the listener the approval page will redirect to (declaring less once shipped a
            // device-bound flow behind a loopback terminal: the page could not pre-arm, the
            // suppressed code was nowhere, and the wait ran out the whole TTL).
            let first = ops::session_login(&ctx, &connectors, address.as_deref(), holds_listener);
            // The auto-open plan: the page URL needs the flow's challenge, which only exists once
            // the start above has written the WAL. Only a LOOPBACK-BOUND flow gets the plan — a
            // resumed device-bound flow (started piped, resumed on a TTY) keeps the typed-code
            // ceremony, and the plan's absence is what makes the wait print its code.
            let loopback_plan = bound.and_then(|(opener, listener, state)| {
                let wal = crate::enroll::read_wal(&fs, &ctx.layout).ok().flatten()?;
                if !wal.loopback {
                    return None;
                }
                Some(LoopbackPlan {
                    opener,
                    runner: &fs,
                    challenge: ops::device_challenge(&wal.device_code),
                    listener,
                    state,
                })
            });
            let result = block_on_pending(
                &clock,
                &*progress,
                &policy,
                first,
                session_login_pending_disclosure,
                loopback_plan,
                || ops::session_login(&ctx, &connectors, address.as_deref(), holds_listener),
            );
            // The breadth arming sweep + the built-in skill ride the completed login (the
            // acceptance event is the trigger-arming moment), exactly as on `follow`'s receipt.
            let result = result.map(|mut data| {
                if data.currency.is_some() {
                    data.triggers = breadth_arm(&ctx.roots, harness.as_ref(), &fs);
                    if let Err(e) = ops::ensure_builtin(&ctx) {
                        let _ = diag.note(cmd_name, &e);
                    }
                }
                data
            });
            finish_session_login(json, cmd_name, result, &diag)
        }
        // `logout [<workspace>|--all]` — end session(s); immediate (a fresh `login` is the way
        // back, and the receipt reports the server-side outcome honestly).
        Command::Logout { workspace: ws, all } => {
            let connect_session_revoke = |base: &str, cred: &str| -> Box<dyn GovernanceSource> {
                Box::new(
                    UreqDeviceClient::new(base.to_owned(), Some(cred.to_owned()))
                        .with_progress(Rc::clone(&progress)),
                )
            };
            finish_session_logout(
                json,
                cmd_name,
                ops::session_logout(&ctx, &connect_session_revoke, ws.as_deref(), all),
                &diag,
            )
        }
        Command::Add {
            source,
            skill,
            agent,
            global,
            yes,
        } => {
            // The `-s`/`-a` SELECTORS (single, multiple, or the `*` fan-outs) narrow a REMOTE
            // import — which members land, into which agent dirs. They pass through the SAME
            // first-trust gate and the SAME per-scope store as a bare `add owner/repo`: a selector
            // is a narrowing of what lands, never a way around whose bytes these are. `-s *`
            // expands to every skill in the repo; `-a *` to every harness DETECTED on this machine.
            let no_selectors = skill.is_empty() && agent.is_empty();
            let looks_remote = matches!(
                crate::source::classify(&source),
                crate::source::SourceSpec::Remote(_)
            );
            if !no_selectors && looks_remote {
                let git =
                    crate::plane_http::UreqGitSource::new().with_progress(Rc::clone(&progress));
                let result = ops::add_forge_selected(
                    &ctx,
                    &connect_session_transports,
                    &git,
                    &source,
                    &skill,
                    &agent,
                    global,
                    yes,
                );
                let result = result.map(|outcome| match outcome {
                    // The breadth arming sweep + the built-in ride an APPLIED import exactly as
                    // they ride any other adopt receipt (the same trigger-arming moment).
                    ops::AddManyOutcome::Applied(mut items) => {
                        if items.iter().any(|d| d.currency.is_some()) {
                            let triggers = breadth_arm(&ctx.roots, harness.as_ref(), &fs);
                            if let Err(e) = ops::ensure_builtin(&ctx) {
                                let _ = diag.note(cmd_name, &e);
                            }
                            if let Some(first) = items.first_mut() {
                                first.triggers = triggers;
                            }
                        }
                        ops::AddManyOutcome::Applied(items)
                    }
                    described => described,
                });
                return finish_add_many(json, cmd_name, result, &diag);
            }
            let single_skill = skill.into_iter().next();
            let single_agent = agent.into_iter().next();
            // The BUILT-IN's restore: `add topos` clears the durable `remove topos` opt-out and
            // re-places (adopting a marker-carrying downloaded copy snapshot-first). Intercepted
            // ahead of every resolution ladder — the name is reserved end-to-end.
            if ops::is_builtin(&source) {
                let result = ops::restore_builtin(&ctx)
                    .map(|sync| serde_json::json!({ "restored": true, "changed": sync.changed }));
                return finish(
                    json,
                    cmd_name,
                    result,
                    |_| {
                        "Restored the built-in `topos` skill on this machine \
                         (undo: topos remove topos --yes)."
                            .to_owned()
                    },
                    &diag,
                );
            }
            // REFERENCES first, read by SHAPE (the joined-key grammar): a workspace bundle
            // (`@ws/name`, the canonical `host/ws/name`), a channel (`@ws/channels/x`), a
            // workspace FEED (`@ws`, `-g`), or a git source (`owner/repo[/skill]`, a github URL).
            // Each records ONE manifest row; a first-trust git source describes before it
            // installs. The `-s`/`-a` selectors stay with the multi-import path below, which
            // places per (skill × harness).
            // A `/`-containing token that names a REAL directory here is a local adopt, whatever
            // the grammar would read it as — the bytes in front of you win over a shorthand.
            let looks_local = matches!(
                crate::source::classify(&source),
                crate::source::SourceSpec::LocalPath(ref p) if p.exists()
            );
            let reference_arm = no_selectors && !looks_local;
            // The `"off"` switch's EXACT INVERSE, at the spelling that wrote it. `remove -g
            // <name>` takes a bare name and writes the switch under the canonical reference; the
            // reference grammar does not own bare names, so a bare `add -g <name>` would fall past
            // this match into the local adopt ladder and answer about re-forking or an
            // already-tracked directory instead of the switch it was asked to lift. Resolving the
            // name to its off row here — and only when one actually stands — hands the reference
            // arm below the spelling it understands; every other bare name is untouched.
            let source = if global && reference_arm && !source.contains('/') {
                match ops::off_row_for(&ctx, &source) {
                    Ok(Some(reference)) => reference,
                    Ok(None) => source,
                    Err(e) => return finish(json, cmd_name, Err(e), render::add_tty, &diag),
                }
            } else {
                source
            };
            match crate::manifest::keys::parse_input(&source, ops::manifest_host(&ctx).as_deref()) {
                Ok(parsed)
                    if reference_arm
                        && !matches!(
                            parsed.shape,
                            crate::manifest::keys::KeyShape::LocalPath { .. }
                        ) =>
                {
                    let git =
                        crate::plane_http::UreqGitSource::new().with_progress(Rc::clone(&progress));
                    let result = ops::add_reference(
                        &ctx,
                        &connect_session_transports,
                        Some(&git),
                        &source,
                        global,
                        yes,
                    );
                    let result = result.map(|outcome| match outcome {
                        // The breadth arming sweep + the built-in ride an APPLIED add exactly as
                        // they ride an adopt receipt (the same trigger-arming moment).
                        ops::AddRefOutcome::Applied(mut data) => {
                            if data.currency.is_some() {
                                data.triggers = breadth_arm(&ctx.roots, harness.as_ref(), &fs);
                                if let Err(e) = ops::ensure_builtin(&ctx) {
                                    let _ = diag.note(cmd_name, &e);
                                }
                            }
                            ops::AddRefOutcome::Applied(data)
                        }
                        described => described,
                    });
                    return finish_add_reference(json, cmd_name, result, &diag);
                }
                // A REFERENCE-SHAPED token the grammar refused (`@x`, a malformed pin, a
                // host-led near-miss, a too-deep forge key) surfaces its typed refusal — it is
                // never retried as a filesystem path or a discovered name, whose answers would
                // name the wrong fix. ONE upgrade: a well-formed `@…` sugar on a machine with NO
                // live session refuses toward `topos login` with the concrete address — the
                // structural fix, not just the spell-it-in-full teaching.
                Err(e) if !looks_local && ops::reference_shaped(&source) => {
                    let no_sessions = crate::sessions::read_sessions(&fs, &ctx.layout)
                        .map(|s| !s.sessions.iter().any(|x| x.status != "ended"))
                        .unwrap_or(true);
                    let ws = source
                        .strip_prefix('@')
                        .and_then(|r| r.split('/').next())
                        .unwrap_or_default();
                    let name_shaped = !ws.is_empty()
                        && ws
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
                    let err = if no_sessions
                        && name_shaped
                        && e.message.contains("none is connected")
                    {
                        ClientError::SessionRequired {
                            address: ws.to_owned(),
                            message: format!(
                                "`{source}` names a workspace this machine is not logged into — \
                                 run `topos login {ws}` first"
                            ),
                        }
                    } else {
                        ClientError::InvalidArgument(e.message)
                    };
                    return finish(json, cmd_name, Err(err), render::add_tty, &diag);
                }
                _ => {}
            }
            // keep-as-yours: a bare NAME that resolves to a RETAINED withdrawn/detached copy re-forks it
            // into a new LOCAL skill, two-phase (bare describes the fork; `--yes` applies). A non-retained
            // name falls through to the ordinary adopt below.
            if let crate::source::SourceSpec::LocalName(name) = crate::source::classify(&source) {
                match ops::keep_as_yours(&ctx, &name, yes) {
                    Ok(Some(outcome)) => {
                        return finish_keep_as_yours(json, cmd_name, outcome, &diag);
                    }
                    Ok(None) => {}
                    Err(e) => return finish(json, cmd_name, Err(e), render::add_tty, &diag),
                }
            }
            // The one positional is classified by shape (crate::source): a local path adopts in place; a
            // bare name resolves against `list`'s untracked discovery; a remote `owner/repo`/URL fetches +
            // imports. No prompts — a remote import is fully non-interactive (the source's trust is the
            // user/agent's to verify).
            // Every adopt also records its MANIFEST line (the demand side: what this folder — or,
            // under `-g`, the person — uses); the receipt names the manifest first with the inverse.
            //
            // The SCOPE is resolved FIRST, before a byte moves (`ops::add_scope`): it decides both
            // the file the row lands in and the STORE the version history belongs to — a project
            // add's history lives in the checkout's own `.topos/`, which is the store that scope's
            // `update` converges. With no `topos.toml` covering this folder the add refuses there
            // and then, never adopting into a store no manifest will ask for. The row is written
            // against that SAME resolved target — nothing is re-resolved between the custody
            // decision and the write.
            let result = match crate::source::classify(&source) {
                crate::source::SourceSpec::LocalPath(p) => {
                    ops::add_scope(&ctx, global).and_then(|scope| {
                        let sctx = ops::ctx_with_layout(&ctx, &scope.layout);
                        let mut d = ops::adopt_path(&sctx, &scope.target, &p)?;
                        ops::note_added_path_in(&ctx, &mut d, &scope.target, &p)?;
                        Ok(d)
                    })
                }
                // A bare NAME resolves against BOTH namespaces — the untracked local inventory and
                // the connected workspaces' catalogs — so a name only the team publishes is not a
                // dead end, and a name both carry says so on the receipt.
                crate::source::SourceSpec::LocalName(name) => match list_discovery() {
                    Some(roots) => {
                        match ops::plan_bare_add(
                            &ctx,
                            &connect_session_transports,
                            &roots,
                            &name,
                            no_selectors,
                            global,
                            workspace.as_deref(),
                        ) {
                            // Nothing local carries the name and exactly ONE workspace publishes
                            // it: record the canonical reference through the ordinary reference
                            // arm — same row, same delivery, same receipt shape — and teach the
                            // grammar in a note, so the next add can be spelled directly.
                            Ok(ops::BareAddPlan::Subscribe {
                                reference,
                                workspace,
                            }) => {
                                let result = ops::add_reference(
                                    &ctx,
                                    &connect_session_transports,
                                    None,
                                    &reference,
                                    global,
                                    yes,
                                );
                                let result = result.map(|outcome| match outcome {
                                    ops::AddRefOutcome::Applied(mut data) => {
                                        ops::push_note(
                                            &mut data,
                                            format!(
                                                "resolved '{name}' to {reference} — no untracked \
                                                 skill of that name is on this machine, and \
                                                 {workspace} publishes it"
                                            ),
                                        );
                                        // The arming sweep + the built-in ride an APPLIED add,
                                        // exactly as on the spelled-out reference arm above.
                                        if data.currency.is_some() {
                                            data.triggers =
                                                breadth_arm(&ctx.roots, harness.as_ref(), &fs);
                                            if let Err(e) = ops::ensure_builtin(&ctx) {
                                                let _ = diag.note(cmd_name, &e);
                                            }
                                        }
                                        ops::AddRefOutcome::Applied(data)
                                    }
                                    described => described,
                                });
                                return finish_add_reference(json, cmd_name, result, &diag);
                            }
                            // Adopt the resolved dir UNDER its resolved name — so
                            // `list`/`add`/`publish`/`diff` agree on the name even for a harness
                            // the active adapter does not recognize.
                            Ok(ops::BareAddPlan::Adopt {
                                path,
                                name,
                                published,
                            }) => ops::add_scope(&ctx, global).and_then(|scope| {
                                let sctx = ops::ctx_with_layout(&ctx, &scope.layout);
                                let mut d = ops::add_with_name(&sctx, &path, Some(&name), true)?;
                                ops::note_added_path_in(&ctx, &mut d, &scope.target, &path)?;
                                // The bytes that just landed are what the disclosure judges
                                // against the team's current version.
                                d.published_match =
                                    published.map(|p| p.suggestion(&d.bundle_digest));
                                Ok(d)
                            }),
                            Err(e) => Err(e),
                        }
                    }
                    None => Err(ClientError::InvalidArgument(
                        "cannot resolve a skill name without $HOME set — adopt a directory by \
                         path (`topos add ./<dir>`)"
                            .into(),
                    )),
                },
                crate::source::SourceSpec::Remote(spec) => match list_discovery() {
                    Some(roots) => {
                        let git = crate::plane_http::UreqGitSource::new()
                            .with_progress(Rc::clone(&progress));
                        // The `-a` selection is a standing fact, not a one-run choice: it rides
                        // the row so the next update keeps the copy where it was asked for.
                        let chosen: Vec<String> = single_agent.iter().cloned().collect();
                        ops::add_scope(&ctx, global).and_then(|scope| {
                            let sctx = ops::ctx_with_layout(&ctx, &scope.layout);
                            let mut d = ops::add_remote(
                                &sctx,
                                &git,
                                &spec,
                                &roots,
                                &ops::AddRemoteOpts {
                                    skill: single_skill,
                                    harness: single_agent.clone(),
                                    global,
                                },
                            )?;
                            ops::note_added_remote(&ctx, &mut d, &scope.target, &chosen)?;
                            // The DEDUP courtesy: when a connected workspace already governs this
                            // source (its catalog's upstream provenance matches), the receipt
                            // SUGGESTS the governed reference — visible, never blocking (the
                            // import above landed exactly as asked). Path-exactness is judged
                            // against the subdir the import actually SELECTED (the recorded
                            // origin), not the spec's spelling.
                            let imported_subdir = d.origin.as_ref().and_then(|o| o.subdir.clone());
                            d.governed_copy = ops::governed_copy_suggestion(
                                &ctx,
                                &connect_session_transports,
                                &spec,
                                imported_subdir.as_deref(),
                            );
                            Ok(d)
                        })
                    }
                    None => Err(ClientError::InvalidArgument(
                        "cannot import a remote skill without $HOME set (needed to resolve the \
                         harness skills dir)"
                            .into(),
                    )),
                },
                crate::source::SourceSpec::Unsupported(msg) => {
                    Err(ClientError::InvalidArgument(msg))
                }
            };
            // The breadth arming sweep rides the adopt receipt: when the active adapter armed its
            // own trigger (`currency`), every OTHER detected agent's trigger is armed here at the
            // composition root — the one layer holding the real ports + `$HOME`. The built-in
            // `topos` skill lands at the same moment (best-effort — an install must not fail over
            // it), so a freshly wired harness knows how to drive the tool.
            let result = result.map(|mut data| {
                if data.currency.is_some() {
                    data.triggers = breadth_arm(&ctx.roots, harness.as_ref(), &fs);
                    if let Err(e) = ops::ensure_builtin(&ctx) {
                        let _ = diag.note(cmd_name, &e);
                    }
                }
                data
            });
            finish(json, cmd_name, result, render::add_tty, &diag)
        }
        Command::Invite {
            email,
            skill,
            channel,
            yes,
        } => {
            let connectors = ops::InviteConnectors {
                governance: &connect_governance,
                directory: &connect_directory,
                session: &connect_session_transports,
            };
            let result = ops::invite(
                &ctx,
                &connectors,
                email,
                skill,
                channel,
                workspace.as_deref(),
                yes,
            );
            finish_invite(json, cmd_name, result, &diag)
        }
        Command::List {
            name,
            global,
            all,
            untracked,
            agent,
            remote,
            footprint,
            channel,
            skill,
            limit,
            offset,
        } => {
            let page = ops::RowPage::resolve(limit, offset, json, ops::DEFAULT_JSON_LIST_LIMIT);
            // The complete NEXT_PAGE argv base — this same invocation, re-spelled with EVERY
            // selector (incl. the global `--workspace`, which narrows the `--remote` catalog); the
            // page flags are appended by the finisher once the effective page is known.
            let page_argv = list_page_argv(
                name.as_deref(),
                global,
                all,
                untracked,
                agent.as_deref(),
                remote,
                footprint,
                &channel,
                &skill,
                workspace.as_deref(),
            );
            let req = ops::ListRequest {
                view: match (global, all) {
                    (true, _) => ops::ScopeView::Machine,
                    (_, true) => ops::ScopeView::All,
                    _ => ops::ScopeView::Here,
                },
                untracked,
                name,
                agent,
                remote,
                footprint,
                channels: channel,
                skills: skill,
                workspace: workspace.clone(),
            };
            // Under `--remote` (the one networked arm) the op reads each LIVE session's channel
            // index + catalog through its own credentialed directory lane (the typed "run login
            // first" refusal lives in the op). Everything else stays offline.
            let connect_list_directory =
                |s: &crate::sessions::Session| -> Box<dyn DirectorySource> {
                    Box::new(
                        UreqDeviceClient::new(s.base_url.clone(), Some(s.credential.clone()))
                            .with_progress(Rc::clone(&progress)),
                    )
                };
            finish_list(
                json,
                cmd_name,
                ops::list_with(
                    &ctx,
                    &req,
                    list_discovery(),
                    Some(&connect_list_directory),
                    page,
                ),
                page,
                page_argv,
                &diag,
            )
        }
        Command::Diff {
            skill,
            r#ref,
            max_bytes,
        } => {
            let budget = ops::DiffBudget::resolve(max_bytes, json);
            // The FETCH_FULL_DIFF argv — this same diff, uncapped.
            let mut full_argv = vec!["topos".to_owned(), "diff".to_owned(), skill.clone()];
            if let Some(r) = &r#ref {
                full_argv.push(r.clone());
            }
            full_argv.extend([
                "--max-bytes".to_owned(),
                "0".to_owned(),
                "--json".to_owned(),
            ]);
            finish_diff(
                json,
                cmd_name,
                ops::diff(&ctx, &skill, r#ref.as_deref(), budget),
                full_argv,
                &diag,
            )
        }
        Command::Publish {
            target,
            to,
            propose,
            message,
            yes,
        } => {
            // Discovery roots for the auto-add pre-step (a `publish` of an untracked local source adopts it
            // first) — the SAME roots `add`/`list` use; `None` degrades name/dir resolution the same way.
            let roots = list_discovery();
            // A bare ENROLLED publish DESCRIBES what shipping would do (nothing lands on the plane); `--yes`
            // applies. An un-enrolled publish refuses typed inside the op (enroll with `follow` first).
            let publish_sessions = crate::sessions::read_sessions(&fs, &ctx.layout)
                .map(|s| !s.sessions.is_empty())
                .unwrap_or(false);
            if !yes && publish_sessions {
                let connectors = ops::PublishDescribeConnectors {
                    directory: &connect_directory,
                    delivery: &connect_delivery,
                };
                let described = ops::publish_describe(
                    &ctx,
                    &connectors,
                    Some(&connect_session_transports),
                    roots.as_ref(),
                    &target,
                    propose,
                    to.as_deref(),
                    workspace.as_deref(),
                );
                // The paste-ready apply: this same publish plus `--yes` (preserving `--propose` / `--to`
                // / `-m` — dropping the message would change the version's commit identity from what the
                // describe computed, so an agent that runs this next action ships the wrong version).
                let mut yes_argv = vec!["topos".to_owned(), "publish".to_owned(), target.clone()];
                if propose {
                    yes_argv.push("--propose".to_owned());
                }
                if let Some(ch) = &to {
                    yes_argv.push("--to".to_owned());
                    yes_argv.push(ch.clone());
                }
                if let Some(m) = &message {
                    yes_argv.push("-m".to_owned());
                    yes_argv.push(m.clone());
                }
                yes_argv.push("--yes".to_owned());
                return finish_publish_describe(json, cmd_name, described, yes_argv, &diag);
            }
            let result = ops::publish(
                &ctx,
                &connect_contribute,
                Some(&connect_directory),
                Some(&connect_session_transports),
                roots.as_ref(),
                &target,
                propose,
                to.as_deref(),
                workspace.as_deref(),
                message.as_deref(),
            );
            finish_publish(json, cmd_name, result, &diag)
        }
        Command::Review {
            target,
            approve,
            reject,
            withdraw,
            message,
            max_bytes,
            yes,
        } => {
            let _ = yes;
            let budget = ops::DiffBudget::resolve(max_bytes, json);
            // clap's `verdict` ArgGroup (now OPTIONAL) guarantees AT MOST one flag is set.
            let verdict = if approve {
                Some(ops::ReviewVerdict::Approve)
            } else if reject {
                Some(ops::ReviewVerdict::Reject { reason: message })
            } else if withdraw {
                Some(ops::ReviewVerdict::Withdraw)
            } else {
                None
            };
            let connectors = ops::ReviewConnectors {
                directory: &connect_directory,
                contribute: &connect_contribute,
                session: &connect_session_transports,
            };
            let result = ops::review_dispatch(
                &ctx,
                &connectors,
                target.as_deref(),
                verdict,
                workspace.as_deref(),
                budget,
            );
            finish_review(json, cmd_name, result, workspace.as_deref(), &diag)
        }
        Command::Revert { skill, to, yes } => finish_revert(
            json,
            cmd_name,
            // Two-phase: bare DESCRIBES the forward move (nothing written); `--yes` applies it and also
            // acknowledges a byte-level no-op.
            ops::revert(
                &ctx,
                &connect_contribute,
                &skill,
                &to,
                yes,
                workspace.as_deref(),
            ),
            &diag,
        ),
        Command::Log {
            skill,
            limit,
            offset,
        } => {
            let connectors = ops::LogConnectors {
                directory: &connect_directory,
                session: &connect_session_transports,
            };
            let page = ops::RowPage::resolve(limit, offset, json, ops::DEFAULT_JSON_LOG_LIMIT);
            finish_log(
                json,
                cmd_name,
                ops::log(&ctx, &connectors, &skill, page),
                &skill,
                page,
                &diag,
            )
        }
        Command::Update {
            targets,
            global,
            reset,
            yes,
            onto_current,
            quiet,
            ttl,
            hook,
            rebuild,
        } => {
            // The scope flag decides which STORE every targeted arm below resolves in — and, since
            // the store it resolves in is the store it writes, which copy it acts on. It is
            // computed here, ahead of the special modes: `-g` inside a project must mean the
            // machine's copy for a reset and a go-back exactly as it does for the reconcile.
            let store_scope = if global {
                ops::StoreScope::Machine
            } else {
                ops::StoreScope::Here
            };
            // `--reset` is its own two-phase discard verb (loss-led describe / `--yes` apply); it
            // does not flow through the reconcile and is never a `--quiet` hook shape.
            if reset {
                return finish_reset(
                    json,
                    cmd_name,
                    ops::reset(&ctx, &targets, yes, store_scope),
                    &diag,
                );
            }
            // The BARE sweep is the hook shape (the auto-update triggers all run `update --quiet`).
            // Hooks fire on every session-start-shaped event, so the quiet path passes a
            // self-throttle gate BEFORE any engine or network work: single-flight + TTL
            // (`--ttl`/`TOPOS_UPDATE_TTL`/default 300 s, `0` disables). An explicit non-quiet
            // sweep always runs but takes the same lock and refreshes the stamp.
            let bare_sweep = targets.is_empty();
            let now_ms = i64::try_from(clock.now_unix_millis()).unwrap_or(i64::MAX);
            let mut _sweep_guard = None;
            if bare_sweep {
                if quiet {
                    match ops::quiet_gate(&fs, &ctx.layout, now_ms, ops::resolve_ttl_ms(ttl)) {
                        Ok(ops::QuietGate::Run(guard)) => _sweep_guard = Some(guard),
                        // Skipped (fresh, or another sweep in flight): byte-silent success.
                        Ok(ops::QuietGate::Skip(reason)) => {
                            let _ = reason;
                            return ExitCode::SUCCESS;
                        }
                        Err(e) => return emit_err(false, cmd_name, &e, &diag),
                    }
                } else {
                    match ops::sweep_lock(&fs, &ctx.layout) {
                        Ok(guard) => _sweep_guard = Some(guard),
                        Err(e) => return emit_err(json, cmd_name, &e, &diag),
                    }
                }
            }
            // The bare sweep also re-syncs the BUILT-IN `topos` skill (force-synced to this
            // binary; the durable opt-out honored inside). Best-effort: a built-in hiccup must
            // never block the team sweep; its byte changes count as a changed sweep below.
            let builtin_changed = if bare_sweep {
                match ops::ensure_builtin(&ctx) {
                    Ok(r) => r.changed,
                    Err(e) => {
                        let _ = diag.note(cmd_name, &e);
                        false
                    }
                }
            } else {
                false
            };
            // The go-back (`<skill>@<hash>`) and the `--onto-current` escape act on LOCAL bytes
            // through the classic per-skill engine; everything else is the MANIFEST reconcile —
            // resolve the manifest layers covering cwd + the logged-in profiles and converge.
            let goback_target = targets
                .len()
                .eq(&1)
                .then(|| targets[0].clone())
                .filter(|t| t.split_once('@').is_some_and(|(_, r)| !r.is_empty()));
            let result = if onto_current || goback_target.is_some() {
                if targets.len() > 1 {
                    Err(ClientError::InvalidArgument(
                        "the go-back and --onto-current take a single <skill> target".into(),
                    ))
                } else if targets
                    .first()
                    .is_some_and(|t| ops::is_builtin(t.split('@').next().unwrap_or(t)))
                {
                    Err(ClientError::InvalidArgument(
                        "`topos` is the built-in skill — the bare `topos update` re-syncs it to \
                         this binary; `topos self-update` updates the binary itself"
                            .into(),
                    ))
                } else {
                    pull_with_name_fallback(
                        &ctx,
                        targets.into_iter().next(),
                        onto_current,
                        store_scope,
                        None,
                        &ops::ReconcileOpts::default(),
                    )
                }
            } else if targets.first().is_some_and(|t| ops::is_builtin(t.as_str())) {
                Err(ClientError::InvalidArgument(
                    "`topos` is the built-in skill — the bare `topos update` re-syncs it to \
                     this binary; `topos self-update` updates the binary itself"
                        .into(),
                ))
            } else {
                // The background sweep NEVER contacts a forge: `--quiet` passes no git source
                // (git rows move only on an explicit update), and the reconcile's forge arms
                // degrade honestly without one.
                let git = (!quiet).then(|| {
                    crate::plane_http::UreqGitSource::new().with_progress(Rc::clone(&progress))
                });
                // The scope rule: a hand-run update converges where the invocation stands (or the
                // machine, with `-g`), and the hook sweep covers BOTH — silent delivery is the
                // promise, so `--quiet` wins over `-g` rather than narrowing what auto-update
                // reaches.
                let scope = if quiet {
                    ops::UpdateScope::Both
                } else if global {
                    ops::UpdateScope::Machine
                } else {
                    ops::UpdateScope::Here
                };
                ops::manifest_update(
                    &ctx,
                    &connect_session_transports,
                    git.as_ref()
                        .map(|g| g as &dyn crate::git_source::GitTarballSource),
                    &ops::ManifestUpdateOpts {
                        targets,
                        ack_notices: !quiet,
                        rebuild,
                        scope,
                    },
                )
            };
            // A COMPLETED bare sweep stamps the TTL clock (best-effort) — success, or the quiet
            // path's soft failure (an unreachable plane must not be re-dialed on every session
            // event). A hard local failure leaves the old stamp so the next session retries.
            if bare_sweep
                && (result.is_ok()
                    || (quiet && result.as_ref().is_err_and(ops::quiet_soft_failure)))
            {
                ops::stamp_sweep(
                    &fs,
                    &ctx.layout,
                    i64::try_from(clock.now_unix_millis()).unwrap_or(now_ms),
                );
            }
            if quiet {
                // NEAR-byte-silent stdout (a session-start hook's stdout reaches the session).
                // ONE renderer decides what a hook hears, always, in the CALLING trigger's dialect
                // (`--hook <harness>`): at most ONE SessionStart hook-output document, carrying
                // `reloadSkills` only when bytes actually MOVED, and the two facts a person must
                // not miss — an ended-session freeze, and unreachable-AND-stale — on
                // `additionalContext` whenever they exist. Never raw text: that field is what
                // INJECTS into the session, and an agent validating hook output against a strict
                // schema may discard anything else. Nothing to say → nothing printed. An
                // auth/transport failure warns and exits 0; a genuinely local failure still
                // surfaces on stderr with a non-zero exit.
                let dialect = ops::HookDialect::from_slug(hook.as_deref());
                let now = i64::try_from(clock.now_unix_millis()).unwrap_or(i64::MAX);
                match result {
                    Ok(out) => {
                        let lines = ops::quiet_hook_lines(&fs, &ctx.layout, now, &out);
                        let changed = ops::sweep_changed_bytes(&out.data) || builtin_changed;
                        if let Some(doc) = ops::hook_output_json(dialect, changed, &lines) {
                            println!("{doc}");
                        }
                        ExitCode::SUCCESS
                    }
                    Err(e) if ops::quiet_soft_failure(&e) => {
                        // A SOFT failure (auth/transport) is a person-facing line like any other,
                        // so it rides the SAME renderer — never raw text a strict-schema agent may
                        // discard instead of inject. Nothing landed, so `changed` is false: no
                        // dialect may claim a reload here. The exit stays 0 — only the message's
                        // shape changes, never the status.
                        let _ = diag.note(cmd_name, &e);
                        let line = format!("topos: update skipped — {}", render::safe_message(&e));
                        if let Some(doc) =
                            ops::hook_output_json(dialect, false, std::slice::from_ref(&line))
                        {
                            println!("{doc}");
                        }
                        ExitCode::SUCCESS
                    }
                    Err(e) => emit_err(false, cmd_name, &e, &diag),
                }
            } else {
                let connected = crate::sessions::read_sessions(&fs, &ctx.layout)
                    .map(|s| !s.sessions.is_empty())
                    .unwrap_or(false);
                finish_pull(json, cmd_name, result, connected, &diag)
            }
        }
        Command::Remove {
            skill,
            global,
            via,
            yes,
        } => {
            // A REFERENCE-SHAPED token the grammar refuses surfaces its typed refusal here too —
            // never retried through the tracked/untracked ladder (whose answers name the wrong fix).
            for t in &skill {
                if ops::reference_shaped(t)
                    && let Err(e) =
                        crate::manifest::keys::parse_input(t, ops::manifest_host(&ctx).as_deref())
                {
                    return emit_err(
                        json,
                        cmd_name,
                        &ClientError::InvalidArgument(e.message),
                        &diag,
                    );
                }
            }
            // `remove -g` edits THIS MACHINE's own file: drop a row, stop adopting a workspace's
            // feed, or switch one feed-delivered bundle off here. It never touches the server —
            // what a workspace assigns you is managed on the web.
            if global {
                let result = ops::remove_global(
                    &ctx,
                    &connect_session_transports,
                    &skill,
                    via.as_deref(),
                    yes,
                );
                return finish_remove(json, cmd_name, result, &diag);
            }
            // The PROJECT manifest arm first: a target naming a row in this folder's file (or a
            // member a channel/repo line brings) edits that file. The classic tracked/untracked
            // removal owns everything no manifest mentions.
            match ops::remove_project(
                &ctx,
                &connect_session_transports,
                &skill,
                via.as_deref(),
                yes,
            ) {
                Ok(Some(outcome)) => {
                    return finish_remove(json, cmd_name, Ok(outcome), &diag);
                }
                Ok(None) => {}
                Err(e) => return emit_err(json, cmd_name, &e, &diag),
            }
            // `--via` selects a manifest set line; a token no manifest arm claimed has no line
            // for it to select — refuse rather than routing the flag into the classic removal,
            // which knows nothing of manifest lines.
            if let Some(via) = &via {
                return emit_err(
                    json,
                    cmd_name,
                    &ClientError::InvalidArgument(format!(
                        "--via {via} selects a channel/repo line in a manifest, and no manifest \
                         line here carries the named target — `topos list` shows what each \
                         scope delivers"
                    )),
                    &diag,
                );
            }
            let connectors = ops::RemoveConnectors {
                directory: &connect_directory,
                session: &connect_session_transports,
            };
            let roots = list_discovery();
            let result = ops::remove(&ctx, &connectors, &skill, &[], roots.as_ref(), yes);
            finish_remove(json, cmd_name, result, &diag)
        }
        // `channel add|remove <channel> <skill>...` dispatches to the two-phase op; a bare `channel` (or an
        // unrecognized subword / `create`) teaches usage without touching the network.
        Command::Protect { target, level, yes } => {
            let connectors = ops::ProtectConnectors {
                directory: &connect_directory,
                session: &connect_session_transports,
            };
            let result = ops::protect(
                &ctx,
                &connectors,
                &target,
                level.as_deref(),
                workspace.as_deref(),
                yes,
            );
            finish_protect(json, cmd_name, result, &diag)
        }
        Command::SelfUpdate { check, version } => {
            // A MAINTENANCE command: it replaces the binary itself. It mints no device identity (kept out
            // of the device-id match above) and never touches a skill / the plane / the account.
            let releases = connect_releases(Rc::clone(&progress));
            let base_url = std::env::var("TOPOS_INSTALL_BASE_URL").ok();
            let result = std::env::current_exe()
                .map_err(ClientError::from)
                .and_then(|exe| {
                    ops::self_update(
                        &ctx,
                        releases.as_ref(),
                        &exe,
                        ops::SelfUpdateOpts {
                            check,
                            version,
                            base_url,
                        },
                    )
                });
            finish(json, cmd_name, result, render::self_update_tty, &diag)
        }
        Command::Auth { cmd } => {
            let connectors = ops::AuthConnectors {
                directory: &connect_directory,
                session: &connect_session_transports,
            };
            match cmd {
                AuthCmd::Status => {
                    finish_auth_status(json, cmd_name, ops::status(&ctx, &connectors), &diag)
                }
            }
        }
        // Dispatched BEFORE state recovery/enrollment loading above — corrupt local state must never
        // block the command that removes it.
        Command::Uninstall { .. } => unreachable!("uninstall dispatches before state recovery"),
        // `topos upgrade` is ambiguous — the disambiguation refusal points skills → `topos update`, the CLI
        // → `topos self-update`, so the retired spelling never silently does the wrong thing.
        Command::Upgrade => emit_err(json, cmd_name, &ClientError::UpgradeAmbiguous, &diag),
    }
}

/// Load the device's ONE Bearer credential from `credentials.json`. Best-effort (absent / corrupt ⇒
/// `None`): a corrupt doc already failed the startup [`load_enrollment`] closed, and a missing
/// credential surfaces downstream as a clear "not enrolled" at request time.
fn load_device_credential(fs: &dyn FsOps, layout: &Layout) -> Option<String> {
    let _ = (fs, layout);
    None
}

/// The real release source for `topos upgrade` — the `ureq` GitHub transport. No base URL / creds: the
/// updater's default download base is compiled in (overridable via `TOPOS_INSTALL_BASE_URL`). It
/// carries the activity sink because the asset download is the one fetch here worth watching.
fn connect_releases(
    progress: Rc<dyn crate::progress::ProgressSink>,
) -> Box<dyn crate::release::ReleaseSource> {
    Box::new(crate::plane_http::UreqReleases::new().with_progress(progress))
}

/// The hosted WEB origin every token-less door defaults to (`follow <bare-workspace>`, `auth login`,
/// the un-enrolled `publish` standup) — the card fetch re-roots it onto the declared API base, so
/// pasting the human-facing origin works. The ONE compiled-in dial point.
pub(crate) const DEFAULT_WEB_ORIGIN: &str = "https://topos.sh";

/// Resolve the default web origin: a non-empty `TOPOS_PLANE_URL` override wins (a self-host plane
/// serves the card at its own base), else the hosted web origin. Pure (the env read happens at the
/// call site) so the override precedence is unit-testable.
pub(crate) fn resolve_web_origin(env_override: Option<String>) -> String {
    match env_override {
        Some(v) if !v.trim().is_empty() => v.trim().trim_end_matches('/').to_owned(),
        _ => DEFAULT_WEB_ORIGIN.to_owned(),
    }
}

fn finish<T: Serialize>(
    json: bool,
    command: &str,
    result: Result<T, ClientError>,
    tty: impl Fn(&T) -> String,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(data) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `auth status`'s finisher — like [`finish`], plus the signed-out fix as structural next actions
/// (the prose half rides `auth_status_tty`): a never-enrolled install gets the join template, an
/// enrolled-but-signed-out one the concrete `auth login`.
fn finish_auth_status(
    json: bool,
    command: &str,
    result: Result<ops::AuthStatusData, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(data) => {
            if json {
                let next_actions = render::auth_status_next_actions(&data);
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = next_actions;
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::auth_status_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `status`'s finisher — the one orientation envelope. An UNENROLLED snapshot carries the join
/// next action (the argv is a template — its `<workspace-address>` placeholder is the caller's to
/// fill); the TTY renders the full snapshot, or the short welcome on a bare `topos` from a fresh
/// machine. Deliberately NO diagnostics channel: even the error path writes nothing (`status` is
/// read-only end to end — the append-only log is a write).
fn finish_status(
    json: bool,
    command: &str,
    result: Result<topos_types::results::StatusData, ClientError>,
    bare: bool,
    argv: &[String],
) -> ExitCode {
    match result {
        Ok(data) => {
            if json {
                let next_actions = if !data.sessions.is_empty() {
                    Vec::new()
                } else {
                    vec![crate::actions::next_action(
                        topos_types::ActionCode::from("LOGIN_WORKSPACE".to_owned()),
                        vec![
                            "topos".to_owned(),
                            "login".to_owned(),
                            "<workspace-address>".to_owned(),
                            "--json".to_owned(),
                        ],
                    )]
                };
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = next_actions;
                println!("{}", render::to_json(&envelope));
            } else if bare && data.sessions.is_empty() {
                println!("{}", render::welcome_tty(&data));
            } else {
                println!("{}", render::status_tty(&data));
            }
            ExitCode::SUCCESS
        }
        // Read-only even on failure: the envelope/TTY error renders, but nothing is appended to
        // the diagnostics log and no `details:` pointer is printed (there is no written detail).
        Err(e) => {
            if json {
                println!(
                    "{}",
                    render::to_json(&render::err_envelope(command, argv, &e))
                );
            } else {
                eprintln!("{}", render::err_tty(&e));
                if let Some(hint) = render::err_hint_tty(command, argv, &e) {
                    eprintln!("{hint}");
                }
            }
            ExitCode::FAILURE
        }
    }
}

/// `pull`'s finisher — like [`finish`], but a bare sweep's isolated per-skill failures ride the
/// envelope's `warnings` (one stable-shape line per failed skill) AND the TTY summary, so a wedged
/// skill is visible on both surfaces. Isolation semantics hold: `ok` stays `true`, exit stays 0 (each
/// failure also reached stderr inside the sweep).
fn finish_pull(
    json: bool,
    command: &str,
    result: Result<ops::PullOutcome, ClientError>,
    enrolled: bool,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(out) => {
            // The UNENROLLED empty sweep is a dead-end without a pointer: nothing is followed
            // because nothing CAN be — state the join fix in prose and mirror it structurally
            // (the argv template's `needs` names the workspace address).
            let unenrolled_dead_end = !enrolled && out.data.skills.is_empty();
            if json {
                // Each WITHDRAWN skill carries a paste-ready `keep-as-yours` next action.
                let mut next_actions = render::withdrawn_next_actions(&out.data);
                if unenrolled_dead_end {
                    next_actions.push(crate::actions::next_action(
                        topos_types::ActionCode::from("LOGIN_WORKSPACE".to_owned()),
                        vec![
                            "topos".to_owned(),
                            "login".to_owned(),
                            "<workspace-address>".to_owned(),
                            "--json".to_owned(),
                        ],
                    ));
                }
                let value = serde_json::to_value(&out.data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                // ONE stable machine channel: failures first, then the disclosures. The split is
                // the receipt's (a disclosure must not be counted as a failure), not the wire's.
                envelope.warnings = out.warnings;
                envelope.warnings.extend(out.disclosures.iter().cloned());
                envelope.next_actions = next_actions;
                println!("{}", render::to_json(&envelope));
            } else {
                let mut text = render::pull_tty(&out.data, &out.warnings, &out.disclosures);
                if !out.access_gone.is_empty() {
                    text.push_str(&format!(
                        "\nnote: the session{} for {} ended — every skill it delivered stays in \
                         place, frozen; `topos login <workspace-address>` reconnects.",
                        if out.access_gone.len() == 1 { "" } else { "s" },
                        out.access_gone.join(", ")
                    ));
                }
                if unenrolled_dead_end {
                    text.push_str(
                        "\nNot logged in — join your team with `topos login \
                         <workspace-address>` (ask a teammate for the address).",
                    );
                }
                println!("{text}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `list`'s finisher — the `--json` envelope carries exactly the schema-pinned `ListData` plus any
/// `--remote` per-workspace catalog-read warnings (mirroring `pull`); the TTY additionally renders the
/// enrollment header + per-row follow annotations the outcome carries alongside (TTY-only disclosure —
/// `ListData`'s pinned shape has no enrollment fields). A row-capped list (any bucket marker) adds
/// the NEXT_PAGE next action carrying this same invocation's COMPLETE argv at the next offset.
fn finish_list(
    json: bool,
    command: &str,
    result: Result<ops::ListOutcome, ClientError>,
    page: ops::RowPage,
    page_argv: Vec<String>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(out) => {
            if json {
                let next_actions = next_page_action(
                    &page,
                    out.data
                        .truncated
                        .iter()
                        .any(|b| (page.offset as u64).saturating_add(b.shown) < b.total),
                    page_argv,
                );
                // The stable-shape truncation warnings — a belt for a consumer that ignores the new
                // typed markers: a capped enumeration is never mistakable for a complete one.
                let mut warnings = out.warnings.clone();
                for b in &out.data.truncated {
                    warnings.push(format!(
                        "LIST_TRUNCATED {}: {} of {} rows shown — the NEXT_PAGE next action pages on",
                        b.bucket, b.shown, b.total
                    ));
                }
                let value = serde_json::to_value(&out.data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.warnings = warnings;
                envelope.next_actions = next_actions;
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::list_tty(&out));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// The complete `topos list` argv this invocation re-spells for its NEXT_PAGE continuation — every
/// selector preserved: the deep-dive name, the scope flags, the view flags, the repeatable
/// `--channel`/`--skill` selectors, AND the global `--workspace` (already canonicalized to an id;
/// the flag accepts it) — so the next page enumerates exactly the same view, never a widened one.
#[allow(clippy::too_many_arguments)]
fn list_page_argv(
    name: Option<&str>,
    global: bool,
    all: bool,
    untracked: bool,
    agent: Option<&str>,
    remote: bool,
    footprint: bool,
    channels: &[String],
    skills: &[String],
    workspace: Option<&str>,
) -> Vec<String> {
    let mut argv = vec!["topos".to_owned(), "list".to_owned()];
    if let Some(n) = name {
        argv.push(n.to_owned());
    }
    for (flag, on) in [
        ("-g", global),
        ("--all", all),
        ("--untracked", untracked),
        ("--remote", remote),
        ("--footprint", footprint),
    ] {
        if on {
            argv.push(flag.to_owned());
        }
    }
    if let Some(a) = agent {
        argv.extend(["--agent".to_owned(), a.to_owned()]);
    }
    for c in channels {
        argv.extend(["--channel".to_owned(), c.clone()]);
    }
    for s in skills {
        argv.extend(["--skill".to_owned(), s.clone()]);
    }
    if let Some(w) = workspace {
        argv.extend(["--workspace".to_owned(), w.to_owned()]);
    }
    argv
}

/// The NEXT_PAGE next action for a row-capped enumeration: the caller's COMPLETE argv re-spelled at
/// the next offset (same page size). Empty when nothing lies past this page. A truncated page always
/// has a finite limit (an unlimited page runs to the end), so the argv always carries `--limit`.
fn next_page_action(
    page: &ops::RowPage,
    more_after: bool,
    mut argv: Vec<String>,
) -> Vec<topos_types::NextAction> {
    if !more_after {
        return Vec::new();
    }
    let Some(limit) = page.limit else {
        return Vec::new();
    };
    argv.extend([
        "--limit".to_owned(),
        limit.to_string(),
        "--offset".to_owned(),
        page.offset.saturating_add(limit).to_string(),
        "--json".to_owned(),
    ]);
    vec![crate::actions::next_action(
        topos_types::ActionCode::NextPage,
        argv,
    )]
}

/// `diff`'s finisher — a byte-capped body adds the FETCH_FULL_DIFF next action (this same diff,
/// `--max-bytes 0`).
fn finish_diff(
    json: bool,
    command: &str,
    result: Result<topos_types::results::DiffData, ClientError>,
    full_argv: Vec<String>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(data) => {
            if json {
                let mut envelope_warnings = Vec::new();
                let next_actions = if data.truncated {
                    envelope_warnings.push(
                        "DIFF_TRUNCATED: the emitted diff is partial (byte cap) — the \
                         FETCH_FULL_DIFF next action re-runs it uncapped"
                            .to_owned(),
                    );
                    vec![crate::actions::next_action(
                        topos_types::ActionCode::FetchFullDiff,
                        full_argv,
                    )]
                } else {
                    Vec::new()
                };
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.warnings = envelope_warnings;
                envelope.next_actions = next_actions;
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::diff_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `log`'s finisher — a row-capped event list adds the NEXT_PAGE next action (this same log at the
/// next offset).
fn finish_log(
    json: bool,
    command: &str,
    result: Result<topos_types::results::LogData, ClientError>,
    skill: &str,
    page: ops::RowPage,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(data) => {
            if json {
                let next_actions = next_page_action(
                    &page,
                    data.truncated,
                    vec!["topos".to_owned(), "log".to_owned(), skill.to_owned()],
                );
                let mut warnings = Vec::new();
                if data.truncated
                    && let Some(total) = data.total
                {
                    warnings.push(format!(
                        "LOG_TRUNCATED: {} of {total} events shown — the NEXT_PAGE next action \
                         pages on",
                        data.events.len()
                    ));
                }
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.warnings = warnings;
                envelope.next_actions = next_actions;
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::log_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// The `keep-as-yours` finisher — the local-fork DESCRIBE (with its `--yes` argv) or the applied fork
/// (rendered as an ordinary `add` receipt over the new local skill).
fn finish_keep_as_yours(
    json: bool,
    command: &str,
    outcome: ops::KeepAsYoursOutcome,
    diag: &Diag<'_>,
) -> ExitCode {
    let _ = diag;
    match outcome {
        ops::KeepAsYoursOutcome::Described { data, yes_argv } => {
            if json {
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::keep_as_yours_describe_tty(&data, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        ops::KeepAsYoursOutcome::Forked(data) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::add_tty(&data));
            }
            ExitCode::SUCCESS
        }
    }
}

/// `add <reference>`'s finisher — the applied row receipt, or the FIRST-TRUST describe of a git
/// source this machine has never used (with its `--yes` argv).
fn finish_add_reference(
    json: bool,
    command: &str,
    result: Result<ops::AddRefOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::AddRefOutcome::Applied(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::undo_next_actions(&data.undo);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::add_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::AddRefOutcome::Described { data, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::add_describe_tty(&data, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// The multi-`add` finisher — one `add` receipt per imported (skill × harness) combination.
fn finish_add_many(
    json: bool,
    command: &str,
    result: Result<ops::AddManyOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        // A first-trust git source describes before it installs — the same two-phase receipt the
        // bare reference arm prints, selectors and all.
        Ok(ops::AddManyOutcome::Described { data, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::add_describe_tty(&data, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::AddManyOutcome::Applied(items)) => {
            if json {
                let value = serde_json::json!({ "added": items });
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                let body = items
                    .iter()
                    .map(render::add_tty)
                    .collect::<Vec<_>>()
                    .join("\n");
                println!("{body}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `remove`'s finisher — the two-phase pair (describe / applied).
fn finish_remove(
    json: bool,
    command: &str,
    result: Result<ops::RemoveOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::RemoveOutcome::Described { data, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::remove_describe_tty(&data, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::RemoveOutcome::Applied(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::undo_next_actions(&data.undo);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::remove_applied_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `update --reset`'s finisher — the loss-led describe / the applied discard.
fn finish_reset(
    json: bool,
    command: &str,
    result: Result<ops::ResetOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::ResetOutcome::Described { items, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": { "items": items } });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::reset_describe_tty(&items, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::ResetOutcome::Applied(items)) => {
            if json {
                let value = serde_json::json!({ "items": items });
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::reset_applied_tty(&items));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `invite`'s finisher — the bare read, the two-phase describe, or the applied roster write.
fn finish_invite(
    json: bool,
    command: &str,
    result: Result<ops::InviteOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::InviteOutcome::Read(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::invite_read_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::InviteOutcome::Described { describe, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": describe });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::invite_describe_tty(&describe, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::InviteOutcome::Applied(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::invite_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `review`'s finisher — the inbox, a target describe, or an applied verdict. `workspace` is the
/// invocation's global `--workspace` (canonicalized), preserved on the describe's FETCH_FULL_DIFF
/// continuation so an ambiguous skill name re-resolves to the SAME workspace.
fn finish_review(
    json: bool,
    command: &str,
    result: Result<ops::ReviewOutcome, ClientError>,
    workspace: Option<&str>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::ReviewOutcome::Inbox(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::review_inbox_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::ReviewOutcome::Describe { data, next_argvs }) => {
            if json {
                let mut next_actions = render::describe_next_actions(next_argvs.clone());
                let mut warnings = Vec::new();
                // A byte-capped describe diff adds the full-fidelity escape: the SAME
                // `current..<proposal>` diff through `topos diff`, uncapped — carrying the
                // invocation's `--workspace` so an ambiguous name re-resolves identically.
                if data.diff_truncated
                    && let Some((_, hash)) = data.proposal.rsplit_once('@')
                {
                    warnings.push(
                        "DIFF_TRUNCATED: the describe's diff is partial (byte cap) — the \
                         FETCH_FULL_DIFF next action re-runs it uncapped"
                            .to_owned(),
                    );
                    let mut argv = vec![
                        "topos".to_owned(),
                        "diff".to_owned(),
                        data.skill.clone(),
                        format!("current..{hash}"),
                        "--max-bytes".to_owned(),
                        "0".to_owned(),
                    ];
                    if let Some(w) = workspace {
                        argv.extend(["--workspace".to_owned(), w.to_owned()]);
                    }
                    argv.push("--json".to_owned());
                    next_actions.push(crate::actions::next_action(
                        topos_types::ActionCode::FetchFullDiff,
                        argv,
                    ));
                }
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.warnings = warnings;
                envelope.next_actions = next_actions;
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::review_describe_tty(&data, &next_argvs));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::ReviewOutcome::Applied(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::review_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `protect`'s finisher — the two-phase pair.
fn finish_protect(
    json: bool,
    command: &str,
    result: Result<ops::ProtectOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::ProtectOutcome::Described {
            data,
            yes_argv,
            warnings,
        }) => {
            if json {
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.warnings = warnings;
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                println!("{}", render::to_json(&envelope));
            } else {
                let mut text = String::new();
                for w in &warnings {
                    text.push_str(&format!("warning: {w}\n"));
                }
                text.push_str(&render::protect_describe_tty(&data, &yes_argv));
                println!("{text}");
            }
            ExitCode::SUCCESS
        }
        Ok(ops::ProtectOutcome::Applied(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::protect_applied_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `uninstall`'s finisher — the two-phase pair (describe / applied).
fn finish_uninstall(
    json: bool,
    command: &str,
    result: Result<ops::UninstallOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::UninstallOutcome::Described { describe, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": describe });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::uninstall_describe_tty(&describe, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::UninstallOutcome::Applied(applied)) => {
            if json {
                let value = serde_json::to_value(&applied).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::uninstall_applied_tty(&applied));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `revert`'s finisher — the two-phase describe, the byte-level no-op, or the applied forward move.
fn finish_revert(
    json: bool,
    command: &str,
    result: Result<ops::RevertOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::RevertOutcome::Describe { data, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::revert_describe_tty(&data, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::RevertOutcome::NoOp(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::revert_noop_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::RevertOutcome::Applied(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::revert_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// The FALLBACK poll cadence while a human opens the browser and approves — used when a pending
/// disclosure carries no server interval. There is no separate client timeout by default — the device
/// code's own expiry makes the server return a terminal Expired/Denied that surfaces as `Err`, ending
/// the loop (a numeric `--wait` deadline can end it sooner, still pending).
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// A pending device-authorization's human-facing disclosure — the clickable URL (the
/// `verification_uri`, which embeds the code) plus the short cross-check code and the
/// server's minimum poll interval. Extracted from either verb's pending outcome so
/// [`block_on_pending`] can print it once, generically.
struct PendingDisclosure {
    verification_uri: String,
    user_code: String,
    /// The server's minimum poll interval, when disclosed (else the fallback cadence).
    interval_secs: Option<u64>,
    /// When the device code expires (epoch millis), when disclosed — the countdown the live
    /// waiting line shows.
    expires_at_millis: Option<i64>,
}

impl PendingDisclosure {
    /// The cadence to re-poll at — the server's interval when disclosed, else the fallback.
    fn poll_interval(&self) -> Duration {
        self.interval_secs
            .map(Duration::from_secs)
            .unwrap_or(DEVICE_POLL_INTERVAL)
    }
}

/// The pending disclosure for a `follow` outcome (None ⇒ not a pending device-auth).
/// The pending disclosure for a `login` (session) outcome (None ⇒ the login settled).
fn session_login_pending_disclosure(
    out: &topos_types::results::LoginData,
) -> Option<PendingDisclosure> {
    out.pending.as_ref().map(|p| PendingDisclosure {
        verification_uri: p.verification_uri.clone(),
        user_code: p.user_code.clone(),
        interval_secs: p.interval_secs,
        expires_at_millis: p.expires_at.as_deref().and_then(parse_rfc3339_utc_millis),
    })
}

/// `login`'s finisher — pending (the browser-approval wait, with the resume next-action) or the
/// completed session receipt.
fn finish_session_login(
    json: bool,
    command: &str,
    result: Result<topos_types::results::LoginData, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(data) => {
            if json {
                let next_actions = if data.pending.is_some() {
                    vec![crate::actions::next_action(
                        topos_types::ActionCode::from("RESUME_LOGIN".to_owned()),
                        vec!["topos".to_owned(), "login".to_owned(), "--json".to_owned()],
                    )]
                } else {
                    vec![crate::actions::next_action(
                        topos_types::ActionCode::from("UPDATE".to_owned()),
                        vec!["topos".to_owned(), "update".to_owned(), "--json".to_owned()],
                    )]
                };
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = next_actions;
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::session_login_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `logout`'s finisher — the ended sessions + the honest server-side outcome.
fn finish_session_logout(
    json: bool,
    command: &str,
    result: Result<topos_types::results::LogoutData, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(data) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::session_logout_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// The pending disclosure for an `auth login` outcome (None ⇒ the sign-in settled).
/// Parse the client's own RFC 3339 UTC spelling (`YYYY-MM-DDTHH:MM:SSZ` — what the pending
/// disclosures carry) back to epoch millis. Anything else answers `None` (the waiting line then
/// shows elapsed time only).
fn parse_rfc3339_utc_millis(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() != 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' || bytes[19] != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Howard Hinnant's days-from-civil (the inverse of `render::civil_from_days`).
    let yy = if m <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hh * 3600 + mm * 60 + ss) * 1000)
}

/// The waiting line — ONE static print, no timer. The device-flow precedent waits without a
/// countdown, and elapsed/expiry arithmetic on screen only made the wait scarier; expiry stays
/// visible where it matters (the expired-flow error, the settle refusal's named expiry). The
/// GLANCE CODE rides it instead: with approval-from-anywhere first-class, the terminal-visible
/// code is the human-verifiable cross-check against what the page shows.
fn waiting_line(user_code: &str) -> String {
    format!(
        "Waiting for approval in your browser — code {user_code} (the page shows the same code)."
    )
}

/// Whether this invocation blocks on a pending device-authorization, and until when.
/// - `block == false` — never block: print the approval instructions (or emit the `--json`
///   pending document with its resume next-action) and exit 0 after the single poll the
///   invocation itself performed. This is the default whenever NOBODY IS WATCHING A TERMINAL —
///   a `--json` run, or a PIPED stdout (an agent harness surfaces output only when a command
///   exits, so the human blocking wait would read as a silent quarter-hour hang there).
/// - `deadline_millis == None` — block until the device code's own TTL ends it (the TTY default,
///   or a bare `--wait`).
/// - `deadline_millis == Some(t)` — block until settled or the wall clock passes `t` (`--wait <seconds>`),
///   whichever comes first.
struct WaitPolicy {
    block: bool,
    deadline_millis: Option<u64>,
}

impl WaitPolicy {
    /// Derive the policy from `--json`, the `--wait [<seconds>]` flag, and whether STDOUT is a
    /// terminal: block when a human watches a TTY (`!json && stdout_tty`) OR when `--wait` was
    /// given in any form (the explicit opt-in — it works piped, so an agent may choose the
    /// one-command blocking enrollment deliberately); a numeric `--wait <seconds>` sets a
    /// wall-clock deadline.
    fn resolve(json: bool, wait: Option<Option<u64>>, stdout_tty: bool, clock: &dyn Clock) -> Self {
        Self {
            block: (!json && stdout_tty) || wait.is_some(),
            deadline_millis: match wait {
                Some(Some(secs)) => Some(
                    clock
                        .now_unix_millis()
                        .saturating_add(secs.saturating_mul(1000)),
                ),
                _ => None,
            },
        }
    }
}

/// The zero-typing browser plan a follow enrollment's wait may carry: which opener to spawn, the
/// runner that spawns it, and the flow's device-code CHALLENGE (hex sha256 — identifies the flow
/// on the approval page's URL without revealing anything; the code itself never rides a URL).
struct LoopbackPlan<'a> {
    opener: &'static str,
    runner: &'a dyn topos_harness::CommandRunner,
    challenge: String,
    /// Bound BEFORE the flow was started — its existence is what let the start declare the flow
    /// loopback-bound, and it is where the approval redirect wakes this wait.
    listener: ops::loopback::LoopbackListener,
    state: String,
}

/// Block on a pending device-authorization until it settles, an optional deadline passes, or the device
/// code's own expiry ends it with a terminal error — so a person never re-invokes the command by hand. The
/// disclosure prints to STDERR once (stdout stays the clean final render). `pending_of` extracts the
/// disclosure (None ⇒ not pending ⇒ return as-is); `repoll` re-invokes the op, which RESUMES via its
/// on-disk WAL. A `policy.block == false` (headless `--json` without `--wait`) returns the first result
/// untouched — a headless agent must not hang.
///
/// The wait is the one place the CLI blocks on a HUMAN rather than a socket, so an animated sink
/// spins under the static disclosure for as long as it lasts. A line-per-phase sink opens nothing:
/// the `waiting_line` below already said it, once, in better words.
///
/// With a `loopback` plan, the wait ALSO binds an ephemeral 127.0.0.1 listener and auto-opens the
/// approval page carrying the state-bound return coordinates — the browser's redirect wakes the
/// next poll immediately (zero typing). The redirect is only an ACCELERATOR: it carries no
/// secret, and the ordinary poll interval completes the login just the same when it never arrives
/// (the human approved on their phone, or the auto-open failed and they pasted the URL elsewhere).
fn block_on_pending<T>(
    clock: &dyn Clock,
    progress: &dyn crate::progress::ProgressSink,
    policy: &WaitPolicy,
    first: Result<T, ClientError>,
    pending_of: impl Fn(&T) -> Option<PendingDisclosure>,
    loopback: Option<LoopbackPlan<'_>>,
    mut repoll: impl FnMut() -> Result<T, ClientError>,
) -> Result<T, ClientError> {
    if !policy.block {
        return first;
    }
    let disc = match &first {
        Ok(out) => pending_of(out),
        Err(_) => None,
    };
    let Some(disc) = disc else {
        return first;
    };

    // The waiting disclosure on STDERR (stdout stays the clean final envelope/TTY): the URL and
    // the short code on separate lines (the code never rides a URL), and the one line that makes
    // the wait unscary — interrupting loses nothing.
    // A LOOPBACK flow deliberately does NOT print the bare page + code pair: this machine's
    // browser is about to open on a page that needs no typing, and a URL-plus-code invitation
    // only tempts typing the code where nothing asked for it. Its own URL is printed by the
    // listener arm below (auto-opened, or to paste when the open fails) — and the code itself
    // still reaches the terminal on the waiting line, as the glance check against what the page
    // shows (the human-verifiable cross-check, now that an approval can arrive from anywhere).
    if loopback.is_none() {
        eprintln!(
            "Open: {}\nCode: {} (the page shows the same code — confirm it matches)",
            disc.verification_uri, disc.user_code,
        );
    }
    eprintln!(
        "Ctrl-C is safe — the same command resumes this login; `--wait <seconds>` caps the wait."
    );
    // The loopback arm: auto-open, and let the redirect wake the poll. Every fault here is
    // silent — the poll behind it settles the login either way.
    let mut listener = loopback.map(|plan| {
        let bound = plan.listener;
        let url = ops::loopback::approval_url(
            &disc.verification_uri,
            &plan.challenge,
            bound.port(),
            &plan.state,
        );
        let opened = plan
            .runner
            .run(plan.opener, &[url.as_str()])
            .map(|out| out.success)
            .unwrap_or(false);
        if opened {
            eprintln!("Opening your browser to approve.");
        } else {
            // The open failed; the flow is unharmed. This URL needs no typing and returns the
            // approval straight to this terminal — and approving anywhere else still completes
            // here on the next poll, so nothing is stranded either way.
            eprintln!(
                "Could not open a browser. Open this URL to approve:\n  {url}\n\
                 (it returns you here immediately; approving elsewhere finishes this login too, \
                 on the next check)"
            );
        }
        // Kept either way: it costs nothing and turns a click into an instant finish.
        bound
    });
    let interval = disc.poll_interval();
    // The waiting line: ONE static print (no per-second rewrite, no countdown — the device-flow
    // precedent waits quietly), carrying the glance code.
    eprintln!("{}", waiting_line(&disc.user_code));
    // …and a phase HELD OPEN for the length of the wait: a repainting sink shows a live spinner
    // beneath it, a plain sink says it once — and on either, the held phase keeps every poll's
    // transport fallback ("contacting …") from re-announcing itself each interval, so a piped
    // `--wait` log stays two lines instead of one per poll.
    let _phase = crate::progress::phase(progress, "waiting for approval in your browser");

    // `last` is the most recent pending result, handed back verbatim if a numeric `--wait` deadline passes
    // (starts as `first`, so `--wait 0` returns immediately without polling again).
    let mut last = first;
    loop {
        // Honor a numeric deadline precisely: stop the instant it passes (checked BEFORE sleeping, so a
        // short `--wait <n>` is not overshot by a whole poll interval).
        if policy
            .deadline_millis
            .is_some_and(|d| clock.now_unix_millis() >= d)
        {
            return last;
        }
        // The FLOW's own expiry is a hard stop too. The server is the usual source of the
        // terminal answer, but its expired rows are only swept when the next login starts — on a
        // quiet install that sweep may never run, and this loop would poll forever.
        if disc
            .expires_at_millis
            .is_some_and(|e| e >= 0 && clock.now_unix_millis() >= e.unsigned_abs())
        {
            return last;
        }
        // Sleep the poll interval, but never past the deadline.
        let nap = match policy.deadline_millis {
            Some(d) => {
                Duration::from_millis(d.saturating_sub(clock.now_unix_millis())).min(interval)
            }
            None => interval,
        };
        if listener.is_some() {
            // With a listener armed, sleep in short slices so the browser's redirect wakes the
            // poll near-instantly.
            let nap_end = clock
                .now_unix_millis()
                .saturating_add(u64::try_from(nap.as_millis()).unwrap_or(u64::MAX));
            loop {
                let now = clock.now_unix_millis();
                if now >= nap_end {
                    break;
                }
                if let Some(bound) = &listener
                    && bound.try_receive().is_some()
                {
                    // The redirect landed (single-use — stop listening) → poll NOW. Approved, the
                    // poll returns the grant; denied, one confirming poll ends the wait with the
                    // server's own answer, which is the only one this client trusts.
                    listener = None;
                    break;
                }
                std::thread::sleep(
                    Duration::from_millis(nap_end.saturating_sub(now))
                        .min(Duration::from_millis(250)),
                );
            }
        } else {
            std::thread::sleep(nap);
        }
        let next = repoll();
        if matches!(&next, Ok(o) if pending_of(o).is_some()) {
            // Still waiting on the human — keep polling (the deadline is re-checked at the loop top).
            last = next;
        } else {
            // Settled (enrolled / published) or a terminal error (incl. the device code's expiry) — done.
            return next;
        }
    }
}

/// `publish`'s bare-DESCRIBE finisher — the enrolled two-phase preview (nothing landed on the plane) +
/// the paste-ready `--yes` next-action. A typed refusal (NO_CHANGES / CONSENT_MISMATCH / …) flows through
/// [`emit_err`].
fn finish_publish_describe(
    json: bool,
    command: &str,
    result: Result<(topos_types::results::PublishDescribeData, Vec<String>), ClientError>,
    yes_argv: Vec<String>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok((data, warnings)) => {
            if json {
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.warnings = warnings;
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                println!("{}", render::to_json(&envelope));
            } else {
                let mut text = String::new();
                for w in &warnings {
                    text.push_str(&format!("warning: {w}\n"));
                }
                text.push_str(&render::publish_describe_tty(&data, &yes_argv));
                println!("{text}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `publish`'s finisher — the verb yields a direct publish ([`PublishData`]) or an opened proposal
/// ([`ProposeData`]); each renders through its own `--json` payload / TTY line. A typed failure
/// (CONFLICT / DENIED / not-enrolled / …) flows through [`emit_err`], which attaches the right
/// `next_actions`.
fn finish_publish(
    json: bool,
    command: &str,
    result: Result<ops::PublishOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::PublishOutcome::Published(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::publish_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::PublishOutcome::Proposed(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::propose_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// The error-side diagnostics channel — where the redacted user surfaces send the detail they withhold.
/// `safe_message` keeps stdout/TTY leak-free; the FULL `Display` chain has to land SOMEWHERE, and this
/// is it: the append-only `~/.topos/log.jsonl` (plus stderr when `TOPOS_DEBUG=1`).
struct Diag<'a> {
    fs: &'a dyn FsOps,
    clock: &'a dyn Clock,
    log_path: PathBuf,
    /// The invocation as typed (past the binary name). It rides HERE because this struct is
    /// already threaded to every place a failure reaches a surface — and the error surfaces need
    /// it for one judgement: whether a rebuilt command would restate the whole request or quietly
    /// drop half of it. Read-only, never logged (the log records the verb + the typed detail).
    argv: &'a [String],
}

impl Diag<'_> {
    /// Record `err` for `command`: best-effort append of the structured error event (returns whether it
    /// landed — the TTY `details:` pointer prints only then), and the full chain on stderr under
    /// `TOPOS_DEBUG=1` (stderr only — stdout stays the clean envelope).
    fn note(&self, command: &str, err: &ClientError) -> bool {
        if std::env::var_os("TOPOS_DEBUG").is_some_and(|v| v == "1") {
            eprintln!("topos {command} [{}]: {}", err.code(), err.detail());
        }
        logfile::append_error_event(
            self.fs,
            &self.log_path,
            command,
            err.code(),
            &err.detail(),
            // Verb-level (not scoped to one skill) — the per-skill field stays for the sweep's
            // isolated failures.
            None,
            self.clock.now_unix_millis(),
        )
    }
}

fn emit_err(json: bool, command: &str, err: &ClientError, diag: &Diag<'_>) -> ExitCode {
    let logged = diag.note(command, err);
    if json {
        println!(
            "{}",
            render::to_json(&render::err_envelope(command, diag.argv, err))
        );
    } else {
        eprintln!("{}", render::err_tty(err));
        // The way out, between the refusal and the pointer: the SAME next actions the `--json`
        // envelope computes, as runnable lines (and the transience clause where the typed outcome
        // says the failure is retryable). One source of truth — never a second hint table. The
        // verb rides along: an ambiguity's ways out ARE this invocation, re-spelled per candidate.
        if let Some(hint) = render::err_hint_tty(command, diag.argv, err) {
            eprintln!("{hint}");
        }
        // Point a human at the detail the fixed message withheld — only when it actually landed.
        if logged {
            eprintln!("details: {}", diag.log_path.display());
        }
    }
    ExitCode::FAILURE
}

/// Parse the optional `pull` target into a [`ops::PullScope`]: absent = the sweep; `<name>` = accept a
/// pending update; `<name>@<ref>` = go back to that version's bytes; `--onto-current` = the escape.
///
/// A go-back suffix is recognized when the part after the LAST `@` is a version reference — the full
/// 64-char lowercase-hex id, or a short prefix of at least 8 hex chars (resolved against the skill's
/// recorded history, so a pasted 12-char short form from any topos output works). Otherwise the whole
/// argument is the skill name: `team@cli` is accepted as a name, and `team@cli@<ref>` still goes back
/// correctly. A hex suffix SHORTER than 8 chars stays part of the name (never a silent go-back).
///
/// `--onto-current` (the escape) requires a `<skill>` target (clap enforces that half via `requires`)
/// and is mutually exclusive with `@<ref>` (a runtime usage error — the suffix shape is only known
/// after parsing). A skill LITERALLY named like `docs@abcdef12` is not lost to the go-back parse:
/// [`pull_with_name_fallback`] retries the whole argument as the name when the pre-@ part resolves to
/// no tracked skill.
///
/// `store` is the invocation's scope flag ([`ops::StoreScope`]) — carried onto the targeted scope so
/// the mode resolves (and acts) in the store the flag named.
fn build_pull_scope(
    skill: Option<String>,
    onto_current: bool,
    store: ops::StoreScope,
) -> Result<ops::PullScope, ClientError> {
    let Some(arg) = skill else {
        if onto_current {
            // Unreachable through clap (`--onto-current` requires the <skill> positional) — kept as a
            // defensive usage error for a direct caller.
            return Err(ClientError::InvalidArgument(
                "--onto-current requires a <skill> target".into(),
            ));
        }
        return Ok(ops::PullScope::AllFollowed);
    };
    if let Some((name, suffix)) = arg.rsplit_once('@')
        && let Some(vref) = ops::VersionRef::recognize(suffix)
    {
        if onto_current {
            return Err(ClientError::InvalidArgument(
                "--onto-current is not valid with @<ref>".into(),
            ));
        }
        return Ok(ops::PullScope::One {
            name: name.to_owned(),
            workspace: None,
            mode: ops::TargetMode::GoBack(vref),
            store,
        });
    }
    Ok(ops::PullScope::One {
        name: arg,
        workspace: None,
        mode: if onto_current {
            ops::TargetMode::OntoCurrent
        } else {
            ops::TargetMode::AcceptPending
        },
        store,
    })
}

/// Run `pull` with the @-suffix shadowing fallback. `<name>@<ref>` parses as a go-back first — but a
/// skill can LITERALLY be named `docs@abcdef12` (a name is a directory basename, unrestricted; only the
/// skill ID charset forbids `@`), so when the go-back interpretation finds NO tracked skill under the
/// pre-@ name, the WHOLE argument is retried as the skill name (a plain targeted pull) before erroring.
/// A tracked pre-@ name still wins: the go-back is primary, and remains reachable for a colliding name
/// via a longer/full version id whose pre-@ part IS the tracked name.
pub(crate) fn pull_with_name_fallback(
    ctx: &Ctx<'_>,
    skill: Option<String>,
    onto_current: bool,
    store: ops::StoreScope,
    delivery: Option<&dyn crate::plane::DeliverySource>,
    reconcile: &ops::ReconcileOpts,
) -> Result<ops::PullOutcome, ClientError> {
    let arg = skill.clone();
    let _ = (delivery, reconcile);
    let first =
        build_pull_scope(skill, onto_current, store).and_then(|scope| ops::pull(ctx, scope));
    match first {
        Err(ClientError::NoSuchSkill { .. })
            if arg.as_ref().is_some_and(|a| {
                a.rsplit_once('@')
                    .is_some_and(|(_, s)| ops::VersionRef::recognize(s).is_some())
            }) =>
        {
            // The retry's own NoSuchSkill (neither interpretation is tracked) names the FULL argument —
            // the exact token the user typed.
            ops::pull(
                ctx,
                ops::PullScope::One {
                    name: arg.expect("guard checked Some"),
                    workspace: None,
                    mode: ops::TargetMode::AcceptPending,
                    store,
                },
            )
        }
        other => other,
    }
}

/// The breadth arming sweep, run at the composition root — the one layer holding the real ports
/// (`RealFs` is both the `ConfigStore` and the `CommandRunner`) and the resolved machine roots.
/// `None` roots (no `$HOME`) arms nothing: detection needs a home, and the active adapter's own
/// trigger was already armed by the verb.
fn breadth_arm(
    roots: &Option<crate::ctx::AgentRoots>,
    active: &dyn HarnessAdapter,
    fs: &RealFs,
) -> Vec<topos_types::results::BreadthTriggerReport> {
    match roots {
        Some(r) => ops::arm_detected(&r.home, r.cwd.as_deref(), active.id().slug(), fs, fs),
        None => Vec::new(),
    }
}

/// Build the harness adapter for `id`, borrowing the shared config-store seam plus the subprocess
/// runner (OpenClaw's trigger drives its own `openclaw` CLI). Adding a harness is ONE new match arm
/// — no caller change. v0 only ever selects Claude Code (the CLI's one selection site above passes
/// `HarnessId::ClaudeCode`; each adapter resolves its own config home: `$CLAUDE_CONFIG_DIR` else
/// `$HOME/.claude`; `$HOME/.openclaw`; `$HERMES_HOME` else `$HOME/.hermes`). The OpenClaw and
/// Hermes arms serve the test rigs while pilot verification stays open (each module's doc).
fn adapter_for<'a>(
    id: HarnessId,
    fs: &'a dyn ConfigStore,
    cli: &'a dyn topos_harness::CommandRunner,
) -> Box<dyn HarnessAdapter + 'a> {
    match id {
        HarnessId::ClaudeCode => Box::new(ClaudeCode::new(ClaudeCode::resolve_home(), fs)),
        HarnessId::OpenClaw => Box::new(OpenClaw::new(OpenClaw::resolve_home(), fs, cli)),
        HarnessId::Hermes => Box::new(topos_harness::Hermes::new(
            topos_harness::Hermes::resolve_home(),
            topos_harness::Hermes::resolve_accept_hooks(),
            fs,
        )),
    }
}

/// `$TOPOS_HOME`, else `$HOME/.topos` (`./.topos` as a last resort).
fn resolve_home() -> PathBuf {
    if let Some(home) = std::env::var_os("TOPOS_HOME") {
        return PathBuf::from(home);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".topos")
}

/// The discovery roots for `list`/`add`: the user home (every harness's global skill dir resolves
/// under it) + the current project dir (repo-scoped skills). A missing `$HOME` degrades to no
/// discovery rather than an error.
fn list_discovery() -> Option<ops::DiscoveryRoots> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(ops::DiscoveryRoots {
        home,
        cwd: std::env::current_dir().ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_WEB_ORIGIN, PendingDisclosure, WaitPolicy, block_on_pending, build_pull_scope,
        list_page_argv, next_page_action, parse_rfc3339_utc_millis, resolve_web_origin,
        waiting_line,
    };
    use crate::ids::Clock;
    use crate::ops::{PullScope, RowPage, StoreScope, TargetMode, VersionRef};

    struct TestClock(u64);
    impl Clock for TestClock {
        fn now_unix_millis(&self) -> u64 {
            self.0
        }
    }

    #[test]
    fn the_wait_policy_blocks_only_for_a_watched_terminal_or_an_explicit_wait() {
        let clock = TestClock(1_000);
        // The human default: a TTY without --json blocks until the code's own expiry.
        assert!(WaitPolicy::resolve(false, None, true, &clock).block);
        // The agent default: a PIPED stdout without --wait never blocks — the harness only
        // surfaces output on exit, so the long poll would read as a silent hang.
        assert!(!WaitPolicy::resolve(false, None, false, &clock).block);
        // --json inherits the same non-blocking default on both faces.
        assert!(!WaitPolicy::resolve(true, None, false, &clock).block);
        assert!(!WaitPolicy::resolve(true, None, true, &clock).block);
        // --wait is the explicit opt-in and works piped, bare or capped.
        assert!(WaitPolicy::resolve(false, Some(None), false, &clock).block);
        assert!(WaitPolicy::resolve(true, Some(Some(30)), false, &clock).block);
        // The numeric cap sets the wall-clock deadline; the bare form has none.
        assert_eq!(
            WaitPolicy::resolve(false, Some(Some(30)), false, &clock).deadline_millis,
            Some(31_000)
        );
        assert_eq!(
            WaitPolicy::resolve(false, Some(None), false, &clock).deadline_millis,
            None
        );
    }

    #[test]
    fn a_non_blocking_wait_returns_the_first_pending_result_without_a_single_repoll() {
        let clock = TestClock(0);
        let policy = WaitPolicy {
            block: false,
            deadline_millis: None,
        };
        let pending = Ok::<_, crate::error::ClientError>("pending-marker");
        let out = block_on_pending(
            &clock,
            crate::progress::silent(),
            &policy,
            pending,
            |_| {
                Some(PendingDisclosure {
                    verification_uri: "https://x/verify".into(),
                    user_code: "AB12-CD34".into(),
                    interval_secs: Some(5),
                    expires_at_millis: None,
                })
            },
            None,
            || panic!("a non-blocking wait must never re-poll"),
        );
        assert_eq!(out.unwrap(), "pending-marker");
    }

    #[test]
    fn a_zero_second_wait_cap_returns_the_pending_result_at_the_deadline() {
        // `--wait 0`: block resolves true, the deadline is already NOW — the loop's top check
        // returns the last pending result without sleeping a poll interval.
        let clock = TestClock(5_000);
        let policy = WaitPolicy::resolve(true, Some(Some(0)), false, &clock);
        assert!(policy.block);
        let pending = Ok::<_, crate::error::ClientError>("still-pending");
        let out = block_on_pending(
            &clock,
            crate::progress::silent(),
            &policy,
            pending,
            |_| {
                Some(PendingDisclosure {
                    verification_uri: "https://x/verify".into(),
                    user_code: "AB12-CD34".into(),
                    interval_secs: Some(5),
                    expires_at_millis: None,
                })
            },
            None,
            || panic!("the elapsed deadline never re-polls"),
        );
        assert_eq!(out.unwrap(), "still-pending");
    }

    #[test]
    fn the_waiting_line_shows_the_glance_code_and_no_timer() {
        // The RFC 3339 parser inverts the client's own spelling exactly (the flow's expiry still
        // ends the wait loop — it just never renders as a countdown).
        assert_eq!(parse_rfc3339_utc_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_utc_millis("2026-06-25T00:15:00Z"),
            Some(1_782_346_500_000)
        );
        // Round-trip against the emitter the disclosures use.
        let millis = 1_782_346_500_000;
        assert_eq!(
            parse_rfc3339_utc_millis(&crate::ops::fmt_rfc3339_millis(millis)),
            Some(millis)
        );
        for bad in ["", "2026-06-25", "2026-06-25T00:15:00", "not a date"] {
            assert_eq!(parse_rfc3339_utc_millis(bad), None, "{bad}");
        }

        // ONE static line, no elapsed tick, no expiry countdown (the device-flow precedent waits
        // without a timer) — and the GLANCE CODE is on it, the human-verifiable cross-check
        // against what the approval page shows.
        let line = waiting_line("S5CE-CL26");
        assert_eq!(
            line,
            "Waiting for approval in your browser — code S5CE-CL26 (the page shows the same code)."
        );
        assert!(!line.contains("elapsed"), "{line}");
        assert!(!line.contains("expires"), "{line}");
    }

    #[test]
    fn list_page_argv_preserves_every_selector_including_workspace() {
        // The NEXT_PAGE continuation must re-spell the WHOLE view — dropping `--workspace` would
        // widen a narrowed `--remote` catalog to every joined workspace on the next page.
        let argv = list_page_argv(
            None,
            false,
            false,
            false,
            None,
            true,
            false,
            &["eng".to_owned()],
            &["deploy".to_owned()],
            Some("w_acme"),
        );
        assert_eq!(
            argv,
            vec![
                "topos",
                "list",
                "--remote",
                "--channel",
                "eng",
                "--skill",
                "deploy",
                "--workspace",
                "w_acme",
            ]
        );
        // The scope + view flags re-spell too.
        let argv = list_page_argv(
            None,
            true,
            false,
            true,
            Some("cursor"),
            false,
            false,
            &[],
            &[],
            None,
        );
        assert_eq!(
            argv,
            vec!["topos", "list", "-g", "--untracked", "--agent", "cursor"]
        );
        // The bare local list re-spells bare.
        assert_eq!(
            list_page_argv(
                None,
                false,
                false,
                false,
                None,
                false,
                false,
                &[],
                &[],
                None
            ),
            vec!["topos", "list"]
        );
    }

    #[test]
    fn next_page_action_advances_the_offset_and_keeps_the_page_size() {
        let page = RowPage {
            offset: 50,
            limit: Some(50),
        };
        let actions = next_page_action(&page, true, vec!["topos".to_owned(), "list".to_owned()]);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].code.as_str(), "NEXT_PAGE");
        assert_eq!(
            actions[0].argv,
            vec![
                "topos", "list", "--limit", "50", "--offset", "100", "--json"
            ]
        );
        // The action carries the rules module's read-only classification.
        assert_eq!(actions[0].mutates, Some(false));
        // Nothing past this page → no action at all.
        assert!(next_page_action(&page, false, vec!["topos".to_owned()]).is_empty());
    }

    #[test]
    fn web_origin_env_override_beats_the_compiled_default() {
        // No override (or a blank one) → the ONE compiled-in hosted web origin every token-less
        // door dials (`follow <bare-ws>`, `auth login`, the un-enrolled standup publish).
        assert_eq!(resolve_web_origin(None), DEFAULT_WEB_ORIGIN);
        assert_eq!(
            resolve_web_origin(Some("   ".to_owned())),
            DEFAULT_WEB_ORIGIN
        );
        // A non-empty TOPOS_PLANE_URL wins, trimmed of whitespace + a trailing slash.
        assert_eq!(
            resolve_web_origin(Some("http://127.0.0.1:8787/".to_owned())),
            "http://127.0.0.1:8787"
        );
    }

    #[test]
    fn pull_target_recognizes_full_ids_and_short_prefixes() {
        // The full 64-hex suffix goes back (the long-standing shape).
        let full = format!("docs@{}", "ab".repeat(32));
        assert!(matches!(
            build_pull_scope(Some(full), false, StoreScope::Here).unwrap(),
            PullScope::One { name, mode: TargetMode::GoBack(VersionRef::Full(_)), .. } if name == "docs"
        ));
        // A pasted 12-char short form is a go-back too — no more silent NO_SUCH_SKILL degradation.
        assert!(matches!(
            build_pull_scope(Some("docs@ab12cd34ef56".to_owned()), false, StoreScope::Here).unwrap(),
            PullScope::One { name, mode: TargetMode::GoBack(VersionRef::Prefix(p)), .. }
                if name == "docs" && p == "ab12cd34ef56"
        ));
        // A hex-ish suffix SHORTER than the prefix floor stays part of the name (a name may contain `@`),
        // as does any non-hex suffix.
        for name in ["docs@ab12", "team@cli"] {
            assert!(matches!(
                build_pull_scope(Some(name.to_owned()), false, StoreScope::Here).unwrap(),
                PullScope::One { name: n, mode: TargetMode::AcceptPending, .. } if n == name
            ));
        }
        // The escape never combines with a go-back ref.
        let err = build_pull_scope(Some("docs@ab12cd34ef56".to_owned()), true, StoreScope::Here)
            .unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
    }
}
