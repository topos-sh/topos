//! The `-a <agent>` / `--dest <folder>` SELECTION — one resolution for every verb that takes it.
//!
//! `-a <slug>` is SUGAR: it resolves through the harness registry to the scope-correct skills
//! folder (machine scope: the `~/`-spelled user dir; project scope: the project-relative dir) —
//! or, for an MCP source, through the descriptor table to the scope-correct config FILE — and is
//! recorded in the row's `dest` exactly as if the folder had been typed. `--dest <folder>` is the
//! literal form, dialect-checked like a hand-written dest entry. The union (agents first, then
//! literals, deduped in spelling order) is the destination set.
//!
//! An unknown slug refuses BEFORE anything is read past the argv (`unknown agent: … — known: …`,
//! the TTY closing with `nothing changed`); the known list is the real registry's slugs,
//! alphabetical, an ellipsis past a handful. The reverse map ([`undo_tail`]) is what receipts use
//! to reconstruct a destination set as `-a` flags where a folder maps back to the registry table,
//! `--dest` otherwise — so an undo is paste-ready in the spelling the person would have typed.

use crate::error::ClientError;
use crate::manifest::document::ManifestScope;

/// The `-a`/`--dest` tokens one invocation carried, verbatim.
#[derive(Debug, Clone, Default)]
pub(crate) struct Selection {
    pub agents: Vec<String>,
    pub dests: Vec<String>,
}

