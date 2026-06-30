//! `topos-e2e` — the workspace-level end-to-end integration tests.
//!
//! This crate holds **no production code**; the integration tests live under `tests/` (the HERO loopback in
//! `tests/hero.rs`). The library itself is an intentionally-empty anchor so the package is a real workspace
//! member that `cargo test --workspace` discovers and runs.
