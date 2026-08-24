//! The in-memory loose-object codec — encode/decode **git loose objects without a repository**.
//!
//! The plane stores every byte as an immutable zlib-compressed loose git object in an object store
//! (a local directory or an S3-compatible bucket), keyed by its own git OID at the exact bare-repo
//! loose path shape. It holds no repo on disk, so the object mechanics it needs are pure functions:
//! frame bytes as a blob, build the (nested) tree objects for a candidate's paths, encode a commit
//! with the SAME reproducible frame [`Store::commit`](crate::Store::commit) writes (fixed committer
//! identity, epoch-zero time — so identical inputs yield the identical commit OID on both paths),
//! and decode any of the three back, verifying the id that named the object.
//!
//! Everything here is deterministic and I/O-free. Identity is still topos's: git OIDs are internal
//! storage handles; `version_id`/`bundle_digest`/`object_id` are the kernel sha256s, computed by
//! the caller over real bytes.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};

use gix::objs::tree::EntryKind;

use topos_core::digest::FileMode;

use crate::error::{GitstoreError, VerifyError};
use crate::fence::GIT_OID_LEN;
use crate::store::{TOPOS_COMMITTER_EMAIL, TOPOS_COMMITTER_NAME, gix_err};

/// The nesting bound for the tree builder and decoder — the same bound every store-side walk
/// applies, so a candidate deep enough to be refused here was already unrenderable.
const MAX_TREE_DEPTH: usize = 64;

/// The largest decompressed loose object the decoder will produce (well above the plane's per-blob
/// ingest cap; a corrupted/hostile stream that inflates past it fails typed instead of exhausting
/// memory).
const MAX_LOOSE_SIZE: u64 = 1 << 30;

/// A loose object's kind — the three the plane ever stores (a tag is refused at decode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Blob,
    Tree,
    Commit,
}

impl ObjectKind {
    fn to_gix(self) -> gix::objs::Kind {
        match self {
            ObjectKind::Blob => gix::objs::Kind::Blob,
            ObjectKind::Tree => gix::objs::Kind::Tree,
            ObjectKind::Commit => gix::objs::Kind::Commit,
        }
    }
}

/// One encoded loose object: its git OID (the storage key) and the zlib loose-object bytes —
/// byte-for-byte what a bare repo would hold at `objects/<aa>/<38-hex>`.
#[derive(Debug, Clone)]
pub struct LooseObject {
    pub git_oid: [u8; GIT_OID_LEN],
    pub zlib_bytes: Vec<u8>,
}

/// The tree objects a candidate's paths encode to: the root tree's OID (what the commit names) and
/// every distinct tree object (nested paths create subtrees), each ready to store.
#[derive(Debug, Clone)]
pub struct EncodedTree {
    pub root_oid: [u8; GIT_OID_LEN],
    pub objects: Vec<LooseObject>,
}

/// One decoded commit's frame facts: the root tree, the git parent commits, the display author
/// name, and the message.
#[derive(Debug, Clone)]
pub struct CommitMeta {
    pub tree_oid: [u8; GIT_OID_LEN],
    pub parent_commit_oids: Vec<[u8; GIT_OID_LEN]>,
    pub author: String,
    pub message: String,
}

/// One entry of a decoded tree object — a subtree to recurse into, or a file leaf.
#[derive(Debug, Clone)]
pub enum TreeChild {
    Subtree {
        name: String,
        git_oid: [u8; GIT_OID_LEN],
    },
    File {
        name: String,
        mode: FileMode,
        git_oid: [u8; GIT_OID_LEN],
    },
}

/// The git OID of `bytes` framed as a blob — the pure hash half of [`encode_blob`], for callers
/// (staging, import verification) that need the storage key without paying for compression.
///
/// # Errors
/// [`GitstoreError::Gix`] on a hashing fault (not reachable for sha1 in practice).
pub fn blob_git_oid(bytes: &[u8]) -> Result<[u8; GIT_OID_LEN], GitstoreError> {
    let oid = gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
        .map_err(gix_err)?;
    oid_to_array(oid)
}

/// Frame raw file bytes as a zlib loose blob object.
///
/// # Errors
/// [`GitstoreError::Gix`] on a hashing fault; [`GitstoreError::Io`] on a compression fault.
pub fn encode_blob(bytes: &[u8]) -> Result<LooseObject, GitstoreError> {
    encode_loose(ObjectKind::Blob, bytes)
}

