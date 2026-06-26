//! The host device identity (`identity/host.json`) — created on first use. Holds **no** private key
//! this increment (signing lands later); only the device id that authors local commits.

use serde::{Deserialize, Serialize};
use topos_types::SCHEMA_VERSION;

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
    let _guard = fs.lock_exclusive(&layout.lock_file("identity"))?;
    let path = layout.host_path();

    if let Some(bytes) = fs.read_opt(&path)? {
        let host: HostIdentity = load_versioned(&bytes, SCHEMA_VERSION)?;
        return Ok(host.device_id);
    }

    let host = HostIdentity {
        schema_version: SCHEMA_VERSION,
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
        SCHEMA_VERSION,
    )?;
    Ok(persisted.device_id)
}
