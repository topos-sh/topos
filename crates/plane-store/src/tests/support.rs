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

    /// A fixture with an overridden size-routing threshold + reject cap — for the offload tests,
    /// which force placement (a tiny threshold routes ordinary test bytes to the large store).
    pub(crate) fn with_large_limits(
        pool: PgPool,
        tag: &str,
        threshold: u64,
        reject_cap: u64,
    ) -> Self {
        Self::build(pool, tag, Some((threshold, reject_cap)))
    }

    fn build(pool: PgPool, tag: &str, limits: Option<(u64, u64)>) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-ps-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let mut authority = Authority::from_pool(pool, &dir.join("stores"), &dir.join("large"))
            .expect("open authority");
        if let Some((threshold, reject_cap)) = limits {
            authority = authority.with_large_limits(threshold, reject_cap);
        }
        Self { dir, authority }
    }

    /// The fixture's temp root (for tests that reach into the physical stores).
    pub(crate) fn dir(&self) -> &PathBuf {
        &self.dir
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
