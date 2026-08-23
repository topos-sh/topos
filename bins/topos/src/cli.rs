//! The `clap` surface. Thin: it only parses argv; every verb's logic lives in the lib over the seams.
//!
//! The MANIFEST-MODEL verb surface: `login`/`logout` manage this installation's workspace
//! sessions; `init` creates a folder's `topos.toml` and `fmt` tidies one; `add`/`remove` edit the
//! nearest manifest (`-g` = this machine's own `~/.topos/topos.toml`); `update` is the
//! reconcile (targeted forms + the `--quiet` sweep); `status` is the offline health panel and
//! `list` the offline inventory (both scoped: here-scope by default, `-g` machine, `--all` both);
//! `publish`/`review`/`revert`/`protect`/`invite` are the workspace governance verbs; the utility
//! verbs (`diff`, `log`, `self-update`, `uninstall`, `auth status`) persist. Two-phase
//! describe/`--yes` gates the acts with REACH or LOSS (`publish`'s describe, `review`'s verdicts,
//! `revert`, `protect`, `invite`, `update --reset`, `uninstall`; a permanent delete, a
//! set-splitting row rewrite, or a removal whose edit scan cannot be read) — plus a bare `add`
//! of a git source, where the two phases are what let a person read what a repo holds before it
//! lands; every other manifest edit applies immediately with an undo-led receipt (`--yes` an
//! accepted no-op) — a row removal uninstalling its copies in the same invocation, edited copies
//! kept in place.
//!
//! The doc comments on the verbs below are USER-FACING twice over: they are `--help`, and
//! `cli_ref.rs` renders them into `docs/cli.md` + the built-in skill's `reference.md`. Write them
//! for someone who has never seen topos — plain words, no internal vocabulary.

use clap::{Parser, Subcommand};

/// The full `clap` command tree, built from the derived `Cli`. The ONE source of truth for both argv
/// parsing and the generated command reference (`cargo xtask gen-cli-ref` renders `docs/cli.md` from
/// this), so the reference can never drift from what the binary actually accepts.
#[must_use]
pub fn cli_command() -> clap::Command {
    <Cli as clap::CommandFactory>::command()
}

/// `topos` — shared skills for your team's AI agents.
#[derive(Debug, Parser)]
#[command(
    name = "topos",
    version,
    about = "Shared skills for your team's AI agents",
    after_help = "Two documents print on their own, with no command: `topos --skill` (how to drive \
                  this CLI, for an agent's context) and `topos --schema` (the JSON contract behind \
                  --json)."
)]
pub(crate) struct Cli {
    /// Print one JSON object instead of human text — for agents and scripts. Never prompts.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    /// Pick which workspace to act in when this machine is logged into more than one. Takes the
    /// workspace's name or id. With a single login it is inferred.
    #[arg(long, global = true, value_name = "WORKSPACE")]
    pub(crate) workspace: Option<String>,

    /// Print the built-in topos skill — the document that teaches an agent to drive this CLI —
    /// and exit. Works anywhere, including where topos cannot place it for your agents.
    // Both documents are MARKDOWN, which is not the envelope `--json` promises: asked for
    // together there is no answer that keeps both promises, so the run is a usage error rather
    // than raw markdown on the lane a parser is reading.
    #[arg(long, conflicts_with = "json")]
    pub(crate) skill: bool,

    /// Print the JSON Schema for every shape `--json` can answer with, as one document, and exit.
    #[arg(long, conflicts_with = "json")]
    pub(crate) schema: bool,

