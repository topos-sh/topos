//! The `clap` surface. Thin: it only parses argv; every verb's logic lives in the lib over the seams.
//!
//! The behavior verbs are grouped by SCOPE — self-scoped (affect only your machine), team-scoped (change
//! shared state), and maintenance. The reshaped verbs run the FULL resolution grammar + the two-phase
//! describe/`--yes` flow (`crate::resolve`): `follow`/`unfollow`/`auth`, plus `remove`, `channel
//! add/remove`, `protect`, the review inbox/describe, `invite`'s bare read/describe, `update --reset`,
//! `publish`'s describe, and the log/list plane columns. A few tails still parse here and refuse with a
//! marked seam at dispatch (see `ops::not_yet`): the `update`/`list` `--channel`/`--skill` selectors +
//! multi-target, `add`'s `'*'` (all-skills/all-agents) selector, and the keep-as-yours re-adopt.

use clap::{Parser, Subcommand};

/// The full `clap` command tree, built from the derived `Cli`. The ONE source of truth for both argv
/// parsing and the generated command reference (`cargo xtask gen-cli-ref` renders `docs/cli.md` from
/// this), so the reference can never drift from what the binary actually accepts.
#[must_use]
pub fn cli_command() -> clap::Command {
    <Cli as clap::CommandFactory>::command()
}

/// `topos` — share agent behaviors across a team. The agent drives it non-interactively with `--json`.
#[derive(Debug, Parser)]
#[command(name = "topos", version, about = "Share agent behaviors across a team")]
pub(crate) struct Cli {
    /// Emit one JSON document on stdout (the agent surface) instead of human text. Never prompts.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    /// Act in a specific workspace when this install follows skills from more than one on the same plane.
    /// Accepts the workspace's address NAME (what you joined by) or its opaque id. Selects the workspace
    /// for the ambient team verbs (a genesis `publish`, `invite`) and disambiguates a skill name shared
    /// across workspaces. Optional — with a single workspace it is inferred.
    #[arg(long, global = true, value_name = "WORKSPACE")]
    pub(crate) workspace: Option<String>,

    #[command(subcommand)]
    pub(crate) command: Command,
}

