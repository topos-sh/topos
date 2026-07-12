//! The Postgres backend — **the only place raw `sqlx` lives.**
//!
//! The pool, every transaction, and every `query!` are private to this module; no `sqlx` type appears
//! in any signature crossing the module boundary. Every operation takes the validated id newtypes plus
//! the data it needs and returns plain domain values (`CommitId`, small enums, `bool`) — so a caller
//! outside this module can never run an unbound query, hold a transaction, or read a bare object. That
//! privacy boundary is the access-rule enforcement mechanism.
//!
//! **The write-transaction discipline is the trust spine.** SQLite's `BEGIN IMMEDIATE` took a global
//! writer lock, so every read-then-write inside a write transaction was automatically safe against a
//! concurrent writer. Postgres does not serialize writers, so the `run_serializable!` macro re-establishes it
//! with `SERIALIZABLE` isolation + a bounded retry on a serialization failure — the one and only write
//! entrypoint (there is no `begin`-returns-a-`Transaction` form to misuse).

use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::authority::PoolConfig;
use crate::error::{AuthorityError, Result};

/// The bounded number of times the `run_serializable!` macro re-runs a write closure that hit a
/// serialization failure (SQLSTATE `40001`) or deadlock (`40P01`). Contention on one team's skill is
/// low, so a small cap suffices; exceeding it is a transient-infra fault (a 500), never a receipted
/// terminal (which would poison replay).
pub(in crate::db) const MAX_TXN_RETRIES: u32 = 10;

/// Full-jitter backoff bounds for those retries: attempt `n` sleeps a uniform-random duration in
/// `[0, min(BASE << (n-1), CAP)]`. Immediate re-runs retry in lockstep — two writers that collided
/// once keep colliding on every synchronized attempt and can burn the whole budget in milliseconds
/// (observed as `standup` racing-test flakes under full-suite load); a randomized pause desynchronizes
/// them so one commits while the other waits. Full jitter maximizes spread, the happy path never
/// sleeps, and the worst single pause (250ms) stays far below any client timeout.
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

/// Convert a stored 32-byte BLOB to a fixed array, or an integrity fault when the width is wrong (the
/// schema's `CHECK (octet_length(…) = 32)` forbids it; a violation is store corruption). The ONE shared
/// definition — every `mod db` sibling imports this one.
pub(in crate::db) fn blob32(bytes: &[u8]) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| AuthorityError::integrity(BadBlobWidth))
}

/// The Postgres-backed authority database: one connection pool. Reads run autocommit at the pool's
/// default `READ COMMITTED`; every write goes through [`run_serializable`](Self::run_serializable),
/// which opens `SERIALIZABLE` per-transaction (it reverts on commit — no connection-global default).
#[derive(Debug)]
pub(crate) struct Db {
    pool: PgPool,
    /// Test-observable count of serialization-failure retries the runner has performed — lets a
    /// concurrency test PROVE the MVCC re-proof actually fired (an outcome assertion alone passes even a
    /// fully-serialized schedule that never raised `40001`). Never read in production.
    #[cfg(test)]
    retry_count: std::sync::atomic::AtomicU64,
}

