//! Garbage collection — the transactional mark-then-acquire fence over the object stores, plus the
//! recovery sweep and the quarantine janitor. The database leads; the filesystem trails.
//!
//! **Scheduling is the composing server's** (this library holds none): the public
//! [`Authority::run_gc`]/[`Authority::run_recovery`]/[`Authority::run_janitor`] wrappers are what it
//! drives, on startup and periodically ([`Authority::workspaces`] enumerates the per-workspace GC
//! targets; recovery and the janitor sweep cross-workspace internally). All three futures are `Send`
//! (the non-`Send` gix `Store` is opened per unlink inside a synchronous helper, never held across an
//! `.await`) so they spawn onto a multi-threaded runtime; a compile-time assertion below pins that.
//!
//! **Clock convention: one server-clock unit = one epoch MILLISECOND**; the stale threshold below is
//! millisecond-valued to match the epoch-ms `now` the composing server stamps.
//!
//! **GC (per workspace):** scan for unrooted `present` objects (advisory), then for each run the
//! three-step fence — **acquire** a guarded `present → deleting` that re-verifies AT DELETE TIME that the
//! object is kept by neither root (a non-purged version's `version_object` edge, a live promotion
//! lease); **unlink** the bytes OUTSIDE any transaction; **finalize** `deleting → absent` (or the
//! terminal `unavailable` for a tombstoned blob). The acquire stamps an ACCURATE wall-clock (the pass's
//! base `now` advanced by its real elapsed) into `status_updated_at`, and that value is the actor's
//! **acquire token**: the unlink first re-confirms ownership of it and the finalize is gated on matching
//! it, so a recovery sweep that takes over a frozen pass can never also unlink/finalize the same row
//! (exactly one actor removes the bytes). Each step is its own short transaction (or none, for the
//! unlink), so no write transaction is held across the filesystem op and a concurrent ingest's
//! lease-insert is never starved. GC acts ONLY on objects with an `object_presence` row.
//!
//! **Recovery sweep:** finalizes a STALE `deleting` (one a crashed GC left behind), each via a
//! one-winner re-acquire that re-verifies the retention surface at delete time (no live version roots
//! it), so a crashed acquire's row a commit re-rooted before recovery runs is spared, not reclaimed. It
//! deliberately does NOT re-check the lease: a lease over a *`deleting`* object is an ingest WAITING
//! for recovery to clear it (its `install_one` re-copies once the row reaches `absent`), so sparing it
//! would strand that waiter — and a lease alone is never readable, so finalizing it loses no readable
//! bytes.
//!
//! **Janitor:** sweeps expired/abandoned staging quarantines, rebuilding the `rm -rf` path from the
//! re-validated ids (never a stored path string) and acquiring each row (flip to `aborted` iff still
//! expired + in-flight) BEFORE the unlink. Op ids are vault-minted fresh per ingest, so an acquired row
//! can never belong to a live ingest under a reused id.
//!
//! **The keep-set re-check is at acquire time, not the physical unlink.** Both the live acquire and the
//! recovery re-acquire re-verify the keep-set inside their transaction, then release the single-writer
//! lock before the out-of-transaction unlink. The ONLY writer of a `version_object` edge is the commit
//! transaction, which holds the candidate's committed lease across the edge write — and
//! `acquire_for_delete` spares any live-leased object — so an edge can never appear in the (acquire,
//! unlink] window for an object the acquire just took: the **lease→edge handoff** closes it by
//! construction. (The live GC also has no `.await`/yield between the acquire and the synchronous unlink,
//! so on a single writer nothing interleaves there.)
//!
//! **Power-loss note (self-healing):** with a relaxed `synchronous_commit` a fsync'd unlink can briefly
//! get ahead of the not-yet-durably-committed acquire/finalize commits, so a crash can leave a `present`
//! row over already-gone bytes. It is reconciled by the next pass (the object is rooted by nothing, so
//! it is re-acquired and the re-unlink tolerates the already-gone object), and no *materializable* root
//! is ever created over it.

use topos_gitstore::LargeObjectStore;

use crate::authority::Authority;
use crate::db::{AcquireOutcome, Location};
use crate::error::Result;
use crate::id::{ObjectId, WorkspaceId};
use crate::lifecycle::elapsed_ms;

/// How long (epoch-ms; one minute) a `deleting` row must sit before the recovery sweep treats it as a
/// crashed GC's leftover. A live `run_gc` stamps every acquire with an accurate wall-clock (it advances `now`
/// by the pass's real elapsed), so a HEALTHY in-flight unlink is never this old and recovery does not race
/// it. A GC frozen longer than this (effectively crashed) IS taken over — and even then the acquire-token
/// fence (`finalize_delete` and `confirm_deleting_owner` both gate on the acquirer's `status_updated_at`)
/// guarantees exactly one actor unlinks + finalizes the row.
const RECOVERY_STALE_MS: i64 = 60 * 1000;

/// Run one GC pass over a workspace. Returns the number of objects reclaimed (acquired → unlinked →
/// finalized) this pass — a bounded result, so a single pass reclaims every currently-unrooted object.
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
        let (location, git_oid) = match authority
            .db()
            .acquire_for_delete(ws, object_id, acquire_now)
            .await?
        {
            AcquireOutcome::Spared => continue,
            AcquireOutcome::Acquired { location, git_oid } => (location, git_oid),
        };
        // Re-confirm ownership IMMEDIATELY before the physical unlink (no `.await` between this and the
        // synchronous delete): if a recovery sweep re-acquired this row (because the pass froze past the stale
        // threshold), the bytes are now that sweeper's to remove — skip, so the two never both unlink and a
        // re-ingest's freshly re-installed bytes are never deleted out from under it.
        if !authority
            .db()
            .confirm_deleting_owner(ws, object_id, acquire_now)
            .await?
        {
            continue;
        }
        // Unlink — remove the bytes OUTSIDE any transaction (the filesystem trails the DB), dispatching on
        // the recorded location: a loose git-object delete, or a large-object-store unlink keyed on the id.
        unlink_object(authority, ws, location, object_id, git_oid)?;
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