/// The verb tree. Ordered by scope (self-scoped, then team-scoped, then maintenance).
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    // ---- Self-scoped (affect only you) ----
    /// Follow a workspace, channel, or skill — enroll if needed, then subscribe two-phase (a bare
    /// invocation DESCRIBES what would land; `--yes` applies). Targets: a workspace address
    /// (`https://topos.sh/acme`, or a bare workspace name), a bare SERVER address with no workspace
    /// slug (`https://topos.example.com`, or the schemeless `topos.example.com`) — "the workspace
    /// that origin addresses", the single-tenant install form, a qualified path
    /// (`acme/channels/eng`, `acme/skills/deploy`), or a bare channel/skill name. A first follow
    /// enrolls this device: open the printed approval URL in a browser, check the code matches, and
    /// approve — the device then holds ONE credential for everything your seats reach. `follow
    /// <skill>` on a KNOWN followed skill places its disclosed first-receive offer (or resumes a
    /// skill `unfollow` paused). While an enrollment is pending, re-invoking `follow` RESUMES it.
    Follow {
        /// The follow targets (addresses, qualified paths, or names). Omitted, it resumes a
        /// pending enrollment.
        targets: Vec<String>,
        /// Follow a channel by name (repeatable; kind-forced).
        #[arg(long, value_name = "NAME")]
        channel: Vec<String>,
        /// Follow a specific skill by name (repeatable; kind-forced).
        #[arg(long, value_name = "NAME")]
        skill: Vec<String>,
        /// Apply the described subscription (the one-shot consent). Bare = describe only.
        #[arg(long)]
        yes: bool,
        /// Install a dirname-colliding skill under `<workspace>.<name>` instead of declining it.
        #[arg(long)]
        prefix_dirname: bool,
        /// Adopt followed skills in confirm-each mode (a one-tap accept per new version) instead of auto.
        #[arg(long)]
        manual: bool,
        /// Block until the browser approval settles, finishing enrollment in ONE command. Bare `--wait`
        /// waits until the code expires; `--wait <seconds>` caps the wait. Put `--wait` AFTER any positional.
        #[arg(long, value_name = "SECONDS", num_args = 0..=1)]
        wait: Option<Option<u64>>,
    },
    /// Stop following a skill or channel — two-phase (bare describes what stops; `--yes` applies).
    /// Delivery ends on EVERY device of yours; local copies are KEPT as frozen copies (nothing is
    /// deleted) and `follow` re-attaches. A workspace cannot be left here (that is a web action),
    /// and the structural `everyone` cannot be left at all.
    Unfollow {
        /// The channel/skill name(s) (or qualified paths) to stop following.
        targets: Vec<String>,
        /// Unfollow a channel by name (repeatable; kind-forced).
        #[arg(long, value_name = "NAME")]
        channel: Vec<String>,
        /// Unfollow a specific skill by name (repeatable; kind-forced).
        #[arg(long, value_name = "NAME")]
        skill: Vec<String>,
        /// Apply the described detach (the one-shot consent). Bare = describe only.
        #[arg(long)]
        yes: bool,
    },
    /// Check for and apply updates to followed skills — the harness currency entry point. Bare = the sweep
    /// over every followed skill (the installed currency trigger runs `update --quiet`). `<skill>` accepts a
    /// pending update for one skill (or resumes a held one); `<skill>@<hash>` goes back to that version.
    #[command(alias = "pull")]
    Update {
        /// Optional target(s): `<name>` accepts a pending update / resumes a hold / resolves a divergence;
        /// `<name>@<hash>` goes back to that version's bytes. Omitted = sweep every followed skill.
        targets: Vec<String>,
        /// Update only this channel's skills (repeatable). Lands with the full resolution grammar.
        #[arg(long, value_name = "NAME")]
        channel: Vec<String>,
        /// Update only this skill (repeatable). Lands with the full resolution grammar.
        #[arg(long, value_name = "NAME")]
        skill: Vec<String>,
        /// Reset a followed skill to `current`, dropping local edits. Lands with the loss-led describe.
        #[arg(long)]
        reset: bool,
        /// Apply without the describe step. Parses today; the two-phase describe lands later.
        #[arg(long)]
        yes: bool,
        /// Resolve a diverged draft via the escape: commit YOUR bytes on top of `current`, dropping the
        /// merge (the dropped changes are disclosed). Requires exactly one `<skill>` target.
        #[arg(long = "onto-current", hide = true)]
        onto_current: bool,
        /// Emit nothing on stdout (the session-start hook's stdout is injected into the session). Errors
        /// still go to stderr with a non-zero exit. Overrides `--json`.
        #[arg(long)]
        quiet: bool,
    },
    /// Adopt a skill into topos. The source is polymorphic:
    ///   • a skill NAME (`deploy`, `deploy@claude-code`) — resolved against the untracked skills
    ///     `topos list` discovers (`@<harness>` disambiguates across harnesses);
    ///   • a PATH (`./skills/deploy`, `~/x`, `/abs`) — adopt that directory in place;
    ///   • a REMOTE source (`owner/repo`, `owner/repo#<ref>`, an https://github.com URL) — fetch it.
    /// Local adopts are offline. A remote import fetches a public repo (no account); the source's
    /// trustworthiness is yours to verify.
    Add {
        /// The skill to adopt — a name, a path, or a remote `owner/repo`/github.com URL.
        source: String,
        /// Pick a skill from a repo that holds several (repeatable; `'*'` = all). A lone skill needs none.
        #[arg(long, short = 's', value_name = "NAME")]
        skill: Vec<String>,
        /// The agent (harness) to land a remote import into (a registry slug, e.g. `cursor`; repeatable;
        /// `'*'` = all). Default: the active harness. Ignored for a local path / name adopt.
        #[arg(long, short = 'a', value_name = "SLUG")]
        agent: Vec<String>,
        /// Land a remote import in the harness's global/user skills dir instead of the project (cwd) dir.
        #[arg(long, short = 'g')]
        global: bool,
        /// Apply without the describe step. Parses today; the two-phase describe lands later.
        #[arg(long)]
        yes: bool,
    },
    /// Remove skills from this machine (or from specific agents). A followed skill becomes a per-device
    /// exclusion (your other devices keep receiving it); an untracked local copy is cleaned.
    Remove {
        /// The skill name(s) to remove.
        skill: Vec<String>,
        /// Remove only from these agents (harness slugs; repeatable; `'*'` = all).
        #[arg(long, short = 'a', value_name = "SLUG")]
        agent: Vec<String>,
        /// Apply without the describe step. Parses today; the two-phase describe lands later.
        #[arg(long)]
        yes: bool,
    },
    /// Inventory the skills on this machine. By default also discovers **untracked** skills sitting in
    /// any known harness's skill dir (across the baked registry) that topos could `add`.
    List {
        /// Narrow to one or more skills by name (errors if a name is ambiguous).
        name: Vec<String>,
        /// Also list skills available in the workspace(s) you follow (the remote catalog), annotated
        /// with your follow-state. Requires enrollment; `--workspace` (name or id) narrows.
        #[arg(long)]
        remote: bool,
        /// Show only locally-tracked skills — skip discovery of untracked harness-dir skills.
        #[arg(long)]
        tracked: bool,
        /// Also report the paths topos owns outside skill directories.
        #[arg(long)]
        footprint: bool,
        /// Narrow to one channel's skills (repeatable). Lands with the full resolution grammar.
        #[arg(long, value_name = "NAME")]
        channel: Vec<String>,
        /// Narrow to a specific skill (repeatable). Lands with the full resolution grammar.
        #[arg(long, value_name = "NAME")]
        skill: Vec<String>,
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
    /// Show a skill's local action log + embedded-git history.
    Log {
        /// The skill name.
        skill: String,
    },

    // ---- Team-scoped ----
    /// Ship a draft to the team, ADDING the skill to topos first if it isn't tracked yet. `publish` moves
    /// `current` to your draft (or genesis-creates a never-published skill); `--propose` opens a PR without
    /// moving `current`. Pin the bytes with an optional `@<digest>` suffix. Needs enrollment — un-enrolled,
    /// it refuses with "run `topos follow <workspace-address>` first". Roster-gated.
    Publish {
        /// The skill to publish: a tracked NAME, an untracked `<skill>` / `<skill>@<harness>` to adopt from
        /// discovery, or a `<dir>` to adopt in place — optionally pinned as `<source>@<digest>`.
        target: String,
        /// Place the skill's reference into this channel (created on first use; a curated channel needs
        /// reviewer+). A brand-new skill with no `--to` lands in `everyone`.
        #[arg(long, value_name = "CHANNEL")]
        to: Option<String>,
        /// Open a proposal (a PR) instead of moving `current`.
        #[arg(long)]
        propose: bool,
        /// The commit message for this version (threaded into the candidate commit id).
        #[arg(long, short = 'm', value_name = "MSG")]
        message: Option<String>,
        /// Apply without the describe step. Parses today; the two-phase describe lands later.
        #[arg(long)]
        yes: bool,
    },
    /// Resolve a proposal (the `gh pr review` model). `--approve` moves `current` to the candidate (a
    /// compare-and-set on its base; a stale base re-dos); `--reject` declines a proposal (reviewer,
    /// `-m <reason>` required); `--withdraw` retracts your own open proposal. A bare `review` (no target /
    /// no verdict) is the review inbox/describe (lands later). Roster-gated.
    #[command(group(clap::ArgGroup::new("verdict").args(["approve", "reject", "withdraw"])))]
    Review {
        /// The proposal to resolve, as `<skill>@<hash>`. Omitted = the review inbox (lands later).
        target: Option<String>,
        /// Approve the proposal — move `current` to the candidate.
        #[arg(long)]
        approve: bool,
        /// Reject the proposal (needs `-m <reason>`).
        #[arg(long)]
        reject: bool,
        /// Withdraw your own open proposal.
        #[arg(long)]
        withdraw: bool,
        /// The reject reason / withdrawal note (required with `--reject`).
        #[arg(long, short = 'm', value_name = "MSG")]
        message: Option<String>,
        /// Apply without the describe step. Parses today; the two-phase describe lands later.
        #[arg(long)]
        yes: bool,
    },
    /// Undo a release for the TEAM: move `current` to the older version named by `--to` — a **forward**
    /// pointer-move (nothing deleted; invertible). `--to <hash>` is the sole source of the GOOD version you
    /// go back TO (not the bad one). Team-only — the local go-back is `update <skill>@<hash>`. Roster-gated.
    Revert {
        /// The skill to revert.
        skill: String,
        /// The GOOD version id (64-char hex, or a unique ≥8-char prefix) to restore — the destination, NOT
        /// the bad version.
        #[arg(long = "to")]
        to: String,
        /// Apply the described revert; also acknowledges a no-op (good's bytes already are `current`).
        /// Bare = describe only.
        #[arg(long)]
        yes: bool,
    },
    /// Group skills into channels. `channel add <channel> <skill>...` places a skill's reference into a
    /// channel (created on first placement); `channel remove <channel> <skill>...` removes it. Curated
    /// channels need reviewer+.
    Channel {
        /// The channel subcommand and its args: `add <channel> <skill>...` or `remove <channel> <skill>...`.
        #[arg(value_name = "ARGS")]
        args: Vec<String>,
        /// Apply without the describe step. Parses today; the two-phase describe lands later.
        #[arg(long)]
        yes: bool,
    },
    /// Set a skill's (or channel's) protection level. Bare tightens to `reviewed` (skill) / `curated`
    /// (channel) — reviewer+; `open` loosens it back — owner.
    Protect {
        /// The skill or channel to protect.
        target: String,
        /// The level (`reviewed` / `curated` / `open`); omitted tightens to the reviewed/curated default.
        #[arg(value_name = "LEVEL")]
        level: Option<String>,
        /// Apply without the describe step. Parses today; the two-phase describe lands later.
        #[arg(long)]
        yes: bool,
    },
    /// Seat emails as invited members of the workspace (a roster write). Every CLI invitee starts as a
    /// member; joining is `follow <address>` plus proof of the invited email. Requires prior enrollment.
    /// A bare `invite` (no emails) reads the workspace address + policy (lands later).
    Invite {
        /// The emails to invite (folded to canonical form; seeded onto the roster as `invited`).
        email: Vec<String>,
        /// Pre-place each invitee into this channel (repeatable).
        #[arg(long, value_name = "NAME")]
        channel: Vec<String>,
        /// Apply without the describe step. Parses today; the two-phase describe lands later.
        #[arg(long)]
        yes: bool,
    },

    // ---- Maintenance ----
    /// Update the `topos` binary itself to the latest release, verifying the download's sha256 against the
    /// release SHA256SUMS (never skippable) and replacing the running binary atomically. A MAINTENANCE
    /// command — it touches no skills, no plane, no account. (Skills are updated by `topos update`.)
    SelfUpdate {
        /// Only check whether a newer release exists; report and exit without downloading or replacing.
        #[arg(long)]
        check: bool,
        /// Install a specific release tag (e.g. v0.2.0) instead of the latest — allows a pinned downgrade.
        #[arg(long, value_name = "TAG")]
        version: Option<String>,
    },
    /// Manage this install's sign-in: `auth login [<server>]`, `auth logout`, `auth status`.
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    /// Remove topos from this machine — two-phase (bare describes what goes; `--yes` applies). Scrubs
    /// the session-start currency hook from the harness config and deletes the `~/.topos/` sidecar tree
    /// (the signed-in credential lives there and goes with it). SKILL FILES IN AGENT DIRS ARE LEFT
    /// UNTOUCHED — uninstall never deletes a skill byte. The `topos` binary is NOT self-deleted; remove
    /// it with the installer you used (or `rm` its printed path). Needs no sign-in.
    Uninstall {
        /// Apply the described uninstall (the one-shot consent). Bare = describe only.
        #[arg(long)]
        yes: bool,
    },

    // ---- Hidden aliases ----
    /// Hidden: `topos upgrade` is ambiguous — it maps to a disambiguation refusal (skills → `topos update`,
    /// the CLI → `topos self-update`), so the old spelling never silently does the wrong thing.
    #[command(hide = true)]
    Upgrade,
}

