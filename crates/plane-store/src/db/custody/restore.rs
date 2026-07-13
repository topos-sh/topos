//! The operator backup/restore **epoch bump** — the raw-SQL half of `restore-bump-epochs`.
//!
//! ONE `SERIALIZABLE` (`run_serializable!`) transaction locks every selected `current` row (`FOR UPDATE`,
//! in one stable order), rewrites each pointer at `max(epoch + 1, the operator floor)` — **same commit,
//! same seq** — and updates the row in place. This is an OPERATOR helper, not a protocol write: it touches
//! ONLY the `current` table (no receipt, no provenance, no proposal, no lease, no generation-advance logic
//! anywhere else), because a restored database already holds every one of those rows for the versions it
//! knows — only the pointer's *generation* has moved backward relative to what was already served. All
//! `sqlx` stays here; the caller ([`crate::custody::restore`]) hands in the validated selection and gets
//! back domain [`EpochBumpReport`]s.
//!
//! Concurrency: the serializable transaction + the `FOR UPDATE` row locks mean a concurrent publish either
//! lands first (this transaction's read then sees — and bumps — the new row, after a serialization retry if
//! needed) or CONFLICTs normally against the bumped `(epoch, seq)` pair — no torn state. The runbook still
//! says stop the plane first.

use sqlx::{Postgres, Transaction};
use topos_types::{
    CurrentRecord, Generation, PointerScope, WIRE_SCHEMA_VERSION, WireCurrentRecord,
};

use crate::custody::restore::EpochBumpReport;
use crate::db::Db;
use crate::error::{AuthorityError, Result};
use crate::id::{BundleId, WorkspaceId};

/// The I-JSON safe-integer bound (2^53 − 1) the wire record enforces — a generation a JSON consumer could
/// not represent exactly is never stored or served. Mirrors the pointer-move's own bound (private to
/// `db/custody/set_current.rs`, which is deliberately untouched by this helper).
const MAX_SAFE_INT: u64 = (1u64 << 53) - 1;

impl Db {
    /// Run the one epoch-bump transaction over the selected workspaces (`None` ⇒ every workspace on the
    /// plane). Returns one [`EpochBumpReport`] per bumped `current` row (an empty selection is an empty
    /// report, not an error). All-or-nothing: any per-row fault (an out-of-range bump, a store corruption)
    /// rolls the whole transaction back with nothing written.
    pub(crate) async fn restore_bump_epochs_txn(
        &self,
        workspaces: Option<&[WorkspaceId]>,
        epoch_at_least: Option<u64>,
        now: i64,
    ) -> Result<Vec<EpochBumpReport>> {
        let filter: Option<Vec<String>> =
            workspaces.map(|ids| ids.iter().map(|w| w.as_str().to_owned()).collect());
        run_serializable!(
            self,
            tx,
            run(&mut tx, filter.as_deref(), epoch_at_least, now).await
        )
    }
}

/// The transaction body (re-runnable: it borrows its inputs, serialization is deterministic, and every
/// write is in-transaction — a serialization retry re-reads fresh committed rows and re-derives the same
/// bumps).
async fn run(
    tx: &mut Transaction<'_, Postgres>,
    filter: Option<&[String]>,
    epoch_at_least: Option<u64>,
    now: i64,
) -> Result<Vec<EpochBumpReport>> {
    let rows = lock_current_rows(tx, filter).await?;
    let mut reports = Vec::with_capacity(rows.len());
    for row in rows {
        let commit = crate::db::custody::read::commit_id_from_row(&row.commit_id)?;
        let old = Generation {
            epoch: i64_to_u64(row.epoch)?,
            seq: i64_to_u64(row.seq)?,
        };
        // Guard the stored generation is in range (it carries no DB-level CHECK) — a violation is store
        // corruption, mirroring the pointer-move's own stored-range guard.
        if old.epoch > MAX_SAFE_INT || old.seq > MAX_SAFE_INT {
            return Err(AuthorityError::integrity(GenerationOutOfRange));
        }
        // `new_epoch = max(epoch + 1, the operator floor)`. The floor exists because an operator who
        // restored ONCE before from an even older backup could otherwise re-issue an `(epoch, seq)` tuple
        // followers already recorded — `--epoch-at-least` lets them jump past every epoch ever served. A
        // floor at or below `epoch + 1` is a no-op (max semantics). The bump keeps `seq` UNCHANGED:
        // epoch-dominant ordering makes `(e+1, s)` beat `(e, anything)`, and preserving `seq` keeps the
        // move count honest — the next publish lands at `(new_epoch, s + 1)`.
        let new_epoch = match old
            .epoch
            .checked_add(1)
            .map(|bumped| bumped.max(epoch_at_least.unwrap_or(0)))
        {
            // The safe-integer guard: past the bound a JSON consumer could not represent the epoch
            // exactly, so fail typed with NOTHING written (the whole transaction rolls back).
            Some(epoch) if epoch <= MAX_SAFE_INT => epoch,
            _ => return Err(AuthorityError::internal(EpochBumpOutOfRange)),
        };
        let new = Generation {
            epoch: new_epoch,
            seq: old.seq,
        };
        let record = serialize_record(&row.workspace_id, &row.bundle_id, &commit.0, new)?;
        bump_row(tx, &row, new.epoch, &record, now).await?;
        // Stored ids were validated on the way in, so a re-parse failure here is store corruption.
        reports.push(EpochBumpReport {
            workspace_id: WorkspaceId::parse(&row.workspace_id)
                .map_err(AuthorityError::integrity)?,
            bundle_id: BundleId::parse(&row.bundle_id).map_err(AuthorityError::integrity)?,
            commit,
            old,
            new,
        });
    }
    Ok(reports)
}

