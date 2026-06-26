//! The read side: render + **authenticate** a stored version, and walk first-parent history. Every
//! blob's bytes are re-hashed through the kernel sha256 — gix's object id is never trusted as identity.

use std::collections::HashMap;

use gix::objs::tree::EntryKind;

use topos_core::digest::{self, FileMode, ManifestEntry};

use crate::VERSION_REF_PREFIX;
use crate::error::VerifyError;
use crate::store::Store;

/// One file rendered out of the store, with its content sha256 recomputed from the raw bytes.
#[derive(Debug, Clone)]
pub struct RenderedFile {
    pub path: String,
    pub mode: FileMode,
    pub bytes: Vec<u8>,
    pub content_sha256: [u8; 32],
}

/// A fully rendered + verified bundle: the files (sorted by raw path bytes) and the recomputed digest.
#[derive(Debug, Clone)]
pub struct RenderedBundle {
    pub files: Vec<RenderedFile>,
    pub bundle_digest: [u8; 32],
}

/// One node of per-skill history (for `log`): the version, its parents, and the commit's display
/// author + message. Author/message are read from the git commit for **display only** — they are not the
/// consent-critical path (that is `bundle_digest`, re-verified in [`Store::render_verified`]).
#[derive(Debug, Clone)]
pub struct VersionNode {
    pub version_id: [u8; 32],
    pub parents: Vec<[u8; 32]>,
    pub author: String,
    pub message: String,
}

impl Store {
    /// Render a stored version and **authenticate it against the pinned digest**.
    ///
    /// Resolves the version ref, walks its tree recursively (re-hashing every blob through the kernel
    /// sha256, never trusting gix's id), recomputes the canonical `bundle_digest`, and asserts it equals
    /// `expected_bundle_digest` (the caller's `lock.json` pin). A single corrupted/forged byte changes a
    /// blob hash → changes the recomputed digest → fails typed.
    ///
    /// # Errors
    /// [`VerifyError::MissingVersion`] / [`VerifyError::MissingObject`] if an object is absent;
    /// [`VerifyError::NonUtf8Name`] / [`VerifyError::NonBlobEntry`] on an illegal stored entry;
    /// [`VerifyError::BundleDigestMismatch`] if the recomputed digest does not match the pin.
    pub fn render_verified(
        &self,
        version_id: [u8; 32],
        expected_bundle_digest: [u8; 32],
    ) -> Result<RenderedBundle, VerifyError> {
        let commit_oid = self
            .resolve_version(&version_id)?
            .ok_or(VerifyError::MissingVersion)?;
        let commit = self
            .repo()
            .find_object(commit_oid)
            .map_err(|_| VerifyError::MissingObject)?
            .try_into_commit()
            .map_err(|e| VerifyError::Malformed(format!("{e}")))?;
        let tree = commit
            .tree()
            .map_err(|e| VerifyError::Malformed(format!("{e}")))?;

        let mut files = Vec::new();
        self.walk_tree(&tree, "", &mut files)?;

        // Re-run the kernel digest over the rendered bytes — re-applies check_path + the collision rules
        // AND recomputes the consent hash. A flipped byte fails here.
        let entries: Vec<ManifestEntry> = files
            .iter()
            .map(|f| ManifestEntry {
                path: f.path.clone(),
                mode: f.mode,
                content_sha256: f.content_sha256,
            })
            .collect();
        let recomputed = digest::bundle_digest(&entries)
            .map_err(|r| VerifyError::Malformed(format!("stored path rejected: {r:?}")))?;
        if recomputed != expected_bundle_digest {
            return Err(VerifyError::BundleDigestMismatch);
        }

        files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        Ok(RenderedBundle {
            files,
            bundle_digest: recomputed,
        })
    }

