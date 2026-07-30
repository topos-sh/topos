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
//!
//! UNDECIPHERABLE IS NOT EMPTY. A corrupt document — or one written by a NEWER build, which
//! [`crate::doc::read_doc`] refuses rather than guesses at — contributes NO recorded stores (this
//! run still knows its own cwd chain, which is a fact of the run, not of the file) and the
//! write-back is REFUSED: an older binary that re-derived an empty index would erase the newer
//! one's record of every other checkout, and every later report would silently under-state what
//! this installation holds.
//!
//! THE WHOLE READ-UNION-WRITE IS SERIALIZED. Two sweeps run from two checkouts are the ordinary
//! case (an agent per repository), and each one's union is built from the bytes it read — so
//! without a lock the second writer's document simply does not contain the first writer's
//! checkout, and that checkout's holdings drop out of every later applied report. The lock is the
//! same `flock` every other writer here takes (`locks/visited-stores.lock`), held across
//! read → union → write.

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
///
/// The read → union → write runs under the index's own writer lock (see the module note): a
/// concurrent sweep's checkout must not be dropped by a document built from bytes it had already
/// replaced. A lock this run cannot take is not fatal — the union still happens, unserialized,
/// which is exactly today's behavior and better than answering with no stores at all.
pub(crate) fn recall_and_record(ctx: &Ctx<'_>, current: &[PathBuf]) -> Vec<sidecar::Layout> {
    let path = ctx.layout.visited_stores_path();
    let _guard = ctx
        .fs
        .create_dir_all(&ctx.layout.locks_dir())
        .ok()
        .and_then(|()| {
            ctx.fs
                .lock_exclusive(&ctx.layout.visited_stores_lock_file())
                .ok()
        });
    // A document this build cannot decipher contributes nothing AND is never written over (see the
    // module note): `readable` carries that decision to the persist step below.
    let (prior, readable) = match crate::doc::read_doc::<VisitedStores>(ctx.fs, &path) {
        Ok(doc) => (doc.unwrap_or_default(), true),
        Err(_) => (VisitedStores::default(), false),
    };
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
    // An undecipherable standing document is left byte-untouched.
    let next = VisitedStores {
        schema_version: PERSISTED_SCHEMA_VERSION,
        stores: kept,
    };
    if readable && ctx.fs.create_dir_all(&ctx.layout.state_dir()).is_ok() {
        let _ = crate::doc::write_doc(ctx.fs, &path, &next);
    }
    layouts
}
