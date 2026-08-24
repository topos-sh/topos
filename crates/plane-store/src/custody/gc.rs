//! Garbage collection — the transactional mark-then-acquire fence over the object store, plus the
//! recovery sweep and the staging janitor. The database leads; the store trails.
//!
//! **Scheduling is the composing server's** (this library holds none): the public
//! [`Authority::run_gc`]/[`Authority::run_recovery`]/[`Authority::run_janitor`] wrappers are what it
//! drives, on startup and periodically ([`Authority::workspaces`] enumerates the per-workspace GC
//! targets; recovery and the janitor sweep cross-workspace internally). All three futures are `Send`
//! so they spawn onto a multi-threaded runtime; a compile-time assertion below pins that.
//!
//! **Clock convention: one server-clock unit = one epoch MILLISECOND**; the stale threshold below is
//! millisecond-valued to match the epoch-ms `now` the composing server stamps.
//!
//! **GC (per workspace):** scan for unrooted `present` objects (advisory), then for each run the
//! three-step fence — **acquire** a guarded `present → deleting` that re-verifies AT DELETE TIME that the
//! object is kept by neither root (a non-purged version's `version_object` edge, a live promotion
//! lease); **delete** the store object OUTSIDE any transaction; **finalize** `deleting → absent` (or the
//! terminal `unavailable` for a tombstoned blob). The acquire stamps an ACCURATE wall-clock (the pass's
//! base `now` advanced by its real elapsed) into `status_updated_at`, and that value is the actor's
//! **acquire token**: the delete step first re-confirms ownership of it and the finalize is gated on
//! matching it, so a recovery sweep that takes over a frozen pass can never also delete/finalize the
//! same row. Each step is its own short transaction (or none, for the delete), so no write
//! transaction is held across the store op. GC acts ONLY on objects with an `object_presence` row.
//!
//! **THE DELETE-LIFETIME INVARIANT (remote store).** A DELETE issued under an acquire token must be
//! provably DEAD before that token can be superseded — otherwise a delayed/retried DELETE could
//! remove bytes a later ingest re-installed under a fresh token (Postgres would then say `present`
//! over missing bytes). The seam enforces it structurally: the delete client has transport retries
//! DISABLED (single attempt), a hard request timeout, and the whole call is bounded by
//! [`REMOTE_DELETE_TIMEOUT_MS`] wall-clock — strictly below [`RECOVERY_STALE_MS`] (the compile-time
//! assertion below pins the ordering), and recovery may supersede a token only once the row is
//! RECOVERY_STALE_MS old. Ownership is re-verified in the same task immediately before the call is
//! issued. An AMBIGUOUS outcome (timeout, transport fault) never finalizes absence — the row stays
//! `deleting` for a later recovery pass, which re-runs the idempotent delete under its own token.
//!
//! **Recovery sweep:** finalizes a STALE `deleting` (one a crashed GC left behind), each via a
//! one-winner re-acquire that re-verifies the retention surface at delete time (no live version roots
//! it), so a crashed acquire's row a commit re-rooted before recovery runs is spared, not reclaimed. It
//! deliberately does NOT re-check the lease: a lease over a *`deleting`* object is an ingest WAITING
//! for recovery to clear it (its `install_one` re-puts once the row reaches `absent`), so sparing it
//! would strand that waiter — and a lease alone is never readable, so finalizing it loses no readable
//! bytes.
//!
//! **Janitor:** sweeps expired/abandoned staging dirs, rebuilding the `rm -rf` path from the
//! re-validated ids (never a stored path string) and acquiring each row (flip to `aborted` iff still
//! expired + in-flight) BEFORE the unlink. Staging is EPHEMERAL — a replaced container's leftover
//! `upload` rows sweep to `aborted` with nothing on disk, which is exactly the contract. Op ids are
//! vault-minted fresh per ingest, so an acquired row can never belong to a live ingest under a
//! reused id.
//!
//! **The keep-set re-check is at acquire time, not the physical delete.** Both the live acquire and the
//! recovery re-acquire re-verify the keep-set inside their transaction, then release the single-writer
//! lock before the out-of-transaction delete. The ONLY writer of a `version_object` edge is the commit
//! transaction, which holds the candidate's committed lease across the edge write — and
//! `acquire_for_delete` spares any live-leased object — so an edge can never appear in the (acquire,
//! delete] window for an object the acquire just took: the **lease→edge handoff** closes it by
//! construction.

use crate::authority::Authority;
use crate::db::AcquireOutcome;
use crate::error::Result;
use crate::id::{ObjectId, WorkspaceId};
use crate::lifecycle::elapsed_ms;
use crate::store::{DeleteOutcome, REMOTE_DELETE_TIMEOUT_MS};

