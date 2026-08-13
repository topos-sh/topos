//! The composition root for the binary: parse argv, wire the real seams, run recovery, dispatch a verb,
//! and emit either the `--json` envelope (stdout) or thin TTY text.

use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use clap::Parser;
use serde::Serialize;
use topos_harness::triggers::TriggerAdapter;
use topos_harness::{ClaudeCode, ConfigStore, HarnessAdapter, OpenClaw, registry, triggers};
use topos_types::HarnessId;

use crate::cli::{AuthCmd, Cli, Command};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::{FsOps, RealFs};
use crate::ids::{Clock, RealClock, RealIds};
use crate::out::{errln, outln};
use crate::plane::{ContributeSource, DirectorySource, EnrollSource, GovernanceSource};
use crate::plane_http::{UreqDeviceClient, UreqPlane};
use crate::sidecar::{Layout, recover};
use crate::{identity, logfile, ops, render};

/// Run the CLI; returns the process exit code. A thin wrapper over the dispatch: AFTER a successful
/// eligible command it runs the passive version check (stderr-only, self-throttled to at most one
/// probe per day, silent on every failure — `ops::version_check` owns the policy). The check never
/// runs on the quiet sweep (the session-start hook path gains zero latency and zero noise), on
/// `self-update`/`upgrade` (they ARE the release surface), on `uninstall` (it would recreate the
/// state dir the command just deleted), or on a failed command.
///
/// THE OUTCOME'S READER LEAVING ends the run cleanly: `topos list | head` closes the pipe
/// mid-print, and every write from there on is dropped (the `out` module) so the last word is an
/// exit code, not a panic. Exit 0 — that reader has everything it asked for, which is what a
/// well-behaved CLI calls success. STDOUT alone decides this: a gone stderr reader has only stopped
/// listening to diagnostics, and turning that into a success would report a failed command as one.
pub fn run() -> ExitCode {
    let code = dispatch();
    if crate::out::stdout_closed() {
        return ExitCode::SUCCESS;
    }
    code
}