    /// Optional so a bare `topos` can orient instead of erroring: on a TTY it renders the status
    /// (or the unenrolled welcome) and exits 0; piped/scripted it keeps the classic usage error on
    /// stderr with exit 2, so automation still fails loudly.
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

/// The verb tree. Ordered by scope (self-scoped, then team-scoped, then maintenance).
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    // ---- Self-scoped (affect only you) ----
    /// Check topos's health: your workspace logins and sessions, whether the auto-update
    /// triggers are registered, which `topos.toml` governs where you stand, and what needs
    /// attention — updates pending, deliveries not applied yet, edits of your own — each with
    /// the command that resolves it. `-g` reports your machine-wide set instead; `--all` both.
    /// For the skill inventory use `topos list`, and `topos list <skill>` for one skill in
    /// depth. Works offline and changes nothing. A bare `topos` on a terminal shows the same
    /// thing.
    Status {
        /// Report your machine-wide set, even when run inside a project.
        #[arg(long, short = 'g')]
        global: bool,
        /// Report both this folder's scope and your machine-wide set in full.
        #[arg(long, conflicts_with = "global")]
        all: bool,
    },
    /// Fetch and apply the latest version of what you asked for, where you are standing: this
    /// folder's `topos.toml` when one covers it, and otherwise your machine-wide set (your own
    /// `topos.toml` and the skills your workspaces give you). `-g` updates the machine-wide set
    /// even from inside a project. The background auto-update that runs at the start of each
    /// agent session always covers both, so nothing goes stale while you work in one folder.
    /// Running it by hand checks everything now, including your GitHub lines — the background
    /// sweep checks those a few times a day rather than every session. Safe to run any time.
    /// `topos update <skill>` updates one skill; `topos update <skill>@<version>` puts that
    /// version's bytes back on this machine only.
    #[command(alias = "pull")]
    Update {
        /// The skill(s) to update; `<skill>@<version>` restores that version's bytes locally.
        /// Omitted, everything in this scope is updated.
        targets: Vec<String>,
        /// Update only your machine-wide skills (`~/.topos/topos.toml` and your workspace feeds),
        /// even when run inside a project.
        #[arg(long, short = 'g')]
        global: bool,
        /// Discard your local edits to a skill and take the team version. Shows what would be
        /// lost first; `--yes` applies.
        #[arg(long, conflicts_with = "keep_mine")]
        reset: bool,
        /// With `--reset`: drop only this agent's copy of the edits (a slug like `codex`); every
        /// other copy keeps its own.
        #[arg(long, short = 'a', value_name = "SLUG", conflicts_with = "dest")]
        agent: Option<String>,
        /// With `--reset`: drop only the edits in this exact folder — the folder as `topos list`
        /// prints it, or the one the `topos.toml` line names.
        #[arg(long, value_name = "FOLDER")]
        dest: Option<String>,
        /// Confirm an action that shows a preview first (like `--reset`).
        #[arg(long)]
        yes: bool,
        /// Finish a merge that stopped because you and your team changed the same lines. Topos
        /// puts one copy of the skill in a folder of its own — never a folder your agents read —
        /// and prints the path; in it, files you both changed hold both versions: marked up in
        /// place with `<<<<<<<` markers where the two can be lined up, and side by side
        /// otherwise. Edit that copy and run this to use it — or run it without touching the
        /// folder to keep your wording on those lines and take the team's other changes. Either
        /// way you get an ordinary draft on top of the team's version, which `topos publish`
        /// ships like any other. Only for a merge that has actually stopped — with nothing
        /// waiting, `topos update <skill>` is the command (add `-g` when the merge is in your
        /// machine-wide set). Takes exactly one skill.
        #[arg(long = "keep-mine")]
        keep_mine: bool,
        /// Print nothing on stdout — the mode the session-start hook uses. The hook sweep always
        /// covers both scopes (this folder's and your machine-wide set), so `-g` has no effect
        /// here. Errors still go to stderr with a non-zero exit.
        #[arg(long)]
        quiet: bool,
        /// With `--quiet`: skip the run entirely when one already completed within this many
        /// seconds, so hooks can fire often at no cost. `0` disables the throttle. Default 300;
        /// `TOPOS_UPDATE_TTL` changes the default.
        #[arg(long, value_name = "SECONDS")]
        ttl: Option<u64>,
        /// Which agent's trigger is calling (machinery for registered triggers, not a human
        /// verb). It selects the stdout shape a changed `--quiet` sweep may emit: an agent that
        /// understands the reload extension declares itself here; everyone else — including an
        /// unrecognized name — gets the conservative document every agent's schema accepts.
        #[arg(long, value_name = "HARNESS", hide = true)]
        hook: Option<String>,
        /// Which agent's trigger is calling, for the EVIDENCE alone (machinery): records "this
        /// agent's hook ran" for `status`, changing nothing about the sweep's output.
        #[arg(long, value_name = "HARNESS", hide = true)]
        from: Option<String>,
        /// Re-create managed skill folders that exist but are damaged — topos normally protects a
        /// changed folder as your own edit. Deleted folders come back on an ordinary `topos
        /// update`.
        #[arg(long = "force")]
        force: bool,
        /// Machinery: `topos install` runs through this verb's plumbing with install semantics.
        #[arg(long, hide = true)]
        install: bool,
        /// Machinery: `topos install --frozen` rides here.
        #[arg(long, hide = true)]
        frozen: bool,
    },
    /// Get what's declared, changing no decisions. In a project: place exactly what topos.lock
    /// records (verified against its checksums); a topos.toml line the lock does not know yet is
    /// resolved once and its lock entry written — an existing entry never moves. On your
    /// machine-wide set: the newest of what follows you. `topos update` is the verb that moves
    /// versions.
    Install {
        /// Install only your machine-wide skills, even when run inside a project.
        #[arg(long, short = 'g')]
        global: bool,
        /// Fail instead of writing anything when topos.lock and topos.toml disagree, anything
        /// cannot be fetched, or a checksum does not match — the CI mode (like `npm ci`).
        #[arg(long)]
        frozen: bool,
        /// Print nothing on stdout — the mode the session-start hook uses.
        #[arg(long)]
        quiet: bool,
        /// With `--quiet`: skip the run entirely when one already completed within this many
        /// seconds. `0` disables the throttle. Default 300; `TOPOS_UPDATE_TTL` changes it.
        #[arg(long, value_name = "SECONDS")]
        ttl: Option<u64>,
        /// Which agent's trigger is calling (machinery for registered triggers).
        #[arg(long, value_name = "HARNESS", hide = true)]
        hook: Option<String>,
        /// Which agent's trigger is calling, for the EVIDENCE alone (machinery).
        #[arg(long, value_name = "HARNESS", hide = true)]
        from: Option<String>,
    },
    /// Which workspace commands act on. `list` shows every workspace this machine is signed
    /// into, with a `*` on the default; `use <name>` moves the default. A command's
    /// `--workspace` flag and the `TOPOS_WORKSPACE` environment variable both beat the default
    /// for one invocation; inside a project, the project file's `workspace = ` line does too.
    Workspace {
        #[command(subcommand)]
        cmd: WorkspaceCmd,
    },
    /// Log this machine in to topos. Opens your browser for a one-click approval, where you
    /// choose (or create) the workspace to join. The first login to a workspace records its feed
    /// line (`[workspaces] "<host>/<workspace>" = "latest"`) in `~/.topos/topos.toml` — from then on, whatever that
    /// workspace delivers to you installs here and stays updated by itself; delete the line
    /// (`topos remove -g @<workspace>`) and it stays deleted — login never re-adds it. Bare
    /// `topos login` uses topos.sh; name your own server when self-hosting, a workspace to go
    /// straight to it, or paste an invitation link. To join another workspace, log in again —
    /// already logged in to that server, it takes no browser.
    Login {
        /// The server, workspace, or invitation link. Omitted, uses the default server (or
        /// resumes a login already awaiting approval).
        address: Option<String>,
        /// Wait for the browser approval before returning. Bare `--wait` waits until the code
        /// expires; `--wait <seconds>` caps it. On a terminal, login waits by default; piped,
        /// it prints the approval URL and returns — run `topos login` again to check.
        #[arg(long, value_name = "SECONDS", num_args = 0..=1)]
        wait: Option<Option<u64>>,
    },
    /// Disconnect this machine from a workspace. Installed skills, your edits, and manifests
    /// stay — they just stop updating. `topos login <workspace-address>` reconnects.
    Logout {
        /// The workspace to log out of (name or id). With one login it is inferred; with
        /// several, name one or pass `--all`.
        workspace: Option<String>,
        /// Log out of every workspace on this machine.
        #[arg(long)]
        all: bool,
    },
    /// Create a `topos.toml` in this folder. The file lists the skills everyone working in this
    /// project should have — commit it, and teammates' agents pick up the same set by
    /// themselves. With `-g`, creates your machine's own `~/.topos/topos.toml` instead, header
    /// only — `topos login` writes a workspace's feed line on this machine's first connection
    /// to it, and `topos add -g` records the rest. If the file already exists, nothing changes.
    Init {
        /// Write the machine-wide file (`~/.topos/topos.toml`) instead of this folder's.
        #[arg(long, short = 'g')]
        global: bool,
        /// The workspace this project's bare names use (`<host>/<name>`). Defaults to your
        /// machine default workspace (`topos workspace list`).
        #[arg(long, value_name = "ADDRESS", conflicts_with = "global")]
        workspace: Option<String>,
    },
    /// Tidy a `topos.toml`: group and sort its lines into the standard layout. Comments
    /// survive; meaning never changes. Formats this folder's file, or your machine-wide one
    /// with `-g`.
    Fmt {
        /// Format `~/.topos/topos.toml` instead of this folder's file.
        #[arg(long, short = 'g')]
        global: bool,
    },
    /// Get skills and keep them updated. The source can be a skill or channel from your
    /// workspace (`code-review`, `@acme/code-review`, `@acme/channels/backend`), a whole
    /// workspace's feed (`@acme`, with `-g`), a local folder (`./tools/my-skill`), or a public
    /// GitHub repo (`owner/repo` for every skill in it, `owner/repo/name` for one). Records one
    /// line in the nearest `topos.toml` at or above this folder — or in your machine-wide file
    /// (`~/.topos/topos.toml`) with `-g` — and installs right away. With no `topos.toml` covering
    /// this folder it stops and says so: `topos init` creates one here, or add `-g`. Every answer
    /// names the file it recorded into and, on a second line, the source it recorded: a workspace
    /// or GitHub reference, or the folder on this machine. A folder whose `SKILL.md` is a link
    /// into another folder adds that original — the folder the bytes actually live in. A plain
    /// name is looked for both in the skills already sitting in your agents' folders and in the
    /// catalogs of the workspaces you are connected to — when only a workspace has it, that is
    /// what you get. A GitHub source shows what it found and waits for `--yes`, every time — a
    /// skill is instructions your agent will follow, and that listing is there to be read.
    /// `add topos` restores the built-in topos skill. `--kind mcp` SHARES AN MCP SERVER with your
    /// workspace and gets it here in the same command: the source is its registry name
    /// (`io.github.acme/weather`) or an https link to its `server.json`, and the workspace reads
    /// the document, rules on it, and answers with the name it shares the server as — your agents
    /// then get it as a tool endpoint in their own MCP config rather than as a skill folder. A
    /// server your workspace already shares needs no flag at all. A server only THIS machine runs
    /// is a line you write in your own `topos.toml` (`"./weather" = { kind = "mcp" }`), so a
    /// folder is not something `--kind mcp` takes; a folder that is plainly a server bundle
    /// refuses on the plain `add` rather than landing as a skill, and `--kind skill` on one adopts
    /// it as a skill anyway. By default a skill reaches every agent on the machine;
    /// `-a <agent>` (repeatable) installs it for just those agents, and `--dest <folder>`
    /// (repeatable) installs into an exact folder — together they freeze the row to exactly
    /// those destinations, recorded in the file so updates keep landing there. For an MCP
    /// source `-a` picks whose config file gets the entry. Adding `-a`/`--dest` to something you
    /// already have EXTENDS its destinations: the copies it already has stay, and the new ones
    /// are added. To take a destination away, use `remove -a`/`--dest`.
    Add {
        /// What to add: a workspace skill, channel, or feed; a local folder; or a GitHub repo.
        source: String,
        /// When a GitHub repo holds several skills, pick which one(s) (repeatable; `'*'` = all).
        #[arg(long, short = 's', value_name = "NAME")]
        skill: Vec<String>,
        /// Install for this agent only (a slug like `codex`; repeatable).
        /// Recorded on the row, so updates keep the copy where you asked.
        #[arg(long, short = 'a', value_name = "SLUG")]
        agent: Vec<String>,
        /// Install into this exact folder (repeatable; combined with `-a` the union is the
        /// destination set). An MCP server takes a known config file instead.
        #[arg(long, value_name = "FOLDER")]
        dest: Vec<String>,
        /// What the source IS: `skill` (the default) or `mcp`, a server to share with your
        /// workspace. Only needed for a new server — one your workspace already shares carries
        /// its own kind.
        #[arg(long, value_name = "KIND")]
        kind: Option<crate::bundle_kind::BundleKind>,
        /// Manage this folder as a copy of a skill you already have (its name, or its full
        /// reference). Nothing in the folder changes; updates land here from now on. Folders only.
        #[arg(long = "as", value_name = "BUNDLE")]
        as_bundle: Option<String>,
        /// Add it machine-wide (your `~/.topos/topos.toml`) instead of to this folder's file.
        #[arg(long, short = 'g')]
        global: bool,
        /// Confirm adding from a GitHub source, after reading what it found (everything else
        /// applies immediately, and `--yes` changes nothing there).
        #[arg(long)]
        yes: bool,
    },
    /// Stop getting skills here — the inverse of `add`. Edits the same file `add` would: the
    /// nearest `topos.toml` at or above this folder, or your machine-wide file with `-g`
    /// (dropping a line, or switching one feed-delivered skill "off" on this machine). It never
    /// reaches across that line — a skill your machine-wide file delivers is refused here,
    /// pointing at `-g`. Removing a line also uninstalls the copies it placed, in the same
    /// command — a copy you edited stays in place, disclosed. With `-a <agent>` or
    /// `--dest <folder>` only THAT destination is removed: the row keeps the rest, and removing
    /// the last one removes the row. Prints exactly what changed and how to undo it; asks first
    /// only when removing would lose local work or rewrite a whole channel/repo line.
    Remove {
        /// The skill(s) to remove — or `@<workspace>` with `-g` to stop adopting its feed here.
        skill: Vec<String>,
        /// Remove only this agent's copy (a slug like `codex`; repeatable) — the skill stays for
        /// every other agent.
        #[arg(long, short = 'a', value_name = "SLUG")]
        agent: Vec<String>,
        /// Remove only the copy in this exact folder (repeatable; combined with `-a` the union
        /// is removed). An MCP source takes a config file.
        #[arg(long, value_name = "FOLDER")]
        dest: Vec<String>,
        /// Edit your machine-wide file (`~/.topos/topos.toml`) instead of this folder's.
        #[arg(long, short = 'g')]
        global: bool,
        /// When more than one channel/repo line carries the skill, name WHICH line's rewrite you
        /// mean (its reference, e.g. `@acme/channels/backend`) — the ambiguity refusal lists the
        /// exact `--via` invocations.
        #[arg(long, value_name = "REF")]
        via: Option<String>,
        /// Confirm a removal that loses local work (unshared edits, or a local-only skill whose
        /// delete is permanent).
        #[arg(long)]
        yes: bool,
    },
    /// See what's installed where you stand, per scope: each skill with its version, where it
    /// comes from, and its state. Inside a project the rows are that folder's `topos.toml`'s;
    /// `-g` lists your machine-wide set, `--all` both. `topos list <skill>` answers one skill
    /// in depth — which file and line (or which workspace's feed) delivers it, and where its
    /// files are. `--untracked` lists skills found in your agents' folders that topos does not
    /// manage yet; `-a <agent>` shows one agent's skill folders exactly as that agent reads
    /// them; `--remote` lists what your workspaces offer (needs a login). Works offline except
    /// `--remote`.
    List {
        /// One skill to answer in depth. Omitted, the full inventory.
        #[arg(conflicts_with_all = ["remote", "untracked", "agent"])]
        name: Option<String>,
        /// List your machine-wide set, even when run inside a project.
        #[arg(long, short = 'g')]
        global: bool,
        /// List both this folder's scope and your machine-wide set in full.
        #[arg(long, conflicts_with = "global")]
        all: bool,
        /// List the skills in your agents' folders that topos does not manage yet.
        #[arg(long)]
        untracked: bool,
        /// Show one agent's skill folders as that agent reads them (a slug like `cursor`).
        // `--skill`/`--channel` conflict on purpose: this view is a DIRECTORY LISTING of what sits
        // in the agent's folders, not a filtered inventory — the selectors have nothing to narrow
        // there, and refusing says so instead of silently ignoring them.
        #[arg(
            long,
            short = 'a',
            value_name = "SLUG",
            conflicts_with_all = ["global", "all", "remote", "untracked", "skill", "channel"]
        )]
        agent: Option<String>,
        /// Also list what your workspace(s) offer, with each skill's state on this machine.
        /// Needs a login.
        #[arg(long, conflicts_with = "untracked")]
        remote: bool,
        /// Also list the files topos owns outside skill folders.
        #[arg(long)]
        footprint: bool,
        /// Only skills in this channel (repeatable).
        #[arg(long, value_name = "NAME")]
        channel: Vec<String>,
        /// Only this skill (repeatable).
        #[arg(long, value_name = "NAME")]
        skill: Vec<String>,
        /// Print at most this many rows per section (`0` = all). Default: unlimited on a
        /// terminal, 50 under `--json`.
        #[arg(long, value_name = "N")]
        limit: Option<u64>,
        /// Skip this many rows first — the next page's cursor.
        #[arg(long, value_name = "N")]
        offset: Option<u64>,
    },
    /// Show what changed in a skill. Bare: your local edits against the version you last applied.
    /// With a version id: that version against the team's current. `<a>..<b>` compares two
    /// versions. Reads the copy where you are standing; `-g` reads your machine-wide copy even
    /// from inside a project. When the skill sits in more than one folder, `--dest <folder>` (or
    /// `-a <agent>`) reads the edits in that one — still against the version you last applied, so
    /// two such runs compare like for like.
    Diff {
        /// The skill name.
        skill: String,
        /// Read your machine-wide copy, even when run inside a project.
        #[arg(long, short = 'g')]
        global: bool,
        /// What to compare: a version id, or `<a>..<b>`. Omitted: your edits vs the version you
        /// last applied.
        #[arg(value_name = "REF")]
        r#ref: Option<String>,
        /// Read this agent's copy of the skill (a slug like `codex`).
        #[arg(long, short = 'a', value_name = "SLUG", conflicts_with = "dest")]
        agent: Option<String>,
        /// Read the copy in this exact folder — the folder as `topos list` prints it, or the one
        /// the `topos.toml` line names.
        #[arg(long, value_name = "FOLDER")]
        dest: Option<String>,
        /// Cap the diff at this many bytes, cut at file boundaries (`0` = no cap). Default:
        /// unlimited on a terminal, 64 KiB under `--json`.
        #[arg(long, value_name = "BYTES")]
        max_bytes: Option<u64>,
    },
    /// Check that an MCP server is actually working, right now, from this machine. Topos dials
    /// the address the bundle names (or runs its package) and asks the server for its tools, then
    /// prints what happened. Nothing is stored and nothing is installed — it is a live check, not
    /// a delivery state. A server that asks for a sign-in is healthy: topos holds no credential,
    /// and your agent app signs in on first use — unless the server takes only clients or tokens
    /// registered in advance, which the line says instead, because no agent can complete that
    /// sign-in by itself. Exit codes: `0` responding, `3` sign-in required, `4` not reachable,
    /// `5` reachable but not answering as an MCP server.
    Verify {
        /// The MCP server bundle's name.
        name: String,
        /// Check the server your machine-wide scope holds, even when run inside a project.
        #[arg(long, short = 'g')]
        global: bool,
    },
    /// Show a skill's history — every version with its message and id.
    Log {
        /// The skill name.
        skill: String,
        /// Print at most this many entries (`0` = all). Default: unlimited on a terminal, 20
        /// under `--json`.
        #[arg(long, value_name = "N")]
        limit: Option<u64>,
        /// Skip this many entries first — the next page's cursor.
        #[arg(long, value_name = "N")]
        offset: Option<u64>,
    },

    // ---- Team-scoped ----
    /// Share a skill with your team. A bare run is a preview — it shows where the skill would
    /// land and whether review is required, and changes nothing; add `--yes` to apply. Publishing
    /// again ships a new version; on a skill that requires review, a publish opens a proposal
    /// instead. Needs a login. When you have edited the same skill in more than one folder,
    /// a bare publish stops and asks which one you mean; `--dest <folder>` (or `-a <agent>`)
    /// answers it. The copy you do not pick keeps its edits and becomes an ordinary draft.
    /// A skill you have both in this project and machine-wide publishes from whichever copy
    /// holds the edits, and says which folder that was; `-g` names your machine-wide copy.
    /// A copy that already matches the published version is not an error — it says so and
    /// stops.
    Publish {
        /// The skill to publish: a name, a folder, or `<name>@<version>` to pin the exact bytes.
        target: String,
        /// Publish your machine-wide copy, even when run inside a project.
        #[arg(long, short = 'g')]
        global: bool,
        /// Publish this agent's copy of the skill (a slug like `codex`).
        #[arg(long, short = 'a', value_name = "SLUG", conflicts_with = "dest")]
        agent: Option<String>,
        /// Publish the copy in this exact folder — the folder as `topos list` prints it, or the
        /// one the `topos.toml` line names.
        #[arg(long, value_name = "FOLDER")]
        dest: Option<String>,
        /// Place the skill in this channel. It must already exist — channels are created in the
        /// browser. A brand-new skill with no `--to` lands in `everyone`.
        #[arg(long, value_name = "CHANNEL")]
        to: Option<String>,
        /// Ask for review instead of shipping directly — opens a proposal.
        #[arg(long)]
        propose: bool,
        /// A short message saying what changed and why — it becomes the version's history line.
        #[arg(long, short = 'm', value_name = "MSG")]
        message: Option<String>,
        /// Apply the previewed publish.
        #[arg(long)]
        yes: bool,
    },
    /// See and settle proposals. Bare: your review inbox. With a proposal
    /// (`<skill>@<version>`): its diff. Add a verdict to settle it — `--approve` ships it,
    /// `--reject -m <reason>` declines it, `--withdraw` retracts your own.
    #[command(group(clap::ArgGroup::new("verdict").args(["approve", "reject", "withdraw"])))]
    Review {
        /// The proposal, as `<skill>@<version>`. Omitted: the inbox.
        target: Option<String>,
        /// Ship the proposal — it becomes the version everyone receives.
        #[arg(long)]
        approve: bool,
        /// Decline the proposal. Say why with `-m` — the author sees the reason.
        #[arg(long)]
        reject: bool,
        /// Retract your own open proposal.
        #[arg(long)]
        withdraw: bool,
        /// The reason or note (required with `--reject`).
        #[arg(long, short = 'm', value_name = "MSG")]
        message: Option<String>,
        /// Cap the shown diff at this many bytes, cut at file boundaries (`0` = no cap).
        /// Default: unlimited on a terminal, 64 KiB under `--json`.
        #[arg(long, value_name = "BYTES")]
        max_bytes: Option<u64>,
        /// Not needed here — the verdict flag is the confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Roll the team back to an earlier version of a skill. `--to` names the good version to
    /// return to; everyone picks it up at their next update. Nothing is deleted, so a revert
    /// can itself be reverted. To roll back only this machine, use
    /// `topos update <skill>@<version>` instead.
    Revert {
        /// The skill to roll back.
        skill: String,
        /// The version to return to — the good one, not the bad one. A full id, or a unique
        /// prefix of at least 8 characters.
        #[arg(long = "to")]
        to: String,
        /// Apply the previewed revert (also confirms when that version is already live).
        #[arg(long)]
        yes: bool,
    },
    /// Require review before a skill changes — or curation before a channel's contents change.
    /// `topos protect <skill>` turns it on; `topos protect <skill> open` turns it off (owners
    /// only).
    Protect {
        /// The skill or channel to protect.
        target: String,
        /// The level: `reviewed` (skill), `curated` (channel), or `open` to loosen. Omitted,
        /// protection is turned on.
        #[arg(value_name = "LEVEL")]
        level: Option<String>,
        /// Apply the previewed change.
        #[arg(long)]
        yes: bool,
    },
    /// Invite teammates by email. Each address gets a single-use link that adds them to the
    /// workspace — they can accept in the browser, hand the mail to their agent, or run
    /// `topos login <invite-url>`. Owners only; the server must have mail configured. A bare
    /// `invite` just prints the workspace address.
    Invite {
        /// The addresses to invite. Each link stays valid for 7 days; re-inviting sends a
        /// fresh one.
        email: Vec<String>,
        /// Set the invitee up with this skill from the start (at most one of
        /// `--skill`/`--channel`).
        #[arg(long, value_name = "NAME", conflicts_with = "channel")]
        skill: Option<String>,
        /// Set the invitee up with this channel from the start.
        #[arg(long, value_name = "NAME")]
        channel: Option<String>,
        /// Send the previewed invitations.
        #[arg(long)]
        yes: bool,
    },

    /// Serve one gateway-routed MCP server over stdio. This is the command a delivered MCP
    /// entry runs — an agent starts it, and it forwards the conversation to the workspace's
    /// gateway at the given address, signed in as this machine's session. The credential stays
    /// in topos's own session store; the config entry carries only this command line. Not a
    /// command a person needs to run by hand.
    Relay {
        /// The gateway address the entry names (`https://<gateway>/<session>/<server>`).
        #[arg(value_name = "URL")]
        url: String,
    },

    // ---- Maintenance ----
    /// Update the `topos` binary itself to the latest release. The download's checksum and
    /// signature are always verified, and the swap is atomic. Your skills are untouched — they
    /// update with `topos update`.
    SelfUpdate {
        /// Only report whether a newer release exists — download nothing.
        #[arg(long)]
        check: bool,
        /// Install a specific release (e.g. v0.2.0) instead of the latest — downgrades included.
        #[arg(long, value_name = "TAG")]
        version: Option<String>,
    },
    /// Check your sign-in state: `topos auth status`.
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    /// Remove topos from this machine. Deletes the auto-update hooks and topos's own state
    /// (`~/.topos/`, logins included); installed skill files are never touched. A bare run
    /// shows what would go; `--yes` applies. The binary itself is left in place — its path is
    /// printed so you can delete it with whatever installed it.
    Uninstall {
        /// Apply the previewed uninstall.
        #[arg(long)]
        yes: bool,
    },

    // ---- Hidden aliases ----
    /// Hidden: `topos upgrade` is ambiguous — it maps to a disambiguation refusal (skills → `topos update`,
    /// the CLI → `topos self-update`), so the old spelling never silently does the wrong thing.
    #[command(hide = true)]
    Upgrade,
}