/// How long (epoch-ms; one minute) a `deleting` row must sit before the recovery sweep treats it as a
/// crashed GC's leftover. A live `run_gc` stamps every acquire with an accurate wall-clock (it advances `now`
/// by the pass's real elapsed), so a HEALTHY in-flight delete is never this old and recovery does not race
/// it. A GC frozen longer than this (effectively crashed) IS taken over — and even then the acquire-token
/// fence (`finalize_delete` and `confirm_deleting_owner` both gate on the acquirer's `status_updated_at`)
/// guarantees exactly one actor deletes + finalizes the row.
pub(crate) const RECOVERY_STALE_MS: i64 = 60 * 1000;

// The delete-lifetime invariant, pinned at compile time: a single-attempt delete's whole
// wall-clock bound must sit strictly below the recovery-staleness threshold, so a delete issued
// under one ownership token is dead before recovery may hand the object to another actor.
const _: () = assert!((REMOTE_DELETE_TIMEOUT_MS as i64) < RECOVERY_STALE_MS);

/// Run one GC pass over a workspace. Returns the number of objects reclaimed (acquired → deleted →
/// finalized) this pass — a bounded result, so a single pass reclaims every currently-unrooted
/// object whose delete resolved.
pub(crate) async fn run_gc(authority: &Authority, ws: &WorkspaceId, now: i64) -> Result<usize> {
    // The advisory scan already anti-joins the keep-set in SQL (the same two clauses the acquire
    // re-verifies), so a pass does work proportional to actual garbage; the guarded per-object acquire
    // below remains the sole authority.
    let candidates = authority.db().gc_candidates(ws, now).await?;
    if candidates.is_empty() {
        return Ok(0);
    }
    let started = tokio::time::Instant::now();
    let mut reclaimed = 0;
    for object_id in candidates {
        // Stamp each acquire with an ACCURATE wall-clock — `now` advanced by this pass's real elapsed, never
        // the pass-fixed `now` — so a long pass does not back-date a late acquire. A back-dated `deleting` row
        // would look older than RECOVERY_STALE_MS the instant it is acquired, and a concurrently-scheduled
        // recovery sweep would wrongly take it for a crashed-GC leftover and re-acquire it. This `acquire_now`
        // is also this actor's ACQUIRE TOKEN (the value the acquire stamps into `status_updated_at`).
        let acquire_now = now.saturating_add(elapsed_ms(started));
        // Acquire — the guarded `present → deleting` (its own short txn; releases the write lock at once).
        let git_oid = match authority
            .db()
            .acquire_for_delete(ws, object_id, acquire_now)
            .await?
        {
            AcquireOutcome::Spared => continue,
            AcquireOutcome::Acquired { git_oid } => git_oid,
        };
        if !delete_owned(authority, ws, object_id, git_oid, acquire_now).await? {
            continue;
        }
        // Finalize — `deleting → absent` / `unavailable` (its own short transaction), GATED on this actor's
        // acquire token so a row a recovery sweep re-acquired is never finalized out from under it.
        let finalize_now = now.saturating_add(elapsed_ms(started));
        authority
            .db()
            .finalize_delete(ws, object_id, acquire_now, finalize_now)
            .await?;
        reclaimed += 1;
    }
    Ok(reclaimed)
}

/// Finalize every STALE `deleting` object across all workspaces — a crash that left an object
/// mid-delete. Idempotent: the re-delete tolerates an already-gone object, and the finalize CAS is
/// guarded. The composing process owns scheduling, but it MUST run this on startup and periodically
/// (≈ every few minutes) so a stranded `deleting` cannot make every re-ingest of that content time out.
pub(crate) async fn recovery_sweep(authority: &Authority, now: i64) -> Result<usize> {
    let older_than = now - RECOVERY_STALE_MS;
    let mut recovered = 0;
    for ws in authority
        .db()
        .workspaces_with_stale_deleting(older_than)
        .await?
    {
        // The stale list is advisory; the per-object acquire is the one-winner guard (mirroring run_gc's
        // acquire) — it keeps the row `deleting` across the delete and hands the locator to exactly one
        // sweeper, so two concurrent recoveries can't both delete and an ingest can't reinstall mid-sweep.
        // It ALSO re-verifies the retention surface at delete time, so a stale `deleting` row a commit
        // re-rooted after the crashed acquire is spared rather than deleted. (It does NOT re-check the
        // lease: a lease over a `deleting` object is a waiting ingest recovery must unblock.)
        for object_id in authority.db().stale_deleting(&ws, older_than).await? {
            // `acquire_stale_for_recovery` stamps `status_updated_at = now`, so `now` is THIS sweeper's acquire
            // token (the value its finalize/owner-check gate on).
            let git_oid = match authority
                .db()
                .acquire_stale_for_recovery(&ws, object_id, older_than, now)
                .await?
            {
                None => continue, // another sweeper already acquired it (or it was re-rooted / no longer stale)
                Some(git_oid) => git_oid,
            };
            if !delete_owned(authority, &ws, object_id, git_oid, now).await? {
                continue;
            }
            authority
                .db()
                .finalize_delete(&ws, object_id, now, now)
                .await?;
            recovered += 1;
        }
    }
    Ok(recovered)
}

