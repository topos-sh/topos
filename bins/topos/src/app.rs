//! The composition root for the binary: parse argv, wire the real seams, run recovery, dispatch a verb,
//! and emit either the `--json` envelope (stdout) or thin TTY text.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use serde::Serialize;
use topos_harness::{ClaudeCode, ConfigStore, HarnessAdapter, OpenClaw};
use topos_types::HarnessId;

use crate::cli::{Cli, Command, RoleArg};
use crate::ctx::Ctx;
use crate::device_signer::DeviceSigner;
use crate::error::ClientError;
use crate::fs_seam::{FsOps, RealFs};
use crate::ids::{Clock, RealClock, RealIds};
use crate::plane::{ContributeSource, EnrollSource, GovernanceSource, PlaneSource};
use crate::plane_http::{FileFollow, SkillCred, UreqDeviceClient, UreqPlane};
use crate::sidecar::{Layout, recover};
use crate::{enroll, identity, logfile, ops, render};

/// Run the CLI; returns the process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    // The global `--workspace <id>` — which workspace the ambient write verbs act in (and the filter that
    // disambiguates a skill name shared across workspaces). Optional; inferred with a single workspace.
    let workspace = cli.workspace;
    let command = cli.command;
    let cmd_name = command.name();

    let fs = RealFs;
    let ids = RealIds;
    let clock = RealClock;
    let layout = Layout::new(&resolve_home());
    // The error-side diagnostics channel: every failure that reaches a finisher below lands its full
    // detail in the append-only log the redacted user surfaces point at.
    let diag = Diag {
        fs: &fs,
        clock: &clock,
        log_path: layout.log_path(),
    };
    // The harness adapter, selected through the one dispatch seam. v0 wires Claude Code only; the
    // adapter touches its config home only when adopting a recognized skill, arming currency, or on
    // uninstall.
    let harness = adapter_for(HarnessId::ClaudeCode, &fs);

    // Recovery runs at the start of every command (it also abandons an expired, never-redeemed
    // enrollment WAL against the real wall clock).
    let now_millis = i64::try_from(clock.now_unix_millis()).unwrap_or(i64::MAX);
    if let Err(e) = recover(&fs, &layout, now_millis) {
        return emit_err(json, cmd_name, &e, &diag);
    }

    // The plane + follow-state sources. When enrollment has been written (`instance.json` present —
    // `follow` writes it), wire the REAL `ureq` transport + the on-disk follow state + the pinned plane
    // key; otherwise keep the INERT pair (a never-enrolled install stays a truthful no-op). The inert
    // ZSTs and the loaded enrollment are both stack locals so the `&dyn` trait objects below borrow
    // correctly for the rest of `run`.
    let inert_plane = crate::plane::InertPlane;
    let inert_follow = crate::plane::InertFollow;
    let enrollment = match load_enrollment(&fs, &layout) {
        Ok(e) => e,
        Err(e) => return emit_err(json, cmd_name, &e, &diag),
    };
    let (plane, follow, plane_key): (
        &dyn crate::plane::PlaneSource,
        &dyn crate::plane::FollowSource,
        [u8; 32],
    ) = match &enrollment {
        Some(e) => (&e.plane, &e.follow, e.plane_key),
        None => (&inert_plane, &inert_follow, [0u8; 32]),
    };

    // `add` and `pull` author commits (adoption / a draft snapshot before a divergence), so both load
    // (and on first use, mint) the device identity — `uninstall` must never create or require it before
    // tearing the home down.
    let device_id = match &command {
        // `follow` also loads (and on first use mints) the device identity: the device-key signer it drives
        // requires `host.json` to exist, and the skill path authors a draft snapshot through the pull engine.
        // `publish` authors the candidate commit and `revert` the forward-revert commit, so both load (and
        // on first use mint) the device id.
        Command::Add { .. }
        | Command::Pull { .. }
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
        plane_key,
        follow,
    };

    match command {
        Command::Add {
            source,
            skill,
            harness,
            global,
        } => {
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
                        "cannot resolve a skill name without $HOME set — adopt a directory by path \
                         (`topos add ./<dir>`)"
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
                                skill,
                                harness,
                                global,
                            },
                        )
                    }
                    None => Err(ClientError::InvalidArgument(
                        "cannot import a remote skill without $HOME set (needed to resolve the harness \
                         skills dir)"
                            .into(),
                    )),
                },
                crate::source::SourceSpec::Unsupported(msg) => {
                    Err(ClientError::InvalidArgument(msg))
                }
            };
            finish(json, cmd_name, result, render::add_tty, &diag)
        }
        Command::Follow {
            target,
            manual,
            wait,
        } => {
            // The transports are built per-base-URL (known only after the op parses the link / reads the
            // WAL): the shared creds-free `ureq` enroll connector + the read transport for the offer
            // disclosure (the one connector that carries per-skill creds, so it stays a local closure).
            let plane_connect =
                |base_url: &str, creds: HashMap<String, SkillCred>| -> Box<dyn PlaneSource> {
                    Box::new(UreqPlane::new(base_url.to_owned(), creds))
                };
            let connectors = ops::FollowConnectors {
                enroll: &connect_enroll,
                plane: &plane_connect,
            };
            let opts = ops::FollowOpts {
                manual,
                // The global `--workspace` disambiguates a positional skill name shared across the
                // workspaces this install follows on one plane (the enrollment motions ignore it).
                workspace: workspace.clone(),
            };
            let first = ops::follow(&ctx, &connectors, target, opts);
            // Block on a pending device-authorization until the human approves (so a person never re-invokes
            // `follow` by hand), unless this is a headless `--json` run without `--wait` (which must not
            // hang). The interactive block only ever RESUMES (target = None + the pending WAL drives it).
            let policy = WaitPolicy::resolve(json, wait, &clock);
            let result =
                block_on_pending(&clock, &policy, first, follow_pending_disclosure, || {
                    ops::follow(
                        &ctx,
                        &connectors,
                        None,
                        ops::FollowOpts {
                            manual,
                            workspace: None,
                        },
                    )
                });
            finish_follow(json, cmd_name, result, &diag)
        }
        Command::Unfollow { skill } => finish(
            json,
            cmd_name,
            ops::unfollow(&ctx, &skill),
            render::unfollow_tty,
            &diag,
        ),
        Command::Invite {
            emails,
            role,
            skills,
        } => finish(
            json,
            cmd_name,
            ops::invite(
                &ctx,
                &connect_governance,
                emails,
                role.map(RoleArg::to_workspace_role),
                skills,
                workspace.as_deref(),
            ),
            render::invite_tty,
            &diag,
        ),
        Command::List {
            skill,
            footprint,
            tracked,
            remote,
        } => {
            // Under `--remote`, resolve the enrolled plane + memberships (a typed "run follow first" when
            // there is no enrollment), then build the device-signed catalog transport + signer as locals so
            // the scope borrows them across the `list()` call.
            let remote_inputs = if remote {
                match list_remote_inputs(&fs, &ctx.layout) {
                    Ok(inputs) => Some(inputs),
                    Err(e) => return emit_err(json, cmd_name, &e, &diag),
                }
            } else {
                None
            };
            let catalog_client;
            let signer;
            let scope = if let Some((base_url, memberships)) = remote_inputs {
                catalog_client = UreqDeviceClient::new(base_url);
                signer = match DeviceSigner::load_or_generate(&fs, &ctx.layout) {
                    Ok(s) => s,
                    Err(e) => return emit_err(json, cmd_name, &e, &diag),
                };
                Some(ops::RemoteScope {
                    catalog: &catalog_client,
                    signer: &signer,
                    memberships,
                    only: workspace.clone(),
                })
            } else {
                None
            };
            finish_list(
                json,
                cmd_name,
                ops::list(
                    &ctx,
                    skill.as_deref(),
                    footprint,
                    list_discovery(tracked),
                    scope,
                ),
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
            propose,
            wait,
        } => {
            // The standup branch's plane base: the env override, else the compiled-in hosted default.
            // Used ONLY when un-enrolled (an enrolled publish reads its plane from instance.json).
            let standup = ops::StandupConnectors {
                enroll: &connect_enroll,
                base_url: resolve_standup_base(std::env::var("TOPOS_PLANE_URL").ok()),
            };
            // Discovery roots for the auto-add pre-step (a `publish` of an untracked local source adopts it
            // first) — the SAME roots `add`/`list` use; `None` degrades name/dir resolution the same way.
            let roots = list_discovery(false);
            let publish_once = |t: &str| {
                ops::publish(
                    &ctx,
                    &connect_contribute,
                    &connect_governance,
                    &standup,
                    roots.as_ref(),
                    t,
                    propose,
                    workspace.as_deref(),
                )
            };
            let first = publish_once(&target);
            // If the standup went PENDING, re-bind the disclosed digest so drift during the sign-in wait is
            // refused by the re-poll's consent gate (see `standup_repoll_target`).
            let repoll_target = standup_repoll_target(&target, &first);
            // Block on a pending standup sign-in until it settles (auto-creating the workspace + publishing
            // in the same command), unless this is a headless `--json` run without `--wait` (which must not
            // hang — it returns the pending state + the `ENROLL_RESUME` next-action, as before).
            let policy = WaitPolicy::resolve(json, wait, &clock);
            let result =
                block_on_pending(&clock, &policy, first, publish_pending_disclosure, || {
                    publish_once(&repoll_target)
                });
            finish_publish(json, cmd_name, result, &diag)
        }
        Command::Review {
            target,
            approve,
            reject,
        } => {
            // clap's `verdict` ArgGroup guarantees exactly one flag (a violation was a standard usage
            // error at exit 2 before this point) — the verdict IS `approve`.
            debug_assert!(approve ^ reject, "clap enforces exactly one verdict flag");
            finish(
                json,
                cmd_name,
                ops::review(
                    &ctx,
                    &connect_contribute,
                    &target,
                    approve,
                    workspace.as_deref(),
                ),
                render::review_tty,
                &diag,
            )
        }
        Command::Revert { skill, to, confirm } => finish(
            json,
            cmd_name,
            ops::revert(
                &ctx,
                &connect_contribute,
                &skill,
                &to,
                confirm,
                workspace.as_deref(),
            ),
            render::revert_tty,
            &diag,
        ),
        Command::Log { skill } => finish(
            json,
            cmd_name,
            ops::log(&ctx, &skill),
            render::log_tty,
            &diag,
        ),
        Command::Pull {
            skill,
            onto_current,
            quiet,
        } => {
            let result = pull_with_name_fallback(&ctx, skill, onto_current);
            if quiet {
                // Byte-silent stdout: the session-start hook injects stdout into the session context, so a
                // clean sweep emits nothing. An error surfaces on stderr with a non-zero exit — never on
                // stdout, even under `--json` (which `--quiet` overrides for the hook path — hence the
                // forced TTY presentation below). Isolated per-skill failures already reached stderr
                // inside the sweep; the exit stays 0 (isolation).
                match result {
                    Ok(_) => ExitCode::SUCCESS,
                    Err(e) => emit_err(false, cmd_name, &e, &diag),
                }
            } else {
                finish_pull(json, cmd_name, result, &diag)
            }
        }
        Command::Uninstall { footprint } => {
            let binary = std::env::current_exe().ok();
            finish(
                json,
                cmd_name,
                ops::uninstall(&ctx, footprint, binary.as_deref()),
                render::uninstall_tty,
                &diag,
            )
        }
        Command::Upgrade { check, version } => {
            // A MAINTENANCE command: it replaces the binary itself. It mints no device identity (kept out
            // of the device-id match above) and never touches a skill / the plane / the account.
            let releases = connect_releases();
            let base_url = std::env::var("TOPOS_INSTALL_BASE_URL").ok();
            let result = std::env::current_exe()
                .map_err(ClientError::from)
                .and_then(|exe| {
                    ops::upgrade(
                        &ctx,
                        releases.as_ref(),
                        &exe,
                        ops::UpgradeOpts {
                            check,
                            version,
                            base_url,
                        },
                    )
                });
            finish(json, cmd_name, result, render::upgrade_tty, &diag)
        }
    }
}

