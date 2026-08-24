//! Shared fixtures for the custody suite: a per-test authority over the injected `PgPool` (each
//! `#[sqlx::test]` provisions + migrates its own database) plus RAII temp store roots.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use sqlx::PgPool;
use topos_core::digest;

use crate::{
    Authority, BundleId, CandidateUpload, CommitId, FileMode, ObjectId, UploadedFile, WorkspaceId,
};

/// The fixed test clock (epoch ms).
pub(crate) const NOW: i64 = 1_700_000_000_000;

/// A temp dir + an open authority, cleaned up on drop (RAII, so a failing test still tidies).
pub(crate) struct Fixture {
    dir: PathBuf,
    pub authority: Authority,
}

impl Fixture {
    pub(crate) fn new(pool: PgPool, tag: &str) -> Self {
        Self::build(pool, tag, None)
    }

    /// A fixture with an overridden per-blob reject cap — for the cap-refusal tests.
    pub(crate) fn with_reject_cap(pool: PgPool, tag: &str, reject_cap: u64) -> Self {
        Self::build(pool, tag, Some(reject_cap))
    }

    fn build(pool: PgPool, tag: &str, reject_cap: Option<u64>) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-ps-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let mut authority = Authority::from_pool(
            pool,
            &crate::StoreConfig::Local {
                root: dir.join("stores"),
            },
            &dir.join("staging"),
        )
        .expect("open authority");
        if let Some(reject_cap) = reject_cap {
            authority = authority.with_reject_cap(reject_cap);
        }
        Self { dir, authority }
    }

    /// The fixture's temp root (for tests that reach into the physical store).
    pub(crate) fn dir(&self) -> &PathBuf {
        &self.dir
    }

    /// The on-disk path of one loose object in the fixture's LOCAL store — the exact bare-repo
    /// loose shape under `stores/<ws>/objects/…` (tests that assert physical presence/absence).
    pub(crate) fn loose_path(&self, ws: &str, git_oid: &[u8; 20]) -> PathBuf {
        let hex = crate::store::hex_lower(git_oid);
        self.dir
            .join("stores")
            .join(ws)
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..])
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(crate) fn ws(s: &str) -> WorkspaceId {
    WorkspaceId::parse(s).expect("workspace id")
}

pub(crate) fn bundle(s: &str) -> BundleId {
    BundleId::parse(s).expect("bundle id")
}

pub(crate) fn file(path: &str, bytes: &[u8]) -> UploadedFile {
    UploadedFile {
        path: path.to_owned(),
        mode: FileMode::Regular,
        bytes: bytes.to_vec(),
    }
}

pub(crate) fn object_id(bytes: &[u8]) -> ObjectId {
    ObjectId(digest::sha256(bytes))
}

/// A one-file candidate with a fixed attribution + message (deterministic version id).
pub(crate) fn candidate(path: &str, bytes: &[u8], parent: Option<CommitId>) -> CandidateUpload {
    CandidateUpload {
        files: vec![file(path, bytes)],
        parent,
        attribution: "Alice (test)".to_owned(),
        message: "test: candidate".to_owned(),
    }
}
