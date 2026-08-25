//! The `dest` field's VOCABULARY — the one place destination spellings are derived.
//!
//! A `dest` entry is a literal destination the row places at: a skills FOLDER for a skill row, a
//! config FILE for an MCP row. Spellings are scope-dialect: the machine file names machine paths
//! (`~/`-prefixed or absolute), a project file names relative paths inside the checkout. One
//! entry is not a path at all — [`DEFAULT_REACH`], the `"*"` token, stands for everything the row
//! would resolve to with no `dest` at all, so a row can name a destination without giving up the
//! reach it already had. This module owns:
//!
//! - the DEFAULT spellings (no env resolution) the `-a` selector's recorded rows use — a harness
//!   slug's skills root (`~/.codex/skills` /
//!   `.agents/skills`) from the registry table, and an MCP-capable slug's config file
//!   (`~/.codex/config.toml` / `.codex/config.toml`) from the descriptor table;
//! - the KNOWN-MCP-FILE match: which harness a dest entry's config file belongs to, by default
//!   spelling AND by the resolved env-override path (a user whose `$CODEX_HOME` moves the file
//!   still wrote a known file);
//! - the project-relative containment rule (`safe_project_rel`) the grammar enforces at load —
//!   the placement engine keeps its own belt at write time.

use std::path::{Component, Path};

use topos_harness::mcp::{McpDialect, descriptor, plugin_dir};
use topos_harness::registry::{self, KnownHarness};

use crate::manifest::document::ManifestScope;

/// The one `dest` entry that is not a path: the row's DEFAULT REACH — every destination the row
/// would resolve to carrying no `dest` at all (a skill row: each PICKED agent's own folder at
/// that scope, decided at plan time on every run; an MCP row: every picked agent with a config
/// surface at that scope). Named entries beside it are ADDITIONS: a skill folder is placed as
/// typed whether or not its agent is picked; an MCP file of an unpicked agent gets no entry and
/// the receipt says so.
pub(crate) const DEFAULT_REACH: &str = "*";

/// Whether a `dest` array stands for the row's default reach as well as what it names.
pub(crate) fn carries_default_reach(dest: &[String]) -> bool {
    dest.iter().any(|e| e == DEFAULT_REACH)
}

/// A `dest` array's NAMED entries — everything but the [`DEFAULT_REACH`] token, in array order.
pub(crate) fn named_entries(dest: &[String]) -> Vec<String> {
    dest.iter()
        .filter(|e| *e != DEFAULT_REACH)
        .cloned()
        .collect()
}

