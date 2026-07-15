//! The Postgres backend — **the only place raw `sqlx` lives.**
//!
//! The pool, every transaction, and every `query!` are private to this module; no `sqlx` type appears
//! in any signature crossing the module boundary. Every operation takes the validated id newtypes plus
//! the data it needs and returns plain domain values (`CommitId`, small enums, `bool`) — so a caller
//! outside this module can never run an unbound query, hold a transaction, or read a bare object. That
//! privacy boundary is the misuse-prevention mechanism.
//!
//! **The write-transaction discipline is the trust spine.** Postgres does not serialize writers, so the
//! `run_serializable!` macro establishes it with `SERIALIZABLE` isolation + a bounded retry on a
//! serialization failure — the one and only write entrypoint (there is no `begin`-returns-a-
//! `Transaction` form to misuse). Every read-then-write invariant (the generation CAS, the
//! object-presence fence, the purge's uniqueness scan) is re-proven by SSI + retry.

use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::authority::PoolConfig;
use crate::error::{AuthorityError, Result};

/// The bounded number of times the `run_serializable!` macro re-runs a write closure that hit a
/// serialization failure (SQLSTATE `40001`) or deadlock (`40P01`). Contention on one bundle is low,
/// so a small cap suffices; exceeding it is a transient-infra fault (a 500).
pub(in crate::db) const MAX_TXN_RETRIES: u32 = 10;

/// Full-jitter backoff bounds for those retries: attempt `n` sleeps a uniform-random duration in
/// `[0, min(BASE << (n-1), CAP)]`. Immediate re-runs retry in lockstep — two writers that collided
/// once keep colliding on every synchronized attempt and can burn the whole budget in milliseconds;
/// a randomized pause desynchronizes them so one commits while the other waits. Full jitter
/// maximizes spread, the happy path never sleeps, and the worst single pause (250ms) stays far
/// below any client timeout.
pub(in crate::db) const RETRY_BACKOFF_BASE_MS: u64 = 10;
pub(in crate::db) const RETRY_BACKOFF_CAP_MS: u64 = 250;

/// The pure half of the backoff: the jitter window's inclusive upper bound for 1-based `attempt`.
pub(in crate::db) fn retry_backoff_cap_ms(attempt: u32) -> u64 {
    // Doubling past the cap is pointless, so the shift saturates well before overflow (10 << 5 > 250).
    let shift = attempt.saturating_sub(1).min(5);
    (RETRY_BACKOFF_BASE_MS << shift).min(RETRY_BACKOFF_CAP_MS)
}

/// Sleep the jittered backoff before retry `attempt`. Entropy failure degrades to the full cap —
/// a spread loss, never a correctness loss (the retry itself is what re-proves the invariants).
pub(in crate::db) async fn retry_backoff(attempt: u32) {
    let cap_ms = retry_backoff_cap_ms(attempt);
    let mut buf = [0u8; 8];
    let delay_ms = match getrandom::getrandom(&mut buf) {
        Ok(()) => u64::from_le_bytes(buf) % (cap_ms + 1),
        Err(_) => cap_ms,
    };
    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
}

/// The Postgres-backed custody database: one connection pool. Reads run autocommit at the pool's
/// default `READ COMMITTED`; every write goes through the `run_serializable!` macro, which opens
/// `SERIALIZABLE` per-transaction.
#[derive(Debug)]
pub(crate) struct Db {
    pool: PgPool,
}

