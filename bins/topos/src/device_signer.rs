//! The client device signer — the client's **only** private-key holder.
//!
//! Mirrors the plane's in-process signer: an Ed25519 signing key load-or-generated from a `0600` seed
//! file (`identity/device.key`), holding only the `SigningKey` (which self-zeroizes on drop via dalek's
//! `zeroize` feature), the raw public key, and a stable, pubkey-derived `device_key_id` — it **never**
//! retains the raw seed, and its `Debug` is hand-written so a key seed can never reach a log or panic.
//!
//! **Cross-component agreement.** `device_key_id` (`dk_` + the first 32 hex chars of
//! `sha256(public_key)`) is the ONE kernel derivation — `topos_core::sign::device_key_id` — which the
//! plane's server-side re-derivation also calls. The client SIGNS the enroll / governance / device-op
//! frames binding this id, and the plane re-derives the SAME id from the registered public key and
//! verifies; a second implementation could silently fork the halves, so neither side carries one.
//!
//! **One signer.** `topos-core` builds every signing preimage and verifies; the concrete `sign` is the
//! caller's — here — over the same `ed25519-dalek` crate. This file never re-implements a preimage: it
//! signs exactly the bytes `topos_core::sign` framed.

use std::path::Path;

use ed25519_dalek::{Signer, SigningKey};
use zeroize::Zeroizing;

use topos_core::sign::{
    DeviceOpFields, EnrollFields, GovernanceOpFields, PreimageError, device_op_preimage,
    enroll_preimage, governance_op_preimage,
};

use crate::atomic::atomic_write_private;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// The Ed25519 seed length (the dalek `SecretKey` is `[u8; 32]`).
const SEED_LEN: usize = 32;