/// Build the tree objects for a candidate's `(path, mode, git_oid)` leaves — nested paths create
/// subtrees, entries sort in git's tree order, and every path component passes the SAME validation
/// the repo-backed [`Store`](crate::Store) write path applies (reject `.git`/`.gitmodules` + their
/// HFS+/NTFS aliases, path separators, Windows devices/illegal chars; the kernel `check_path`
/// covers `.`/`..`/NUL/absolute upstream). Identical subtrees dedup to one object.
///
/// # Errors
/// [`GitstoreError::RejectPath`] on an invalid component or a file/directory path collision;
/// [`GitstoreError::Gix`]/[`GitstoreError::Io`] on an encode fault.
pub fn encode_tree(
    entries: &[(&str, FileMode, [u8; GIT_OID_LEN])],
) -> Result<EncodedTree, GitstoreError> {
    // Build the nested directory structure, validating every component exactly as the repo-backed
    // plumbing editor build does (fence.rs) — a server distributing bundles to heterogeneous
    // clients rejects every platform's aliases regardless of its own host.
    let mut root = DirNode::default();
    for (path, mode, git_oid) in entries {
        for component in path.split('/') {
            gix::validate::path::component(
                component.into(),
                None,
                gix::validate::path::component::Options::default(),
            )
            .map_err(|e| GitstoreError::RejectPath(format!("{component:?}: {e}")))?;
        }
        root.insert(path, path, *mode, *git_oid)?;
    }
    // Post-order encode, deduping identical subtree objects by OID.
    let mut objects: BTreeMap<[u8; GIT_OID_LEN], LooseObject> = BTreeMap::new();
    let root_oid = encode_dir(&root, 0, &mut objects)?;
    Ok(EncodedTree {
        root_oid,
        objects: objects.into_values().collect(),
    })
}

/// Encode a commit object with the SAME reproducible frame the repo-backed
/// [`Store::commit`](crate::Store::commit) writes: author = the display attribution over the fixed
/// topos email, fixed committer identity, epoch-zero time — so identical inputs yield the identical
/// commit OID whether written through a repo or through this codec (a parity test pins it).
///
/// # Errors
/// [`GitstoreError::Gix`] on an encode/hash fault; [`GitstoreError::Io`] on a compression fault.
pub fn encode_commit(
    tree_oid: [u8; GIT_OID_LEN],
    parent_commit_oids: &[[u8; GIT_OID_LEN]],
    author: &str,
    message: &str,
) -> Result<LooseObject, GitstoreError> {
    let time = gix::date::Time::new(0, 0);
    let commit = gix::objs::Commit {
        tree: oid_from_array(tree_oid)?,
        parents: parent_commit_oids
            .iter()
            .map(|p| oid_from_array(*p))
            .collect::<Result<_, _>>()?,
        author: gix::actor::Signature {
            name: author.into(),
            email: TOPOS_COMMITTER_EMAIL.into(),
            time,
        },
        committer: gix::actor::Signature {
            name: TOPOS_COMMITTER_NAME.into(),
            email: TOPOS_COMMITTER_EMAIL.into(),
            time,
        },
        encoding: None,
        message: message.into(),
        extra_headers: Vec::new(),
    };
    let mut payload = Vec::new();
    gix::objs::WriteTo::write_to(&commit, &mut payload).map_err(gix_err)?;
    encode_loose(ObjectKind::Commit, &payload)
}

