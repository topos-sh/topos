//! The crate's one public error type.
//!
//! The privacy boundary extends to errors: no `sqlx` or git-store type appears in any public
//! shape. Internal faults carry a **boxed** source (the chain is preserved for diagnostics) but
//! their `Display` is generic, so a wire layer maps the *variant* and never echoes internals.

/// The result of an authorized authority operation.
pub type Result<T> = core::result::Result<T, AuthorityError>;

/// A failure of an authorized authority operation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthorityError {
    /// The requested object is not available to this principal. This single variant covers **every**
    /// not-entitled and not-found case — the caller is not rostered for the skill, the skill does not
    /// reach the object, or the object does not exist — so they are byte-for-byte indistinguishable.
    /// A caller can never probe which skills or objects exist. (The skill-scoped read surfaces this
    /// as a 404, never a 403.)
    #[error("not found")]
    NotFound,

    /// An upload was refused: the uploading principal is not rostered for the target skill, or the
    /// candidate would adopt a commit already owned by another bundle.
    #[error("denied")]
    Denied,

    /// A malformed identifier reached the boundary (rejected before any authority logic ran).
    #[error("invalid identifier: {0}")]
    InvalidId(#[from] crate::id::IdError),

    /// An uploaded bundle violated the canonical rules (a rejected path/mode/collision), referenced a
    /// parent the workspace does not hold, or carried an id that does not match the recomputed bytes.
    #[error("rejected upload: {0}")]
    RejectedUpload(String),

    /// The authority's own provenance says an object is reachable, but the object store cannot produce
    /// or verify its bytes — a divergence between the database and the store (data corruption). It is
    /// reachable **only after** entitlement was already proven on a read, so surfacing it leaks
    /// nothing about existence; it must never be folded into [`Self::NotFound`].
    #[error("object store integrity fault")]
    Integrity(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// An internal failure (a database or runtime fault). Its `Display` is generic; the source chain
    /// is retained for server-side diagnostics only.
    #[error("internal store error")]
    Internal(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl AuthorityError {
    /// Wrap an internal error (e.g. a database fault) as [`AuthorityError::Internal`], preserving its
    /// source chain without naming its type in the public API.
    pub(crate) fn internal(e: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Internal(Box::new(e))
    }

    /// Wrap a provenance/store divergence as [`AuthorityError::Integrity`] (a corruption alarm on an
    /// already-authorized read), preserving its source chain.
    pub(crate) fn integrity(e: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Integrity(Box::new(e))
    }
}
