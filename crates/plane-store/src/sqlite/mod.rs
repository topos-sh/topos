//! The SQLite backend — **the only place raw `sqlx` lives.**
//!
//! The pool, every transaction, and every `query!` are private to this module; no `sqlx` type appears
//! in any signature crossing the module boundary. Every operation takes the validated id newtypes plus
//! the data it needs and returns plain domain values (`CommitId`, small enums, `bool`) — so a caller
//! outside this module can never run an unbound query, hold a transaction, or read a bare object. That
//! privacy boundary is the access-rule enforcement mechanism. A future Postgres backend is a sibling
//! module with its own `query!` invocations and its own offline metadata; the domain-typed surface is
//! the seam an `enum Db { Sqlite, Pg }` would wrap with no change to callers.

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, Principal, SkillId, WorkspaceId};

/// The connection pool's busy-timeout — how long a writer waits for the single SQLite write lock
/// before failing, rather than erroring immediately under contention.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The outcome of recording a candidate commit's provenance under the authoritative roster check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordOutcome {
    /// Provenance + reachability recorded (or already present for this same skill — idempotent).
    Recorded,
    /// The uploading principal is not rostered for the skill (the in-transaction authoritative check).
    NotRostered,
    /// The commit id is already owned by a different skill — a cross-skill adoption attempt, blocked
    /// structurally by the `skill_commit` primary key.
    OwnedByOtherSkill,
}

/// The SQLite-backed authority database: one pool configured for the transaction discipline the
/// authority requires (WAL, `BEGIN IMMEDIATE`, a busy timeout, foreign keys on).
#[derive(Debug)]
pub(crate) struct Db {
    pool: SqlitePool,
}

