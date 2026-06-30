//! `topos` (client lib) — the local, accountless core: the `~/.topos/` sidecar, the crash-safe document
//! protocol, the bundle scanner, and the local verbs over the kernel + the embedded-git store.
//!
//! **The client is never an authority.** It depends on no SQL and no `plane-store` — it is a thin sync
//! tool. Every trust decision (the byte-exact digest, the commit id) is the kernel's; the sidecar only
//! stores and re-verifies.
//!
//! ## Durability
//!
//! All on-disk mutation goes through one fault-injectable `FsOps` seam, so the crash gate can
//! fail the Nth syscall and assert recovery. Documents are written atomically (temp → fsync → rename →
//! fsync-dir; never in place); a fresh `add` is staged in full and published with one directory rename,
//! so adoption is all-or-nothing on top of the per-document guarantee. The git objects a document refers
//! to are made durable **before** the document that names them.

mod app;
pub(crate) mod atomic;
pub(crate) mod cli;
pub(crate) mod config_io;
pub(crate) mod ctx;
pub(crate) mod doc;
pub(crate) mod enroll;
pub(crate) mod error;
pub(crate) mod fs_seam;
pub(crate) mod identity;
pub(crate) mod ids;
pub(crate) mod logfile;
pub(crate) mod materialize;
pub(crate) mod ops;
pub(crate) mod plane;
pub(crate) mod plane_http;
pub(crate) mod render;
pub(crate) mod scan;
pub(crate) mod sidecar;

#[cfg(test)]
mod durability_tests;
#[cfg(test)]
mod sync_tests;
#[cfg(test)]
mod verb_tests;

pub use app::run;
