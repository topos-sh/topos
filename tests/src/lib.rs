//! `topos-e2e` — the workspace-level end-to-end integration tests.
//!
//! This crate holds **no production code**; the integration tests live under `tests/` — the loopback-HTTP
//! e2e suites (`hero.rs`, `hero_claude.rs`, `follow_e2e.rs`, `contribute_e2e.rs`) plus their shared
//! Postgres-provisioning harness (`common/`). The library itself is an intentionally-empty anchor so the
//! package is a real workspace member that `cargo test --workspace` discovers and runs.
