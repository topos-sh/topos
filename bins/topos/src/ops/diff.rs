//! `diff <skill>` (bare) — draft ↔ current, where local `current` is the on-machine base commit. The
//! `current..<hash>` plane half lands later.

use std::path::Path;

use topos_gitstore::Store;
use topos_types::persisted::PlacementMap;
use topos_types::results::{DiffData, DiffSource};

use super::{parse_hex32, resolve_skill};
use crate::ctx::Ctx;
use crate::diff::{FileBytes, unified_bundle_diff};
use crate::error::ClientError;
use crate::scan;
use crate::{doc, scan::ScannedBundle};

/// Render the bare draft↔current diff for `skill`.
///
/// # Errors
/// [`ClientError::NoSuchSkill`] / [`ClientError::AmbiguousName`] on name resolution; an integrity error
/// if the base version fails verify-on-read; a scan/io error reading the draft.
pub(crate) fn diff(ctx: &Ctx<'_>, skill: &str) -> Result<DiffData, ClientError> {
    let (id, lock) = resolve_skill(ctx, skill)?;
    let paths = ctx.layout.published(&id);

    // The base (current) bytes, verified on read against the pinned digest.
    let store = Store::open(&paths.store)?;
    let version_id = parse_hex32(&lock.base_commit)?;
    let bundle_digest = parse_hex32(&lock.bundle_digest)?;
    let base = store.render_verified(version_id, bundle_digest)?;

    // The draft = the live source, re-scanned.
    let map: PlacementMap = doc::read_doc(ctx.fs, &paths.map)?
        .ok_or_else(|| ClientError::Corrupt("missing placement map".into()))?;
    let placement = map
        .placements
        .first()
        .ok_or_else(|| ClientError::Corrupt("placement map has no path".into()))?;
    let ScannedBundle { files: draft, .. } = scan::scan(Path::new(placement))?;

    let base_files: Vec<FileBytes<'_>> = base
        .files
        .iter()
        .map(|f| FileBytes {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    let draft_files: Vec<FileBytes<'_>> = draft
        .iter()
        .map(|f| FileBytes {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    let diff = unified_bundle_diff(&base_files, &draft_files);

    Ok(DiffData {
        source: DiffSource::Local,
        version_id: lock.base_commit,
        bundle_digest: lock.bundle_digest,
        diff,
    })
}