/// The `auth` subcommands — `status` is the one that remains (sessions are managed by the
/// top-level `login`/`logout`).
#[derive(Debug, Subcommand)]
pub(crate) enum WorkspaceCmd {
    /// The signed-in workspaces, `*` on the machine default.
    List,
    /// Set the machine default workspace.
    #[command(name = "use")]
    Use {
        /// The workspace: its name, or `<host>/<name>` when one name is signed in on two servers.
        name: String,
    },
}

/// The `auth` sub-verbs.
#[derive(Debug, clap::Subcommand)]
pub(crate) enum AuthCmd {
    /// Show each workspace login and whether it still works, plus the state of the auto-update
    /// hooks. Changes nothing.
    Status,
}

impl Command {
    /// The verb name carried in the `--json` envelope + receipt.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Command::Status { .. } => "status",
            Command::Login { .. } => "login",
            Command::Logout { .. } => "logout",
            Command::Init { .. } => "init",
            Command::Fmt { .. } => "fmt",
            // `pull` is a hidden alias of `update` — the envelope always reads "update".
            Command::Update { .. } => "update",
            Command::Install { .. } => "install",
            Command::Workspace { .. } => "workspace",
            Command::Add { .. } => "add",
            Command::Remove { .. } => "remove",
            Command::List { .. } => "list",
            Command::Diff { .. } => "diff",
            Command::Verify { .. } => "verify",
            Command::Log { .. } => "log",
            Command::Publish { .. } => "publish",
            Command::Review { .. } => "review",
            Command::Revert { .. } => "revert",
            Command::Protect { .. } => "protect",
            Command::Invite { .. } => "invite",
            Command::Relay { .. } => "relay",
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
    fn status_parses_and_the_subcommand_is_optional() {
        // `topos status` is the explicit health verb.
        let out = Cli::try_parse_from(["topos", "status"]).unwrap();
        assert!(matches!(
            out.command,
            Some(Command::Status {
                global: false,
                all: false
            })
        ));
        assert_eq!(out.command.unwrap().name(), "status");
        // The scope flags parse; `-g` and `--all` refuse each other.
        assert!(matches!(
            Cli::try_parse_from(["topos", "status", "-g"])
                .unwrap()
                .command,
            Some(Command::Status { global: true, .. })
        ));
        assert!(matches!(
            Cli::try_parse_from(["topos", "status", "--all"])
                .unwrap()
                .command,
            Some(Command::Status { all: true, .. })
        ));
        assert!(Cli::try_parse_from(["topos", "status", "-g", "--all"]).is_err());
        // The deep dive moved to `list <skill>`: `status` takes no positional any more.
        let moved = Cli::try_parse_from(["topos", "status", "docs"]).unwrap_err();
        assert_eq!(moved.kind(), ErrorKind::UnknownArgument);
        // A bare `topos` parses (no subcommand) — the composition root decides between the TTY
        // orientation render and the scripted usage error.
        let bare = Cli::try_parse_from(["topos"]).unwrap();
        assert!(bare.command.is_none());
        let bare_json = Cli::try_parse_from(["topos", "--json"]).unwrap();
        assert!(bare_json.command.is_none() && bare_json.json);
    }

