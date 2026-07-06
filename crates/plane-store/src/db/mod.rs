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

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres};

use crate::authority::PoolConfig;
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, Principal, SkillId, WorkspaceId};

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

/// Which principal gate authorizes a read — the lane of the gate/reach split ([`Db::read_gate`]). The
/// reachability statements are lane-blind; the lane decides ONLY who may ask.
///
/// - [`SkillRoster`](Self::SkillRoster) — the device lane: a per-skill `roster` row exists (the
///   read-token scope's gate).
/// - [`WorkspaceMember`](Self::WorkspaceMember) — the web-session lane: a CONFIRMED `workspace_member`
///   row exists (skill-blind BY DESIGN — catalog visibility is workspace membership; the composing
///   caller's session verification is the authentication).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadLane {
    SkillRoster,
    WorkspaceMember,
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
        ws: &WorkspaceId,
        skill: &SkillId,
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

    /// The principal GATE of the read-authorization **gate/reach split**, dispatched by [`ReadLane`].
    /// Each authorization ([`Self::authorize_object_read`] / [`Self::authorize_version_read`] /
    /// [`Self::list_open_proposals`]) runs this gate and then ONE principal-free reachability statement,
    /// so every lane shares the identical reachability SQL and the lane decides only WHO may ask. Zero
    /// new SQL: each arm delegates to an existing probe. The gate and the reach are two statements — a
    /// principal revoked between them completes one in-flight read, the same accepted window as the
    /// authorize-then-fetch TOCTOU [`crate::read::read_object`] already re-guards (and re-runs on a miss).
    pub(in crate::db) async fn read_gate(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
        lane: ReadLane,
    ) -> Result<bool> {
        match lane {
            ReadLane::SkillRoster => roster_exists(&self.pool, ws, skill, principal).await,
            ReadLane::WorkspaceMember => {
                workspace_member_confirmed(&self.pool, ws, principal).await
            }
        }
    }

    /// A CONFIRMED `workspace_member` row exists — the session-read preamble's probe (the same predicate
    /// the [`ReadLane::WorkspaceMember`] gate runs; exposed separately so the preamble can deny BEFORE any
    /// per-skill work).
    pub(crate) async fn confirmed_member(
        &self,
        ws: &WorkspaceId,
        principal: &Principal,
    ) -> Result<bool> {
        workspace_member_confirmed(&self.pool, ws, principal).await
    }

    /// The object-read authorization: the lane's principal gate ([`Self::read_gate`]), then the
    /// principal-free reachability witness ([`Self::object_witness`]). Returns the **witness** commit id
    /// iff the gate admits the principal AND the skill makes the object readable. An empty result is the
    /// single not-entitled/not-found signal (gate-denied, skill-doesn't-reach, and object-nonexistent are
    /// indistinguishable).
    pub(crate) async fn authorize_object_read(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
        object_id: ObjectId,
        lane: ReadLane,
    ) -> Result<Option<CommitId>> {
        if !self.read_gate(ws, skill, principal, lane).await? {
            return Ok(None);
        }
        self.object_witness(ws, skill, object_id).await
    }

    /// The principal-free object-reachability witness — the reach half of the split (the lane gate has
    /// already admitted the caller). Two disjoint arms over the SAME (workspace-bound, skill-scoped)
    /// envelope:
    /// - **trunk**: `∃ c: skill_commit(w,s,c) ∧ commit_object(w,c,object_id)` — any accepted version of
    ///   the skill reaches the object.
    /// - **proposal**: `∃ p: proposal_object(w,p,object_id) ∧ p.skill=s ∧ p.status='open' ∧ p.base ==
    ///   current(w,s)` — an OPEN, NON-STALE proposal of the skill roots the object. This arm shares its
    ///   `open ∧ non-stale` predicate **verbatim** with the two GC keep-checks
    ///   ([`claim_for_delete`](Self::claim_for_delete) / [`claim_stale_for_recovery`](Self::claim_stale_for_recovery)),
    ///   so a reclaimed object is never still readable and a readable object is never reclaimed — the
    ///   keep-set == read-authorization invariant holds for pending proposals exactly as it does for the
    ///   trunk. The predicate is duplicated, not shared as one SQL string (`query!` cannot compose a literal,
    ///   and the bind-parameter numbering differs per call site); there are **FIVE** verbatim copies of
    ///   `open ∧ base == current` — this witness's proposal arm, [`Self::version_readable`]'s proposal arm,
    ///   the two GC keep-checks ([`claim_for_delete`](Self::claim_for_delete) /
    ///   [`claim_stale_for_recovery`](Self::claim_stale_for_recovery)), and the proposals listing
    ///   ([`Self::open_proposal_rows`]) — and a dedicated equivalence test pins the three
    ///   object-keyed copies (this arm + the two GC keep-checks) together against drift, while behavioral
    ///   tests pin the version-read and the listing copies to the same staleness semantics. A reclaimed object
    ///   that briefly outlives this check on a concurrent read is handled by
    ///   [`crate::read::read_object`]'s re-authorize-on-miss guard (404, never Integrity).
    ///
    /// Every table is bound on `workspace_id`, so no fact can cross a tenant.
    async fn object_witness(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        object_id: ObjectId,
    ) -> Result<Option<CommitId>> {
        let ws = ws.as_str();
        let skill = skill.as_str();
        let object = object_id.0.as_slice();
        let row = sqlx::query!(
            r#"
            SELECT w.commit_id AS "commit_id!: Vec<u8>" FROM (
                SELECT sc.commit_id AS commit_id
                FROM skill_commit  sc
                JOIN commit_object co ON co.workspace_id = sc.workspace_id AND co.commit_id = sc.commit_id
                WHERE sc.workspace_id = $1 AND sc.skill_id = $2 AND co.object_id = $3
              UNION ALL
                SELECT p.commit_id AS commit_id
                FROM proposal_object po
                JOIN proposals p  ON p.workspace_id = po.workspace_id AND p.id = po.proposal_id
                JOIN current    c  ON c.workspace_id = p.workspace_id  AND c.skill_id = p.skill_id
                WHERE po.workspace_id = $1 AND po.object_id = $3 AND p.skill_id = $2
                  AND p.status = 'open' AND c.epoch = p.base_epoch AND c.seq = p.base_seq
            ) w
            LIMIT 1
            "#,
            ws,
            skill,
            object,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        row.map(|r| commit_id_from_row(&r.commit_id)).transpose()
    }

    /// Gather the owning skill of each given commit id that has provenance in this workspace (absent
    /// ids — no provenance in any skill — are simply not returned). The cross-skill lineage predicate
    /// turns this into its membership facts; keeping the read here keeps `sqlx` out of that pure logic.
    pub(crate) async fn commit_owners(
        &self,
        ws: &WorkspaceId,
        commit_ids: &[CommitId],
    ) -> Result<Vec<(CommitId, SkillId)>> {
        let ws_s = ws.as_str();
        let mut out = Vec::new();
        // One bound lookup per id (the candidate-and-parents set is tiny). A per-id `query!` keeps
        // compile-time checking and the offline metadata; a dynamic `IN (..)` list would forfeit both.
        for &id in commit_ids {
            let cid = id.0.as_slice();
            let row = sqlx::query!(
                r#"SELECT skill_id AS "skill_id!" FROM skill_commit WHERE workspace_id = $1 AND commit_id = $2"#,
                ws_s,
                cid,
            )
            .fetch_optional(&self.pool)
            .await
            .map_err(AuthorityError::internal)?;
            if let Some(row) = row {
                // A stored skill_id is always pre-validated on the way in, so a re-parse failure here is
                // store corruption — map it to an integrity fault, not the boundary `InvalidId` (mirroring
                // `commit_id_from_row`'s handling of a bad-width BLOB).
                let skill = SkillId::parse(&row.skill_id).map_err(AuthorityError::integrity)?;
                out.push((id, skill));
            }
        }
        Ok(out)
    }

    /// Resolve a read token's sha256 to its `(workspace, skill, principal)` scope — the read-credential
    /// resolver. **The one lookup NOT bound on `workspace_id`:** the token IS what resolves the workspace,
    /// so this probes the globally-unique `token_sha256` primary key (O(1)) and ESTABLISHES the binding
    /// every subsequent query carries. Only the hash is stored, never the plaintext. The row's strings were
    /// validated when the token was minted, so a re-parse failure is store corruption (an integrity fault),
    /// not a client error — mirroring `commit_owners` / `resolve_device`. `None` ⇒ no such token.
    pub(crate) async fn lookup_read_token(
        &self,
        token_sha256: &[u8; 32],
        now: i64,
    ) -> Result<Option<(WorkspaceId, SkillId, Principal)>> {
        let key = token_sha256.as_slice();
        let row = sqlx::query!(
            r#"SELECT workspace_id AS "workspace_id!", skill_id AS "skill_id!", principal AS "principal!"
               FROM read_token WHERE token_sha256 = $1 AND (expires_at IS NULL OR expires_at > $2)"#,
            key,
            now,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some((
                WorkspaceId::parse(&r.workspace_id).map_err(AuthorityError::integrity)?,
                SkillId::parse(&r.skill_id).map_err(AuthorityError::integrity)?,
                Principal::parse(&r.principal).map_err(AuthorityError::integrity)?,
            ))),
        }
    }

    /// The version-read authorization — the R1 gate the version-metadata route runs, mirroring
    /// [`Self::authorize_object_read`]'s gate/reach split but anchored on a VERSION (`commit_id`) rather
    /// than an object: the lane's principal gate ([`Self::read_gate`]), then the principal-free
    /// [`Self::version_readable`]. `false` collapses gate-denied and not-reachable into the caller's one
    /// indistinguishable not-found.
    pub(crate) async fn authorize_version_read(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
        version_id: CommitId,
        lane: ReadLane,
    ) -> Result<bool> {
        if !self.read_gate(ws, skill, principal, lane).await? {
            return Ok(false);
        }
        self.version_readable(ws, skill, version_id).await
    }

    /// The principal-free version-reachability test — the reach half of the version read (the lane gate
    /// has already admitted the caller). `true` iff the version is readable through EITHER:
    /// - **trunk**: the version is owned by the skill (`skill_commit`) AND has ≥1 `commit_object` edge — the
    ///   accepted-trunk test (every accepted version roots ≥1 object, so a non-empty edge set is exact), OR
    /// - **proposal**: an OPEN, NON-STALE proposal of the skill whose `commit_id` is this version. This arm
    ///   reuses the SAME `status='open' ∧ (base_epoch, base_seq) == current.(epoch, seq)` staleness predicate
    ///   the object-reach arm ([`Self::object_witness`]) and the two GC keep-checks
    ///   ([`Self::claim_for_delete`] / [`Self::claim_stale_for_recovery`]) use — here anchored on
    ///   `proposals.commit_id`, not `proposal_object.object_id` (the bind shape differs, so it is the 4th copy
    ///   of the literal — the proposals listing [`Self::open_proposal_rows`] is the 5th — not a shared
    ///   string; the behavioral proposal-version tests pin it to the same
    ///   staleness semantics). It deliberately does NOT authorize on bare `skill_commit`, which also names
    ///   unaccepted/rejected proposal candidates — that would leak a never-accepted version's metadata (the
    ///   `commit_object` ≥1-edge join is load-bearing; a rejected-candidate-404 test pins it).
    ///
    /// Every table is bound on `workspace_id`, so no fact can cross a tenant.
    async fn version_readable(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        version_id: CommitId,
    ) -> Result<bool> {
        let ws = ws.as_str();
        let skill = skill.as_str();
        let cid = version_id.0.as_slice();
        let row = sqlx::query!(
            r#"
            SELECT 1::int8 AS "ok!: i64" FROM (
                SELECT 1 AS ok
                FROM skill_commit  sc
                JOIN commit_object co ON co.workspace_id = sc.workspace_id AND co.commit_id = sc.commit_id
                WHERE sc.workspace_id = $1 AND sc.skill_id = $2 AND sc.commit_id = $3
              UNION ALL
                SELECT 1 AS ok
                FROM proposals p
                JOIN current   c ON c.workspace_id = p.workspace_id  AND c.skill_id = p.skill_id
                WHERE p.workspace_id = $1 AND p.skill_id = $2
                  AND p.commit_id = $3 AND p.status = 'open'
                  AND c.epoch = p.base_epoch AND c.seq = p.base_seq
            ) w
            LIMIT 1
            "#,
            ws,
            skill,
            cid,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.is_some())
    }
}