/// One selected-and-locked `current` row, exactly as stored.
struct LockedCurrentRow {
    workspace_id: String,
    bundle_id: String,
    commit_id: Vec<u8>,
    epoch: i64,
    seq: i64,
}

/// Select + lock (`FOR UPDATE`) the `current` rows the bump will re-sign, in one stable order. Two
/// spellings of one query (`query!` cannot compose a literal): every workspace, or the operator's explicit
/// selection (`= ANY($1)` — a named-but-absent workspace simply matches no rows).
async fn lock_current_rows(
    tx: &mut Transaction<'_, Postgres>,
    filter: Option<&[String]>,
) -> Result<Vec<LockedCurrentRow>> {
    match filter {
        None => sqlx::query!(
            r#"SELECT workspace_id AS "workspace_id!", skill_id AS "skill_id!",
                      commit_id AS "commit_id!: Vec<u8>", epoch AS "epoch!: i64", seq AS "seq!: i64"
               FROM current ORDER BY workspace_id, skill_id FOR UPDATE"#,
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(AuthorityError::internal)
        .map(|rows| {
            rows.into_iter()
                .map(|r| LockedCurrentRow {
                    workspace_id: r.workspace_id,
                    bundle_id: r.skill_id,
                    commit_id: r.commit_id,
                    epoch: r.epoch,
                    seq: r.seq,
                })
                .collect()
        }),
        Some(ids) => sqlx::query!(
            r#"SELECT workspace_id AS "workspace_id!", skill_id AS "skill_id!",
                      commit_id AS "commit_id!: Vec<u8>", epoch AS "epoch!: i64", seq AS "seq!: i64"
               FROM current WHERE workspace_id = ANY($1)
               ORDER BY workspace_id, skill_id FOR UPDATE"#,
            ids,
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(AuthorityError::internal)
        .map(|rows| {
            rows.into_iter()
                .map(|r| LockedCurrentRow {
                    workspace_id: r.workspace_id,
                    bundle_id: r.skill_id,
                    commit_id: r.commit_id,
                    epoch: r.epoch,
                    seq: r.seq,
                })
                .collect()
        }),
    }
}

/// Write one bump: `epoch`, the fresh `record`, and `updated_at` — the commit id and `seq` are
/// deliberately untouched. The `WHERE` re-asserts the exact generation the row was read (and locked) at.
async fn bump_row(
    tx: &mut Transaction<'_, Postgres>,
    row: &LockedCurrentRow,
    new_epoch: u64,
    record: &[u8],
    now: i64,
) -> Result<()> {
    let epoch = u64_to_i64(new_epoch)?;
    let ws_s = row.workspace_id.as_str();
    let skill_s = row.bundle_id.as_str();
    let done = sqlx::query!(
        "UPDATE current SET epoch = $1, record = $2, updated_at = $3 \
         WHERE workspace_id = $4 AND skill_id = $5 AND epoch = $6 AND seq = $7",
        epoch,
        record,
        now,
        ws_s,
        skill_s,
        row.epoch,
        row.seq,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    // The row was read under this transaction's own FOR UPDATE lock, so it cannot have moved beneath us; a
    // miss is an internal fault that rolls the whole bump back (and a genuine concurrent writer surfaces as
    // a serialization failure the runner retries — the retry re-reads fresh rows).
    if done.rows_affected() != 1 {
        return Err(AuthorityError::internal(BumpedRowMoved));
    }
    Ok(())
}

/// Serialize the [`WireCurrentRecord`] document stored in `current.record` — the SAME typed-DTO
/// construction the pointer-move's promote path performs (its serializer is private to
/// `db/custody/set_current.rs`, which this helper deliberately never touches; the record-parity test in
/// `tests/restore.rs` pins the two shapes together against drift).
fn serialize_record(
    workspace_id: &str,
    bundle_id: &str,
    version_id: &[u8; 32],
    generation: Generation,
) -> Result<Vec<u8>> {
    let record = WireCurrentRecord {
        schema_version: WIRE_SCHEMA_VERSION,
        scope: PointerScope {
            workspace_id: workspace_id.to_owned(),
            skill_id: bundle_id.to_owned(),
        },
        record: CurrentRecord {
            version_id: topos_core::digest::to_hex(version_id),
            generation,
        },
    };
    serde_json::to_vec(&record).map_err(AuthorityError::internal)
}

// --- small conversions (a stored value that violates a width/range CHECK is store corruption) ---

fn i64_to_u64(v: i64) -> Result<u64> {
    u64::try_from(v).map_err(|_| AuthorityError::integrity(GenerationOutOfRange))
}

fn u64_to_i64(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| AuthorityError::integrity(GenerationOutOfRange))
}

#[derive(Debug, thiserror::Error)]
#[error("a stored generation is out of the safe-integer range")]
struct GenerationOutOfRange;

#[derive(Debug, thiserror::Error)]
#[error("the bumped epoch would exceed the safe-integer bound (2^53 - 1); nothing was written")]
struct EpochBumpOutOfRange;

#[derive(Debug, thiserror::Error)]
#[error("a current row vanished under its own FOR UPDATE lock during the epoch bump")]
struct BumpedRowMoved;
