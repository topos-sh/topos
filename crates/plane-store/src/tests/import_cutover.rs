//! The `import-local` cutover, committed and repeatable: seed a REAL pre-object-store layout
//! (bare repos + refs via the gitstore fence primitives, raw large-store files, rows with NULL
//! de-git columns), then prove the import backfills + converts + verifies — and that the
//! fail-closed lanes (`PreImportRow` / the pre-import location) refuse typed until it has run.

use sqlx::PgPool;
use topos_core::digest::{self, FileMode};
use topos_core::identity::{self, Commit};
use topos_gitstore::{ImportFile, Store, codec};

use std::error::Error as _;

use crate::tests::support::NOW;
use crate::{Authority, AuthorityError, StoreConfig};

/// The typed refusal's SOURCE message (the public Display is deliberately generic; the cure —
/// `import-local` — rides the source chain).
fn source_message(e: &AuthorityError) -> String {
    e.source().map(ToString::to_string).unwrap_or_default()
}

const AUTHOR: &str = "Alice (test)";

/// One seeded file: `(path, mode, git_oid, object_id, size)`.
type SeededEntry = (String, FileMode, [u8; 20], [u8; 32], u64);

/// One seeded version's facts.
struct Seeded {
    version_hex: String,
    bundle_digest_hex: String,
    entries: Vec<SeededEntry>,
}

/// A scratch dir shaped like the OLD deployment: `git/<ws>` bare repos, `large/<ws>/objects/…`
/// raw files, and a store root the import targets (kept SEPARATE so the loose copy is real).
struct OldLayout {
    dir: std::path::PathBuf,
}

impl OldLayout {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("topos-import-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        Self { dir }
    }

    fn git_root(&self) -> std::path::PathBuf {
        self.dir.join("git")
    }

    fn large_root(&self) -> std::path::PathBuf {
        self.dir.join("large")
    }

    fn store_root(&self) -> std::path::PathBuf {
        self.dir.join("store")
    }

    fn authority(&self, pool: PgPool) -> Authority {
        Authority::from_pool(
            pool,
            &StoreConfig::Local {
                root: self.store_root(),
            },
            &self.dir.join("staging"),
        )
        .expect("authority")
    }

    /// The old repo's loose path for an oid.
    fn old_loose(&self, ws: &str, git_oid: &[u8; 20]) -> std::path::PathBuf {
        let hex = crate::store::hex_lower(git_oid);
        self.git_root()
            .join(ws)
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..])
    }

    /// The target store's loose path for an oid.
    fn store_loose(&self, ws: &str, git_oid: &[u8; 20]) -> std::path::PathBuf {
        let hex = crate::store::hex_lower(git_oid);
        self.store_root()
            .join(ws)
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..])
    }
}

impl Drop for OldLayout {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Commit one version into the OLD-layout repo through the fence primitives (stage → install →
/// commit_durable, exactly the pre-object-store write path), `skip_install` naming the entries
/// (by path) whose blobs stay OUT of git — the size-routed offload's on-disk shape.
fn seed_version(
    ws_repo: &std::path::Path,
    quarantine: &std::path::Path,
    files: &[(&str, FileMode, &[u8])],
    parent: Option<&Seeded>,
    message: &str,
    skip_install: &[&str],
) -> Seeded {
    if let Some(parent) = ws_repo.parent() {
        std::fs::create_dir_all(parent).expect("git root");
    }
    let main = Store::open(ws_repo)
        .or_else(|_| Store::init(ws_repo))
        .expect("main repo");
    let import: Vec<ImportFile<'_>> = files
        .iter()
        .map(|(path, mode, bytes)| ImportFile {
            path,
            mode: *mode,
            bytes,
        })
        .collect();
    let staged = Store::stage(quarantine, &import).expect("stage");
    let q = Store::open(quarantine).expect("open quarantine");
    for e in &staged.entries {
        if !skip_install.contains(&e.path.as_str()) {
            main.install_object_durable(&q, e.git_oid).expect("install");
        }
    }
    let parents: Vec<[u8; 32]> = parent
        .iter()
        .map(|p| {
            crate::id::CommitId::parse_hex(&p.version_hex)
                .expect("hex")
                .0
        })
        .collect();
    let version_id = identity::commit_id(&Commit {
        parents: &parents,
        tree: staged.bundle_digest,
        author: AUTHOR,
        message,
    })
    .expect("kernel id");
    let entry_refs: Vec<(&str, FileMode, [u8; 20])> = staged
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.mode, e.git_oid))
        .collect();
    main.commit_durable(
        version_id,
        &parents,
        &entry_refs,
        staged.bundle_digest,
        AUTHOR,
        message,
    )
    .expect("commit_durable");
    Seeded {
        version_hex: digest::to_hex(&version_id),
        bundle_digest_hex: digest::to_hex(&staged.bundle_digest),
        entries: staged
            .entries
            .iter()
            .map(|e| (e.path.clone(), e.mode, e.git_oid, e.object_id, e.size))
            .collect(),
    }
}

