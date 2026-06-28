//! The in-process plane signer — this crate is the **only** private-key holder.
//!
//! `topos-core` is verify-only (`no_std`, no key material). The plane's Ed25519 signing key lives here,
//! behind the privacy boundary, and signs the `current` pointer **inside** the pointer-move transaction
//! — a pure in-memory, RFC-8032-**deterministic**, RNG-free `sign` call (no I/O, no clock). The key is
//! **load-or-generated** from a `0600` file at open time; **at-rest encryption / KMS is the named next
//! step**, not built — the v0 posture is a plaintext seed in the server's confined, owner-only directory.
//!
//! The signer holds only the `SigningKey` (which self-zeroizes on drop via dalek's `zeroize` feature),
//! the derived `key_id`, and the public key; it **never** retains the raw seed, and its `Debug` is
//! hand-written to print only the `key_id` — so a key seed can never reach a log or panic message.

use std::path::Path;

use ed25519_dalek::{Signer, SigningKey};
use topos_core::sign::{self, CurrentPointer};
use zeroize::Zeroizing;

use crate::error::{AuthorityError, Result};

/// The Ed25519 seed length (the dalek `SecretKey` is `[u8; 32]`).
const SEED_LEN: usize = 32;

/// A typed signer fault, wrapped into [`AuthorityError::Internal`] so no key bytes or low-level type
/// crosses the public boundary.
#[derive(Debug, thiserror::Error)]
enum SignerError {
    #[error("plane pointer preimage rejected (generation out of range)")]
    Preimage,
    #[error("plane key file: {0}")]
    KeyFile(&'static str),
    #[error("could not gather OS entropy for the plane key")]
    Entropy,
}

/// The in-process plane signer. Holds the Ed25519 signing key; signs `current` pointers; exports the
/// public key + a stable `key_id` so a follower can pin the plane key out-of-band.
pub(crate) struct PlaneSigner {
    /// The signing key (self-zeroizes on drop — dalek's `zeroize` feature is enabled for this crate).
    key: SigningKey,
    /// The raw 32-byte public key (the verify credential a follower TOFU-pins).
    public_key: [u8; 32],
    /// A stable, public selector derived from the public key (never the secret).
    key_id: String,
}

impl PlaneSigner {
    /// Load the plane key from `path`, or — on first run — generate a fresh one and persist it `0600`.
    ///
    /// A concurrent process that wins the create race is handled by falling back to a load, so two plane
    /// processes starting together converge on one key. Loading **re-validates** the file (exactly 32
    /// bytes, owner-only) and fails typed rather than deriving a bogus key from a truncated/garbage seed.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if the key file cannot be read/created/validated or OS entropy is
    /// unavailable on first generation.
    pub(crate) fn load_or_generate(path: &Path) -> Result<Self> {
        if let Some(seed) = read_seed(path)? {
            return Ok(Self::from_seed(&seed));
        }
        // Absent — generate and persist atomically (O_EXCL). A racing process may create it first.
        let fresh = generate_seed()?;
        match create_seed_file(path, &fresh) {
            CreateOutcome::Created => Ok(Self::from_seed(&fresh)),
            CreateOutcome::Raced => {
                let seed = read_seed(path)?.ok_or_else(|| {
                    AuthorityError::internal(SignerError::KeyFile("vanished after a create race"))
                })?;
                Ok(Self::from_seed(&seed))
            }
            CreateOutcome::Failed(reason) => {
                Err(AuthorityError::internal(SignerError::KeyFile(reason)))
            }
        }
    }

    /// Construct from a raw seed (used by `load_or_generate` and the tests). The seed is consumed into the
    /// `SigningKey` and not retained — the source buffer is the caller's `Zeroizing`.
    fn from_seed(seed: &[u8; SEED_LEN]) -> Self {
        let key = SigningKey::from_bytes(seed);
        let public_key = key.verifying_key().to_bytes();
        let key_id = derive_key_id(&public_key);
        Self {
            key,
            public_key,
            key_id,
        }
    }

