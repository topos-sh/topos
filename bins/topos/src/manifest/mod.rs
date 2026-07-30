//! The MANIFEST model — the demand side of delivery.
//!
//! The server knows what a person is GIVEN (their feed: assignments minus declines, computed
//! server-side and served whole by the delivery route). A file knows what a folder TAKES:
//!
//! - the GLOBAL manifest (`~/.topos/topos.toml`) is the machine's personal recipe. Absent, the
//!   machine behaves exactly as if the file held one feed row per connected workspace; present,
//!   it is COMPLETE — only its rows deliver, and the feed flows iff a feed row says so.
//! - a PROJECT manifest (`<dir>/topos.toml`) is a repo fact, identical for every contributor.
//!   The NEAREST file covering the working directory wins WHOLE (walking up like git; no
//!   merging across ancestors).
//!
//! Every statement is positive; the only negatives are the web decline (server-side) and the
//! `"off"` row value (machine-local, global file only). Scopes are UNBLENDED — no cross-scope
//! shadowing or layer arithmetic.
//!
//! The submodules: [`keys`] — the joined-key reference grammar (`[bundles]` key shapes + the
//! CLI input sugar); [`document`] — the `topos.toml` parse + the format-preserving editor +
//! file birth; [`normal`] — the `fmt` normal form; [`scopes`] — scope discovery + the
//! partitioned plans the reconcile drives.

pub(crate) mod document;
pub(crate) mod keys;
pub(crate) mod normal;
pub(crate) mod scopes;

/// The manifest's one filename, at a project root (or any nested folder) and in `~/.topos/`.
pub(crate) const MANIFEST_FILE: &str = "topos.toml";
