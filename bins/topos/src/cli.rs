//! The `clap` surface. Thin: it only parses argv; every verb's logic lives in the lib over the seams.

use clap::{Parser, Subcommand, ValueEnum};

use topos_types::requests::WorkspaceRole;

/// `topos` — share agent skills across a team. The agent drives it non-interactively with `--json`.
#[derive(Debug, Parser)]
#[command(name = "topos", version, about = "Share agent behaviors across a team")]
pub(crate) struct Cli {
    /// Emit one JSON document on stdout (the agent surface) instead of human text. Never prompts.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    /// Act in a specific workspace when this install follows skills from more than one on the same plane.
    /// Selects the workspace for the ambient write verbs (a genesis `publish`, `invite`) and disambiguates
    /// a skill name shared across workspaces (`publish`/`review`/`revert`, and the `follow` positional
    /// skill). Optional — with a single workspace it is inferred.
    #[arg(long, global = true, value_name = "ID")]
    pub(crate) workspace: Option<String>,

    #[command(subcommand)]
    pub(crate) command: Command,
}

/// The local, accountless verbs available this increment.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Adopt a skill into topos. The source is polymorphic:
    ///   • a skill NAME (`deploy`, `deploy@claude-code`) — resolved against the untracked skills
    ///     `topos list` discovers (`@<harness>` disambiguates across harnesses);
    ///   • a PATH (`./skills/deploy`, `~/x`, `/abs`) — adopt that directory in place;
    ///   • a REMOTE source (`owner/repo`, `owner/repo#<ref>`, an https://github.com URL, incl. a
    ///     `/tree/<ref>/<subdir>` URL) — fetch it and adopt it.
    /// Local adopts are offline. A remote import fetches a public repo (no account); the source's
    /// trustworthiness is yours to verify.
    Add {
        /// The skill to adopt — a name, a path, or a remote `owner/repo`/github.com URL (see the command
        /// help). Path shapes (`./ ../ ~/ /`) adopt in place; `owner/repo` is a GitHub shorthand.
        source: String,
        /// Pick ONE skill from a repo that holds several (a remote source). A lone skill needs no `--skill`;
        /// several without it is a typed error listing the choices.
        #[arg(long, short = 's', value_name = "NAME")]
        skill: Option<String>,
        /// Land a remote import into THIS harness's skills dir (a registry slug, e.g. `cursor`). Default:
        /// the active harness. Ignored for a local path / name adopt (those stay where they are).
        #[arg(long, value_name = "SLUG")]
        harness: Option<String>,
        /// Land a remote import in the harness's global/user skills dir instead of the project (cwd) dir.
        #[arg(long, short = 'g')]
        global: bool,
    },
    /// Enroll with a plane and follow its skills, or place/resume a followed skill — dispatched by the
    /// single positional. `follow <link>` (an `/i/` invite, a one-time admin CLAIM link, or a bare token
    /// once enrolled) starts enrollment; `follow <skill>[@<hash>]` places a disclosed first-receive offer
    /// (or resumes a skill `unfollow` paused). A device-flow enrollment returns a verification URL; while
    /// one is pending, re-invoking `follow` (with any target, or none) RESUMES it — no separate flag.
    Follow {
        /// An `/i/<token>` invite or claim link (the full URL, or a bare token once enrolled) to enroll —
        /// OR a followed skill name, optionally `<skill>@<hash>`, to place its offer / resume it. Omitted,
        /// it resumes a pending enrollment.
        target: Option<String>,
        /// Adopt followed skills in confirm-each mode (a one-tap accept per new version) instead of auto.
        #[arg(long)]
        manual: bool,
        /// Block until the browser approval settles, finishing enrollment in ONE command (no manual
        /// re-run). Bare `--wait` waits until the code expires; `--wait <seconds>` caps the wait. Without
        /// `--wait`, an agent (`--json`) run returns the pending state immediately; an interactive run
        /// always waits. Put `--wait` AFTER any positional (its value binds greedily).
        #[arg(long, value_name = "SECONDS", num_args = 0..=1)]
        wait: Option<Option<u64>>,
    },
    /// Stop following a skill's `current`. Your local copy is KEPT as a frozen copy (nothing is deleted);
    /// auto-updates stop, and `follow <skill>` resumes. Local-only.
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
    /// Inventory the skills on this machine. By default also discovers **untracked** skills sitting in
    /// any known harness's skill dir (across the baked registry) that topos could `add`.
    List {
        /// Narrow to one skill by name (errors if the name is ambiguous).
        skill: Option<String>,
        /// Also report the paths topos owns outside skill directories.
        #[arg(long)]
        footprint: bool,
        /// Show only locally-tracked skills — skip discovery of untracked harness-dir skills.
        #[arg(long)]
        tracked: bool,
        /// Also list skills available in the workspace(s) you follow (the remote catalog), annotated
        /// with your follow-state. Requires enrollment; `--workspace <id>` narrows.
        #[arg(long)]
        remote: bool,
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
    /// Ship a draft to the team, ADDING the skill to topos first if it isn't tracked yet. The target is a
    /// tracked skill NAME, or an untracked LOCAL source topos adopts before publishing — a discovered
    /// `<skill>` / `<skill>@<harness>` (disambiguates across harnesses) or a `<dir>` path (adopted in
    /// place) — so one command does `add` + `publish`. (A remote `owner/repo`/URL is NOT adopted here — run
    /// `topos add <source>` first.) `publish` moves `current` to your draft (or genesis-creates a never-yet-
    /// published skill); `--propose` opens a PR **without** moving `current`. Pin the bytes with an optional
    /// `@<digest>` suffix (the disclosed consent digest — when present it must match the bytes being
    /// shipped, refusing on mismatch; when absent the computed digest just ships). Under review-required a
    /// direct publish fails typed — re-run as `--propose` (never an auto-flip). Un-enrolled, a direct
    /// publish STANDS UP a workspace on the hosted plane (a human signs in to approve; an interactive run
    /// then auto-creates the workspace and publishes in the same command, and `--wait` makes an agent run
    /// block until it does); `--propose` still requires prior enrollment (`follow` first). Device-signed;
    /// roster-gated.
    Publish {
        /// The skill to publish: a tracked NAME, an untracked `<skill>` / `<skill>@<harness>` to adopt from
        /// discovery, or a `<dir>` to adopt in place — optionally pinned as `<source>@<digest>` (the
        /// byte-exact consent digest of the bytes being shipped).
        target: String,
        /// Open a proposal (a PR) instead of moving `current`.
        #[arg(long)]
        propose: bool,
        /// Block until the browser sign-in settles, then auto-create the workspace and publish in ONE
        /// command (the un-enrolled standup path). Bare `--wait` waits until the code expires; `--wait
        /// <seconds>` caps the wait. Without `--wait`, an agent (`--json`) run returns the pending state
        /// immediately; an interactive run always waits. Put `--wait` AFTER the positional target.
        #[arg(long, value_name = "SECONDS", num_args = 0..=1)]
        wait: Option<Option<u64>>,
    },
    /// Resolve a proposal (the `gh pr review --approve` model). `--approve` moves `current` to the candidate
    /// (a compare-and-set on its base; a stale base re-dos); `--reject` declines a proposal (reviewer) or
    /// withdraws your own (proposer). Exactly one of `--approve` / `--reject` is required — enforced by
    /// clap (the `verdict` group), so a violation is a standard usage error at exit 2. Device-signed.
    #[command(group(clap::ArgGroup::new("verdict").required(true).args(["approve", "reject"])))]
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
    /// pointer-move (nothing deleted; invertible). `--to <hash>` is the sole source of the GOOD version you
    /// go back TO (not the bad one). `--confirm` for a no-op (already-current) revert. Team-only — the local
    /// go-back is `pull <skill>@<hash>`. Device-signed; roster-gated.
    Revert {
        /// The skill to revert.
        skill: String,
        /// The GOOD version id (64-char hex, or a unique ≥8-char prefix) to restore — the destination, NOT
        /// the bad version.
        #[arg(long = "to")]
        to: String,
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
        /// merge (the dropped changes are disclosed). Requires a `<skill>` target (enforced by clap);
        /// not valid with `@<hash>`.
        #[arg(long = "onto-current", requires = "skill")]
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
    /// Update the `topos` binary itself to the latest release, verifying the download's sha256 against the
    /// release SHA256SUMS (never skippable) and replacing the running binary atomically. A MAINTENANCE
    /// command — it touches no skills, no plane, no account, and mints no device identity.
    Upgrade {
        /// Only check whether a newer release exists; report and exit without downloading or replacing.
        #[arg(long)]
        check: bool,
        /// Install a specific release tag (e.g. v0.2.0) instead of the latest — allows a pinned downgrade.
        /// Bypasses the latest-version check and downloads that tag directly.
        #[arg(long, value_name = "TAG")]
        version: Option<String>,
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
            Command::Upgrade { .. } => "upgrade",
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;
    use clap::{CommandFactory, Parser};

    use super::{Cli, Command};

    #[test]
    fn cli_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn review_verdict_is_a_clap_usage_error_at_exit_2() {
        // Both flags is a conflict; neither is missing-required — both are STANDARD usage errors
        // (help hint, exit code 2), never a CORRUPT_STATE envelope.
        let both = Cli::try_parse_from(["topos", "review", "docs@abc", "--approve", "--reject"])
            .unwrap_err();
        assert_eq!(both.kind(), ErrorKind::ArgumentConflict);
        assert_eq!(both.exit_code(), 2);
        let neither = Cli::try_parse_from(["topos", "review", "docs@abc"]).unwrap_err();
        assert_eq!(neither.kind(), ErrorKind::MissingRequiredArgument);
        assert_eq!(neither.exit_code(), 2);
    }

    #[test]
    fn pull_onto_current_requires_a_skill_target_in_clap() {
        let err = Cli::try_parse_from(["topos", "pull", "--onto-current"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
        assert_eq!(err.exit_code(), 2);
        // With the target present it parses (the `@<hash>` exclusivity stays a runtime usage error).
        assert!(Cli::try_parse_from(["topos", "pull", "docs", "--onto-current"]).is_ok());
    }

    #[test]
    fn publish_requires_a_single_positional_target_and_has_no_approve_flag() {
        // A bare `publish` with no positional is a standard missing-required usage error at exit 2.
        let missing = Cli::try_parse_from(["topos", "publish"]).unwrap_err();
        assert_eq!(missing.kind(), ErrorKind::MissingRequiredArgument);
        assert_eq!(missing.exit_code(), 2);
        // The bare skill and the `<skill>@<digest>` pin both parse as the single positional.
        assert!(Cli::try_parse_from(["topos", "publish", "docs"]).is_ok());
        let pinned = format!("docs@{}", "ab".repeat(32));
        assert!(Cli::try_parse_from(["topos", "publish", &pinned, "--propose"]).is_ok());
        // The removed `--approve` flag is now an unknown-argument usage error.
        let approve =
            Cli::try_parse_from(["topos", "publish", "docs", "--approve", &pinned]).unwrap_err();
        assert_eq!(approve.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn revert_requires_a_skill_and_to_and_has_no_approve_flag() {
        let hash = "ab".repeat(32);
        assert!(Cli::try_parse_from(["topos", "revert", "docs", "--to", &hash]).is_ok());
        // Skill is now required.
        let no_skill = Cli::try_parse_from(["topos", "revert", "--to", &hash]).unwrap_err();
        assert_eq!(no_skill.kind(), ErrorKind::MissingRequiredArgument);
        // The removed `--approve` flag is an unknown argument.
        let approve = Cli::try_parse_from([
            "topos",
            "revert",
            "docs",
            "--to",
            &hash,
            "--approve",
            &format!("docs@{hash}"),
        ])
        .unwrap_err();
        assert_eq!(approve.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn add_takes_one_polymorphic_source_positional() {
        // A name, a `<skill>@<harness>` name, a path, and a remote all parse as the single positional.
        assert!(Cli::try_parse_from(["topos", "add", "deploy"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "add", "deploy@claude-code"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "add", "./skills/deploy"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "add", "vercel-labs/agent-skills"]).is_ok());
        // The remote flags parse (short forms too).
        assert!(
            Cli::try_parse_from([
                "topos",
                "add",
                "vercel-labs/agent-skills",
                "-s",
                "web-design",
                "--harness",
                "cursor",
                "-g",
            ])
            .is_ok()
        );
        // The source is required: omitting it is a missing-required usage error at exit 2.
        let neither = Cli::try_parse_from(["topos", "add"]).unwrap_err();
        assert_eq!(neither.kind(), ErrorKind::MissingRequiredArgument);
        assert_eq!(neither.exit_code(), 2);
        // `--path` is gone (path-ness is now inferred from the positional's shape).
        let removed = Cli::try_parse_from(["topos", "add", "--path", "/tmp/x"]).unwrap_err();
        assert_eq!(removed.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn follow_has_an_optional_positional_and_no_resume_or_approve_flags() {
        // Bare, a link, and a skill positional all parse.
        assert!(Cli::try_parse_from(["topos", "follow"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "follow", "https://topos.sh/i/tok"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "follow", "docs", "--manual"]).is_ok());
        // The removed `--resume` / `--approve` flags are unknown arguments.
        assert_eq!(
            Cli::try_parse_from(["topos", "follow", "--resume"])
                .unwrap_err()
                .kind(),
            ErrorKind::UnknownArgument
        );
        assert_eq!(
            Cli::try_parse_from(["topos", "follow", "--approve", "docs"])
                .unwrap_err()
                .kind(),
            ErrorKind::UnknownArgument
        );
    }

    #[test]
    fn wait_flag_is_an_optional_valued_flag_on_publish_and_follow() {
        // Absent ⇒ None; bare `--wait` ⇒ Some(None) (wait until the code expires); `--wait <n>` ⇒
        // Some(Some(n)) (cap the wait). The value binds greedily, so `--wait` goes AFTER the positional.
        let absent = Cli::try_parse_from(["topos", "publish", "docs"]).unwrap();
        let bare = Cli::try_parse_from(["topos", "publish", "docs", "--wait"]).unwrap();
        let valued = Cli::try_parse_from(["topos", "publish", "docs", "--wait", "300"]).unwrap();
        assert!(matches!(
            absent.command,
            Command::Publish { wait: None, .. }
        ));
        assert!(matches!(
            bare.command,
            Command::Publish {
                wait: Some(None),
                ..
            }
        ));
        assert!(matches!(
            valued.command,
            Command::Publish {
                wait: Some(Some(300)),
                ..
            }
        ));
        // Same shape on `follow` (bare here — no positional to compete for the value).
        let follow_bare = Cli::try_parse_from(["topos", "follow", "--wait"]).unwrap();
        assert!(matches!(
            follow_bare.command,
            Command::Follow {
                wait: Some(None),
                ..
            }
        ));
        // A non-numeric value is a parse error (not silently swallowed).
        assert!(Cli::try_parse_from(["topos", "publish", "docs", "--wait", "soon"]).is_err());
    }
}