/// Insert the PRE-IMPORT rows a version carried: version (NULL locator/message), digest, edges,
/// and a presence row per entry (`location` as given; `skip_presence` entries get none).
#[allow(clippy::too_many_arguments)]
async fn seed_rows(
    pool: &PgPool,
    ws: &str,
    bundle: &str,
    v: &Seeded,
    parent: Option<&Seeded>,
    locations: &[(&str, &str)], // (path, location)
    purged: bool,
) {
    let parent_hex = parent.map(|p| p.version_hex.clone());
    sqlx::query(
        "INSERT INTO version (workspace_id, bundle_id, version_id, commit_id, first_parent, \
             author_display, created_at, purged_at) \
         VALUES ($1,$2,$3,$3,$4,$5, to_timestamp($6/1000.0), \
                 CASE WHEN $7 THEN to_timestamp($6/1000.0) END)",
    )
    .bind(ws)
    .bind(bundle)
    .bind(&v.version_hex)
    .bind(parent_hex)
    .bind(AUTHOR)
    .bind(NOW as f64)
    .bind(purged)
    .execute(pool)
    .await
    .expect("version row");
    sqlx::query(
        "INSERT INTO version_digest (workspace_id, bundle_id, version_id, bundle_digest) \
         VALUES ($1,$2,$3,$4)",
    )
    .bind(ws)
    .bind(bundle)
    .bind(&v.version_hex)
    .bind(&v.bundle_digest_hex)
    .execute(pool)
    .await
    .expect("digest row");
    for (path, _, git_oid, object_id, size) in &v.entries {
        sqlx::query(
            "INSERT INTO version_object (workspace_id, bundle_id, version_id, object_id) \
             VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING",
        )
        .bind(ws)
        .bind(bundle)
        .bind(&v.version_hex)
        .bind(object_id.as_slice())
        .execute(pool)
        .await
        .expect("edge row");
        if let Some((_, location)) = locations.iter().find(|(p, _)| p == path) {
            sqlx::query(
                "INSERT INTO object_presence (workspace_id, object_id, status, location, size, \
                     git_oid, status_updated_at) \
                 VALUES ($1,$2,'present',$3,$4,$5,$6) ON CONFLICT DO NOTHING",
            )
            .bind(ws)
            .bind(object_id.as_slice())
            .bind(location)
            .bind(i64::try_from(*size).expect("size"))
            .bind(git_oid.as_slice())
            .bind(NOW)
            .execute(pool)
            .await
            .expect("presence row");
        }
    }
}

async fn seed_pointer(pool: &PgPool, ws: &str, bundle: &str, v: &Seeded, generation: i64) {
    sqlx::query(
        "INSERT INTO current_pointer (workspace_id, bundle_id, version_id, generation, \
             moved_by_display, moved_at) VALUES ($1,$2,$3,$4,$5, to_timestamp($6/1000.0))",
    )
    .bind(ws)
    .bind(bundle)
    .bind(&v.version_hex)
    .bind(generation)
    .bind(AUTHOR)
    .bind(NOW as f64)
    .execute(pool)
    .await
    .expect("pointer row");
}

/// Write one raw large-store file at the OLD layout's sharded path.
fn seed_large_file(large_root: &std::path::Path, ws: &str, bytes: &[u8]) {
    let hex = digest::to_hex(&digest::sha256(bytes));
    let shard = large_root
        .join(ws)
        .join("objects")
        .join(&hex[0..2])
        .join(&hex[2..4]);
    std::fs::create_dir_all(&shard).expect("large shard");
    std::fs::write(shard.join(&hex), bytes).expect("large file");
}

