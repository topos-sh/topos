//! The composition root for the binary: parse argv, wire the real seams, run recovery, dispatch a verb,
//! and emit either the `--json` envelope (stdout) or thin TTY text.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use serde::Serialize;
use topos_harness::{ClaudeCode, ConfigStore, HarnessAdapter, OpenClaw};
use topos_types::HarnessId;

use crate::cli::{Cli, Command, RoleArg};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::{FsOps, RealFs};
use crate::ids::{Clock, RealClock, RealIds};
use crate::plane::{ContributeSource, EnrollSource, GovernanceSource, PlaneSource};
use crate::plane_http::{FileFollow, SkillCred, UreqEnroll, UreqPlane};
use crate::sidecar::{Layout, recover};
use crate::{enroll, identity, logfile, ops, render};

/// Run the CLI; returns the process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
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
        // requires `host.json` to exist, and `--approve` authors a draft snapshot through the pull engine.
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
        Command::Add { path } => finish(
            json,
            cmd_name,
            ops::add(&ctx, &path),
            render::add_tty,
            &diag,
        ),
        Command::Follow {
            link,
            manual,
            resume,
            approve,
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
                resume,
                approve,
            };
            finish_follow(
                json,
                cmd_name,
                ops::follow(&ctx, &connectors, link, opts),
                &diag,
            )
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
            ),
            render::invite_tty,
            &diag,
        ),
        Command::List { skill, footprint } => finish(
            json,
            cmd_name,
            ops::list(&ctx, skill.as_deref(), footprint),
            render::list_tty,
            &diag,
        ),
        Command::Diff { skill, r#ref } => finish(
            json,
            cmd_name,
            ops::diff(&ctx, &skill, r#ref.as_deref()),
            render::diff_tty,
            &diag,
        ),
        Command::Publish {
            skill,
            propose,
            approve,
        } => finish_publish(
            json,
            cmd_name,
            ops::publish(
                &ctx,
                &connect_contribute,
                &connect_governance,
                skill.as_deref(),
                propose,
                &approve,
            ),
            &diag,
        ),
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
                ops::review(&ctx, &connect_contribute, &target, approve),
                render::review_tty,
                &diag,
            )
        }
        Command::Revert {
            skill,
            to,
            approve,
            confirm,
        } => finish(
            json,
            cmd_name,
            ops::revert(
                &ctx,
                &connect_contribute,
                skill.as_deref(),
                &to,
                &approve,
                confirm,
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
            let result =
                build_pull_scope(skill, onto_current).and_then(|scope| ops::pull(&ctx, scope));
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
    }
}

/// The shared per-base-URL wire connectors: the enroll / governance / contribute seams all box the SAME
/// creds-free `ureq` client (`UreqEnroll` implements every one of those source traits) — only the trait
/// object type differs, so each is one coercion of one constructor (a `&connect_*` fn reference coerces
/// to the seam's `&dyn Fn`).
fn connect_enroll(base_url: &str) -> Box<dyn EnrollSource> {
    Box::new(UreqEnroll::new(base_url.to_owned()))
}

fn connect_governance(base_url: &str) -> Box<dyn GovernanceSource> {
    Box::new(UreqEnroll::new(base_url.to_owned()))
}

fn connect_contribute(base_url: &str) -> Box<dyn ContributeSource> {
    Box::new(UreqEnroll::new(base_url.to_owned()))
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
/// envelope's `warnings` (one stable-shape line per failed skill), so an agent driving `pull --json`
/// sees a wedged skill machine-visibly. Isolation semantics hold: `ok` stays `true`, exit stays 0 (each
/// failure also reached stderr inside the sweep, which covers the TTY presentation).
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
                println!("{}", render::pull_tty(&out.data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `follow`'s finisher — like [`finish`], but it carries the success-path `next_actions` (run
/// `follow --resume` while pending; `pull` once offers are disclosed) on the envelope.
fn finish_follow(
    json: bool,
    command: &str,
    result: Result<topos_types::results::FollowData, ClientError>,
    diag: &Diag<'_>,
) -> ExitCode {
    match result {
        Ok(data) => {
            if json {
                let value = serde_json::to_value(&data).unwrap_or_default();
                let mut envelope = render::ok_envelope(command, value);
                envelope.next_actions = render::follow_next_actions(&data);
                println!("{}", render::to_json(&envelope));
            } else {
                println!("{}", render::follow_tty(&data));
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_err(json, command, &e, diag),
    }
}

/// `publish`'s finisher — the verb yields either a direct publish ([`PublishData`]) or an opened proposal
/// ([`ProposeData`]); each renders through its own `--json` payload / TTY line. A typed failure
/// (APPROVAL_REQUIRED / CONFLICT / DENIED / …) flows through [`emit_err`], which attaches the right
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
/// pending update; `<name>@<hash>` = go back to that version's bytes; `--onto-current` = the escape.
///
/// A go-back suffix is recognized only when the part after the LAST `@` is a valid 64-char lowercase-hex
/// version id; otherwise the whole argument is the skill name. So a skill whose name contains `@` (e.g.
/// `team@cli`) is accepted as a name, and `team@cli@<hash>` still goes back correctly.
///
/// `--onto-current` (the escape) requires a `<skill>` target (clap enforces that half via `requires`)
/// and is mutually exclusive with `@<hash>` (a runtime usage error — the suffix shape is only known
/// after parsing).
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
        && let Ok(hash) = ops::parse_hex32(suffix)
    {
        if onto_current {
            return Err(ClientError::InvalidArgument(
                "--onto-current is not valid with @<hash>".into(),
            ));
        }
        return Ok(ops::PullScope::One {
            name: name.to_owned(),
            mode: ops::TargetMode::GoBack(hash),
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
        schema_version: topos_types::SCHEMA_VERSION,
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
