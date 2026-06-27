//! Garbage collection — the transactional mark-then-claim fence over the git object store, plus the
//! recovery sweep and the quarantine janitor. The database leads; the filesystem trails.
//!
//! **GC (per workspace):** scan `present` objects (advisory), then for each run the three-step fence —
//! **claim** a guarded `present → deleting` that re-verifies AT DELETE TIME that the object is referenced by
//! no commit (the read-authorization surface) and named by no live lease; **unlink** the loose object
//! OUTSIDE any transaction; **finalize** `deleting → absent`. Each step is its own short transaction (or
//! none, for the unlink), so the single SQLite writer is never held across the filesystem op and a
//! concurrent migrate's lease-insert is never starved. GC acts ONLY on objects with an `object_presence`
//! row — the legacy straight-to-git upload path (no presence row) is invisible to it.
//!
//! **Recovery sweep:** finalizes a STALE `deleting` (one a crashed GC left behind) — re-unlink is
//! idempotent. **Janitor:** sweeps expired/abandoned quarantines, rebuilding the `rm -rf` path from the
//! re-validated ids (never trusting the stored objdir string).

use crate::authority::Authority;
use crate::error::Result;
use crate::id::WorkspaceId;
use crate::sqlite::ClaimOutcome;

/// How long a `deleting` row must sit before the recovery sweep treats it as a crashed GC's leftover (and
/// so never races a live GC's in-flight unlink/finalize).
const RECOVERY_STALE_SECS: i64 = 60;

/// Run one GC pass over a workspace. Returns the number of objects reclaimed (claimed → unlinked →
/// finalized) this pass — a bounded result, so a single pass reclaims every currently-unrooted object.
pub(crate) async fn run_gc(authority: &Authority, ws: &WorkspaceId, now: i64) -> Result<usize> {
    let candidates = authority.db().present_objects(ws).await?;
    if candidates.is_empty() {
        return Ok(0);
    }
    let store = authority.open_store(ws)?;
    let mut reclaimed = 0;
    for object_id in candidates {
        // Claim — the guarded `present → deleting` (its own short txn; releases the write lock at once).
        let git_oid = match authority.db().claim_for_delete(ws, object_id, now).await? {
            ClaimOutcome::Spared => continue,
            ClaimOutcome::Claimed { git_oid } => git_oid,
        };
        // Unlink — remove the loose object OUTSIDE any transaction (the filesystem trails the DB).
        store
            .delete_loose_object(git_oid)
            .map_err(crate::error::AuthorityError::internal)?;
        // Finalize — `deleting → absent` (its own short transaction).
        authority.db().finalize_delete(ws, object_id, now).await?;
        reclaimed += 1;
    }
    Ok(reclaimed)
}

/// Finalize every STALE `deleting` object across all workspaces — a crash that left an object mid-unlink.
/// Idempotent: the re-unlink tolerates an already-gone object, and the finalize CAS is guarded. The
/// composing process owns scheduling (this library holds no scheduler), but it MUST run this on startup and
/// periodically (≈ every few minutes) so a stranded `deleting` cannot make every migrate of that content
/// time out.
pub(crate) async fn recovery_sweep(authority: &Authority, now: i64) -> Result<usize> {
    let older_than = now - RECOVERY_STALE_SECS;
    let mut recovered = 0;
    for ws in authority
        .db()
        .workspaces_with_stale_deleting(older_than)
        .await?
    {
        let store = authority.open_store(&ws)?;
        for (object_id, git_oid) in authority.db().stale_deleting(&ws, older_than).await? {
            store
                .delete_loose_object(git_oid)
                .map_err(crate::error::AuthorityError::internal)?;
            authority.db().finalize_delete(&ws, object_id, now).await?;
            recovered += 1;
        }
    }
    Ok(recovered)
}

/// Sweep every expired/abandoned upload quarantine across all workspaces, removing its objdir whole. The
/// destructive `rm -rf` path is REBUILT from the re-validated `(WorkspaceId, OpId)` — never the stored
/// `objdir` string — so a poisoned path string can never escape the quarantine root.
pub(crate) async fn quarantine_janitor(authority: &Authority, now: i64) -> Result<usize> {
    let mut swept = 0;
    for ws in authority
        .db()
        .workspaces_with_expired_quarantine(now)
        .await?
    {
        for op_id in authority.db().expired_quarantine_ops(&ws, now).await? {
            let dir = authority.workspace_quarantine_dir(&ws, &op_id);
            let _ = std::fs::remove_dir_all(&dir);
            authority.db().delete_quarantine(&ws, &op_id).await?;
            swept += 1;
        }
    }
    Ok(swept)
}