/// The `auth` sign-in subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum AuthCmd {
    /// Re-enroll this machine (the same browser-approval device flow `follow` runs, minus a follow
    /// target): approve in the browser and this device's ONE credential is re-minted — it covers
    /// every workspace your seats reach. On an already-enrolled install the new credential REPLACES
    /// the stored one. An optional `<server>` names the server (default https://topos.sh;
    /// TOPOS_PLANE_URL overrides). A never-enrolled install joins with `topos follow
    /// <workspace-address>` instead.
    Login {
        /// The server URL to sign in to (optional; the enrolled plane, else the hosted default).
        #[arg(value_name = "SERVER_URL")]
        server_url: Option<String>,
        /// Block until the browser approval settles in ONE command. Bare `--wait` waits until the
        /// code expires; `--wait <seconds>` caps the wait.
        #[arg(long, value_name = "SECONDS", num_args = 0..=1)]
        wait: Option<Option<u64>>,
    },
    /// Sign out of this install: revoke this device in each workspace (best-effort), delete the
    /// stored credential — skills, follows, and drafts stay. Two-phase (bare describes; `--yes`
    /// applies).
    Logout {
        /// Apply the described sign-out.
        #[arg(long)]
        yes: bool,
    },
    /// Show who you are, per-workspace access health, hook health, and reporting posture.
    /// Side-effect-free.
    Status,
}

