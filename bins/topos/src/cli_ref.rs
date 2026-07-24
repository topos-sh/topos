//! The CLI reference renderer — one markdown document from the REAL `clap` tree (`cli::cli_command()`),
//! so the reference can never drift from what the binary parses. TWO consumers, one implementation:
//! `cargo xtask gen-cli-ref` writes/checks the committed `docs/cli.md`, and the built-in `topos` skill
//! places the same bytes as its `reference.md` — which is why this lives in the client lib, not xtask.

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

/// Render one command (recursing into any subcommands) into `out` at the given heading level.
fn render_command(out: &mut String, path: &str, cmd: &clap::Command, level: usize) {
    let hashes = "#".repeat(level);
    out.push_str(&format!("\n{hashes} `{path}`\n\n"));

    // The usage line.
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
    out.push_str(&format!("```\n{}\n```\n\n", usage.join(" ")));

    // The about text — the long form when present (the full description), collapsed to one paragraph.
    if let Some(about) = cmd.get_long_about().or_else(|| cmd.get_about()) {
        out.push_str(&format!("{}\n\n", cell(&about.to_string())));
    }

    // The args/flags table (visible, non-boilerplate args only).
    let rows: Vec<&clap::Arg> = cmd
        .get_arguments()
        .filter(|a| !a.is_hide_set() && !is_boilerplate(a))
        .collect();
    if !rows.is_empty() {
        out.push_str("| Argument / flag | Value | Default | Description |\n");
        out.push_str("|---|---|---|---|\n");
        for arg in rows {
            let name = if arg.is_positional() {
                positional_token(arg)
            } else {
                option_spellings(arg)
            };
            let value = if takes_value(arg) && !arg.is_positional() {
                format!("`<{}>`", value_name(arg))
            } else {
                String::new()
            };
            let default = arg
                .get_default_values()
                .iter()
                .map(|v| v.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(", ");
            let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
            out.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                cell(&name),
                value,
                cell(&default),
                cell(&help),
            ));
        }
        out.push('\n');
    }

    // Recurse into visible subcommands (e.g. `auth login|logout|status`).
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
        "`topos` is the client an agent drives non-interactively. topos asks first when an act \
         REACHES your team, LOSES local work, or TRUSTS something new — those verbs are TWO-PHASE: a \
         bare invocation DESCRIBES what would change (nothing is written), and `--yes` applies it in \
         one shot (`revert` is the exception — `--yes` there also acknowledges a no-op). Everything \
         else — self-scoped acts reversible by their inverse command — applies immediately and prints \
         an undo-led receipt (`--yes` is an accepted no-op there). `--json` works \
         on every verb and prints exactly one envelope on stdout (never a prompt). The exit status is \
         one of three classes: `0` on success, `1` on a domain refusal or a failed operation (the \
         envelope's `ok` + `error.outcome` distinguish a refusal from a transport fault), and `2` on a \
         usage error (an unknown flag or a missing argument). The session-start auto-update hook runs \
         `topos update --quiet`, which stays silent except a freshness one-liner and exits `0` on a \
         network blip so a session never fails to start.\n\n",
    );

    // The JSON contract — the envelope's shape, the next-action grammar (incl. the `needs`
    // placeholder list), and where the full schemas live. Rendered here so `docs/cli.md` and the
    // built-in skill's `reference.md` describe the same contract the binary emits.
    out.push_str(
        "## The `--json` envelope\n\n\
         Every `--json` run prints exactly one envelope object on stdout: `schema_version` (1), \
         `command`, `ok`, the per-verb `data` payload, `warnings` (strings), `next_actions`, and — \
         on `ok: false` — `error` (`code`, `outcome`, `retryable`, and its own `next_actions` \
         mirror). Each entry in `next_actions` is a machine-actionable step: `code` (an open \
         vocabulary — execute an unknown code via its argv, never reject it), `argv` (a complete \
         argv array), optional safety metadata (`mutates`, `needs_network`, `risk_note`; absent = \
         unknown), and `needs` — the placeholder names the argv template still requires before it \
         can execute (e.g. `\"workspace-address\"` for an argv token `<workspace-address>`; \
         substitute your value for each named `<placeholder>`, then run it). An action without \
         `needs` is executable as-is. Errors whose prose names a concrete `topos` command carry \
         the same command structurally in `next_actions`. The full JSON-Schemas live under \
         `contracts/schemas/` with golden examples under `contracts/fixtures/json/`.\n\n",
    );

    // Global options — rendered from the root command's own args (the `--json` + `--workspace` flags).
    let globals: Vec<&clap::Arg> = root
        .get_arguments()
        .filter(|a| !a.is_hide_set() && !is_auto_help(a))
        .collect();
    if !globals.is_empty() {
        out.push_str("## Global options\n\nThese work before or after any verb.\n\n");
        out.push_str("| Flag | Value | Description |\n|---|---|---|\n");
        for arg in globals {
            let value = if takes_value(arg) {
                format!("`<{}>`", value_name(arg))
            } else {
                String::new()
            };
            let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
            out.push_str(&format!(
                "| `{}` | {} | {} |\n",
                cell(&option_spellings(arg)),
                value,
                cell(&help),
            ));
        }
        out.push('\n');
    }

    // The verbs, grouped by scope (the known verb lists, not clap metadata).
    for (title, names) in [
        ("Self-scoped verbs", SELF_SCOPED.as_slice()),
        ("Team-scoped verbs", TEAM_SCOPED.as_slice()),
        ("Maintenance", MAINTENANCE.as_slice()),
    ] {
        out.push_str(&format!("## {title}\n"));
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
        "\n## Renamed verbs\n\n\
         - `topos pull` is a hidden alias of `topos update` (armed session-start hooks in the field \
         still invoke `pull`); the `--json` envelope always reads `update`.\n\
         - `topos upgrade` is intentionally ambiguous and refuses with a disambiguation: `topos \
         update` refreshes followed skills, while `topos self-update` replaces the `topos` binary.\n",
    );

    out
}