/// Decompress + parse one loose object, verifying it against the id that named it: the header must
/// be well-formed, the declared size exact, and `sha1(header + payload)` must equal `expected_oid`
/// — a corrupted or substituted object can never be returned. Returns the kind + raw payload.
///
/// # Errors
/// [`VerifyError::Malformed`] on a corrupt stream/header or an oversize object;
/// [`VerifyError::IdMismatch`] when the bytes do not hash to `expected_oid`.
pub fn decode_loose(
    zlib_bytes: &[u8],
    expected_oid: [u8; GIT_OID_LEN],
) -> Result<(ObjectKind, Vec<u8>), VerifyError> {
    // Bounded inflate: a corrupt/hostile stream that would expand past the cap fails typed.
    let mut decoder = flate2::read::ZlibDecoder::new(zlib_bytes).take(MAX_LOOSE_SIZE + 64);
    let mut raw = Vec::new();
    decoder
        .read_to_end(&mut raw)
        .map_err(|e| VerifyError::Malformed(format!("undecodable zlib stream: {e}")))?;
    if raw.len() as u64 > MAX_LOOSE_SIZE {
        return Err(VerifyError::Malformed(
            "loose object exceeds the decode size cap".into(),
        ));
    }
    // Parse the `<kind> <decimal-size>\0` header.
    let nul = raw
        .iter()
        .take(32)
        .position(|&b| b == 0)
        .ok_or_else(|| VerifyError::Malformed("loose object header has no NUL".into()))?;
    let header = std::str::from_utf8(&raw[..nul])
        .map_err(|_| VerifyError::Malformed("loose object header is not UTF-8".into()))?;
    let (kind_word, size_word) = header
        .split_once(' ')
        .ok_or_else(|| VerifyError::Malformed("loose object header has no size".into()))?;
    let kind = match kind_word {
        "blob" => ObjectKind::Blob,
        "tree" => ObjectKind::Tree,
        "commit" => ObjectKind::Commit,
        other => {
            return Err(VerifyError::Malformed(format!(
                "unexpected loose object kind: {other}"
            )));
        }
    };
    let declared: u64 = size_word
        .parse()
        .map_err(|_| VerifyError::Malformed("loose object header size is not decimal".into()))?;
    let payload = raw[nul + 1..].to_vec();
    if declared != payload.len() as u64 {
        return Err(VerifyError::Malformed(
            "loose object payload does not match its declared size".into(),
        ));
    }
    // Verify-on-decode: the whole object must hash to the id that named it.
    let recomputed = gix::objs::compute_hash(gix::hash::Kind::Sha1, kind.to_gix(), &payload)
        .map_err(|e| VerifyError::Malformed(format!("{e}")))?;
    if recomputed.as_slice() != expected_oid.as_slice() {
        return Err(VerifyError::IdMismatch);
    }
    Ok((kind, payload))
}

/// Decode a commit payload's frame facts (tree, git parents, display author name, message).
///
/// # Errors
/// [`VerifyError::Malformed`] on an undecodable commit.
pub fn decode_commit(payload: &[u8]) -> Result<CommitMeta, VerifyError> {
    let commit = gix::objs::CommitRef::from_bytes(payload, gix::hash::Kind::Sha1)
        .map_err(|e| VerifyError::Malformed(format!("{e}")))?;
    let mut parent_commit_oids = Vec::new();
    for parent in commit.parents() {
        parent_commit_oids.push(
            oid_to_array(parent).map_err(|_| VerifyError::Malformed("bad parent oid".into()))?,
        );
    }
    Ok(CommitMeta {
        tree_oid: oid_to_array(commit.tree())
            .map_err(|_| VerifyError::Malformed("bad tree oid".into()))?,
        parent_commit_oids,
        author: commit
            .author()
            .map(|s| s.name.to_string())
            .unwrap_or_default(),
        message: commit.message.to_string(),
    })
}

/// Decode a tree payload's entries. A non-blob/non-tree entry (a symlink, a gitlink) is a
/// forged/corrupt object — the write paths never produce one.
///
/// # Errors
/// [`VerifyError::Malformed`] on an undecodable tree; [`VerifyError::NonUtf8Name`] /
/// [`VerifyError::NonBlobEntry`] on an illegal stored entry.
pub fn decode_tree(payload: &[u8]) -> Result<Vec<TreeChild>, VerifyError> {
    let tree = gix::objs::TreeRef::from_bytes(payload, gix::hash::Kind::Sha1)
        .map_err(|e| VerifyError::Malformed(format!("{e}")))?;
    let mut out = Vec::with_capacity(tree.entries.len());
    for entry in tree.entries {
        let name = std::str::from_utf8(entry.filename)
            .map_err(|_| VerifyError::NonUtf8Name)?
            .to_owned();
        let git_oid: [u8; GIT_OID_LEN] = entry
            .oid
            .as_bytes()
            .try_into()
            .map_err(|_| VerifyError::Malformed("git oid is not 20 bytes".into()))?;
        match entry.mode.kind() {
            EntryKind::Tree => out.push(TreeChild::Subtree { name, git_oid }),
            EntryKind::Blob => out.push(TreeChild::File {
                name,
                mode: FileMode::Regular,
                git_oid,
            }),
            EntryKind::BlobExecutable => out.push(TreeChild::File {
                name,
                mode: FileMode::Executable,
                git_oid,
            }),
            _ => return Err(VerifyError::NonBlobEntry),
        }
    }
    Ok(out)
}

// ── internals ─────────────────────────────────────────────────────────────────────────────────

/// The nested directory structure the tree builder assembles before encoding.
#[derive(Debug, Default)]
struct DirNode {
    files: BTreeMap<String, (FileMode, [u8; GIT_OID_LEN])>,
    dirs: BTreeMap<String, DirNode>,
}