/// The one write-transaction entrypoint — run `$body` (which uses the bound `$tx`, e.g.
/// `foo_txn(&mut $tx, …).await`) as ONE `SERIALIZABLE` transaction, retrying on a serialization failure
/// (SQLSTATE `40001`) or deadlock (`40P01`) — raised by a statement OR by `COMMIT` (Postgres's deferred SSI
/// checks can fire only at commit) — up to [`MAX_TXN_RETRIES`], with a [full-jitter pause](retry_backoff)
/// before each re-run so concurrent writers desynchronize instead of colliding in lockstep.
///
/// It is a **macro, not a generic `fn`**, so the retry loop inlines into each method and that method's
/// future stays a concrete `async fn` future whose `Send` is auto-derived: a generic `run_serializable<F:
/// AsyncFnMut…>` cannot bound the closure future `Send` on stable (the `CallRefFuture` GAT is unstable),
/// and axum's `Handler` requires `Send`.
///
/// `$body` MUST be re-runnable: it borrows its inputs and performs no non-DB side effect (all filesystem
/// work — ingest, migrate, the deleting-wait, the GC unlink — stays OUTSIDE), so an aborted attempt rolls
/// back with no durable trace and the retry re-runs against fresh committed state. Cap-exceeded is
/// [`AuthorityError::Internal`] (a 500). A typed refusal returned as `Err` from `$body` (a
/// [`AuthorityError::Conflict`], a [`AuthorityError::PointedAt`]) rolls the transaction back — that
/// rollback IS the contract: a refused write leaves no durable trace.
///
/// `$body` must **return** its `Result` (every call site is the `.await` of an extracted `_txn` fn,
/// so a `?` inside that fn returns at the fn boundary into an `Err` value the macro's `Err` arm can
/// classify and roll back). A bare `?` written directly in `$body` would instead return from the
/// *enclosing* `async fn`, bypassing the rollback + retry arms.
macro_rules! run_serializable {
    ($self:expr, $tx:ident, $body:expr) => {{
        let mut __attempt: u32 = 0;
        loop {
            #[allow(unused_mut)]
            let mut $tx = $self
                .pool
                .begin_with("BEGIN ISOLATION LEVEL SERIALIZABLE")
                .await
                .map_err($crate::error::AuthorityError::internal)?;
            match $body {
                ::core::result::Result::Ok(__value) => match $tx.commit().await {
                    ::core::result::Result::Ok(()) => break ::core::result::Result::Ok(__value),
                    ::core::result::Result::Err(__e) => {
                        if $crate::db::is_serialization_failure_sqlx(&__e)
                            && __attempt < $crate::db::MAX_TXN_RETRIES
                        {
                            __attempt += 1;
                            $self.note_retry();
                            $crate::db::retry_backoff(__attempt).await;
                        } else {
                            break ::core::result::Result::Err(
                                $crate::error::AuthorityError::internal(__e),
                            );
                        }
                    }
                },
                ::core::result::Result::Err(__e) => {
                    let _ = $tx.rollback().await;
                    if $crate::db::is_serialization_failure(&__e)
                        && __attempt < $crate::db::MAX_TXN_RETRIES
                    {
                        __attempt += 1;
                        $self.note_retry();
                        $crate::db::retry_backoff(__attempt).await;
                    } else {
                        break ::core::result::Result::Err(__e);
                    }
                }
            }
        }
    }};
}

impl Db {
    /// Open a pool for `database_url` and apply the embedded migrations. sqlx's `Migrator` takes a
    /// `pg_advisory_lock` for the duration of the run, so this is multi-replica-safe with no hand-rolled
    /// lock.
    pub(crate) async fn connect(database_url: &str, pool_config: &PoolConfig) -> Result<Self> {
        let mut opts = PgPoolOptions::new();
        if let Some(max) = pool_config.max_connections {
            opts = opts.max_connections(max);
        }
        if let Some(acquire) = pool_config.acquire_timeout {
            opts = opts.acquire_timeout(acquire);
        }
        // Opt-in connection GUCs, applied on every pooled connection. Only a `Some` timeout emits a `SET`, so an
        // unset one inherits the server default (a long legitimate whole-bundle render is never capped unless
        // the operator opts in). The values are plane-controlled integers formatted into the statement — never
        // client input — so the string build carries no injection surface.
        let statement_ms = duration_millis(pool_config.statement_timeout);
        let lock_ms = duration_millis(pool_config.lock_timeout);
        let idle_ms = duration_millis(pool_config.idle_in_transaction_timeout);
        if statement_ms.or(lock_ms).or(idle_ms).is_some() {
            opts = opts.after_connect(move |conn, _meta| {
                Box::pin(async move {
                    if let Some(ms) = statement_ms {
                        sqlx::query(&format!("SET statement_timeout = {ms}"))
                            .execute(&mut *conn)
                            .await?;
                    }
                    if let Some(ms) = lock_ms {
                        sqlx::query(&format!("SET lock_timeout = {ms}"))
                            .execute(&mut *conn)
                            .await?;
                    }
                    if let Some(ms) = idle_ms {
                        sqlx::query(&format!("SET idle_in_transaction_session_timeout = {ms}"))
                            .execute(&mut *conn)
                            .await?;
                    }
                    Ok(())
                })
            });
        }
        let pool = opts
            .connect(database_url)
            .await
            .map_err(AuthorityError::internal)?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(AuthorityError::internal)?;
        Ok(Self::wrap(pool))
    }