    /// The two stand-alone DOCUMENT flags, and the scoping that keeps them out of the way of the
    /// `--skill` SELECTOR the subcommands already own.
    #[test]
    fn the_document_flags_parse_at_the_root_and_never_shadow_a_verbs_own_skill_flag() {
        let skill = Cli::try_parse_from(["topos", "--skill"]).unwrap();
        assert!(skill.skill && !skill.schema && skill.command.is_none());
        let schema = Cli::try_parse_from(["topos", "--schema"]).unwrap();
        assert!(schema.schema && !schema.skill && schema.command.is_none());

        // Inside a verb, `--skill` is that verb's own value-taking selector — clap resolves by
        // scope, so the root flag can never swallow `topos list --skill docs`.
        let listed = Cli::try_parse_from(["topos", "list", "--skill", "docs"]).unwrap();
        assert!(!listed.skill, "the root flag stays unset inside a verb");
        assert!(matches!(
            listed.command,
            Some(Command::List { skill, .. }) if skill == ["docs"]
        ));
        // …and the selector still demands its value rather than parsing as the bare root flag.
        let bare = Cli::try_parse_from(["topos", "list", "--skill"]).unwrap_err();
        assert_eq!(bare.kind(), ErrorKind::InvalidValue);
    }

    #[test]
    fn list_takes_the_new_scope_and_view_flags() {
        // The deep dive is the one positional.
        let out = Cli::try_parse_from(["topos", "list", "docs"]).unwrap();
        assert!(matches!(
            out.command,
            Some(Command::List { name: Some(n), .. }) if n == "docs"
        ));
        // The scope flags and the views parse.
        assert!(Cli::try_parse_from(["topos", "list", "-g"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "list", "--all"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "list", "--untracked"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "list", "-a", "cursor"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "list", "--remote"]).is_ok());
        // `--tracked` is DELETED — the tracked rows are the default body now.
        let removed = Cli::try_parse_from(["topos", "list", "--tracked"]).unwrap_err();
        assert_eq!(removed.kind(), ErrorKind::UnknownArgument);
        // The views refuse to combine where the answer would be ambiguous.
        for bad in [
            &["topos", "list", "docs", "--remote"][..],
            &["topos", "list", "docs", "--untracked"][..],
            &["topos", "list", "docs", "-a", "cursor"][..],
            &["topos", "list", "-a", "cursor", "-g"][..],
            &["topos", "list", "-a", "cursor", "--all"][..],
            &["topos", "list", "-a", "cursor", "--remote"][..],
            &["topos", "list", "--remote", "--untracked"][..],
            &["topos", "list", "-g", "--all"][..],
            // The agent view is a DIRECTORY listing, not a filtered inventory: the row selectors
            // have nothing to narrow there, so they refuse instead of being silently dropped.
            &["topos", "list", "-a", "cursor", "--skill", "docs"][..],
            &["topos", "list", "-a", "cursor", "--channel", "backend"][..],
        ] {
            assert!(Cli::try_parse_from(bad.iter().copied()).is_err(), "{bad:?}");
        }
        // The selectors still combine with every OTHER view.
        assert!(Cli::try_parse_from(["topos", "list", "--skill", "docs"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "list", "--remote", "--channel", "backend"]).is_ok());
    }

