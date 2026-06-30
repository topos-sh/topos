//! The host device identity (`identity/host.json`) — created on first use. Carries the device id that
//! authors local commits **and** (once enrolled) a reference to the device signing key: the PUBLIC key + a
//! pointer to the sibling `0600` `device.key` seed file. The private seed is **never** in `host.json` — it
//! lives only in that `0600` sibling ([`crate::device_signer`]); `host.json` stays secret-free.

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
    /// The device signing-key reference (the PUBLIC key + a pointer to the sibling `0600` seed) — absent
    /// until enrollment records it; the private seed is NEVER here. Omitted from the JSON while absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_key: Option<DeviceKeyRef>,
}

/// A reference, from `host.json`, to the device's Ed25519 signing key: the non-secret PUBLIC key + a
/// pointer to the sibling `0600` seed file that holds the private key. `host.json` stays secret-free — the
/// raw seed is NEVER serialized here. (The `device_key_id` is DISTINCT from `device_id`: the former binds
/// the signed frames and is server-re-derivable from the public key; the latter is the commit-author token.)
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeviceKeyRef {
    /// The signature algorithm — `"Ed25519"`.
    pub alg: String,
    /// The id this device key is known by (the server-re-derivable `dk_<…>`; binds the signed frames).
    pub device_key_id: String,
    /// The raw Ed25519 public key as 64-char lowercase hex.
    pub public_key: String,
    /// The sibling `0600` seed file holding the PRIVATE key — `"device.key"`, NOT the seed bytes.
    pub private_key_ref: String,
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
        device_key: None,
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

/// Record the device's signing-key reference in `host.json` (read-modify-write under the identity lock,
/// crash-safe via [`atomic_write`]). The PRIVATE seed lives only in the sibling `0600` `device.key`;
/// `host.json` stays secret-free. Idempotent — re-recording the same ref rewrites identical bytes.
/// `host.json` must already exist (mint it first via [`load_or_create_device_id`]).
///
/// # Errors
/// [`ClientError::Corrupt`] if `host.json` is absent or unparseable; the schema-version errors for an
/// unsupported identity; otherwise an io failure.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn set_device_key(
    fs: &dyn FsOps,
    layout: &Layout,
    device_key: &DeviceKeyRef,
) -> Result<(), ClientError> {
    let _guard = fs.lock_exclusive(&layout.lock_file("identity"))?;
    let path = layout.host_path();
    let bytes = fs.read_opt(&path)?.ok_or_else(|| {
        ClientError::Corrupt(
            "host identity is absent; mint the device id before the device key".into(),
        )
    })?;
    let mut host: HostIdentity = load_versioned(&bytes, SCHEMA_VERSION)?;
    host.device_key = Some(device_key.clone());
    let mut out =
        serde_json::to_vec_pretty(&host).map_err(|e| ClientError::Corrupt(format!("{e}")))?;
    out.push(b'\n');
    atomic_write(fs, &path, &out)?;
    Ok(())
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

    fn sample_ref() -> DeviceKeyRef {
        DeviceKeyRef {
            alg: "Ed25519".to_owned(),
            device_key_id: "dk_56475aa75463474c0285df5dbf2bcab7".to_owned(),
            public_key: "ab".repeat(32),
            private_key_ref: "device.key".to_owned(),
        }
    }

    #[test]
    fn set_device_key_records_a_secret_free_reference() {
        let fs = RealFs;
        let layout = Layout::new(&scratch("setdk"));
        let device_id = load_or_create_device_id(&fs, &layout).unwrap();

        let dref = sample_ref();
        set_device_key(&fs, &layout, &dref).unwrap();

        // host.json round-trips the reference; the device id is unchanged.
        let bytes = fs.read_opt(&layout.host_path()).unwrap().unwrap();
        let host: HostIdentity = load_versioned(&bytes, SCHEMA_VERSION).unwrap();
        assert_eq!(host.device_id, device_id);
        assert_eq!(host.device_key.as_ref(), Some(&dref));
        // The device id is stable across a re-load — the device_key addition didn't fork identity.
        assert_eq!(load_or_create_device_id(&fs, &layout).unwrap(), device_id);
        // Idempotent: re-recording the same ref is fine.
        set_device_key(&fs, &layout, &dref).unwrap();

        // The serialized doc carries only the PUBLIC reference — the field name points at the sibling file,
        // never an inline seed.
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"private_key_ref\""));
        assert!(text.contains("\"device.key\""));
    }

    #[test]
    fn set_device_key_requires_an_existing_host_identity() {
        let fs = RealFs;
        let layout = Layout::new(&scratch("nohost"));
        assert!(matches!(
            set_device_key(&fs, &layout, &sample_ref()),
            Err(ClientError::Corrupt(_))
        ));
    }
}
