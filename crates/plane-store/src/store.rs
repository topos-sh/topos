//! The object-store seam — the ONE place the vault's bytes live behind.
//!
//! Every byte the vault holds is an **immutable zlib loose git object** in an object store, keyed
//! at the exact bare-repo loose path shape `<workspace>/objects/<aa>/<38-hex-remainder>` (the
//! object's own git OID). Two backends behind one trait ([`object_store`]): a local directory
//! (the self-host DEFAULT — over the existing git root, so a pre-existing store's loose objects
//! are already at their keys) and any S3-compatible endpoint (R2, MinIO).
//!
//! The seam is deliberately narrow: put / get / exists / delete, plus prefix-list + bulk-delete
//! used ONLY by workspace deletion and import verification. **Write policy: plain idempotent
//! put.** Keys are content-addressed on both lanes (sha256-identified file bytes framed as git
//! blobs; OID-keyed skeleton objects), so an overwrite writes the identical logical object — a
//! re-put is self-HEALING, never destructive — and no conditional-write support is required of a
//! backend (the weakest-backend rule; the atomic swap is the Postgres transaction).
//!
//! **Delete safety (the GC invariant):** the remote DELETE a GC issues runs on a dedicated
//! client with transport retries DISABLED (single attempt) and a hard request timeout, and the
//! call's total wall-clock is additionally bounded by [`REMOTE_DELETE_TIMEOUT_MS`] — strictly
//! below the recovery-staleness threshold — so a delete can never outlive the ownership token
//! that authorized it. An ambiguous failure (timeout, transport fault) reports
//! [`DeleteOutcome::Ambiguous`] and the caller leaves the row for a later pass: absence is never
//! finalized on ambiguity.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use object_store::path::Path as StorePath;
use object_store::{ObjectStore, ObjectStoreExt as _, PutPayload};

use crate::error::{AuthorityError, Result};
use crate::id::WorkspaceId;

/// The width of a git object id (SHA-1) — the seam's key unit.
const GIT_OID_LEN: usize = 20;

/// The hard bound on one GC DELETE call's total wall-clock (transport timeout AND an outer
/// `tokio::time::timeout`), in epoch milliseconds' unit. MUST stay strictly below the recovery
/// staleness threshold (`gc::RECOVERY_STALE_MS`, 60s — a compile-time assertion there pins the
/// ordering), so a delete issued under an ownership token is guaranteed dead before recovery may
/// hand that object to another actor.
pub(crate) const REMOTE_DELETE_TIMEOUT_MS: u64 = 15_000;

/// Which physical backend the vault's object store runs on. Built by the composing server from
/// its environment; the default (`local`) reuses the existing per-workspace git root, so a
/// self-host deployment needs zero new configuration.
#[derive(Debug, Clone)]
pub enum StoreConfig {
    /// A local directory (the self-host default). Keys resolve to real files at
    /// `<root>/<workspace>/objects/<aa>/<38-hex>` — the exact layout the pre-object-store bare
    /// repos used, so existing loose objects are already at their keys.
    Local { root: PathBuf },
    /// Any S3-compatible endpoint (R2, MinIO, AWS). `region` is `"auto"` for R2.
    S3 {
        endpoint: String,
        bucket: String,
        access_key_id: String,
        secret_access_key: String,
        region: String,
    },
}

/// What one single-attempt GC delete did.
#[derive(Debug)]
pub(crate) enum DeleteOutcome {
    /// The object is gone (deleted now, or already absent — idempotent).
    Removed,
    /// The attempt's outcome is unknown or it failed (timeout, transport fault, refused request).
    /// The caller must NOT finalize absence; the object stays `deleting` for a later pass.
    Ambiguous(String),
}

/// The vault's object store: one client for the ordinary ops (default retries), and a second,
/// retry-DISABLED, hard-timeout client used exclusively for the GC's single-attempt deletes.
#[derive(Debug, Clone)]
pub(crate) struct PlaneStore {
    ops: Arc<dyn ObjectStore>,
    deleter: Arc<dyn ObjectStore>,
    /// The local backend's root — present only for `StoreConfig::Local`, where the seam adds the
    /// fsync the backend itself omits (a durable install is what lets the DB flip `present`).
    local_root: Option<PathBuf>,
}

