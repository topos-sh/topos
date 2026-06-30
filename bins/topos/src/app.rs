//! The composition root for the binary: parse argv, wire the real seams, run recovery, dispatch a verb,
//! and emit either the `--json` envelope (stdout) or thin TTY text.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use serde::Serialize;
use topos_harness::ClaudeCode;

use crate::cli::{Cli, Command};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::{FsOps, RealFs};
use crate::ids::{RealClock, RealIds};
use crate::plane_http::{FileFollow, UreqPlane};
use crate::sidecar::{Layout, recover};
use crate::{enroll, identity, ops, render};

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
    // The Claude Code adapter, wired to the real config-store seam (the same `RealFs`). It resolves
    // Claude Code's own config home (`$CLAUDE_CONFIG_DIR` else `$HOME/.claude`) so a relocated config is
    // honored; it touches that home only when adopting a recognized skill or on uninstall.
    let harness = ClaudeCode::new(ClaudeCode::resolve_home(), &fs);

    // Recovery runs at the start of every command (it also sweeps a torn enrollment-doc temp).
    if let Err(e) = recover(&fs, &layout) {
        return emit_err(json, cmd_name, &e);
    }

    // The plane + follow-state sources. When enrollment has been written (`instance.json` present AND
    // `follows.json` has at least one followed skill), wire the REAL `ureq` transport + the on-disk follow
    // state + the pinned plane key; otherwise keep the INERT pair (production stays a truthful no-op until
    // enrollment writes those docs — nothing writes them yet). The inert ZSTs and the loaded enrollment are
    // both stack locals so the `&dyn` trait objects below borrow correctly for the rest of `run`.
    let inert_plane = crate::plane::InertPlane;
    let inert_follow = crate::plane::InertFollow;
    let enrollment = match load_enrollment(&fs, &layout) {
        Ok(e) => e,
        Err(e) => return emit_err(json, cmd_name, &e),
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
        Command::Add { .. } | Command::Pull { .. } => {
            match identity::load_or_create_device_id(&fs, &layout) {
                Ok(d) => d,
                Err(e) => return emit_err(json, cmd_name, &e),
            }
        }
        _ => String::new(),
    };
    let ctx = Ctx {
        fs: &fs,
        ids: &ids,
        clock: &clock,
        device_id,
        layout,
        harness: &harness,
        plane,
        plane_key,
        follow,
    };

    match command {
        Command::Add { path } => finish(json, cmd_name, ops::add(&ctx, &path), render::add_tty),
        Command::List { skill, footprint } => finish(
            json,
            cmd_name,
            ops::list(&ctx, skill.as_deref(), footprint),
            render::list_tty,
        ),
        Command::Diff { skill } => {
            finish(json, cmd_name, ops::diff(&ctx, &skill), render::diff_tty)
        }
        Command::Log { skill } => finish(json, cmd_name, ops::log(&ctx, &skill), render::log_tty),
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
                // stdout, even under `--json` (which `--quiet` overrides for the hook path).
                match result {
                    Ok(_) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("{}", render::err_tty(&e));
                        ExitCode::FAILURE
                    }
                }
            } else {
                finish(json, cmd_name, result, render::pull_tty)
            }
        }
        Command::Uninstall { footprint } => {
            let binary = std::env::current_exe().ok();
            finish(
                json,
                cmd_name,
                ops::uninstall(&ctx, footprint, binary.as_deref()),
                render::uninstall_tty,
            )
        }
    }
}

fn finish<T: Serialize>(
    json: bool,
    command: &str,
    result: Result<T, ClientError>,
    tty: impl Fn(&T) -> String,
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
        Err(e) => emit_err(json, command, &e),
    }
}

fn emit_err(json: bool, command: &str, err: &ClientError) -> ExitCode {
    if json {
        println!("{}", render::to_json(&render::err_envelope(command, err)));
    } else {
        eprintln!("{}", render::err_tty(err));
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
/// `--onto-current` (the escape) requires a `<skill>` target and is mutually exclusive with `@<hash>`.
fn build_pull_scope(
    skill: Option<String>,
    onto_current: bool,
) -> Result<ops::PullScope, ClientError> {
    let Some(arg) = skill else {
        if onto_current {
            return Err(ClientError::PlacementUnsupported {
                reason: "--onto-current requires a <skill> target".into(),
            });
        }
        return Ok(ops::PullScope::AllFollowed);
    };
    if let Some((name, suffix)) = arg.rsplit_once('@')
        && let Ok(hash) = ops::parse_hex32(suffix)
    {
        if onto_current {
            return Err(ClientError::PlacementUnsupported {
                reason: "--onto-current is not valid with @<hash>".into(),
            });
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

/// Load the enrollment docs read-only. Returns `Some` ONLY when `instance.json` is present AND
/// `follows.json` names at least one followed skill — so the bare `pull` stays an honest no-op until a real
/// enrollment exists. A corrupt / newer-schema doc fails closed (propagated), never silently degraded to
/// inert. (This increment never WRITES these docs — see [`crate::enroll`] for the `0600` requirement the
/// future enrollment writer must honor for `follows.json`'s secret read tokens.)
fn load_enrollment(fs: &dyn FsOps, layout: &Layout) -> Result<Option<Enrollment>, ClientError> {
    let Some(instance) = enroll::read_instance(fs, layout)? else {
        return Ok(None);
    };
    let Some(follows) = enroll::read_follows(fs, layout)? else {
        return Ok(None);
    };
    if !follows.follows.iter().any(|f| f.following) {
        return Ok(None);
    }
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
