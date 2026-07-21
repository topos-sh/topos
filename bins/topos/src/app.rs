//! The composition root for the binary: parse argv, wire the real seams, run recovery, dispatch a verb,
//! and emit either the `--json` envelope (stdout) or thin TTY text.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use serde::Serialize;
use topos_harness::{ClaudeCode, ConfigStore, HarnessAdapter, OpenClaw};
use topos_types::HarnessId;

use crate::cli::{AuthCmd, Cli, Command};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::{FsOps, RealFs};
use crate::git_source::GitTarballSource;
use crate::ids::{Clock, RealClock, RealIds};
use crate::plane::{
    ContributeSource, DirectorySource, EnrollSource, GovernanceSource, ReconcileTransport,
};
use crate::plane_http::{FileFollow, UreqDeviceClient, UreqPlane};
use crate::sidecar::{Layout, recover};
use crate::{enroll, identity, logfile, ops, render};

/// Run the CLI; returns the process exit code. A thin wrapper over the dispatch: AFTER a successful
/// eligible command it runs the passive version check (stderr-only, self-throttled to at most one
/// probe per day, silent on every failure — `ops::version_check` owns the policy). The check never
/// runs on the quiet sweep (the session-start hook path gains zero latency and zero noise), on
/// `self-update`/`upgrade` (they ARE the release surface), on `uninstall` (it would recreate the
/// state dir the command just deleted), or on a failed command.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    // No subcommand: on a TTY, orient (the status snapshot / the unenrolled welcome, exit 0);
    // piped or under `--json`, keep the classic usage error (stderr, exit 2) so scripts fail
    // loudly. The version nag never rides the bare invocation — orientation stays offline.
    let Some(command) = cli.command else {
        return run_bare(cli.json, cli.workspace);
    };
    let check = version_check_applies(&command) && ops::version_check_env_allows();
    let code = run_command(cli.json, cli.workspace, command, false);
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
            | Command::Status
    )
}

/// The bare `topos` invocation (no subcommand). A TTY gets the orientation render — the same
/// snapshot `topos status` computes, with the unenrolled welcome as its short form — and exit 0.
/// Anything scripted (piped stdout, or `--json`) keeps the classic clap usage error on stderr with
/// exit 2, so automation that forgot a verb still fails loudly.
fn run_bare(json: bool, workspace: Option<String>) -> ExitCode {
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
    run_command(false, workspace, Command::Status, true)
}

