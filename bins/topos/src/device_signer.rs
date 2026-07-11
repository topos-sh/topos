//! The client device keypair — the device's presented identity.
//!
//! An Ed25519 keypair load-or-generated from a `0600` seed file (`identity/device.key`), holding only the
//! `SigningKey` (which self-zeroizes on drop via dalek's `zeroize` feature), the raw public key, and a
//! stable, pubkey-derived `device_key_id`. It **never** retains the raw seed, and its `Debug` is
//! hand-written so a key seed can never reach a log or panic.
//!
//! **What the keypair is for.** The public key REGISTERS the device with the plane at enroll; the
//! `device_key_id` (`dk_…`, derived from that public key) is the device's presented identity on every
//! request. **Nothing signs with the private key** — the trust model is git/GitHub-level (the client
//! trusts the plane it enrolled with; authority is the plane's database policy rows, integrity is the
//! content-addressed `version_id` re-verified by digest on apply). The private key sits unused; a later
//! increment owns the per-workspace credential redesign.
//!
//! **Cross-component agreement.** `device_key_id` (`dk_` + the first 32 hex chars of `sha256(public_key)`)
//! is the ONE kernel derivation — `topos_core::identity::device_key_id` — which the plane's server-side
//! re-derivation also calls, so the id the client presents is the SAME id the plane re-derives from the
//! registered public key.

use std::path::Path;

use ed25519_dalek::SigningKey;
use zeroize::Zeroizing;

use crate::atomic::atomic_write_private;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// The Ed25519 seed length (the dalek `SecretKey` is `[u8; 32]`).
const SEED_LEN: usize = 32;

/// The client's device keypair. Holds the Ed25519 signing key (custody only — nothing signs), the raw
/// public key (the plane's registration credential), and the stable pubkey-derived `device_key_id` (the
/// device's presented identity).
pub(crate) struct DeviceSigner {
    /// The signing key (self-zeroizes on drop — dalek's `zeroize` feature is enabled for this crate). Held
    /// for custody only; the client no longer signs anything with it.
    #[allow(dead_code)]
    signing_key: SigningKey,
    /// The raw 32-byte public key (the credential the plane registers).
    public_key: [u8; 32],
    /// The stable, public selector derived from the public key (byte-identical to the server derivation).
    device_key_id: String,
}

impl DeviceSigner {
    /// Load the device key from `identity/device.key`, or — on first run — generate a fresh one and persist
    /// it `0600`. Serialized under the identity lock (the same lock `load_or_create_device_id` uses), so two
    /// concurrent first-runs converge on one key rather than forking; a present file is re-validated
    /// (exactly 32 bytes, owner-only) and refused — never silently regenerated — if it is permissive or the
    /// wrong length.
    ///
    /// # Errors
    /// [`ClientError::Corrupt`] if the key file is group/other-accessible or not exactly 32 bytes;
    /// [`ClientError::Io`] if OS entropy is unavailable on first generation; otherwise an io failure.
    pub(crate) fn load_or_generate(
        fs: &dyn FsOps,
        layout: &Layout,
    ) -> Result<DeviceSigner, ClientError> {
        let seed = load_or_generate_seed(fs, layout)?;
        Ok(Self::from_seed(&seed))
    }

    /// Construct from a raw seed (used by `load_or_generate` and the tests). The seed is consumed into the
    /// `SigningKey` and not retained — the source buffer is the caller's `Zeroizing`.
    fn from_seed(seed: &[u8; SEED_LEN]) -> Self {
        let signing_key = SigningKey::from_bytes(seed);
        let public_key = signing_key.verifying_key().to_bytes();
        // The ONE kernel derivation (`dk_` + first 32 hex of sha256(pubkey)) — the plane re-derives the
        // SAME id from the registered key via the same fn, so the halves cannot drift.
        let device_key_id = topos_core::identity::device_key_id(&public_key);
        Self {
            signing_key,
            public_key,
            device_key_id,
        }
    }

    /// The raw 32-byte device public key (what the plane registers).
    pub(crate) fn public_key(&self) -> [u8; 32] {
        self.public_key
    }

    /// The stable device key id (`dk_<…>`) presented on requests; the plane re-derives + matches it.
    pub(crate) fn device_key_id(&self) -> &str {
        &self.device_key_id
    }
}

/// Redacting `Debug` — prints the public `device_key_id` + public key, never the key material (the crate
/// lints `missing_debug_implementations`, so a `Debug` is required; this one is safe).
impl std::fmt::Debug for DeviceSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceSigner")
            .field("device_key_id", &self.device_key_id)
            .field("public_key", &topos_core::digest::to_hex(&self.public_key))
            .field("signing_key", &"<redacted>")
            .finish()
    }
}