/// Sweep every expired/abandoned staging dir across all workspaces, removing its dir whole. The
/// destructive `rm -rf` path is REBUILT from the re-validated `(WorkspaceId, OpId)` — never a stored
/// path string — so a poisoned value can never escape the staging root. Each candidate is **acquired
/// (flip `aborted` iff still expired + in-flight) before the unlink**.
pub(crate) async fn quarantine_janitor(authority: &Authority, now: i64) -> Result<usize> {
    let older_than = now - crate::lifecycle::QUARANTINE_TTL_MS;
    let mut swept = 0;
    for (ws, op_id) in authority.db().expired_uploads(older_than).await? {
        // Atomically ACQUIRE the expired row before touching the filesystem: only the winner proceeds
        // to the unlink, so two concurrent janitors never both sweep one dir.
        if !authority
            .db()
            .acquire_expired_upload(&op_id, older_than)
            .await?
        {
            continue;
        }
        // Rebuild the rm -rf path from the re-validated ids and remove the dir whole. The row is
        // already terminal, so a transient rm failure leaves only an orphan dir — a low-severity,
        // disk-only residual on an EPHEMERAL root — never a wrongly-swept active staging dir.
        let dir = authority.workspace_quarantine_dir(&ws, &op_id);
        crate::lifecycle::remove_quarantine_dir(&dir);
        swept += 1;
    }
    Ok(swept)
}

/// One owned, bounded store delete — the shared delete step of the live GC, the recovery sweep, and
/// the purge's inline reclaim. Returns whether the caller may FINALIZE absence.
///
/// **INVARIANT (delete-vs-reinstall):** ownership of `acquire_token` is re-verified HERE, in the
/// same task, immediately before the delete is issued; the delete itself is a SINGLE attempt whose
/// total wall-clock is bounded by `REMOTE_DELETE_TIMEOUT_MS` — strictly below `RECOVERY_STALE_MS`,
/// the earliest instant recovery may supersede the token. So by the time any other actor can own
/// this object (and an ingest can re-install its bytes), this call's delete is guaranteed dead —
/// a delayed delete can never land on freshly re-installed bytes. An ambiguous outcome returns
/// `false`: absence is NEVER finalized on ambiguity; the row stays `deleting` for a later pass.
pub(crate) async fn delete_owned(
    authority: &Authority,
    ws: &WorkspaceId,
    object_id: ObjectId,
    git_oid: [u8; 20],
    acquire_token: i64,
) -> Result<bool> {
    // Re-confirm ownership immediately before issuing the delete: if a recovery sweep re-acquired
    // this row (the pass froze past the stale threshold), the bytes are now that sweeper's to
    // remove — skip, so two actors never both delete.
    if !authority
        .db()
        .confirm_deleting_owner(ws, object_id, acquire_token)
        .await?
    {
        return Ok(false);
    }
    match authority
        .store()
        .delete_loose_single_attempt(ws, &git_oid)
        .await
    {
        DeleteOutcome::Removed => Ok(true),
        DeleteOutcome::Ambiguous(reason) => {
            // Leave the row `deleting`; a later pass (recovery once it is stale) re-runs the
            // idempotent delete under its own token. Never finalize absent on ambiguity.
            tracing::warn!(
                reason,
                "gc delete ambiguous; leaving the object for a later pass"
            );
            Ok(false)
        }
    }
}

/// Compile-time pin: the three GC entry points' futures are `Send`, so the composing server can spawn them
/// onto a multi-threaded runtime. Never called — the assertion is the compilation itself.
#[cfg(test)]
#[allow(dead_code)]
fn assert_gc_futures_are_send(authority: &Authority, ws: &WorkspaceId) {
    fn assert_send<T: Send>(_: T) {}
    assert_send(run_gc(authority, ws, 0));
    assert_send(recovery_sweep(authority, 0));
    assert_send(quarantine_janitor(authority, 0));
}
