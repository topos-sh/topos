//! The CLI reference renderer — one markdown document from the REAL `clap` tree (`cli::cli_command()`),
//! so the reference can never drift from what the binary parses. TWO consumers, one implementation:
//! `cargo xtask gen-cli-ref` writes/checks the committed `docs/cli.md`, and the built-in `topos` skill
//! places the same bytes as its `reference.md` — which is why this lives in the client lib, not xtask.
//!
//! The rendering rules serve a first-time reader: each command appears ONCE as its heading (the
//! usage line is added only when it says more than the heading — arguments or options exist), the
//! per-command table is two columns (the flag with its value placeholder, then the plain-language
//! help), and the preamble states the one behavior rule in ordinary words.

/// The behavior verbs grouped by SCOPE — the KNOWN verb lists drive the grouping (not clap metadata),
/// so the reference reads the way the tool is taught: self-scoped, then team-scoped, then maintenance.
const SELF_SCOPED: [&str; 11] = [
    "status", "login", "logout", "init", "fmt", "add", "remove", "update", "list", "diff", "log",
];
const TEAM_SCOPED: [&str; 5] = ["publish", "review", "revert", "protect", "invite"];
const MAINTENANCE: [&str; 3] = ["self-update", "auth", "uninstall"];

/// The cross-agent MACHINE folders — several agents read the same one, so the agent section names
/// each ONCE with its readers instead of repeating it down a column.
const SHARED_MACHINE_FOLDERS: [&str; 2] = ["~/.agents/skills", "~/.config/agents/skills"];
/// The one cross-agent PROJECT folder — a `shared` cell in the agent table means this.
const SHARED_PROJECT_FOLDER: &str = ".agents/skills";

