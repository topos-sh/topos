//! `state/visited_stores.json` — the machine-local index of project stores the reconcile has
//! visited.
//!
//! The applied report is COMPLETE-state per session: what this installation holds for the
//! workspace, wherever it holds it. A project store only sits on the cwd's ancestor chain while
//! an update runs FROM that checkout — so the reconcile records every project store it visits
//! here (home sidecar, machine-local), and every later report unions the recorded stores in,
//! whichever checkout the update runs from. Nothing here grants anything: the index is pure
//! bookkeeping over where managed bytes live. A recorded store that no longer exists is pruned
//! on read (and a recorded store whose placements are gone simply contributes nothing) — the
//! natural drop. A PLAIN document (paths, never a secret) through the ordinary crash-safe
//! [`crate::doc`] writers.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use topos_types::PERSISTED_SCHEMA_VERSION;

use crate::ctx::Ctx;
use crate::sidecar;

/// The whole document: the project DIRECTORIES whose stores a reconcile has visited (a
/// `BTreeSet`, so the on-disk bytes are deterministic).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VisitedStores {
    #[serde(default)]
    pub schema_version: u32,
    /// Absolute project-dir paths (the store lives at `<dir>/.topos/state/<user>/`).
    #[serde(default)]
    pub stores: BTreeSet<String>,
}

/// Union the recorded stores with this run's own chain (`current` — project dirs, stored when
/// they carry a live store), prune what no longer exists, persist the result best-effort, and
/// return the LAYOUTS of every store that is real right now. `ctx` must be the BASE ctx (home
/// layout).
pub(crate) fn recall_and_record(ctx: &Ctx<'_>, current: &[PathBuf]) -> Vec<sidecar::Layout> {
    let path = ctx.layout.visited_stores_path();
    let prior: VisitedStores = crate::doc::read_doc(ctx.fs, &path)
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut candidates: BTreeSet<String> = prior.stores;
    for dir in current {
        candidates.insert(dir.display().to_string());
    }
    let mut layouts = Vec::new();
    let mut kept = BTreeSet::new();
    for dir in candidates {
        // The natural drop: a dir whose store is gone leaves the index on this read.
        if let Some(layout) = sidecar::existing_project_store(ctx.fs, std::path::Path::new(&dir)) {
            layouts.push(layout);
            kept.insert(dir);
        }
    }
    // Best-effort persistence — a failed write just re-derives next run from what still exists.
    let next = VisitedStores {
        schema_version: PERSISTED_SCHEMA_VERSION,
        stores: kept,
    };
    if ctx.fs.create_dir_all(&ctx.layout.state_dir()).is_ok() {
        let _ = crate::doc::write_doc(ctx.fs, &path, &next);
    }
    layouts
}
