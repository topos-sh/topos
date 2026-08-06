//! `state/connected.json` — the machine-local record of every workspace this machine has EVER
//! connected to, plus the one-time feed-line migration's done marker.
//!
//! `topos login` writes a workspace's feed line (`"<host>/<workspace>" = "*"`) into the global
//! manifest only on this machine's FIRST connection to that workspace — this document is the
//! witness that decides "first". It deliberately OUTLIVES logout: a person who deleted the feed
//! line and later reconnects must not have the line re-added, so the record of having connected
//! must survive the session that created it. ENGINE-INTERNAL, always: no receipt, no `--json`
//! field, no list/status line ever surfaces it. A PLAIN document (addresses, never a secret)
//! through the ordinary crash-safe [`crate::doc`] writers.
//!
//! UNDECIPHERABLE IS NOT EMPTY. A corrupt document — or one written by a NEWER build, which
//! [`crate::doc::read_doc`] refuses rather than guesses at — is never written over, and the login
//! treats it as holding EVERY workspace ([`first_connection`] answers `false`): re-adding a
//! deliberately deleted feed line is the one wrong this document exists to prevent, so an
//! unreadable witness fails toward writing nothing.
//!
//! THE WHOLE READ-UNION-WRITE IS SERIALIZED. Two logins can land concurrently (an agent per
//! terminal), and each union is built from the bytes it read — so without a lock the second
//! writer's document simply would not contain the first writer's workspace, and a later login
//! would re-add a line it had no business touching. The lock is the same `flock` every other
//! writer here takes (`locks/connected.lock`), held across read → union → write.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use topos_types::PERSISTED_SCHEMA_VERSION;

use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// The whole document: the workspaces this machine has ever connected to (a `BTreeSet`, so the
/// on-disk bytes are deterministic), and the one-time feed-line migration marker.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ConnectedWorkspaces {
    #[serde(default)]
    pub schema_version: u32,
    /// The one-time feed-line migration's done marker (see `crate::ops::ensure_feed_migration`).
    #[serde(default)]
    pub migrated: bool,
    /// `"<host>/<workspace-name>"` keys — the manifest reference grammar's feed spelling.
    #[serde(default)]
    pub workspaces: BTreeSet<String>,
}

/// The witness key for `(host, workspace)` — the feed reference's own spelling.
pub(crate) fn key(host: &str, workspace: &str) -> String {
    format!("{host}/{workspace}")
}

/// The standing document, if one exists.
///
/// # Errors
/// As [`crate::doc::read_doc`] — an unreadable or newer-build document is an error, never
/// "empty" (see the module note).
pub(crate) fn read(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Option<ConnectedWorkspaces>, ClientError> {
    crate::doc::read_doc(fs, &layout.connected_path())
}

/// Whether this is the machine's FIRST connection to `(host, workspace)`: the document is
/// readable and does not hold the key. An UNREADABLE document answers `false` — failing toward
/// never re-adding a feed line (the module note's one wrong).
pub(crate) fn first_connection(
    fs: &dyn FsOps,
    layout: &Layout,
    host: &str,
    workspace: &str,
) -> bool {
    match read(fs, layout) {
        Ok(doc) => !doc
            .unwrap_or_default()
            .workspaces
            .contains(&key(host, workspace)),
        Err(_) => false,
    }
}

/// Record `(host, workspace)` as connected — a read-union-write under the document's own lock.
///
/// # Errors
/// As [`with_doc`] — an undecipherable standing document refuses (left byte-untouched).
pub(crate) fn record(
    fs: &dyn FsOps,
    layout: &Layout,
    host: &str,
    workspace: &str,
) -> Result<(), ClientError> {
    let k = key(host, workspace);
    with_doc(fs, layout, |doc| {
        doc.workspaces.insert(k);
    })
}

/// Read → mutate → write the document under its writer lock. The read failing closed is what
/// keeps an undecipherable (or newer-build) document byte-untouched.
///
/// # Errors
/// The read failure (see the module note), or the [`FsOps`] lock/write failure.
pub(crate) fn with_doc(
    fs: &dyn FsOps,
    layout: &Layout,
    f: impl FnOnce(&mut ConnectedWorkspaces),
) -> Result<(), ClientError> {
    fs.create_dir_all(&layout.locks_dir())?;
    let _guard = fs.lock_exclusive(&layout.connected_lock_file())?;
    let mut doc = read(fs, layout)?.unwrap_or_default();
    f(&mut doc);
    doc.schema_version = PERSISTED_SCHEMA_VERSION;
    fs.create_dir_all(&layout.state_dir())?;
    crate::doc::write_doc(fs, &layout.connected_path(), &doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("topos-connected-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn record_unions_and_first_connection_reads_it() {
        let home = scratch("union");
        let layout = Layout::new(&home.join(".topos"));
        let fs = RealFs;
        // A missing document holds nothing: every workspace is a first connection.
        assert!(first_connection(&fs, &layout, "topos.sh", "acme"));
        record(&fs, &layout, "topos.sh", "acme").unwrap();
        assert!(!first_connection(&fs, &layout, "topos.sh", "acme"));
        // Keys carry the HOST half too — the same name on another server is its own first.
        assert!(first_connection(&fs, &layout, "topos.example.com", "acme"));
        record(&fs, &layout, "topos.example.com", "acme").unwrap();
        let doc = read(&fs, &layout).unwrap().unwrap();
        assert_eq!(doc.workspaces.len(), 2);
        assert!(!doc.migrated);
        assert_eq!(doc.schema_version, PERSISTED_SCHEMA_VERSION);
    }

    #[test]
    fn an_undecipherable_document_answers_held_and_is_never_written_over() {
        let home = scratch("corrupt");
        let layout = Layout::new(&home.join(".topos"));
        let fs = RealFs;
        std::fs::create_dir_all(layout.state_dir()).unwrap();
        std::fs::write(layout.connected_path(), b"not json").unwrap();
        // Fail toward never re-adding: unreadable reads as "held".
        assert!(!first_connection(&fs, &layout, "topos.sh", "acme"));
        // …and the write-back is refused, the standing bytes untouched.
        assert!(record(&fs, &layout, "topos.sh", "acme").is_err());
        assert_eq!(std::fs::read(layout.connected_path()).unwrap(), b"not json");
    }
}
