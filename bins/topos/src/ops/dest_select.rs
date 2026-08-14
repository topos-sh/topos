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
//! to reconstruct a destination set as `-a` flags where a folder maps back to exactly ONE slug at
//! that scope, `--dest` otherwise — so an undo is paste-ready in the spelling the person would
//! have typed. The map is many-to-one (nineteen-odd agents default to `.agents/skills` in a
//! project), and a slug picked out of a shared set names something the person never asked for, so
//! the shared folder prints as itself.

use std::path::{Path, PathBuf};

use topos_types::persisted::PlacementMap;

use crate::bundle_kind::BundleKind;
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::manifest::document::ManifestScope;
use crate::placement::ScanStatus;

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

    /// The SINGULAR spelling — the one `diff` / `publish` / `update --reset` take. Those verbs act
    /// on exactly ONE copy, so a repeatable flag would offer a set the act has no meaning for; the
    /// two spellings refuse each other at the parser for the same reason.
    pub(crate) fn one(agent: Option<&str>, dest: Option<&str>) -> Self {
        Selection {
            agents: agent.map(str::to_owned).into_iter().collect(),
            dests: dest.map(str::to_owned).into_iter().collect(),
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
        self.entries(scope, BundleKind::Skill)
    }

    /// The destination set for an MCP source: each `-a` slug's scope-correct config FILE plus the
    /// literal `--dest` files (each must be a config file the descriptor table knows).
    ///
    /// # Errors
    /// As [`Selection::skill_entries`], over the MCP descriptor table.
    pub(crate) fn mcp_entries(&self, scope: ManifestScope) -> Result<Vec<String>, ClientError> {
        self.entries(scope, BundleKind::Mcp)
    }

    /// The tokens this selection names an EXISTING copy by: each `-a` slug's scope-correct skills
    /// folder (the same registry resolution a recorded row gets) plus the literal `--dest` tokens
    /// VERBATIM.
    ///
    /// The literals skip the manifest dialect check [`Selection::skill_entries`] runs, and
    /// deliberately: that check asks "may a file at this scope RECORD this line", which is a
    /// different question from "does this folder name a copy that is already there". A person
    /// reading a project copy's absolute path off a receipt must be able to paste it back, and the
    /// project dialect refuses absolute paths in a file. Nothing is recorded here, so nothing needs
    /// the file's grammar — the token is matched against the spellings the copy already answers to
    /// ([`copy_spellings`]), and a token that names none of them refuses.
    ///
    /// # Errors
    /// [`ClientError::UnknownAgent`] / [`ClientError::SelectionRefused`], exactly as
    /// [`Selection::skill_entries`] raises them for the `-a` half.
    pub(crate) fn copy_tokens(&self, scope: ManifestScope) -> Result<Vec<String>, ClientError> {
        let mut out: Vec<String> = Vec::new();
        for slug in &self.agents {
            let entry = crate::manifest::dest::skills_dest_spelling(slug, scope);
            if entry.is_none()
                && !topos_harness::registry::known_harnesses()
                    .iter()
                    .any(|h| h.slug == slug)
            {
                return Err(unknown_agent(slug, false));
            }
            let entry = entry.ok_or_else(|| no_scope_dir(slug, scope, false))?;
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
        for entry in &self.dests {
            if !out.contains(entry) {
                out.push(entry.clone());
            }
        }
        Ok(out)
    }

    /// The destination set a bundle of `kind` resolves to at `scope` — the ONE resolution both
    /// public spellings ride, dispatching on the kind rather than on a caller's boolean.
    fn entries(&self, scope: ManifestScope, kind: BundleKind) -> Result<Vec<String>, ClientError> {
        let mut out: Vec<String> = Vec::new();
        // MATCHED, not tested: a kind added later has no destination vocabulary until someone
        // writes one here, and the compiler is what says so.
        let mcp = match kind {
            BundleKind::Mcp => true,
            BundleKind::Skill => false,
        };
        for slug in &self.agents {
            let entry = if mcp {
                // Spelled for THIS MACHINE: three agents keep their machine config under the
                // platform's application-support directory, which the table has no single token
                // for, and `-a` refused them outright while `--dest` accepted the very path it
                // could not produce.
                let known = crate::manifest::dest::mcp_dest_spelling_here(slug, scope);
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
            // THE DEFAULT-REACH TOKEN IS ROW GRAMMAR, not a destination anyone can ask for: it is
            // what a row spells to keep the reach it already had, and there is nothing for a verb
            // to place at it. The manifest's own dialect check lets it through (a row may carry
            // it), so the argv refuses it here, where the ask is.
            if entry == crate::manifest::dest::DEFAULT_REACH {
                return Err(ClientError::SelectionRefused(format!(
                    "`{}` is not a destination — it is the topos.toml token for a row's default \
                     reach; a bare `topos add <bundle>` already reaches every agent",
                    crate::manifest::dest::DEFAULT_REACH
                )));
            }
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

// ---------------------------------------------------------------------------------------------
// Naming ONE existing copy — the `-a`/`--dest` selector on `diff` / `publish` / `update --reset`.
// ---------------------------------------------------------------------------------------------

/// The two spellings ONE recorded placement answers to on a receipt and on a command line: the
/// folder as a person reads it, and the `--dest` value that names it back.
///
/// The display path is written against the thing the reader is standing in — `project/<rest>` under
/// the checkout a project store belongs to, `~/`-abbreviated on the machine — the same spelling the
/// `update` receipt and a draft row already print. The `--dest` value is the folder HOLDING the
/// copy (the skills root, not the skill dir): the shortest form, and the one a manifest row records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CopySpelling {
    /// `project/.agents/skills/coolify-deploy` · `~/.claude/skills/coolify-deploy`.
    pub display: String,
    /// `.agents/skills` · `~/.claude/skills` — the `--dest` value.
    pub dest: String,
}

/// How ONE placement dir is spelled at `ctx`'s scope (see [`CopySpelling`]). A dir outside the
/// scope's own root (nothing plans one there; the read is best-effort) falls back to the plain
/// `~`-abbreviated path rather than a `project/` prefix that would not resolve.
pub(crate) fn copy_spellings(ctx: &Ctx<'_>, dir: &Path) -> CopySpelling {
    let parent = dir.parent().unwrap_or(dir);
    if let Some(root) = ctx.layout.project_root()
        && let Ok(rest) = dir.strip_prefix(root)
    {
        return CopySpelling {
            display: format!("project/{}", rest.display()),
            dest: parent.strip_prefix(root).map_or_else(
                |_| super::inventory::pretty(ctx, parent),
                |p| p.display().to_string(),
            ),
        };
    }
    CopySpelling {
        display: super::inventory::pretty(ctx, dir),
        dest: super::inventory::pretty(ctx, parent),
    }
}

/// The ONE copy a `-a`/`--dest` selection named, with the bytes it holds.
pub(crate) struct SelectedCopy {
    /// The placement dir itself — always an EDITED copy (a clean one refuses).
    pub dir: PathBuf,
    /// How a receipt spells it.
    pub spelling: CopySpelling,
    /// The OTHER copies holding edits, as a receipt spells them — what this act leaves alone.
    pub others_edited: Vec<String>,
}

/// Resolve a `-a`/`--dest` selection to ONE of `map`'s recorded copies, scanned.
///
/// **The placement freeze is deliberately NOT consulted.** Naming the copy IS the choice the freeze
/// asks for, so a bundle whose copies disagree can still be read, published, or dropped one copy at
/// a time — where the aggregate classification ([`crate::placement::work_tree_dir`]) refuses before
/// anything can be picked, which left a frozen bundle impossible to even inspect.
///
/// Three spellings resolve to the same copy: the folder the row records (`.claude/skills`), the
/// display path a receipt prints (`project/.claude/skills/coolify-deploy`), and either of those
/// absolute — with a `./` prefix and a trailing slash tolerated on all of them. `-a <slug>` is the
/// registry's sugar for the first.
///
/// # Errors
/// [`ClientError::UnknownAgent`] for a slug the registry does not know;
/// [`ClientError::SelectionRefused`] when the folder names no copy of this bundle (the refusal
/// names every copy it DOES have), when it names one holding no edits (refused plainly rather than
/// doing a no-op), or when the copy cannot be read as this bundle's right now.
pub(crate) fn select_copy(
    ctx: &Ctx<'_>,
    sel: &Selection,
    skill: &str,
    map: &PlacementMap,
) -> Result<SelectedCopy, ClientError> {
    let scope = if ctx.layout.is_project_scope() {
        ManifestScope::Project
    } else {
        ManifestScope::Global
    };
    let tokens = sel.copy_tokens(scope)?;
    let dirs: Vec<PathBuf> = map.placements.iter().map(PathBuf::from).collect();
    let spellings: Vec<CopySpelling> = dirs.iter().map(|d| copy_spellings(ctx, d)).collect();

    let mut picked: Vec<usize> = Vec::new();
    for token in &tokens {
        for (i, dir) in dirs.iter().enumerate() {
            if names_copy(token, dir, &spellings[i]) && !picked.contains(&i) {
                picked.push(i);
            }
        }
    }
    let [idx] = picked[..] else {
        return Err(ClientError::SelectionRefused(if picked.is_empty() {
            format!(
                "no copy of '{skill}' is in {} — its copies are: {}",
                tokens.join(", "),
                spellings
                    .iter()
                    .map(|s| s.display.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            format!(
                "that names {} copies of '{skill}' — these verbs act on ONE copy; name a single \
                 folder",
                picked.len()
            )
        }));
    };

    // The scan is the ordinary per-placement one (each copy against ITS OWN recorded baseline) —
    // only the aggregate verdict is skipped, so the edited/clean answer is the same one every other
    // surface reads.
    let scans = crate::placement::scan_placements(ctx, map)?;
    let Some(row) = scans.iter().find(|s| s.idx == idx) else {
        return Err(ClientError::Corrupt("placement scan missing a row".into()));
    };
    if !matches!(row.status, ScanStatus::Modified { .. }) {
        return Err(unusable_copy(&spellings[idx].display, &row.status));
    }
    Ok(SelectedCopy {
        dir: dirs[idx].clone(),
        spelling: spellings[idx].clone(),
        others_edited: scans
            .iter()
            .filter(|s| s.idx != idx && matches!(s.status, ScanStatus::Modified { .. }))
            .filter_map(|s| spellings.get(s.idx).map(|sp| sp.display.clone()))
            .collect(),
    })
}

/// Whether a typed token names this copy — matched against the four spellings it answers to (the
/// recorded folder, the display path, and either absolute), each compared after the same light
/// normalization (`./` prefix, trailing slashes) so a pasted path is not refused over punctuation.
pub(crate) fn names_copy(token: &str, dir: &Path, spelling: &CopySpelling) -> bool {
    let want = norm(token);
    let parent = dir.parent().unwrap_or(dir);
    [
        spelling.dest.clone(),
        spelling.display.clone(),
        dir.display().to_string(),
        parent.display().to_string(),
    ]
    .iter()
    .any(|candidate| norm(candidate) == want)
}

/// A path token as it is compared: trimmed, `./`-stripped, trailing slashes dropped.
fn norm(raw: &str) -> String {
    let t = raw.trim();
    let t = t.strip_prefix("./").unwrap_or(t);
    t.trim_end_matches('/').to_owned()
}

/// The refusal for a named copy that holds nothing to act on. A CLEAN copy is the one the rule was
/// written for — naming it would otherwise publish nothing, or reset nothing, and report success;
/// the other three states say what is actually in the way instead of pretending it is emptiness.
fn unusable_copy(display: &str, status: &ScanStatus) -> ClientError {
    use crate::placement::Drift;
    // The words come from the ONE drift vocabulary — the classification, never the bytes.
    ClientError::SelectionRefused(match status.drift() {
        Drift::Clean => {
            format!("that copy has no edits — {display} holds the version topos placed there")
        }
        Drift::Absent => format!("that copy is not there — nothing is placed at {display}"),
        Drift::Foreign => {
            format!("{display} holds files topos did not write — it is not a copy of this bundle")
        }
        Drift::Modified | Drift::Unscannable => {
            format!("{display} cannot be read safely — nothing was touched")
        }
    })
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
///
/// The way out names what `--dest` actually takes for this kind: an MCP server lands as an ENTRY
/// in a config FILE, and telling a person to name a folder for one sent them somewhere no MCP
/// entry has ever been written.
fn no_scope_dir(slug: &str, scope: ManifestScope, mcp: bool) -> ClientError {
    let (what, scope_word, way_out) = match (mcp, scope) {
        (true, ManifestScope::Global) => ("config file", "machine-wide", "file"),
        (true, ManifestScope::Project) => ("project config file", "project", "file"),
        (false, ManifestScope::Global) => ("machine-wide skills folder", "machine-wide", "folder"),
        (false, ManifestScope::Project) => ("project skills folder", "project", "folder"),
    };
    ClientError::SelectionRefused(format!(
        "agent `{slug}` has no {what} — `-a {slug}` cannot place at the {scope_word} scope; name \
         a {way_out} with `--dest` instead"
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

/// The registry slug a recorded skills-folder entry maps BACK to at `scope` — ONLY when exactly
/// ONE slug spells that folder here. `None` when no slug does, and equally when SEVERAL do (an
/// undo then says `--dest <folder>`).
///
/// The map is many-to-one: at project scope `.agents/skills` is the default of some nineteen
/// agents, so "the alphabetically first slug claiming it" would answer `amp` to a person who
/// typed `-a codex` — a reconstruction that neither matches what they ran nor names anything they
/// can check. The folder is the fact the row actually recorded; where the slug is not the row's
/// unambiguous other spelling, the undo prints the folder and stays verifiable.
pub(crate) fn slug_for_skill_entry(entry: &str, scope: ManifestScope) -> Option<String> {
    let mut claimants = topos_harness::registry::known_harnesses()
        .iter()
        .filter(|h| {
            crate::manifest::dest::skills_dest_spelling(h.slug, scope).as_deref() == Some(entry)
        });
    let only = claimants.next()?;
    claimants.next().is_none().then(|| only.slug.to_owned())
}

/// The MCP slug a config-file entry maps back to (the descriptor match the engine itself uses).
pub(crate) fn slug_for_mcp_entry(entry: &str, scope: ManifestScope) -> Option<String> {
    crate::manifest::dest::mcp_slug_for_dest(entry, scope).map(str::to_owned)
}

/// A destination set re-spelled as the argv tail that reconstructs it: `-a <slug>` where the
/// entry maps back to EXACTLY ONE slug in the table for this scope, `--dest <entry>` otherwise —
/// entry order preserved. An undo must name what the person can verify, so a folder several
/// agents share is spelled as the folder rather than as one arbitrary slug of the set.
pub(crate) fn undo_tail(entries: &[String], scope: ManifestScope, kind: BundleKind) -> Vec<String> {
    let mut out = Vec::new();
    for entry in entries {
        let slug = match kind {
            BundleKind::Mcp => slug_for_mcp_entry(entry, scope),
            BundleKind::Skill => slug_for_skill_entry(entry, scope),
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

    /// The four refusals a named-but-unusable copy takes, whole. The foreign one says FILES, not
    /// the word for what a file is made of: a person looking at a folder sees files in it, and the
    /// sentence is about what is in the folder, not about its contents' representation.
    #[test]
    fn a_named_copy_that_cannot_be_acted_on_says_which_of_the_four_it_is() {
        let msg = |status: &ScanStatus| -> String {
            match unusable_copy("~/.claude/skills/coolify-deploy", status) {
                ClientError::SelectionRefused(m) => m,
                other => panic!("expected a selection refusal, got {other:?}"),
            }
        };
        assert_eq!(
            msg(&ScanStatus::Foreign),
            "~/.claude/skills/coolify-deploy holds files topos did not write — it is not a copy \
             of this bundle"
        );
        assert_eq!(
            msg(&ScanStatus::Clean { digest: [0; 32] }),
            "that copy has no edits — ~/.claude/skills/coolify-deploy holds the version topos \
             placed there"
        );
        assert_eq!(
            msg(&ScanStatus::Absent),
            "that copy is not there — nothing is placed at ~/.claude/skills/coolify-deploy"
        );
        assert_eq!(
            msg(&ScanStatus::Unscannable),
            "~/.claude/skills/coolify-deploy cannot be read safely — nothing was touched"
        );
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

    /// The DEFAULT-REACH token is a row's own spelling, never something to ask for: there is
    /// nothing at `*` to place at. `--dest '*'` refuses at the argv, teaching the command that
    /// already does what it was reaching for.
    #[test]
    fn the_default_reach_token_is_not_an_askable_destination() {
        for scope in [ManifestScope::Global, ManifestScope::Project] {
            for entries in [
                sel(&[], &["*"]).skill_entries(scope),
                sel(&[], &["*"]).mcp_entries(scope),
            ] {
                let ClientError::SelectionRefused(m) = entries.unwrap_err() else {
                    panic!("the token refuses as a selection");
                };
                assert_eq!(
                    m,
                    "`*` is not a destination — it is the topos.toml token for a row's default \
                     reach; a bare `topos add <bundle>` already reaches every agent"
                );
            }
        }
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
            BundleKind::Skill,
        );
        assert_eq!(tail, vec!["-a", "claude-code", "-a", "codex"]);
        let tail = undo_tail(
            &["~/somewhere/else".to_owned()],
            ManifestScope::Global,
            BundleKind::Skill,
        );
        assert_eq!(tail, vec!["--dest", "~/somewhere/else"]);
        let tail = undo_tail(
            &["~/.codex/config.toml".to_owned()],
            ManifestScope::Global,
            BundleKind::Mcp,
        );
        assert_eq!(tail, vec!["-a", "codex"]);
    }

    /// ONE copy answers to three spellings — the folder its manifest row records, the display path
    /// a receipt prints, and either of those absolute — with `./` and a trailing slash tolerated on
    /// every one. That is the whole promise of `--dest` on `diff`/`publish`/`update --reset`: a
    /// person pastes back whatever topos last printed at them, in whichever form they have.
    #[test]
    fn one_copy_answers_to_the_row_the_display_path_and_the_absolute_form() {
        let dir = Path::new("/proj/.claude/skills/coolify-deploy");
        let spelling = CopySpelling {
            display: "project/.claude/skills/coolify-deploy".to_owned(),
            dest: ".claude/skills".to_owned(),
        };
        for token in [
            // The row's own spelling — what the freeze menu and `topos.toml` print.
            ".claude/skills",
            "./.claude/skills",
            ".claude/skills/",
            // The display path — what `list` and the `update` receipt print.
            "project/.claude/skills/coolify-deploy",
            "./project/.claude/skills/coolify-deploy",
            // Absolute, either the dir or the folder holding it.
            "/proj/.claude/skills/coolify-deploy",
            "/proj/.claude/skills",
            "/proj/.claude/skills/",
        ] {
            assert!(names_copy(token, dir, &spelling), "{token}");
        }
        // A neighbour folder, a prefix of the row, and the bare skill name name nothing.
        for token in [
            ".agents/skills",
            ".claude",
            "coolify-deploy",
            "project/.claude/skills",
        ] {
            assert!(!names_copy(token, dir, &spelling), "{token}");
        }
    }

    /// The selector's tokens are the SAME agent resolution a recorded row gets — and a literal
    /// folder passes through untouched, INCLUDING an absolute one inside a project, which the
    /// manifest dialect refuses as a line but which is a perfectly ordinary way to name a copy that
    /// is already there.
    #[test]
    fn copy_tokens_reuse_the_agent_resolution_and_pass_literals_through() {
        assert_eq!(
            sel(&["claude-code"], &[])
                .copy_tokens(ManifestScope::Project)
                .unwrap(),
            vec![".claude/skills".to_owned()]
        );
        assert_eq!(
            sel(&["codex"], &[])
                .copy_tokens(ManifestScope::Global)
                .unwrap(),
            vec!["~/.codex/skills".to_owned()]
        );
        // The literal a `--dest` line could never RECORD at this scope still NAMES a copy here.
        assert_eq!(
            sel(&[], &["/proj/.claude/skills"])
                .copy_tokens(ManifestScope::Project)
                .unwrap(),
            vec!["/proj/.claude/skills".to_owned()]
        );
        assert!(
            sel(&[], &["/proj/.claude/skills"])
                .skill_entries(ManifestScope::Project)
                .is_err(),
            "the premise: a project FILE refuses to record that spelling"
        );
        // An unknown slug still refuses with the registry list — the selector teaches the same way.
        assert!(matches!(
            sel(&["codx"], &[]).copy_tokens(ManifestScope::Global),
            Err(ClientError::UnknownAgent { .. })
        ));
    }

    /// A folder SEVERAL slugs share is reconstructed as the folder, never as one of them: the
    /// project default `.agents/skills` is where most of the registry lands, so `-a codex` in a
    /// project undoes as `--dest .agents/skills` — the fact the row recorded, and the one the
    /// person can check. Its machine-scope twin `~/.agents/skills` is shared too and behaves the
    /// same, while the single-claimant folders keep their slug.
    #[test]
    fn a_shared_folder_undoes_as_the_folder_not_one_of_its_slugs() {
        // The premise: `-a codex` in a project resolves to the folder most of the registry shares.
        let entries = sel(&["codex"], &[])
            .skill_entries(ManifestScope::Project)
            .unwrap();
        assert_eq!(entries, vec![".agents/skills".to_owned()]);
        let claimants = topos_harness::registry::known_harnesses()
            .iter()
            .filter(|h| h.project_dir() == ".agents/skills")
            .count();
        assert!(claimants > 1, "the premise: {claimants} slugs share it");

        assert_eq!(
            undo_tail(&entries, ManifestScope::Project, BundleKind::Skill),
            vec!["--dest", ".agents/skills"]
        );
        assert_eq!(
            undo_tail(
                &["~/.agents/skills".to_owned()],
                ManifestScope::Global,
                BundleKind::Skill
            ),
            vec!["--dest", "~/.agents/skills"]
        );
        // A folder exactly one slug spells still reconstructs as the slug.
        assert_eq!(
            undo_tail(
                &[".claude/skills".to_owned()],
                ManifestScope::Project,
                BundleKind::Skill
            ),
            vec!["-a", "claude-code"]
        );
    }
}