    /// Wrap an already-open pool WITHOUT migrating — the injection seam for `#[sqlx::test]`, which
    /// provisions a fresh per-test database, runs `./migrations` on it, and hands us the pool. Test /
    /// `test-fixtures` only (an external e2e harness provisions its own per-test database the same way).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub(crate) fn from_pool(pool: PgPool) -> Self {
        Self::wrap(pool)
    }

    fn wrap(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The connection pool — PRIVATE to `mod db`, so the child submodules reach it for their
    /// autocommit pool reads while it stays unreachable elsewhere in the crate (no `sqlx` handle
    /// ever crosses the module boundary).
    fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The retry hook the `run_serializable!` macro calls on each re-run — a tracing breadcrumb, so
    /// a contended deployment can see the MVCC re-proof firing.
    #[inline]
    fn note_retry(&self) {
        tracing::debug!("serializable write retried after a serialization failure");
    }
}

/// True if `e` (a raw `sqlx::Error`, e.g. from `tx.commit()`) is a transient class the SERIALIZABLE runner
/// retries: a serialization failure (`40001`) or deadlock (`40P01`), OR a unique-violation (`23505`) on one
/// of the CONVERGENT constraints:
///
/// - `version_pkey` — two racing commits of the IDENTICAL candidate both pass the exists-probe
///   before either lands; the id is content-derived, so the collision only ever means "the same
///   version"; the loser's retry finds the winner's row and converges to the idempotent success.
/// - `current_pointer_pkey` — two racing genesis publishes both observe "no pointer"; the loser's
///   retry re-reads the winner's row and answers the typed CONFLICT (or the idempotent replay).
///
/// Scoped to exactly these so an ordinary unique violation (a genuine bug) still surfaces, never a
/// silent retry.
pub(in crate::db) fn is_serialization_failure_sqlx(e: &sqlx::Error) -> bool {
    let Some(db) = e.as_database_error() else {
        return false;
    };
    match db.code().as_deref() {
        Some("40001" | "40P01") => true,
        Some("23505") => matches!(
            db.constraint(),
            Some("version_pkey" | "current_pointer_pkey")
        ),
        _ => false,
    }
}

/// True if `e` is an [`AuthorityError::Internal`] wrapping such a serialization failure. The query
/// helpers box `sqlx::Error` into `Internal` (`error.rs`), so the runner must downcast the boxed source
/// to recover the SQLSTATE — a raw string match on the `Display` would be brittle.
pub(in crate::db) fn is_serialization_failure(e: &AuthorityError) -> bool {
    let AuthorityError::Internal(src) = e else {
        return false;
    };
    src.downcast_ref::<sqlx::Error>()
        .is_some_and(is_serialization_failure_sqlx)
}

