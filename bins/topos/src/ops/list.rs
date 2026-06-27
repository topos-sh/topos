//! `list [<skill>] [--footprint]` — inventory this machine. This increment populates only the
//! **tracked** bucket (followed / published-by-you / untracked need the plane + adapters and render
//! empty). `--footprint` reports every topos-owned path outside skill dirs: the `~/.topos/` tree plus
//! any harness config the currency hook lives in (disclosed, never deleted).

use std::path::Path;

use topos_core::digest::to_hex;
use topos_types::persisted::{Lock, PlacementMap};
use topos_types::results::{ListData, SkillEntry};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::scan;
use crate::sidecar;
use crate::{doc, scan::ScannedBundle};

/// Inventory the tracked skills, optionally narrowed to one name and/or with the footprint.
///
/// # Errors
/// [`ClientError::NoSuchSkill`] / [`ClientError::AmbiguousName`] when a name filter does not resolve to
/// exactly one skill; otherwise a read failure.
pub(crate) fn list(
    ctx: &Ctx<'_>,
    skill: Option<&str>,
    want_footprint: bool,
) -> Result<ListData, ClientError> {
    let mut tracked = Vec::new();
    for entry in ctx.fs.read_dir(&ctx.layout.skills_dir())? {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Skip the transient staging dirs (and anything else hidden); a skill id never starts with '.'.
        if id.starts_with('.') || !entry.is_dir() {
            continue;
        }
        let paths = ctx.layout.published(id);
        let Some(lock): Option<Lock> = doc::read_doc(ctx.fs, &paths.lock)? else {
            continue;
        };
        let draft = is_draft(ctx, &paths.map, &lock)?;
        tracked.push(SkillEntry {
            skill: lock.name,
            version_id: lock.base_commit,
            bundle_digest: lock.bundle_digest,
            draft,
            pending_proposals: Vec::new(),
        });
    }
    // Deterministic order (name, then version).
    tracked.sort_by(|a, b| {
        a.skill
            .cmp(&b.skill)
            .then_with(|| a.version_id.cmp(&b.version_id))
    });

    if let Some(want) = skill {
        let count = tracked.iter().filter(|e| e.skill == want).count();
        match count {
            0 => {
                return Err(ClientError::NoSuchSkill {
                    name: want.to_owned(),
                });
            }
            1 => tracked.retain(|e| e.skill == want),
            count => {
                return Err(ClientError::AmbiguousName {
                    name: want.to_owned(),
                    count,
                });
            }
        }
    }

    let footprint = if want_footprint {
        // The `~/.topos/` walk PLUS any harness config path topos holds a managed entry in (disclosed,
        // never deleted) — every topos-owned path outside skill dirs.
        let mut paths = sidecar::footprint(ctx.fs, &ctx.layout)?;
        paths.extend(
            ctx.harness
                .uninstall_footprint()
                .iter()
                .map(|p| p.to_string_lossy().into_owned()),
        );
        paths.sort();
        Some(paths)
    } else {
        None
    };

    Ok(ListData {
        followed: Vec::new(),
        published_by_you: Vec::new(),
        tracked,
        untracked: Vec::new(),
        footprint,
    })
}

/// A skill carries a draft iff the live source bytes hash to a different `bundle_digest` than the lock
/// pins. A missing/unscannable source is reported as no-draft (nothing to compare), never an error.
fn is_draft(ctx: &Ctx<'_>, map_path: &Path, lock: &Lock) -> Result<bool, ClientError> {
    let Some(map): Option<PlacementMap> = doc::read_doc(ctx.fs, map_path)? else {
        return Ok(false);
    };
    let Some(placement) = map.placements.first() else {
        return Ok(false);
    };
    let source = Path::new(placement);
    if !source.exists() {
        return Ok(false);
    }
    match scan::scan(source) {
        Ok(ScannedBundle { bundle_digest, .. }) => Ok(to_hex(&bundle_digest) != lock.bundle_digest),
        Err(_) => Ok(false),
    }
}