/// The shared per-base-URL wire connectors: the enroll / governance / contribute seams all box the SAME
/// creds-free `ureq` client (`UreqDeviceClient` implements every one of those source traits) — only the trait
/// object type differs, so each is one coercion of one constructor (a `&connect_*` fn reference coerces
/// to the seam's `&dyn Fn`).
fn connect_enroll(base_url: &str) -> Box<dyn EnrollSource> {
    Box::new(UreqDeviceClient::new(base_url.to_owned()))
}

fn connect_governance(base_url: &str) -> Box<dyn GovernanceSource> {
    Box::new(UreqDeviceClient::new(base_url.to_owned()))
}

fn connect_contribute(base_url: &str) -> Box<dyn ContributeSource> {
    Box::new(UreqDeviceClient::new(base_url.to_owned()))
}

/// The real release source for `topos upgrade` — the `ureq` GitHub transport. No base URL / creds: the
/// updater's default download base is compiled in (overridable via `TOPOS_INSTALL_BASE_URL`).
fn connect_releases() -> Box<dyn crate::release::ReleaseSource> {
    Box::new(crate::plane_http::UreqReleases::new())
}

/// The hosted plane's compiled-in base URL — used ONLY by the un-enrolled `publish` standup branch (an
/// enrolled client reads its plane from `instance.json`, and the `/i/` doors carry their own base).
pub(crate) const DEFAULT_HOSTED_BASE_URL: &str = "https://api.topos.sh";