/// A `dest` array in NORMAL form: the [`DEFAULT_REACH`] token first when the array carries it,
/// then the named entries in their own order, duplicates collapsed.
pub(crate) fn normal_dest(dest: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(dest.len());
    if carries_default_reach(dest) {
        out.push(DEFAULT_REACH.to_owned());
    }
    for entry in named_entries(dest) {
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}

/// Whether a string is a SAFE project-relative path: non-empty, relative, every component
/// ordinary (no `..`, no root) — so `project_dir.join(value)` provably stays inside the
/// checkout. The grammar-layer port of the placement engine's rule (which stays as the belt).
pub(crate) fn safe_project_rel(raw: &str) -> bool {
    let p = Path::new(raw);
    !raw.trim().is_empty()
        && p.is_relative()
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// An MCP-capable slug's config-FILE spelling at `scope`, DEFAULT spellings only (machine scope:
/// the user surface under its `~/`-spelled default root; project scope: the project surface
/// string verbatim). `None` when the slug is not MCP-capable, or has no surface at this scope.
/// The `~/` spelling of every root is the registry's ([`DirSpec::default_spelling`]), so a new root
/// is spelled in exactly one place.
///
/// Always the FILE, never the directory holding it: Claude Code's surface is a topos-owned plugin
/// DIRECTORY, and every receipt names the `.mcp.json` inside it. Teaching one spelling while
/// printing another meant pasting a receipt's own path into `dest` was refused.
///
/// **Table-only, and only the suite reads it.** Three rows have no platform-neutral spelling and
/// answer `None` here; every runtime caller wants [`mcp_dest_spelling_here`], which resolves those
/// against this machine, so the two cannot be picked by accident.
#[cfg(test)]
fn mcp_dest_spelling(slug: &str, scope: ManifestScope) -> Option<String> {
    mcp_dest_spelling_of(descriptor::mcp_harness(slug)?, scope)
}

/// [`mcp_dest_spelling`] over a ROW the caller already holds — the form the reference renderer
/// uses, so it can spell a stated table (the bundled one, for the committed copy) without a second
/// lookup rule.
pub(crate) fn mcp_dest_spelling_of(harness: &KnownHarness, scope: ManifestScope) -> Option<String> {
    let mcp = harness.mcp()?;
    match scope {
        ManifestScope::Global => {
            let user = mcp.user?;
            let base = user.dir.default_spelling()?;
            Some(match user.dialect {
                McpDialect::ClaudePluginDir => format!(
                    "{}/{}",
                    base.trim_end_matches('/'),
                    plugin_dir::PLUGIN_MCP_PATH
                ),
                _ => base,
            })
        }
        ManifestScope::Project => mcp.project.map(|(rel, _)| rel.to_owned()),
    }
}

/// [`mcp_dest_spelling`] **as this machine spells it** — the one a verb writes into a row and the
/// one a refusal teaches.
///
/// Three agents keep their machine config under the platform's application-support directory
/// (`~/Library/Application Support` on macOS, `%APPDATA%` on Windows, the XDG config home
/// elsewhere), which the table deliberately has no single spelling for. That left them with no
/// machine `dest` token at all: `-a vscode` refused with "agent `vscode` has no config file" — a
/// sentence that was false twice over (it has one, and `--dest` takes a FILE) — and the
/// known-file list a `--dest` refusal teaches named 13 of the 16 files.
///
/// The machine-wide manifest's own dest dialect is MACHINE PATHS (`~/`-prefixed or absolute — the
/// grammar's rule, not a workaround), so the spelling for those three is the surface's resolved
/// path written back as `~/…`. It round-trips: [`mcp_slug_for_dest`] already resolves a `~/` entry
/// against this machine and matches it to the surface, which is why `--dest` accepted the very
/// path `-a` could not produce. A root that resolves OUTSIDE home has no `~/` form and still
/// answers `None` — an absolute machine path is not something a verb should write for someone.
pub(crate) fn mcp_dest_spelling_here(slug: &str, scope: ManifestScope) -> Option<String> {
    let harness = descriptor::mcp_harness(slug)?;
    if let Some(spelling) = mcp_dest_spelling_of(harness, scope) {
        return Some(spelling);
    }
    if scope != ManifestScope::Global {
        return None;
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let resolved = harness.mcp_user_path(&home)?;
    let rest = resolved.strip_prefix(&home).ok()?.to_str()?;
    let base = format!("~/{rest}");
    Some(match harness.mcp()?.user?.dialect {
        McpDialect::ClaudePluginDir => format!(
            "{}/{}",
            base.trim_end_matches('/'),
            plugin_dir::PLUGIN_MCP_PATH
        ),
        _ => base,
    })
}

/// One `dest` entry as the SURFACE is keyed — the directory, for the one dialect whose surface is
/// a directory. Both spellings of that surface (the dir, and the `.mcp.json` inside it) name the
/// same place, so both are accepted and both normalize here; everything downstream sees one form.
fn surface_key(entry: &str, dialect: McpDialect) -> &str {
    match dialect {
        McpDialect::ClaudePluginDir => entry
            .strip_suffix(plugin_dir::PLUGIN_MCP_PATH)
            .map_or(entry, |head| head.trim_end_matches('/')),
        _ => entry,
    }
}

/// A harness slug's SKILLS-ROOT spelling at `scope`, DEFAULT spellings only: the registry's
/// first user dir under its `~/`-spelled default root for the machine file, the project dir
/// string verbatim for a project file. `None` for an unknown slug, a scope the harness has no
/// dir at, or a machine root with no `~/` spelling (a cwd- or env-only root).
pub(crate) fn skills_dest_spelling(slug: &str, scope: ManifestScope) -> Option<String> {
    skills_dest_spelling_of(registry::known_harness(slug)?, scope)
}

/// [`skills_dest_spelling`] over a ROW the caller already holds (see [`mcp_dest_spelling_of`]).
pub(crate) fn skills_dest_spelling_of(h: &KnownHarness, scope: ManifestScope) -> Option<String> {
    match scope {
        ManifestScope::Global => h.user_dirs().first()?.default_spelling(),
        ManifestScope::Project => {
            let dir = h.project_dir();
            (!dir.is_empty()).then(|| dir.to_owned())
        }
    }
}

/// Quote + comma-join a destination list: `"a", "b"`.
pub(crate) fn quoted_list(items: &[String]) -> String {
    items
        .iter()
        .map(|d| format!("\"{d}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every known MCP config-file spelling at `scope`, table order — the list an unknown-file
/// refusal teaches. Spelled for THIS MACHINE ([`mcp_dest_spelling_here`]), so the list a person is
/// told to choose from names every file topos would write and not merely the ones with a
/// platform-neutral token: a refusal that teaches an incomplete list teaches the wrong thing.
pub(crate) fn known_mcp_files(scope: ManifestScope) -> Vec<String> {
    descriptor::mcp_harnesses()
        .iter()
        .filter_map(|h| mcp_dest_spelling_here(h.slug, scope))
        .collect()
}

/// The one unknown-MCP-file message shape — shared by the load refusal (local rows, where the
/// kind is knowable) and the converge-time warning (workspace rows, whose kind arrives with the
/// delivery).
pub(crate) fn unknown_mcp_file(entry: &str, scope: ManifestScope) -> String {
    format!(
        "dest entry `{entry}` is not a known MCP config file — the known files here are {}",
        known_mcp_files(scope).join(", ")
    )
}

/// The clause a `dest` that names ONLY unknown files earns — the row is frozen to what it names,
/// so the bundle reaches NO agent at all. Teaches the same known-file set [`unknown_mcp_file`]
/// does, from the same source, so the two lines can never name different files.
pub(crate) fn dest_names_no_mcp_file(unknown: &[String], scope: ManifestScope) -> String {
    let (noun, verb) = if unknown.len() == 1 {
        ("dest entry", "is")
    } else {
        ("dest entries", "are")
    };
    format!(
        "{noun} {} {verb} not a known MCP config file; the known files here are {}",
        quoted_list(unknown),
        known_mcp_files(scope).join(", ")
    )
}

/// The same fact as [`dest_names_no_mcp_file`], said to the person whose file carries the array —
/// so it names the file the entries live in and both ways out. The sweep's warning uses this one;
/// the receipt-row clause above stays the bare statement of what the row is frozen to.
pub(crate) fn dest_names_no_mcp_file_remedy(unknown: &[String], scope: ManifestScope) -> String {
    let (noun, verb) = if unknown.len() == 1 {
        ("dest entry", "is")
    } else {
        ("dest entries", "are")
    };
    format!(
        "the {noun} {} in your topos.toml {verb} not a known MCP config file. The known files \
         here are {} — use one of those, or drop dest so the bundle reaches every MCP-capable \
         agent.",
        quoted_list(unknown),
        known_mcp_files(scope).join(", ")
    )
}

/// Which MCP-capable harness a dest entry's config file belongs to at `scope` — matched against
/// the descriptor's DEFAULT spelling, and against the resolved env-override path (a moved
/// `$CODEX_HOME` still names a known file). `None` when no harness claims the file.
pub(crate) fn mcp_slug_for_dest(entry: &str, scope: ManifestScope) -> Option<&'static str> {
    for h in descriptor::mcp_harnesses() {
        if dest_names_mcp_surface(entry, scope, h) {
            return Some(h.slug);
        }
    }
    None
}

/// Whether `entry` names harness `h`'s config surface at `scope`. Project entries compare
/// against the project surface string (`./`-prefix tolerated); machine entries against the
/// default `~/` spelling, and — resolved against the real home — against the env-honoring
/// surface path the engine would edit.
fn dest_names_mcp_surface(entry: &str, scope: ManifestScope, h: &KnownHarness) -> bool {
    match scope {
        ManifestScope::Project => h
            .mcp()
            .and_then(|m| m.project)
            .is_some_and(|(rel, _)| entry.trim_start_matches("./") == rel),
        ManifestScope::Global => {
            let Some(user) = h.mcp().and_then(|m| m.user) else {
                return false;
            };
            // Normalized to the surface's own key, so the file spelling a receipt prints and the
            // directory spelling the table holds both answer to the one surface.
            let entry = surface_key(entry, user.dialect);
            if user.dir.default_spelling().as_deref() == Some(entry) {
                return true;
            }
            // The resolved comparison needs a real home; without one only the default spelling
            // matches (tests that never set $HOME stay deterministic).
            let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
                return false;
            };
            let Some(resolved) = h.mcp_user_path(&home) else {
                return false;
            };
            let expanded = match entry.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => std::path::PathBuf::from(entry),
            };
            expanded == resolved
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_spellings_are_scope_correct() {
        // The MCP descriptor table, machine scope: the user surface under its default root.
        assert_eq!(
            mcp_dest_spelling("codex", ManifestScope::Global).as_deref(),
            Some("~/.codex/config.toml")
        );
        // The FILE, not the directory holding it — the spelling every receipt prints.
        assert_eq!(
            mcp_dest_spelling("claude-code", ManifestScope::Global).as_deref(),
            Some("~/.claude/skills/topos-mcp/.mcp.json")
        );
        assert_eq!(
            mcp_dest_spelling("opencode", ManifestScope::Global).as_deref(),
            Some("~/.config/opencode/opencode.json")
        );
        // Project scope: the project surface string verbatim; no surface = no spelling.
        assert_eq!(
            mcp_dest_spelling("codex", ManifestScope::Project).as_deref(),
            Some(".codex/config.toml")
        );
        assert_eq!(
            mcp_dest_spelling("claude-code", ManifestScope::Project).as_deref(),
            Some(".mcp.json")
        );
        assert_eq!(mcp_dest_spelling("openclaw", ManifestScope::Project), None);
        // The registry skills roots.
        assert_eq!(
            skills_dest_spelling("codex", ManifestScope::Global).as_deref(),
            Some("~/.codex/skills")
        );
        assert_eq!(
            skills_dest_spelling("claude-code", ManifestScope::Global).as_deref(),
            Some("~/.claude/skills")
        );
        assert_eq!(
            skills_dest_spelling("codex", ManifestScope::Project).as_deref(),
            Some(".agents/skills")
        );
        assert_eq!(
            skills_dest_spelling("claude-code", ManifestScope::Project).as_deref(),
            Some(".claude/skills")
        );
        // Unknown slugs map nowhere.
        assert_eq!(mcp_dest_spelling("notepad", ManifestScope::Global), None);
        assert_eq!(skills_dest_spelling("notepad", ManifestScope::Global), None);
    }

    /// **Every MCP-capable agent has a machine spelling, and the list a refusal teaches names
    /// them all.** Three of them keep their config under the platform's application-support
    /// directory, which the table has no single token for — so they had no machine `dest` at all:
    /// `-a vscode` refused with "agent `vscode` has no config file" (false: it has one, and
    /// `--dest` takes a FILE not a folder), and the known-file list a `--dest` refusal prints
    /// named thirteen of sixteen. The machine-wide file's own dest dialect IS machine paths, so
    /// the spelling for those three is the resolved path written back as `~/…` — which
    /// [`mcp_slug_for_dest`] already accepted, which is exactly why `--dest` worked where `-a`
    /// did not.
    #[test]
    fn every_mcp_agent_has_a_machine_spelling_that_resolves_back_to_it() {
        let table = descriptor::mcp_harnesses();
        let spelled: Vec<&str> = table
            .iter()
            .filter(|h| mcp_dest_spelling_here(h.slug, ManifestScope::Global).is_some())
            .map(|h| h.slug)
            .collect();
        assert_eq!(
            spelled.len(),
            table.len(),
            "every MCP-capable agent spells its machine config file: {spelled:?}"
        );
        assert_eq!(known_mcp_files(ManifestScope::Global).len(), table.len());

        // The round trip that makes it a vocabulary and not a string: what `-a <slug>` writes is
        // what `--dest` accepts, and it resolves back to the same agent.
        for h in table {
            let entry = mcp_dest_spelling_here(h.slug, ManifestScope::Global).expect("a spelling");
            assert!(
                entry.starts_with("~/"),
                "the machine file names machine paths: {entry}"
            );
            crate::manifest::document::check_dest_entry(&entry, ManifestScope::Global)
                .unwrap_or_else(|e| panic!("{entry} is not writable into a machine row: {e:?}"));
            assert_eq!(
                mcp_slug_for_dest(&entry, ManifestScope::Global),
                Some(h.slug),
                "{entry} must resolve back to {}",
                h.slug
            );
        }
    }

    #[test]
    fn dest_entries_match_known_mcp_files() {
        assert_eq!(
            mcp_slug_for_dest("~/.codex/config.toml", ManifestScope::Global),
            Some("codex")
        );
        assert_eq!(
            mcp_slug_for_dest("~/.cursor/mcp.json", ManifestScope::Global),
            Some("cursor")
        );
        assert_eq!(
            mcp_slug_for_dest(".codex/config.toml", ManifestScope::Project),
            Some("codex")
        );
        assert_eq!(
            mcp_slug_for_dest("./.mcp.json", ManifestScope::Project),
            Some("claude-code")
        );
        // Claude Code's plugin-dir surface answers to BOTH its spellings — the file a receipt
        // prints, and the directory the table holds.
        assert_eq!(
            mcp_slug_for_dest(
                "~/.claude/skills/topos-mcp/.mcp.json",
                ManifestScope::Global
            ),
            Some("claude-code")
        );
        assert_eq!(
            mcp_slug_for_dest("~/.claude/skills/topos-mcp", ManifestScope::Global),
            Some("claude-code")
        );
        assert_eq!(
            mcp_slug_for_dest("~/.codex/skills", ManifestScope::Global),
            None
        );
        assert_eq!(
            mcp_slug_for_dest("weird.json", ManifestScope::Project),
            None
        );
    }

    #[test]
    fn env_resolved_mcp_files_still_match() {
        // `$HOME`-expanded absolute spellings of the default surfaces match too (the resolved
        // comparison; env overrides resolve through the same seam).
        if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
            let h = descriptor::mcp_harness("cursor").unwrap();
            let resolved = h.mcp_user_path(&home).unwrap();
            assert_eq!(
                mcp_slug_for_dest(&resolved.display().to_string(), ManifestScope::Global),
                Some("cursor")
            );
        }
    }

    #[test]
    fn the_project_containment_rule_is_ported() {
        assert!(safe_project_rel(".claude/skills"));
        assert!(safe_project_rel("./tools/x"));
        assert!(!safe_project_rel("../out"));
        assert!(!safe_project_rel("/abs"));
        assert!(!safe_project_rel(""));
        assert!(!safe_project_rel("a/../b"));
    }
}