/// One markdown table cell: collapse internal whitespace to single spaces and escape the `|` that would
/// otherwise split the row.
fn cell(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// A prose paragraph, terminated. Restores the sentence-final `.` `clap` removes from a derived
/// `about` — and only that: text already ending in terminal punctuation (or in nothing at all) is
/// returned untouched, so this can never double a full stop or punctuate an empty string.
fn sentence(s: &str) -> String {
    match s.chars().last() {
        Some('.' | '!' | '?' | ':') | None => s.to_owned(),
        Some(_) => format!("{s}."),
    }
}

/// Does the arg carry a value (an option or a positional), vs a bare boolean flag?
fn takes_value(arg: &clap::Arg) -> bool {
    matches!(
        arg.get_action(),
        clap::ArgAction::Set | clap::ArgAction::Append
    )
}

/// Is the arg repeatable / multi-valued (a `Vec` field)?
fn is_multiple(arg: &clap::Arg) -> bool {
    matches!(arg.get_action(), clap::ArgAction::Append)
}

/// The first declared value name for an arg (its `<NAME>` placeholder), falling back to the id in caps.
fn value_name(arg: &clap::Arg) -> String {
    arg.get_value_names()
        .and_then(|names| names.first().map(|n| n.as_str().to_owned()))
        .unwrap_or_else(|| arg.get_id().as_str().to_uppercase())
}

/// The usage token for a positional arg: `<NAME>` (required) / `[NAME]` (optional), plus `...` when
/// repeatable.
fn positional_token(arg: &clap::Arg) -> String {
    let name = value_name(arg);
    let inner = if arg.is_required_set() {
        format!("<{name}>")
    } else {
        format!("[{name}]")
    };
    if is_multiple(arg) {
        format!("{inner}...")
    } else {
        inner
    }
}

/// The auto-generated `--help` / `--version` args clap injects — identified by their ACTION, so a real
/// user field literally named `version` (e.g. `self-update --version <TAG>`) is NEVER mistaken for one.
fn is_auto_help(arg: &clap::Arg) -> bool {
    matches!(
        arg.get_action(),
        clap::ArgAction::Help
            | clap::ArgAction::HelpShort
            | clap::ArgAction::HelpLong
            | clap::ArgAction::Version
    )
}

/// True for the auto-generated help/version pair + the two global flags surfaced once under "Global
/// options" — the args each per-verb table omits.
fn is_boilerplate(arg: &clap::Arg) -> bool {
    is_auto_help(arg) || matches!(arg.get_id().as_str(), "json" | "workspace")
}

/// The comma-joined spellings of an option (`-m, --message`); empty for a bare positional.
fn option_spellings(arg: &clap::Arg) -> String {
    let mut spellings = Vec::new();
    if let Some(short) = arg.get_short() {
        spellings.push(format!("-{short}"));
    }
    if let Some(long) = arg.get_long() {
        spellings.push(format!("--{long}"));
    }
    spellings.join(", ")
}

/// The table's first cell: an option with its value placeholder (`-m, --message <MSG>`), or the
/// positional's usage token (`<SOURCE>`).
fn arg_label(arg: &clap::Arg) -> String {
    if arg.is_positional() {
        return positional_token(arg);
    }
    let spellings = option_spellings(arg);
    if takes_value(arg) {
        format!("{spellings} <{}>", value_name(arg))
    } else {
        spellings
    }
}

/// One flag section: the heading + blurb, then the two-column table. Nothing is emitted when the
/// section has no flags, so an empty heading can never stand over an empty table.
fn render_flag_table(out: &mut String, heading: &str, args: &[&clap::Arg]) {
    if args.is_empty() {
        return;
    }
    out.push_str(heading);
    out.push_str("| Flag | What it does |\n|---|---|\n");
    for arg in args {
        let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
        out.push_str(&format!(
            "| `{}` | {} |\n",
            cell(&arg_label(arg)),
            cell(&help)
        ));
    }
    out.push('\n');
}

/// Render one command (recursing into any subcommands) into `out` at the given heading level.
fn render_command(out: &mut String, path: &str, cmd: &clap::Command, level: usize) {
    let hashes = "#".repeat(level);
    out.push_str(&format!("\n{hashes} `{path}`\n\n"));

    // The about text — the long form when present (the full description), collapsed to one
    // paragraph and RE-TERMINATED. `clap` strips one trailing `.` off a derived `about` (the help
    // screen's own convention), which is right there and wrong in prose: every paragraph in this
    // reference is several full sentences, and the last one was arriving without its full stop.
    // The source doc comments already end in one — this puts back exactly what was taken.
    if let Some(about) = cmd.get_long_about().or_else(|| cmd.get_about()) {
        out.push_str(&format!("{}\n\n", sentence(&cell(&about.to_string()))));
    }

    // The usage line — rendered only when it says more than the heading already does (the command
    // takes arguments, options, or a subcommand). A bare `topos status` block under a `topos
    // status` heading would state the command twice.
    let mut usage = vec![path.to_owned()];
    let has_flags = cmd
        .get_arguments()
        .any(|a| !a.is_positional() && !a.is_hide_set() && !is_boilerplate(a));
    if has_flags {
        usage.push("[OPTIONS]".to_owned());
    }
    if cmd.has_subcommands() {
        usage.push("<COMMAND>".to_owned());
    }
    for arg in cmd.get_arguments() {
        if arg.is_positional() && !arg.is_hide_set() && !is_boilerplate(arg) {
            usage.push(positional_token(arg));
        }
    }
    if usage.len() > 1 {
        out.push_str(&format!("```\n{}\n```\n\n", usage.join(" ")));
    }

    // The args/flags table (visible, non-boilerplate args only), two columns.
    let rows: Vec<&clap::Arg> = cmd
        .get_arguments()
        .filter(|a| !a.is_hide_set() && !is_boilerplate(a))
        .collect();
    if !rows.is_empty() {
        out.push_str("| Argument / flag | What it does |\n");
        out.push_str("|---|---|\n");
        for arg in rows {
            let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
            out.push_str(&format!(
                "| `{}` | {} |\n",
                cell(&arg_label(arg)),
                cell(&help)
            ));
        }
        out.push('\n');
    }

    // Recurse into visible subcommands (e.g. `auth status`).
    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        render_command(out, &format!("{path} {}", sub.get_name()), sub, level + 1);
    }
}

