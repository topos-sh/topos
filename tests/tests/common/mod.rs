//! Shared harness for the loopback e2e tests: provision a fresh, migrated per-test Postgres database.
//!
//! Each e2e (HERO / follow / contribute) runs a **blocking `ureq` client** on the test thread alongside a
//! live `axum` server on a self-owned **multi-thread** runtime, so it cannot use `#[sqlx::test]` — that
//! macro drives the test on a **current-thread** runtime, where the blocking client would starve the
//! server and deadlock. Instead each test calls [`provision_pg`] inside its own runtime to get a `PgPool`
//! over a fresh database, then builds `Authority::from_pool(pool, git_root, large_root)`.
//!
//! The provisioned databases are left behind on the target Postgres — the CI / local build Postgres is
//! disposable (a container), and dropping a database while its pool still holds connections is racy.

use std::sync::atomic::{AtomicU32, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, Executor, PgConnection, PgPool};

/// Create a uniquely-named database on the `$DATABASE_URL` server, run the production migrations
/// ([`plane_store::MIGRATOR`]) on it, and return a pool over it. Panics with a clear message if
/// `DATABASE_URL` is unset or the server is unreachable (the e2e suite requires a Postgres, exactly like
/// the in-crate `#[sqlx::test]` suite).
pub(crate) async fn provision_pg() -> PgPool {
    static N: AtomicU32 = AtomicU32::new(0);
    let base = std::env::var("DATABASE_URL")
        .expect("the e2e suite requires DATABASE_URL to point at a Postgres");
    let opts: PgConnectOptions = base
        .parse()
        .expect("DATABASE_URL must be a valid Postgres connection string");
    let name = format!(
        "topos_e2e_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );

    // CREATE the fresh database on the base connection (identifier-quoted; the name is ASCII-safe anyway).
    let mut admin = PgConnection::connect_with(&opts)
        .await
        .expect("connect to the base Postgres database");
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .expect("create the per-test database");
    admin.close().await.ok();

    // Connect to the fresh database and apply the SAME migrations production runs.
    let pool = PgPoolOptions::new()
        .connect_with(opts.database(&name))
        .await
        .expect("connect to the per-test database");
    plane_store::MIGRATOR
        .run(&pool)
        .await
        .expect("migrate the per-test database");
    pool
}