/// The one write-transaction entrypoint — run `$body` (which uses the bound `$tx`, e.g.
/// `foo_txn(&mut $tx, …).await`) as ONE `SERIALIZABLE` transaction, retrying on a serialization failure
/// (SQLSTATE `40001`) or deadlock (`40P01`) — raised by a statement OR by `COMMIT` (Postgres's deferred SSI
/// checks can fire only at commit) — up to [`MAX_TXN_RETRIES`], with a [full-jitter pause](retry_backoff)
/// before each re-run so concurrent writers desynchronize instead of colliding in lockstep.
///
/// This replaces SQLite's global-writer-lock `BEGIN IMMEDIATE`: Postgres does not serialize writers, so
/// every read-then-write invariant SQLite got for free — the whole-`(epoch,seq)` CAS, the last-owner
/// write-skew guard, the object-presence fence, op-id idempotency — is re-proven here by SSI + retry.
///
/// It is a **macro, not a generic `fn`**, so the retry loop inlines into each method and that method's
/// future stays a concrete `async fn` future whose `Send` is auto-derived: a generic `run_serializable<F:
/// AsyncFnMut…>` cannot bound the closure future `Send` on stable (the `CallRefFuture` GAT is unstable),
/// and axum's `Handler` requires `Send`.
///
/// `$body` MUST be re-runnable: it borrows its inputs and performs no non-DB side effect (all filesystem
/// work — ingest, migrate, the deleting-wait, the GC unlink — stays OUTSIDE), so an aborted attempt rolls
/// back with no durable trace and the retry re-runs against fresh committed state (`now`/`created_at` are
/// captured by the caller before the loop, so a retried receipt is byte-stable — signing/HMAC are
/// deterministic). Cap-exceeded is [`AuthorityError::Internal`] (a 500), never a receipted terminal (which
/// would replay forever); the caller's `op_id` retry re-drives under lower contention.
///
/// `$body` must **return** its `Result` (every call site is the `.await` of an extracted `_txn`/`_run` fn,
/// so a `?` inside that fn returns at the fn boundary into an `Err` value the macro's `Err` arm can classify
/// and roll back). A bare `?` written directly in `$body` would instead return from the *enclosing* `async
/// fn`, bypassing the rollback + retry arms — so the body hands the macro a `Result`, never `?`-propagates.
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
    /// lock (session-level, so a session-mode pool is required — a plain pooled connection is one).
    pub(crate) async fn connect(database_url: &str, pool_config: &PoolConfig) -> Result<Self> {
        let mut opts = PgPoolOptions::new();
        if let Some(max) = pool_config.max_connections {
            opts = opts.max_connections(max);
        }
        if let Some(acquire) = pool_config.acquire_timeout {
            opts = opts.acquire_timeout(acquire);
        }
        // Opt-in session GUCs, applied on every pooled connection. Only a `Some` timeout emits a `SET`, so an
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
        Self {
            pool,
            #[cfg(test)]
            retry_count: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The connection pool — PRIVATE to `mod db`, so the child `lifecycle`/`seed` submodules reach it
    /// for their autocommit pool reads while it stays unreachable elsewhere in the crate (no `sqlx`
    /// handle ever crosses the module boundary).
    fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Bump the test-visible retry counter (a no-op in production). Called by the
    /// [`run_serializable!`](crate::db::run_serializable) macro on each retry.
    #[inline]
    fn note_retry(&self) {
        #[cfg(test)]
        self.retry_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// The number of serialization-failure retries the runner has performed since open — the concurrency
    /// tests read it to prove a `40001` actually occurred and was retried (not merely that the outcome
    /// matched). Test / `test-fixtures` only.
    #[cfg(test)]
    pub(crate) fn retry_count(&self) -> u64 {
        self.retry_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Test-only: DETERMINISTICALLY force the `run_serializable!` macro to retry exactly one Postgres
    /// serialization failure, so a test can assert [`retry_count`](Self::retry_count) advanced by one
    /// (a live-concurrency assertion is scheduler-dependent — an accidentally-serialized schedule reaches
    /// the same outcome without ever raising `40001`). On the FIRST attempt the body commits a conflicting
    /// bump to the same `current` row via a SEPARATE autocommit connection, so this transaction's own
    /// UPDATE serialization-fails (SQLSTATE `40001`); the macro rolls back and re-runs with the injector
    /// cleared, and the second attempt commits. Requires a `current` row for `(ws, skill)`.
    #[cfg(test)]
    pub(crate) async fn test_force_one_serialization_retry(
        &self,
        ws: &crate::id::WorkspaceId,
        skill: &crate::id::SkillId,
    ) -> Result<()> {
        let inject = std::sync::atomic::AtomicBool::new(true);
        let ws_s = ws.as_str();
        let skill_s = skill.as_str();
        run_serializable!(
            self,
            tx,
            async {
                // Read the row inside the serializable snapshot.
                sqlx::query!(
                    "SELECT seq FROM current WHERE workspace_id = $1 AND skill_id = $2",
                    ws_s,
                    skill_s
                )
                .fetch_one(&mut *tx)
                .await
                .map_err(AuthorityError::internal)?;
                if inject.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    // A concurrent COMMITTED writer bumps the SAME row on another connection, so our
                    // pending write below serialization-fails.
                    sqlx::query!(
                        "UPDATE current SET updated_at = updated_at + 1 \
                         WHERE workspace_id = $1 AND skill_id = $2",
                        ws_s,
                        skill_s
                    )
                    .execute(self.pool())
                    .await
                    .map_err(AuthorityError::internal)?;
                }
                // Our own write to the same row — conflicts with the injected bump on the first attempt.
                sqlx::query!(
                    "UPDATE current SET updated_at = updated_at + 1 \
                     WHERE workspace_id = $1 AND skill_id = $2",
                    ws_s,
                    skill_s
                )
                .execute(&mut *tx)
                .await
                .map_err(AuthorityError::internal)?;
                Ok(())
            }
            .await
        )
    }
}

/// True if `e` (a raw `sqlx::Error`, e.g. from `tx.commit()`) is a transient class the SERIALIZABLE runner
/// retries: a serialization failure (`40001`) or deadlock (`40P01`), OR a unique-violation (`23505`) on one
/// of the CONVERGENT constraints — the two idempotency-key PKs, the one-open-proposal index, the catalog's
/// registration keys, and the create-workspace request ledger (`genesis_requests_pkey`: two racing creates of the SAME request both
/// pass the replay probe; the loser's ledger INSERT aborts here, and its retry's probe replays the winner's
/// workspace — the same shape as the `workspace_events` idempotency slot).
///
/// All three restore what SQLite's global writer lock gave for free: a losing racer of an idempotent write
/// converges to the winner's outcome instead of a spurious `Internal`/500. Two same-`op_id` writes can BOTH
/// pass their replay-miss before either commits, and the loser's receipt/event INSERT then hits
/// `op_receipts_pkey`/`workspace_events_pkey`; retrying makes the re-run's replay find the now-committed
/// receipt and return it byte-identically. The `proposals_one_open` case is the same shape: two proposers of
/// the SAME candidate on the SAME base (the index key is `(workspace_id, skill_id, commit_id, base_epoch,
/// base_seq) WHERE status='open'`, and `commit_id` is content-derived, so a violation ONLY ever means
/// "identical candidate, identical base, already open") both pass `propose_arm`'s `read_open_proposal`
/// is-none guard, and the loser's `INSERT INTO proposals` hits the partial-unique index; retrying re-runs the
/// arm against the winner's now-committed row, so `read_open_proposal` finds it, the duplicate insert is
/// skipped, and the loser returns the SAME idempotent `NEEDS_REVIEW` the sequential re-propose guard produces
/// (the retry always resolves, since the index key pins the exact row the re-read then finds). Scoped to
/// exactly these three so an ordinary unique violation (a `roster_pkey`, a real integrity duplicate) still
/// surfaces, never a silent retry.
pub(in crate::db) fn is_serialization_failure_sqlx(e: &sqlx::Error) -> bool {
    let Some(db) = e.as_database_error() else {
        return false;
    };
    match db.code().as_deref() {
        Some("40001" | "40P01") => true,
        Some("23505") => matches!(
            db.constraint(),
            Some(
                "op_receipts_pkey"
                    | "workspace_events_pkey"
                    | "proposals_one_open"
                    | "genesis_requests_pkey"
                    // The catalog registration two concurrent GENESIS publishes both attempt: both
                    // probe the catalog before either commits, so the loser's INSERT aborts on the
                    // skill-id PK (same skill) or the name index (two skills minting one name).
                    // Retrying re-runs `register_publish`'s probe against the winner's committed row
                    // and takes the already-registered arm (or answers the typed NameTaken) — the
                    // same convergence the idempotency slots get, instead of a spurious 500.
                    | "catalog_pkey"
                    | "catalog_by_name"
            )
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

/// A stored commit-id BLOB was not exactly 32 bytes (the schema CHECK should forbid this).
#[derive(Debug, thiserror::Error)]
#[error("stored content id is not 32 bytes")]
struct BadBlobWidth;

// The custody raw-SQL twins (the pointer-move transaction, the object-lifecycle fence, the contribute-table
// SQL, the receipt machinery, and the restore epoch bump) — grouped under `db/custody/`.
pub(crate) mod custody;

// The directory raw-SQL twins (enrollment issuance, governance + admin-claim, and the two web-session
// directory legs' SQL) — grouped under `db/directory/`.
pub(crate) mod directory;

pub(crate) use custody::lifecycle::{ClaimOutcome, InstallOutcome, Location, ObjectStatus};

// Gated under `test` OR the `test-fixtures` feature: `--tests` still compiles its `query!`s (so the sqlx
// `prepare --check -- --tests` drift gate keeps covering them), and `--features test-fixtures` exposes them
// to the feature-gated `Authority` shims a downstream test crate drives.
#[cfg(any(test, feature = "test-fixtures"))]
mod seed;

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

    /// The SERIALIZABLE runner retries a `23505` on a CONVERGENT constraint — the two idempotency-key PKs
    /// (`op_receipts` / `workspace_events`) and the one-open-proposal partial-unique (`proposals_one_open`) —
    /// plus the catalog's registration keys (two racing GENESIS publishes both probe the catalog before
    /// either commits) — so a concurrent same-`op_id` receipt sibling, same-candidate proposer, or
    /// co-genesis publisher converges to the winner's outcome rather than surfacing a 500 — but NEVER on an
    /// ordinary unique violation (e.g. `skill_follows_pkey`), a real business/integrity duplicate that must
    /// not be silently retried. Proven against real Postgres duplicate-key errors. Raw `sqlx::query` (not
    /// `query!`), so it adds nothing to the `.sqlx` drift surface.
    #[sqlx::test]
    async fn only_convergent_unique_violations_are_retryable(pool: PgPool) {
        // op_receipts_pkey → a unique violation the runner treats as retryable.
        let receipt = "INSERT INTO op_receipts \
            (workspace_id, actor, op_id, command, skill_id, expected_epoch, expected_seq, \
             outcome, created_at) \
            VALUES ('w_a', 'dk', 'op1', 'publish', 's_a', 1, 1, 'OK', '2026-06-30T00:00:00Z')";
        sqlx::query(receipt)
            .execute(&pool)
            .await
            .expect("first receipt insert");
        let dup = sqlx::query(receipt)
            .execute(&pool)
            .await
            .expect_err("a duplicate op_id receipt must raise a unique violation");
        assert!(
            is_serialization_failure_sqlx(&dup),
            "a 23505 on op_receipts_pkey must be retryable"
        );

        // proposals_one_open → the one-open-proposal partial-unique. A concurrent same-candidate/same-base
        // propose races past `propose_arm`'s `read_open_proposal` is-none guard; the loser's INSERT hits this
        // index and, on retry, converges to the winner's NEEDS_REVIEW — so it is retryable, like the receipt
        // PKs. (The FK targets `skill_commit`, so seed the provenance row first.)
        let provenance = "INSERT INTO skill_commit (workspace_id, commit_id, skill_id, bundle_digest) \
            VALUES ('w_a', decode(repeat('ab', 32), 'hex'), 's_a', decode(repeat('cd', 32), 'hex'))";
        sqlx::query(provenance)
            .execute(&pool)
            .await
            .expect("skill_commit provenance insert");
        let open_proposal_op1 = "INSERT INTO proposals \
            (workspace_id, id, skill_id, commit_id, base_commit_id, base_epoch, base_seq, status, \
             proposer, created_at) \
            VALUES ('w_a', 'op1', 's_a', decode(repeat('ab', 32), 'hex'), decode(repeat('ef', 32), 'hex'), \
             1, 1, 'open', 'p_a', '2026-06-30T00:00:00Z')";
        let open_proposal_op2 = "INSERT INTO proposals \
            (workspace_id, id, skill_id, commit_id, base_commit_id, base_epoch, base_seq, status, \
             proposer, created_at) \
            VALUES ('w_a', 'op2', 's_a', decode(repeat('ab', 32), 'hex'), decode(repeat('ef', 32), 'hex'), \
             1, 1, 'open', 'p_a', '2026-06-30T00:00:00Z')";
        sqlx::query(open_proposal_op1)
            .execute(&pool)
            .await
            .expect("first open proposal insert");
        let dup = sqlx::query(open_proposal_op2)
            .execute(&pool)
            .await
            .expect_err(
                "a second open proposal of the same candidate+base must violate proposals_one_open",
            );
        assert!(
            is_serialization_failure_sqlx(&dup),
            "a 23505 on proposals_one_open must be retryable (it converges to the winner's NEEDS_REVIEW)"
        );

        // genesis_requests_pkey → the create-workspace request ledger: a racing same-request loser converges
        // to the winner's workspace on retry, so it is retryable.
        let genesis = "INSERT INTO genesis_requests (request_sha256, owner_principal, workspace_id, created_at) \
            VALUES (decode(repeat('aa', 32), 'hex'), 'o@x.com', 'w_a', '2026-07-03T00:00:00Z')";
        sqlx::query(genesis)
            .execute(&pool)
            .await
            .expect("first genesis request insert");
        let dup = sqlx::query(genesis)
            .execute(&pool)
            .await
            .expect_err("a duplicate genesis request must raise a unique violation");
        assert!(
            is_serialization_failure_sqlx(&dup),
            "a 23505 on genesis_requests_pkey must be retryable"
        );

        // skill_follows_pkey → an ordinary unique violation the runner must NOT retry (a person's own
        // subscription row: a real duplicate, never a convergent idempotency key). The catalog keys, by
        // contrast, ARE convergent now — two racing genesis publishes both probe the catalog before either
        // commits, and the loser must converge on the winner's registration rather than 500.
        let catalog = "INSERT INTO catalog (workspace_id, skill_id, name, status, created_at) \
            VALUES ('w_a', 's_a', 's-a', 'active', 'seed')";
        sqlx::query(catalog)
            .execute(&pool)
            .await
            .expect("catalog row for the follow FK");
        let follow = "INSERT INTO skill_follows (workspace_id, principal, skill_id, created_at) \
            VALUES ('w_a', 'bob@x.io', 's_a', 'seed')";
        sqlx::query(follow)
            .execute(&pool)
            .await
            .expect("first follow insert");
        let dup = sqlx::query(follow)
            .execute(&pool)
            .await
            .expect_err("a duplicate follow row must raise a unique violation");
        assert!(
            !is_serialization_failure_sqlx(&dup),
            "a 23505 on an ordinary constraint must NOT be retried"
        );
    }
}
