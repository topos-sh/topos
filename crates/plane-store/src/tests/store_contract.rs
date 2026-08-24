//! The store-seam contract suite — ONE behavioral contract, run over every backend the vault can
//! sit on: `InMemory`, `LocalFileSystem` (the self-host default, plus the exact-key-shape pin),
//! MinIO (any S3-compatible endpoint; env-gated), and live R2/S3 (env-gated). Plus the seam-level
//! delete-bound test: a hung delete resolves `Ambiguous` within the hard wall-clock bound.
//!
//! No database — the seam is byte-custody only.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::StreamExt as _;
use object_store::ObjectStore;

use crate::id::WorkspaceId;
use crate::store::{DeleteOutcome, PlaneStore, REMOTE_DELETE_TIMEOUT_MS, StoreConfig};

fn ws(s: &str) -> WorkspaceId {
    WorkspaceId::parse(s).expect("workspace id")
}

/// A real loose frame (the only content the vault ever stores): `(git_oid, zlib_bytes)`.
fn loose(bytes: &[u8]) -> ([u8; 20], Vec<u8>) {
    let obj = topos_gitstore::codec::encode_blob(bytes).expect("encode");
    (obj.git_oid, obj.zlib_bytes)
}

/// The one contract every backend must satisfy.
async fn contract(store: &PlaneStore) {
    let (w1, w2) = (ws("cw1"), ws("cw2"));
    let (oid_a, zlib_a) = loose(b"alpha bytes");
    let (oid_b, zlib_b) = loose(b"beta bytes");

    // Absent reads answer None/false, never an error.
    assert!(
        store
            .get_loose(&w1, &oid_a)
            .await
            .expect("get absent")
            .is_none()
    );
    assert!(!store.exists(&w1, &oid_a).await.expect("exists absent"));

    // put → get round-trips the exact bytes; exists flips.
    store
        .put_loose(&w1, &oid_a, zlib_a.clone())
        .await
        .expect("put");
    assert_eq!(
        store
            .get_loose(&w1, &oid_a)
            .await
            .expect("get")
            .expect("present"),
        zlib_a
    );
    assert!(store.exists(&w1, &oid_a).await.expect("exists"));

    // A re-put of the identical object is idempotent (self-healing, never destructive).
    store
        .put_loose(&w1, &oid_a, zlib_a.clone())
        .await
        .expect("re-put");
    assert_eq!(
        store
            .get_loose(&w1, &oid_a)
            .await
            .expect("get")
            .expect("present"),
        zlib_a
    );

    // Workspace prefixes are disjoint: the same object in two workspaces is two keys.
    store
        .put_loose(&w2, &oid_a, zlib_a.clone())
        .await
        .expect("put ws2");
    store
        .put_loose(&w2, &oid_b, zlib_b.clone())
        .await
        .expect("put ws2 b");
    let mut w2_keys = store.list_workspace(&w2).await.expect("list ws2");
    w2_keys.sort();
    assert_eq!(w2_keys.len(), 2, "ws2 holds exactly its own two objects");
    assert_eq!(store.list_workspace(&w1).await.expect("list ws1").len(), 1);

    // The single-attempt delete removes; re-deleting an already-gone object is Removed (the
    // idempotent recovery re-run).
    assert!(matches!(
        store.delete_loose_single_attempt(&w1, &oid_a).await,
        DeleteOutcome::Removed
    ));
    assert!(!store.exists(&w1, &oid_a).await.expect("gone"));
    assert!(matches!(
        store.delete_loose_single_attempt(&w1, &oid_a).await,
        DeleteOutcome::Removed
    ));
    // ws2's copy is untouched.
    assert!(store.exists(&w2, &oid_a).await.expect("ws2 untouched"));

    // Workspace prefix bulk-delete clears exactly that workspace.
    let removed = store
        .delete_workspace_prefix(&w2)
        .await
        .expect("prefix delete");
    assert_eq!(removed, 2);
    assert!(store.list_workspace(&w2).await.expect("list").is_empty());
}