/// The whole seeded world of the happy-path tests: v1 (all-git, PURGED later), v2 (mixed: one git
/// blob + one size-routed large blob), pointer at v2.
async fn seed_world(pool: &PgPool, layout: &OldLayout) -> (Seeded, Seeded, Vec<u8>) {
    let ws_repo = layout.git_root().join("w1");
    let q = layout.dir.join("q");
    let large_bytes: Vec<u8> = (0u32..40_000).flat_map(u32::to_le_bytes).collect();

    let v1 = seed_version(
        &ws_repo,
        &q.join("op1"),
        &[
            ("GUIDE.md", FileMode::Regular, b"guide one\n".as_slice()),
            ("scripts/run.sh", FileMode::Executable, b"#!/bin/sh\n"),
        ],
        None,
        "m1",
        &[],
    );
    let v2 = seed_version(
        &ws_repo,
        &q.join("op2"),
        &[
            ("GUIDE.md", FileMode::Regular, b"guide two\n".as_slice()),
            ("data/big.bin", FileMode::Regular, &large_bytes),
        ],
        Some(&v1),
        "m2",
        &["data/big.bin"], // offloaded: its blob never enters git
    );
    seed_large_file(&layout.large_root(), "w1", &large_bytes);

    seed_rows(
        pool,
        "w1",
        "b1",
        &v1,
        None,
        &[("GUIDE.md", "git"), ("scripts/run.sh", "git")],
        true, // purged — refs + skeleton survive; log still lists it
    )
    .await;
    seed_rows(
        pool,
        "w1",
        "b1",
        &v2,
        Some(&v1),
        &[("GUIDE.md", "git"), ("data/big.bin", "large-local")],
        false,
    )
    .await;
    seed_pointer(pool, "w1", "b1", &v2, 2).await;
    (v1, v2, large_bytes)
}

fn ws(s: &str) -> crate::WorkspaceId {
    crate::WorkspaceId::parse(s).expect("ws")
}

fn bundle(s: &str) -> crate::BundleId {
    crate::BundleId::parse(s).expect("bundle")
}

#[sqlx::test]
async fn the_import_backfills_converts_verifies_and_reruns_idempotently(pool: PgPool) {
    let layout = OldLayout::new("happy");
    let (v1, v2, large_bytes) = seed_world(&pool, &layout).await;
    let authority = layout.authority(pool.clone());

    let report = authority
        .import_local(&layout.git_root(), &layout.large_root())
        .await
        .expect("import runs");
    assert!(report.passed(), "failures: {:?}", report.failures);
    assert_eq!(report.workspaces.len(), 1);
    let w = &report.workspaces[0];
    assert!(w.loose_copied > 0, "a separate store root really copies");
    assert_eq!(w.large_converted, 1);
    assert_eq!(w.versions_backfilled, 2, "the PURGED row backfills too");

    // Every row — purged included — carries locator + message.
    let rows: Vec<(String, Option<Vec<u8>>, Option<String>)> = sqlx::query_as(
        "SELECT version_id, git_commit_oid, message FROM version WHERE workspace_id='w1'",
    )
    .fetch_all(&pool)
    .await
    .expect("rows");
    assert_eq!(rows.len(), 2);
    for (vid, oid, message) in &rows {
        assert!(oid.is_some(), "{vid} missing locator");
        let expected = if *vid == v1.version_hex { "m1" } else { "m2" };
        assert_eq!(message.as_deref(), Some(expected));
    }
    // The converted large blob's row flipped to the one live location.
    let stale: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM object_presence WHERE workspace_id='w1' AND location <> 'git'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(stale, 0);

    // The framed large blob landed in BOTH stores — the target AND the old git root (rollback).
    let large_oid = codec::blob_git_oid(&large_bytes).expect("oid");
    assert!(layout.store_loose("w1", &large_oid).is_file());
    assert!(
        layout.old_loose("w1", &large_oid).is_file(),
        "flipping back to the old local layout must stay a valid rollback"
    );

    // The serving reads answer whole: metadata, bytes (incl. the converted large blob), log.
    let v2_id = crate::CommitId::parse_hex(&v2.version_hex).expect("hex");
    let meta = authority
        .read_version(&ws("w1"), &bundle("b1"), v2_id)
        .await
        .expect("read_version");
    assert_eq!(meta.files.len(), 2);
    assert_eq!(meta.message, "m2");
    let got = authority
        .read_object(
            &ws("w1"),
            &bundle("b1"),
            crate::ObjectId(digest::sha256(&large_bytes)),
        )
        .await
        .expect("large blob serves");
    assert_eq!(got, large_bytes);
    let log = authority
        .log(&ws("w1"), &bundle("b1"), 10)
        .await
        .expect("log");
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].message, "m2");
    assert_eq!(log[1].message, "m1");
    assert!(log[1].purged_at_ms.is_some(), "the purged hop stays listed");

    // Idempotent rerun: PASS again, nothing left to backfill.
    let rerun = authority
        .import_local(&layout.git_root(), &layout.large_root())
        .await
        .expect("rerun");
    assert!(rerun.passed(), "failures: {:?}", rerun.failures);
    assert_eq!(rerun.workspaces[0].versions_backfilled, 0);
}

