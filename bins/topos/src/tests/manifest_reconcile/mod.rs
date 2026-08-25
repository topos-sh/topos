//! The RECONCILE over fakes (no HTTP): two UNBLENDED scopes, each converged on its own recipe.
//!
//! The global FILE is the machine's complete recipe (only its rows deliver — a feed row, written
//! by login on the first connection, is what makes a workspace's feed flow; with no file nothing
//! is demanded machine-wide, and it says so loudly when a workspace's feed is left out); an
//! `"off"` row withholds exactly its bundle; an explicit row's pin beats the feed's and a set's
//! version of the same identity; the NEAREST project file governs whole; the same bundle at both
//! scopes lands twice with two stores; git rows move only on an explicit update; a dropped row
//! cleans snapshot-first; and `--force` absorbs before it drops.

mod bare_name;
mod copies;
mod demand;
mod dest;
mod forge;
mod import;
mod lock;
mod migrate;
mod pick;
mod publish_scope;
mod rig;
mod scope;
mod scope_verbs;
mod sweep;
mod token;
mod write_path;
mod write_row;

pub(super) use rig::NoGovernance;
