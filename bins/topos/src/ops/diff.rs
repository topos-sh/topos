//! `diff <skill> [<ref>]` — the single show-the-change verb. Bare = draft ↔ current (the on-machine base);
//! `<hash>` / `@<hash>` = `current..<hash>` (review that version against current); `<a>..<b>` = version ↔
//! version. In a `<ref>` diff (the proposal-review view) an endpoint is `current` (the PLANE's live signed
//! current — the trunk a proposal lands on, so a behind reviewer still diffs against the real current) or a
//! 64-hex version id; either way the bytes are **fetched ONCE and re-verified** (the same bytes that
//! reproduce the version id are the bytes displayed — never a second, unverified fetch).

use topos_core::digest::{FileMode, to_hex};
use topos_gitstore::{DiffFile, FileDiffSection, Store, unified_diff_sections};
use topos_types::persisted::{Lock, PlacementMap};
use topos_types::results::{DiffData, DiffPatchInfo, DiffSource};

use super::contribute;
use super::{VersionRef, parse_hex32, resolve_version_ref};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::scan::{self, ScannedBundle};
use crate::{doc, sidecar};

/// One resolved diff endpoint: its version id, consent digest, and the verified files.
struct Endpoint {
    version_hex: String,
    digest_hex: String,
    files: Vec<DiffFileOwned>,
}

/// An owned `(path, mode, bytes)` triple — the verified diff input from a plane fetch.
struct DiffFileOwned {
    path: String,
    mode: FileMode,
    bytes: Vec<u8>,
}

/// The default byte budget a `--json` diff is capped at when no explicit `--max-bytes` was given —
/// generous for review, small enough that a machine-generated monster diff cannot blow an agent's
/// context. TTY output stays uncapped by default; `--max-bytes 0` lifts the cap everywhere.
pub(crate) const DEFAULT_JSON_DIFF_BUDGET: usize = 64 * 1024;

/// The byte budget an emitted diff body honors. `None` = unlimited (the TTY default, and
/// `--max-bytes 0`); `Some(n)` caps the emitted hunks at `n` bytes, truncating at FILE boundaries.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DiffBudget(pub Option<usize>);

impl DiffBudget {
    /// Resolve the effective budget from the `--max-bytes` flag + the output surface: an explicit
    /// flag wins everywhere (`0` = unlimited); with no flag, `--json` defaults to
    /// [`DEFAULT_JSON_DIFF_BUDGET`] and the TTY stays uncapped (human output is left alone).
    pub(crate) fn resolve(max_bytes: Option<u64>, json: bool) -> Self {
        match max_bytes {
            Some(0) => DiffBudget(None),
            Some(n) => DiffBudget(Some(usize::try_from(n).unwrap_or(usize::MAX))),
            None if json => DiffBudget(Some(DEFAULT_JSON_DIFF_BUDGET)),
            None => DiffBudget(None),
        }
    }

    /// The unlimited budget — for internal diff consumers (loss disclosures, the reset describe)
    /// that must never truncate.
    pub(crate) fn unlimited() -> Self {
        DiffBudget(None)
    }
}

/// Render `skill`'s diff for the optional `<ref>`, capped by `budget` (file-boundary truncation).
///
/// # Errors
/// [`ClientError::NoSuchSkill`] / [`ClientError::AmbiguousName`] on name resolution; an integrity error if a
/// version fails verify-on-read or a fetched version does not reproduce its id; a scan/transport failure.
pub(crate) fn diff(
    ctx: &Ctx<'_>,
    skill: &str,
    r#ref: Option<&str>,
    budget: DiffBudget,
    sel: &super::Selection,
) -> Result<DiffData, ClientError> {
    // Resolve WHERE YOU STAND: a bundle a project `topos.toml` delivers keeps its custody in the
    // checkout's own store, and a `diff` run from inside that checkout is about THAT copy — not a
    // same-named machine twin, and not a not-found.
    let (layout, id, lock) = super::resolve_skill_here(ctx, skill, None)?;
    if !sel.is_empty() && r#ref.is_some() {
        return Err(ClientError::InvalidArgument(
            "`--dest`/`-a` names the copy of YOUR edits a diff reads, and a version-to-version \
             diff reads the same in every folder — drop the `<ref>`, or drop the selector"
                .into(),
        ));
    }
    diff_resolved(ctx, &layout, &id, &lock, r#ref, budget, sel)
}