impl Db {
    /// Open (creating if missing) the database at `path`, applying the embedded migrations.
    ///
    /// The pragmas are set on the connect options so **every** pooled connection is configured
    /// identically: WAL journaling, `synchronous = NORMAL` (the correct pairing with WAL), a busy
    /// timeout, and foreign-key enforcement (off by default in SQLite — the composite foreign keys are
    /// silently ignored without it).
    pub(crate) async fn open(path: &Path) -> Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(BUSY_TIMEOUT)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .connect_with(opts)
            .await
            .map_err(AuthorityError::internal)?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(AuthorityError::internal)?;
        Ok(Self { pool })
    }

    /// The connection pool — PRIVATE to `mod sqlite`, so the child `lifecycle`/`seed` submodules reach it
    /// for their pool reads while it stays unreachable elsewhere in the crate (no `sqlx` handle ever crosses
    /// the module boundary). (Only the not-yet-wired lifecycle pool reads use it, so it is unreferenced in a
    /// non-test production build.)
    #[cfg_attr(not(test), allow(dead_code))]
    fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Begin an IMMEDIATE-mode transaction pinned to one pooled connection.
    ///
    /// This is the **only** way the authority opens a write transaction. `begin_with("BEGIN
    /// IMMEDIATE")` takes the write lock up front (a plain `begin()` issues a deferred `BEGIN` that
    /// upgrades on first write and can then fail busy) **and** pins the whole transaction to one held
    /// connection (a bare `execute("BEGIN IMMEDIATE")` on the pool could route the next statement or
    /// the commit to a different connection). Keeping it private here makes the wrong forms unreachable.
    async fn begin_immediate(&self) -> Result<Transaction<'_, Sqlite>> {
        self.pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(AuthorityError::internal)
    }

    /// The read-authorization join: return the **witness** commit id iff the principal is rostered for
    /// the skill AND some commit of that skill reaches `object_id` — i.e.
    /// `∃ c: skill_commit(w,s,c) ∧ commit_object(w,c,object_id)`. An empty result is the single
    /// not-entitled/not-found signal (not-rostered, skill-doesn't-reach, and object-nonexistent are
    /// indistinguishable). Every table is bound on `workspace_id`, so no fact can cross a tenant.
    pub(crate) async fn authorize_object_read(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
        object_id: ObjectId,
    ) -> Result<Option<CommitId>> {
        let ws = ws.as_str();
        let skill = skill.as_str();
        let principal = principal.as_str();
        let object = object_id.0.as_slice();
        let row = sqlx::query!(
            r#"
            SELECT sc.commit_id AS "commit_id!: Vec<u8>"
            FROM roster r
            JOIN skill_commit  sc ON sc.workspace_id = r.workspace_id AND sc.skill_id = r.skill_id
            JOIN commit_object co ON co.workspace_id = sc.workspace_id AND co.commit_id = sc.commit_id
            WHERE r.workspace_id = ?1 AND r.skill_id = ?2 AND r.principal = ?3 AND co.object_id = ?4
            LIMIT 1
            "#,
            ws,
            skill,
            principal,
            object,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        row.map(|r| commit_id_from_row(&r.commit_id)).transpose()
    }

    /// Whether the principal currently has a roster row for the skill (the cheap pre-read before an
    /// upload's git write — a non-authoritative fail-fast; the authoritative check is inside the
    /// transaction below).
    pub(crate) async fn is_rostered(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
    ) -> Result<bool> {
        roster_exists(&self.pool, ws, skill, principal).await
    }

    /// Record a candidate commit's provenance + reachability **under the authoritative roster check**,
    /// all in one immediate-write transaction (authorization before provenance: the read join trusts
    /// `skill_commit` directly, so nothing readable may be recorded for an un-rostered caller).
    ///
    /// Order: (1) the authoritative roster check; (2) an owner-guarded `skill_commit` insert — the
    /// primary key makes a content-derived commit id belong to exactly one skill, so a cross-skill
    /// re-upload (same id) is refused; (3) the `commit_object` reachability edges (idempotent). A deny
    /// rolls the transaction back, recording nothing.
    pub(crate) async fn record_authorized_commit(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
        commit: CommitId,
        objects: &[ObjectId],
    ) -> Result<RecordOutcome> {
        let ws_s = ws.as_str();
        let skill_s = skill.as_str();
        let cid = commit.0.as_slice();

        let mut tx = self.begin_immediate().await?;

        // (1) Authoritative authorization — serialized in the write transaction, so a roster change
        // racing the upload cannot let an un-rostered candidate through.
        if !roster_exists(&mut *tx, ws, skill, principal).await? {
            tx.rollback().await.map_err(AuthorityError::internal)?;
            return Ok(RecordOutcome::NotRostered);
        }

        // (2) Provenance, owner-guarded by the primary key. Insert-if-absent, then read the owner: if a
        // different skill already owns this commit id, deny without naming the other skill.
        sqlx::query!(
            "INSERT INTO skill_commit (workspace_id, commit_id, skill_id) VALUES (?1, ?2, ?3) \
             ON CONFLICT (workspace_id, commit_id) DO NOTHING",
            ws_s,
            cid,
            skill_s,
        )
        .execute(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        let owner = sqlx::query!(
            r#"SELECT skill_id AS "skill_id!" FROM skill_commit WHERE workspace_id = ?1 AND commit_id = ?2"#,
            ws_s,
            cid,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        match owner {
            Some(row) if row.skill_id == skill_s => {}
            Some(_) => {
                tx.rollback().await.map_err(AuthorityError::internal)?;
                return Ok(RecordOutcome::OwnedByOtherSkill);
            }
            // The row was inserted-or-present above, so it must exist; a missing row is a store fault.
            None => {
                tx.rollback().await.map_err(AuthorityError::internal)?;
                return Err(AuthorityError::integrity(MissingProvenanceRow));
            }
        }

        // (3) Reachability edges (the FK to skill_commit is now satisfied). Idempotent: a re-upload of
        // already-present edges is a no-op with no observable difference (dedup is invisible).
        for obj in objects {
            let object = obj.0.as_slice();
            sqlx::query!(
                "INSERT INTO commit_object (workspace_id, commit_id, object_id) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (workspace_id, commit_id, object_id) DO NOTHING",
                ws_s,
                cid,
                object,
            )
            .execute(&mut *tx)
            .await
            .map_err(AuthorityError::internal)?;
        }

        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(RecordOutcome::Recorded)
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
                r#"SELECT skill_id AS "skill_id!" FROM skill_commit WHERE workspace_id = ?1 AND commit_id = ?2"#,
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
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let ws = ws.as_str();
    let skill = skill.as_str();
    let principal = principal.as_str();
    let row = sqlx::query!(
        "SELECT principal FROM roster WHERE workspace_id = ?1 AND skill_id = ?2 AND principal = ?3 LIMIT 1",
        ws,
        skill,
        principal,
    )
    .fetch_optional(executor)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.is_some())
}

/// Convert a stored 32-byte BLOB into a [`CommitId`], or an integrity fault if the width is wrong (the
/// schema's length CHECK should prevent it; a violation means store corruption).
fn commit_id_from_row(bytes: &[u8]) -> Result<CommitId> {
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| AuthorityError::integrity(BadBlobWidth))?;
    Ok(CommitId(arr))
}

/// A stored commit-id BLOB was not exactly 32 bytes (the schema CHECK should forbid this).
#[derive(Debug, thiserror::Error)]
#[error("stored content id is not 32 bytes")]
struct BadBlobWidth;

/// The owner row for a just-inserted commit was unexpectedly absent.
#[derive(Debug, thiserror::Error)]
#[error("provenance row absent immediately after insert")]
struct MissingProvenanceRow;

// The object-lifecycle transitions (the fenced CAS state machine, leases, quarantine, tombstones). Driven
// by the not-yet-wired orchestration + the tests; unreferenced in a non-test production build.
#[cfg_attr(not(test), allow(dead_code))]
mod lifecycle;

pub(crate) use lifecycle::{ClaimOutcome, InstallOutcome, ObjectStatus};

#[cfg(test)]
mod seed;