#[sqlx::test]
async fn a_swapped_ref_fails_the_identity_verification(pool: PgPool) {
    let layout = OldLayout::new("swap");
    // Two INDEPENDENT genesis versions in one bundle — both internally valid commits.
    let ws_repo = layout.git_root().join("w1");
    let q = layout.dir.join("q");
    let va = seed_version(
        &ws_repo,
        &q.join("opA"),
        &[("A.md", FileMode::Regular, b"alpha\n".as_slice())],
        None,
        "mA",
        &[],
    );
    let vb = seed_version(
        &ws_repo,
        &q.join("opB"),
        &[("B.md", FileMode::Regular, b"beta\n".as_slice())],
        None,
        "mB",
        &[],
    );
    seed_rows(&pool, "w1", "b1", &va, None, &[("A.md", "git")], false).await;
    seed_rows(&pool, "w1", "b1", &vb, None, &[("B.md", "git")], false).await;
    seed_pointer(&pool, "w1", "b1", &vb, 1).await;

    // The corruption: va's ref now points at vb's commit — a DIFFERENT, internally-valid commit.
    let refs = layout.git_root().join("w1/refs/topos/versions");
    let va_commit = std::fs::read_to_string(refs.join(&va.version_hex)).expect("va ref");
    let vb_commit = std::fs::read_to_string(refs.join(&vb.version_hex)).expect("vb ref");
    std::fs::write(refs.join(&va.version_hex), &vb_commit).expect("swap");

    let authority = layout.authority(pool.clone());
    let report = authority
        .import_local(&layout.git_root(), &layout.large_root())
        .await
        .expect("import runs");
    assert!(
        !report.passed(),
        "a swapped ref must FAIL the import, not ride through"
    );
    assert!(
        report
            .failures
            .iter()
            .any(|f| f.contains(&va.version_hex) && f.contains("DIFFERENT version id")),
        "the failure names the swapped version AND the typed cause: {:?}",
        report.failures
    );
    // The bad ref never touched the row: no poisoned locator to clean up by hand.
    assert_eq!(
        locator_hex(&pool, "w1", &va.version_hex).await,
        None,
        "a failed identity check leaves the row unbackfilled"
    );

    // The cure the docs name — fix the ref, rerun — is the whole cure.
    std::fs::write(refs.join(&va.version_hex), &va_commit).expect("restore");
    let rerun = authority
        .import_local(&layout.git_root(), &layout.large_root())
        .await
        .expect("rerun runs");
    assert!(rerun.passed(), "failures: {:?}", rerun.failures);
    assert_eq!(
        locator_hex(&pool, "w1", &va.version_hex).await.as_deref(),
        Some(va_commit.trim())
    );
}