/// The agent → folder tables, BUILT FROM the baked registry (skills folders) and the MCP
/// descriptor table (config files) at render time — so what the reference teaches about `-a` can
/// never drift from what `-a` actually writes. The `--check` gate turns any registry change into a
/// failing build until the committed copies are regenerated.
fn render_agents(out: &mut String) {
    use crate::manifest::dest::{mcp_dest_spelling, skills_dest_spelling};
    use crate::manifest::document::ManifestScope;

    // Per agent: its DEFAULT machine folders (the first is the one `-a` writes) + its project folder.
    // Sorted by slug — the reference is a lookup table, and row order carries no meaning here.
    let mut rows: Vec<(&str, Vec<String>, Option<String>)> =
        topos_harness::registry::known_harnesses()
            .iter()
            .map(|h| {
                let machine: Vec<String> = h
                    .user_dirs()
                    .iter()
                    .filter_map(|spec| spec.default_spelling())
                    .collect();
                (
                    h.slug,
                    machine,
                    skills_dest_spelling(h.slug, ManifestScope::Project),
                )
            })
            .collect();
    rows.sort_unstable_by_key(|(slug, _, _)| *slug);

    out.push_str("## Agents (`-a`) and where files land\n\n");
    out.push_str(
        "`-a <slug>` places a bundle in that agent's folder from the table below — the machine \
         folder when you work machine-wide, the project folder when you work in a project. \
         `--dest <folder>` names a folder literally instead. Where a row lists more than one \
         machine folder, `-a` writes the first.\n\n\
         Some folders are cross-agent conventions that several agents read, so each is named here \
         once rather than repeated on every row:\n\n",
    );
    for folder in SHARED_MACHINE_FOLDERS {
        let readers: Vec<&str> = rows
            .iter()
            .filter(|(_, machine, _)| machine.iter().any(|m| m == folder))
            .map(|(slug, _, _)| *slug)
            .collect();
        if !readers.is_empty() {
            out.push_str(&format!(
                "- `{folder}` — the machine folder read by {}.\n",
                readers.join(", ")
            ));
        }
    }
    let project_readers: Vec<&str> = rows
        .iter()
        .filter(|(_, _, project)| project.as_deref() == Some(SHARED_PROJECT_FOLDER))
        .map(|(slug, _, _)| *slug)
        .collect();
    if !project_readers.is_empty() {
        out.push_str(&format!(
            "- `{SHARED_PROJECT_FOLDER}` — the project folder read by {}. A `shared` cell below \
             means this folder.\n",
            project_readers.join(", ")
        ));
    }
    out.push('\n');

    // One row per agent that has a folder of its OWN somewhere; an agent whose folders are all
    // shared is already fully described by the list above.
    out.push_str("| Agent | Machine folder | Project folder |\n|---|---|---|\n");
    for (slug, machine, project) in &rows {
        let own_machine = machine
            .iter()
            .any(|m| !SHARED_MACHINE_FOLDERS.contains(&m.as_str()));
        let own_project = project
            .as_deref()
            .is_some_and(|p| p != SHARED_PROJECT_FOLDER);
        if !own_machine && !own_project {
            continue;
        }
        let machine_cell = if machine.is_empty() {
            "—".to_owned()
        } else {
            machine
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let project_cell = match project.as_deref() {
            None => "—".to_owned(),
            Some(p) if p == SHARED_PROJECT_FOLDER => "shared".to_owned(),
            Some(p) => format!("`{p}`"),
        };
        out.push_str(&format!("| `{slug}` | {machine_cell} | {project_cell} |\n"));
    }
    out.push_str(
        "\nA `—` means the agent has no folder at that scope. An agent absent from the table reads \
         only the shared folders named above.\n\n",
    );

    // MCP servers land in the agent's own config file, never in a skills folder.
    out.push_str("### MCP server config files\n\n");
    out.push_str(
        "An MCP-server bundle arrives as an entry in the agent's own MCP config rather than a \
         skills folder. `-a <slug>` picks the file below; `--dest <file>` names one literally. \
         Claude Code's machine entry is a topos-owned plugin folder, not a single file.\n\n",
    );
    out.push_str("| Agent | Machine config file | Project config file |\n|---|---|---|\n");
    for h in topos_harness::mcp::descriptor::mcp_harnesses() {
        let cell_or_dash = |spelling: Option<String>| {
            spelling.map_or_else(|| "—".to_owned(), |s| format!("`{s}`"))
        };
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            h.slug,
            cell_or_dash(mcp_dest_spelling(h.slug, ManifestScope::Global)),
            cell_or_dash(mcp_dest_spelling(h.slug, ManifestScope::Project)),
        ));
    }
    out.push('\n');
}

