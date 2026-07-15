//! The custody raw-SQL half — grouped by concern:
//!
//! - [`lifecycle`] — the fenced `object_presence` state machine, promotion leases, the `upload`
//!   staging bookkeeping, and tombstones (the GC fence).
//! - [`pointer`]   — the version/commit transaction and the generation-fenced pointer CAS, plus the
//!   purge and the bundle/workspace row reclaims.
//! - [`read`]      — the pool reads (the pointer record, version rows, reachability, the log joins).

pub(crate) mod lifecycle;
pub(crate) mod pointer;
pub(crate) mod read;