#[sqlx::test]
async fn a_poisoned_row_is_repaired_by_a_rerun_from_a_proven_ref(pool: PgPool) {
    let layout = OldLayout::new("repair");
    let ws_repo = layout.git_root().join("w1");
    let q = layout.dir.join("q");
    let va = seed_version(
        &ws_repo,
        &q.join("opA"),
        &[("A.md", FileMode::Regular, b"alpha\n".as_slice())],
        None,
        "mA",
        &[],
    );
    let vb = seed_version(
        &ws_repo,
        &q.join("opB"),
        &[("B.md", FileMode::Regular, b"beta\n".as_slice())],
        None,
        "mB",
        &[],
    );
    seed_rows(&pool, "w1", "b1", &va, None, &[("A.md", "git")], false).await;
    seed_rows(&pool, "w1", "b1", &vb, None, &[("B.md", "git")], false).await;
    seed_pointer(&pool, "w1", "b1", &vb, 1).await;
    let refs = layout.git_root().join("w1/refs/topos/versions");
    let va_commit = std::fs::read_to_string(refs.join(&va.version_hex)).expect("va ref");
    let vb_commit = std::fs::read_to_string(refs.join(&vb.version_hex)).expect("vb ref");

    let authority = layout.authority(pool.clone());
    let first = authority
        .import_local(&layout.git_root(), &layout.large_root())
        .await
        .expect("import runs");
    assert!(first.passed(), "failures: {:?}", first.failures);

    // What an earlier, failed run could have left behind: va's row pointing at vb's commit with
    // vb's message — a poisoned locator the ref (correct all along) disagrees with.
    sqlx::query(
        "UPDATE version SET git_commit_oid = decode($3, 'hex'), message = 'poison' \
         WHERE workspace_id = $1 AND version_id = $2",
    )
    .bind("w1")
    .bind(&va.version_hex)
    .bind(vb_commit.trim())
    .execute(&pool)
    .await
    .expect("poison");

    let rerun = authority
        .import_local(&layout.git_root(), &layout.large_root())
        .await
        .expect("rerun runs");
    assert!(rerun.passed(), "failures: {:?}", rerun.failures);
    assert_eq!(
        rerun.workspaces[0].versions_backfilled, 1,
        "the repair counts"
    );
    assert_eq!(
        locator_hex(&pool, "w1", &va.version_hex).await.as_deref(),
        Some(va_commit.trim()),
        "the proven ref wins over the poisoned locator"
    );
    let message: Option<String> = sqlx::query_scalar(
        "SELECT message FROM version WHERE workspace_id = $1 AND version_id = $2",
    )
    .bind("w1")
    .bind(&va.version_hex)
    .fetch_one(&pool)
    .await
    .expect("row");
    assert_eq!(
        message.as_deref(),
        Some("mA"),
        "the message is the ref's commit's"
    );
}

/// A row's commit locator as lowercase hex (`None` when unbackfilled).
async fn locator_hex(pool: &PgPool, ws: &str, version_hex: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT encode(git_commit_oid, 'hex') FROM version \
         WHERE workspace_id = $1 AND version_id = $2",
    )
    .bind(ws)
    .bind(version_hex)
    .fetch_one(pool)
    .await
    .expect("row")
}

#[sqlx::test]
async fn pre_import_reads_fail_closed_typed_and_the_import_is_the_cure(pool: PgPool) {
    let layout = OldLayout::new("closed");
    let (_v1, v2, large_bytes) = seed_world(&pool, &layout).await;
    let authority = layout.authority(pool);
    let v2_id = crate::CommitId::parse_hex(&v2.version_hex).expect("hex");

    // BEFORE the import: metadata and log fail closed on the NULL de-git columns, and the
    // pre-import 'large-local' location refuses the byte read — each a typed Integrity naming
    // the cure, never a NotFound and never a wrong answer.
    let e = authority
        .read_version(&ws("w1"), &bundle("b1"), v2_id)
        .await
        .expect_err("pre-import metadata must refuse");
    assert!(matches!(e, AuthorityError::Integrity(_)));
    assert!(
        source_message(&e).contains("import-local"),
        "names the cure: {e:?}"
    );
    let e = authority
        .log(&ws("w1"), &bundle("b1"), 10)
        .await
        .expect_err("pre-import log must refuse");
    assert!(matches!(e, AuthorityError::Integrity(_)));
    assert!(
        source_message(&e).contains("import-local"),
        "names the cure: {e:?}"
    );
    let e = authority
        .read_object(
            &ws("w1"),
            &bundle("b1"),
            crate::ObjectId(digest::sha256(&large_bytes)),
        )
        .await
        .expect_err("a pre-import location must refuse the byte read");
    assert!(matches!(e, AuthorityError::Integrity(_)));
    assert!(
        source_message(&e).contains("import-local"),
        "names the cure: {e:?}"
    );

    // The cure, then every lane serves.
    let report = authority
        .import_local(&layout.git_root(), &layout.large_root())
        .await
        .expect("import");
    assert!(report.passed(), "failures: {:?}", report.failures);
    authority
        .read_version(&ws("w1"), &bundle("b1"), v2_id)
        .await
        .expect("metadata serves after the import");
    authority
        .log(&ws("w1"), &bundle("b1"), 10)
        .await
        .expect("log serves after the import");
    assert_eq!(
        authority
            .read_object(
                &ws("w1"),
                &bundle("b1"),
                crate::ObjectId(digest::sha256(&large_bytes)),
            )
            .await
            .expect("bytes serve after the import"),
        large_bytes
    );
}