/// True if `e` (a raw `sqlx::Error`, e.g. from `tx.commit()`) is a transient class the SERIALIZABLE runner
/// retries: a serialization failure (`40001`) or deadlock (`40P01`), OR a unique-violation (`23505`) on one
/// of the four CONVERGENT constraints — the two idempotency-key PKs, the one-open-proposal index, and the
/// create-workspace request ledger (`genesis_requests_pkey`: two racing creates of the SAME request both
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

/// Shared roster-existence probe (used by both the cheap pre-read on the pool and the authoritative
/// check inside the transaction). Generic over the executor so the identical query serves both.
async fn roster_exists<'e, E>(
    executor: E,
    ws: &WorkspaceId,
    skill: &SkillId,
    principal: &Principal,
) -> Result<bool>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let ws = ws.as_str();
    let skill = skill.as_str();
    let principal = principal.as_str();
    let row = sqlx::query!(
        "SELECT principal FROM roster WHERE workspace_id = $1 AND skill_id = $2 AND principal = $3 LIMIT 1",
        ws,
        skill,
        principal,
    )
    .fetch_optional(executor)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.is_some())
}

/// A CONFIRMED `workspace_member` row exists for this principal — the genesis-standup trust gate (the
/// workspace-level RBAC roster, distinct from the per-skill read `roster`). The query text is byte-identical
/// to `enroll::read_member_status`, so the committed `.sqlx` cache already covers it.
async fn workspace_member_confirmed<'e, E>(
    executor: E,
    ws: &WorkspaceId,
    principal: &Principal,
) -> Result<bool>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let ws_s = ws.as_str();
    let principal = principal.as_str();
    let row = sqlx::query!(
        r#"SELECT status AS "status!" FROM workspace_member WHERE workspace_id = $1 AND principal = $2"#,
        ws_s,
        principal,
    )
    .fetch_optional(executor)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(matches!(row, Some(r) if r.status == "confirmed"))
}

