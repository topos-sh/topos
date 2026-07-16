//! The host device identity (`identity/host.json`) — created on first use. Carries the LOCAL device id
//! that authors local commits (`d_<hex>`, a controlled-ASCII token). This is a commit-author label, not
//! an authentication artifact: the device's bearer credential (and the server-registered device id it
//! belongs to) live in `identity/credentials.json` ([`crate::enroll`]).

use serde::{Deserialize, Serialize};
use topos_types::PERSISTED_SCHEMA_VERSION;

use crate::atomic::{atomic_write, load_versioned};
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

#[derive(Debug, Serialize, Deserialize)]
struct HostIdentity {
    schema_version: u32,
    device_id: String,
}

/// Load the device id, minting + persisting a fresh `d_<hex>` token on first use.
///
/// Serialized under an exclusive lock so two concurrent processes can never fork the identity, and the
/// minted value is re-read after the write so a racing winner's id is the one returned. A present
/// `host.json` is parsed **fail-closed** on its `schema_version` (an unknown/newer identity is an upgrade
/// error, never silently used).
///
/// # Errors
/// [`ClientError::UnknownSchemaVersion`] / [`ClientError::UnsupportedLegacy`] for an unsupported identity;
/// [`ClientError::Corrupt`] if it cannot be parsed; otherwise an io failure.
pub(crate) fn load_or_create_device_id(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<String, ClientError> {
    let _guard = fs.lock_exclusive(&layout.identity_lock_file())?;
    let path = layout.host_path();

    if let Some(bytes) = fs.read_opt(&path)? {
        let host: HostIdentity = load_versioned(&bytes, PERSISTED_SCHEMA_VERSION)?;
        return Ok(host.device_id);
    }

    let host = HostIdentity {
        schema_version: PERSISTED_SCHEMA_VERSION,
        device_id: format!("d_{}", uuid::Uuid::new_v4().simple()),
    };
    fs.create_dir_all(&layout.identity_dir())?;
    let mut bytes =
        serde_json::to_vec_pretty(&host).map_err(|e| ClientError::Corrupt(format!("{e}")))?;
    bytes.push(b'\n');
    atomic_write(fs, &path, &bytes)?;

    // Return the persisted value (defensive against any concurrent winner).
    let persisted: HostIdentity = load_versioned(
        &fs.read_opt(&path)?
            .ok_or_else(|| ClientError::Corrupt("host identity vanished after write".into()))?,
        PERSISTED_SCHEMA_VERSION,
    )?;
    Ok(persisted.device_id)
}

/// Read the LOCAL device id from `host.json` WITHOUT minting one — the read-only sibling of
/// [`load_or_create_device_id`] for surfaces that only DISPLAY the local author (e.g. `log`, mapping its
/// own device-authored versions to "you") and must never create identity as a side effect. `None` when
/// no host identity exists yet.
///
/// # Errors
/// As [`load_or_create_device_id`]'s read path (schema / parse / io), minus the mint.
pub(crate) fn read_device_id(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Option<String>, ClientError> {
    match fs.read_opt(&layout.host_path())? {
        Some(bytes) => {
            let host: HostIdentity = load_versioned(&bytes, PERSISTED_SCHEMA_VERSION)?;
            Ok(Some(host.device_id))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;

    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("topos-ident-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn device_id_is_minted_once_and_stable_across_reloads() {
        let fs = RealFs;
        let layout = Layout::new(&scratch("mint"));
        let device_id = load_or_create_device_id(&fs, &layout).unwrap();
        assert!(device_id.starts_with("d_"), "{device_id}");
        // Re-loading returns the SAME id (never a fork).
        assert_eq!(load_or_create_device_id(&fs, &layout).unwrap(), device_id);
        // A host.json that carries only the id + schema version (no key material of any kind).
        let text = String::from_utf8(fs.read_opt(&layout.host_path()).unwrap().unwrap()).unwrap();
        assert!(text.contains("device_id"));
        assert!(!text.contains("key"), "host.json is key-free: {text}");
    }

    #[test]
    fn a_stale_host_doc_with_extra_fields_still_loads_its_device_id() {
        // A pre-flip host.json carried a device-key reference block; serde ignores unknown fields, so
        // the id survives an upgrade without a migration.
        let fs = RealFs;
        let layout = Layout::new(&scratch("stale"));
        std::fs::create_dir_all(layout.identity_dir()).unwrap();
        let stale =
            br#"{"schema_version":1,"device_id":"d_stable","device_key":{"alg":"Ed25519"}}"#;
        std::fs::write(layout.host_path(), stale).unwrap();
        assert_eq!(load_or_create_device_id(&fs, &layout).unwrap(), "d_stable");
    }
}