/// Parse-free dispatch: wire the real seams, run recovery, dispatch the verb, emit the outcome.
/// `bare` marks the no-subcommand TTY orientation (it only softens `status`'s render on a fresh
/// machine — the welcome instead of the full snapshot).
fn run_command(json: bool, workspace: Option<String>, command: Command, bare: bool) -> ExitCode {
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
    let workspace = enroll::canonicalize_workspace_flag(&fs, &layout, workspace);
    // The error-side diagnostics channel: every failure that reaches a finisher below lands its full
    // detail in the append-only log the redacted user surfaces point at.
    let diag = Diag {
        fs: &fs,
        clock: &clock,
        log_path: layout.log_path(),
    };
    // The harness adapter, selected through the one dispatch seam. v0 wires Claude Code only; the
    // adapter touches its config home only when adopting a recognized skill, arming auto-updates, or on
    // uninstall.
    let harness = adapter_for(HarnessId::ClaudeCode, &fs, &fs);

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
    if let Command::Status = &command {
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
        };
        // The snapshot from local state; the trigger rows from the read-only probe at this root
        // (the one layer holding the real config port + $HOME) — the same layering the arming
        // receipts use, minus every write.
        let result = ops::status_snapshot(&ctx).map(|mut data| {
            if let Some(r) = &ctx.roots {
                data.triggers =
                    ops::probe_detected(&r.home, r.cwd.as_deref(), harness.as_ref(), &fs);
            }
            data
        });
        return finish_status(json, cmd_name, result, bare);
    }

    // Recovery runs at the start of every command (it also abandons an expired, never-redeemed
    // enrollment WAL against the real wall clock).
    let now_millis = i64::try_from(clock.now_unix_millis()).unwrap_or(i64::MAX);
    if let Err(e) = recover(&fs, &layout, now_millis) {
        return emit_err(json, cmd_name, &e, &diag);
    }

    // The plane + follow-state sources. When enrollment has been written (`instance.json` present —
    // `follow` writes it), wire the REAL `ureq` transport + the on-disk follow state; otherwise keep the
    // INERT pair (a never-enrolled install stays a truthful no-op). The inert ZSTs and the loaded
    // enrollment are both stack locals so the `&dyn` trait objects below borrow correctly for the rest of
    // `run`.
    let inert_plane = crate::plane::InertPlane;
    let inert_follow = crate::plane::InertFollow;
    let enrollment = match load_enrollment(&fs, &layout) {
        Ok(e) => e,
        Err(e) => return emit_err(json, cmd_name, &e, &diag),
    };
    let (plane, follow): (
        &dyn crate::plane::PlaneSource,
        &dyn crate::plane::FollowSource,
    ) = match &enrollment {
        Some(e) => (&e.plane, &e.follow),
        None => (&inert_plane, &inert_follow),
    };

    // `add` and `pull` author commits (adoption / a draft snapshot before a divergence), so both load
    // (and on first use, mint) the device identity — `uninstall` must never create or require it before
    // tearing the home down.
    let device_id = match &command {
        // `follow` also loads (and on first use mints) the device identity: its skill path authors a
        // draft snapshot through the pull engine. `publish` authors the candidate commit and `revert`
        // the forward-revert commit, so both load (and on first use mint) the device id.
        Command::Add { .. }
        | Command::Update { .. }
        | Command::Follow { .. }
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
    };

    // The credentialed device connectors — every credentialed route presents the device's ONE Bearer
    // credential. Built as closures so `credentials.json` is re-read FRESH on each build: a `follow
    // <address>` mints it mid-invocation (at the granted poll), and the continued describe/apply must
    // see the freshly-minted credential.
    let connect_governance = |base_url: &str| -> Box<dyn GovernanceSource> {
        Box::new(UreqDeviceClient::new(
            base_url.to_owned(),
            load_device_credential(ctx.fs, &ctx.layout),
        ))
    };
    let connect_contribute = |base_url: &str| -> Box<dyn ContributeSource> {
        Box::new(UreqDeviceClient::new(
            base_url.to_owned(),
            load_device_credential(ctx.fs, &ctx.layout),
        ))
    };
    // The DIRECTORY connector (describe reads + subscription/curation/notice row ops) and the
    // RECONCILE connector (delivery + fleet report + the per-skill read lane on one object) — both
    // re-read the on-disk credential fresh per build, for the same mid-invocation reason as above.
    let connect_directory = |base_url: &str| -> Box<dyn DirectorySource> {
        Box::new(UreqDeviceClient::new(
            base_url.to_owned(),
            load_device_credential(ctx.fs, &ctx.layout),
        ))
    };
    let connect_delivery = |base_url: &str| -> Box<dyn ReconcileTransport> {
        let follows = enroll::read_follows(ctx.fs, &ctx.layout)
            .ok()
            .flatten()
            .unwrap_or_else(|| enroll::Follows {
                schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
                follows: Vec::new(),
            });
        let workspaces: Vec<String> = enroll::read_user(ctx.fs, &ctx.layout)
            .ok()
            .flatten()
            .map(|u| u.workspaces.into_iter().map(|m| m.workspace_id).collect())
            .unwrap_or_default();
        Box::new(
            UreqPlane::new(
                base_url.to_owned(),
                load_device_credential(ctx.fs, &ctx.layout),
                enroll::skill_workspaces(&follows),
            )
            .with_workspaces(workspaces),
        )
    };
    // The default WEB origin the enrollment doors dial on a fresh install (`follow <bare-ws>`,
    // `auth login`): the env override, else the hosted web origin (the card re-roots onto the API).
    let web_origin = resolve_web_origin(std::env::var("TOPOS_PLANE_URL").ok());

    match command {
        // Dispatched BEFORE state recovery above — the read-only promise admits no sweep write.
        Command::Status => unreachable!("status dispatches before state recovery"),
        Command::Add {
            source,
            skill,
            agent,
            global,
            yes,
        } => {
            let has_star = skill.iter().chain(agent.iter()).any(|v| v == "*");
            // MULTI selectors (`-s a -s b`, `-a x -a y`) and the `*` fan-outs apply to a REMOTE import —
            // loop the single-select path per (skill × harness) combination, disclosing each landing. `-s *`
            // expands to every skill in the repo; `-a *` to every harness DETECTED on this machine.
            if has_star || skill.len() > 1 || agent.len() > 1 {
                let result = add_multi(&ctx, &source, &skill, &agent, global);
                return finish_add_many(json, cmd_name, result, &diag);
            }
            let single_skill = skill.into_iter().next();
            let single_agent = agent.into_iter().next();
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
            let result = match crate::source::classify(&source) {
                crate::source::SourceSpec::LocalPath(p) => ops::add(&ctx, &p),
                crate::source::SourceSpec::LocalName(name) => match list_discovery(false) {
                    // Adopt the resolved dir UNDER its resolved name — so `list`/`add`/`publish`/`diff`
                    // agree on the name even for a harness the active adapter does not recognize.
                    Some(roots) => ops::resolve_add_target(&ctx, &roots, &name)
                        .and_then(|(p, n)| ops::add_with_name(&ctx, &p, Some(&n))),
                    None => Err(ClientError::InvalidArgument(
                        "cannot resolve a skill name without $HOME set — adopt a directory by \
                         path (`topos add ./<dir>`)"
                            .into(),
                    )),
                },
                crate::source::SourceSpec::Remote(spec) => match list_discovery(false) {
                    Some(roots) => {
                        let git = crate::plane_http::UreqGitSource::new();
                        ops::add_remote(
                            &ctx,
                            &git,
                            &spec,
                            &roots,
                            &ops::AddRemoteOpts {
                                skill: single_skill,
                                harness: single_agent,
                                global,
                            },
                        )
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
        Command::Follow {
            targets,
            channel,
            skill,
            agent,
            yes,
            prefix_dirname,
            manual,
            wait,
        } => {
            // The transports are built per-base-URL (known only after the op parses the target /
            // the card / the WAL): the shared creds-free `ureq` enroll connector, the directory
            // (describe + rows) and the reconcile transport.
            // The bareword-enroll consent prompt: only a real TTY (stdin AND stderr) may ask;
            // `--json` and piped runs answer Headless, which the op turns into the typed refusal
            // naming `--yes` and the full address form. The prompt rides stderr (stdout stays the
            // clean render).
            let confirm_bareword = |name: &str, server: &str| -> ops::BarewordDecision {
                use std::io::{IsTerminal, Write};
                if json || !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
                    return ops::BarewordDecision::Headless;
                }
                eprint!(
                    "'{name}' looks like a workspace on {server} — enroll this device with it? [y/N] "
                );
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                if std::io::stdin().read_line(&mut line).is_err() {
                    return ops::BarewordDecision::Headless;
                }
                match line.trim() {
                    "y" | "Y" | "yes" | "Yes" | "YES" => ops::BarewordDecision::Proceed,
                    _ => ops::BarewordDecision::Declined,
                }
            };
            let connectors = ops::FollowConnectors {
                enroll: &connect_enroll,
                directory: &connect_directory,
                delivery: &connect_delivery,
                web_origin: web_origin.clone(),
                confirm_bareword: &confirm_bareword,
            };
            let mk_opts = || ops::FollowOpts {
                manual,
                // The global `--workspace` disambiguates a positional skill name shared across the
                // workspaces this install follows on one plane (the enrollment motions ignore it).
                workspace: workspace.clone(),
                yes,
                prefix_dirname,
                channels: channel.clone(),
                skills: skill.clone(),
                agents: agent.clone(),
            };
            let first = ops::follow(&ctx, &connectors, targets, mk_opts());
            // Block on a pending device-authorization until the human approves (so a person never re-invokes
            // `follow` by hand), unless this is a headless `--json` run without `--wait` (which must not
            // hang). The interactive block only ever RESUMES (no targets + the pending WAL drives it) —
            // and the resumed invocation keeps THIS invocation's flags, so a `--yes` carries through
            // the wait into the apply. A PIPED stdout without an explicit `--wait` never blocks:
            // the instructions print, the single poll answers, and the exit-0 pending document
            // carries the resume next-action — an agent harness would otherwise stare at a silent
            // long poll it cannot see into.
            let stdout_tty = {
                use std::io::IsTerminal;
                std::io::stdout().is_terminal()
            };
            let policy = WaitPolicy::resolve(json, wait, stdout_tty, &clock);
            // The zero-typing loopback: only for an interactive blocking wait (the listener must
            // outlive the human's click — a TTY, or an explicit `--wait` on a pipe), only when a
            // local browser is plausible (never over SSH / headless / `TOPOS_NO_BROWSER`), and
            // only when a pending WAL exists to derive the flow's challenge from. Everything else
            // keeps the typed-code fallback untouched.
            let loopback_plan = if policy.block && !json {
                let interactive = {
                    use std::io::IsTerminal;
                    std::io::stderr().is_terminal()
                };
                ops::loopback::choose_browser(&ops::loopback::BrowserEnv::detect(
                    interactive,
                    stdout_tty,
                    wait.is_some(),
                ))
                .and_then(|opener| {
                    let wal = crate::enroll::read_wal(&fs, &ctx.layout).ok().flatten()?;
                    Some(LoopbackPlan {
                        opener,
                        runner: &fs,
                        challenge: ops::device_challenge(&wal.device_code),
                    })
                })
            } else {
                None
            };
            let result = block_on_pending(
                &clock,
                &policy,
                first,
                follow_pending_disclosure,
                loopback_plan,
                || ops::follow(&ctx, &connectors, Vec::new(), mk_opts()),
            );
            // The breadth arming sweep rides the enrollment receipt: the promote armed the active
            // adapter (`currency`); every OTHER detected agent's trigger is armed here at the
            // composition root, and the per-agent outcomes ride the same payload honestly.
            let result = result.map(|outcome| match outcome {
                ops::FollowOutcome::Data { mut data, resumed } => {
                    if data.currency.is_some() {
                        data.triggers = breadth_arm(&ctx.roots, harness.as_ref(), &fs);
                        // The built-in `topos` skill lands with the enrollment (best-effort).
                        if let Err(e) = ops::ensure_builtin(&ctx) {
                            let _ = diag.note(cmd_name, &e);
                        }
                    }
                    ops::FollowOutcome::Data { data, resumed }
                }
                other => other,
            });
            finish_follow(json, cmd_name, result, &diag)
        }
        Command::Unfollow {
            targets,
            channel,
            skill,
            agent,
            yes,
        } => {
            // `unfollow --agent <slug>` is the per-agent exclusion — the SAME implementation
            // `remove --agent` runs on a followed skill: offline placement policy (the subscription
            // is untouched, no server call), so it dispatches before the networked detach path.
            if !agent.is_empty() {
                if !channel.is_empty() {
                    let e = ClientError::InvalidArgument(
                        "`--agent` scopes where a SKILL's bytes land — it cannot combine with \
                         `--channel`"
                            .into(),
                    );
                    return emit_err(json, cmd_name, &e, &diag);
                }
                let mut all_targets = targets.clone();
                all_targets.extend(skill.iter().cloned());
                let result = ops::exclude_agents(
                    &ctx,
                    "unfollow",
                    &all_targets,
                    &agent,
                    workspace.as_deref(),
                    yes,
                );
                return finish_agent_scope(json, cmd_name, result, &diag);
            }
            let connectors = ops::UnfollowConnectors {
                directory: &connect_directory,
                delivery: &connect_delivery,
            };
            let result = ops::unfollow(&ctx, &connectors, &targets, &channel, &skill, yes);
            finish_unfollow(json, cmd_name, result, &diag)
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
            remote,
            tracked,
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
                &name,
                remote,
                tracked,
                footprint,
                &channel,
                &skill,
                workspace.as_deref(),
            );
            // The full row filter: positional names + the `--channel`/`--skill` selectors. A single bare
            // name keeps the classic exactly-one narrowing; richer forms resolve ALL-OR-NONE (an unmatched
            // name refuses the whole invocation) and filter the tracked rows.
            let filter = ops::ListFilter {
                names: name,
                channels: channel,
                skills: skill,
            };
            // Under `--remote`, resolve the enrolled plane + memberships (a typed "run follow first" when
            // there is no enrollment), then build the credentialed catalog transport as a local so the
            // scope borrows it across the `list()` call. The transport holds the per-workspace credential
            // map (each catalog read presents its workspace's Bearer credential).
            let remote_inputs = if remote {
                match list_remote_inputs(&fs, &ctx.layout) {
                    Ok(inputs) => Some(inputs),
                    Err(e) => return emit_err(json, cmd_name, &e, &diag),
                }
            } else {
                None
            };
            let catalog_client;
            let scope = if let Some((base_url, memberships)) = remote_inputs {
                catalog_client =
                    UreqDeviceClient::new(base_url, load_device_credential(&fs, &ctx.layout));
                Some(ops::RemoteScope {
                    catalog: &catalog_client,
                    memberships,
                    only: workspace.clone(),
                })
            } else {
                None
            };
            finish_list(
                json,
                cmd_name,
                ops::list_with(
                    &ctx,
                    &filter,
                    footprint,
                    list_discovery(tracked),
                    scope,
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
            let roots = list_discovery(false);
            // A bare ENROLLED publish DESCRIBES what shipping would do (nothing lands on the plane); `--yes`
            // applies. An un-enrolled publish refuses typed inside the op (enroll with `follow` first).
            if !yes && enrollment.is_some() {
                let connectors = ops::PublishDescribeConnectors {
                    directory: &connect_directory,
                    delivery: &connect_delivery,
                };
                let described = ops::publish_describe(
                    &ctx,
                    &connectors,
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
            channel,
            skill,
            reset,
            yes,
            onto_current,
            quiet,
            ttl,
        } => {
            // The bare sweep prefers the delivery-driven reconcile when enrolled (one delivery
            // call per workspace answers "what should this device have"); every targeted form and
            // the un-enrolled state keep the classic engine. `--reset` is a two-phase discard (below);
            // the `--channel`/`--skill` selectors + multi-target resolution land later.
            let delivery = enrollment
                .as_ref()
                .map(|e| &e.plane as &dyn crate::plane::DeliverySource);
            // The notices posture: an interactive or `--json` update ACKS what it returns (the
            // narration/data carries them); the quiet hook fetches WITHOUT acking — nothing is
            // marked read that no one saw.
            let reconcile_opts = ops::ReconcileOpts {
                ack_notices: !quiet,
                ..ops::ReconcileOpts::default()
            };
            // `--reset` is its own two-phase discard verb (loss-led describe / `--yes` apply); it does not
            // flow through the update engine and is never a `--quiet` hook shape.
            if reset {
                let mut reset_targets = targets.clone();
                reset_targets.extend(skill.iter().cloned());
                let _ = &channel;
                return finish_reset(json, cmd_name, ops::reset(&ctx, &reset_targets, yes), &diag);
            }
            // A `--channel`/`--skill` selector or more than one positional is the SELECTOR / MULTI-TARGET
            // update: resolve every name through the grammar all-or-none, then the targeted path per skill
            // and the channel-filtered sync per channel. A single bare target (or none) keeps the classic
            // engine — the go-back `<skill>@<hash>` and the `--onto-current` escape live only there.
            let has_selectors = !channel.is_empty() || !skill.is_empty() || targets.len() > 1;
            // The BARE sweep is the hook shape (the auto-update triggers all run `update --quiet`).
            // Hooks now fire on every session-start-shaped event, so the quiet path passes a
            // self-throttle gate BEFORE any engine or network work: single-flight (another sweep
            // in flight → silent no-op) + TTL (a completed sweep within the window → silent
            // no-op; `--ttl`/`TOPOS_UPDATE_TTL`/default 300 s, `0` disables). An explicit
            // non-quiet sweep always runs but takes the same lock (never two concurrent sweeps)
            // and refreshes the stamp.
            let bare_sweep = !has_selectors && targets.is_empty();
            let now_ms = i64::try_from(clock.now_unix_millis()).unwrap_or(i64::MAX);
            let mut _sweep_guard = None;
            if bare_sweep {
                if quiet {
                    match ops::quiet_gate(&fs, &ctx.layout, now_ms, ops::resolve_ttl_ms(ttl)) {
                        Ok(ops::QuietGate::Run(guard)) => _sweep_guard = Some(guard),
                        // Skipped (fresh, or another sweep in flight): byte-silent success — the
                        // whole point is that redundant hook fires cost nothing. The reason stays
                        // a typed value (tests pin it) but is deliberately not narrated anywhere.
                        Ok(ops::QuietGate::Skip(reason)) => {
                            let _ = reason;
                            return ExitCode::SUCCESS;
                        }
                        // A gate I/O failure is a LOCAL failure — surface it (nonzero), exactly
                        // like any other local quiet failure.
                        Err(e) => return emit_err(false, cmd_name, &e, &diag),
                    }
                } else {
                    match ops::sweep_lock(&fs, &ctx.layout) {
                        Ok(guard) => _sweep_guard = Some(guard),
                        Err(e) => return emit_err(json, cmd_name, &e, &diag),
                    }
                }
            }
            // The bare sweep also re-syncs the BUILT-IN `topos` skill (create/refresh/converge —
            // force-synced to this binary; the durable opt-out is honored inside). Best-effort: a
            // built-in hiccup must never block the team sweep. Its byte changes count toward the
            // quiet hook's `reloadSkills` below.
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
            let result = if has_selectors {
                if onto_current {
                    Err(ClientError::InvalidArgument(
                        "--onto-current takes a single <skill> target, not selectors or several targets"
                            .into(),
                    ))
                } else {
                    ops::update_selective(
                        &ctx,
                        &connect_directory,
                        delivery,
                        &targets,
                        &channel,
                        &skill,
                        workspace.as_deref(),
                    )
                }
            } else {
                let target = targets.into_iter().next();
                // A TARGETED update of the built-in skill has no served pointer to sync against —
                // refuse toward the verbs that do move it (the name is reserved, so this can never
                // shadow a followed skill).
                if target
                    .as_deref()
                    .is_some_and(|t| ops::is_builtin(t.split('@').next().unwrap_or(t)))
                {
                    Err(ClientError::InvalidArgument(
                        "`topos` is the built-in skill — the bare `topos update` re-syncs it to \
                         this binary; `topos self-update` updates the binary itself"
                            .into(),
                    ))
                } else {
                    pull_with_name_fallback(&ctx, target, onto_current, delivery, &reconcile_opts)
                }
            };
            // A COMPLETED bare sweep stamps the TTL clock (best-effort) — success, or the quiet
            // path's soft failure (an unreachable plane must not be re-dialed on every session
            // event; the staleness warning still fires once the window blows). A hard local
            // failure leaves the old stamp so the next session retries.
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
                // NEAR-byte-silent stdout (a session-start hook's stdout reaches the session):
                // a clean no-change sweep emits nothing; a sweep that CHANGED skill bytes emits the
                // ONE SessionStart hook-output JSON (`reloadSkills`, so Claude Code re-scans its
                // skill dirs same-session — other harnesses ignore hook stdout by construction),
                // with the two facts a person must not miss — an access-gone freeze, and
                // unreachable-AND-stale — riding its context injection; without changes those
                // facts stay ONE plain line each. An auth/transport failure warns and exits 0
                // (the hook must never fail a session start for a network blip); a genuinely
                // local failure still surfaces on stderr with a non-zero exit.
                let now = i64::try_from(clock.now_unix_millis()).unwrap_or(i64::MAX);
                match result {
                    Ok(out) => {
                        let lines = ops::quiet_hook_lines(&fs, &ctx.layout, now, &out);
                        if ops::sweep_changed_bytes(&out.data) || builtin_changed {
                            println!("{}", ops::reload_skills_json(&lines));
                        } else {
                            for line in lines {
                                println!("{line}");
                            }
                        }
                        ExitCode::SUCCESS
                    }
                    Err(e) if ops::quiet_soft_failure(&e) => {
                        // The detail still lands in the diagnostics log; stdout gets one honest line.
                        let _ = diag.note(cmd_name, &e);
                        println!("topos: update skipped — {}", render::safe_message(&e));
                        ExitCode::SUCCESS
                    }
                    Err(e) => emit_err(false, cmd_name, &e, &diag),
                }
            } else {
                finish_pull(json, cmd_name, result, enrollment.is_some(), &diag)
            }
        }
        Command::Remove { skill, agent, yes } => {
            let connectors = ops::RemoveConnectors {
                directory: &connect_directory,
            };
            let roots = list_discovery(false);
            let result = ops::remove(&ctx, &connectors, &skill, &agent, roots.as_ref(), yes);
            finish_remove(json, cmd_name, result, &diag)
        }
        // `channel add|remove <channel> <skill>...` dispatches to the two-phase op; a bare `channel` (or an
        // unrecognized subword / `create`) teaches usage without touching the network.
        Command::Channel { args, yes } => match args.first().map(String::as_str) {
            Some("add") | Some("remove") => {
                let connectors = ops::ChannelConnectors {
                    directory: &connect_directory,
                };
                let result = ops::channel(&ctx, &connectors, &args, workspace.as_deref(), yes);
                finish_channel(json, cmd_name, result, &diag)
            }
            _ => emit_err(json, cmd_name, &channel_seam(&args), &diag),
        },
        Command::Protect { target, level, yes } => {
            let connectors = ops::ProtectConnectors {
                directory: &connect_directory,
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
            let releases = connect_releases();
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
            let connect_auth_governance = |base_url: &str| -> Box<dyn GovernanceSource> {
                Box::new(UreqDeviceClient::new(
                    base_url.to_owned(),
                    load_device_credential(ctx.fs, &ctx.layout),
                ))
            };
            let connectors = ops::AuthConnectors {
                enroll: &connect_enroll,
                directory: &connect_directory,
                governance: &connect_auth_governance,
                web_origin: web_origin.clone(),
            };
            match cmd {
                AuthCmd::Login { server_url, wait } => {
                    let first = ops::login(
                        &ctx,
                        &connectors,
                        server_url.as_deref(),
                        workspace.as_deref(),
                    );
                    // The same blocking idiom as `follow`: a TTY (or `--wait`) run re-polls
                    // until the browser approval settles; a `--json` or PIPED run without
                    // `--wait` returns the pending state and never hangs.
                    let stdout_tty = {
                        use std::io::IsTerminal;
                        std::io::stdout().is_terminal()
                    };
                    let policy = WaitPolicy::resolve(json, wait, stdout_tty, &clock);
                    let result = block_on_pending(
                        &clock,
                        &policy,
                        first,
                        login_pending_disclosure,
                        None,
                        || {
                            ops::login(
                                &ctx,
                                &connectors,
                                server_url.as_deref(),
                                workspace.as_deref(),
                            )
                        },
                    );
                    finish_login(json, cmd_name, result, &diag)
                }
                AuthCmd::Logout { yes } => {
                    finish_logout(json, cmd_name, ops::logout(&ctx, &connectors, yes), &diag)
                }
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

/// Map a NON-`add`/`remove` `topos channel …` invocation to its typed refusal: a bare `channel` (or an
/// unrecognized subword) teaches channel-first usage; `create` is recognized as a hint keyword — channels
/// are created on first placement, never as a standalone verb. (`add`/`remove` dispatch to the op above.)
fn channel_seam(args: &[String]) -> ClientError {
    match args.first().map(String::as_str) {
        Some("create") => ClientError::InvalidArgument(
            "channels are created on first placement — run `topos channel add <channel> <skill>` (a \
             new channel is created automatically); there is no separate `channel create`"
                .into(),
        ),
        _ => ClientError::InvalidArgument(
            "usage: `topos channel add <channel> <skill>...` or `topos channel remove <channel> \
             <skill>...` (a channel is created on first placement)"
                .into(),
        ),
    }
}

/// The ENROLLMENT connector: `UreqDeviceClient` with NO credential — the device-flow routes are
/// unauthenticated (they mint the credential the other connectors then present). The credentialed
/// connectors are closures in [`run`] (they must re-read `credentials.json` fresh so an enrollment
/// that just persisted mid-invocation is seen).
fn connect_enroll(base_url: &str) -> Box<dyn EnrollSource> {
    Box::new(UreqDeviceClient::new(base_url.to_owned(), None))
}

/// Load the device's ONE Bearer credential from `credentials.json`. Best-effort (absent / corrupt ⇒
/// `None`): a corrupt doc already failed the startup [`load_enrollment`] closed, and a missing
/// credential surfaces downstream as a clear "not enrolled" at request time.
fn load_device_credential(fs: &dyn FsOps, layout: &Layout) -> Option<String> {
    enroll::read_credentials(fs, layout)
        .ok()
        .flatten()
        .map(|c| c.credential)
}

/// The real release source for `topos upgrade` — the `ureq` GitHub transport. No base URL / creds: the
/// updater's default download base is compiled in (overridable via `TOPOS_INSTALL_BASE_URL`).
fn connect_releases() -> Box<dyn crate::release::ReleaseSource> {
    Box::new(crate::plane_http::UreqReleases::new())
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
) -> ExitCode {
    match result {
        Ok(data) => {
            if json {
                let next_actions = if data.enrolled {
                    Vec::new()
                } else {
                    vec![crate::actions::next_action(
                        topos_types::ActionCode::from("FOLLOW_WORKSPACE".to_owned()),
                        vec![
                            "topos".to_owned(),
                            "follow".to_owned(),
                            "<workspace-address>".to_owned(),
                            "--json".to_owned(),
                        ],
                    )]
                };
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = next_actions;
                println!("{}", render::to_json(&envelope));
            } else if bare && !data.enrolled {
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
                println!("{}", render::to_json(&render::err_envelope(command, &e)));
            } else {
                eprintln!("{}", render::err_tty(&e));
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
                        topos_types::ActionCode::from("FOLLOW_WORKSPACE".to_owned()),
                        vec![
                            "topos".to_owned(),
                            "follow".to_owned(),
                            "<workspace-address>".to_owned(),
                            "--json".to_owned(),
                        ],
                    ));
                }
                let value = serde_json::to_value(&out.data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.warnings = out.warnings;
                envelope.next_actions = next_actions;
                println!("{}", render::to_json(&envelope));
            } else {
                let mut text = render::pull_tty(&out.data, &out.warnings);
                if unenrolled_dead_end {
                    text.push_str(
                        "\nNot enrolled — join your team with `topos follow \
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
/// selector preserved: the positional names, the mode flags, the repeatable `--channel`/`--skill`
/// selectors, AND the global `--workspace` (already canonicalized to an id; the flag accepts it) —
/// so the next page enumerates exactly the same view, never a widened one.
fn list_page_argv(
    names: &[String],
    remote: bool,
    tracked: bool,
    footprint: bool,
    channels: &[String],
    skills: &[String],
    workspace: Option<&str>,
) -> Vec<String> {
    let mut argv = vec!["topos".to_owned(), "list".to_owned()];
    argv.extend(names.iter().cloned());
    for (flag, on) in [
        ("--remote", remote),
        ("--tracked", tracked),
        ("--footprint", footprint),
    ] {
        if on {
            argv.push(flag.to_owned());
        }
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

/// Resolve the `list --remote` inputs from the on-disk enrollment: the pinned plane base URL
/// (`instance.json`) + every joined workspace as `(workspace_id, display_label)` (`user.json`). Not
/// enrolled (no `instance.json` / no membership) ⇒ a typed, friendly "run `topos follow` first" (the same
/// not-enrolled shape the write verbs use), never a panic.
///
/// # Errors
/// [`ClientError::Enrollment`] when there is no plane or no membership to read a catalog from.
fn list_remote_inputs(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<(String, Vec<(String, String)>), ClientError> {
    let instance = enroll::read_instance(fs, layout)?.ok_or(ClientError::NotEnrolled)?;
    let memberships: Vec<(String, String)> = enroll::read_user(fs, layout)?
        .map(|u| {
            u.workspaces
                .into_iter()
                .map(|m| {
                    let label = m.display_name;
                    (m.workspace_id, label)
                })
                .collect()
        })
        .unwrap_or_default();
    if memberships.is_empty() {
        return Err(ClientError::NotEnrolled);
    }
    Ok((instance.base_url, memberships))
}

/// `follow`'s finisher — the three surfaces: the classic wire payload (with the success-path
/// `next_actions` — re-invoke `follow` while pending; `update` once offers are disclosed), the
/// two-phase DESCRIBE (`data.describe` + the paste-ready `--yes` argvs), and the apply report
/// (its reconcile warnings ride the envelope's `warnings`).
fn finish_follow(
    json: bool,
    command: &str,
    result: Result<ops::FollowOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::FollowOutcome::Data { data, resumed }) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::follow_next_actions(&data);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::follow_tty(&data, &resumed));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::FollowOutcome::Described {
            describe,
            next_argvs,
        }) => {
            if json {
                let value = serde_json::json!({ "describe": describe });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(next_argvs);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::follow_describe_tty(&describe, &next_argvs));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::FollowOutcome::Applied(applied)) => {
            if json {
                let warnings = applied.warnings.clone();
                let value = serde_json::to_value(&applied).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.warnings = warnings;
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::follow_applied_tty(&applied));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::FollowOutcome::Scope(outcome)) => {
            finish_agent_scope(json, command, Ok(outcome), diag)
        }
        Ok(ops::FollowOutcome::ReattachDescribed { reattach, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "reattach": reattach });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::reattach_describe_tty(&reattach, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::FollowOutcome::ReattachApplied(reattach)) => {
            if json {
                let warnings = reattach.warnings.clone();
                let value = serde_json::json!({ "reattach": reattach });
                let mut envelope = render::ok_envelope(command, value);
                envelope.warnings = warnings;
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::reattach_applied_tty(&reattach));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `unfollow`'s finisher — the two-phase pair (describe / applied).
/// The `--agent` scope verbs' finisher (shared by `follow --agent`, `unfollow --agent`, and
/// `remove --agent` on a followed skill) — the same describe/apply envelope shape as every other
/// two-phase verb.
fn finish_agent_scope(
    json: bool,
    command: &str,
    result: Result<ops::AgentScopeOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::AgentScopeOutcome::Described { data, yes_argv }) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::agent_scope_tty(&data, Some(&yes_argv)));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::AgentScopeOutcome::Applied(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::agent_scope_tty(&data, None));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

fn finish_unfollow(
    json: bool,
    command: &str,
    result: Result<ops::UnfollowOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::UnfollowOutcome::Described { describe, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": describe });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::unfollow_describe_tty(&describe, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::UnfollowOutcome::Applied(applied)) => {
            if json {
                let value = serde_json::to_value(&applied).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::unfollow_applied_tty(&applied));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// Import a REMOTE source into several skills and/or several harness dirs — the `-s a -s b` / `-a x -a y`
/// loop over the single-select [`ops::add_remote`] path (disclosing each landing). Multi selectors apply
/// to a remote import only (a local path/name adopts exactly one skill). A `*` selector fans out:
/// `-s '*'` imports EVERY skill of a multi-skill repo, `-a '*'` places into every harness DETECTED on
/// this machine (the same registry discovery `list` uses, filtered to those with a skills dir at the
/// chosen scope).
///
/// # Errors
/// [`ClientError::InvalidArgument`] for a non-remote source, a missing `$HOME`, or `-a '*'` matching no
/// detected harness; [`ClientError::NoSkillInSource`] for `-s '*'` on a repo with no skill; whatever
/// [`ops::add_remote`] returns for a given combination (all-or-error).
fn add_multi(
    ctx: &Ctx<'_>,
    source: &str,
    skills: &[String],
    agents: &[String],
    global: bool,
) -> Result<Vec<topos_types::results::AddData>, ClientError> {
    let spec = match crate::source::classify(source) {
        crate::source::SourceSpec::Remote(spec) => spec,
        _ => {
            return Err(ClientError::InvalidArgument(
                "multiple `-s`/`-a` selectors (or `*`) apply to a REMOTE import (`owner/repo` or a \
                 github.com URL) — a local path or name adopts a single skill"
                    .into(),
            ));
        }
    };
    let Some(roots) = list_discovery(false) else {
        return Err(ClientError::InvalidArgument(
            "cannot import a remote skill without $HOME set (needed to resolve the harness skills dir)"
                .into(),
        ));
    };
    let git = crate::plane_http::UreqGitSource::new();
    // `-a '*'` → every DETECTED harness with a skills dir at the chosen scope; explicit values loop as-is.
    let agent_opts: Vec<Option<String>> = if agents.iter().any(|a| a == "*") {
        let detected = detected_harness_slugs(&roots, global);
        if detected.is_empty() {
            return Err(ClientError::InvalidArgument(
                "`-a '*'` found no harness on this machine to place into at the chosen scope — name one \
                 with `-a <slug>` (or drop `--global` for project scope)"
                    .into(),
            ));
        }
        detected.into_iter().map(Some).collect()
    } else if agents.is_empty() {
        vec![None]
    } else {
        agents.iter().cloned().map(Some).collect()
    };
    // `-s '*'` → every skill in the repo (fetch + extract once to enumerate the names).
    let skill_opts: Vec<Option<String>> = if skills.iter().any(|s| s == "*") {
        let targz = git.fetch(&spec)?;
        let repo = crate::git_source::extract_tree(&targz)?;
        let names = repo.skill_names(spec.subdir.as_deref(), &spec.repo);
        if names.is_empty() {
            return Err(ClientError::NoSkillInSource { src: spec.label() });
        }
        names.into_iter().map(Some).collect()
    } else if skills.is_empty() {
        vec![None]
    } else {
        skills.iter().cloned().map(Some).collect()
    };
    let mut out = Vec::with_capacity(skill_opts.len() * agent_opts.len());
    for s in &skill_opts {
        for a in &agent_opts {
            out.push(ops::add_remote(
                ctx,
                &git,
                &spec,
                &roots,
                &ops::AddRemoteOpts {
                    skill: s.clone(),
                    harness: a.clone(),
                    global,
                },
            )?);
        }
    }
    Ok(out)
}

/// The harness slugs DETECTED on this machine (the same registry discovery `list` uses) that have a
/// skills directory at the chosen scope — the fan-out set for `add -a '*'`. Deduped + sorted; a harness
/// with no writable dir at this scope is dropped (so the loop never fails on it).
fn detected_harness_slugs(roots: &ops::DiscoveryRoots, global: bool) -> Vec<String> {
    let scope = if global {
        topos_harness::registry::SkillScope::User
    } else {
        topos_harness::registry::SkillScope::Project
    };
    let mut slugs: Vec<String> =
        topos_harness::registry::discover_all(&roots.home, roots.cwd.as_deref())
            .into_iter()
            .map(|d| d.harness_slug)
            .filter(|slug| {
                topos_harness::registry::skills_root(slug, scope, &roots.home, roots.cwd.as_deref())
                    .is_some()
            })
            .collect();
    slugs.sort();
    slugs.dedup();
    slugs
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

/// The multi-`add` finisher — one `add` receipt per imported (skill × harness) combination.
fn finish_add_many(
    json: bool,
    command: &str,
    result: Result<Vec<topos_types::results::AddData>, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(items) => {
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
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::remove_applied_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::RemoveOutcome::AgentScope(outcome)) => {
            finish_agent_scope(json, command, Ok(outcome), diag)
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `channel add|remove`'s finisher — the two-phase pair.
fn finish_channel(
    json: bool,
    command: &str,
    result: Result<ops::ChannelOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::ChannelOutcome::Described { data, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::channel_describe_tty(&data, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::ChannelOutcome::Applied(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::channel_applied_tty(&data));
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
        Ok(ops::ProtectOutcome::Described { data, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::protect_describe_tty(&data, &yes_argv));
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

/// `auth login`'s finisher — pending (the device-flow wait) or done (the per-workspace report).
fn finish_login(
    json: bool,
    command: &str,
    result: Result<ops::AuthLoginOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::AuthLoginOutcome::Pending(p)) => {
            if json {
                let value = serde_json::json!({ "pending": p });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::login_pending_next_actions();
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::login_pending_tty(&p));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::AuthLoginOutcome::Done(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::login_done_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `auth logout`'s finisher — the two-phase pair.
fn finish_logout(
    json: bool,
    command: &str,
    result: Result<ops::AuthLogoutOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(ops::AuthLogoutOutcome::Described { describe, yes_argv }) => {
            if json {
                let value = serde_json::json!({ "describe": describe });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::logout_describe_tty(&describe, &yes_argv));
            }
            ExitCode::SUCCESS
        }
        Ok(ops::AuthLogoutOutcome::Applied(data)) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                println!("{}", render::to_json(&render::ok_envelope(command, value)));
            } else {
                println!("{}", render::logout_applied_tty(&data));
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
fn follow_pending_disclosure(out: &ops::FollowOutcome) -> Option<PendingDisclosure> {
    match out {
        ops::FollowOutcome::Data { data, .. } => data.pending.as_ref().map(|p| PendingDisclosure {
            verification_uri: p.verification_uri.clone(),
            user_code: p.user_code.clone(),
            interval_secs: p.interval_secs,
            expires_at_millis: p.expires_at.as_deref().and_then(parse_rfc3339_utc_millis),
        }),
        _ => None,
    }
}

/// The pending disclosure for an `auth login` outcome (None ⇒ the sign-in settled).
fn login_pending_disclosure(out: &ops::AuthLoginOutcome) -> Option<PendingDisclosure> {
    match out {
        ops::AuthLoginOutcome::Pending(p) => Some(PendingDisclosure {
            verification_uri: p.verification_uri.clone(),
            user_code: p.user_code.clone(),
            interval_secs: Some(p.interval_secs),
            expires_at_millis: p.expires_at.as_deref().and_then(parse_rfc3339_utc_millis),
        }),
        ops::AuthLoginOutcome::Done(_) => None,
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

/// One `m:ss` (or `h:mm:ss`) spelling of a millisecond span, floored at zero — the waiting line's
/// honest time.
fn fmt_span(millis: i64) -> String {
    let secs = millis.max(0) / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The live waiting line: elapsed time since the wait began, plus the code's expiry countdown when
/// known. Rewritten in place on a TTY; plain otherwise.
fn waiting_line(now_millis: u64, started_millis: u64, expires_at_millis: Option<i64>) -> String {
    let mut line = format!(
        "Waiting for approval… {} elapsed",
        fmt_span(i64::try_from(now_millis.saturating_sub(started_millis)).unwrap_or(i64::MAX))
    );
    if let Some(exp) = expires_at_millis {
        let left = exp.saturating_sub(i64::try_from(now_millis).unwrap_or(i64::MAX));
        line.push_str(&format!(" · code expires in {}", fmt_span(left)));
    }
    line
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
}

/// Block on a pending device-authorization until it settles, an optional deadline passes, or the device
/// code's own expiry ends it with a terminal error — so a person never re-invokes the command by hand. The
/// disclosure prints to STDERR once (stdout stays the clean final render). `pending_of` extracts the
/// disclosure (None ⇒ not pending ⇒ return as-is); `repoll` re-invokes the op, which RESUMES via its
/// on-disk WAL. A `policy.block == false` (headless `--json` without `--wait`) returns the first result
/// untouched — a headless agent must not hang.
///
/// With a `loopback` plan, the wait ALSO binds an ephemeral 127.0.0.1 listener and auto-opens the
/// approval page carrying the state-bound return coordinates — the browser's redirect wakes the
/// next poll immediately (zero typing); the poll stays the source of truth, so a failed open or a
/// lost redirect degrades to the typed-code wait already printed above it.
fn block_on_pending<T>(
    clock: &dyn Clock,
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
    eprintln!(
        "Open: {}\nCode: {} (the page shows the same code — confirm it matches)",
        disc.verification_uri, disc.user_code,
    );
    eprintln!(
        "Ctrl-C is safe — the same command resumes this enrollment; `--wait <seconds>` caps the \
         wait."
    );
    // The loopback arm: bind, auto-open, and let the redirect wake the poll. Every fault here is
    // silent — the typed-code lines above already carry the whole ceremony.
    let mut listener = loopback.and_then(|plan| {
        let state = uuid::Uuid::new_v4().simple().to_string();
        let bound = ops::loopback::LoopbackListener::bind(state.clone()).ok()?;
        let url = ops::loopback::approval_url(
            &disc.verification_uri,
            &plan.challenge,
            bound.port(),
            &state,
        );
        let opened = plan
            .runner
            .run(plan.opener, &[url.as_str()])
            .map(|out| out.success)
            .unwrap_or(false);
        if opened {
            eprintln!("Opening your browser to approve — or use the URL and code above.");
        }
        opened.then_some(bound)
    });
    let interval = disc.poll_interval();
    // The live waiting line: honest time (elapsed + the code's expiry countdown), rewritten in
    // place once a second when stderr is a TTY; a single plain line otherwise (no escape codes in
    // a log file).
    let live = {
        use std::io::IsTerminal;
        std::io::stderr().is_terminal()
    };
    let started = clock.now_unix_millis();
    if !live {
        eprintln!("{}", waiting_line(started, started, disc.expires_at_millis));
    }

    // `last` is the most recent pending result, handed back verbatim if a numeric `--wait` deadline passes
    // (starts as `first`, so `--wait 0` returns immediately without polling again).
    let mut last = first;
    let finish_line = |live: bool| {
        if live {
            eprintln!();
        }
    };
    loop {
        // Honor a numeric deadline precisely: stop the instant it passes (checked BEFORE sleeping, so a
        // short `--wait <n>` is not overshot by a whole poll interval).
        if policy
            .deadline_millis
            .is_some_and(|d| clock.now_unix_millis() >= d)
        {
            finish_line(live);
            return last;
        }
        // Sleep the poll interval, but never past the deadline — ticking the live line per second.
        let nap = match policy.deadline_millis {
            Some(d) => {
                Duration::from_millis(d.saturating_sub(clock.now_unix_millis())).min(interval)
            }
            None => interval,
        };
        if live {
            let nap_end = clock
                .now_unix_millis()
                .saturating_add(u64::try_from(nap.as_millis()).unwrap_or(u64::MAX));
            // With a listener armed, tick in short slices so the browser's redirect wakes the
            // poll near-instantly; a lone terminal keeps the calm one-second cadence.
            let slice = if listener.is_some() {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(1)
            };
            loop {
                let now = clock.now_unix_millis();
                if now >= nap_end {
                    break;
                }
                if let Some(bound) = &listener
                    && bound.try_receive().is_some()
                {
                    // The redirect landed (single-use — stop listening) → poll NOW.
                    listener = None;
                    break;
                }
                use std::io::Write;
                eprint!("\r{}  ", waiting_line(now, started, disc.expires_at_millis));
                let _ = std::io::stderr().flush();
                std::thread::sleep(Duration::from_millis(nap_end.saturating_sub(now)).min(slice));
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
            finish_line(live);
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
    result: Result<topos_types::results::PublishDescribeData, ClientError>,
    yes_argv: Vec<String>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(data) => {
            if json {
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(vec![yes_argv.clone()]);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::publish_describe_tty(&data, &yes_argv));
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
        println!("{}", render::to_json(&render::err_envelope(command, err)));
    } else {
        eprintln!("{}", render::err_tty(err));
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
fn build_pull_scope(
    skill: Option<String>,
    onto_current: bool,
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
    delivery: Option<&dyn crate::plane::DeliverySource>,
    reconcile: &ops::ReconcileOpts,
) -> Result<ops::PullOutcome, ClientError> {
    let arg = skill.clone();
    let first = build_pull_scope(skill, onto_current).and_then(|scope| match (&scope, delivery) {
        (ops::PullScope::AllFollowed, Some(d)) => ops::pull_reconcile_with(ctx, d, reconcile),
        _ => ops::pull(ctx, scope),
    });
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
                },
            )
        }
        other => other,
    }
}

/// The real plane wiring, present only when enrollment has been written. Owns the transport + the on-disk
/// follow source so [`run`] can borrow them as `&dyn` trait objects for the lifetime of the command.
struct Enrollment {
    plane: UreqPlane,
    follow: FileFollow,
}

/// Load the enrollment docs read-only. Returns `Some` whenever `instance.json` is present — enrollment is
/// what writes it, so its presence IS the enrolled state; `follows.json` is optional (an empty membership
/// door, or every follow since flipped off by `unfollow`). The read transport carries the ONE device
/// credential (`credentials.json`) plus the follow-state's skill → workspace map — a signed-out install
/// (no credential) reads nothing (never a request without a credential). The transport stays wired even
/// with zero active follows: the write verbs (publish/revert/review) still need the plane base, and an
/// enrolled author with nothing followed is a normal state. The bare `pull` stays an honest no-op either
/// way (the sweep skips a `following == false` entry, and renders "No followed skills." over an empty
/// set). A corrupt / newer-schema doc (incl. a permissive `credentials.json`) fails closed (propagated),
/// never silently degraded to inert.
fn load_enrollment(fs: &dyn FsOps, layout: &Layout) -> Result<Option<Enrollment>, ClientError> {
    let Some(instance) = enroll::read_instance(fs, layout)? else {
        return Ok(None);
    };
    let follows = enroll::read_follows(fs, layout)?.unwrap_or_else(|| enroll::Follows {
        schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
        follows: Vec::new(),
    });
    let credential = enroll::read_credentials(fs, layout)?.map(|c| c.credential);
    let workspaces: Vec<String> = enroll::read_user(fs, layout)?
        .map(|u| u.workspaces.into_iter().map(|m| m.workspace_id).collect())
        .unwrap_or_default();
    let plane = UreqPlane::new(
        instance.base_url,
        credential,
        enroll::skill_workspaces(&follows),
    )
    .with_workspaces(workspaces);
    let follow = FileFollow::new(enroll::follow_contexts(&follows));
    Ok(Some(Enrollment { plane, follow }))
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

/// The discovery roots for `list`: `None` under `--tracked` (skip discovery), else the user home (every
/// harness's global skill dir resolves under it) + the current project dir (repo-scoped skills). A missing
/// `$HOME` degrades to no discovery rather than an error.
fn list_discovery(tracked: bool) -> Option<ops::DiscoveryRoots> {
    if tracked {
        return None;
    }
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
        fmt_span, list_page_argv, next_page_action, parse_rfc3339_utc_millis, resolve_web_origin,
        waiting_line,
    };
    use crate::ids::Clock;
    use crate::ops::{PullScope, RowPage, TargetMode, VersionRef};

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
    fn the_waiting_line_shows_honest_elapsed_and_expiry_time() {
        // The RFC 3339 parser inverts the client's own spelling exactly.
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

        assert_eq!(fmt_span(7_000), "0:07");
        assert_eq!(fmt_span(15 * 60_000 - 7_000), "14:53");
        assert_eq!(fmt_span(3_600_000 + 61_000), "1:01:01");
        assert_eq!(fmt_span(-5), "0:00", "a passed expiry never goes negative");

        let line = waiting_line(1_000_000 + 7_000, 1_000_000, Some(1_007_000 + 893_000));
        assert_eq!(
            line,
            "Waiting for approval… 0:07 elapsed · code expires in 14:53"
        );
        // No expiry known → elapsed only (never a guessed countdown).
        assert_eq!(
            waiting_line(1_000_000 + 7_000, 1_000_000, None),
            "Waiting for approval… 0:07 elapsed"
        );
    }

    #[test]
    fn list_page_argv_preserves_every_selector_including_workspace() {
        // The NEXT_PAGE continuation must re-spell the WHOLE view — dropping `--workspace` would
        // widen a narrowed `--remote` catalog to every joined workspace on the next page.
        let argv = list_page_argv(
            &["docs".to_owned()],
            true,
            false,
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
                "docs",
                "--remote",
                "--channel",
                "eng",
                "--skill",
                "deploy",
                "--workspace",
                "w_acme",
            ]
        );
        // The bare local list re-spells bare.
        assert_eq!(
            list_page_argv(&[], false, false, false, &[], &[], None),
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
            build_pull_scope(Some(full), false).unwrap(),
            PullScope::One { name, mode: TargetMode::GoBack(VersionRef::Full(_)), .. } if name == "docs"
        ));
        // A pasted 12-char short form is a go-back too — no more silent NO_SUCH_SKILL degradation.
        assert!(matches!(
            build_pull_scope(Some("docs@ab12cd34ef56".to_owned()), false).unwrap(),
            PullScope::One { name, mode: TargetMode::GoBack(VersionRef::Prefix(p)), .. }
                if name == "docs" && p == "ab12cd34ef56"
        ));
        // A hex-ish suffix SHORTER than the prefix floor stays part of the name (a name may contain `@`),
        // as does any non-hex suffix.
        for name in ["docs@ab12", "team@cli"] {
            assert!(matches!(
                build_pull_scope(Some(name.to_owned()), false).unwrap(),
                PullScope::One { name: n, mode: TargetMode::AcceptPending, .. } if n == name
            ));
        }
        // The escape never combines with a go-back ref.
        let err = build_pull_scope(Some("docs@ab12cd34ef56".to_owned()), true).unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
    }
}