/// Self-insert a per-skill roster row — the genesis standup's one write (a first publish rosters its own
/// author for the skill it creates, inside the same transaction as the pointer). The INSERT text is
/// byte-identical to `enroll::redeem_run`'s roster grant, so the committed `.sqlx` cache already covers it;
/// `ON CONFLICT DO NOTHING` keeps a concurrent standup / governance roster mutation convergent.
async fn insert_roster<'e, E>(
    executor: E,
    ws: &WorkspaceId,
    skill: &SkillId,
    principal: &Principal,
) -> Result<()>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let ws_s = ws.as_str();
    let sk = skill.as_str();
    let prin = principal.as_str();
    sqlx::query!(
        "INSERT INTO roster (workspace_id, skill_id, principal) VALUES ($1, $2, $3) \
         ON CONFLICT (workspace_id, skill_id, principal) DO NOTHING",
        ws_s,
        sk,
        prin,
    )
    .execute(executor)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

/// Convert a stored 32-byte BLOB into a [`CommitId`] via the shared [`blob32`].
fn commit_id_from_row(bytes: &[u8]) -> Result<CommitId> {
    Ok(CommitId(blob32(bytes)?))
}

/// A stored commit-id BLOB was not exactly 32 bytes (the schema CHECK should forbid this).
#[derive(Debug, thiserror::Error)]
#[error("stored content id is not 32 bytes")]
struct BadBlobWidth;