impl Command {
    /// The verb name carried in the `--json` envelope + receipt.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Command::Follow { .. } => "follow",
            Command::Unfollow { .. } => "unfollow",
            // `pull` is a hidden alias of `update` — the envelope always reads "update".
            Command::Update { .. } => "update",
            Command::Add { .. } => "add",
            Command::Remove { .. } => "remove",
            Command::List { .. } => "list",
            Command::Diff { .. } => "diff",
            Command::Log { .. } => "log",
            Command::Publish { .. } => "publish",
            Command::Review { .. } => "review",
            Command::Revert { .. } => "revert",
            Command::Channel { .. } => "channel",
            Command::Protect { .. } => "protect",
            Command::Invite { .. } => "invite",
            Command::SelfUpdate { .. } => "self-update",
            Command::Auth { .. } => "auth",
            Command::Uninstall { .. } => "uninstall",
            Command::Upgrade => "upgrade",
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
    fn pull_is_a_hidden_alias_of_update() {
        // The armed hooks in the field run `topos pull --quiet`; it must parse as Update and read "update".
        let pull = Cli::try_parse_from(["topos", "pull", "--quiet"]).unwrap();
        assert!(matches!(pull.command, Command::Update { quiet: true, .. }));
        assert_eq!(pull.command.name(), "update");
        // A targeted go-back over the alias parses too.
        let go_back = Cli::try_parse_from(["topos", "pull", "docs@abc"]).unwrap();
        assert!(matches!(go_back.command, Command::Update { .. }));
    }

