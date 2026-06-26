//! The host device identity (`identity/host.json`) — created on first use. Holds **no** private key
//! this increment (signing lands later); only the device id that authors local commits.

use serde::{Deserialize, Serialize};

use crate::atomic::atomic_write;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

#[derive(Debug, Serialize, Deserialize)]
struct HostIdentity {
    schema_version: u32,
    device_id: String,
}

/// Load the device id, minting + persisting a fresh `d_<hex>` token on first use. A present-but-corrupt
/// identity is a typed error (never silently regenerated — that would fork the author identity).
///
/// # Errors
/// [`ClientError::Corrupt`] if `host.json` exists but cannot be parsed; otherwise an io failure.
pub(crate) fn load_or_create_device_id(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<String, ClientError> {
    let path = layout.host_path();
    if let Some(bytes) = fs.read_opt(&path)? {
        let host: HostIdentity = serde_json::from_slice(&bytes)
            .map_err(|e| ClientError::Corrupt(format!("host identity: {e}")))?;
        return Ok(host.device_id);
    }
    let device_id = format!("d_{}", uuid::Uuid::new_v4().simple());
    fs.create_dir_all(&layout.identity_dir())?;
    let host = HostIdentity {
        schema_version: 1,
        device_id: device_id.clone(),
    };
    let mut bytes =
        serde_json::to_vec_pretty(&host).map_err(|e| ClientError::Corrupt(format!("{e}")))?;
    bytes.push(b'\n');
    atomic_write(fs, &path, &bytes)?;
    Ok(device_id)
}