/// Load (or, on first run, generate + persist `0600`) the raw 32-byte device seed, serialized under the
/// identity lock so two first-runs converge on one key (the `FsOps` seam has no `O_EXCL` create, so the
/// lock + a re-read is how we mirror the race-safety). Returned in a [`Zeroizing`] buffer.
fn load_or_generate_seed(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<Zeroizing<[u8; SEED_LEN]>, ClientError> {
    let _guard = fs.lock_exclusive(&layout.identity_lock_file())?;
    let path = layout.device_key_path();

    if let Some(seed) = read_seed(fs, &path)? {
        return Ok(seed);
    }
    // Absent — generate, persist `0600`, then re-read under the lock (defensive parity with
    // `load_or_create_device_id`; the lock makes a concurrent winner impossible, the re-read is belt-and-braces).
    let fresh = generate_seed()?;
    fs.create_dir_all(&layout.identity_dir())?;
    atomic_write_private(fs, &path, fresh.as_slice())?;
    read_seed(fs, &path)?
        .ok_or_else(|| ClientError::Corrupt("device key vanished after write".into()))
}

/// Fill a 32-byte seed from the OS CSPRNG.
fn generate_seed() -> Result<Zeroizing<[u8; SEED_LEN]>, ClientError> {
    let mut seed = Zeroizing::new([0u8; SEED_LEN]);
    getrandom::getrandom(seed.as_mut_slice())
        .map_err(|_| ClientError::Io("could not gather OS entropy for the device key".into()))?;
    Ok(seed)
}

/// Read + validate the seed at `path`: `Ok(None)` if absent; a typed fault if present but readable by
/// group/other (refuse-on-permissive) or not exactly 32 bytes — a tampered/world-readable device key is
/// refused, never silently used.
fn read_seed(
    fs: &dyn FsOps,
    path: &Path,
) -> Result<Option<Zeroizing<[u8; SEED_LEN]>>, ClientError> {
    let Some(bytes) = fs.read_opt(path)? else {
        return Ok(None);
    };
    let bytes = Zeroizing::new(bytes);
    if !fs.private_perms_ok(path)? {
        return Err(ClientError::Corrupt(
            "device.key is group/other-accessible; must be 0600".into(),
        ));
    }
    let seed: [u8; SEED_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| ClientError::Corrupt("device.key must be exactly 32 bytes".into()))?;
    Ok(Some(Zeroizing::new(seed)))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::fs_seam::RealFs;

    /// The kernel's frozen device-key known-answer (seed = bytes 00..1f → this public key).
    const KERNEL_DEVICE_PK: &str =
        "03a107bff3ce10be1d70dd18e74bc09967e4d6309ba50d5f1ddc8664125531b8";

    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-sig-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_or_generate_is_idempotent_and_persists_0600() {
        let fs = RealFs;
        let layout = Layout::new(&scratch("idem"));
        let first = DeviceSigner::load_or_generate(&fs, &layout).unwrap();
        let second = DeviceSigner::load_or_generate(&fs, &layout).unwrap();
        // A reload yields the SAME key (persisted, not regenerated).
        assert_eq!(first.public_key(), second.public_key());
        assert_eq!(first.device_key_id(), second.device_key_id());
        // The persisted seed is exactly 32 bytes, owner-only.
        let path = layout.device_key_path();
        assert_eq!(std::fs::read(&path).unwrap().len(), SEED_LEN);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_group_or_other_readable_seed_is_refused() {
        let fs = RealFs;
        let layout = Layout::new(&scratch("perm"));
        fs.create_dir_all(&layout.identity_dir()).unwrap();
        // A 32-byte seed at 0644 (group/other-readable) — refused, never used.
        fs.write_staged(&layout.device_key_path(), &[0u8; SEED_LEN], false)
            .unwrap();
        assert!(matches!(
            DeviceSigner::load_or_generate(&fs, &layout),
            Err(ClientError::Corrupt(_))
        ));
    }

    #[test]
    fn debug_never_prints_key_material() {
        let signer = DeviceSigner::from_seed(&[0xCD; SEED_LEN]);
        let shown = format!("{signer:?}");
        assert!(shown.contains("device_key_id"));
        assert!(shown.contains("dk_"));
        assert!(shown.contains("<redacted>"));
        // No seed bytes leak — neither the raw seed hex nor its decimal byte value.
        assert!(!shown.contains(&"cd".repeat(SEED_LEN)));
        assert!(!shown.contains("205")); // 0xCD = 205, were the seed ever Debug-printed
    }

    #[test]
    fn device_key_id_matches_the_server_derivation() {
        // The kernel's frozen seed 00..1f → KERNEL_DEVICE_PK, so this keypair's public key is a known answer.
        let seed: [u8; SEED_LEN] = std::array::from_fn(|i| i as u8);
        let signer = DeviceSigner::from_seed(&seed);
        assert_eq!(
            topos_core::digest::to_hex(&signer.public_key()),
            KERNEL_DEVICE_PK
        );

        // (1) Recompute the id from the formula, independent of the keypair's own derivation path.
        let hex = topos_core::digest::to_hex(&topos_core::digest::sha256(&signer.public_key()));
        assert_eq!(signer.device_key_id(), format!("dk_{}", &hex[..32]));
        assert!(signer.device_key_id().starts_with("dk_"));
        assert_eq!(signer.device_key_id().len(), 3 + 32);

        // (2) Pin the exact server-side known-answer (`dk_` + first 32 hex of sha256(KERNEL_DEVICE_PK)),
        // computed independently — the SAME value the plane's `device_key_id` produces.
        assert_eq!(
            signer.device_key_id(),
            "dk_56475aa75463474c0285df5dbf2bcab7"
        );
    }
}
