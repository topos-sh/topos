//! The `0600` secret-seed custody — load-or-generate a raw 32-byte server secret.
//!
//! One custody pattern for the plane's file-backed secrets (today: the enrollment HMAC secret): a present
//! file is re-validated (exactly 32 bytes, owner-only) and a fresh one is published atomically (`O_EXCL`
//! temp + hard-link), so two processes racing on first boot converge on one secret. **At-rest encryption /
//! KMS is the named next step**, not built — the v0 posture is a plaintext seed in the server's confined,
//! owner-only directory. The seed only ever travels in a [`Zeroizing`] buffer, and no error ever carries
//! key bytes.

use std::path::Path;

use zeroize::Zeroizing;

use crate::error::{AuthorityError, Result};

/// The secret seed length (raw 32 bytes).
pub(crate) const SEED_LEN: usize = 32;

/// A typed custody fault, wrapped into [`AuthorityError::Internal`] so no key bytes or low-level type
/// crosses the public boundary.
#[derive(Debug, thiserror::Error)]
enum SecretError {
    #[error("secret seed file: {0}")]
    KeyFile(&'static str),
    #[error("could not gather OS entropy for the secret seed")]
    Entropy,
}

/// Load (or, on first run, generate + persist `0600`) a raw 32-byte secret seed from `path`. A present
/// file is re-validated (exactly 32 bytes, owner-only) and a fresh one is published atomically (`O_EXCL`
/// temp + hard-link), so two processes racing converge on one secret. The seed is returned in a
/// [`Zeroizing`] buffer.
///
/// # Errors
/// [`AuthorityError::Internal`] if the file cannot be read/created/validated or OS entropy is unavailable.
pub(crate) fn load_or_generate_seed(path: &Path) -> Result<Zeroizing<[u8; SEED_LEN]>> {
    if let Some(seed) = read_seed(path)? {
        return Ok(seed);
    }
    // Absent — generate and persist atomically (O_EXCL). A racing process may create it first.
    let fresh = generate_seed()?;
    match create_seed_file(path, &fresh) {
        CreateOutcome::Created => Ok(fresh),
        CreateOutcome::Raced => read_seed(path)?.ok_or_else(|| {
            AuthorityError::internal(SecretError::KeyFile("vanished after a create race"))
        }),
        CreateOutcome::Failed(reason) => {
            Err(AuthorityError::internal(SecretError::KeyFile(reason)))
        }
    }
}

/// Fill a 32-byte seed from the OS CSPRNG.
fn generate_seed() -> Result<Zeroizing<[u8; SEED_LEN]>> {
    let mut seed = Zeroizing::new([0u8; SEED_LEN]);
    getrandom::getrandom(seed.as_mut_slice())
        .map_err(|_| AuthorityError::internal(SecretError::Entropy))?;
    Ok(seed)
}

/// Read + validate the seed at `path`: `Ok(None)` if absent; a typed fault if present but not exactly 32
/// bytes or readable by group/other (a tampered/world-readable secret is refused, never silently used).
fn read_seed(path: &Path) -> Result<Option<Zeroizing<[u8; SEED_LEN]>>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => Zeroizing::new(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(AuthorityError::internal(SecretError::KeyFile("unreadable"))),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)
            .map_err(|_| AuthorityError::internal(SecretError::KeyFile("unreadable")))?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(AuthorityError::internal(SecretError::KeyFile(
                "is group/other-accessible; must be 0600",
            )));
        }
    }
    let seed: [u8; SEED_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| AuthorityError::internal(SecretError::KeyFile("must be exactly 32 bytes")))?;
    Ok(Some(Zeroizing::new(seed)))
}

/// The outcome of an exclusive create.
enum CreateOutcome {
    Created,
    Raced,
    Failed(&'static str),
}

/// Publish the secret file **atomically**: write the seed to a unique sibling temp (`O_EXCL`, mode `0600`),
/// fully `fsync` it, then **hard-link** it onto the real path. The link is create-only (fails if the path
/// already exists) **and** atomic, so the secret path is never observable at a partial length — a crash
/// mid-write leaves only an orphan temp, never a 0-byte file that would wedge the next startup with a
/// "must be exactly 32 bytes" error. A lost create race surfaces as `Raced` (the loser loads the winner's
/// seed — no split brain), exactly as a plain `O_EXCL` create would.
fn create_seed_file(path: &Path, seed: &[u8; SEED_LEN]) -> CreateOutcome {
    use std::io::Write;

    // A unique temp name in the same directory (so the hard-link stays on one filesystem).
    let mut nonce = [0u8; 8];
    if getrandom::getrandom(&mut nonce).is_err() {
        return CreateOutcome::Failed("could not gather entropy for the temp secret file");
    }
    let temp = path.with_extension(format!("tmp.{}", topos_core::digest::to_hex(&nonce)));

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| -> std::result::Result<CreateOutcome, &'static str> {
        let mut file = opts
            .open(&temp)
            .map_err(|_| "could not create the temp secret file (0600)")?;
        file.write_all(seed)
            .map_err(|_| "could not write the seed")?;
        file.sync_all().map_err(|_| "could not fsync the seed")?;
        drop(file);
        // Atomic, create-only publish: the link succeeds iff `path` does not already exist.
        match std::fs::hard_link(&temp, path) {
            Ok(()) => Ok(CreateOutcome::Created),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(CreateOutcome::Raced),
            Err(_) => Err("could not publish the secret file"),
        }
    })();
    // Best-effort temp cleanup on every path (the hard link leaves the temp behind).
    let _ = std::fs::remove_file(&temp);
    match result {
        Ok(outcome) => outcome,
        Err(reason) => CreateOutcome::Failed(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_generate_persists_then_reloads_the_same_seed() {
        let dir = tempdir();
        let path = dir.join("enroll.secret");
        let first = load_or_generate_seed(&path).unwrap();
        let second = load_or_generate_seed(&path).unwrap();
        // Reload yields the SAME seed (persisted, not regenerated).
        assert_eq!(*first, *second);
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
    fn a_wrong_length_seed_file_is_refused() {
        let dir = tempdir();
        let path = dir.join("enroll.secret");
        std::fs::write(&path, [0u8; 16]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(load_or_generate_seed(&path).is_err());
    }

    /// A throwaway directory under the OS temp dir (no `tempfile` dev-dep in this crate).
    fn tempdir() -> std::path::PathBuf {
        // A unique-enough name from the address of a stack local + the thread id; this crate forbids the
        // ambient clock/RNG helpers, and a collision only weakens isolation between concurrent runs.
        let probe = 0u8;
        let uniq = format!(
            "topos-secret-{:x}-{:?}",
            std::ptr::addr_of!(probe) as usize,
            std::thread::current().id()
        );
        let dir = std::env::temp_dir().join(uniq);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