/// [`diff`] over an ALREADY-RESOLVED copy — the entry the reset describe uses, so the loss it
/// discloses is measured against exactly the store the discard will act on (a second, independent
/// resolution could land on the other scope's copy and describe bytes nobody is about to lose).
/// Every store/doc/scan read rides the OWNING layout; the plane and follow seams are machine-level
/// and stay as they are.
/// The `-a`/`--dest` selector narrows the RIGHT-HAND side only. The left side stays what it always
/// is — the applied base — so two `--dest` runs answer against the same ruler and are directly
/// comparable, which is what makes them a substitute for the copy-vs-copy grammar this deliberately
/// does not invent.
pub(crate) fn diff_resolved(
    ctx: &Ctx<'_>,
    layout: &sidecar::Layout,
    id: &crate::id::SkillId,
    lock: &Lock,
    r#ref: Option<&str>,
    budget: DiffBudget,
    sel: &super::Selection,
) -> Result<DiffData, ClientError> {
    let sctx = super::pull::ctx_with_layout(ctx, layout);
    let sp = layout.published(id);

    let Some(reference) = r#ref else {
        return diff_draft_vs_current(&sctx, &sp, lock, budget, sel);
    };

    // Parse the ref: `<a>..<b>` is a range; otherwise a single endpoint compared against `current` (so a
    // bare `<hash>` reviews that version against current, exactly like `current..<hash>`).
    let (from, to) = match reference.split_once("..") {
        Some((a, b)) => (a.to_owned(), b.to_owned()),
        None => ("current".to_owned(), reference.to_owned()),
    };

    let base = resolve_endpoint(&sctx, id, &from)?;
    let target = resolve_endpoint(&sctx, id, &to)?;

    let sections = unified_diff_sections(&diff_files(&base.files), &diff_files(&target.files));
    let (diff, truncated, files) = apply_budget(sections, budget);
    Ok(DiffData {
        // Both endpoints of a `<ref>` diff are plane-fetched + re-verified.
        source: DiffSource::Plane,
        version_id: target.version_hex,
        bundle_digest: target.digest_hex,
        diff,
        truncated,
        files,
        // A version-vs-version diff has no local copy to name: the same bytes reproduce the same
        // ids everywhere, so no folder is part of the answer.
        dest: None,
        skill: None,
    })
}

/// Apply a byte budget to per-file diff sections: emit LEADING whole sections while the running
/// total stays within the budget; from the first section that would blow it, every remaining
/// section is omitted (a deterministic prefix — never an out-of-order cherry-pick, so the emitted
/// text is always a clean prefix of the full diff). Under budget (or unlimited), the concatenation
/// is byte-identical to the full unified diff and the additive fields stay empty/false.
fn apply_budget(
    sections: Vec<FileDiffSection>,
    budget: DiffBudget,
) -> (String, bool, Vec<DiffPatchInfo>) {
    let Some(max) = budget.0 else {
        return (
            sections.into_iter().map(|s| s.text).collect(),
            false,
            Vec::new(),
        );
    };
    let mut emitted = String::new();
    let mut rows = Vec::with_capacity(sections.len());
    let mut omitting = false;
    for s in &sections {
        if !omitting && emitted.len().saturating_add(s.text.len()) > max {
            omitting = true;
        }
        if !omitting {
            emitted.push_str(&s.text);
        }
        rows.push(DiffPatchInfo {
            path: s.path.clone(),
            patch_omitted: omitting,
            patch_bytes: s.text.len() as u64,
        });
    }
    if omitting {
        (emitted, true, rows)
    } else {
        // Everything fit — the pinned uncapped shape (no marker fields).
        (emitted, false, Vec::new())
    }
}

