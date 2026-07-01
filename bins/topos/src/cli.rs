//! The `clap` surface. Thin: it only parses argv; every verb's logic lives in the lib over the seams.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use topos_types::requests::WorkspaceRole;

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
    /// Enroll with a plane via an `/i/` invite link, then follow its skills. Two-call resume: `follow
    /// <link>` returns a verification URL; `follow --resume` polls + completes. `follow --approve
    /// <skill>[@<hash>]` places a disclosed first-receive offer.
    Follow {
        /// The `/i/<token>` invite link (the full URL, or a bare token once already enrolled). Omitted
        /// with `--resume` / `--approve`.
        link: Option<String>,
        /// Adopt followed skills in confirm-each mode (a one-tap accept per new version) instead of auto.
        #[arg(long)]
        manual: bool,
        /// Poll a pending enrollment (started by an earlier `follow <link>`) and complete it.
        #[arg(long)]
        resume: bool,
        /// Place the named, already-disclosed first-receive offer(s): `<skill>` or `<skill>@<hash>`.
        #[arg(long = "approve")]
        approve: Vec<String>,
    },
    /// Stop following a skill's `current`. Your local copy is KEPT as a frozen copy (nothing is deleted);
    /// auto-updates stop, and a later `follow` resumes. Local-only.
    Unfollow {
        /// The skill name to stop following.
        skill: String,
    },
    /// Mint an `/i/<token>` invite link (OWNER only): sign the governance Invite op with this device's key,
    /// then POST it. Seeds the invited emails onto the workspace roster and pre-offers the named skills.
    /// Requires prior enrollment (run `follow` first); the link itself never carries a role.
    Invite {
        /// The emails to invite (0 or more). Seeded onto the roster as `invited`; bound as a set in the
        /// signed governance frame (order is irrelevant — the kernel sorts + dedups).
        emails: Vec<String>,
        /// The role the invitees are granted; omitted defaults to `member` (the least-privilege default).
        #[arg(long)]
        role: Option<RoleArg>,
        /// The skill ids to pre-offer on the invite (bound as a set in the signed frame).
        #[arg(long = "skills")]
        skills: Vec<String>,
    },
    /// Inventory the skills on this machine.
    List {
        /// Narrow to one skill by name (errors if the name is ambiguous).
        skill: Option<String>,
        /// Also report the paths topos owns outside skill directories.
        #[arg(long)]
        footprint: bool,
    },
    /// Show a skill's change. Bare = draft ↔ current; `<hash>` / `@<hash>` reviews that version against
    /// current (`current..<hash>` — a proposal IS a version); `<a>..<b>` = version ↔ version. `--json`
    /// emits the target digest + `source: local|plane`.
    Diff {
        /// The skill name.
        skill: String,
        /// The optional ref: `<hash>` / `@<hash>` / `current..<hash>` / `<a>..<b>`. Omitted = draft ↔ current.
        #[arg(value_name = "REF")]
        r#ref: Option<String>,
    },
    /// Ship a draft to the team. `publish` moves `current` to your draft (or genesis-creates a never-yet-
    /// published skill); `--propose` opens a PR **without** moving `current`. Gated by `--approve
    /// <skill>@<digest>` (the disclosed consent digest matching the bytes being shipped). Under
    /// review-required a direct publish fails typed — re-run as `--propose` (never an auto-flip). Requires
    /// prior enrollment (`follow` first). Device-signed; roster-gated.
    Publish {
        /// The skill to publish — optional; inferred from the `--approve <skill>@<digest>` token if omitted
        /// (they must agree when both are given).
        skill: Option<String>,
        /// Open a proposal (a PR) instead of moving `current`.
        #[arg(long)]
        propose: bool,
        /// The consent token `<skill>@<digest>` matching the bytes being shipped (required).
        #[arg(long = "approve")]
        approve: String,
    },
    /// Resolve a proposal (the `gh pr review --approve` model). `--approve` moves `current` to the candidate
    /// (a compare-and-set on its base; a stale base re-dos); `--reject` declines a proposal (reviewer) or
    /// withdraws your own (proposer). Exactly one of `--approve` / `--reject` is required. Device-signed.
    Review {
        /// The proposal to resolve, as `<skill>@<hash>`.
        target: String,
        /// Approve the proposal — move `current` to the candidate.
        #[arg(long)]
        approve: bool,
        /// Reject the proposal (reviewer) or withdraw your own (proposer).
        #[arg(long)]
        reject: bool,
    },
    /// Undo a release for the TEAM: move `current` to the older version named by `--to` — a **forward**
    /// pointer-move (nothing deleted; invertible). `--to <hash>` is the GOOD version you go back TO (not the
    /// bad one). `--approve <skill>@<hash>` binds that good version. `--confirm` for a no-op (already-current)
    /// revert. Team-only — the local go-back is `pull <skill>@<hash>`. Device-signed; roster-gated.
    Revert {
        /// The skill to revert (optional — inferred from the `--approve <skill>@<hash>` token if omitted).
        skill: Option<String>,
        /// The GOOD version id (64-char hex) to restore — the destination, NOT the bad version.
        #[arg(long = "to")]
        to: String,
        /// The consent token `<skill>@<hash>` naming the same good version as `--to` (required).
        #[arg(long = "approve")]
        approve: String,
        /// Acknowledge a no-op revert (the `--to` version is already `current`).
        #[arg(long)]
        confirm: bool,
    },
    /// Show a skill's local action log + embedded-git history.
    Log {
        /// The skill name.
        skill: String,
    },
    /// Check for and apply updates to followed skills — the harness currency entry point. Bare = the
    /// sweep over every followed skill (the installed currency trigger runs `pull --quiet`). `<skill>` accepts a
    /// pending update for one skill (or resumes a held one); `<skill>@<hash>` goes back to that version.
    Pull {
        /// Optional target: `<name>` accepts a pending update / resumes a hold / resolves a divergence;
        /// `<name>@<hash>` goes back to that version's bytes. Omitted = sweep every followed skill.
        skill: Option<String>,
        /// Resolve a diverged draft via the escape: commit YOUR bytes on top of `current`, dropping the
        /// merge (the dropped changes are disclosed). Requires a `<skill>` target; not valid with `@<hash>`.
        #[arg(long = "onto-current")]
        onto_current: bool,
        /// Emit nothing on stdout (the session-start hook's stdout is injected into the session). Errors
        /// still go to stderr with a non-zero exit. Overrides `--json`.
        #[arg(long)]
        quiet: bool,
    },
    /// Remove topos: scrub the harness currency hook, then delete the binary + `~/.topos/`. Touches no
    /// skill bytes.
    Uninstall {
        /// First report the paths topos owns outside skill directories.
        #[arg(long)]
        footprint: bool,
    },
}