    /// First-parent history from `head`, newest first. Maps each git commit back to its `version_id` via
    /// the version-ref set, detecting an ambiguous lineage (two refs at one commit).
    ///
    /// # Errors
    /// [`VerifyError::MissingVersion`] if `head` is absent; [`VerifyError::DuplicateLineage`] on an
    /// ambiguous map; [`VerifyError::Malformed`] if a commit cannot be decoded.
    pub fn log(&self, head: [u8; 32]) -> Result<Vec<VersionNode>, VerifyError> {
        let reverse = self.reverse_map()?;
        let mut out = Vec::new();
        let mut cur = self
            .resolve_version(&head)?
            .ok_or(VerifyError::MissingVersion)?;
        loop {
            let version_id = *reverse
                .get(&cur)
                .ok_or_else(|| VerifyError::Malformed("commit not in version map".into()))?;
            let commit = self
                .repo()
                .find_object(cur)
                .map_err(|_| VerifyError::MissingObject)?
                .try_into_commit()
                .map_err(|e| VerifyError::Malformed(format!("{e}")))?;
            let parent_git: Vec<gix::ObjectId> =
                commit.parent_ids().map(|id| id.detach()).collect();
            let (author, message) = {
                let decoded = commit
                    .decode()
                    .map_err(|e| VerifyError::Malformed(format!("{e}")))?;
                let author = decoded
                    .author()
                    .map(|s| s.name.to_string())
                    .unwrap_or_default();
                (author, decoded.message.to_string())
            };
            let parents: Vec<[u8; 32]> = parent_git
                .iter()
                .filter_map(|g| reverse.get(g).copied())
                .collect();
            out.push(VersionNode {
                version_id,
                parents,
                author,
                message,
            });
            match parent_git.first() {
                Some(p) => cur = *p,
                None => break,
            }
        }
        Ok(out)
    }

    /// Every `version_id` recorded in this store (unordered).
    ///
    /// # Errors
    /// [`VerifyError::DuplicateLineage`] on an ambiguous map; [`VerifyError::Gix`] on a ref-read failure.
    pub fn list_versions(&self) -> Result<Vec<[u8; 32]>, VerifyError> {
        Ok(self.reverse_map()?.into_values().collect())
    }

    fn walk_tree(
        &self,
        tree: &gix::Tree<'_>,
        prefix: &str,
        out: &mut Vec<RenderedFile>,
    ) -> Result<(), VerifyError> {
        for entry in tree.iter() {
            let entry = entry.map_err(|e| VerifyError::Malformed(format!("{e}")))?;
            // Byte-oriented: reject a non-UTF-8 name (the scanner never wrote one).
            let name =
                std::str::from_utf8(entry.filename()).map_err(|_| VerifyError::NonUtf8Name)?;
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            let oid = entry.oid().to_owned();
            match entry.mode().kind() {
                EntryKind::Tree => {
                    let sub = self
                        .repo()
                        .find_object(oid)
                        .map_err(|_| VerifyError::MissingObject)?
                        .try_into_tree()
                        .map_err(|e| VerifyError::Malformed(format!("{e}")))?;
                    self.walk_tree(&sub, &path, out)?;
                }
                kind @ (EntryKind::Blob | EntryKind::BlobExecutable) => {
                    let mode = if kind == EntryKind::BlobExecutable {
                        FileMode::Executable
                    } else {
                        FileMode::Regular
                    };
                    let bytes = self
                        .repo()
                        .find_object(oid)
                        .map_err(|_| VerifyError::MissingObject)?
                        .detach()
                        .data;
                    let content_sha256 = digest::sha256(&bytes);
                    out.push(RenderedFile {
                        path,
                        mode,
                        bytes,
                        content_sha256,
                    });
                }
                // A symlink, gitlink, or any other entry kind the scanner would never have written.
                _ => return Err(VerifyError::NonBlobEntry),
            }
        }
        Ok(())
    }

    /// Build the `git OID -> version_id` map from the version-ref set, rejecting an ambiguous lineage.
    fn reverse_map(&self) -> Result<HashMap<gix::ObjectId, [u8; 32]>, VerifyError> {
        let mut map = HashMap::new();
        let platform = self.repo().references().map_err(verify_gix)?;
        for r in platform.prefixed(VERSION_REF_PREFIX).map_err(verify_gix)? {
            let mut r = r.map_err(verify_gix)?;
            let full = r.name().as_bstr().to_owned();
            let hex = full
                .strip_prefix(VERSION_REF_PREFIX.as_bytes())
                .ok_or_else(|| VerifyError::Malformed("ref outside version prefix".into()))?;
            let vid = decode_hex32(hex)?;
            let oid = r.peel_to_id().map_err(verify_gix)?.detach();
            if let Some(prev) = map.insert(oid, vid)
                && prev != vid
            {
                return Err(VerifyError::DuplicateLineage);
            }
        }
        Ok(map)
    }
}

fn decode_hex32(hex: &[u8]) -> Result<[u8; 32], VerifyError> {
    if hex.len() != 64 {
        return Err(VerifyError::Malformed(
            "version ref is not 64 hex chars".into(),
        ));
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_val(hex[2 * i]).ok_or_else(|| VerifyError::Malformed("non-hex ref".into()))?;
        let lo =
            hex_val(hex[2 * i + 1]).ok_or_else(|| VerifyError::Malformed("non-hex ref".into()))?;
        *slot = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

fn verify_gix<E: std::fmt::Display>(e: E) -> VerifyError {
    VerifyError::Gix(format!("{e}"))
}
