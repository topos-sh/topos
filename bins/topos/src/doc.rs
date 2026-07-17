//! Typed read/write of the persisted sidecar documents (lock / map / sync), atomic on write and
//! fail-closed on an unknown schema version.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;
use topos_types::persisted::{PlacementKind, PlacementMap, PlacementState};
use topos_types::{PERSISTED_SCHEMA_VERSION, PLACEMENT_MAP_SCHEMA_VERSION};

use crate::atomic::{atomic_write, atomic_write_private, load_versioned};
use crate::error::ClientError;
use crate::fs_seam::FsOps;

/// The 64-char all-zero hex sentinel (a never-materialized baseline's `materialized_sha`).
const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Serialize a document (pretty + trailing newline — the committed on-disk shape) and write it atomically.
///
/// # Errors
/// [`ClientError::Corrupt`] if serialization fails; otherwise the [`FsOps`] failure.
pub(crate) fn write_doc<T: Serialize>(
    fs: &dyn FsOps,
    target: &Path,
    doc: &T,
) -> Result<(), ClientError> {
    let mut bytes = serde_json::to_vec_pretty(doc)
        .map_err(|e| ClientError::Corrupt(format!("serialize: {e}")))?;
    bytes.push(b'\n');
    atomic_write(fs, target, &bytes)
}

/// Read + parse a document, returning `None` if the file does not exist. Fails closed on an
/// unknown/newer `schema_version` (never silently parsed).
///
/// # Errors
/// As [`load_versioned`], plus the [`FsOps`] read failure.
pub(crate) fn read_doc<T: DeserializeOwned>(
    fs: &dyn FsOps,
    path: &Path,
) -> Result<Option<T>, ClientError> {
    match fs.read_opt(path)? {
        None => Ok(None),
        Some(bytes) => Ok(Some(load_versioned::<T>(&bytes, PERSISTED_SCHEMA_VERSION)?)),
    }
}

/// Read + upgrade `map.json` — the ONE persisted document with its own schema ceiling
/// ([`PLACEMENT_MAP_SCHEMA_VERSION`], v2: the per-placement `placement_state` shape). A v1 document
/// (one placement, map-level state only — real installs exist) upgrades LOSSLESSLY in memory: the
/// single placement's state is synthesized from the map-level fields (an all-zero map-level sha is
/// the never-received baseline's "never materialized" sentinel → `None`). Every other document keeps
/// [`read_doc`]'s [`PERSISTED_SCHEMA_VERSION`] dispatch; the same fail-closed rule applies here (a v3
/// map is an upgrade error, never silently parsed or deleted).
///
/// # Errors
/// As [`read_doc`]; additionally [`ClientError::Corrupt`] when a v2 document's `placement_state` is
/// not 1:1 with `placements` (a hand-edited half state the engine must never guess over).
pub(crate) fn read_map(fs: &dyn FsOps, path: &Path) -> Result<Option<PlacementMap>, ClientError> {
    let Some(bytes) = fs.read_opt(path)? else {
        return Ok(None);
    };
    let mut map: PlacementMap = load_versioned(&bytes, PLACEMENT_MAP_SCHEMA_VERSION)?;
    if map.placement_state.is_empty() && !map.placements.is_empty() {
        // The v1 shape (also tolerated on a v2 doc, where it means the same thing): synthesize the
        // per-placement state from the map-level fields. v1 recorded exactly one placement; a native
        // dir when a harness slug was attributed, else a plain tracked dir (still `native` — the
        // shared kind is minted only by the placement engine, never by an upgrade guess).
        map.placement_state = map
            .placements
            .iter()
            .map(|_| PlacementState {
                kind: PlacementKind::Native,
                agent: map.harness_slug.clone(),
                materialized_sha: (map.materialized_sha != ZERO_HEX)
                    .then(|| map.materialized_sha.clone()),
                pre_existing_sha: map.pre_existing_sha.clone(),
                swap_capability: map.swap_capability,
            })
            .collect();
    }
    if map.placement_state.len() != map.placements.len() {
        return Err(ClientError::Corrupt(
            "map.json placement_state is not 1:1 with placements".into(),
        ));
    }
    map.schema_version = PLACEMENT_MAP_SCHEMA_VERSION; // in-memory only; the next write re-emits v2
    Ok(Some(map))
}

