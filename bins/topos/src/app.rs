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
use crate::fs_seam::RealFs;
use crate::ids::{RealClock, RealIds};
use crate::sidecar::{Layout, recover};
use crate::{identity, ops, render};

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
    // The plane + follow-state sources. Production wires the INERT pair (no enrollment, no HTTP transport
    // yet), so `pull` follows nothing and stays a truthful no-op; the sync engine, floor, and materializer
    // are real and exercised by fixture-driven tests. The pinned plane key lands with enrollment.
    let plane = crate::plane::InertPlane;
    let follow = crate::plane::InertFollow;
    let plane_key = [0u8; 32];

    // Recovery runs at the start of every command.
    if let Err(e) = recover(&fs, &layout) {
        return emit_err(json, cmd_name, &e);
    }

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
        plane: &plane,
        plane_key,
        follow: &follow,
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
        Command::Pull { skill, quiet } => {
            let result = build_pull_scope(skill).and_then(|scope| ops::pull(&ctx, scope));
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
/// pending update; `<name>@<hash>` = go back to that version's bytes.
fn build_pull_scope(skill: Option<String>) -> Result<ops::PullScope, ClientError> {
    let Some(arg) = skill else {
        return Ok(ops::PullScope::AllFollowed);
    };
    match arg.split_once('@') {
        None => Ok(ops::PullScope::One {
            name: arg,
            mode: ops::TargetMode::AcceptPending,
        }),
        Some((name, hash)) => Ok(ops::PullScope::One {
            name: name.to_owned(),
            mode: ops::TargetMode::GoBack(ops::parse_hex32(hash)?),
        }),
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
