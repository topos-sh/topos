//! End-to-end tests of the pull/apply engine against a real git store + fixture plane responses (no
//! HTTP). These exercise the release-blocker invariants: never-clobber-draft, the served record IS the
//! sync target (a server-restore backward move applies too), the crash-after-swap heal, mis-scoped
//! rejection, go-back/resume, and the first receive landing on the bare sweep (a manifest row IS the
//! consent), all through the public `ops::pull` entry point. Integrity is the content-addressed version
//! id re-verified on apply — there is no signature and no anti-rollback floor.

mod merge;
mod merge_records;
mod pull;
mod revert_fanout;
mod rig;
