//! The content-addressed identity derivation — the one shared implementation.
//!
//! `topos-core` builds the canonical bytes for the identified object and hashes them. This is an
//! **identity / content-addressing** construction — there are no keys and no crypto here. Because a
//! single encoder is the one implementation, every component that re-derives an id agrees on the bytes
//! by construction (a divergent second derivation could silently fork the value).
//!
//! One derivation: the **commit** identity — `commit_id = sha256(frame)`, the user-facing
//! `version_id`. A length-prefixed binary frame. ([`commit_id`])
//!
//! ## Why a hand-specified binary frame, not a serialization crate
//!
//! A commit id is a content commitment: its bytes must be reproducible *forever* and across
//! independent implementations. General serialization formats (`bincode`, `borsh`, `postcard`) are the
//! wrong tool — their byte layout is a property of the library version, not a stability contract, so an
//! upgrade can silently change what a re-derivation reproduces. The established practice (TLS
//! transcripts, SSH wire format) is an explicit, length-prefixed, domain-separated frame. The one
//! library this leans on is the primitive `sha2`.

use crate::digest::sha256;
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
    const COMMIT_PREIMAGE: &str = "544f504f535f434f4d4d49545f563100011111111111111111111111111111111111111111111111111111111111111111222222222222222222222222222222222222222222222222222222222222222200000007645f616c69636500000029496d70726f76652050522074656d706c6174650a0a55736520696d7065726174697665206d6f6f642e";
    const COMMIT_ID: &str = "a10ee836cc1b8290caa8f55ce70c7ff2a281922adf9a94315cbf6c07edfa9225";

    const FIX_PARENTS: [[u8; 32]; 1] = [[0x11u8; 32]];
    const FIX_TREE: [u8; 32] = [0x22u8; 32];

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