    #[test]
    fn init_parses_and_names_itself() {
        let out = Cli::try_parse_from(["topos", "init"]).unwrap();
        assert!(matches!(out.command, Some(Command::Init { .. })));
        assert_eq!(out.command.unwrap().name(), "init");
    }

    #[test]
    fn login_and_logout_parse_as_top_level_session_verbs() {
        let login = Cli::try_parse_from(["topos", "login", "acme", "--wait", "30"]).unwrap();
        assert!(matches!(
            login.command,
            Some(Command::Login {
                wait: Some(Some(30)),
                ..
            })
        ));
        assert_eq!(login.command.unwrap().name(), "login");
        // A bare `login` resumes a pending flow.
        assert!(Cli::try_parse_from(["topos", "login"]).is_ok());
        let logout = Cli::try_parse_from(["topos", "logout", "acme"]).unwrap();
        assert!(matches!(
            logout.command,
            Some(Command::Logout { all: false, .. })
        ));
        assert!(Cli::try_parse_from(["topos", "logout", "--all"]).is_ok());
    }

    #[test]
    fn pull_is_a_hidden_alias_of_update() {
        // The armed hooks in the field run `topos pull --quiet`; it must parse as Update and read "update".
        let pull = Cli::try_parse_from(["topos", "pull", "--quiet"]).unwrap();
        assert!(matches!(
            pull.command,
            Some(Command::Update { quiet: true, .. })
        ));
        assert_eq!(pull.command.unwrap().name(), "update");
        // A targeted go-back over the alias parses too.
        let go_back = Cli::try_parse_from(["topos", "pull", "docs@abc"]).unwrap();
        assert!(matches!(go_back.command, Some(Command::Update { .. })));
    }

