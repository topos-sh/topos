//! `plane-store` — the vault's byte-custody boundary.
//!
//! A crate **so that raw access is private.** It owns the vault's per-workspace SQL (raw `sqlx`, no
//! ORM) and per-workspace object storage; the pool, every transaction, every raw SQL statement, and
//! every raw object read are `pub(crate)`-private, and the **only** public surface is the custody
//! operations on [`Authority`]. No code outside this crate can run an unbound query or read a bare
//! object — that privacy boundary is misuse-prevention by encapsulation.
//!
//! ## The trust shape
//!
//! The vault is PURE BYTE CUSTODY with ONE caller — the composing server fronting the product app —
//! and treats every request as **pre-authorized**: authorization, protection, and entitlement are
//! decided app-side, once. No identity, membership, or policy row lives here. Requests carry opaque
//! `(workspace_id, bundle_id, …)` strings plus `attribution` display strings the vault stores
//! verbatim; the vault validates SHAPE (charset/length), never meaning.
//!
//! ## What this layer does
//!
//! - **Per-workspace storage, hard tenant binding.** One git object store + one large-object store
//!   per workspace under confined roots, plus a Postgres schema whose every row carries
//!   `workspace_id` and whose every query binds it.
//! - **Content-addressed versions.** Every byte-introducing write ingests the full candidate and
//!   **recomputes every id from the bytes** (no client id trusted, no reference-by-id), dedups
//!   invisibly, installs under a promotion lease, and records the version in one serializable
//!   transaction. Committing an identical candidate twice converges on the same ids.
//! - **The generation-fenced pointer.** One movable `current` per bundle; every move is a
//!   compare-and-set on a single `generation` counter, with an idempotent-replay carve-out (a
//!   pointer already sitting exactly one past `expected` and naming the exact target answers
//!   success), so app-side retries after a crash are safe without vault-side receipts. Revert is a
//!   FORWARD commit; the pointer never moves backward.
//! - **Verified reads.** The pointer record, one object's bytes (served only through a bundle whose
//!   live version reaches it — never by bare hash — and re-verified against the id that named it),
//!   a version's metadata + file listing, and the first-parent log.
//! - **Purge + reclaim.** A byte purge tombstones the blobs unique to a version and stamps
//!   `purged_at` (the hash stays); bundle/workspace deletion reclaims rows + bytes on app
//!   instruction. The DB-authoritative object-lifecycle fence (quarantine → lease → install, the
//!   three-step mark-then-acquire GC, the recovery sweep, the quarantine janitor) keeps the
//!   filesystem strictly trailing the database. The GC pass / recovery / janitor are public ops the
//!   composing server MUST schedule — this library holds no scheduler. The server clock is one unit
//!   throughout: epoch **milliseconds**.

// Layout — the orchestration/SQL twin convention: `custody/` holds the orchestration OUTSIDE the
// transaction (filesystem work, candidate assembly; no SQL); `db/custody/` holds the raw-SQL half
// (the serializable transactions + pool reads; no `sqlx` type crosses out of `mod db`).
mod authority;
mod custody;
mod db;
mod error;
mod id;

#[cfg(test)]
mod tests;

// Internal path forwarding — the crate names these modules at the root.
pub(crate) use custody::{commit, gc, lifecycle, read, upload};

pub use authority::{Authority, DEFAULT_LOG_LIMIT, PoolConfig};
pub use commit::{BundleDeleteReport, CommittedVersion, PointerState, PurgeReport};
pub use error::{AuthorityError, LivePointer, Result};
pub use id::{BundleId, CommitId, IdError, ObjectId, OpId, WorkspaceId, validate_attribution};
pub use read::{CurrentInfo, LogEntry, VersionFile, VersionMeta};
pub use upload::{CandidateUpload, UploadedFile};

/// The embedded Postgres migration set, exposed for out-of-crate test harnesses (the loopback e2e
/// crates) that provision their own per-test database and migrate it before
/// [`Authority::from_pool`] — so they use the SAME migrations as production without a brittle
/// relative path. **Test-fixtures only** (this is the one place, besides `from_pool`, where a
/// `sqlx` type is public, and it is compiled out of every production build).
#[cfg(feature = "test-fixtures")]
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Re-exported for constructing [`UploadedFile`]s — the two regular-file modes the kernel allows.
pub use topos_core::digest::FileMode;