/// [`run`] minus the closed-pipe verdict — parse argv, dispatch, then the passive version check.
fn dispatch() -> ExitCode {
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
    // The two DOCUMENT flags, intercepted before any dispatch: they read no state, touch no file,
    // dial nothing, and answer identically on a broken install. `--skill` first — an agent that
    // asked for both wants the teaching document. A document asked for BESIDE a verb is a usage
    // error, not a document printed over a verb that silently never ran.
    if let Some(code) = document_flag_over_a_verb(&cli) {
        return code;
    }
    if let Some(doc) = document_flag(&cli) {
        outln!("{doc}");
        return ExitCode::SUCCESS;
    }
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
            errln!("{line}");
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

/// The refusal a DOCUMENT flag typed beside a verb earns: exit 2 with a clap usage error naming
/// both halves of the conflict, or `None` when there is no conflict to answer.
///
/// `topos --skill list` asks two questions with one answer between them. Printing the document
/// would swallow the verb — a person, or an agent, waits for a listing and reads a skill document
/// instead, with a success code under it — and running the verb would swallow the flag. Both
/// documents stand alone by construction (they read no state and touch no file), so there is
/// nothing for a verb to add to them. The `--json` half of the same rule is clap's own, declared on
/// the flags themselves.
fn document_flag_over_a_verb(cli: &Cli) -> Option<ExitCode> {
    let flag = if cli.skill {
        "--skill"
    } else if cli.schema {
        "--schema"
    } else {
        return None;
    };
    let verb = cli.command.as_ref()?.name();
    let mut cmd = crate::cli::cli_command();
    let err = cmd.error(
        clap::error::ErrorKind::ArgumentConflict,
        format!("the argument '{flag}' cannot be used with the subcommand '{verb}'"),
    );
    let _ = err.print();
    Some(ExitCode::from(2))
}

/// The stand-alone DOCUMENT flags (`--skill`, `--schema`) — the text to print, or `None` when
/// neither was asked for.
///
/// Both answer from bytes compiled INTO the binary, which is the whole point: they work on a
/// machine with no sidecar, no login, and no detected harness, so an agent can always be handed
/// the teaching document and the `--json` contract even where placement is impossible (a foreign
/// `topos` dir, a `remove topos` opt-out, an unknown agent). `--skill` wins when both are set —
/// whoever asked for both asked to be taught first.
fn document_flag(cli: &Cli) -> Option<String> {
    let mut text = if cli.skill {
        ops::builtin_skill_md().to_owned()
    } else if cli.schema {
        crate::schema_doc::schema_doc()
    } else {
        return None;
    };
    // Each document carries its own trailing newline; `outln!` supplies the line ending.
    if text.ends_with('\n') {
        text.pop();
    }
    Some(text)
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
    // The PLACEMENT port, selected through the one dispatch seam. v0 wires Claude Code only; it
    // reads skill dirs and writes nothing.
    let harness = adapter_for(HarnessId::ClaudeCode, &fs, &fs);
    // The TRIGGER ports, composed beside it — never through it. The active harness's trigger comes
    // from the ONE registry factory (so this and the breadth sweep can never build a slug two ways),
    // and `$HOME` widens the set to the whole machine: with no home there is no detection, so the
    // active trigger is the whole surface, exactly as the breadth sweeps degrade.
    let active_trigger = trigger_for(harness.id(), &fs, &fs);
    let triggers = match std::env::var_os("HOME") {
        Some(home) => {
            ops::Triggers::machine(active_trigger.as_ref(), PathBuf::from(home), &fs, &fs)
        }
        None => ops::Triggers::active_only(active_trigger.as_ref()),
    };
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
            triggers: triggers.clone(),
            plane: &inert_plane,
            follow: &inert_follow,
            // The teardown reaches the machine's agent CONFIGS as well as its own tree — the MCP
            // entries topos placed are retired before the ledger proving they are topos's is
            // deleted with the sidecar — and those surfaces resolve under `$HOME`.
            roots: std::env::var_os("HOME").map(|h| {
                crate::ctx::AgentRoots::new(PathBuf::from(h), std::env::current_dir().ok())
            }),
            // The teardown is entirely local (delete `~/.topos/`, scrub the triggers) — nothing to
            // report activity about.
            progress: crate::progress::silent(),
        };
        let binary = std::env::current_exe().ok();
        // The breadth scrub rides the applied receipt, from inside the verb: once the teardown's
        // own deletions have succeeded, every OTHER agent's trigger artifact is removed too (or its
        // survival disclosed — OpenClaw's gateway may be down). Swept over the SUPPORTED set, not
        // detection: an artifact must be scrubbed even when its harness's detect dir has since
        // vanished — and the bare describe disclosed exactly that set.
        let result = ops::uninstall(&ctx, binary, *yes);
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
            triggers: triggers.clone(),
            plane: &inert_plane,
            follow: &inert_follow,
            roots: std::env::var_os("HOME").map(|h| {
                crate::ctx::AgentRoots::new(PathBuf::from(h), std::env::current_dir().ok())
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
                    ops::probe_detected(&r.home, r.cwd.as_deref(), ctx.triggers.active(), &fs, &fs);
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
        errln!("topos: {}", warning.text);
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
        triggers: triggers.clone(),
        plane,
        follow,
        // The machine roots the placement engine detects agents against — the same `$HOME` + cwd
        // resolution untracked discovery uses. Absent `$HOME` degrades to the classic single-dir
        // placement (the active adapter's), never an error.
        roots: std::env::var_os("HOME")
            .map(|h| crate::ctx::AgentRoots::new(PathBuf::from(h), std::env::current_dir().ok())),
        progress: &*progress,
    };

    // The CONTRIBUTE connector — the one op (`revert`) that builds its write transport from a base
    // URL plus the credential its caller resolved, rather than from a session lane.
    let connect_contribute =
        |base_url: &str, credential: Option<&str>| -> Box<dyn ContributeSource> {
            Box::new(
                UreqDeviceClient::new(base_url.to_owned(), credential.map(str::to_owned))
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
    // The default WEB origin `login` dials on a fresh install: the env override, else the hosted
    // web origin (the card re-roots onto the API).
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
            let connect_session_directory =
                |base: &str, cred: &str| -> Box<dyn crate::plane::DirectorySource> {
                    Box::new(
                        UreqDeviceClient::new(base.to_owned(), Some(cred.to_owned()))
                            .with_progress(Rc::clone(&progress)),
                    )
                };
            let connectors = ops::LoginConnectors {
                enroll: &connect_enroll,
                delivery: &connect_session_delivery,
                lane: &connect_session_lane,
                directory: &connect_session_directory,
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
                errln!(
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
            dest,
            kind,
            as_bundle,
            global,
            yes,
        } => {
            // The `-a`/`--dest` SELECTION, verbatim — resolved per arm below (a skill source
            // freezes skills folders, an MCP source config files).
            let selection = ops::Selection::new(&agent, &dest);
            // THE IDENTITY CLAIM, ahead of every resolution ladder: `--as` says the positional is
            // a FOLDER that already holds a copy of a bundle this scope manages, which is a
            // different question from every one the ladders answer. It is refused for anything
            // else rather than reinterpreted — a claim over a workspace reference or a repo would
            // be a guess about what the person meant.
            if let Some(as_bundle) = as_bundle {
                let result = claim_arm(
                    &ctx,
                    &source,
                    &as_bundle,
                    &skill,
                    kind,
                    &selection,
                    workspace.as_deref(),
                    global,
                );
                return finish(json, cmd_name, result, render::add_tty, &diag);
            }
            // `--kind` declares WHAT is being added, so it is read before every resolution ladder
            // below: an MCP server is a tool endpoint, not a skill folder, and none of the
            // ladder's answers (untracked discovery, the keep-as-yours fork, the workspace
            // catalogs) mean anything for one. `-a`/`--dest` narrow WHICH config files get the
            // entry.
            if kind == Some(crate::bundle_kind::BundleKind::Mcp) {
                // `-s` picks WHICH skills to take from a repo holding several. An MCP server
                // bundle is one document with nothing to pick from, so the pair is refused by
                // name rather than silently ignoring the selector.
                if !skill.is_empty() {
                    return emit_err(
                        json,
                        cmd_name,
                        &ClientError::InvalidArgument(
                            "`-s` picks which skills to take from a repository that holds \
                             several — an MCP server bundle is one document, so `--kind mcp` has \
                             nothing to select; drop `-s`"
                                .into(),
                        ),
                        &diag,
                    );
                }
                // The one door applies immediately with an undo-led receipt; `--yes` stays parsed
                // and changes nothing here (the flag's own help says so).
                let result = ops::add_mcp(&ctx, &source, global, &selection);
                // The arming sweep + the built-in ride an APPLIED import exactly as they ride any
                // other adopt receipt (the same trigger-arming moment).
                let result = result.map(|mut added| {
                    if added.data.currency.is_some() {
                        added.data.triggers = breadth_arm(&ctx.roots, harness.as_ref(), &fs);
                        if let Err(e) = ops::ensure_builtin(&ctx) {
                            let _ = diag.note(cmd_name, &e);
                        }
                    }
                    added
                });
                return finish_add_mcp(json, cmd_name, result, &diag);
            }
            // The `-s`/`-a`/`--dest` SELECTORS (single, multiple, or the `*` fan-outs) narrow a
            // REMOTE import — which members land, into which destinations. They pass through the
            // SAME describe-first shape and the SAME per-scope store as a bare `add owner/repo`:
            // a selector narrows what lands, never what a person gets to read first. `-s *`
            // expands to every skill in the repo; `-a *` to every harness DETECTED on this machine.
            let has_selectors = !skill.is_empty() || !selection.is_empty();
            let looks_remote = matches!(
                crate::source::classify(&source),
                crate::source::SourceSpec::Remote(_)
            );
            if has_selectors && looks_remote {
                let git =
                    crate::plane_http::UreqGitSource::new().with_progress(Rc::clone(&progress));
                let result = ops::add_forge_selected(
                    &ctx,
                    &connect_session_transports,
                    &git,
                    &source,
                    &skill,
                    &agent,
                    &dest,
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
            // Whether the invocation SAID what kind it is adding — the one input the local-path
            // adopt's server-bundle guard reads (it arms on silence, never over an explicit word).
            let declared = if kind.is_some() {
                ops::KindDeclared::Yes
            } else {
                ops::KindDeclared::No
            };
            let single_skill = skill.into_iter().next();
            let single_agent = agent.first().cloned();
            // The BUILT-IN's restore: `add topos` clears the durable `remove topos` opt-out and
            // re-places (adopting a marker-carrying downloaded copy snapshot-first). Intercepted
            // ahead of every resolution ladder — the name is reserved end-to-end. It manages its
            // own placement, so a selection over it refuses whole.
            if ops::is_builtin(&source) && !selection.is_empty() {
                return finish(
                    json,
                    cmd_name,
                    Err(ClientError::SelectionRefused(
                        "the built-in topos skill manages its own placement — drop `-a`/`--dest`"
                            .into(),
                    )),
                    render::add_tty,
                    &diag,
                );
            }
            if ops::is_builtin(&source) {
                let result = ops::restore_builtin(&ctx)
                    .map(|sync| serde_json::json!({ "restored": true, "changed": sync.changed }));
                return finish(
                    json,
                    cmd_name,
                    result,
                    |_| {
                        "Restored the built-in `topos` skill on this machine \
                         (undo: topos remove topos --yes).\nsource: built-in"
                            .to_owned()
                    },
                    &diag,
                );
            }
            // REFERENCES first, read by SHAPE (the joined-key grammar): a workspace bundle
            // (`@ws/name`, the canonical `host/ws/name`), a channel (`@ws/channels/x`), a
            // workspace FEED (`@ws`, `-g`), or a git source (`owner/repo[/skill]`, a github URL).
            // Each records ONE manifest row; a git source describes before it installs. A
            // `-a`/`--dest` selection rides the arm and lands in the row's `dest` (the `-s`
            // member selector stays with the multi-import path above, which places per
            // (skill × destination)).
            // A `/`-containing token that names a REAL directory here is a local adopt, whatever
            // the grammar would read it as — the bytes in front of you win over a shorthand.
            let looks_local = matches!(
                crate::source::classify(&source),
                crate::source::SourceSpec::LocalPath(ref p) if p.exists()
            );
            let reference_arm = !looks_local;
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
                        &selection,
                        kind,
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
            // name falls through to the ordinary adopt below. A `-a`/`--dest` selection skips the
            // fork question entirely — it is about where a row's copies live, which the adopt and
            // subscribe arms below both answer.
            if selection.is_empty()
                && let crate::source::SourceSpec::LocalName(name) = crate::source::classify(&source)
            {
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
                        // A RE-ADD NAMING DESTINATIONS is about where the copies live, not about
                        // adopting the folder again — which is what `adopt_path` would refuse. The
                        // row this scope already spells for it gains them, exactly as a bare name
                        // standing here does.
                        if !selection.is_empty()
                            && let Some(mut d) =
                                ops::extend_folder_dest(&ctx, &scope, &p, &selection)?
                        {
                            let targets = vec![d.name.clone()];
                            clear_unchanged_if_bytes_moved(
                                &mut d,
                                ops::manifest_update(
                                    &ctx,
                                    &connect_session_transports,
                                    None,
                                    &ops::ManifestUpdateOpts {
                                        targets,
                                        scope: if global {
                                            ops::UpdateScope::Machine
                                        } else {
                                            ops::UpdateScope::Here
                                        },
                                        ..Default::default()
                                    },
                                ),
                            );
                            return Ok(d);
                        }
                        // The selection resolves (and refuses) BEFORE the adopt, so an unknown
                        // agent leaves nothing half-landed.
                        let entries = selection.skill_entries(scope.target.scope)?;
                        // The server-bundle guard arms on SILENCE: `--kind skill` on a
                        // `server.json`-rooted folder adopts it as a skill, the person's own word.
                        let mut d = ops::adopt_path(&ctx, &scope, &p, declared)?;
                        if entries.is_empty() {
                            ops::note_added_path_in(&ctx, &mut d, &scope.target, &p)?;
                        } else {
                            ops::note_added_path_dest_in(
                                &ctx,
                                &mut d,
                                &scope.target,
                                &p,
                                &entries,
                            )?;
                            // The dest copies land NOW through the same narrowed update the
                            // workspace arm runs (best-effort; the row is durable demand).
                            let targets = vec![d.name.clone()];
                            clear_unchanged_if_bytes_moved(
                                &mut d,
                                ops::manifest_update(
                                    &ctx,
                                    &connect_session_transports,
                                    None,
                                    &ops::ManifestUpdateOpts {
                                        targets,
                                        scope: if global {
                                            ops::UpdateScope::Machine
                                        } else {
                                            ops::UpdateScope::Here
                                        },
                                        ..Default::default()
                                    },
                                ),
                            );
                            d.dest = entries;
                        }
                        Ok(d)
                    })
                }
                // A bare NAME resolves STANDING first — what the two scopes already know it to be
                // — and only then the two discovery namespaces: the untracked local inventory and
                // the connected workspaces' catalogs.
                crate::source::SourceSpec::LocalName(name) => match list_discovery() {
                    Some(roots) => {
                        // The `-a`/`--dest` selection does NOT suppress the workspace half — the
                        // reference arm records it on the row; only a `-s` member pick (which is
                        // about a repo) does. It DOES change what a name already standing here
                        // means: the act is about destinations, so the row extends.
                        match ops::plan_bare_add(
                            &ctx,
                            &connect_session_transports,
                            &roots,
                            &name,
                            ops::BareAdd {
                                subscribe: single_skill.is_none(),
                                dest_selected: !selection.is_empty(),
                                global,
                                workspace: workspace.as_deref(),
                            },
                        ) {
                            // The name resolved to a REFERENCE — the bundle standing in the other
                            // scope, or the one workspace publishing a name nothing local carries.
                            // It goes through the ordinary reference arm: same row, same delivery,
                            // same receipt shape.
                            Ok(ops::BareAddPlan::Reference { reference, note }) => {
                                let result = ops::add_reference(
                                    &ctx,
                                    &connect_session_transports,
                                    None,
                                    &reference,
                                    global,
                                    yes,
                                    &selection,
                                    kind,
                                );
                                let result = result.map(|outcome| match outcome {
                                    ops::AddRefOutcome::Applied(mut data) => {
                                        if let Some(note) = note {
                                            ops::push_note(&mut data, note);
                                        }
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
                            // The bundle already stands HERE, adopted from a folder, and the
                            // invocation named destinations: the row gains them. Nothing is
                            // adopted again, so no trigger is armed and no version is minted —
                            // the copies land through the same narrowed update every other
                            // destination-carrying arm runs.
                            Ok(ops::BareAddPlan::ExtendFolderDest { dir }) => {
                                ops::add_scope(&ctx, global).and_then(|scope| {
                                    // The record is found by the FOLDER, so a second bundle of the
                                    // same name in this store cannot answer for it.
                                    let Some(mut d) =
                                        ops::extend_folder_dest(&ctx, &scope, &dir, &selection)?
                                    else {
                                        return Err(ClientError::NoSuchSkill {
                                            name: dir.display().to_string(),
                                        });
                                    };
                                    let targets = vec![d.name.clone()];
                                    clear_unchanged_if_bytes_moved(
                                        &mut d,
                                        ops::manifest_update(
                                            &ctx,
                                            &connect_session_transports,
                                            None,
                                            &ops::ManifestUpdateOpts {
                                                targets,
                                                scope: if global {
                                                    ops::UpdateScope::Machine
                                                } else {
                                                    ops::UpdateScope::Here
                                                },
                                                ..Default::default()
                                            },
                                        ),
                                    );
                                    Ok(d)
                                })
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
                                let entries = selection.skill_entries(scope.target.scope)?;
                                // The server-bundle guard arms on SILENCE here exactly as it does
                                // on the path arm: a folder a name resolved to can be an MCP
                                // bundle too, and adopting one as a skill delivers raw JSON into
                                // skills dirs. It asks about the folder that will really be
                                // adopted — for a link shell, the folder its links point into.
                                if declared == ops::KindDeclared::No {
                                    ops::refuse_unflagged_mcp_dir(&sctx, &ops::origin_dir(&path)?)?;
                                }
                                let mut d = ops::add_with_name(
                                    &sctx,
                                    &path,
                                    Some(&name),
                                    true,
                                    crate::bundle_kind::BundleKind::Skill,
                                )?;
                                if entries.is_empty() {
                                    ops::note_added_path_in(&ctx, &mut d, &scope.target, &path)?;
                                } else {
                                    ops::note_added_path_dest_in(
                                        &ctx,
                                        &mut d,
                                        &scope.target,
                                        &path,
                                        &entries,
                                    )?;
                                    let targets = vec![d.name.clone()];
                                    clear_unchanged_if_bytes_moved(
                                        &mut d,
                                        ops::manifest_update(
                                            &ctx,
                                            &connect_session_transports,
                                            None,
                                            &ops::ManifestUpdateOpts {
                                                targets,
                                                scope: if global {
                                                    ops::UpdateScope::Machine
                                                } else {
                                                    ops::UpdateScope::Here
                                                },
                                                ..Default::default()
                                            },
                                        ),
                                    );
                                    d.dest = entries;
                                }
                                // The bytes that just landed are what the disclosure judges
                                // against the team's current version.
                                let adopted = d.bundle_digest.clone().unwrap_or_default();
                                d.published_match = published.map(|p| p.suggestion(&adopted));
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
                        // the row — as the selected agents' dest dirs — so the next update keeps
                        // the copy where it was asked for.
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
                                    dest_root: None,
                                    global,
                                },
                            )?;
                            let dest = ops::dest_for_selected_agents(&chosen, scope.target.scope);
                            ops::note_added_remote(&ctx, &mut d, &scope.target, &dest)?;
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
            global,
            r#ref,
            agent,
            dest,
            max_bytes,
        } => {
            let budget = ops::DiffBudget::resolve(max_bytes, json);
            let selection = ops::Selection::one(agent.as_deref(), dest.as_deref());
            // The FETCH_FULL_DIFF argv — this same diff, uncapped. The scope flag and the copy
            // selector both ride along: dropping either would refetch a DIFFERENT copy's diff
            // than the one just truncated.
            let mut full_argv = vec!["topos".to_owned(), "diff".to_owned(), skill.clone()];
            if let Some(r) = &r#ref {
                full_argv.push(r.clone());
            }
            if global {
                full_argv.push("-g".to_owned());
            }
            full_argv.extend(selection.argv_tail());
            full_argv.extend([
                "--max-bytes".to_owned(),
                "0".to_owned(),
                "--json".to_owned(),
            ]);
            finish_diff(
                json,
                cmd_name,
                ops::diff(
                    &ctx,
                    &skill,
                    r#ref.as_deref(),
                    budget,
                    &selection,
                    if global {
                        ops::StoreScope::Machine
                    } else {
                        ops::StoreScope::Here
                    },
                ),
                full_argv,
                &diag,
            )
        }
        Command::Publish {
            target,
            global,
            agent,
            dest,
            to,
            propose,
            message,
            yes,
        } => {
            let selection = ops::Selection::one(agent.as_deref(), dest.as_deref());
            // The scope flag decides which STORE the name resolves in — and, since publish ships
            // the copy it resolved, which bytes reach the team. Bare stands where you are; `-g`
            // is the machine store alone.
            let store_scope = if global {
                ops::StoreScope::Machine
            } else {
                ops::StoreScope::Here
            };
            // Discovery roots for the auto-add pre-step (a `publish` of an untracked local source adopts it
            // first) — the SAME roots `add`/`list` use; `None` degrades name/dir resolution the same way.
            let roots = list_discovery();
            // A bare ENROLLED publish DESCRIBES what shipping would do (nothing lands on the plane); `--yes`
            // applies. An un-enrolled publish refuses typed inside the op (enroll with `follow` first).
            let publish_sessions = crate::sessions::read_sessions(&fs, &ctx.layout)
                .map(|s| !s.sessions.is_empty())
                .unwrap_or(false);
            if !yes && publish_sessions {
                let described = ops::publish_describe(
                    &ctx,
                    Some(&connect_session_transports),
                    roots.as_ref(),
                    &target,
                    propose,
                    to.as_deref(),
                    workspace.as_deref(),
                    &selection,
                    store_scope,
                );
                // The paste-ready apply: this same publish plus `--yes` (preserving `--propose` / `--to`
                // / `-m` — dropping the message would change the version's commit identity from what the
                // describe computed, so an agent that runs this next action ships the wrong version —
                // and the copy selector, without which the apply would face the freeze again). The
                // scope flag rides where a person writes it: right after the verb.
                let mut yes_argv = vec!["topos".to_owned(), "publish".to_owned()];
                if global {
                    yes_argv.push("-g".to_owned());
                }
                yes_argv.push(target.clone());
                yes_argv.extend(selection.argv_tail());
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
                Some(&connect_session_transports),
                roots.as_ref(),
                &target,
                propose,
                to.as_deref(),
                workspace.as_deref(),
                message.as_deref(),
                &selection,
                store_scope,
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
        // `verify <name> [-g]` — the LIVE check of one MCP server. Read-only end to end: it stores
        // nothing, and its finisher maps the verdict onto the verb's own exit codes (0/3/4/5).
        Command::Verify { name, global } => {
            let store_scope = if global {
                ops::StoreScope::Machine
            } else {
                ops::StoreScope::Here
            };
            finish_verify(json, cmd_name, ops::verify(&ctx, &name, store_scope), &diag)
        }
        Command::Log {
            skill,
            limit,
            offset,
        } => {
            let connectors = ops::LogConnectors {
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
            agent,
            dest,
            yes,
            keep_mine,
            quiet,
            ttl,
            hook,
            force,
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
            // The BUILT-IN by name, refused ONCE for every targeted arm — ahead of all of them,
            // including `--reset`, which dispatches on its own and used to slip past a guard the
            // reconcile, the go-back and `--keep-mine` each enforced separately. It is not a
            // subscription and no store of ours holds a version to take: the bare sweep re-syncs it
            // to this binary, and the binary itself is `self-update`'s business.
            if targets
                .iter()
                .any(|t| ops::is_builtin(t.split('@').next().unwrap_or(t)))
            {
                return emit_err(
                    json,
                    cmd_name,
                    &ClientError::InvalidArgument(
                        "`topos` is the built-in skill — the bare `topos update` re-syncs it to \
                         this binary; `topos self-update` updates the binary itself"
                            .into(),
                    ),
                    &diag,
                );
            }
            // `--reset` is its own two-phase discard verb (loss-led describe / `--yes` apply); it
            // does not flow through the reconcile and is never a `--quiet` hook shape.
            let selection = ops::Selection::one(agent.as_deref(), dest.as_deref());
            if reset {
                return finish_reset(
                    json,
                    cmd_name,
                    ops::reset(&ctx, &targets, yes, store_scope, &selection),
                    &diag,
                );
            }
            // `--dest`/`-a` on `update` narrows ONE thing: which copy's edits `--reset` drops. It
            // has no meaning for the reconcile, which converges every copy the manifest demands —
            // so it is refused by name rather than silently ignored, which would read as a
            // narrowed update that quietly touched everything.
            if !selection.is_empty() {
                return emit_err(
                    json,
                    cmd_name,
                    &ClientError::InvalidArgument(
                        "`--dest`/`-a` on `update` names the copy `--reset` drops the edits of — \
                         add `--reset`, or drop the selector (an update converges every copy your \
                         `topos.toml` asks for)"
                            .into(),
                    ),
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
            // The go-back (`<skill>@<hash>`) and the `--keep-mine` escape act on LOCAL bytes
            // through the classic per-skill engine; everything else is the MANIFEST reconcile —
            // resolve the manifest layers covering cwd + the logged-in profiles and converge.
            let goback_target = targets
                .len()
                .eq(&1)
                .then(|| targets[0].clone())
                .filter(|t| t.split_once('@').is_some_and(|(_, r)| !r.is_empty()));
            let result = if keep_mine || goback_target.is_some() {
                if targets.len() > 1 {
                    Err(ClientError::InvalidArgument(
                        "the go-back and --keep-mine take a single <skill> target".into(),
                    ))
                } else {
                    pull_with_name_fallback(
                        &ctx,
                        targets.into_iter().next(),
                        keep_mine,
                        store_scope,
                    )
                }
            } else {
                // The background sweep DOES contact a forge — a row pointing at a repository is
                // kept current like everything else. It just runs on the git lane's own, much
                // slower clock rather than this sweep's: the cadence decision belongs to the
                // reconcile, which is where the clock is read and advanced.
                let git =
                    crate::plane_http::UreqGitSource::new().with_progress(Rc::clone(&progress));
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
                    Some(&git as &dyn crate::git_source::GitTarballSource),
                    &ops::ManifestUpdateOpts {
                        targets,
                        ack_notices: !quiet,
                        rebuild: force,
                        scope,
                        // A person who typed the command gets an answer now; the hook sweep waits
                        // for the interval.
                        forge: if quiet {
                            ops::ForgeCadence::Scheduled
                        } else {
                            ops::ForgeCadence::Now
                        },
                    },
                )
            };
            // The bare sweep also re-syncs the BUILT-IN `topos` skill (force-synced to this
            // binary; the durable opt-out honored inside) — and it runs HERE, after the reconcile,
            // not before it. The reconcile refuses a run WHOLE when a manifest it would drive
            // fails to load, before a session is dialed or a byte moves; a refusal that closes the
            // TTY with `nothing changed` must not have placed a skill folder on its way to saying
            // so. A SOFT failure is not that refusal: the built-in is rendered from this binary
            // and owes nothing to a reachable plane, so an offline sweep still lands the meta-skill
            // a fresh `self-update` brought. Best-effort either way — a built-in hiccup must never
            // fail the team sweep; its byte changes count as a changed sweep below.
            let builtin_changed = if bare_sweep
                && (result.is_ok() || result.as_ref().is_err_and(ops::quiet_soft_failure))
            {
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
            // The HARNESS TABLE rides the same sweep, on the forge lane's own much slower clock:
            // which directories an agent reads changes when a vendor ships, not when topos does,
            // so a machine keeps its copy current between releases. LAST, and best-effort by
            // construction — it moves no skill bytes, nothing downstream depends on it, and a
            // table that lands here is read by the NEXT process, never this one.
            if bare_sweep {
                let _ = crate::harness_registry::refresh_if_due(
                    &fs,
                    &ctx.layout,
                    i64::try_from(clock.now_unix_millis()).unwrap_or(now_ms),
                    i64::try_from(crate::ids::IdSource::jitter_below(
                        &ids,
                        u64::try_from(crate::harness_registry::CHECK_JITTER_MS).unwrap_or(0),
                    ))
                    .unwrap_or(0),
                    &crate::plane_http::UreqHarnessRegistry::new(),
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
                            outln!("{doc}");
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
                            outln!("{doc}");
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
            agent,
            dest,
            global,
            via,
            yes,
        } => {
            let selection = ops::Selection::new(&agent, &dest);
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
            // feed, or switch one feed-delivered bundle off here — or, with `-a`/`--dest`,
            // subtract just those destinations from a row. It never touches the server — what a
            // workspace assigns you is managed on the web.
            if global {
                let result = ops::remove_global(
                    &ctx,
                    &connect_session_transports,
                    &skill,
                    via.as_deref(),
                    yes,
                    &selection,
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
                &selection,
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
            // `--dest` names a manifest row's destination; a token no manifest arm claimed has
            // no row to narrow — refuse rather than routing the flag into the classic removal.
            if !dest.is_empty() {
                return emit_err(
                    json,
                    cmd_name,
                    &ClientError::SelectionRefused(
                        "--dest narrows a manifest row's destinations, and no manifest line here \
                         carries the named target — `topos list` shows what each scope delivers"
                            .into(),
                    ),
                    &diag,
                );
            }
            let connectors = ops::RemoveConnectors {
                session: &connect_session_transports,
            };
            let roots = list_discovery();
            let result = ops::remove(&ctx, &connectors, &skill, &agent, roots.as_ref(), yes);
            finish_remove(json, cmd_name, result, &diag)
        }
        // `protect <target> [<level>]` — the two-phase protection change for a skill or a channel
        // (bare tightens to the kind's protected level; an explicit `open` loosens).
        Command::Protect { target, level, yes } => {
            let connectors = ops::ProtectConnectors {
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
                outln!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                outln!("{}", tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `verify`'s finisher — like [`finish`], except the SUCCESS path does not always exit `0`.
///
/// A verification that ran is a successful command whatever it found: the envelope stays `ok: true`
/// and the payload carries the verdict. What the verdict decides is the PROCESS exit code — `0`
/// responding · `3` sign-in required (healthy) · `4` not reachable · `5` reachable but not
/// answering as an MCP server — so a script can branch on the answer without parsing a word of it.
/// The code is also carried IN the payload, because an agent reading `--json` through a wrapper may
/// never see the process's own.
///
/// A REFUSAL (no such bundle, an ambiguous name, a skill) is a different thing entirely and keeps
/// the ordinary `1`: nothing was verified, so there is no verdict to report.
fn finish_verify(
    json: bool,
    command: &str,
    result: Result<topos_types::results::VerifyData, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(data) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                outln!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                outln!("{}", render::verify_tty(&data));
            }
            ExitCode::from(data.exit_code)
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
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::auth_status_tty(&data));
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
                outln!("{}", render::to_json(&envelope));
            } else if bare && data.sessions.is_empty() {
                outln!("{}", render::welcome_tty(&data));
            } else {
                outln!("{}", render::status_tty(&data));
            }
            ExitCode::SUCCESS
        }
        // Read-only even on failure: the envelope/TTY error renders, but nothing is appended to
        // the diagnostics log and no `details:` pointer is printed (there is no written detail).
        Err(e) => {
            if json {
                outln!(
                    "{}",
                    render::to_json(&render::err_envelope(command, argv, &e))
                );
            } else {
                errln!("{}", render::err_tty(&e));
                if let Some(hint) = render::err_hint_tty(command, argv, &e) {
                    errln!("{hint}");
                }
            }
            ExitCode::FAILURE
        }
    }
}

/// `pull`'s finisher — like [`finish`], but a bare sweep's isolated per-skill failures ride the
/// envelope's `warnings` (one stable-shape line per failed skill) AND the TTY summary, so a wedged
/// skill is visible on both surfaces. Isolation semantics hold — `ok` stays `true` and the sweep
/// still reports every other bundle — but the EXIT STATUS does not lie: a run that could not carry
/// a bundle forward exits non-zero, because the caller most likely to be watching is an agent, and
/// an agent that reads 0 forever never learns the run is not converging.
///
/// A pending DECISION is the one thing that looks like a failure and is not: the bundle is
/// waiting on the person, nothing is broken, and no retry changes the answer. Those exit 0 and are
/// counted under `waiting on you`; only real faults (network, unreadable store, corrupt record)
/// take the non-zero status.
/// Whether a sweep could not carry something forward — the ONE verdict `finish_pull` reads for
/// both the envelope's `ok` and the exit status, so the two can never contradict each other.
/// Isolation is unaffected: the payload still reports every other bundle, and the failure lines
/// ride `warnings` either way.
fn sweep_failed(warnings: &[topos_types::Message]) -> bool {
    !warnings.is_empty()
}

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
            // A DEAD END is a run with nothing to say and no way to have anything: no sessions,
            // no rows, no faults. A run that FAILED on a row is none of those — it had work and
            // could not finish it — and offering `login` there answered a question nobody asked
            // while the fault's own remedy went unmentioned.
            let unenrolled_dead_end =
                !enrolled && out.data.skills.is_empty() && out.warnings.is_empty();
            // ONE binding answers both surfaces — the envelope's `ok` and the process's exit
            // status. They were computed apart, and a partial failure printed `ok: true` on the
            // very run it exited non-zero; deriving them from one value makes that disagreement
            // unrepresentable rather than merely fixed.
            let failed = sweep_failed(&out.warnings);
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
                // A merge that STOPPED carries its two ways out — the same pair the TTY prints
                // under the row. The TTY named them and the envelope did not, which left an
                // agent reading the one state that asks for a decision with nothing to act on.
                next_actions.extend(render::conflict_next_actions(&out.data));
                // A merge that a `--keep-mine` just FINISHED carries the one command left to run on
                // it: the ordinary publish. It replaces a state an agent could not ship at all.
                next_actions.extend(render::resolution_next_actions(&out.data));
                // A bundle carrying unshared edits gets the same `publish` the TTY row prints.
                next_actions.extend(render::draft_next_actions(&out.data));
                // A bundle waiting on a decision carries its ways out as runnable argv — the same
                // structured channel every other "what do I do now" answer uses, so an agent acts
                // on the choice instead of parsing a sentence about it.
                next_actions.extend(render::decision_next_actions(&out.decisions));
                // The faults' own remedies — the same fix the warning line spells in prose, as
                // argv. They lead nothing and follow nothing; they are simply the answer to the
                // one question an agent reading a failing run has.
                next_actions.extend(out.fault_actions.iter().cloned());
                let value = serde_json::to_value(&out.data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                // `ok` says what the EXIT STATUS says. A sweep that could not carry a bundle
                // forward exits non-zero (below) — and used to hand the agent reading its JSON an
                // `ok: true` on the same run. One of the two answers had to be false, and an
                // agent that trusts `ok` over the status is exactly the caller this channel
                // exists for. Isolation is unchanged: the payload still reports every other
                // bundle, and the failures are named in `warnings`.
                envelope.ok = !failed;
                // ONE stable machine channel: failures first, then the decisions, then the
                // advisories, then the disclosures. The split is the receipt's (a decision, an
                // advisory or a disclosure must not be counted as a failure), not the wire's.
                let mut messages = out.warnings;
                messages.extend(out.decisions.iter().map(render::decision_message));
                messages.extend(out.advisories.iter().cloned());
                messages.extend(out.disclosures.iter().cloned());
                // ONE string, two renderings: the legacy array is DERIVED from the typed one, in
                // the same order, so every consumer that matches on it reads exactly what it read
                // before — and the code it matched on now has a field of its own.
                envelope.warnings = crate::message::legacy_lines(&messages);
                envelope.messages = messages;
                envelope.next_actions = next_actions;
                outln!("{}", render::to_json(&envelope));
            } else {
                let mut text = render::pull_tty(
                    &out.data,
                    &out.decisions,
                    &out.warnings,
                    &out.advisories,
                    &out.disclosures,
                    out.failed_bundles.len(),
                );
                if !out.access_gone.is_empty() {
                    text.push_str(&format!(
                        "\nnote: the session{} for {} ended. Every skill it delivered stays in \
                         place, frozen. Run 'topos login <workspace-address>' to reconnect.",
                        if out.access_gone.len() == 1 { "" } else { "s" },
                        out.access_gone.join(", ")
                    ));
                }
                if unenrolled_dead_end {
                    text.push_str(
                        "\nNot logged in. Join your team with 'topos login <workspace-address>' \
                         — ask a teammate for the address.",
                    );
                }
                outln!("{text}");
            }
            if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

#[cfg(test)]
mod sweep_verdict_tests {
    /// An `add --kind mcp` whose converge failed used to print `ok: true` and exit 0 — the row had
    /// landed, so the verb called itself done while nothing had been delivered anywhere.
    #[test]
    fn an_mcp_add_that_delivered_nothing_is_not_ok_and_a_partial_one_still_exits_non_zero() {
        let failure = crate::message::failure("MCP_CUSTODY_WRITE_FAILED", "x".to_owned());
        // Clean: ok, exit 0.
        assert_eq!(super::mcp_add_verdict(false, &[]), (true, false));
        // Reached SOME agent, failed on others: still ok (the server is somewhere), exit non-zero.
        assert_eq!(
            super::mcp_add_verdict(false, std::slice::from_ref(&failure)),
            (true, true)
        );
        // Reached NOBODY: not ok, exit non-zero.
        assert_eq!(
            super::mcp_add_verdict(true, std::slice::from_ref(&failure)),
            (false, true)
        );
    }

    /// `ok` and the exit status are ONE answer. A sweep that could not carry a bundle forward
    /// used to print `ok: true` in the envelope and exit non-zero on the same run — and an agent
    /// reading `ok` is exactly the caller that channel exists for.
    #[test]
    fn the_envelope_ok_is_the_exit_status() {
        for warnings in [
            Vec::new(),
            vec![crate::message::failure(
                "IO_ERROR",
                "docs: a filesystem operation failed.".to_owned(),
            )],
        ] {
            let failed = super::sweep_failed(&warnings);
            let ok = !failed;
            let exit_is_failure = failed;
            assert_eq!(ok, !exit_is_failure, "{warnings:?}");
            assert_eq!(ok, warnings.is_empty(), "{warnings:?}");
        }
    }

    /// **The two channels are ONE channel.** `messages` carries the typed line and `warnings` the
    /// legacy string derived from it — same membership, same ORDER, index for index — so every
    /// consumer that matches on `warnings[]` reads exactly what it read before the field existed,
    /// and the derivation is `"{code} {text}"` with a code and the bare text without one.
    #[test]
    fn the_legacy_array_mirrors_the_typed_one_line_for_line() {
        let messages = vec![
            crate::message::failure("CATALOG_UNAVAILABLE", "acme: unreadable.".to_owned()),
            crate::message::uncoded_failure("beta: skipped.".to_owned()),
            crate::message::decision("deploy: waiting on you.".to_owned()),
            crate::message::advisory("GIT_VISIBLE", "docs: visible to git.".to_owned()),
            crate::message::disclosure("MCP_DRIFTED", "cursor: left alone.".to_owned()),
        ];
        let mut envelope = super::render::ok_envelope("update", serde_json::json!({}));
        envelope.warnings = crate::message::legacy_lines(&messages);
        envelope.messages = messages;

        assert_eq!(envelope.warnings.len(), envelope.messages.len());
        for (line, message) in envelope.warnings.iter().zip(&envelope.messages) {
            match &message.code {
                Some(code) => assert_eq!(line, &format!("{code} {}", message.text)),
                None => assert_eq!(line, &message.text),
            }
        }
        assert_eq!(
            envelope.warnings,
            vec![
                "CATALOG_UNAVAILABLE acme: unreadable.",
                "beta: skipped.",
                "deploy: waiting on you.",
                "GIT_VISIBLE docs: visible to git.",
                "MCP_DRIFTED cursor: left alone.",
            ]
        );
    }

    /// **A TTY receipt never prints a code.** A sweep whose every channel carries a coded message
    /// renders as prose only — the SCREAMING_SNAKE token is a lookup key on `messages[].code` and
    /// must not reach the person reading the receipt.
    #[test]
    fn a_tty_receipt_prints_no_screaming_snake_code() {
        let data = topos_types::results::PullData {
            skills: Vec::new(),
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: None,
        };
        let warnings = vec![
            crate::message::failure("CATALOG_UNAVAILABLE", "acme: unreadable.".to_owned()),
            crate::message::failure("PATH_MISSING", "\"~/x\": the folder is gone.".to_owned()),
        ];
        let advisories = vec![crate::message::advisory(
            "MCP_DEST_UNKNOWN",
            "\"linear\" (person): the entry was skipped.".to_owned(),
        )];
        let disclosures = vec![crate::message::disclosure(
            "MCP_FILE_REMOVED",
            "~/.cursor/mcp.json: deleted with the last entry.".to_owned(),
        )];
        let tty = super::render::pull_tty(&data, &[], &warnings, &advisories, &disclosures, 2);
        for code in [
            "CATALOG_UNAVAILABLE",
            "PATH_MISSING",
            "MCP_DEST_UNKNOWN",
            "MCP_FILE_REMOVED",
        ] {
            assert!(!tty.contains(code), "{code} reached the TTY:\n{tty}");
        }
        // …and every message's own prose DID reach it.
        for m in warnings.iter().chain(&advisories).chain(&disclosures) {
            assert!(
                tty.contains(&m.text),
                "{:?} is missing from:\n{tty}",
                m.text
            );
        }
        // The one general rail: no bare SCREAMING_SNAKE token anywhere in the receipt.
        for token in tty.split_whitespace() {
            let word = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
            assert!(
                !(word.len() > 3
                    && word.contains('_')
                    && word
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())),
                "a code-shaped token reached the TTY: {word}\n{tty}"
            );
        }
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
                let mut next_actions = next_page_action(
                    &page,
                    out.data
                        .truncated
                        .iter()
                        .any(|b| (page.offset as u64).saturating_add(b.shown) < b.total),
                    page_argv,
                );
                // A NOT-MANAGED deep dive that found unmanaged copies points at the adopt — the
                // SAME single command the TTY prints, templated: the name is known, the
                // destination is the person's choice (`<dest>` rides `needs`). One action, not
                // one per folder: the folders are where copies WERE found, not a list of adopts
                // to run, and the enumeration is unbounded while the recommendation is one.
                if let Some(detail) = out
                    .data
                    .detail
                    .as_ref()
                    .filter(|d| !d.managed && !d.folders.is_empty())
                {
                    next_actions.push(crate::actions::next_action(
                        topos_types::ActionCode::from("ADD_SKILL".to_owned()),
                        vec![
                            "topos".to_owned(),
                            "add".to_owned(),
                            "-g".to_owned(),
                            detail.name.clone(),
                            "--dest".to_owned(),
                            "<dest>".to_owned(),
                            "--json".to_owned(),
                        ],
                    ));
                }
                // The stable-shape truncation line — a belt for a consumer that ignores the new
                // typed markers: a capped enumeration is never mistakable for a complete one. Its
                // KIND is `disclosure`: the run succeeded and the page it returned is exactly the
                // page it promised, so a channel where `failure` means "this did not happen" must
                // not carry it. The legacy `warnings` membership and order do not move.
                let mut messages = out.warnings.clone();
                for b in &out.data.truncated {
                    messages.push(crate::message::list_truncated(&b.bucket, b.shown, b.total));
                }
                let value = serde_json::to_value(&out.data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.warnings = crate::message::legacy_lines(&messages);
                envelope.messages = messages;
                envelope.next_actions = next_actions;
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::list_tty(&out));
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
                outln!(
                    "{}",
                    render::to_json(&diff_envelope(command, &data, full_argv))
                );
            } else {
                outln!("{}", render::diff_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// The `--json` envelope `diff` prints — the ONE place its payload, its size fact, the legacy
/// derivation of that fact, and the full-fidelity next action are joined. Split out of the
/// finisher so the committed golden can be asserted against the FINISHER'S OWN bytes: a golden
/// rebuilt from `data` alone can never notice a line channel that stopped being populated.
pub(crate) fn diff_envelope(
    command: &str,
    data: &topos_types::results::DiffData,
    full_argv: Vec<String>,
) -> topos_types::JsonEnvelope {
    let mut messages = Vec::new();
    let next_actions = if data.truncated {
        messages.push(crate::message::diff_truncated(false));
        vec![crate::actions::next_action(
            topos_types::ActionCode::FetchFullDiff,
            full_argv,
        )]
    } else {
        Vec::new()
    };
    let value = serde_json::to_value(data).unwrap_or_default();
    let mut envelope = render::ok_envelope(command, value);
    envelope.warnings = crate::message::legacy_lines(&messages);
    envelope.messages = messages;
    envelope.next_actions = next_actions;
    envelope
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
                let mut messages = Vec::new();
                if data.truncated
                    && let Some(total) = data.total
                {
                    messages.push(crate::message::log_truncated(data.events.len(), total));
                }
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.warnings = crate::message::legacy_lines(&messages);
                envelope.messages = messages;
                envelope.next_actions = next_actions;
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::log_tty(&data));
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
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::keep_as_yours_describe_tty(&data, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        ops::KeepAsYoursOutcome::Forked(data) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                outln!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                outln!("{}", render::add_tty(&data));
            }
            ExitCode::SUCCESS
        }
    }
}

/// `add <reference>`'s finisher — the applied row receipt, or a git source's describe (with its
/// `--yes` argv).
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
                // A workspace reference resolves to whatever the catalog says it is, so the undo's
                // caution is read off the RECEIPT, not assumed: an `add @ws/linear` of an MCP
                // bundle came through here and borrowed a skill's sentence about folders.
                envelope.next_actions = render::undo_next_actions(
                    &data.undo,
                    crate::actions::Subject::of_receipt(&data),
                );
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::add_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::AddRefOutcome::Described { data, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::add_describe_tty(&data, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// The `add --kind mcp` finisher — an APPLY RECEIPT that leads with the undo, like every other ungated
/// arm; the typed `mcp` block on the payload carries the server facts a JSON consumer reads.
fn finish_add_mcp(
    json: bool,
    command: &str,
    result: Result<ops::McpAdded, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::McpAdded {
            data,
            messages,
            reached_nobody,
        }) => {
            let (ok, failed) = mcp_add_verdict(reached_nobody, &messages);
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.ok = ok;
                envelope.warnings = crate::message::legacy_lines(&messages);
                envelope.messages = messages;
                // The receipt is a SERVER's: its undo takes a server off this machine, and the
                // caution has to say so rather than borrowing a skill's sentence.
                envelope.next_actions =
                    render::undo_next_actions(&data.undo, crate::actions::Subject::McpServer);
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::add_tty(&data));
            }
            if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `(ok, exit-is-failure)` for an `add --kind mcp` — the ONE derivation both surfaces read.
///
/// The row is durable, so `ok` follows DELIVERY: an add that reached no agent at all is not a
/// success with a note about it. A PARTIAL reach keeps `ok: true` — some agent has the server —
/// and still exits non-zero, the sweep's own rule, so an agent watching a loop learns the run is
/// not converging instead of reading 0 forever.
fn mcp_add_verdict(reached_nobody: bool, messages: &[topos_types::Message]) -> (bool, bool) {
    (!reached_nobody, !messages.is_empty())
}

/// The multi-`add` finisher — one `add` receipt per imported (skill × harness) combination.
fn finish_add_many(
    json: bool,
    command: &str,
    result: Result<ops::AddManyOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        // A git source describes before it installs — the same two-phase receipt the bare
        // reference arm prints, selectors and all.
        Ok(ops::AddManyOutcome::Described { data, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::add_describe_tty(&data, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::AddManyOutcome::Applied(items)) => {
            if json {
                let value = serde_json::json!({ "added": items });
                outln!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                let body = items
                    .iter()
                    .map(render::add_tty)
                    .collect::<Vec<_>>()
                    .join("\n");
                outln!("{body}");
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
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::remove_describe_tty(&data, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::RemoveOutcome::Applied(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                // Same rule as `add`: the removal's own rows say what left this machine, so the
                // caution names servers when that is what they were. A mixed removal keeps the
                // skill sentence — it is the one that speaks for folders on disk.
                envelope.next_actions = render::undo_next_actions(
                    &data.undo,
                    crate::actions::Subject::of_removal(&data.uninstalled),
                );
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::remove_applied_tty(&data));
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
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::reset_describe_tty(&items, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::ResetOutcome::Applied(items)) => {
            if json {
                let value = serde_json::json!({ "items": items });
                let mut envelope = render::ok_envelope(command, value);
                // A merge this reset left STANDING carries its two ways out, exactly as the
                // conflicted row does — the state that still asks for a decision must offer that
                // decision on the machine surface too.
                envelope.next_actions = render::reset_next_actions(&items);
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::reset_applied_tty(&items));
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
                outln!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                outln!("{}", render::invite_read_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::InviteOutcome::Described { describe, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": describe });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::invite_describe_tty(&describe, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::InviteOutcome::Applied(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                outln!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                outln!("{}", render::invite_tty(&data));
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
                outln!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                outln!("{}", render::review_inbox_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::ReviewOutcome::Describe { data, next_argvs }) => {
            if json {
                let mut next_actions = render::describe_next_actions(next_argvs.clone());
                let mut messages = Vec::new();
                // A byte-capped describe diff adds the full-fidelity escape: the SAME
                // `current..<proposal>` diff through `topos diff`, uncapped — carrying the
                // invocation's `--workspace` so an ambiguous name re-resolves identically.
                if data.diff_truncated
                    && let Some((_, hash)) = data.proposal.rsplit_once('@')
                {
                    messages.push(crate::message::diff_truncated(true));
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
                envelope.warnings = crate::message::legacy_lines(&messages);
                envelope.messages = messages;
                envelope.next_actions = next_actions;
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::review_describe_tty(&data, &next_argvs));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::ReviewOutcome::Applied(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                outln!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                outln!("{}", render::review_tty(&data));
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
        Ok(ops::ProtectOutcome::Described { data, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::protect_describe_tty(&data, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::ProtectOutcome::Applied(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                outln!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                outln!("{}", render::protect_applied_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `uninstall`'s finisher — the two-phase pair (describe / applied), plus the teardown that died
/// partway and still owes a receipt for what it removed.
fn finish_uninstall(
    json: bool,
    command: &str,
    result: Result<ops::UninstallOutcome, Box<ops::UninstallFailure>>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::UninstallOutcome::Described { describe, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": describe });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::uninstall_describe_tty(&describe, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::UninstallOutcome::Applied { applied, messages }) => {
            // ONE binding for `ok`, the exit status and the headline: a teardown that left an
            // agent's trigger armed did not uninstall topos, and the three surfaces must not be
            // able to disagree about that.
            let failed = !messages.is_empty();
            if json {
                let value = serde_json::to_value(&applied).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.ok = !failed;
                envelope.warnings = crate::message::legacy_lines(&messages);
                envelope.messages = messages;
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!(
                    "{}",
                    render::uninstall_applied_tty(&applied, &messages, !failed)
                );
            }
            if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        // A teardown that FAILED still owes the receipt of the destructive work it already did —
        // the same rows the success receipt spells. The envelope carries it as `data`, so a JSON
        // consumer reads the same document on both paths; the TTY prints it under the refusal.
        Err(failure) => {
            let ops::UninstallFailure { error, partial } = *failure;
            let logged = diag.note(command, &error);
            if json {
                let mut envelope = render::err_envelope(command, diag.argv, &error);
                if let Some(applied) = &partial {
                    envelope.data = serde_json::to_value(applied).unwrap_or_default();
                }
                outln!("{}", render::to_json(&envelope));
            } else {
                errln!("{}", render::err_tty(&error));
                if let Some(applied) = &partial {
                    errln!("{}", render::uninstall_applied_tty(applied, &[], false));
                }
                if let Some(hint) = render::err_hint_tty(command, diag.argv, &error) {
                    errln!("{hint}");
                }
                if logged {
                    errln!("details: {}", diag.log_path.display());
                }
            }
            ExitCode::FAILURE
        }
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
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::revert_describe_tty(&data, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::RevertOutcome::NoOp(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                outln!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                outln!("{}", render::revert_noop_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::RevertOutcome::Applied(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                outln!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                outln!("{}", render::revert_tty(&data));
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
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::session_login_tty(&data));
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
                outln!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                outln!("{}", render::session_logout_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

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
        errln!(
            "Open: {}\nCode: {} (the page shows the same code — confirm it matches)",
            disc.verification_uri,
            disc.user_code,
        );
    }
    errln!(
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
            errln!("Opening your browser to approve.");
        } else {
            // The open failed; the flow is unharmed. This URL needs no typing and returns the
            // approval straight to this terminal — and approving anywhere else still completes
            // here on the next poll, so nothing is stranded either way.
            errln!(
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
    errln!("{}", waiting_line(&disc.user_code));
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
/// the paste-ready `--yes` next-action. A copy already at `current` has nothing to preview, so it
/// settles through the same already-published document the apply prints. A typed refusal
/// (CONSENT_MISMATCH / …) flows through [`emit_err`].
fn finish_publish_describe(
    json: bool,
    command: &str,
    result: Result<ops::PublishPreview, ClientError>,
    yes_argv: Vec<String>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::PublishPreview::Describe(data)) => {
            if json {
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                outln!("{}", render::to_json(&envelope));
            } else {
                outln!("{}", render::publish_describe_tty(&data, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::PublishPreview::NoChanges(data)) => emit_publish_no_changes(json, command, &data),
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// The already-published answer, on both surfaces: exit 0, no error envelope. Its ONE offered
/// command is the publish of the other scope's copy — present exactly when that copy holds edits
/// this run did not ship, so the success never ends a session with unshared work unmentioned.
fn emit_publish_no_changes(
    json: bool,
    command: &str,
    data: &topos_types::results::PublishNoChangesData,
) -> ExitCode {
    if json {
        let value = serde_json::to_value(data).unwrap_or_default();
        let mut envelope = render::ok_envelope(command, value);
        envelope.next_actions = render::publish_no_changes_actions(data);
        outln!("{}", render::to_json(&envelope));
    } else {
        outln!("{}", render::publish_no_changes_tty(data));
    }
    ExitCode::SUCCESS
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
                outln!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                outln!("{}", render::publish_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::PublishOutcome::Proposed(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                outln!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                outln!("{}", render::propose_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::PublishOutcome::NoChanges(data)) => emit_publish_no_changes(json, command, &data),
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
            errln!("topos {command} [{}]: {}", err.code(), err.detail());
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
        outln!(
            "{}",
            render::to_json(&render::err_envelope(command, diag.argv, err))
        );
    } else {
        errln!("{}", render::err_tty(err));
        // The way out, between the refusal and the pointer: the SAME next actions the `--json`
        // envelope computes, as runnable lines (and the transience clause where the typed outcome
        // says the failure is retryable). One source of truth — never a second hint table. The
        // verb rides along: an ambiguity's ways out ARE this invocation, re-spelled per candidate.
        if let Some(hint) = render::err_hint_tty(command, diag.argv, err) {
            errln!("{hint}");
        }
        // Point a human at the detail the fixed message withheld — only when it actually landed.
        if logged {
            errln!("details: {}", diag.log_path.display());
        }
    }
    ExitCode::FAILURE
}

/// Parse the optional `pull` target into a [`ops::PullScope`]: absent = the sweep; `<name>` = accept a
/// pending update; `<name>@<ref>` = go back to that version's bytes; `--keep-mine` = the escape.
///
/// A go-back suffix is recognized when the part after the LAST `@` is a version reference — the full
/// 64-char lowercase-hex id, or a short prefix of at least 8 hex chars (resolved against the skill's
/// recorded history, so a pasted 12-char short form from any topos output works). Otherwise the whole
/// argument is the skill name: `team@cli` is accepted as a name, and `team@cli@<ref>` still goes back
/// correctly. A hex suffix SHORTER than 8 chars stays part of the name (never a silent go-back).
///
/// `--keep-mine` (the escape) requires a `<skill>` target and is mutually exclusive with `@<ref>`
/// (both runtime usage errors — the suffix shape is only known
/// after parsing). A skill LITERALLY named like `docs@abcdef12` is not lost to the go-back parse:
/// [`pull_with_name_fallback`] retries the whole argument as the name when the pre-@ part resolves to
/// no tracked skill.
///
/// `store` is the invocation's scope flag ([`ops::StoreScope`]) — carried onto the targeted scope so
/// the mode resolves (and acts) in the store the flag named.
fn build_pull_scope(
    skill: Option<String>,
    keep_mine: bool,
    store: ops::StoreScope,
) -> Result<ops::PullScope, ClientError> {
    let Some(arg) = skill else {
        if keep_mine {
            return Err(ClientError::InvalidArgument(
                "--keep-mine requires a <skill> target".into(),
            ));
        }
        return Ok(ops::PullScope::AllFollowed);
    };
    if let Some((name, suffix)) = arg.rsplit_once('@')
        && let Some(vref) = ops::VersionRef::recognize(suffix)
    {
        if keep_mine {
            return Err(ClientError::InvalidArgument(
                "--keep-mine is not valid with @<ref>".into(),
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
        mode: if keep_mine {
            ops::TargetMode::KeepMine
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
    keep_mine: bool,
    store: ops::StoreScope,
) -> Result<ops::PullOutcome, ClientError> {
    let arg = skill.clone();
    let first = build_pull_scope(skill, keep_mine, store).and_then(|scope| ops::pull(ctx, scope));
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

/// `add <path> --as <bundle>` — the grammar half of the identity claim, at the composition root
/// where the argv still is what the person typed.
///
/// The claim is about a FOLDER and about a bundle that is ALREADY added here, and each of the
/// three flags it refuses would be asking for something else entirely: `-s` picks members out of a
/// repository, `--kind` says what a NEW bundle is, and `-a`/`--dest` ask for copies somewhere the
/// claim is not about. Refusing by name beats silently ignoring a flag someone typed on purpose.
#[allow(clippy::too_many_arguments)]
fn claim_arm(
    ctx: &Ctx<'_>,
    source: &str,
    as_bundle: &str,
    skill: &[String],
    kind: Option<crate::bundle_kind::BundleKind>,
    selection: &ops::Selection,
    workspace: Option<&str>,
    global: bool,
) -> Result<topos_types::results::AddData, ClientError> {
    let path = claim_grammar(source, as_bundle, skill, kind, selection, workspace)?;
    let scope = ops::add_scope(ctx, global)?;
    ops::claim(ctx, &scope, &path, as_bundle)
}

/// A REDUNDANT ROW is only the whole story when the delivery moved nothing either. The row write
/// knows the file did not change; only the narrowed update that follows knows whether folders did,
/// and a receipt leading with "nothing changed" over a copy that just landed would be answering
/// about the file while the disk changed under it.
///
/// Best-effort exactly as the update itself is: an update that could not run proves no movement,
/// and the row — the durable demand — is what the receipt reports either way.
fn clear_unchanged_if_bytes_moved(
    data: &mut topos_types::results::AddData,
    outcome: Result<ops::PullOutcome, ClientError>,
) {
    if !data.unchanged {
        return;
    }
    if outcome.is_ok_and(|o| {
        o.data
            .skills
            .iter()
            .any(|r| r.skill == data.name && ops::moved_bytes(r.action))
    }) {
        data.unchanged = false;
    }
}

/// The PURE half of [`claim_arm`]: the folder a `--as` invocation is about, or the refusal its
/// argv already earns — decided from the tokens alone, so nothing is read before a wrong command
/// is answered.
fn claim_grammar(
    source: &str,
    as_bundle: &str,
    skill: &[String],
    kind: Option<crate::bundle_kind::BundleKind>,
    selection: &ops::Selection,
    workspace: Option<&str>,
) -> Result<PathBuf, ClientError> {
    let crate::source::SourceSpec::LocalPath(path) = crate::source::classify(source) else {
        return Err(ClientError::InvalidArgument(format!(
            "`--as` names the bundle a FOLDER is already a copy of — spell '{source}' as a path \
             (`topos add {} --as {as_bundle}`)",
            crate::error::path_token(source)
        )));
    };
    if !skill.is_empty() {
        return Err(ClientError::InvalidArgument(
            "`-s` picks which skills to take from a repository that holds several — `--as` names \
             one folder on this machine; drop `-s`"
                .into(),
        ));
    }
    if kind.is_some() {
        return Err(ClientError::InvalidArgument(
            "`--kind` says what a NEW bundle is — `--as` names one that is already added, and its \
             kind was recorded when it was; drop `--kind`"
                .into(),
        ));
    }
    if !selection.is_empty() {
        return Err(ClientError::SelectionRefused(
            "`--as` brings the folder you named under management — `-a`/`--dest` ask for copies \
             somewhere else; run them as two commands"
                .into(),
        ));
    }
    if workspace.is_some() {
        return Err(ClientError::InvalidArgument(
            "`--workspace` picks which workspace an ambient command means — `--as` names a bundle \
             this scope already manages, and reads no catalog; drop `--workspace`"
                .into(),
        ));
    }
    Ok(path)
}

/// The breadth arming sweep, run at the composition root — the one layer holding the real ports
/// (`RealFs` is both the `ConfigStore` and the `CommandRunner`) and the resolved machine roots.
/// `None` roots (no `$HOME`) arms nothing: detection needs a home, and the active adapter's own
/// trigger was already armed by the verb.
fn breadth_arm(
    roots: &Option<crate::ctx::AgentRoots>,
    active: &dyn HarnessAdapter,
    fs: &RealFs,
) -> Vec<topos_types::TriggerReport> {
    match roots {
        Some(r) => ops::arm_detected(&r.home, r.cwd.as_deref(), active.id().slug(), fs, fs),
        None => Vec::new(),
    }
}

/// Build the PLACEMENT adapter for `id`. Adding a harness is ONE new match arm — no caller change.
/// v0 only ever selects Claude Code (the CLI's one selection site above passes
/// `HarnessId::ClaudeCode`; each adapter resolves its own config home: `$CLAUDE_CONFIG_DIR` else
/// `$HOME/.claude`; `$HOME/.openclaw`; `$HERMES_HOME` else `$HOME/.hermes`). The OpenClaw and
/// Hermes arms serve the test rigs while pilot verification stays open (each module's doc).
fn adapter_for<'a>(
    id: HarnessId,
    fs: &'a dyn ConfigStore,
    cli: &'a dyn topos_harness::CommandRunner,
) -> Box<dyn HarnessAdapter + 'a> {
    match id {
        // Claude Code's placement half needs no ports at all — it reads dirs and writes nothing.
        HarnessId::ClaudeCode => Box::new(ClaudeCode::new(ClaudeCode::resolve_home())),
        // These two carry BOTH ports on one type, so they still hold the seam their trigger half
        // drives: OpenClaw's own CLI, and Hermes's config surface.
        HarnessId::OpenClaw => Box::new(OpenClaw::new(OpenClaw::resolve_home(), cli)),
        HarnessId::Hermes => Box::new(topos_harness::Hermes::new(
            topos_harness::Hermes::resolve_home(),
            topos_harness::Hermes::resolve_accept_hooks(),
            fs,
        )),
    }
}

/// Build the TRIGGER adapter for `id`, borrowing the shared config-store seam plus the subprocess
/// runner (OpenClaw's trigger drives its own `openclaw` CLI). It goes through
/// [`topos_harness::triggers::adapter_for_slug`] — the ONE place a harness's trigger machinery is
/// named — over the real user home, so each adapter resolves its own harness root exactly as
/// detection resolves it. Every `HarnessId` names a trigger-capable registry row (the crate's own
/// view test pins that), so the factory always answers.
fn trigger_for<'a>(
    id: HarnessId,
    fs: &'a dyn ConfigStore,
    cli: &'a dyn topos_harness::CommandRunner,
) -> Box<dyn TriggerAdapter + 'a> {
    triggers::adapter_for_slug(id.slug(), &registry::real_home(), fs, cli)
        .expect("every selectable harness is a trigger-capable registry row")
}

/// `$TOPOS_HOME`, else `$HOME/.topos` (`./.topos` as a last resort) — through the SAME root
/// resolution the detection roots take (`crate::ctx::resolved_root`).
///
/// One boundary for both, because they are compared against each other lexically: the sidecar paths
/// this home builds (`~/.topos/topos.toml`, the log) are printed by stripping the detection home's
/// prefix, and a `$HOME` carrying a symlink resolved on one side only made that strip miss — the
/// machine file then printed as an absolute path on every surface that names it.
fn resolve_home() -> PathBuf {
    let home = std::env::var_os("TOPOS_HOME").map_or_else(
        || {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".topos")
        },
        PathBuf::from,
    );
    crate::ctx::resolved_root(home)
}

/// The discovery roots for `list`/`add`: the user home (every harness's global skill dir resolves
/// under it) + the current project dir (repo-scoped skills). A missing `$HOME` degrades to no
/// discovery rather than an error.
fn list_discovery() -> Option<ops::DiscoveryRoots> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    // Through the ctx roots' own constructor: discovery compares these paths against tracked
    // placements and prints them, so they take the one spelling rule and take it exactly once.
    let roots = crate::ctx::AgentRoots::new(home, std::env::current_dir().ok());
    Some(ops::DiscoveryRoots {
        home: roots.home,
        cwd: roots.cwd,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_WEB_ORIGIN, PendingDisclosure, WaitPolicy, block_on_pending, build_pull_scope,
        claim_grammar, document_flag, list_page_argv, next_page_action, parse_rfc3339_utc_millis,
        resolve_web_origin, waiting_line,
    };
    use std::process::ExitCode;

    use crate::ids::Clock;
    use crate::ops::{self, PullScope, RowPage, StoreScope, TargetMode, VersionRef};

    struct TestClock(u64);
    impl Clock for TestClock {
        fn now_unix_millis(&self) -> u64 {
            self.0
        }
    }

    /// The document flags pick their text (and their precedence), and neither one is chosen when
    /// neither was asked for — the ordinary run must never be swallowed by this interception.
    #[test]
    fn the_document_flags_pick_their_text_and_stay_out_of_the_way() {
        let cli = |skill: bool, schema: bool| crate::cli::Cli {
            json: false,
            workspace: None,
            skill,
            schema,
            command: None,
        };
        assert!(document_flag(&cli(false, false)).is_none());
        // Exactly the embedded bytes, minus the trailing newline `outln!` supplies.
        let skill = document_flag(&cli(true, false)).expect("--skill answers");
        assert_eq!(
            format!("{skill}\n"),
            ops::builtin_skill_md(),
            "--skill re-prints the embedded SKILL.md"
        );
        let schema = document_flag(&cli(false, true)).expect("--schema answers");
        let parsed: serde_json::Value =
            serde_json::from_str(&schema).expect("--schema is one JSON document");
        assert!(parsed["schemas"]["json-envelope"].is_object());
        // Both set: the teaching document wins.
        assert_eq!(document_flag(&cli(true, true)).as_deref(), Some(&*skill));
    }

    #[test]
    fn a_claim_takes_a_folder_and_refuses_every_flag_that_asks_for_something_else() {
        let none = ops::Selection::default();
        // A FOLDER, and the path spelling is handed back runnable.
        assert_eq!(
            claim_grammar("./skills/review", "review", &[], None, &none, None).unwrap(),
            std::path::PathBuf::from("./skills/review")
        );
        // A bare name (or a reference) is not a folder — the refusal spells the path form.
        let err = claim_grammar("review", "review", &[], None, &none, None).unwrap_err();
        assert!(
            err.to_string().contains("`topos add ./review --as review`"),
            "{err}"
        );
        // Each of the four flags asks a DIFFERENT question, and says so rather than being
        // ignored: a repo member pick, a NEW bundle's kind, copies somewhere else, and which
        // workspace an ambient command means — none of which a claim about one folder is.
        for err in [
            claim_grammar("./x", "review", &["one".to_owned()], None, &none, None).unwrap_err(),
            claim_grammar(
                "./x",
                "review",
                &[],
                Some(crate::bundle_kind::BundleKind::Skill),
                &none,
                None,
            )
            .unwrap_err(),
            claim_grammar(
                "./x",
                "review",
                &[],
                None,
                &ops::Selection::new(&["codex".to_owned()], &[]),
                None,
            )
            .unwrap_err(),
            claim_grammar("./x", "review", &[], None, &none, Some("w_eng")).unwrap_err(),
        ] {
            assert!(err.to_string().contains("--as"), "{err}");
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

    /// The one thing an agent reads without being asked: the exit status. It has to distinguish
    /// the two states a sweep can end in without erroring, because they call for opposite
    /// responses — a FAULT is worth retrying or escalating, and a DECISION is worth stopping and
    /// asking a person. A status of 0 on both taught an agent that a run which never converges is
    /// a run that succeeded.
    #[test]
    fn a_pending_decision_exits_zero_and_a_real_failure_does_not() {
        let fs = crate::fs_seam::RealFs;
        let clock = TestClock(0);
        let argv = ["update".to_owned()];
        let diag = super::Diag {
            fs: &fs,
            clock: &clock,
            log_path: std::env::temp_dir().join("topos-exit-code-test-never-written.jsonl"),
            argv: &argv,
        };
        let empty = || topos_types::results::PullData {
            skills: Vec::new(),
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: Some("machine".to_owned()),
        };
        let outcome = |decisions: Vec<crate::ops::PendingDecision>,
                       warnings: Vec<topos_types::Message>| {
            Ok(ops::PullOutcome {
                // One failed BUNDLE per fault line, keyed the way the sweep keys them:
                // `(scope label, bundle identity)`. The fixture's faults are all one scope's.
                failed_bundles: warnings
                    .iter()
                    .map(|w| ("person".to_owned(), w.text.clone()))
                    .collect(),
                data: empty(),
                warnings,
                fault_actions: Vec::new(),
                decisions,
                advisories: Vec::new(),
                disclosures: Vec::new(),
                access_gone: Vec::new(),
                unreachable: Vec::new(),
                stale_forge: Vec::new(),
                forge_gone: Vec::new(),
                failed_channels: std::collections::HashSet::new(),
            })
        };
        let decision = crate::ops::PendingDecision {
            name: "find-skills".to_owned(),
            line: "github.com/vercel-labs/skills has a newer version, but your edits would be \
                   overwritten"
                .to_owned(),
            detail: vec!["to share your edits:   topos publish find-skills".to_owned()],
            ways_out: vec![vec![
                "topos".to_owned(),
                "publish".to_owned(),
                "find-skills".to_owned(),
            ]],
        };

        // A clean sweep, and a sweep that only ever asked a question: both are successes.
        assert_eq!(
            super::finish_pull(
                false,
                "update",
                outcome(Vec::new(), Vec::new()),
                true,
                &diag
            ),
            ExitCode::SUCCESS
        );
        assert_eq!(
            super::finish_pull(
                false,
                "update",
                outcome(vec![decision.clone()], Vec::new()),
                true,
                &diag
            ),
            ExitCode::SUCCESS
        );
        // A real fault is not, on either face — and a decision standing beside it does not soften
        // the status either.
        assert_eq!(
            super::finish_pull(
                false,
                "update",
                outcome(
                    Vec::new(),
                    vec![crate::message::failure(
                        "IO_ERROR",
                        "deploy: the store is unreadable.".to_owned()
                    )]
                ),
                true,
                &diag
            ),
            ExitCode::FAILURE
        );
        assert_eq!(
            super::finish_pull(
                true,
                "update",
                outcome(
                    vec![decision],
                    vec![crate::message::failure(
                        "IO_ERROR",
                        "deploy: the store is unreadable.".to_owned()
                    )]
                ),
                true,
                &diag
            ),
            ExitCode::FAILURE
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
