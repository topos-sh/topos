//! The read side: render + **authenticate** a stored version, and walk first-parent history. Every
//! blob's bytes are re-hashed through the kernel sha256 — gix's object id is never trusted as identity.

use std::collections::HashMap;

use gix::objs::tree::EntryKind;

use topos_core::digest::{self, FileMode, ManifestEntry};

use crate::error::VerifyError;
use crate::store::Store;
use crate::{GIT_OID_LEN, VERSION_REF_PREFIX};

/// The maximum directory nesting `render_verified` will follow — a forged store can't overflow the stack.
/// Far beyond any real bundle's depth.
const MAX_TREE_DEPTH: usize = 64;

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

/// One leaf of a stored version's tree **structure**: its bundle-relative path, mode, and the git OID the
/// tree entry records — recovered by walking the tree **without reading the blob's bytes**, so an offloaded
/// blob (whose git object is intentionally absent) is yielded too. The authority's location-dispatching
/// render joins this against `object_presence` to fetch each blob from git or the large-object store.
#[derive(Debug, Clone)]
pub struct TreeLeaf {
    pub path: String,
    pub mode: FileMode,
    pub git_oid: [u8; GIT_OID_LEN],
}

/// One node of per-bundle history (for `log`): the version, its parents, and the commit's display
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
    /// [`VerifyError::BundleDigestMismatch`] if the recomputed digest does not match the pin;
    /// [`VerifyError::Malformed`] on an undecodable/too-deep tree; [`VerifyError::Gix`] on a ref-read failure.
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
        self.walk_tree(&tree, "", 0, &mut files)?;

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

    /// Read + verify **one** object's bytes from a stored version, by its content id.
    ///
    /// Resolves the version's commit, walks its tree re-hashing each blob through the kernel sha256
    /// (never trusting gix's object id), and returns the bytes of the entry whose recomputed hash
    /// equals `object_id` — the hash match **is** the verification, so a corrupted or forged blob can
    /// never be returned. The plane drives this only *after* its bundle-scoped authorization has
    /// produced a witness version that provenance says reaches `object_id`; there is no read-by-bare-
    /// hash path. Keying retrieval on the content sha256 keeps a future size-routed large-object store
    /// a one-branch change here, with no change to identity, the database, or this signature.
    ///
    /// # Errors
    /// [`VerifyError::MissingVersion`] if the version is absent; [`VerifyError::ObjectNotInVersion`] if
    /// no blob in the version's tree hashes to `object_id`; [`VerifyError::MissingObject`] /
    /// [`VerifyError::NonBlobEntry`] / [`VerifyError::Malformed`] on a corrupt/forged store;
    /// [`VerifyError::Gix`] on a ref-read failure.
    pub fn read_object_in_version(
        &self,
        version_id: [u8; 32],
        object_id: [u8; 32],
    ) -> Result<Vec<u8>, VerifyError> {
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
        self.find_object_in_tree(&tree, object_id, 0)?
            .ok_or(VerifyError::ObjectNotInVersion)
    }

    /// Recover a stored version's tree **structure** — `(path, mode, git_oid)` per file — **without reading
    /// any blob bytes**, so a version that contains offloaded blobs (absent from the git object store) is
    /// walked fine (tree iteration reads only the tree objects' own bytes; sub-trees, which are never
    /// offloaded, are loaded to recurse). This is the dumb gitstore half of the authority's
    /// location-dispatching whole-bundle render: the authority joins each leaf's `git_oid` against
    /// `object_presence` to learn the location, then fetches from git or the large-object store.
    ///
    /// # Errors
    /// [`VerifyError::MissingVersion`] if the version is absent; [`VerifyError::MissingObject`] if a
    /// sub-tree object is missing; [`VerifyError::NonUtf8Name`] / [`VerifyError::NonBlobEntry`] /
    /// [`VerifyError::Malformed`] on an illegal or too-deep stored tree; [`VerifyError::Gix`] on a ref read.
    pub fn read_tree_structure(&self, version_id: [u8; 32]) -> Result<Vec<TreeLeaf>, VerifyError> {
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
        let mut leaves = Vec::new();
        self.walk_structure(&tree, "", 0, &mut leaves)?;
        Ok(leaves)
    }

    /// Read one **git-resident** blob's bytes by its git id, returning `(bytes, recomputed sha256)`. The
    /// authority's render dispatches here for a leaf the database locates in git, and trusts the bytes only
    /// via the returned content id. This **never infers offload from absence**: a missing object is the
    /// typed [`VerifyError::MissingObject`] (corruption for a rooted version) — whether a blob is offloaded
    /// is the database's `location`, not a git miss.
    ///
    /// # Errors
    /// [`VerifyError::MissingObject`] if the object is absent; [`VerifyError::NonBlobEntry`] if it is not a
    /// blob; [`VerifyError::Gix`] if the git id is malformed.
    pub fn read_git_blob_verified(
        &self,
        git_oid: [u8; GIT_OID_LEN],
    ) -> Result<(Vec<u8>, [u8; 32]), VerifyError> {
        let oid = gix::ObjectId::try_from(git_oid.as_slice())
            .map_err(|e| VerifyError::Gix(format!("{e}")))?;
        let object = self
            .repo()
            .find_object(oid)
            .map_err(|_| VerifyError::MissingObject)?;
        if object.kind != gix::objs::Kind::Blob {
            return Err(VerifyError::NonBlobEntry);
        }
        let bytes = object.detach().data;
        let content_sha256 = digest::sha256(&bytes);
        Ok((bytes, content_sha256))
    }

    /// Walk `tree` (bounded like [`Store::render_verified`]'s walk), returning the first blob whose
    /// recomputed sha256 equals `object_id`. Short-circuits on the match. A non-blob/non-tree entry in
    /// a stored tree is a forged/corrupt store (the scanner never writes one) and fails typed.
    fn find_object_in_tree(
        &self,
        tree: &gix::Tree<'_>,
        object_id: [u8; 32],
        depth: usize,
    ) -> Result<Option<Vec<u8>>, VerifyError> {
        if depth > MAX_TREE_DEPTH {
            return Err(VerifyError::Malformed("tree nesting too deep".into()));
        }
        for entry in tree.iter() {
            let entry = entry.map_err(|e| VerifyError::Malformed(format!("{e}")))?;
            let oid = entry.oid().to_owned();
            match entry.mode().kind() {
                EntryKind::Tree => {
                    let sub = self
                        .repo()
                        .find_object(oid)
                        .map_err(|_| VerifyError::MissingObject)?
                        .try_into_tree()
                        .map_err(|e| VerifyError::Malformed(format!("{e}")))?;
                    if let Some(found) = self.find_object_in_tree(&sub, object_id, depth + 1)? {
                        return Ok(Some(found));
                    }
                }
                EntryKind::Blob | EntryKind::BlobExecutable => {
                    // The mode says "blob"; assert the resolved object actually IS a blob so a forged
                    // entry can't make us hash a tree/commit payload as if it were file content.
                    let object = self
                        .repo()
                        .find_object(oid)
                        .map_err(|_| VerifyError::MissingObject)?;
                    if object.kind != gix::objs::Kind::Blob {
                        return Err(VerifyError::NonBlobEntry);
                    }
                    let bytes = object.detach().data;
                    if digest::sha256(&bytes) == object_id {
                        return Ok(Some(bytes));
                    }
                }
                // A symlink, gitlink, or any other entry kind the scanner would never have written.
                _ => return Err(VerifyError::NonBlobEntry),
            }
        }
        Ok(None)
    }

    /// First-parent history from `head`, newest first. Maps each git commit back to its `version_id` via
    /// the version-ref set, detecting an ambiguous lineage (two refs at one commit).
    ///
    /// # Errors
    /// [`VerifyError::MissingVersion`] if `head` is absent; [`VerifyError::DuplicateLineage`] on an
    /// ambiguous map; [`VerifyError::MissingObject`] / [`VerifyError::Malformed`] if a commit cannot be
    /// read or decoded; [`VerifyError::Gix`] on a ref-read failure.
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
            // Advance only to a first-parent that is itself a known version; an unknown parent is the
            // boundary of this machine's history (a partial sidecar store), where the walk stops cleanly
            // rather than hard-erroring — consistent with how `parents` already drops unknown OIDs.
            match parent_git.first() {
                Some(p) if reverse.contains_key(p) => cur = *p,
                _ => break,
            }
        }
        Ok(out)
    }

    /// Read EXACTLY one commit's metadata — `(version_id, parents, author, message)` — **without walking
    /// history**. Resolves `version_id` to its git commit, decodes its display author + message, and maps
    /// EVERY parent's git OID back to a `version_id` via the version-ref set.
    ///
    /// Unlike [`Self::log`] — whose `filter_map` silently drops a parent it cannot map (correct for a partial
    /// sidecar's display history, where the walk stops at its boundary) — this **fails** [`VerifyError::
    /// UnmappedParent`] on any unmapped parent: the authority's version-metadata response must carry the
    /// complete, exact parent set, never a quietly truncated one. Author/message are display-only (the
    /// consent-critical fact is `bundle_digest`, re-verified in [`Self::render_verified`]).
    ///
    /// # Errors
    /// [`VerifyError::MissingVersion`] if `version_id` is absent; [`VerifyError::UnmappedParent`] if a parent
    /// is not in the version-ref set; [`VerifyError::DuplicateLineage`] on an ambiguous map;
    /// [`VerifyError::MissingObject`] / [`VerifyError::Malformed`] if the commit cannot be read/decoded;
    /// [`VerifyError::Gix`] on a ref-read failure.
    pub fn read_commit_meta(&self, version_id: [u8; 32]) -> Result<VersionNode, VerifyError> {
        // The reverse map (owned) is built first so its transient repo borrow is released before the commit
        // borrow below; it maps each parent git OID back to its topos version_id.
        let reverse = self.reverse_map()?;
        let commit_oid = self
            .resolve_version(&version_id)?
            .ok_or(VerifyError::MissingVersion)?;
        let commit = self
            .repo()
            .find_object(commit_oid)
            .map_err(|_| VerifyError::MissingObject)?
            .try_into_commit()
            .map_err(|e| VerifyError::Malformed(format!("{e}")))?;
        let parent_git: Vec<gix::ObjectId> = commit.parent_ids().map(|id| id.detach()).collect();
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
        // Map EVERY parent (never a lenient `filter_map`): a parent outside the version-ref set is a fault,
        // not a silent history boundary — the metadata response reports the complete, exact lineage.
        let mut parents = Vec::with_capacity(parent_git.len());
        for g in &parent_git {
            let vid = reverse.get(g).copied().ok_or(VerifyError::UnmappedParent)?;
            parents.push(vid);
        }
        Ok(VersionNode {
            version_id,
            parents,
            author,
            message,
        })
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
        depth: usize,
        out: &mut Vec<RenderedFile>,
    ) -> Result<(), VerifyError> {
        // Bound the recursion so a forged/corrupted store with a deep tree chain can't overflow the stack.
        if depth > MAX_TREE_DEPTH {
            return Err(VerifyError::Malformed("tree nesting too deep".into()));
        }
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
                    self.walk_tree(&sub, &path, depth + 1, out)?;
                }
                kind @ (EntryKind::Blob | EntryKind::BlobExecutable) => {
                    let mode = if kind == EntryKind::BlobExecutable {
                        FileMode::Executable
                    } else {
                        FileMode::Regular
                    };
                    // The mode says "blob"; assert the resolved object actually IS a blob, so a forged
                    // entry can't make us hash a tree/commit payload as if it were file content.
                    let object = self
                        .repo()
                        .find_object(oid)
                        .map_err(|_| VerifyError::MissingObject)?;
                    if object.kind != gix::objs::Kind::Blob {
                        return Err(VerifyError::NonBlobEntry);
                    }
                    let bytes = object.detach().data;
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

    /// Walk `tree` like [`Self::walk_tree`] but record only the leaf **structure** (`path, mode, git_oid`),
    /// **never reading a blob object** — so an offloaded blob with an absent git object does not fault here.
    /// Sub-trees (never offloaded) are loaded to recurse; a non-blob/non-tree entry is a forged/corrupt store.
    fn walk_structure(
        &self,
        tree: &gix::Tree<'_>,
        prefix: &str,
        depth: usize,
        out: &mut Vec<TreeLeaf>,
    ) -> Result<(), VerifyError> {
        if depth > MAX_TREE_DEPTH {
            return Err(VerifyError::Malformed("tree nesting too deep".into()));
        }
        for entry in tree.iter() {
            let entry = entry.map_err(|e| VerifyError::Malformed(format!("{e}")))?;
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
                    self.walk_structure(&sub, &path, depth + 1, out)?;
                }
                kind @ (EntryKind::Blob | EntryKind::BlobExecutable) => {
                    let mode = if kind == EntryKind::BlobExecutable {
                        FileMode::Executable
                    } else {
                        FileMode::Regular
                    };
                    // Record the locator from the tree entry itself — NO blob read, so an offloaded blob's
                    // absent git object is fine. The git OID is the bridge the authority joins on `location`.
                    let git_oid: [u8; GIT_OID_LEN] = oid
                        .as_slice()
                        .try_into()
                        .map_err(|_| VerifyError::Malformed("git oid is not 20 bytes".into()))?;
                    out.push(TreeLeaf {
                        path,
                        mode,
                        git_oid,
                    });
                }
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