/// Resolve the standup base URL: a non-empty `TOPOS_PLANE_URL` override wins, else the hosted default.
/// Pure (the env read happens at the call site) so the override precedence is unit-testable.
pub(crate) fn resolve_standup_base(env_override: Option<String>) -> String {
    match env_override {
        Some(v) if !v.trim().is_empty() => v.trim().trim_end_matches('/').to_owned(),
        _ => DEFAULT_HOSTED_BASE_URL.to_owned(),
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
                let value = serde_json::to_value(&out.data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.warnings = out.warnings;
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
                    let label = m.display_name.unwrap_or_else(|| m.workspace_id.clone());
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

/// `follow`'s finisher — like [`finish`], but it carries the success-path `next_actions` (run
/// re-invoke `follow` while pending; `pull` once offers are disclosed) on the envelope. The `--json`
/// payload is exactly the schema-pinned `FollowData`; the resume disclosure the outcome carries
/// alongside is TTY-only (the pinned shape has no resume field).
fn finish_follow(
    json: bool,
    command: &str,
    result: Result<ops::FollowOutcome, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(out) => {
            if json {
                let value = serde_json::to_value(&out.data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::follow_next_actions(&out.data);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::follow_tty(&out));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// The poll cadence while a human opens the browser and approves: re-poll the device-authorization grant
/// this often. There is no separate client timeout by default — the device code's own expiry makes the
/// plane return a terminal Expired/Denied that surfaces as `Err`, ending the loop (a numeric `--wait`
/// deadline can end it sooner, still pending).
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// A pending device-authorization's human-facing disclosure — the clickable URL (the RFC-8628
/// `verification_uri_complete`, which embeds the code) plus the anti-phishing fingerprint. Extracted from
/// either verb's pending outcome so [`block_on_pending`] can print it once, generically. No bare code line:
/// the code rides inside the URL (clicked, never typed).
struct PendingDisclosure {
    verification_uri_complete: String,
    device_fingerprint: String,
}

/// The pending disclosure for a `follow` outcome (None ⇒ not a pending device-auth).
fn follow_pending_disclosure(out: &ops::FollowOutcome) -> Option<PendingDisclosure> {
    out.data.pending.as_ref().map(|p| PendingDisclosure {
        verification_uri_complete: p.verification_uri_complete.clone(),
        device_fingerprint: p.device_fingerprint.clone(),
    })
}

/// The pending disclosure for a `publish` outcome (only the un-enrolled standup branch is ever pending).
fn publish_pending_disclosure(out: &ops::PublishOutcome) -> Option<PendingDisclosure> {
    match out {
        ops::PublishOutcome::Pending { data, .. } => {
            data.pending.as_ref().map(|p| PendingDisclosure {
                verification_uri_complete: p.verification_uri_complete.clone(),
                device_fingerprint: p.device_fingerprint.clone(),
            })
        }
        _ => None,
    }
}

/// The target a standup re-poll must use. If the first outcome went PENDING, reuse the canonical pinned
/// target the ops layer already built into `resume_argv` (`<skill>@<digest>` with the SAME `<skill>` parse
/// `publish` uses and the disclosed digest baked in), so the re-poll's consent gate refuses bytes that
/// drift during the sign-in wait — never silently shipped, and never mis-parsing a skill name that itself
/// contains `@`. `resume_argv` is `[topos, publish, <target>, --json]`; take the element after `publish`.
/// A non-pending first result (already published, or an error), or an unexpected argv shape, keeps the
/// original target (the block returns a non-pending first result untouched anyway).
fn standup_repoll_target(
    original: &str,
    first: &Result<ops::PublishOutcome, ClientError>,
) -> String {
    match first {
        Ok(ops::PublishOutcome::Pending { resume_argv, .. }) => resume_argv
            .iter()
            .skip_while(|a| a.as_str() != "publish")
            .nth(1)
            .cloned()
            .unwrap_or_else(|| original.to_owned()),
        _ => original.to_owned(),
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

    // The waiting disclosure on STDERR (stdout stays the clean final envelope/TTY). No bare code line —
    // the code rides inside the URL; the fingerprint is the one thing to eyeball against the page.
    eprintln!(
        "Open this URL to approve:\n  {}\n  fingerprint: {} (confirm it matches the page)",
        disc.verification_uri_complete,
        render::group_fingerprint(&disc.device_fingerprint),
    );
    eprintln!("Waiting for approval…");

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
            Some(d) => Duration::from_millis(d.saturating_sub(clock.now_unix_millis()))
                .min(DEVICE_POLL_INTERVAL),
            None => DEVICE_POLL_INTERVAL,
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

/// `publish`'s finisher — the verb yields a direct publish ([`PublishData`]), an opened proposal
/// ([`ProposeData`]), or a PENDING standup sign-in (an `ok` envelope whose `ENROLL_RESUME` next-action
/// carries this same command's argv); each renders through its own `--json` payload / TTY line. A typed
/// failure (APPROVAL_REQUIRED / CONFLICT / DENIED / …) flows through [`emit_err`], which attaches the
/// right `next_actions`.
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
        Ok(ops::PublishOutcome::Pending { data, resume_argv }) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::publish_pending_next_actions(resume_argv);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::publish_pending_tty(&data, &resume_argv));
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
            mode: ops::TargetMode::GoBack(vref),
        });
    }
    Ok(ops::PullScope::One {
        name: arg,
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
) -> Result<ops::PullOutcome, ClientError> {
    let arg = skill.clone();
    let first = build_pull_scope(skill, onto_current).and_then(|scope| ops::pull(ctx, scope));
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
    plane_key: [u8; 32],
}

/// Load the enrollment docs read-only. Returns `Some` whenever `instance.json` is present — enrollment is
/// what writes it, so its presence IS the enrolled state; `follows.json` is optional (an empty membership
/// door, or every follow since flipped off by `unfollow`). The pinned plane key must stay loaded even with
/// zero active follows: the write verbs (publish/revert/review) verify the OK receipt's signed pointer
/// against it, and an enrolled author with nothing followed is a normal state. The bare `pull` stays an
/// honest no-op either way (the sweep skips a `following == false` entry, and renders "No followed
/// skills." over an empty set). A corrupt / newer-schema doc fails closed (propagated), never silently
/// degraded to inert.
fn load_enrollment(fs: &dyn FsOps, layout: &Layout) -> Result<Option<Enrollment>, ClientError> {
    let Some(instance) = enroll::read_instance(fs, layout)? else {
        return Ok(None);
    };
    let follows = enroll::read_follows(fs, layout)?.unwrap_or_else(|| enroll::Follows {
        schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
        follows: Vec::new(),
    });
    let plane_key = ops::parse_hex32(&instance.plane_key).map_err(|_| {
        ClientError::Corrupt("instance.json plane_key is not 32-byte lowercase hex".into())
    })?;
    let plane = UreqPlane::new(instance.base_url, enroll::skill_creds(&follows));
    let follow = FileFollow::new(enroll::follow_contexts(&follows));
    Ok(Some(Enrollment {
        plane,
        follow,
        plane_key,
    }))
}

/// Build the harness adapter for `id`, borrowing the shared config-store seam. Adding a harness is ONE
/// new match arm — no caller change. v0 only ever selects Claude Code (the CLI's one selection site
/// above passes `HarnessId::ClaudeCode`; each adapter resolves its own config home:
/// `$CLAUDE_CONFIG_DIR` else `$HOME/.claude`; `$HOME/.openclaw`; `$HERMES_HOME` else `$HOME/.hermes`).
/// The OpenClaw and Hermes arms serve the test rigs while their concrete config bytes stay provisional
/// behind the pilot readiness probes (each module's doc).
fn adapter_for<'a>(id: HarnessId, fs: &'a dyn ConfigStore) -> Box<dyn HarnessAdapter + 'a> {
    match id {
        HarnessId::ClaudeCode => Box::new(ClaudeCode::new(ClaudeCode::resolve_home(), fs)),
        HarnessId::OpenClaw => Box::new(OpenClaw::new(OpenClaw::resolve_home(), fs)),
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
        DEFAULT_HOSTED_BASE_URL, build_pull_scope, resolve_standup_base, standup_repoll_target,
    };
    use crate::ops::{PublishOutcome, PullScope, TargetMode, VersionRef};

    /// A standup PENDING outcome whose ops-built `resume_argv` pins `resume_target`, as
    /// `standup_repoll_target` sees it.
    fn pending_publish(resume_target: &str) -> Result<PublishOutcome, crate::error::ClientError> {
        use topos_types::results::{PublishData, PublishPending, PublishPendingStatus};
        Ok(PublishOutcome::Pending {
            data: PublishData {
                skill_id: "sk_x".to_owned(),
                version_id: None,
                bundle_digest: "d1".to_owned(),
                current_generation: None,
                invite_link: None,
                pending: Some(PublishPending {
                    status: PublishPendingStatus::SigninRequired,
                    verification_uri_complete: "https://topos.sh/verify/tok".to_owned(),
                    user_code: "tok".to_owned(),
                    device_fingerprint: "abcd".to_owned(),
                    expires_at: None,
                }),
                standup: None,
                added: None,
            },
            resume_argv: vec![
                "topos".to_owned(),
                "publish".to_owned(),
                resume_target.to_owned(),
                "--json".to_owned(),
            ],
        })
    }

    #[test]
    fn standup_repoll_reuses_the_canonical_pinned_resume_target() {
        // The re-poll target is the ops-built pinned target from `resume_argv` verbatim — its `<digest>`
        // pin makes the consent gate refuse byte-drift during the sign-in wait, never silently shipping it.
        assert_eq!(
            standup_repoll_target("docs", &pending_publish("docs@d1")),
            "docs@d1"
        );
        // A skill NAME that itself contains `@` (a harness-qualified name) is preserved verbatim — not
        // truncated at the first `@` (the ops layer parsed the digest off the tail, so `resume_argv` already
        // holds the right target).
        assert_eq!(
            standup_repoll_target("docs@claude-code", &pending_publish("docs@claude-code@d1")),
            "docs@claude-code@d1"
        );
        // A non-pending first result has nothing to re-bind (the block returns it as-is).
        let published = pending_publish("docs@d1").map(|o| match o {
            PublishOutcome::Pending { mut data, .. } => {
                data.pending = None;
                data.version_id = Some("v".repeat(64));
                PublishOutcome::Published(data)
            }
            other => other,
        });
        assert_eq!(standup_repoll_target("docs", &published), "docs");
    }

    #[test]
    fn standup_base_env_override_beats_the_compiled_default() {
        // No override (or a blank one) → the compiled-in hosted default.
        assert_eq!(resolve_standup_base(None), DEFAULT_HOSTED_BASE_URL);
        assert_eq!(
            resolve_standup_base(Some("   ".to_owned())),
            DEFAULT_HOSTED_BASE_URL
        );
        // A non-empty TOPOS_PLANE_URL wins, trimmed of whitespace + a trailing slash.
        assert_eq!(
            resolve_standup_base(Some("http://127.0.0.1:8787/".to_owned())),
            "http://127.0.0.1:8787"
        );
    }

    #[test]
    fn pull_target_recognizes_full_ids_and_short_prefixes() {
        // The full 64-hex suffix goes back (the long-standing shape).
        let full = format!("docs@{}", "ab".repeat(32));
        assert!(matches!(
            build_pull_scope(Some(full), false).unwrap(),
            PullScope::One { name, mode: TargetMode::GoBack(VersionRef::Full(_)) } if name == "docs"
        ));
        // A pasted 12-char short form is a go-back too — no more silent NO_SUCH_SKILL degradation.
        assert!(matches!(
            build_pull_scope(Some("docs@ab12cd34ef56".to_owned()), false).unwrap(),
            PullScope::One { name, mode: TargetMode::GoBack(VersionRef::Prefix(p)) }
                if name == "docs" && p == "ab12cd34ef56"
        ));
        // A hex-ish suffix SHORTER than the prefix floor stays part of the name (a name may contain `@`),
        // as does any non-hex suffix.
        for name in ["docs@ab12", "team@cli"] {
            assert!(matches!(
                build_pull_scope(Some(name.to_owned()), false).unwrap(),
                PullScope::One { name: n, mode: TargetMode::AcceptPending } if n == name
            ));
        }
        // The escape never combines with a go-back ref.
        let err = build_pull_scope(Some("docs@ab12cd34ef56".to_owned()), true).unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
    }
}
