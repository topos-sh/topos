//! `diff <skill> [<ref>]` — the single show-the-change verb. Bare = draft ↔ current (the on-machine base);
//! `<hash>` / `@<hash>` = `current..<hash>` (review that version against current); `<a>..<b>` = version ↔
//! version. In a `<ref>` diff (the proposal-review view) an endpoint is `current` (the PLANE's live signed
//! current — the trunk a proposal lands on, so a behind reviewer still diffs against the real current) or a
//! 64-hex version id; either way the bytes are **fetched ONCE and re-verified** (the same bytes that
//! reproduce the version id are the bytes displayed — never a second, unverified fetch).

use std::path::Path;

use topos_core::digest::{FileMode, to_hex};
use topos_gitstore::{DiffFile, Store, unified_diff};
use topos_types::persisted::{Lock, PlacementMap};
use topos_types::results::{DiffData, DiffSource};

use super::contribute;
use super::{parse_hex32, resolve_skill};
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

/// Render `skill`'s diff for the optional `<ref>`.
///
/// # Errors
/// [`ClientError::NoSuchSkill`] / [`ClientError::AmbiguousName`] on name resolution; an integrity error if a
/// version fails verify-on-read or a fetched version does not reproduce its id; a scan/transport failure.
pub(crate) fn diff(
    ctx: &Ctx<'_>,
    skill: &str,
    r#ref: Option<&str>,
) -> Result<DiffData, ClientError> {
    let (id, lock) = resolve_skill(ctx, skill)?;
    let sp = ctx.layout.published(&id);

    let Some(reference) = r#ref else {
        return diff_draft_vs_current(ctx, &sp, &lock);
    };

    // Parse the ref: `<a>..<b>` is a range; otherwise a single endpoint compared against `current` (so a
    // bare `<hash>` reviews that version against current, exactly like `current..<hash>`).
    let (from, to) = match reference.split_once("..") {
        Some((a, b)) => (a.to_owned(), b.to_owned()),
        None => ("current".to_owned(), reference.to_owned()),
    };

    let base = resolve_endpoint(ctx, &id, &from)?;
    let target = resolve_endpoint(ctx, &id, &to)?;

    let diff = unified_diff(&diff_files(&base.files), &diff_files(&target.files));
    Ok(DiffData {
        // Both endpoints of a `<ref>` diff are plane-fetched + re-verified.
        source: DiffSource::Plane,
        version_id: target.version_hex,
        bundle_digest: target.digest_hex,
        diff,
    })
}

/// The bare draft ↔ current diff (current = the on-machine base commit).
fn diff_draft_vs_current(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    lock: &Lock,
) -> Result<DiffData, ClientError> {
    let store = Store::open(&sp.store)?;
    let version_id = parse_hex32(&lock.base_commit)?;
    let bundle_digest = parse_hex32(&lock.bundle_digest)?;
    let base = store.render_verified(version_id, bundle_digest)?;

    let map: PlacementMap = doc::read_doc(ctx.fs, &sp.map)?
        .ok_or_else(|| ClientError::Corrupt("missing placement map".to_owned()))?;
    let placement = map
        .placements
        .first()
        .ok_or_else(|| ClientError::Corrupt("placement map has no path".to_owned()))?;
    let ScannedBundle { files: draft, .. } = scan::scan(Path::new(placement))?;

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
    let diff = unified_diff(&base_files, &draft_files);

    Ok(DiffData {
        source: DiffSource::Local,
        version_id: lock.base_commit.clone(),
        bundle_digest: lock.bundle_digest.clone(),
        diff,
    })
}

/// Resolve a plane-backed diff endpoint to its verified bytes. `current` = the PLANE's live signed current
/// (authenticated; so a behind reviewer diffs against the real trunk a proposal lands on, not a stale local
/// view); a 64-hex id = that version (a proposal IS a version). Either way the bytes are fetched ONCE and
/// re-verified to reproduce the version id — the SAME bytes are returned for display, never a second,
/// unverified fetch (so a tampered plane cannot show benign bytes while the id commits to other bytes).
fn resolve_endpoint(
    ctx: &Ctx<'_>,
    skill_id: &str,
    endpoint: &str,
) -> Result<Endpoint, ClientError> {
    let ep = endpoint.strip_prefix('@').unwrap_or(endpoint);
    let version_id = if ep == "current" {
        let workspace_id = workspace_of(ctx, skill_id)?;
        contribute::fresh_current(ctx, skill_id, &workspace_id)?.0
    } else {
        parse_hex32(ep)?
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

/// The workspace a followed skill lives in (its expected pointer scope) — needed to authenticate the plane's
/// live `current` for a `<ref>` diff.
fn workspace_of(ctx: &Ctx<'_>, skill_id: &str) -> Result<String, ClientError> {
    ctx.follow
        .followed()
        .into_iter()
        .find(|(id, _)| id == skill_id)
        .map(|(_, fc)| fc.workspace_id)
        .ok_or_else(|| {
            ClientError::Plane(format!(
                "'{skill_id}' is not a followed skill; a plane diff needs its workspace"
            ))
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