/// Whole milliseconds for a Postgres `SET <timeout> = <ms>` GUC, saturating rather than wrapping (a duration
/// beyond `u64::MAX` ms is clamped). A sub-millisecond non-zero duration floors to 0 — which Postgres reads
/// as "disabled" — but callers pass whole seconds, so that is not reachable in practice.
fn duration_millis(d: Option<Duration>) -> Option<u64> {
    d.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

// The custody raw-SQL: the object-lifecycle fence, the version/pointer transaction, and the reads.
pub(crate) mod custody;

pub(crate) use custody::lifecycle::{AcquireOutcome, InstallOutcome, Location, ObjectStatus};

#[cfg(test)]
mod retry_backoff_tests {
    use super::{MAX_TXN_RETRIES, RETRY_BACKOFF_CAP_MS, retry_backoff_cap_ms};

    /// The jitter window doubles from the base and saturates at the cap — and the whole budget's
    /// worst-case pause stays bounded (no attempt may stall a write beyond the cap).
    #[test]
    fn backoff_window_doubles_then_saturates() {
        assert_eq!(retry_backoff_cap_ms(1), 10);
        assert_eq!(retry_backoff_cap_ms(2), 20);
        assert_eq!(retry_backoff_cap_ms(3), 40);
        assert_eq!(retry_backoff_cap_ms(5), 160);
        assert_eq!(retry_backoff_cap_ms(6), 250);
        for attempt in 6..=MAX_TXN_RETRIES + 1 {
            assert_eq!(retry_backoff_cap_ms(attempt), RETRY_BACKOFF_CAP_MS);
        }
        // Degenerate input (the macro always passes >= 1) still yields a sane window.
        assert_eq!(retry_backoff_cap_ms(0), 10);
    }
}

#[cfg(test)]
mod retry_classification_tests {
    use sqlx::PgPool;

    use super::is_serialization_failure_sqlx;

    /// The SERIALIZABLE runner retries a `23505` ONLY on the convergent constraints (`version_pkey`,
    /// `current_pointer_pkey`) — an ordinary unique violation must surface, never silently retry.
    /// Proven against real Postgres duplicate-key errors. Raw `sqlx::query` (not `query!`), so it
    /// adds nothing to the `.sqlx` drift surface.
    #[sqlx::test]
    async fn only_convergent_unique_violations_are_retryable(pool: PgPool) {
        // version_pkey → retryable (a racing identical commit converges).
        let version = "INSERT INTO version (workspace_id, bundle_id, version_id, commit_id, author_display) \
            VALUES ('w1', 'b1', 'v1', 'v1', 'alice')";
        sqlx::query(version).execute(&pool).await.expect("insert");
        let dup = sqlx::query(version)
            .execute(&pool)
            .await
            .expect_err("duplicate version must raise a unique violation");
        assert!(
            is_serialization_failure_sqlx(&dup),
            "a 23505 on version_pkey must be retryable"
        );

        // current_pointer_pkey → retryable (a racing genesis converges to the typed conflict).
        let pointer = "INSERT INTO current_pointer (workspace_id, bundle_id, version_id, moved_by_display) \
            VALUES ('w1', 'b1', 'v1', 'alice')";
        sqlx::query(pointer).execute(&pool).await.expect("insert");
        let dup = sqlx::query(pointer)
            .execute(&pool)
            .await
            .expect_err("duplicate pointer must raise a unique violation");
        assert!(
            is_serialization_failure_sqlx(&dup),
            "a 23505 on current_pointer_pkey must be retryable"
        );

        // An ordinary constraint (tombstones_pkey) is NOT retryable.
        let tomb = "INSERT INTO tombstones (workspace_id, blob_id, reason, at) \
            VALUES ('w1', decode(repeat('ab', 32), 'hex'), 'purge', 1)";
        sqlx::query(tomb).execute(&pool).await.expect("insert");
        let dup = sqlx::query(tomb)
            .execute(&pool)
            .await
            .expect_err("duplicate tombstone must raise a unique violation");
        assert!(
            !is_serialization_failure_sqlx(&dup),
            "a 23505 on an ordinary constraint must NOT be retried"
        );
    }
}
