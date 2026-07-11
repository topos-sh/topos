//! The content-addressed identity derivations — the one shared implementation of each.
//!
//! `topos-core` builds the canonical bytes for each identified object and hashes them. These are
//! **identity / content-addressing** constructions — there are no keys and no crypto here. Because a
//! single encoder is the one implementation, every component that re-derives an id agrees on the bytes
//! by construction (a divergent second derivation could silently fork the value).
//!
//! Three derivations:
//!
//! - the **commit** identity: `commit_id = sha256(frame)` — the user-facing `version_id`. A
//!   length-prefixed binary frame. ([`commit_id`])
//! - the **device key id** [`device_key_id`] — `dk_` + the first 32 hex chars of `sha256(pubkey)`,
//!   the server-derived id a registered device public key is known by.
//! - the **canonical principal** fold [`canonical_principal`] — the ASCII-lowercase fold every
//!   email-valued identifier passes so one human's `Alice@x` / `alice@x` are one identity everywhere.
//!
//! ## Why a hand-specified binary frame, not a serialization crate
//!
//! A commit id is a content commitment: its bytes must be reproducible *forever* and across
//! independent implementations. General serialization formats (`bincode`, `borsh`, `postcard`) are the
//! wrong tool — their byte layout is a property of the library version, not a stability contract, so an
//! upgrade can silently change what a re-derivation reproduces. The established practice (TLS
//! transcripts, SSH wire format) is an explicit, length-prefixed, domain-separated frame. The one
//! library this leans on is the primitive `sha2`.

use crate::digest::{sha256, to_hex};
use alloc::string::String;
use alloc::vec::Vec;

/// The ASCII context tag for the commit-id frame (15 chars + NUL = 16 bytes).
const COMMIT_TAG: &[u8] = b"TOPOS_COMMIT_V1\0";

/// Why a preimage could not be built. Every case is unreachable for well-formed inputs (a commit has
/// ≤ 2 parents; ids/messages are far under 4 GiB) — they exist so the builders stay **total and
/// panic-free** rather than silently emitting bytes a re-derivation won't match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreimageError {
    /// A commit may have at most two parents (0 = genesis, 1 = normal, 2 = author-merge).
    TooManyParents,
    /// A length-prefixed string field exceeded `u32::MAX` bytes and cannot be framed.
    FieldTooLong,
}

// ---------------------------------------------------------------------------------------------
// Cross-component identity: the device key id and the principal fold.
// ---------------------------------------------------------------------------------------------

/// The `dk_`-prefixed hex length of a [`device_key_id`] (the first 32 hex chars of the sha256).
const DEVICE_KEY_ID_HEX_LEN: usize = 32;

/// The device key id derived from a raw 32-byte device public key: `dk_` + the first
/// 32 hex chars of `sha256(public_key)`.
///
/// A **cross-component identity**: the plane derives this id server-side from the registered public key
/// — a client-asserted id is never trusted — and it is written once here so every component that maps a
/// key to its id agrees. Stable across restarts (derived from the persisted key, never random) and
/// public (it does not reveal the key).
#[must_use]
pub fn device_key_id(public_key: &[u8; 32]) -> String {
    let hex = to_hex(&sha256(public_key));
    let mut id = String::with_capacity(3 + DEVICE_KEY_ID_HEX_LEN);
    id.push_str("dk_");
    id.push_str(&hex[..DEVICE_KEY_ID_HEX_LEN]);
    id
}

/// The canonical form of a principal identifier (an email, or a device-rooted `dev.dk_…` id):
/// the ASCII-lowercase fold of the input.
///
/// A **cross-component identity rule**, like [`device_key_id`]: every email-valued identifier — the
/// governance Invite email set, the RosterSet/RosterRemove targets — folds through this function so one
/// human's `Alice@x` / `alice@x` are one rostered identity everywhere (storage, roster gates,
/// idempotency hashes), and the plane folds at its parse boundary. Principals are ASCII-only by charset
/// (the plane's parse rejects non-ASCII), so the ASCII fold is total; device key ids are lowercase hex,
/// so folding a `dev.dk_…` principal is a no-op.
#[must_use]
pub fn canonical_principal(s: &str) -> String {
    s.to_ascii_lowercase()
}