/// The bare draft ↔ current diff (current = the on-machine base commit). `ctx` is the OWNING
/// store's (see [`diff`]): the store, the placement map, and the drift scan all belong to whichever
/// scope holds this bundle's custody.
fn diff_draft_vs_current(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    lock: &Lock,
    budget: DiffBudget,
    sel: &super::Selection,
) -> Result<DiffData, ClientError> {
    let store = Store::open(&sp.store)?;
    let version_id = parse_hex32(&lock.base_commit)?;
    let bundle_digest = parse_hex32(&lock.bundle_digest)?;
    let base = store.render_verified(version_id, bundle_digest)?;

    let map: PlacementMap = doc::read_map(ctx.fs, &sp.map)?
        .ok_or_else(|| ClientError::Corrupt("missing placement map".to_owned()))?;
    // A CONFIG-PLACED (mcp) bundle has no working tree — its bytes live in agent configs, not in
    // an editable dir — so the honest bare-diff answer is the no-local-draft shape: an empty diff
    // whose endpoint IS the held current (never the "map has no placement" corruption error a
    // store-only record would otherwise trip).
    if map.placements.is_empty()
        && crate::mcp_engine::record_kind(ctx, &lock.skill_id, &map)
            == crate::mcp_engine::RecordKind::Mcp
    {
        return Ok(DiffData {
            source: DiffSource::Local,
            version_id: lock.base_commit.clone(),
            bundle_digest: lock.bundle_digest.clone(),
            diff: String::new(),
            truncated: false,
            files: Vec::new(),
            dest: None,
            skill: None,
        });
    }
    // The draft side is the WORK TREE — the single edited copy when one exists (draft-anywhere),
    // else the first placement; several divergent copies freeze typed. A `-a`/`--dest` selection
    // names ONE copy instead and BYPASSES that freeze: the aggregate classification refuses before
    // anything can be picked, which left a frozen bundle impossible to even read.
    let (placement, source_dir) = if sel.is_empty() {
        (
            crate::placement::work_tree_dir(ctx, &lock.name, &map)?,
            None,
        )
    } else {
        let picked = super::dest_select::select_copy(ctx, sel, &lock.name, &map)?;
        (picked.dir, Some(picked.spelling.display))
    };
    let ScannedBundle {
        files: draft,
        bundle_digest: draft_digest,
        ..
    } = scan::scan(&placement)?;
    // WHICH copy this read: named only where the bundle sits in more than one folder — with a
    // single copy there is nothing to disambiguate, and the answer stays exactly what it was.
    let dest = (map.placements.len() > 1).then(|| {
        source_dir.unwrap_or_else(|| super::dest_select::copy_spellings(ctx, &placement).display)
    });

    let base_files: Vec<DiffFile<'_>> = base
        .files
        .iter()
        .map(|f| DiffFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    let draft_files: Vec<DiffFile<'_>> = draft
        .iter()
        .map(|f| DiffFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    let sections = unified_diff_sections(&base_files, &draft_files);
    let (diff, truncated, files) = apply_budget(sections, budget);

    Ok(DiffData {
        source: DiffSource::Local,
        // The DRAFT is the target endpoint (like `target.digest_hex` on the `<ref>` path): report
        // ITS digest — the byte-exact value `publish <skill>@<digest>` consents to — not
        // the base's. When the draft equals current the scan reproduces the current digest, so a
        // no-change diff is unaffected.
        version_id: lock.base_commit.clone(),
        bundle_digest: to_hex(&draft_digest),
        diff,
        truncated,
        files,
        skill: dest.as_ref().map(|_| lock.name.clone()),
        dest,
    })
}

/// Resolve a plane-backed diff endpoint to its verified bytes. `current` = the PLANE's live signed current
/// (authenticated; so a behind reviewer diffs against the real trunk a proposal lands on, not a stale local
/// view); a 64-hex id = that version (a proposal IS a version); a short (≥8 char) prefix resolves against
/// the skill's locally recorded pointer history — which never holds an OPEN proposal's candidate id, so a
/// proposal review still pastes the full hash `publish --propose` / `list <skill>` already print. Either
/// way the bytes are fetched ONCE and re-verified to reproduce the version id — the SAME bytes are
/// returned for display, never a second, unverified fetch (so a tampered plane cannot show benign bytes
/// while the id commits to other bytes).
///
/// `ctx` is the OWNING store's (see [`diff`]) — the recorded pointer history a short prefix resolves
/// against is that store's; the plane and follow seams it carries are the machine's, unchanged.
fn resolve_endpoint(
    ctx: &Ctx<'_>,
    id: &crate::id::SkillId,
    endpoint: &str,
) -> Result<Endpoint, ClientError> {
    let skill_id = id.as_str();
    let ep = endpoint.strip_prefix('@').unwrap_or(endpoint);
    let version_id = if ep == "current" {
        let workspace_id = super::workspace_of(ctx, skill_id)?;
        contribute::fresh_current(ctx, skill_id, &workspace_id)?.0
    } else {
        // The endpoint is user-typed argv — a malformed hash is a usage error, never CORRUPT_STATE
        // (the draft-vs-current path's lock-field parses keep the corruption classification).
        let vref = VersionRef::parse_arg(
            ep,
            "a diff <ref> endpoint must be `current`, a 64-char lowercase hex version id, or a \
             unique prefix of at least 8 chars",
        )?;
        resolve_version_ref(
            &super::local_version_ids(ctx, &ctx.layout.published(id))?,
            &vref,
        )?
        .ok_or_else(|| {
            ClientError::InvalidArgument(format!(
                "'{}' matches no locally held version of this skill; use the full 64-char \
                     id (an open proposal's id is only known in full — `list <skill>` prints it)",
                vref.shown()
            ))
        })?
    };
    let (digest, fetched) = contribute::fetch_verified_bundle(ctx, skill_id, version_id)?;
    Ok(Endpoint {
        version_hex: to_hex(&version_id),
        digest_hex: to_hex(&digest),
        files: fetched
            .files
            .into_iter()
            .map(|f| DiffFileOwned {
                path: f.path,
                mode: f.mode,
                bytes: f.bytes,
            })
            .collect(),
    })
}

fn diff_files(files: &[DiffFileOwned]) -> Vec<DiffFile<'_>> {
    files
        .iter()
        .map(|f| DiffFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(path: &str, text: &str) -> FileDiffSection {
        FileDiffSection {
            path: path.to_owned(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn budget_resolution_defaults_json_capped_tty_uncapped_zero_lifts() {
        assert_eq!(
            DiffBudget::resolve(None, true).0,
            Some(DEFAULT_JSON_DIFF_BUDGET)
        );
        assert_eq!(DiffBudget::resolve(None, false).0, None);
        // `--max-bytes 0` lifts the cap on BOTH surfaces; an explicit value wins on both.
        assert_eq!(DiffBudget::resolve(Some(0), true).0, None);
        assert_eq!(DiffBudget::resolve(Some(10), false).0, Some(10));
    }

    #[test]
    fn budget_truncates_at_file_boundaries_as_a_prefix() {
        let sections = vec![
            section("a", "aaaa"),
            section("b", "bbbbbb"),
            section("c", "cc"),
        ];
        // 4 + 6 > 8 → only `a` fits; `c` (which WOULD fit alone) is still omitted — the emitted
        // text must stay a clean prefix of the full diff, never a cherry-pick.
        let (diff, truncated, files) = apply_budget(sections.clone(), DiffBudget(Some(8)));
        assert_eq!(diff, "aaaa");
        assert!(truncated);
        let flags: Vec<(&str, bool)> = files
            .iter()
            .map(|f| (f.path.as_str(), f.patch_omitted))
            .collect();
        assert_eq!(flags, vec![("a", false), ("b", true), ("c", true)]);
        assert_eq!(files[1].patch_bytes, 6);

        // Everything fits → the pinned uncapped shape (no marker fields at all).
        let (diff, truncated, files) = apply_budget(sections.clone(), DiffBudget(Some(64)));
        assert_eq!(diff, "aaaabbbbbbcc");
        assert!(!truncated && files.is_empty());

        // Unlimited is byte-identical to the concatenation.
        let (diff, truncated, files) = apply_budget(sections, DiffBudget::unlimited());
        assert_eq!(diff, "aaaabbbbbbcc");
        assert!(!truncated && files.is_empty());
    }
}
