//! The in-crate custody behavior suite — one named module per concern. Every test provisions its
//! own per-test database through `#[sqlx::test]` (which runs `./migrations`) and its own temp store
//! roots, so tests are hermetic and parallel-safe.

mod support;

mod commit_publish;
mod gc_purge;
mod reads;