impl DirNode {
    /// Insert one leaf at `rest` (the remaining path), carrying `full` for error messages. A path
    /// that is both a file and a directory prefix is a collision the kernel digest would have
    /// rejected upstream — refused typed here so the codec never silently drops a leaf.
    fn insert(
        &mut self,
        full: &str,
        rest: &str,
        mode: FileMode,
        git_oid: [u8; GIT_OID_LEN],
    ) -> Result<(), GitstoreError> {
        match rest.split_once('/') {
            None => {
                if self.dirs.contains_key(rest)
                    || self
                        .files
                        .insert(rest.to_owned(), (mode, git_oid))
                        .is_some()
                {
                    return Err(GitstoreError::RejectPath(format!(
                        "path collision at {full:?}"
                    )));
                }
                Ok(())
            }
            Some((dir, tail)) => {
                if self.files.contains_key(dir) {
                    return Err(GitstoreError::RejectPath(format!(
                        "path collision at {full:?}"
                    )));
                }
                self.dirs
                    .entry(dir.to_owned())
                    .or_default()
                    .insert(full, tail, mode, git_oid)
            }
        }
    }
}

/// Post-order: encode every subtree of `node`, then `node`'s own tree object, into `objects`
/// (deduped by OID), returning `node`'s tree OID.
fn encode_dir(
    node: &DirNode,
    depth: usize,
    objects: &mut BTreeMap<[u8; GIT_OID_LEN], LooseObject>,
) -> Result<[u8; GIT_OID_LEN], GitstoreError> {
    if depth > MAX_TREE_DEPTH {
        return Err(GitstoreError::RejectPath("tree nesting too deep".into()));
    }
    let mut entries: Vec<gix::objs::tree::Entry> =
        Vec::with_capacity(node.files.len() + node.dirs.len());
    for (name, sub) in &node.dirs {
        let sub_oid = encode_dir(sub, depth + 1, objects)?;
        entries.push(gix::objs::tree::Entry {
            mode: EntryKind::Tree.into(),
            filename: name.as_str().into(),
            oid: oid_from_array(sub_oid)?,
        });
    }
    for (name, (mode, git_oid)) in &node.files {
        let kind = match mode {
            FileMode::Regular => EntryKind::Blob,
            FileMode::Executable => EntryKind::BlobExecutable,
        };
        entries.push(gix::objs::tree::Entry {
            mode: kind.into(),
            filename: name.as_str().into(),
            oid: oid_from_array(*git_oid)?,
        });
    }
    // git's tree order (a directory name compares as if suffixed with '/') — the Entry Ord impl.
    entries.sort();
    let tree = gix::objs::Tree { entries };
    let mut payload = Vec::new();
    gix::objs::WriteTo::write_to(&tree, &mut payload).map_err(gix_err)?;
    let loose = encode_loose(ObjectKind::Tree, &payload)?;
    let oid = loose.git_oid;
    objects.entry(oid).or_insert(loose);
    Ok(oid)
}

/// Hash + zlib-frame one payload as a loose object of `kind`.
fn encode_loose(kind: ObjectKind, payload: &[u8]) -> Result<LooseObject, GitstoreError> {
    let oid =
        gix::objs::compute_hash(gix::hash::Kind::Sha1, kind.to_gix(), payload).map_err(gix_err)?;
    let header = gix::objs::encode::loose_header(kind.to_gix(), payload.len() as u64);
    let mut encoder = flate2::write::ZlibEncoder::new(
        Vec::with_capacity(payload.len() / 2 + 64),
        flate2::Compression::default(),
    );
    encoder
        .write_all(&header)
        .and_then(|()| encoder.write_all(payload))
        .map_err(|e| GitstoreError::Io(format!("{e}")))?;
    let zlib_bytes = encoder
        .finish()
        .map_err(|e| GitstoreError::Io(format!("{e}")))?;
    Ok(LooseObject {
        git_oid: oid_to_array(oid)?,
        zlib_bytes,
    })
}

/// A git [`gix::ObjectId`] from a 20-byte locator (these stores are sha1).
fn oid_from_array(git_oid: [u8; GIT_OID_LEN]) -> Result<gix::ObjectId, GitstoreError> {
    gix::ObjectId::try_from(git_oid.as_slice())
        .map_err(|e| GitstoreError::Gix(format!("bad git oid: {e}")))
}

/// A 20-byte locator from a git [`gix::ObjectId`] (sha1). A non-20-byte id is a typed error.
fn oid_to_array(oid: impl AsRef<gix::oid>) -> Result<[u8; GIT_OID_LEN], GitstoreError> {
    oid.as_ref()
        .as_bytes()
        .try_into()
        .map_err(|_| GitstoreError::Gix("git oid is not 20 bytes (sha1 expected)".into()))
}