// ---------------------------------------------------------------------------------------------
// Commit — the content hash that yields `commit_id` (= `version_id`). A content id, not a credential.
// ---------------------------------------------------------------------------------------------

/// The content a commit commits to (git's model, reused): ordered parents + the bundle digest as the
/// tree + the author device-id + the message. `parents[0]` is the trunk parent (the first-parent rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commit<'a> {
    /// 0 (genesis), 1 (normal), or 2 (author-merge) parent commit ids; `parents[0]` is the trunk parent.
    pub parents: &'a [[u8; 32]],
    /// The bundle digest (the byte-exact consent hash) — git's "tree".
    pub tree: [u8; 32],
    /// The author device-id.
    pub author: &'a str,
    /// The commit message (title + body, already composed into one string).
    pub message: &'a str,
}

/// Build the canonical commit frame (the bytes hashed to form `commit_id`).
///
/// Layout: `TOPOS_COMMIT_V1\0` ‖ `u8`(parent count) ‖ each parent (32 B) ‖ `tree` (32 B) ‖
/// `u32be`(author len) ‖ author ‖ `u32be`(message len) ‖ message. Every multi-byte integer is
/// big-endian; every string length prefix is `u32be` (one width rule).
///
/// # Errors
/// [`PreimageError::TooManyParents`] if more than two parents are supplied;
/// [`PreimageError::FieldTooLong`] if a string field exceeds `u32::MAX` bytes.
pub fn commit_preimage(commit: &Commit) -> Result<Vec<u8>, PreimageError> {
    // Checked, like the length prefixes (no silent `as u8` truncation if the cap is ever raised).
    let parent_count = u8::try_from(commit.parents.len())
        .ok()
        .filter(|&n| n <= 2)
        .ok_or(PreimageError::TooManyParents)?;
    let mut out = Vec::new();
    out.extend_from_slice(COMMIT_TAG);
    out.push(parent_count);
    for parent in commit.parents {
        out.extend_from_slice(parent);
    }
    out.extend_from_slice(&commit.tree);
    put_lp_str(&mut out, commit.author)?;
    put_lp_str(&mut out, commit.message)?;
    Ok(out)
}

/// The commit id (= `version_id`): `sha256` over the canonical commit frame.
///
/// # Errors
/// As [`commit_preimage`].
pub fn commit_id(commit: &Commit) -> Result<[u8; 32], PreimageError> {
    Ok(sha256(&commit_preimage(commit)?))
}

// ---------------------------------------------------------------------------------------------
// Internal encoder.
// ---------------------------------------------------------------------------------------------

