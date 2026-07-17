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

/// Run the CLI; returns the process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    // The global `--workspace` — which workspace the ambient write verbs act in (and the filter that
    // disambiguates a skill name shared across workspaces). Optional; inferred with a single workspace.
    // Canonicalized below (name → id) once the layout exists, so every consumer keeps id semantics.
    let workspace = cli.workspace;
    let command = cli.command;
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
            let connectors = ops::FollowConnectors {
                enroll: &connect_enroll,
                directory: &connect_directory,
                delivery: &connect_delivery,
                web_origin: web_origin.clone(),
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
            // the wait into the apply.
            let policy = WaitPolicy::resolve(json, wait, &clock);
            let result =
                block_on_pending(&clock, &policy, first, follow_pending_disclosure, || {
                    ops::follow(&ctx, &connectors, Vec::new(), mk_opts())
                });
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
            channel,
            yes,
        } => {
            let connectors = ops::InviteConnectors {
                governance: &connect_governance,
                directory: &connect_directory,
            };
            let result = ops::invite(&ctx, &connectors, email, channel, workspace.as_deref(), yes);
            finish_invite(json, cmd_name, result, &diag)
        }
        Command::List {
            name,
            remote,
            tracked,
            footprint,
            channel,
            skill,
        } => {
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
                ops::list_with(&ctx, &filter, footprint, list_discovery(tracked), scope),
                &diag,
            )
        }
        Command::Diff { skill, r#ref } => finish(
            json,
            cmd_name,
            ops::diff(&ctx, &skill, r#ref.as_deref()),
            render::diff_tty,
            &diag,
        ),
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
            yes,
        } => {
            let _ = yes;
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
            );
            finish_review(json, cmd_name, result, &diag)
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
        Command::Log { skill } => {
            let connectors = ops::LogConnectors {
                directory: &connect_directory,
            };
            finish(
                json,
                cmd_name,
                ops::log(&ctx, &connectors, &skill),
                render::log_tty,
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
                finish_pull(json, cmd_name, result, &diag)
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
                    // The same blocking idiom as `follow`: interactive (or `--wait`) runs re-poll
                    // until the browser approval settles; a headless `--json` run without `--wait`
                    // returns the pending state and never hangs.
                    let policy = WaitPolicy::resolve(json, wait, &clock);
                    let result =
                        block_on_pending(&clock, &policy, first, login_pending_disclosure, || {
                            ops::login(
                                &ctx,
                                &connectors,
                                server_url.as_deref(),
                                workspace.as_deref(),
                            )
                        });
                    finish_login(json, cmd_name, result, &diag)
                }
                AuthCmd::Logout { yes } => {
                    finish_logout(json, cmd_name, ops::logout(&ctx, &connectors, yes), &diag)
                }
                AuthCmd::Status => finish(
                    json,
                    cmd_name,
                    ops::status(&ctx, &connectors),
                    render::auth_status_tty,
                    &diag,
                ),
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

/// `pull`'s finisher — like [`finish`], but a bare sweep's isolated per-skill failures ride the
/// envelope's `warnings` (one stable-shape line per failed skill) AND the TTY summary, so a wedged
/// skill is visible on both surfaces. Isolation semantics hold: `ok` stays `true`, exit stays 0 (each
/// failure also reached stderr inside the sweep).
fn finish_pull(
    json: bool,
    command: &str,
    result: Result<ops::PullOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(out) => {
            if json {
                // Each WITHDRAWN skill carries a paste-ready `keep-as-yours` next action.
                let next_actions = render::withdrawn_next_actions(&out.data);
                let value = serde_json::to_value(&out.data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.warnings = out.warnings;
                envelope.next_actions = next_actions;
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::pull_tty(&out.data, &out.warnings));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `list`'s finisher — the `--json` envelope carries exactly the schema-pinned `ListData` plus any
/// `--remote` per-workspace catalog-read warnings (mirroring `pull`); the TTY additionally renders the
/// enrollment header + per-row follow annotations the outcome carries alongside (TTY-only disclosure —
/// `ListData`'s pinned shape has no enrollment fields).
fn finish_list(
    json: bool,
    command: &str,
    result: Result<ops::ListOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(out) => {
            if json {
                let value = serde_json::to_value(&out.data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.warnings = out.warnings;
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::list_tty(&out));
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
    let instance = enroll::read_instance(fs, layout)?.ok_or_else(|| {
        ClientError::Enrollment("not enrolled; run `topos follow <link>` first".into())
    })?;
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
        return Err(ClientError::Enrollment(
            "not enrolled in any workspace; run `topos follow <link>` first".into(),
        ));
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

/// `review`'s finisher — the inbox, a target describe, or an applied verdict.
fn finish_review(
    json: bool,
    command: &str,
    result: Result<ops::ReviewOutcome, ClientError>,
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
                let value = serde_json::json!({ "describe": data });
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::describe_next_actions(next_argvs.clone());
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
/// `verification_uri_complete`, which embeds the code) plus the short cross-check code and the
/// server's minimum poll interval. Extracted from either verb's pending outcome so
/// [`block_on_pending`] can print it once, generically.
struct PendingDisclosure {
    verification_uri_complete: String,
    user_code: String,
    /// The server's minimum poll interval, when disclosed (else the fallback cadence).
    interval_secs: Option<u64>,
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
            verification_uri_complete: p.verification_uri_complete.clone(),
            user_code: p.user_code.clone(),
            interval_secs: p.interval_secs,
        }),
        _ => None,
    }
}

/// The pending disclosure for an `auth login` outcome (None ⇒ the sign-in settled).
fn login_pending_disclosure(out: &ops::AuthLoginOutcome) -> Option<PendingDisclosure> {
    match out {
        ops::AuthLoginOutcome::Pending(p) => Some(PendingDisclosure {
            verification_uri_complete: p.verification_uri_complete.clone(),
            user_code: p.user_code.clone(),
            interval_secs: Some(p.interval_secs),
        }),
        ops::AuthLoginOutcome::Done(_) => None,
    }
}

/// Whether this invocation blocks on a pending device-authorization, and until when.
/// - `block == false` — never block (a headless `--json` run without `--wait`): return the first result.
/// - `deadline_millis == None` — block until the device code's own TTL ends it (interactive default, or a
///   bare `--wait`).
/// - `deadline_millis == Some(t)` — block until settled or the wall clock passes `t` (`--wait <seconds>`),
///   whichever comes first.
struct WaitPolicy {
    block: bool,
    deadline_millis: Option<u64>,
}

impl WaitPolicy {
    /// Derive the policy from `--json` and the `--wait [<seconds>]` flag: block when interactive (`!json`)
    /// OR when `--wait` was given in any form; a numeric `--wait <seconds>` sets a wall-clock deadline.
    fn resolve(json: bool, wait: Option<Option<u64>>, clock: &dyn Clock) -> Self {
        Self {
            block: !json || wait.is_some(),
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

/// Block on a pending device-authorization until it settles, an optional deadline passes, or the device
/// code's own expiry ends it with a terminal error — so a person never re-invokes the command by hand. The
/// disclosure prints to STDERR once (stdout stays the clean final render). `pending_of` extracts the
/// disclosure (None ⇒ not pending ⇒ return as-is); `repoll` re-invokes the op, which RESUMES via its
/// on-disk WAL. A `policy.block == false` (headless `--json` without `--wait`) returns the first result
/// untouched — a headless agent must not hang.
fn block_on_pending<T>(
    clock: &dyn Clock,
    policy: &WaitPolicy,
    first: Result<T, ClientError>,
    pending_of: impl Fn(&T) -> Option<PendingDisclosure>,
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

    // The waiting disclosure on STDERR (stdout stays the clean final envelope/TTY): the clickable URL
    // plus the short code the human cross-checks against the approval page.
    eprintln!(
        "Open this URL to approve:\n  {}\n  code: {} (confirm it matches the page)",
        disc.verification_uri_complete, disc.user_code,
    );
    eprintln!("Waiting for approval…");
    let interval = disc.poll_interval();

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
        // Sleep the poll interval, but never past the deadline.
        let nap = match policy.deadline_millis {
            Some(d) => {
                Duration::from_millis(d.saturating_sub(clock.now_unix_millis())).min(interval)
            }
            None => interval,
        };
        std::thread::sleep(nap);
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
    use super::{DEFAULT_WEB_ORIGIN, build_pull_scope, resolve_web_origin};
    use crate::ops::{PullScope, TargetMode, VersionRef};

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