    #[test]
    fn update_onto_current_is_hidden_but_parses() {
        let out = Cli::try_parse_from(["topos", "update", "docs", "--onto-current"]).unwrap();
        assert!(matches!(
            out.command,
            Command::Update {
                onto_current: true,
                ..
            }
        ));
    }

    #[test]
    fn review_verdict_group_is_now_optional() {
        // A bare `review` (no target, no verdict) parses — the inbox/describe is a runtime seam.
        assert!(Cli::try_parse_from(["topos", "review"]).is_ok());
        // A verdict + target parses.
        assert!(Cli::try_parse_from(["topos", "review", "docs@abc", "--approve"]).is_ok());
        assert!(
            Cli::try_parse_from(["topos", "review", "docs@abc", "--reject", "-m", "no"]).is_ok()
        );
        assert!(Cli::try_parse_from(["topos", "review", "docs@abc", "--withdraw"]).is_ok());
    }

    #[test]
    fn publish_takes_message_and_channel_flags() {
        let out = Cli::try_parse_from(["topos", "publish", "docs", "-m", "tidy up", "--to", "eng"])
            .unwrap();
        assert!(matches!(
            out.command,
            Command::Publish {
                message: Some(_),
                to: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn add_uses_agent_not_harness_and_accepts_multiples() {
        // `--harness` is gone; `-a`/`--agent` replace it (repeatable).
        assert!(
            Cli::try_parse_from(["topos", "add", "deploy", "-a", "cursor", "-a", "windsurf"])
                .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "topos",
                "add",
                "vercel-labs/agent-skills",
                "-s",
                "web-design"
            ])
            .is_ok()
        );
        let removed =
            Cli::try_parse_from(["topos", "add", "deploy", "--harness", "cursor"]).unwrap_err();
        assert_eq!(removed.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn revert_replaces_confirm_with_yes() {
        let hash = "ab".repeat(32);
        assert!(Cli::try_parse_from(["topos", "revert", "docs", "--to", &hash, "--yes"]).is_ok());
        // The old `--confirm` flag is gone.
        let removed = Cli::try_parse_from(["topos", "revert", "docs", "--to", &hash, "--confirm"])
            .unwrap_err();
        assert_eq!(removed.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn channel_and_auth_and_protect_parse() {
        assert!(Cli::try_parse_from(["topos", "channel", "add", "eng", "deploy"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "channel"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "protect", "docs", "reviewed"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "auth", "login"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "auth", "status"]).is_ok());
    }

    #[test]
    fn upgrade_is_a_hidden_disambiguation_subcommand() {
        let out = Cli::try_parse_from(["topos", "upgrade"]).unwrap();
        assert!(matches!(out.command, Command::Upgrade));
        assert_eq!(out.command.name(), "upgrade");
    }

    #[test]
    fn wait_flag_is_an_optional_valued_flag_on_follow_and_gone_from_publish() {
        let follow_bare = Cli::try_parse_from(["topos", "follow", "--wait"]).unwrap();
        assert!(matches!(
            follow_bare.command,
            Command::Follow {
                wait: Some(None),
                ..
            }
        ));
        let follow_valued =
            Cli::try_parse_from(["topos", "follow", "acme", "--wait", "300"]).unwrap();
        assert!(matches!(
            follow_valued.command,
            Command::Follow {
                wait: Some(Some(300)),
                ..
            }
        ));
        // `publish` has no pending flow any more (an un-enrolled publish refuses typed), so no --wait.
        let removed = Cli::try_parse_from(["topos", "publish", "docs", "--wait"]).unwrap_err();
        assert_eq!(removed.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn follow_takes_the_two_phase_and_collision_flags() {
        let out = Cli::try_parse_from([
            "topos",
            "follow",
            "acme/channels/eng",
            "--prefix-dirname",
            "--yes",
        ])
        .unwrap();
        assert!(matches!(
            out.command,
            Command::Follow {
                yes: true,
                prefix_dirname: true,
                ..
            }
        ));
    }

    #[test]
    fn auth_login_takes_wait_and_logout_takes_yes() {
        let login = Cli::try_parse_from(["topos", "auth", "login", "--wait", "60"]).unwrap();
        assert!(matches!(
            login.command,
            Command::Auth {
                cmd: super::AuthCmd::Login {
                    wait: Some(Some(60)),
                    ..
                }
            }
        ));
        // The account-switch `--yes` died with the per-account credential set (the identity is
        // whoever approves in the browser; the one credential is replaced wholesale).
        let removed = Cli::try_parse_from(["topos", "auth", "login", "--yes"]).unwrap_err();
        assert_eq!(removed.kind(), ErrorKind::UnknownArgument);
        let logout = Cli::try_parse_from(["topos", "auth", "logout", "--yes"]).unwrap();
        assert!(matches!(
            logout.command,
            Command::Auth {
                cmd: super::AuthCmd::Logout { yes: true }
            }
        ));
    }
}
