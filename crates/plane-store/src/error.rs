//! The crate's one public error type.
//!
//! The privacy boundary extends to errors: no `sqlx` or git-store type appears in any public
//! shape. Internal faults carry a **boxed** source (the chain is preserved for diagnostics) but
//! their `Display` is generic, so a wire layer maps the *variant* and never echoes internals.

use crate::id::CommitId;

/// The result of an authority operation.
pub type Result<T> = core::result::Result<T, AuthorityError>;

/// The live pointer state a [`AuthorityError::Conflict`] carries back — what the caller's retry
/// re-targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivePointer {
    /// The pointer's current generation.
    pub generation: u64,
    /// The version the pointer currently names.
    pub version_id: CommitId,
}

/// A failure of an authority operation. The vault has ONE pre-authorized caller (the app), so
/// these are protocol outcomes, not access decisions.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthorityError {
    /// The named thing does not exist here — one uniform variant for every read/target miss
    /// (unknown bundle, unknown version, unreachable object). Single caller, so no oracle
    /// discipline is needed beyond consistency; corruption is NEVER folded into this (see
    /// [`Self::Integrity`]).
    #[error("not found")]
    NotFound,

    /// A malformed identifier or attribution reached the boundary (rejected before any authority
    /// logic ran).
    #[error("invalid identifier: {0}")]
    InvalidId(#[from] crate::id::IdError),

    /// A candidate was refused: it violated the canonical rules (a rejected path/mode/collision),
    /// referenced a parent this bundle does not hold, exceeded the per-blob cap, carried a
    /// denylisted (purged) blob, or broke the same-bundle lineage rule.
    #[error("rejected candidate: {0}")]
    RejectedUpload(String),

    /// The pointer compare-and-set lost: the live pointer is not where the caller expected (and
    /// the idempotent-replay carve-out did not apply). Carries the live state so the caller can
    /// rebase; `None` means no pointer exists yet.
    #[error("pointer conflict")]
    Conflict(Option<LivePointer>),

    /// A revert/re-commit named a PURGED version — its bytes are gone by decision and may not be
    /// restored or re-introduced through this op.
    #[error("target version is purged")]
    TargetPurged,

    /// A purge named the version `current` points at — refused; move the pointer first.
    #[error("version is pointed at by current")]
    PointedAt,

    /// The vault's own bookkeeping says the bytes exist, but the store cannot produce or verify
    /// them — a divergence between the database and the store (data corruption). Never folded into
    /// [`Self::NotFound`].
    #[error("object store integrity fault")]
    Integrity(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// An internal failure (a database or runtime fault). Its `Display` is generic; the source
    /// chain is retained for server-side diagnostics only.
    #[error("internal store error")]
    Internal(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl AuthorityError {
    /// Wrap an internal error (e.g. a database fault) as [`AuthorityError::Internal`], preserving its
    /// source chain without naming its type in the public API.
    pub(crate) fn internal(e: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Internal(Box::new(e))
    }

    /// Wrap a bookkeeping/store divergence as [`AuthorityError::Integrity`] (a corruption alarm),
    /// preserving its source chain.
    pub(crate) fn integrity(e: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Integrity(Box::new(e))
    }
}