#[tokio::test]
async fn contract_holds_on_in_memory() {
    let store = PlaneStore::from_object_store(Arc::new(object_store::memory::InMemory::new()));
    contract(&store).await;
}

#[tokio::test]
async fn contract_holds_on_local_filesystem_at_the_bare_repo_key_shape() {
    let dir = std::env::temp_dir().join(format!(
        "topos-store-contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let store = PlaneStore::open(&StoreConfig::Local { root: dir.clone() }).expect("open");
    contract(&store).await;

    // The key-shape pin: a put lands the loose file at the EXACT bare-repo path
    // `<root>/<ws>/objects/<aa>/<38-hex>` — what makes a pre-existing git root readable in place.
    let (oid, zlib) = loose(b"key shape");
    let w = ws("shape");
    store.put_loose(&w, &oid, zlib.clone()).await.expect("put");
    let hex = crate::store::hex_lower(&oid);
    let on_disk = dir
        .join("shape")
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    assert_eq!(
        std::fs::read(&on_disk).expect("loose file at the bare-repo path"),
        zlib
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The MinIO leg — the SAME contract against a real S3-compatible endpoint. Gated on
/// `TOPOS_TEST_MINIO_URL` (e.g. `http://127.0.0.1:9010`) + `TOPOS_TEST_MINIO_{ACCESS_KEY,SECRET_KEY,BUCKET}`,
/// because CI has no MinIO service; when unset it SKIPS with a loud printed warning.
#[tokio::test]
async fn contract_holds_on_minio() {
    let Ok(endpoint) = std::env::var("TOPOS_TEST_MINIO_URL") else {
        eprintln!(
            "WARNING: store contract suite SKIPPED its MinIO leg — set TOPOS_TEST_MINIO_URL \
             (+ TOPOS_TEST_MINIO_ACCESS_KEY / TOPOS_TEST_MINIO_SECRET_KEY / TOPOS_TEST_MINIO_BUCKET) \
             to run it against a real S3-compatible endpoint"
        );
        return;
    };
    let store = PlaneStore::open(&StoreConfig::S3 {
        endpoint,
        bucket: std::env::var("TOPOS_TEST_MINIO_BUCKET").unwrap_or_else(|_| "topos-test".into()),
        access_key_id: std::env::var("TOPOS_TEST_MINIO_ACCESS_KEY")
            .unwrap_or_else(|_| "topostest".into()),
        secret_access_key: std::env::var("TOPOS_TEST_MINIO_SECRET_KEY")
            .unwrap_or_else(|_| "topostest123".into()),
        region: "us-east-1".into(),
    })
    .expect("open minio store");
    contract(&store).await;
}

/// The live S3/R2 leg — the SAME contract against a real bucket. Gated on the full
/// `TOPOS_TEST_S3_{ENDPOINT,BUCKET,ACCESS_KEY_ID,SECRET_ACCESS_KEY,REGION}` set; when unset it
/// SKIPS with a loud printed warning (region defaults to "auto", the R2 value).
#[tokio::test]
async fn contract_holds_on_live_s3() {
    let (Ok(endpoint), Ok(bucket), Ok(access_key_id), Ok(secret_access_key)) = (
        std::env::var("TOPOS_TEST_S3_ENDPOINT"),
        std::env::var("TOPOS_TEST_S3_BUCKET"),
        std::env::var("TOPOS_TEST_S3_ACCESS_KEY_ID"),
        std::env::var("TOPOS_TEST_S3_SECRET_ACCESS_KEY"),
    ) else {
        eprintln!(
            "WARNING: store contract suite SKIPPED its live-S3/R2 leg — set \
             TOPOS_TEST_S3_{{ENDPOINT,BUCKET,ACCESS_KEY_ID,SECRET_ACCESS_KEY[,REGION]}} to run it"
        );
        return;
    };
    let store = PlaneStore::open(&StoreConfig::S3 {
        endpoint,
        bucket,
        access_key_id,
        secret_access_key,
        region: std::env::var("TOPOS_TEST_S3_REGION").unwrap_or_else(|_| "auto".into()),
    })
    .expect("open live store");
    contract(&store).await;
}

// ── the delete-bound half of the GC invariant, at the seam ──────────────────────────────────

/// How the fault deleter treats the FIRST delete it sees.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FirstDeleteFault {
    /// Never resolves (a stuck remote) — the seam's hard timeout is what bounds it.
    Hang,
    /// Fails with a transport-shaped error (an ambiguous outcome: the backend may or may not have
    /// acted) — the fast fault for the DB-integration race replay.
    Error,
}

/// A deleter whose FIRST delete faults per `fault` — every other op passes through untouched.
/// The counters ride `Arc`s so the pass-through stream (which must be `'static`) can stamp
/// completions.
#[derive(Debug)]
pub(crate) struct FaultFirstDelete {
    pub(crate) inner: Arc<dyn ObjectStore>,
    pub(crate) fault: FirstDeleteFault,
    pub(crate) deletes_started: Arc<AtomicUsize>,
    pub(crate) deletes_completed: Arc<AtomicUsize>,
}

impl FaultFirstDelete {
    pub(crate) fn new(inner: Arc<dyn ObjectStore>, fault: FirstDeleteFault) -> Arc<Self> {
        Arc::new(Self {
            inner,
            fault,
            deletes_started: Arc::new(AtomicUsize::new(0)),
            deletes_completed: Arc::new(AtomicUsize::new(0)),
        })
    }
}

impl std::fmt::Display for FaultFirstDelete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FaultFirstDelete({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for FaultFirstDelete {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.inner.get_opts(location, options).await
    }

    /// The interception point: a single delete (`ObjectStoreExt::delete`) drives ONE
    /// `delete_stream` call, so faulting the first call faults the first delete. The fault stream
    /// never touches the backend; pass-through streams count each path the backend confirms.
    fn delete_stream(
        &self,
        locations: futures_util::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>
    {
        if self.deletes_started.fetch_add(1, Ordering::SeqCst) == 0 {
            return match self.fault {
                FirstDeleteFault::Hang => futures_util::stream::pending().boxed(),
                FirstDeleteFault::Error => futures_util::stream::once(async {
                    Err(object_store::Error::Generic {
                        store: "fault",
                        source: "injected transport fault on the first delete".into(),
                    })
                })
                .boxed(),
            };
        }
        let completed = Arc::clone(&self.deletes_completed);
        self.inner
            .delete_stream(locations)
            .map(move |item| {
                if item.is_ok() {
                    completed.fetch_add(1, Ordering::SeqCst);
                }
                item
            })
            .boxed()
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
    {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// A hung DELETE resolves `Ambiguous` within the hard bound — never blocks past it, never reports
/// `Removed` — and the abandoned attempt provably never reached the backend. Paused-clock, so the
/// real 15s bound is exercised without waiting it.
#[tokio::test(start_paused = true)]
async fn a_hung_delete_is_ambiguous_within_the_hard_bound_and_never_lands() {
    let inner: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    let gated = FaultFirstDelete::new(Arc::clone(&inner), FirstDeleteFault::Hang);
    let store = PlaneStore::from_object_stores(Arc::clone(&inner), gated.clone());
    let w = ws("hangws");
    let (oid, zlib) = loose(b"soon stuck");
    store.put_loose(&w, &oid, zlib).await.expect("put");

    let before = tokio::time::Instant::now();
    let outcome = store.delete_loose_single_attempt(&w, &oid).await;
    let waited = before.elapsed();
    assert!(matches!(outcome, DeleteOutcome::Ambiguous(_)));
    assert!(
        waited <= std::time::Duration::from_millis(REMOTE_DELETE_TIMEOUT_MS + 1000),
        "the delete resolved within the hard bound (took {waited:?})"
    );
    // The abandoned attempt never reached the backend: the object is still there, untouched.
    assert_eq!(gated.deletes_started.load(Ordering::SeqCst), 1);
    assert_eq!(gated.deletes_completed.load(Ordering::SeqCst), 0);
    assert!(store.exists(&w, &oid).await.expect("exists"));
}