/// The workspace role an invite grants, as a CLI arg — maps 1:1 to [`WorkspaceRole`]. The client signs the
/// SAME role byte the plane re-derives + verifies, so this mapping is load-bearing.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum RoleArg {
    /// Full governance authority (invite, roster, revoke).
    Owner,
    /// A reviewer (review-gate authority; no governance authority in v0).
    Reviewer,
    /// An ordinary member (no governance authority).
    Member,
}

impl RoleArg {
    /// Map to the wire [`WorkspaceRole`]. The op then maps that to the governance signing byte the plane
    /// agrees on (Owner=1, Reviewer=2, Member=3).
    pub(crate) fn to_workspace_role(self) -> WorkspaceRole {
        match self {
            RoleArg::Owner => WorkspaceRole::Owner,
            RoleArg::Reviewer => WorkspaceRole::Reviewer,
            RoleArg::Member => WorkspaceRole::Member,
        }
    }
}

impl Command {
    /// The verb name carried in the `--json` envelope + receipt.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Command::Add { .. } => "add",
            Command::Follow { .. } => "follow",
            Command::Unfollow { .. } => "unfollow",
            Command::Invite { .. } => "invite",
            Command::List { .. } => "list",
            Command::Diff { .. } => "diff",
            Command::Publish { .. } => "publish",
            Command::Review { .. } => "review",
            Command::Revert { .. } => "revert",
            Command::Log { .. } => "log",
            Command::Pull { .. } => "pull",
            Command::Uninstall { .. } => "uninstall",
        }
    }
}