/// Finalize every STALE `deleting` object across all workspaces — a crash that left an object mid-unlink.
/// Idempotent: the re-unlink tolerates an already-gone object, and the finalize CAS is guarded. The
/// composing process owns scheduling, but it MUST run this on startup and periodically (≈ every few
/// minutes) so a stranded `deleting` cannot make every re-ingest of that content time out.
pub(crate) async fn recovery_sweep(authority: &Authority, now: i64) -> Result<usize> {
    let older_than = now - RECOVERY_STALE_MS;
    let mut recovered = 0;
    for ws in authority
        .db()
        .workspaces_with_stale_deleting(older_than)
        .await?
    {
        // The stale list is advisory; the per-object acquire is the one-winner guard (mirroring run_gc's
        // acquire) — it keeps the row `deleting` across the unlink and hands the locator to exactly one
        // sweeper, so two concurrent recoveries can't both unlink and an ingest can't reinstall mid-sweep.
        // It ALSO re-verifies the retention surface at delete time, so a stale `deleting` row a commit
        // re-rooted after the crashed acquire is spared rather than unlinked. (It does NOT re-check the
        // lease: a lease over a `deleting` object is a waiting ingest recovery must unblock.)
        for object_id in authority.db().stale_deleting(&ws, older_than).await? {
            // `acquire_stale_for_recovery` stamps `status_updated_at = now`, so `now` is THIS sweeper's acquire
            // token (the value its finalize/owner-check gate on).
            let (location, git_oid) = match authority
                .db()
                .acquire_stale_for_recovery(&ws, object_id, older_than, now)
                .await?
            {
                None => continue, // another sweeper already acquired it (or it was re-rooted / no longer stale)
                Some((location, git_oid)) => (location, git_oid),
            };
            // Re-confirm ownership right before the unlink (no `.await` between): leave the bytes to whoever
            // holds the current token, so the original live GC and this sweeper never both unlink.
            if !authority
                .db()
                .confirm_deleting_owner(&ws, object_id, now)
                .await?
            {
                continue;
            }
            unlink_object(authority, &ws, location, object_id, git_oid)?;
            authority
                .db()
                .finalize_delete(&ws, object_id, now, now)
                .await?;
            recovered += 1;
        }
    }
    Ok(recovered)
}

/// Sweep every expired/abandoned staging quarantine across all workspaces, removing its objdir whole. The
/// destructive `rm -rf` path is REBUILT from the re-validated `(WorkspaceId, OpId)` — never a stored
/// path string — so a poisoned value can never escape the quarantine root. Each candidate is **acquired
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
        // disk-only residual — never a wrongly-swept active quarantine.
        let dir = authority.workspace_quarantine_dir(&ws, &op_id);
        crate::lifecycle::remove_quarantine_dir(&dir);
        swept += 1;
    }
    Ok(swept)
}

/// Unlink one reclaimed object's bytes from the store its `location` names — a loose git-object delete, or a
/// large-object-store unlink keyed on the object id (the large store is per-workspace, so the unlink can
/// only ever touch this workspace's bytes). Both are idempotent (re-unlinking an already-gone object is a
/// no-op), so the recovery sweep's re-run is safe. The DB transitions + the acquire-token fence are unchanged
/// — only the physical target differs by location.
///
/// The non-`Send` gix `Store` is opened INSIDE this synchronous fn, per unlink, and only on a `git`-located
/// one — so the calling futures stay `Send` (never a `Store` across an `.await`), and a workspace that has
/// only `large-local` objects (and therefore may have no git repo yet) never tries to open one. The unlink
/// deliberately runs inline (never on the blocking pool): the owner-check → unlink step must have no
/// `.await` between them, and a single loose-object delete is small.
pub(crate) fn unlink_object(
    authority: &Authority,
    ws: &WorkspaceId,
    location: Location,
    object_id: ObjectId,
    git_oid: [u8; 20],
) -> Result<()> {
    match location {
        Location::Git => {
            authority
                .open_store(ws)?
                .delete_loose_object(git_oid)
                .map_err(crate::error::AuthorityError::internal)?;
        }
        Location::LargeLocal => {
            authority
                .large_store(ws)
                .delete(object_id.0)
                .map_err(crate::error::AuthorityError::internal)?;
        }
    }
    Ok(())
}

/// Compile-time pin: the three GC entry points' futures are `Send`, so the composing server can spawn them
/// onto a multi-threaded runtime. Fails to compile if a non-`Send` value (the gix `Store`) is ever again
/// held across an `.await` in any of them. Never called — the assertion is the compilation itself.
#[cfg(test)]
#[allow(dead_code)]
fn assert_gc_futures_are_send(authority: &Authority, ws: &WorkspaceId) {
    fn assert_send<T: Send>(_: T) {}
    assert_send(run_gc(authority, ws, 0));
    assert_send(recovery_sweep(authority, 0));
    assert_send(quarantine_janitor(authority, 0));
}
