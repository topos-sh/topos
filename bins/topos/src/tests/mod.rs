//! The multi-file test suites (unit tests stay inline in their production modules as
//! `#[cfg(test)] mod tests`). Each file here is a cross-module suite over the client's public seams:
//! the crash/durability gate, the follow/enrollment flow, the pull/apply sync engine, and the verbs.

mod auth;
mod builtin_skill;
mod durability;
mod follow;
mod manifest_reconcile;
mod placement_breadth;
mod publish_autoadd;
mod seams;
mod subscribe;
mod sync;
mod verbs;
mod verbs_b;