impl PlaneStore {
    /// Open the store `config` names. The local root is created if absent.
    pub(crate) fn open(config: &StoreConfig) -> Result<Self> {
        match config {
            StoreConfig::Local { root } => {
                std::fs::create_dir_all(root).map_err(AuthorityError::internal)?;
                let store = object_store::local::LocalFileSystem::new_with_prefix(root)
                    .map_err(AuthorityError::internal)?
                    // Prune directories a delete empties, so a reclaimed workspace does not
                    // accumulate empty shard-dir husks on disk.
                    .with_automatic_cleanup(true);
                let store: Arc<dyn ObjectStore> = Arc::new(store);
                Ok(Self {
                    ops: Arc::clone(&store),
                    // Local deletes are plain unlinks — no transport to retry, no timeout needed;
                    // one client serves both roles.
                    deleter: store,
                    local_root: Some(root.clone()),
                })
            }
            StoreConfig::S3 {
                endpoint,
                bucket,
                access_key_id,
                secret_access_key,
                region,
            } => {
                let builder = || {
                    object_store::aws::AmazonS3Builder::new()
                        .with_endpoint(endpoint.clone())
                        .with_bucket_name(bucket.clone())
                        .with_access_key_id(access_key_id.clone())
                        .with_secret_access_key(secret_access_key.clone())
                        .with_region(region.clone())
                        // A plain-HTTP endpoint is a local MinIO / test rig; TLS endpoints are
                        // unaffected by the flag.
                        .with_allow_http(endpoint.starts_with("http://"))
                };
                let ops = builder().build().map_err(AuthorityError::internal)?;
                // The delete client: retries OFF (a transport-level retry could re-issue a DELETE
                // after ownership moved) and a hard request timeout strictly below the recovery
                // staleness threshold. `with_client_options` REPLACES the builder's options
                // wholesale, so the allow-http flag must ride inside it.
                let deleter = builder()
                    .with_retry(object_store::RetryConfig {
                        max_retries: 0,
                        retry_timeout: Duration::from_millis(REMOTE_DELETE_TIMEOUT_MS),
                        ..Default::default()
                    })
                    .with_client_options(
                        object_store::ClientOptions::new()
                            .with_timeout(Duration::from_millis(REMOTE_DELETE_TIMEOUT_MS))
                            .with_allow_http(endpoint.starts_with("http://")),
                    )
                    .build()
                    .map_err(AuthorityError::internal)?;
                Ok(Self {
                    ops: Arc::new(ops),
                    deleter: Arc::new(deleter),
                    local_root: None,
                })
            }
        }
    }

    /// Wrap an already-built [`ObjectStore`] (both roles) — the injection seam for the in-memory
    /// contract suite and the delete-race fault rig. Test-only.
    #[cfg(test)]
    pub(crate) fn from_object_store(store: Arc<dyn ObjectStore>) -> Self {
        Self {
            ops: Arc::clone(&store),
            deleter: store,
            local_root: None,
        }
    }

    /// As [`Self::from_object_store`], with a distinct delete-role client (the fault rig wraps
    /// only the deleter).
    #[cfg(test)]
    pub(crate) fn from_object_stores(
        ops: Arc<dyn ObjectStore>,
        deleter: Arc<dyn ObjectStore>,
    ) -> Self {
        Self {
            ops,
            deleter,
            local_root: None,
        }
    }

    /// Whether this is the LOCAL backend rooted at exactly `path` — the import's in-place
    /// detection (keys are the old layout's paths, so the loose copy is a no-op).
    pub(crate) fn local_root_is(&self, path: &std::path::Path) -> bool {
        self.local_root.as_deref().is_some_and(|root| {
            let canon = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            canon(root) == canon(path)
        })
    }

    /// Store one loose object at its key — a plain idempotent put (an overwrite re-writes the
    /// identical logical object; see the module docs). On the local backend the landed file and
    /// its directory chain are fsynced, so "put returned" ⇒ durable on both backends.
    pub(crate) async fn put_loose(
        &self,
        ws: &WorkspaceId,
        git_oid: &[u8; GIT_OID_LEN],
        zlib_bytes: Vec<u8>,
    ) -> Result<()> {
        let key = loose_key(ws, git_oid);
        self.ops
            .put(&key, PutPayload::from(zlib_bytes))
            .await
            .map_err(AuthorityError::internal)?;
        if let Some(root) = &self.local_root {
            fsync_landed(root, ws, git_oid)?;
        }
        Ok(())
    }