    /// Sign a `current` pointer: the signed message is **exactly** the bytes `verify_pointer` reconstructs
    /// — `topos_core::sign::pointer_preimage` (the RFC-8785 JCS string), UTF-8. The signature is
    /// deterministic (no RNG). The generation bound is the caller's precheck; an out-of-range pointer here
    /// is an internal fault that aborts the txn (it could never have been signed).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if the pointer's generation exceeds the JCS safe-integer bound.
    pub(crate) fn sign_pointer(&self, pointer: &CurrentPointer) -> Result<[u8; 64]> {
        let preimage = sign::pointer_preimage(pointer)
            .map_err(|_| AuthorityError::internal(SignerError::Preimage))?;
        Ok(self.key.sign(preimage.as_bytes()).to_bytes())
    }

    /// The raw 32-byte plane public key (for client pinning + the later bootstrap; verify in-crate).
    pub(crate) fn public_key(&self) -> [u8; 32] {
        self.public_key
    }

    /// The stable plane key id (goes in `SignedCurrentRecord.signature.key_id` + `Receipt.key_id`).
    pub(crate) fn key_id(&self) -> &str {
        &self.key_id
    }
}

/// Redacting `Debug` — prints only the public `key_id`, never key material (the crate lints
/// `missing_debug_implementations`, so a derived `Debug` would be required; this one is safe).
impl std::fmt::Debug for PlaneSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaneSigner")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// Derive the stable, public `key_id` from the public key: `plane_<first 16 hex of sha256(pubkey)>`. A
/// short, pubkey-derived selector — it does not leak the key and is stable across restarts (derived from
/// the persisted key, never random).
fn derive_key_id(public_key: &[u8; 32]) -> String {
    let hex = topos_core::digest::to_hex(&topos_core::digest::sha256(public_key));
    format!("plane_{}", &hex[..16])
}

/// Fill a 32-byte seed from the OS CSPRNG.
fn generate_seed() -> Result<Zeroizing<[u8; SEED_LEN]>> {
    let mut seed = Zeroizing::new([0u8; SEED_LEN]);
    getrandom::getrandom(seed.as_mut_slice())
        .map_err(|_| AuthorityError::internal(SignerError::Entropy))?;
    Ok(seed)
}

/// Read + validate the seed at `path`: `Ok(None)` if absent; a typed fault if present but not exactly 32
/// bytes or readable by group/other (a tampered/world-readable plane key is refused, never silently used).
fn read_seed(path: &Path) -> Result<Option<Zeroizing<[u8; SEED_LEN]>>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => Zeroizing::new(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(AuthorityError::internal(SignerError::KeyFile("unreadable"))),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)
            .map_err(|_| AuthorityError::internal(SignerError::KeyFile("unreadable")))?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(AuthorityError::internal(SignerError::KeyFile(
                "is group/other-accessible; must be 0600",
            )));
        }
    }
    let seed: [u8; SEED_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| AuthorityError::internal(SignerError::KeyFile("must be exactly 32 bytes")))?;
    Ok(Some(Zeroizing::new(seed)))
}

/// The outcome of an exclusive create.
enum CreateOutcome {
    Created,
    Raced,
    Failed(&'static str),
}

/// Create the key file **exclusively** (`O_EXCL`) with mode `0600` and write the seed, so the seed is
/// owner-only from creation (no write-then-chmod window). `Raced` if another process created it first.
fn create_seed_file(path: &Path, seed: &[u8; SEED_LEN]) -> CreateOutcome {
    use std::io::Write;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return CreateOutcome::Raced,
        Err(_) => return CreateOutcome::Failed("could not create (0600)"),
    };
    if file.write_all(seed).is_err() {
        return CreateOutcome::Failed("could not write the seed");
    }
    if file.sync_all().is_err() {
        return CreateOutcome::Failed("could not fsync the seed");
    }
    CreateOutcome::Created
}

#[cfg(test)]
mod tests {
    use super::*;
    use topos_core::sign::verify_pointer;