/// The client's device signer. Holds the Ed25519 signing key, the raw public key, and the stable
/// pubkey-derived `device_key_id`; signs the enroll / governance / device-op frames `topos-core` frames.
pub(crate) struct DeviceSigner {
    /// The signing key (self-zeroizes on drop — dalek's `zeroize` feature is enabled for this crate).
    signing_key: SigningKey,
    /// The raw 32-byte public key (the verify credential the plane registers + pins).
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
        let device_key_id = topos_core::sign::device_key_id(&public_key);
        Self {
            signing_key,
            public_key,
            device_key_id,
        }
    }

    /// The raw 32-byte device public key (what the plane registers and a frame verifies against).
    pub(crate) fn public_key(&self) -> [u8; 32] {
        self.public_key
    }

    /// The stable device key id (`dk_<…>`), bound into every signed frame; the plane re-derives + matches it.
    pub(crate) fn device_key_id(&self) -> &str {
        &self.device_key_id
    }

    /// Sign a device-enrollment possession proof: the signed message is **exactly** the bytes
    /// `verify_enroll` reconstructs — `topos_core::sign::enroll_preimage`. Deterministic (no RNG).
    ///
    /// # Errors
    /// [`ClientError::Corrupt`] if the preimage cannot be framed (unreachable for well-formed inputs).
    pub(crate) fn sign_enroll(&self, fields: &EnrollFields) -> Result<[u8; 64], ClientError> {
        let preimage = enroll_preimage(fields).map_err(preimage_err)?;
        Ok(self.signing_key.sign(&preimage).to_bytes())
    }

    /// Sign a governance op (invite / roster mutation / device revoke) over
    /// `topos_core::sign::governance_op_preimage`. Deterministic (no RNG).
    ///
    /// # Errors
    /// [`ClientError::Corrupt`] if the preimage cannot be framed (unreachable for well-formed inputs).
    pub(crate) fn sign_governance(
        &self,
        fields: &GovernanceOpFields,
    ) -> Result<[u8; 64], ClientError> {
        let preimage = governance_op_preimage(fields).map_err(preimage_err)?;
        Ok(self.signing_key.sign(&preimage).to_bytes())
    }

    /// Sign a device op (publish / revert / review) over `topos_core::sign::device_op_preimage` — the
    /// contribute verbs' signature. Deterministic (no RNG).
    ///
    /// # Errors
    /// [`ClientError::Corrupt`] if the preimage cannot be framed (unreachable for well-formed inputs).
    pub(crate) fn sign_device_op(&self, fields: &DeviceOpFields) -> Result<[u8; 64], ClientError> {
        let preimage = device_op_preimage(fields).map_err(preimage_err)?;
        Ok(self.signing_key.sign(&preimage).to_bytes())
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

/// Map a `topos-core` preimage-framing failure to the client error family. Every case is unreachable for
/// well-formed inputs (a > 4 GiB string, or an `epoch`/`seq` past 2^53), so this is an internal-fault path.
fn preimage_err(e: PreimageError) -> ClientError {
    ClientError::Corrupt(format!(
        "could not build the device signing preimage: {e:?}"
    ))
}

/// Load (or, on first run, generate + persist `0600`) the raw 32-byte device seed, serialized under the
/// identity lock so two first-runs converge on one key (the `FsOps` seam has no `O_EXCL` create, so the
/// lock + a re-read is how we mirror the plane signer's race-safety). Returned in a [`Zeroizing`] buffer.
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
/// group/other (refuse-on-permissive, like the plane signer's seed read) or not exactly 32 bytes — a
/// tampered/world-readable device key is refused, never silently used.
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
    use topos_core::sign::{
        DeviceOp, GovernanceOpKind, verify_device_op, verify_enroll, verify_governance_op,
    };

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
        // The kernel's frozen seed 00..1f → KERNEL_DEVICE_PK, so this signer's public key is a known answer.
        let seed: [u8; SEED_LEN] = std::array::from_fn(|i| i as u8);
        let signer = DeviceSigner::from_seed(&seed);
        assert_eq!(
            topos_core::digest::to_hex(&signer.public_key()),
            KERNEL_DEVICE_PK
        );

        // (1) Recompute the id from the formula, independent of the signer's own derivation path.
        let hex = topos_core::digest::to_hex(&topos_core::digest::sha256(&signer.public_key()));
        assert_eq!(signer.device_key_id(), format!("dk_{}", &hex[..32]));
        assert!(signer.device_key_id().starts_with("dk_"));
        assert_eq!(signer.device_key_id().len(), 3 + 32);

        // (2) Pin the exact server-side known-answer (`dk_` + first 32 hex of sha256(KERNEL_DEVICE_PK)),
        // computed independently — the SAME value the plane's `device_key_id_for` produces.
        assert_eq!(
            signer.device_key_id(),
            "dk_56475aa75463474c0285df5dbf2bcab7"
        );
    }

    #[test]
    fn sign_enroll_round_trips_through_the_kernel_verify() {
        let signer = DeviceSigner::from_seed(&[7u8; SEED_LEN]);
        let fields = EnrollFields {
            workspace_id: "w_acme",
            grant_hash: [0x33; 32],
            device_auth_id: "da_acme_001",
            device_key_id: signer.device_key_id(),
            device_public_key: signer.public_key(),
            offered_skill_ids: &["s_deploy", "s_prdescribe"],
        };
        let sig = signer.sign_enroll(&fields).unwrap();
        assert!(verify_enroll(&fields, &sig, &signer.public_key()));
        // A one-bit flip in the signature fails verification (the kernel is the integrity authority).
        let mut tampered = sig;
        tampered[0] ^= 0x01;
        assert!(!verify_enroll(&fields, &tampered, &signer.public_key()));
    }

    #[test]
    fn sign_governance_round_trips_through_the_kernel_verify() {
        let signer = DeviceSigner::from_seed(&[9u8; SEED_LEN]);
        let fields = GovernanceOpFields {
            workspace_id: "w_acme",
            op_id: [0xAB; 16],
            device_key_id: signer.device_key_id(),
            op: GovernanceOpKind::RosterRemove { target: "p_bob" },
        };
        let sig = signer.sign_governance(&fields).unwrap();
        assert!(verify_governance_op(&fields, &sig, &signer.public_key()));
    }

    #[test]
    fn sign_device_op_round_trips_through_the_kernel_verify() {
        // Exercises the device-op signer (so it is not dead code) AND proves client/kernel agree on bytes.
        let signer = DeviceSigner::from_seed(&[11u8; SEED_LEN]);
        let fields = DeviceOpFields {
            workspace_id: "w_acme",
            skill_id: "s_deploy",
            op: DeviceOp::PublishDirect,
            op_id: [0x01; 16],
            device_key_id: signer.device_key_id(),
            expected_epoch: 1,
            expected_seq: 1,
            commit_id: [0x22; 32],
            bundle_digest: [0x33; 32],
        };
        let sig = signer.sign_device_op(&fields).unwrap();
        assert!(verify_device_op(&fields, &sig, &signer.public_key()));
    }
}