    #[test]
    fn update_keep_mine_parses() {
        let out = Cli::try_parse_from(["topos", "update", "docs", "--keep-mine"]).unwrap();
        assert!(matches!(
            out.command,
            Some(Command::Update {
                keep_mine: true,
                ..
            })
        ));
    }

    /// The scope flag PARSES on either side of the bundle name — a person who typed it after the
    /// name is not corrected — while every command topos PRINTS puts it right after the verb, the
    /// one order the suggestions use. This test is what lets the printed order be a display choice
    /// rather than a grammar change.
    #[test]
    fn the_scope_flag_parses_on_either_side_of_the_name() {
        for argv in [
            ["topos", "update", "-g", "notes", "--reset"],
            ["topos", "update", "notes", "-g", "--reset"],
        ] {
            let out = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert!(
                matches!(
                    out.command,
                    Some(Command::Update { global: true, reset: true, ref targets, .. })
                        if targets == &["notes".to_owned()]
                ),
                "{argv:?}"
            );
        }
        for argv in [
            ["topos", "update", "-g", "notes", "--keep-mine"],
            ["topos", "update", "notes", "-g", "--keep-mine"],
        ] {
            let out = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert!(
                matches!(
                    out.command,
                    Some(Command::Update {
                        global: true,
                        keep_mine: true,
                        ..
                    })
                ),
                "{argv:?}"
            );
        }
        // The deep dive a diverged row points at, both ways round.
        for argv in [
            ["topos", "list", "-g", "coolify-deploy"],
            ["topos", "list", "coolify-deploy", "-g"],
        ] {
            let out = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert!(
                matches!(
                    out.command,
                    Some(Command::List { global: true, name: Some(ref n), .. })
                        if n == "coolify-deploy"
                ),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn update_force_parses_and_is_taught() {
        let out = Cli::try_parse_from(["topos", "update", "--force"]).unwrap();
        assert!(matches!(
            out.command,
            Some(Command::Update { force: true, .. })
        ));
        // `--force` is the ONE spelling the help and the generated reference teach.
        let help = super::cli_command()
            .get_subcommands()
            .find(|c| c.get_name() == "update")
            .expect("the update subcommand")
            .clone()
            .render_help()
            .to_string();
        assert!(help.contains("--force"), "{help}");
    }

    /// The one flag help of a verb or an arg, flattened the way the generated reference flattens
    /// it — the paragraph as a reader meets it, with the source's own wrapping gone.
    fn flat(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn subcommand(name: &str) -> clap::Command {
        super::cli_command()
            .get_subcommands()
            .find(|c| c.get_name() == name)
            .unwrap_or_else(|| panic!("the {name} subcommand"))
            .clone()
    }

    fn flag_help(command: &str, flag: &str) -> String {
        flat(
            &subcommand(command)
                .get_arguments()
                .find(|a| a.get_id().as_str() == flag)
                .unwrap_or_else(|| panic!("{command} has no {flag}"))
                .get_help()
                .unwrap_or_else(|| panic!("{command} {flag} has no help"))
                .to_string(),
        )
    }

    /// The teaching surfaces a person meets before they meet the behaviour, asserted whole. Each
    /// one used to describe something the binary does not do: `--keep-mine` named a folder that
    /// does not exist inside a checkout and promised markers that are absent whenever the two
    /// copies share no history; `diff` said it compares against the team's version when it
    /// compares against the version you last applied; `publish`'s preview promised an audience it
    /// deliberately withholds.
    #[test]
    fn the_help_paragraphs_describe_what_the_binary_actually_does() {
        assert_eq!(
            flag_help("update", "keep_mine"),
            "Finish a merge that stopped because you and your team changed the same lines. Topos \
             puts one copy of the skill in a folder of its own — never a folder your agents read \
             — and prints the path; in it, files you both changed hold both versions: marked up \
             in place with `<<<<<<<` markers where the two can be lined up, and side by side \
             otherwise. Edit that copy and run this to use it — or run it without touching the \
             folder to keep your wording on those lines and take the team's other changes. Either \
             way you get an ordinary draft on top of the team's version, which `topos publish` \
             ships like any other. Only for a merge that has actually stopped — with nothing \
             waiting, `topos update <skill>` is the command (add `-g` when the merge is in your \
             machine-wide set). Takes exactly one skill"
        );
        assert_eq!(
            flat(
                &subcommand("diff")
                    .get_about()
                    .expect("diff has an about")
                    .to_string()
            ),
            "Show what changed in a skill. Bare: your local edits against the version you last \
             applied. With a version id: that version against the team's current. `<a>..<b>` \
             compares two versions. Reads the copy where you are standing; `-g` reads your \
             machine-wide copy even from inside a project. When the skill sits in more than one \
             folder, `--dest <folder>` (or `-a <agent>`) reads the edits in that one — still \
             against the version you last applied, so two such runs compare like for like"
        );
        assert_eq!(
            flag_help("diff", "ref"),
            "What to compare: a version id, or `<a>..<b>`. Omitted: your edits vs the version you \
             last applied"
        );
        // The publish preview's own sentence — the rest of the paragraph is untouched.
        assert!(
            flat(
                &subcommand("publish")
                    .get_about()
                    .expect("publish has an about")
                    .to_string()
            )
            .starts_with(
                "Share a skill with your team. A bare run is a preview — it shows where the skill \
                 would land and whether review is required, and changes nothing; add `--yes` to \
                 apply."
            )
        );
    }

    /// `--keep-mine` and `--reset` are opposites — keep your version, or take the team's. Accepting
    /// both let the reset silently win a command that asked for the exact opposite as well, so clap
    /// refuses the pair outright (the same shape `diff`'s `-a`/`--dest` pair already takes).
    #[test]
    fn update_keep_mine_and_reset_are_refused_together() {
        for argv in [
            ["topos", "update", "docs", "--keep-mine", "--reset"],
            ["topos", "update", "docs", "--reset", "--keep-mine"],
        ] {
            let err = Cli::try_parse_from(argv).expect_err("the pair is refused");
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{argv:?}: {err}"
            );
        }
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
            Some(Command::Publish {
                message: Some(_),
                to: Some(_),
                ..
            })
        ));
    }

    #[test]
    fn add_takes_repeated_agents_and_a_forge_skill_selector() {
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
    }

    #[test]
    fn revert_takes_a_version_and_yes() {
        let hash = "ab".repeat(32);
        assert!(Cli::try_parse_from(["topos", "revert", "docs", "--to", &hash, "--yes"]).is_ok());
    }

    #[test]
    fn follow_and_unfollow_are_gone_and_auth_keeps_status_only() {
        // The two verbs a person may still type from muscle memory: `follow`/`unfollow` folded
        // into `add`/`remove -g`, and neither spelling parses any more.
        for retired in [
            &["topos", "follow", "acme"][..],
            &["topos", "unfollow", "docs"][..],
        ] {
            assert!(
                Cli::try_parse_from(retired.iter().copied()).is_err(),
                "{retired:?}"
            );
        }
        assert!(Cli::try_parse_from(["topos", "protect", "docs", "reviewed"]).is_ok());
        assert!(Cli::try_parse_from(["topos", "auth", "status"]).is_ok());
        // `remove -a <agent>` narrows the removal to that agent's destination (repeatable).
        assert!(matches!(
            Cli::try_parse_from(["topos", "remove", "docs", "-a", "cursor"])
                .unwrap()
                .command,
            Some(Command::Remove { agent, .. }) if agent == ["cursor"]
        ));
        // `remove -g` edits this machine's own `~/.topos/topos.toml`.
        assert!(Cli::try_parse_from(["topos", "remove", "-g", "@acme/docs"]).is_ok());
    }

    #[test]
    fn add_and_remove_take_agent_and_dest_selectors() {
        // Both flags are repeatable and combine — the union is the destination set.
        let out = Cli::try_parse_from([
            "topos",
            "add",
            "-g",
            "@acme/deploy",
            "-a",
            "codex",
            "--dest",
            "~/.claude/skills",
        ])
        .unwrap();
        assert!(matches!(
            out.command,
            Some(Command::Add { agent, dest, .. })
                if agent == ["codex"] && dest == ["~/.claude/skills"]
        ));
        let out = Cli::try_parse_from([
            "topos", "remove", "-g", "deploy", "-a", "codex", "-a", "cursor", "--dest", "~/x",
        ])
        .unwrap();
        assert!(matches!(
            out.command,
            Some(Command::Remove { agent, dest, .. })
                if agent == ["codex", "cursor"] && dest == ["~/x"]
        ));
        // `--kind mcp` combines with `-a` (the agent picks whose config file gets the entry).
        assert!(
            Cli::try_parse_from(["topos", "add", "--kind", "mcp", "./weather", "-a", "codex"])
                .is_ok()
        );
    }

    /// `--kind` takes the CLOSED vocabulary and nothing else: the value enum is the same list this
    /// build can deliver, so a word it cannot place is refused at the parse rather than carried to
    /// a door that would have to guess. Omitting the flag is legal — silence means "a skill,
    /// unless the folder is plainly a server bundle", which the adopt door judges.
    #[test]
    fn add_kind_takes_the_closed_vocabulary() {
        use crate::bundle_kind::BundleKind;
        for (word, want) in [("skill", BundleKind::Skill), ("mcp", BundleKind::Mcp)] {
            let out = Cli::try_parse_from(["topos", "add", "--kind", word, "./x"]).unwrap();
            assert!(
                matches!(out.command, Some(Command::Add { kind: Some(k), .. }) if k == want),
                "{word}"
            );
        }
        for word in ["knowledge", "MCP", "skills", ""] {
            assert!(
                Cli::try_parse_from(["topos", "add", "--kind", word, "./x"]).is_err(),
                "`{word}` must not parse as a kind"
            );
        }
        let out = Cli::try_parse_from(["topos", "add", "./x"]).unwrap();
        assert!(matches!(out.command, Some(Command::Add { kind: None, .. })));
    }

    /// The three verbs that act on ONE copy take the selector in the SINGULAR — a repeatable flag
    /// would offer a set none of them can act on — and the two spellings refuse each other, being
    /// two ways of saying the same thing.
    #[test]
    fn diff_publish_and_update_take_one_copy_selector() {
        assert!(matches!(
            Cli::try_parse_from(["topos", "diff", "deploy", "--dest", ".claude/skills"])
                .unwrap()
                .command,
            Some(Command::Diff { dest: Some(d), .. }) if d == ".claude/skills"
        ));
        assert!(matches!(
            Cli::try_parse_from(["topos", "publish", "deploy", "-a", "codex"])
                .unwrap()
                .command,
            Some(Command::Publish { agent: Some(a), .. }) if a == "codex"
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "topos",
                "update",
                "deploy",
                "--reset",
                "--dest",
                "project/.claude/skills/deploy"
            ])
            .unwrap()
            .command,
            Some(Command::Update { reset: true, dest: Some(d), .. })
                if d == "project/.claude/skills/deploy"
        ));
        // `-a` and `--dest` are two spellings of ONE copy; naming both is a usage error, and so is
        // naming either twice.
        for bad in [
            &["topos", "diff", "deploy", "-a", "codex", "--dest", "x"][..],
            &["topos", "publish", "deploy", "-a", "codex", "--dest", "x"][..],
            &[
                "topos", "update", "deploy", "--reset", "-a", "codex", "--dest", "x",
            ][..],
            &["topos", "diff", "deploy", "--dest", "x", "--dest", "y"][..],
        ] {
            assert!(Cli::try_parse_from(bad.iter().copied()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn upgrade_is_a_hidden_disambiguation_subcommand() {
        let out = Cli::try_parse_from(["topos", "upgrade"]).unwrap();
        assert!(matches!(out.command, Some(Command::Upgrade)));
        assert_eq!(out.command.unwrap().name(), "upgrade");
    }
}