    fn pointer<'a>(
        ws: &'a str,
        skill: &'a str,
        vid: [u8; 32],
        epoch: u64,
        seq: u64,
    ) -> CurrentPointer<'a> {
        CurrentPointer {
            workspace_id: ws,
            skill_id: skill,
            version_id: vid,
            epoch,
            seq,
        }
    }

    #[test]
    fn sign_pointer_round_trips_through_verify_pointer() {
        let signer = PlaneSigner::from_seed(&[7u8; SEED_LEN]);
        let vid = [0xABu8; 32];
        let ptr = pointer("w_acme", "s_deploy", vid, 1, 42);
        let sig = signer.sign_pointer(&ptr).unwrap();
        assert!(verify_pointer(&ptr, &sig, &signer.public_key()));
    }

    #[test]
    fn a_one_bit_flip_fails_verification() {
        let signer = PlaneSigner::from_seed(&[9u8; SEED_LEN]);
        let ptr = pointer("w_acme", "s_deploy", [0x11u8; 32], 1, 1);
        let mut sig = signer.sign_pointer(&ptr).unwrap();
        sig[0] ^= 0x01;
        assert!(!verify_pointer(&ptr, &sig, &signer.public_key()));
    }

    #[test]
    fn a_pointer_does_not_verify_under_a_different_scope() {
        // The preimage binds workspace_id + skill_id, so a valid signature cannot be replayed into
        // another skill or workspace.
        let signer = PlaneSigner::from_seed(&[3u8; SEED_LEN]);
        let vid = [0x55u8; 32];
        let sig = signer
            .sign_pointer(&pointer("w_acme", "s_deploy", vid, 2, 5))
            .unwrap();
        assert!(verify_pointer(
            &pointer("w_acme", "s_deploy", vid, 2, 5),
            &sig,
            &signer.public_key()
        ));
        assert!(!verify_pointer(
            &pointer("w_acme", "s_OTHER", vid, 2, 5),
            &sig,
            &signer.public_key()
        ));
        assert!(!verify_pointer(
            &pointer("w_OTHER", "s_deploy", vid, 2, 5),
            &sig,
            &signer.public_key()
        ));
    }

    #[test]
    fn signing_is_deterministic() {
        let signer = PlaneSigner::from_seed(&[1u8; SEED_LEN]);
        let ptr = pointer("w", "s", [0u8; 32], 1, 1);
        assert_eq!(
            signer.sign_pointer(&ptr).unwrap(),
            signer.sign_pointer(&ptr).unwrap()
        );
    }

    #[test]
    fn key_id_is_stable_and_pubkey_derived() {
        let a = PlaneSigner::from_seed(&[42u8; SEED_LEN]);
        let b = PlaneSigner::from_seed(&[42u8; SEED_LEN]);
        assert_eq!(a.key_id(), b.key_id());
        assert!(a.key_id().starts_with("plane_"));
        let different = PlaneSigner::from_seed(&[43u8; SEED_LEN]);
        assert_ne!(a.key_id(), different.key_id());
    }

    #[test]
    fn debug_never_prints_key_material() {
        let signer = PlaneSigner::from_seed(&[0xCDu8; SEED_LEN]);
        let shown = format!("{signer:?}");
        assert!(shown.contains("key_id"));
        assert!(shown.contains("plane_"));
        // No seed/secret bytes leak.
        assert!(!shown.contains("205")); // 0xCD = 205 decimal, were the seed ever Debug-printed
    }

    #[test]
    fn load_or_generate_persists_then_reloads_the_same_key() {
        let dir = tempdir();
        let path = dir.join("plane.key");
        let first = PlaneSigner::load_or_generate(&path).unwrap();
        let second = PlaneSigner::load_or_generate(&path).unwrap();
        // Reload yields the SAME key (persisted, not regenerated).
        assert_eq!(first.public_key(), second.public_key());
        assert_eq!(first.key_id(), second.key_id());
        // And the persisted file is exactly the 32-byte seed, owner-only.
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), SEED_LEN);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_wrong_length_key_file_is_refused() {
        let dir = tempdir();
        let path = dir.join("plane.key");
        std::fs::write(&path, [0u8; 16]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(PlaneSigner::load_or_generate(&path).is_err());
    }

    /// A throwaway directory under the OS temp dir (no `tempfile` dev-dep in this crate).
    fn tempdir() -> std::path::PathBuf {
        // A unique-enough name from the address of a stack local + the thread id; this crate forbids the
        // ambient clock/RNG helpers, and a collision only weakens isolation between concurrent runs.
        let probe = 0u8;
        let uniq = format!(
            "topos-signer-{:x}-{:?}",
            std::ptr::addr_of!(probe) as usize,
            std::thread::current().id()
        );
        let dir = std::env::temp_dir().join(uniq);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