/// Write `map.json` at its own current schema version (v2), enforcing the 1:1 placements ↔
/// `placement_state` invariant at the write boundary (a violation is a bug upstream, refused before
/// it becomes a durable lie).
///
/// # Errors
/// [`ClientError::Corrupt`] on a non-1:1 map; otherwise as [`write_doc`].
pub(crate) fn write_map(
    fs: &dyn FsOps,
    target: &Path,
    map: &PlacementMap,
) -> Result<(), ClientError> {
    if map.placement_state.len() != map.placements.len() {
        return Err(ClientError::Corrupt(
            "map.json placement_state is not 1:1 with placements".into(),
        ));
    }
    let mut out = map.clone();
    out.schema_version = PLACEMENT_MAP_SCHEMA_VERSION;
    write_doc(fs, target, &out)
}

/// Serialize a **SECRET** document (pretty + trailing newline — the committed on-disk shape) and write it
/// atomically at **0600** ([`atomic_write_private`]). Used for every sidecar doc that carries a secret
/// (`follows.json`'s read tokens, the enrollment WAL); ordinary, non-secret docs use [`write_doc`].
///
/// # Errors
/// [`ClientError::Corrupt`] if serialization fails; otherwise the [`FsOps`] failure.
pub(crate) fn write_doc_private<T: Serialize>(
    fs: &dyn FsOps,
    target: &Path,
    doc: &T,
) -> Result<(), ClientError> {
    let mut bytes = serde_json::to_vec_pretty(doc)
        .map_err(|e| ClientError::Corrupt(format!("serialize: {e}")))?;
    bytes.push(b'\n');
    atomic_write_private(fs, target, &bytes)
}

/// Read + parse a **SECRET** document, returning `None` if absent. **Fails closed on a permissive secret**:
/// a group/other-accessible file is refused via [`FsOps::private_perms_ok`] BEFORE any byte is parsed (like
/// the plane signer's seed read) — never trust a secret a wider audience could have written. Then the usual
/// fail-closed `schema_version` dispatch.
///
/// # Errors
/// [`ClientError::Corrupt`] if the secret is group/other-accessible; as [`load_versioned`] otherwise; plus
/// the [`FsOps`] read failure.
pub(crate) fn read_doc_private<T: DeserializeOwned>(
    fs: &dyn FsOps,
    path: &Path,
) -> Result<Option<T>, ClientError> {
    match fs.read_opt(path)? {
        None => Ok(None),
        Some(bytes) => {
            if !fs.private_perms_ok(path)? {
                return Err(ClientError::Corrupt(format!(
                    "{} is group/other-accessible; a secret must be 0600",
                    path.display()
                )));
            }
            Ok(Some(load_versioned::<T>(&bytes, PERSISTED_SCHEMA_VERSION)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use serde::Deserialize;

    use super::*;
    use crate::fs_seam::RealFs;

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Secret {
        schema_version: u32,
        token: String,
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-doc-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_then_read_doc_private_round_trips_at_0600() {
        let fs = RealFs;
        let dir = scratch("rt");
        let p = dir.join("follows.json");
        let s = Secret {
            schema_version: PERSISTED_SCHEMA_VERSION,
            token: "rt_secret".to_owned(),
        };
        write_doc_private(&fs, &p, &s).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let back: Secret = read_doc_private(&fs, &p).unwrap().unwrap();
        assert_eq!(back, s);
        // Absent → None (not an error).
        assert!(
            read_doc_private::<Secret>(&fs, &dir.join("nope.json"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn read_doc_private_refuses_a_group_or_other_readable_secret() {
        let fs = RealFs;
        let p = scratch("perm").join("follows.json");
        let s = Secret {
            schema_version: PERSISTED_SCHEMA_VERSION,
            token: "rt_secret".to_owned(),
        };
        // Write it as a NON-private 0644 doc, then demand it as a secret → refused BEFORE parsing.
        write_doc(&fs, &p, &s).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(matches!(
            read_doc_private::<Secret>(&fs, &p),
            Err(ClientError::Corrupt(_))
        ));
    }
}