/// Append a `u32be` length prefix + the raw UTF-8 bytes of `s`.
fn put_lp_str(out: &mut Vec<u8>, s: &str) -> Result<(), PreimageError> {
    let len = u32::try_from(s.len()).map_err(|_| PreimageError::FieldTooLong)?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    // ---- The frozen known-answer vectors (computed once from these encoders, then pinned). A change
    // ---- to any encoding breaks one of these loudly; update only if the change is INTENTIONAL. ----
    //
    // Vector key (a test seed, NOT a real key): device key seed = bytes 00..1f.
    const DEVICE_PK: &str = "03a107bff3ce10be1d70dd18e74bc09967e4d6309ba50d5f1ddc8664125531b8";
    const COMMIT_PREIMAGE: &str = "544f504f535f434f4d4d49545f563100011111111111111111111111111111111111111111111111111111111111111111222222222222222222222222222222222222222222222222222222222222222200000007645f616c69636500000029496d70726f76652050522074656d706c6174650a0a55736520696d7065726174697665206d6f6f642e";
    const COMMIT_ID: &str = "a10ee836cc1b8290caa8f55ce70c7ff2a281922adf9a94315cbf6c07edfa9225";

    const FIX_PARENTS: [[u8; 32]; 1] = [[0x11u8; 32]];
    const FIX_TREE: [u8; 32] = [0x22u8; 32];

    fn unhex(s: &str) -> Vec<u8> {
        hex::decode(s).expect("valid hex vector")
    }
    fn arr32(s: &str) -> [u8; 32] {
        unhex(s).try_into().expect("32-byte vector")
    }

    fn fixture_commit() -> Commit<'static> {
        Commit {
            parents: &FIX_PARENTS,
            tree: FIX_TREE,
            author: "d_alice",
            message: "Improve PR template\n\nUse imperative mood.",
        }
    }

    // ---- Commit-id ----

    #[test]
    fn commit_id_known_answer() {
        let commit = fixture_commit();
        assert_eq!(
            crate::digest::to_hex(&commit_preimage(&commit).unwrap()),
            COMMIT_PREIMAGE,
            "commit frame changed — update only if the encoding INTENTIONALLY changed",
        );
        assert_eq!(
            crate::digest::to_hex(&commit_id(&commit).unwrap()),
            COMMIT_ID
        );
    }

    #[test]
    fn commit_parent_count_is_framed_and_capped() {
        // The parent count is the first byte after the 16-byte tag.
        let genesis = Commit {
            parents: &[],
            ..fixture_commit()
        };
        assert_eq!(commit_preimage(&genesis).unwrap()[16], 0);

        let two = [[0xAAu8; 32], [0xBBu8; 32]];
        let merge = Commit {
            parents: &two,
            ..fixture_commit()
        };
        assert_eq!(commit_preimage(&merge).unwrap()[16], 2);

        // A third parent is unrepresentable, not a panic.
        let three = [[0u8; 32], [1u8; 32], [2u8; 32]];
        let bad = Commit {
            parents: &three,
            ..fixture_commit()
        };
        assert_eq!(commit_preimage(&bad), Err(PreimageError::TooManyParents));
        assert_eq!(commit_id(&bad), Err(PreimageError::TooManyParents));
    }

    #[test]
    fn commit_id_binds_every_field() {
        let base = commit_id(&fixture_commit()).unwrap();
        let other_tree = commit_id(&Commit {
            tree: [0x33; 32],
            ..fixture_commit()
        })
        .unwrap();
        let other_author = commit_id(&Commit {
            author: "d_bob",
            ..fixture_commit()
        })
        .unwrap();
        let other_msg = commit_id(&Commit {
            message: "Different",
            ..fixture_commit()
        })
        .unwrap();
        let other_parent = commit_id(&Commit {
            parents: &[[0x99; 32]],
            ..fixture_commit()
        })
        .unwrap();
        assert_ne!(base, other_tree);
        assert_ne!(base, other_author);
        assert_ne!(base, other_msg);
        assert_ne!(base, other_parent);
    }

    // ---- Cross-component identity derivations (the shared impls every component calls) ----

    #[test]
    fn device_key_id_known_answer() {
        // The frozen device key (seed 00..1f → DEVICE_PK) derives this exact id — the SAME value the
        // plane re-derives from the registered key.
        assert_eq!(
            device_key_id(&arr32(DEVICE_PK)),
            "dk_56475aa75463474c0285df5dbf2bcab7"
        );
        // Shape: the `dk_` prefix + exactly the first 32 hex chars of sha256(pubkey).
        let full = to_hex(&sha256(&arr32(DEVICE_PK)));
        assert_eq!(
            device_key_id(&arr32(DEVICE_PK)),
            alloc::format!("dk_{}", &full[..32])
        );
    }

    #[test]
    fn canonical_principal_is_the_total_ascii_fold() {
        // The one identity fold every component binds: emails fold to lowercase, already-canonical
        // strings (every lowercase email, every `dev.dk_…` device-rooted principal — key ids are
        // lowercase hex) are fixpoints.
        assert_eq!(canonical_principal("Alice@Acme.COM"), "alice@acme.com");
        assert_eq!(canonical_principal("alice@acme.com"), "alice@acme.com");
        assert_eq!(
            canonical_principal("dev.dk_56475aa75463474c0285df5dbf2bcab7"),
            "dev.dk_56475aa75463474c0285df5dbf2bcab7"
        );
    }

    #[test]
    fn lp_str_writes_a_u32be_length_prefix() {
        // The frozen width: a 4-byte big-endian length, then the raw UTF-8 bytes. (A field longer
        // than u32::MAX — unreachable in practice — would be a typed error, never a truncation/panic.)
        let mut buf = vec![];
        put_lp_str(&mut buf, "abc").unwrap();
        assert_eq!(buf, vec![0x00, 0x00, 0x00, 0x03, b'a', b'b', b'c']);

        let mut empty = vec![];
        put_lp_str(&mut empty, "").unwrap();
        assert_eq!(empty, vec![0x00, 0x00, 0x00, 0x00]);
    }
}