    /// Fetch one loose object's zlib bytes. `None` = the key is absent (the caller decides
    /// whether that is a legitimate reclaim or an integrity divergence).
    pub(crate) async fn get_loose(
        &self,
        ws: &WorkspaceId,
        git_oid: &[u8; GIT_OID_LEN],
    ) -> Result<Option<Vec<u8>>> {
        match self.ops.get(&loose_key(ws, git_oid)).await {
            Ok(got) => {
                let bytes = got.bytes().await.map_err(AuthorityError::internal)?;
                Ok(Some(bytes.to_vec()))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(AuthorityError::internal(e)),
        }
    }

    /// Whether a loose object exists (a HEAD). An idempotency / re-materialize belt only — the
    /// database's `object_presence` row remains the sole presence authority.
    pub(crate) async fn exists(
        &self,
        ws: &WorkspaceId,
        git_oid: &[u8; GIT_OID_LEN],
    ) -> Result<bool> {
        match self.ops.head(&loose_key(ws, git_oid)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(AuthorityError::internal(e)),
        }
    }

    /// The GC unlink: ONE delete attempt on the retry-disabled client, bounded by
    /// [`REMOTE_DELETE_TIMEOUT_MS`] total wall-clock. Never errors — the tri-state outcome is the
    /// caller's fence input: `Removed` (incl. already-gone, the idempotent recovery re-run) may
    /// finalize absence; `Ambiguous` must not.
    pub(crate) async fn delete_loose_single_attempt(
        &self,
        ws: &WorkspaceId,
        git_oid: &[u8; GIT_OID_LEN],
    ) -> DeleteOutcome {
        let key = loose_key(ws, git_oid);
        let attempt = tokio::time::timeout(
            Duration::from_millis(REMOTE_DELETE_TIMEOUT_MS),
            self.deleter.delete(&key),
        )
        .await;
        match attempt {
            Ok(Ok(())) | Ok(Err(object_store::Error::NotFound { .. })) => DeleteOutcome::Removed,
            Ok(Err(e)) => DeleteOutcome::Ambiguous(format!("delete failed: {e}")),
            Err(_) => DeleteOutcome::Ambiguous(format!(
                "delete did not resolve within {REMOTE_DELETE_TIMEOUT_MS}ms"
            )),
        }
    }

    /// List every key under one workspace's prefix — used ONLY by workspace deletion and by
    /// import verification (never on a serving path).
    pub(crate) async fn list_workspace(&self, ws: &WorkspaceId) -> Result<Vec<StorePath>> {
        let prefix = StorePath::from(ws.as_str());
        let mut stream = self.ops.list(Some(&prefix));
        let mut keys = Vec::new();
        while let Some(meta) = stream.next().await {
            keys.push(meta.map_err(AuthorityError::internal)?.location);
        }
        Ok(keys)
    }

    /// Bulk-delete everything under one workspace's prefix (workspace deletion; the DB rows are
    /// already gone, so nothing reaches these keys). Returns the number of objects removed.
    pub(crate) async fn delete_workspace_prefix(&self, ws: &WorkspaceId) -> Result<usize> {
        let keys = self.list_workspace(ws).await?;
        let count = keys.len();
        let locations = futures_util::stream::iter(keys.into_iter().map(Ok)).boxed();
        let mut results = self.ops.delete_stream(locations);
        while let Some(deleted) = results.next().await {
            match deleted {
                Ok(_) | Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(AuthorityError::internal(e)),
            }
        }
        Ok(count)
    }
}

/// The loose key for a git OID: `<workspace>/objects/<aa>/<38-hex-remainder>` — the exact
/// bare-repo loose path shape, so the local backend over an existing git root finds pre-existing
/// objects at their keys, and any git tooling pointed at a synced bucket reads them natively.
fn loose_key(ws: &WorkspaceId, git_oid: &[u8; GIT_OID_LEN]) -> StorePath {
    let hex = hex_lower(git_oid);
    StorePath::from(format!(
        "{}/objects/{}/{}",
        ws.as_str(),
        &hex[..2],
        &hex[2..]
    ))
}

/// fsync a freshly-landed local object file + its directory chain up to the store root — the
/// durability the `LocalFileSystem` backend's stage-and-rename omits. "Put returned" must imply
/// bytes durably at their final path before the DB may flip the object `present`.
fn fsync_landed(
    root: &std::path::Path,
    ws: &WorkspaceId,
    git_oid: &[u8; GIT_OID_LEN],
) -> Result<()> {
    let hex = hex_lower(git_oid);
    let file = root
        .join(ws.as_str())
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    fsync_path(&file)?;
    let mut dir = file.parent();
    while let Some(d) = dir {
        fsync_path(d)?;
        if d == root {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}

/// fsync one path (file or directory); a not-found path is tolerated (a racing delete).
fn fsync_path(path: &std::path::Path) -> Result<()> {
    match std::fs::File::open(path) {
        Ok(f) => f.sync_all().map_err(AuthorityError::internal),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(AuthorityError::internal(e)),
    }
}

/// Lowercase hex of a byte slice.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}
