//! The `dest` field's VOCABULARY — the one place destination spellings are derived.
//!
//! A `dest` entry is a literal destination the row freezes placement to: a skills FOLDER for a
//! skill row, a config FILE for an MCP row. Spellings are scope-dialect: the machine file names
//! machine paths (`~/`-prefixed or absolute), a project file names relative paths inside the
//! checkout. This module owns:
//!
//! - the DEFAULT spellings (no env resolution) the retired-`harness` rewrites and the `-a`
//!   selector's recorded rows use — a harness slug's skills root (`~/.codex/skills` /
//!   `.agents/skills`) from the registry table, and an MCP-capable slug's config file
//!   (`~/.codex/config.toml` / `.codex/config.toml`) from the descriptor table;
//! - the KNOWN-MCP-FILE match: which harness a dest entry's config file belongs to, by default
//!   spelling AND by the resolved env-override path (a user whose `$CODEX_HOME` moves the file
//!   still wrote a known file);
//! - the project-relative containment rule (`safe_project_rel`) the grammar enforces at load —
//!   the placement engine keeps its own belt at write time.

use std::path::{Component, Path};

use topos_harness::mcp::descriptor::{self, McpHarness, SurfaceRoot};
use topos_harness::registry;

use crate::manifest::document::ManifestScope;

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

/// The DEFAULT `~/`-spelled base a descriptor surface root resolves to with no env override.
fn mcp_root_default(root: SurfaceRoot) -> &'static str {
    match root {
        SurfaceRoot::Home => "~",
        SurfaceRoot::Config => "~/.config",
        SurfaceRoot::ClaudeHome => "~/.claude",
        SurfaceRoot::CodexHome => "~/.codex",
        SurfaceRoot::HermesHome => "~/.hermes",
    }
}

/// An MCP-capable slug's config-file spelling at `scope`, DEFAULT spellings only (machine scope:
/// the user surface under its `~/`-spelled default root; project scope: the project surface
/// string verbatim). `None` when the slug is not MCP-capable, or has no surface at this scope.
pub(crate) fn mcp_dest_spelling(slug: &str, scope: ManifestScope) -> Option<String> {
    let h = descriptor::mcp_harness(slug)?;
    match scope {
        ManifestScope::Global => {
            let s = h.user_surface.as_ref()?;
            Some(format!("{}/{}", mcp_root_default(s.root), s.suffix))
        }
        ManifestScope::Project => h.project_surface.map(|(rel, _)| rel.to_owned()),
    }
}

/// ONE registry user-dir spec string (the `<root-tag>/<suffix>` encoding
/// [`registry::KnownHarness::user_dir_specs`] renders) as its DEFAULT `~/`-spelled machine
/// folder — no env resolution. `None` for a root with no machine-file spelling (a cwd- or
/// env-only root).
pub(crate) fn spec_default_spelling(spec: &str) -> Option<String> {
    let (tag, suffix) = match spec.split_once('/') {
        Some((t, s)) => (t, s),
        None => (spec, ""),
    };
    // An absolute spec renders `/`-rooted (empty tag) and carries over verbatim.
    if tag.is_empty() {
        return Some(spec.to_owned());
    }
    let base = match tag {
        "home" => "~",
        "configHome" => "~/.config",
        "codexHome" => "~/.codex",
        "claudeHome" => "~/.claude",
        "vibeHome" => "~/.vibe",
        "hermesHome" => "~/.hermes",
        "autohandHome" => "~/.autohand",
        "grokHome" => "~/.grok",
        // cwd-only / env-only roots have no machine-file default spelling.
        _ => return None,
    };
    Some(if suffix.is_empty() {
        base.to_owned()
    } else {
        format!("{base}/{suffix}")
    })
}

/// A harness slug's SKILLS-ROOT spelling at `scope`, DEFAULT spellings only: the registry's
/// first user dir under its `~/`-spelled default root for the machine file, the project dir
/// string verbatim for a project file. `None` for an unknown slug or a scope the harness has no
/// dir at.
pub(crate) fn skills_dest_spelling(slug: &str, scope: ManifestScope) -> Option<String> {
    let h = registry::known_harnesses()
        .iter()
        .find(|h| h.slug == slug)?;
    match scope {
        ManifestScope::Global => spec_default_spelling(&h.user_dir_specs().into_iter().next()?),
        ManifestScope::Project => {
            let dir = h.project_dir();
            (!dir.is_empty()).then(|| dir.to_owned())
        }
    }
}

/// Map a list of harness slugs through a spelling fn: the quoted, comma-joined rewrite list of
/// what maps (or `None` when nothing does), plus the slugs that map nowhere.
pub(crate) fn rewrite_slugs(
    slugs: &[String],
    map: impl Fn(&str) -> Option<String>,
) -> (Option<String>, Vec<String>) {
    let mut mapped = Vec::new();
    let mut unmapped = Vec::new();
    for slug in slugs {
        match map(slug) {
            Some(d) if !mapped.contains(&d) => mapped.push(d),
            Some(_) => {}
            None => unmapped.push(slug.clone()),
        }
    }
    let list = (!mapped.is_empty()).then(|| quoted_list(&mapped));
    (list, unmapped)
}

/// Quote + comma-join a rewrite list: `"a", "b"`.
pub(crate) fn quoted_list(items: &[String]) -> String {
    items
        .iter()
        .map(|d| format!("\"{d}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `"x" maps` / `"x" and "y" map` clause for slugs the rewrite could not place.
pub(crate) fn maps_clause(unmapped: &[String]) -> String {
    let names = unmapped
        .iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(" and ");
    if unmapped.len() == 1 {
        format!("{names} maps")
    } else {
        format!("{names} map")
    }
}

/// Every known MCP config-file spelling at `scope`, table order — the list an unknown-file
/// refusal teaches.
pub(crate) fn known_mcp_files(scope: ManifestScope) -> Vec<String> {
    descriptor::mcp_harnesses()
        .iter()
        .filter_map(|h| mcp_dest_spelling(h.slug, scope))
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
fn dest_names_mcp_surface(entry: &str, scope: ManifestScope, h: &McpHarness) -> bool {
    match scope {
        ManifestScope::Project => h
            .project_surface
            .is_some_and(|(rel, _)| entry.trim_start_matches("./") == rel),
        ManifestScope::Global => {
            if mcp_dest_spelling(h.slug, ManifestScope::Global).as_deref() == Some(entry) {
                return true;
            }
            // The resolved comparison needs a real home; without one only the default spelling
            // matches (tests that never set $HOME stay deterministic).
            let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
                return false;
            };
            let Some(resolved) = descriptor::user_surface_path(h, &home) else {
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
        assert_eq!(
            mcp_dest_spelling("claude-code", ManifestScope::Global).as_deref(),
            Some("~/.claude/skills/topos-mcp")
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
            let resolved = descriptor::user_surface_path(h, &home).unwrap();
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
