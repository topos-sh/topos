//! The `clap` surface. Thin: it only parses argv; every verb's logic lives in the lib over the seams.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// `topos` — share agent skills across a team. The agent drives it non-interactively with `--json`.
#[derive(Debug, Parser)]
#[command(name = "topos", version, about = "Share agent behaviors across a team")]
pub(crate) struct Cli {
    /// Emit one JSON document on stdout (the agent surface) instead of human text. Never prompts.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    #[command(subcommand)]
    pub(crate) command: Command,
}

/// The local, accountless verbs available this increment.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Adopt a local skill into topos (offline; no server, no account).
    Add {
        /// The skill directory to adopt.
        path: PathBuf,
    },
    /// Inventory the skills on this machine.
    List {
        /// Narrow to one skill by name (errors if the name is ambiguous).
        skill: Option<String>,
        /// Also report the paths topos owns outside skill directories.
        #[arg(long)]
        footprint: bool,
    },
    /// Show a skill's local draft against its current version.
    Diff {
        /// The skill name.
        skill: String,
    },
    /// Show a skill's local action log + embedded-git history.
    Log {
        /// The skill name.
        skill: String,
    },
    /// Remove topos: the binary + `~/.topos/`. Touches no skill bytes.
    Uninstall {
        /// First report the paths topos owns under the home directory.
        #[arg(long)]
        footprint: bool,
    },
}

impl Command {
    /// The verb name carried in the `--json` envelope + receipt.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Command::Add { .. } => "add",
            Command::List { .. } => "list",
            Command::Diff { .. } => "diff",
            Command::Log { .. } => "log",
            Command::Uninstall { .. } => "uninstall",
        }
    }
}
