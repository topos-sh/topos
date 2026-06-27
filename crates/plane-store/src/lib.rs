//! `plane-store` — the server authority boundary.
//!
//! A crate **so that raw access is private.** It owns the plane's per-workspace SQL (raw `sqlx`, no
//! ORM) and per-workspace git-object storage, and it is the single place the **skill-scoped access
//! rule** is enforced. The pool, every transaction, every raw SQL statement, and every raw git-object
//! read are `pub(crate)`-private; the **only** public surface is authorized authority operations on
//! [`Authority`]. No code outside this crate can run an unbound query or read a bare object — that
//! privacy boundary *is* the enforcement mechanism (misuse-prevention by encapsulation, not isolation
//! of malicious same-process code).
//!
//! ## What this layer does today
//!
//! - **Per-workspace storage, hard tenant binding.** One git object store per workspace under a
//!   confined root, plus a SQLite database whose every row carries `workspace_id` and whose every query
//!   binds it. Cross-company is physically separate; isolation is the database binding, never the
//!   directory.
//! - **The skill-scoped object read** ([`Authority::read_object`]). One join authorizes on two
//!   independent facts — the caller is rostered for the skill, and some commit of that skill reaches
//!   the object — and yields a witness commit; the bytes are then read + re-verified from the store.
//!   Every not-entitled/not-found case returns one indistinguishable not-found; objects are never
//!   served by bare hash.
//! - **Full-tree upload + server rehash** ([`Authority::upload_candidate`]). The server recomputes
//!   every id from the uploaded bytes (no client id is trusted; no reference-by-id), applies the
//!   canonical rules, writes the objects, and records provenance + reachability only after an
//!   authoritative roster check — in one transaction. Dedup is invisible. No pointer is moved.
//! - **The cross-skill lineage predicate** ([`Authority::check_lineage`]) — built read-only here.
//!
//! ## Deliberately not here yet
//!
//! The object-lifecycle/garbage-collection fence, the pointer-move write (compare-and-set, the
//! in-process signer, durable receipts), the HTTP surface, identity/roster issuance, and Postgres are
//! later work. The `current` pointer table is created and seedable but never moved; nothing is signed.

mod authority;
mod error;
mod id;
mod lineage;
mod read;
mod sqlite;
mod upload;

// The object-lifecycle fence (quarantine ingest, lease-before-migrate, the 3-phase GC, recovery + janitor)
// is built behind the privacy boundary and exercised by the in-crate tests, but it is wired to NO public
// verb this increment — the pointer-move write that drives it lands later. So in a non-test build its
// `pub(crate)` ops are legitimately unreferenced; the lint stays active under `test`, where they are used.
#[cfg_attr(not(test), allow(dead_code))]
mod gc;
#[cfg_attr(not(test), allow(dead_code))]
mod lifecycle;

#[cfg(test)]
mod tests;

pub use authority::Authority;
pub use error::{AuthorityError, Result};
pub use id::{CommitId, IdError, ObjectId, OpId, Principal, SkillId, WorkspaceId};
pub use lineage::{CandidateCommit, LineageDecision};
pub use upload::{CandidateUpload, UploadReceipt, UploadedFile};

/// Re-exported for constructing [`UploadedFile`]s — the two regular-file modes the kernel allows.
pub use topos_core::digest::FileMode;
