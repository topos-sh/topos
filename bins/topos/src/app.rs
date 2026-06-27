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

    // Recovery runs at the start of every command.
    if let Err(e) = recover(&fs, &layout) {
        return emit_err(json, cmd_name, &e);
    }

    // Only `add` authors a commit, so only `add` loads (and on first use, mints) the device identity —
    // `uninstall` must never create or require it before tearing the home down.
    let device_id = match &command {
        Command::Add { .. } => match identity::load_or_create_device_id(&fs, &layout) {
            Ok(d) => d,
            Err(e) => return emit_err(json, cmd_name, &e),
        },
        _ => String::new(),
    };
    let ctx = Ctx {
        fs: &fs,
        ids: &ids,
        clock: &clock,
        device_id,
        layout,
        harness: &harness,
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
        Command::Pull { quiet } => {
            let result = ops::pull(&ctx);
            if quiet {
                // Byte-silent stdout: the session-start hook injects stdout into the session context, so
                // the no-op emits nothing. An error (none today) surfaces on stderr with a non-zero exit
                // — never on stdout, even under `--json` (which `--quiet` overrides for the hook path).
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