impl Selection {
    pub(crate) fn new(agents: &[String], dests: &[String]) -> Self {
        Selection {
            agents: agents.to_vec(),
            dests: dests.to_vec(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.agents.is_empty() && self.dests.is_empty()
    }

    /// The selection re-spelled as argv flags (`-a <slug>` / `--dest <folder>`), for a rebuilt
    /// command that must carry the whole request.
    pub(crate) fn argv_tail(&self) -> Vec<String> {
        let mut out = Vec::new();
        for a in &self.agents {
            out.push("-a".to_owned());
            out.push(a.clone());
        }
        for d in &self.dests {
            out.push("--dest".to_owned());
            out.push(d.clone());
        }
        out
    }

    /// The destination set for a SKILL source: each `-a` slug's scope-correct skills folder plus
    /// the literal `--dest` folders, deduped in selection order.
    ///
    /// # Errors
    /// [`ClientError::UnknownAgent`] for a slug the registry does not know;
    /// [`ClientError::SelectionRefused`] for a known slug with no folder at this scope, or a
    /// literal folder the scope's dialect refuses.
    pub(crate) fn skill_entries(&self, scope: ManifestScope) -> Result<Vec<String>, ClientError> {
        self.entries(scope, false)
    }

    /// The destination set for an MCP source: each `-a` slug's scope-correct config FILE plus the
    /// literal `--dest` files (each must be a config file the descriptor table knows).
    ///
    /// # Errors
    /// As [`Selection::skill_entries`], over the MCP descriptor table.
    pub(crate) fn mcp_entries(&self, scope: ManifestScope) -> Result<Vec<String>, ClientError> {
        self.entries(scope, true)
    }

    fn entries(&self, scope: ManifestScope, mcp: bool) -> Result<Vec<String>, ClientError> {
        let mut out: Vec<String> = Vec::new();
        for slug in &self.agents {
            let entry = if mcp {
                let known = crate::manifest::dest::mcp_dest_spelling(slug, scope);
                if known.is_none() && topos_harness::mcp::descriptor::mcp_harness(slug).is_none() {
                    return Err(unknown_agent(slug, true));
                }
                known.ok_or_else(|| no_scope_dir(slug, scope, true))?
            } else {
                let entry = crate::manifest::dest::skills_dest_spelling(slug, scope);
                if entry.is_none()
                    && !topos_harness::registry::known_harnesses()
                        .iter()
                        .any(|h| h.slug == slug)
                {
                    return Err(unknown_agent(slug, false));
                }
                entry.ok_or_else(|| no_scope_dir(slug, scope, false))?
            };
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
        for entry in &self.dests {
            crate::manifest::document::check_dest_entry(entry, scope)
                .map_err(|e| ClientError::SelectionRefused(e.message))?;
            if mcp && crate::manifest::dest::mcp_slug_for_dest(entry, scope).is_none() {
                return Err(ClientError::SelectionRefused(
                    crate::manifest::dest::unknown_mcp_file(entry, scope),
                ));
            }
            if !out.contains(entry) {
                out.push(entry.clone());
            }
        }
        Ok(out)
    }
}

/// The `unknown agent: <typed> — known: …` refusal — real registry slugs, alphabetical, ellipsis
/// past a handful.
fn unknown_agent(slug: &str, mcp: bool) -> ClientError {
    let known = if mcp {
        known_mcp_agents()
    } else {
        known_agents()
    };
    ClientError::UnknownAgent {
        agent: slug.to_owned(),
        known,
    }
}

/// A known slug with nothing at this scope — refused, never silently dropped.
fn no_scope_dir(slug: &str, scope: ManifestScope, mcp: bool) -> ClientError {
    let (what, scope_word) = match (mcp, scope) {
        (true, ManifestScope::Global) => ("config file", "machine-wide"),
        (true, ManifestScope::Project) => ("project config file", "project"),
        (false, ManifestScope::Global) => ("machine-wide skills folder", "machine-wide"),
        (false, ManifestScope::Project) => ("project skills folder", "project"),
    };
    ClientError::SelectionRefused(format!(
        "agent `{slug}` has no {what} — `-a {slug}` cannot place at the {scope_word} scope; name \
         a folder with `--dest` instead"
    ))
}

/// The alphabetical registry slugs, ellipsis past a handful (the teaching list every
/// unknown-agent refusal carries).
fn known_agents() -> String {
    let mut slugs: Vec<&str> = topos_harness::registry::known_harnesses()
        .iter()
        .map(|h| h.slug)
        .collect();
    slugs.sort_unstable();
    slugs.dedup();
    truncated_list(&slugs)
}

/// The MCP descriptor table's slugs, same shape.
fn known_mcp_agents() -> String {
    let mut slugs: Vec<&str> = topos_harness::mcp::descriptor::mcp_harnesses()
        .iter()
        .map(|h| h.slug)
        .collect();
    slugs.sort_unstable();
    slugs.dedup();
    truncated_list(&slugs)
}

/// The first handful, comma-joined, `…` when more exist.
fn truncated_list(slugs: &[&str]) -> String {
    const HANDFUL: usize = 4;
    let head: Vec<&str> = slugs.iter().copied().take(HANDFUL).collect();
    if slugs.len() > HANDFUL {
        format!("{}, …", head.join(", "))
    } else {
        head.join(", ")
    }
}

/// The registry slug a recorded skills-folder entry maps BACK to at `scope` — alphabetically
/// first when several slugs share the folder. `None` = no slug spells it (an undo then says
/// `--dest`).
pub(crate) fn slug_for_skill_entry(entry: &str, scope: ManifestScope) -> Option<String> {
    let mut slugs: Vec<&str> = topos_harness::registry::known_harnesses()
        .iter()
        .map(|h| h.slug)
        .collect();
    slugs.sort_unstable();
    slugs
        .into_iter()
        .find(|slug| {
            crate::manifest::dest::skills_dest_spelling(slug, scope).as_deref() == Some(entry)
        })
        .map(str::to_owned)
}

/// The MCP slug a config-file entry maps back to (the descriptor match the engine itself uses).
pub(crate) fn slug_for_mcp_entry(entry: &str, scope: ManifestScope) -> Option<String> {
    crate::manifest::dest::mcp_slug_for_dest(entry, scope).map(str::to_owned)
}

/// A destination set re-spelled as the argv tail that reconstructs it: `-a <slug>` where the
/// entry maps back to the registry table for this scope, `--dest <entry>` otherwise — entry
/// order preserved.
pub(crate) fn undo_tail(entries: &[String], scope: ManifestScope, mcp: bool) -> Vec<String> {
    let mut out = Vec::new();
    for entry in entries {
        let slug = if mcp {
            slug_for_mcp_entry(entry, scope)
        } else {
            slug_for_skill_entry(entry, scope)
        };
        match slug {
            Some(s) => {
                out.push("-a".to_owned());
                out.push(s);
            }
            None => {
                out.push("--dest".to_owned());
                out.push(entry.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(agents: &[&str], dests: &[&str]) -> Selection {
        Selection {
            agents: agents.iter().map(|s| (*s).to_owned()).collect(),
            dests: dests.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn agent_sugar_resolves_to_the_scope_correct_folder() {
        let entries = sel(&["codex"], &[])
            .skill_entries(ManifestScope::Global)
            .unwrap();
        assert_eq!(entries, vec!["~/.codex/skills".to_owned()]);
        let entries = sel(&["codex"], &[])
            .skill_entries(ManifestScope::Project)
            .unwrap();
        assert_eq!(entries, vec![".agents/skills".to_owned()]);
        // The union with a literal, deduped in selection order.
        let entries = sel(&["codex"], &["~/.claude/skills", "~/.codex/skills"])
            .skill_entries(ManifestScope::Global)
            .unwrap();
        assert_eq!(
            entries,
            vec!["~/.codex/skills".to_owned(), "~/.claude/skills".to_owned()]
        );
    }

    #[test]
    fn the_unknown_agent_refusal_lists_real_slugs_alphabetically() {
        let err = sel(&["codx"], &[])
            .skill_entries(ManifestScope::Global)
            .unwrap_err();
        let ClientError::UnknownAgent { agent, known } = &err else {
            panic!("expected UnknownAgent, got {err:?}");
        };
        assert_eq!(agent, "codx");
        // Real registry slugs, alphabetical, ellipsis after the handful.
        let mut slugs: Vec<&str> = topos_harness::registry::known_harnesses()
            .iter()
            .map(|h| h.slug)
            .collect();
        slugs.sort_unstable();
        assert_eq!(*known, format!("{}, …", slugs[..4].join(", ")));
    }

    #[test]
    fn literal_dests_are_dialect_checked() {
        // A relative literal refuses in the machine file; `~/` refuses in a project file.
        assert!(
            sel(&[], &["skills/x"])
                .skill_entries(ManifestScope::Global)
                .is_err()
        );
        assert!(
            sel(&[], &["~/.codex/skills"])
                .skill_entries(ManifestScope::Project)
                .is_err()
        );
        assert!(
            sel(&[], &[".agents/skills"])
                .skill_entries(ManifestScope::Project)
                .is_ok()
        );
    }

    #[test]
    fn mcp_selection_resolves_config_files() {
        let entries = sel(&["codex"], &[])
            .mcp_entries(ManifestScope::Global)
            .unwrap();
        assert_eq!(entries, vec!["~/.codex/config.toml".to_owned()]);
        // A literal must be a KNOWN config file.
        assert!(
            sel(&[], &["~/.codex/notes.toml"])
                .mcp_entries(ManifestScope::Global)
                .is_err()
        );
        assert!(
            sel(&[], &["~/.cursor/mcp.json"])
                .mcp_entries(ManifestScope::Global)
                .is_ok()
        );
    }

    #[test]
    fn undo_tails_reconstruct_agent_sugar_where_it_maps() {
        let tail = undo_tail(
            &["~/.claude/skills".to_owned(), "~/.codex/skills".to_owned()],
            ManifestScope::Global,
            false,
        );
        assert_eq!(tail, vec!["-a", "claude-code", "-a", "codex"]);
        let tail = undo_tail(
            &["~/somewhere/else".to_owned()],
            ManifestScope::Global,
            false,
        );
        assert_eq!(tail, vec!["--dest", "~/somewhere/else"]);
        let tail = undo_tail(
            &["~/.codex/config.toml".to_owned()],
            ManifestScope::Global,
            true,
        );
        assert_eq!(tail, vec!["-a", "codex"]);
    }
}
