//! The composition root for the binary: parse argv, wire the real seams, run recovery, dispatch a verb,
//! and emit either the `--json` envelope (stdout) or thin TTY text.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use serde::Serialize;

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

    // Recovery + the host identity are part of starting any command.
    let device_id = match startup(&fs, &layout) {
        Ok(d) => d,
        Err(e) => return emit_err(json, cmd_name, &e),
    };
    let ctx = Ctx {
        fs: &fs,
        ids: &ids,
        clock: &clock,
        device_id,
        layout,
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

fn startup(fs: &RealFs, layout: &Layout) -> Result<String, ClientError> {
    recover(fs, layout)?;
    identity::load_or_create_device_id(fs, layout)
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