/// Render the full CLI reference markdown from the real clap command tree.
#[must_use]
pub fn cli_ref_md() -> String {
    let root = crate::cli::cli_command();
    let mut out = String::new();
    out.push_str("# `topos` command reference\n\n");
    out.push_str(
        "> GENERATED from the `clap` command tree by `cargo xtask gen-cli-ref` — do not hand-edit. \
         Change the CLI, re-run the command, and commit the result; the `--check` variant is the drift \
         gate.\n\n",
    );
    out.push_str(
        "Every command prints human-readable text on a terminal, or exactly one JSON object with \
         `--json` (for agents and scripts — it never prompts).\n\n\
         One rule covers all of them: **a command that reaches other people, discards local work, \
         or trusts a new source shows a preview first.** Run it bare to see exactly what would \
         happen, then add `--yes` to apply. Everything else applies immediately and prints what \
         changed, along with the command that undoes it.\n\n\
         Exit codes: `0` — success. `1` — the operation was refused or failed (with `--json`, the \
         `error` object says which and how to fix it). `2` — the command line itself was invalid.\n\n",
    );

    // The JSON contract — the envelope's shape and where the full schemas live. Rendered here so
    // `docs/cli.md` and the built-in skill's `reference.md` describe the same contract the binary
    // emits; the worked examples live in the agents guide (topos.sh/docs/agents).
    out.push_str(
        "## The `--json` envelope\n\n\
         Every `--json` run prints one object on stdout: `schema_version` (1), `command`, `ok`, a \
         per-command `data` payload, `warnings`, `messages`, `next_actions`, and — on `ok: false` \
         — an `error` (`code`, `outcome`, `retryable`, plus its own `next_actions`). Each \
         `next_actions` entry is a ready-to-run step: `argv` is a complete command; `needs` lists \
         any `<placeholder>` tokens you must substitute first; `mutates`, `needs_network`, and \
         `risk_note` are safety metadata (absent means unknown). Treat `code` as an open \
         vocabulary — run an unfamiliar action by its `argv` rather than rejecting it.\n\n\
         `messages` is the typed line channel: one `{code, kind, text}` entry per line, where \
         `kind` is `failure` · `decision` · `advisory` · `disclosure`, `code` is the open \
         SCREAMING_SNAKE vocabulary (absent when the producer has none), and `text` is the \
         sentence a person reads. Machine-readable message codes now ride `messages[].code` — \
         each entry is `{code, kind, text}`. The `warnings` array is unchanged in shape for now \
         and will be retired in a future contract version; match on `messages[].code` rather than \
         `warnings[]` string prefixes.\n\n\
         The full JSON-Schemas live under `contracts/schemas/` in the repository, with golden \
         examples under `contracts/fixtures/json/`; the agents guide (topos.sh/docs/agents) shows \
         worked examples.\n\n",
    );

    // The agent → folder tables, generated from the baked registry + the MCP descriptor table.
    render_agents(&mut out);

    // The root command's own args, split by REACH so neither table claims the other's behavior:
    // the clap-global ones (`--json` + `--workspace`) attach to any verb, the rest stand alone.
    let root_args: Vec<&clap::Arg> = root
        .get_arguments()
        .filter(|a| !a.is_hide_set() && !is_auto_help(a))
        .collect();
    let (globals, standalone): (Vec<&clap::Arg>, Vec<&clap::Arg>) =
        root_args.into_iter().partition(|a| a.is_global_set());
    render_flag_table(
        &mut out,
        "## Global options\n\nThese work before or after any command.\n\n",
        &globals,
    );
    render_flag_table(
        &mut out,
        "## Documents topos prints\n\nEach of these is the whole command: it prints one document \
         and exits, reading nothing and dialing nothing.\n\n",
        &standalone,
    );

    // The verbs, grouped by scope (the known verb lists, not clap metadata).
    for (title, blurb, names) in [
        (
            "Everyday commands",
            "These act on this machine only.",
            SELF_SCOPED.as_slice(),
        ),
        (
            "Team commands",
            "These reach your workspace, so each one previews first or confirms via its flag.",
            TEAM_SCOPED.as_slice(),
        ),
        (
            "Maintenance",
            "The binary and the installation itself.",
            MAINTENANCE.as_slice(),
        ),
    ] {
        out.push_str(&format!("## {title}\n\n{blurb}\n"));
        for name in names {
            let cmd = root
                .get_subcommands()
                .find(|c| c.get_name() == *name)
                .unwrap_or_else(|| panic!("the cli tree is missing the `{name}` verb"));
            render_command(&mut out, &format!("topos {name}"), cmd, 3);
        }
    }

    // The hidden / renamed verbs note (the reference omits hidden subcommands themselves).
    out.push_str(
        "\n## Aliases\n\n\
         - `topos pull` — an alias of `topos update`, kept for hooks installed by older versions.\n\
         - `topos upgrade` — deliberately refused with a pointer to both meanings: `topos update` \
         refreshes your skills, `topos self-update` replaces the `topos` binary itself.\n",
    );

    out
}
