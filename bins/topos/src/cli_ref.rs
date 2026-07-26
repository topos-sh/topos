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
const SELF_SCOPED: [&str; 10] = [
    "status", "login", "logout", "init", "add", "remove", "update", "list", "diff", "log",
];
const TEAM_SCOPED: [&str; 5] = ["publish", "review", "revert", "protect", "invite"];
const MAINTENANCE: [&str; 3] = ["self-update", "auth", "uninstall"];

/// One markdown table cell: collapse internal whitespace to single spaces and escape the `|` that would
/// otherwise split the row.
fn cell(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
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

/// Render one command (recursing into any subcommands) into `out` at the given heading level.
fn render_command(out: &mut String, path: &str, cmd: &clap::Command, level: usize) {
    let hashes = "#".repeat(level);
    out.push_str(&format!("\n{hashes} `{path}`\n\n"));

    // The about text — the long form when present (the full description), collapsed to one paragraph.
    if let Some(about) = cmd.get_long_about().or_else(|| cmd.get_about()) {
        out.push_str(&format!("{}\n\n", cell(&about.to_string())));
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
         per-command `data` payload, `warnings`, `next_actions`, and — on `ok: false` — an `error` \
         (`code`, `outcome`, `retryable`, plus its own `next_actions`). Each `next_actions` entry \
         is a ready-to-run step: `argv` is a complete command; `needs` lists any `<placeholder>` \
         tokens you must substitute first; `mutates`, `needs_network`, and `risk_note` are safety \
         metadata (absent means unknown). Treat `code` as an open vocabulary — run an unfamiliar \
         action by its `argv` rather than rejecting it. The full JSON-Schemas live under \
         `contracts/schemas/` in the repository, with golden examples under \
         `contracts/fixtures/json/`; the agents guide (topos.sh/docs/agents) shows worked \
         examples.\n\n",
    );

    // Global options — rendered from the root command's own args (the `--json` + `--workspace` flags).
    let globals: Vec<&clap::Arg> = root
        .get_arguments()
        .filter(|a| !a.is_hide_set() && !is_auto_help(a))
        .collect();
    if !globals.is_empty() {
        out.push_str("## Global options\n\nThese work before or after any command.\n\n");
        out.push_str("| Flag | What it does |\n|---|---|\n");
        for arg in globals {
            let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
            out.push_str(&format!(
                "| `{}` | {} |\n",
                cell(&arg_label(arg)),
                cell(&help)
            ));
        }
        out.push('\n');
    }

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