// The object-lifecycle transitions (the fenced CAS state machine, leases, quarantine, tombstones). Driven
// by the ingest/migrate orchestration and the GC/recovery/janitor entry points; a few helpers (e.g.
// `release_lease`) are exercised only by tests, so the dead-code waiver stays on the module.
#[cfg_attr(not(test), allow(dead_code))]
mod lifecycle;

pub(crate) use lifecycle::{ClaimOutcome, InstallOutcome, Location, ObjectStatus};

// The operator backup/restore epoch bump (re-sign `current` one epoch forward; touches ONLY `current`).
mod restore;

// The pointer-move transaction (the `set-current` write) + its policy/device-registry helpers.
mod set_current;

// The durable all-outcome receipt machinery (read/insert/replay, the terminal-outcome writers, the outcome
// codecs) — shared by `set_current`'s promote and reject paths.
mod receipts;

// The contribute authority's proposal + approval SQL (publish --propose / review --approve|--reject).
mod proposals;

// The enrollment issuance SQL (invites / device-auth / passcodes / grants / redeem).
mod enroll;

/// The web-session roster SQL (invite / remove / rotate / the roster read).
pub(crate) mod session_read;
mod session_roster;

// The governance + admin-claim SQL (owner-signed create-invite + roster/revoke mutations; the first-boot claim).
mod governance;

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
    /// so a concurrent same-`op_id` receipt sibling or same-candidate proposer converges to the winner's
    /// outcome rather than surfacing a 500 — but NEVER on an ordinary unique violation (e.g. `roster_pkey`), a
    /// real business/integrity duplicate that must not be silently retried. Proven against real Postgres
    /// duplicate-key errors. Raw `sqlx::query` (not `query!`), so it adds nothing to the `.sqlx` drift surface.
    #[sqlx::test]
    async fn only_convergent_unique_violations_are_retryable(pool: PgPool) {
        // op_receipts_pkey → a unique violation the runner treats as retryable.
        let receipt = "INSERT INTO op_receipts \
            (workspace_id, device_key_id, op_id, command, skill_id, expected_epoch, expected_seq, \
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

        // roster_pkey → an ordinary unique violation the runner must NOT retry.
        let roster =
            "INSERT INTO roster (workspace_id, skill_id, principal) VALUES ('w_a', 's_a', 'p_a')";
        sqlx::query(roster)
            .execute(&pool)
            .await
            .expect("first roster insert");
        let dup = sqlx::query(roster)
            .execute(&pool)
            .await
            .expect_err("a duplicate roster row must raise a unique violation");
        assert!(
            !is_serialization_failure_sqlx(&dup),
            "a 23505 on an ordinary constraint must NOT be retried"
        );
    }
}
