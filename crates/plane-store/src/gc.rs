//! Garbage collection — the transactional mark-then-claim fence over the git object store, plus the
//! recovery sweep and the quarantine janitor. The database leads; the filesystem trails.
//!
//! **GC (per workspace):** scan `present` objects (advisory), then for each run the three-step fence —
//! **claim** a guarded `present → deleting` that re-verifies AT DELETE TIME that the object is referenced by
//! no commit (the read-authorization surface) and named by no live lease; **unlink** the loose object
//! OUTSIDE any transaction; **finalize** `deleting → absent`. The claim stamps an ACCURATE wall-clock (the
//! pass's base `now` advanced by its real elapsed) into `status_updated_at`, and that value is the actor's
//! **claim token**: the unlink first re-confirms ownership of it and the finalize is gated on matching it, so
//! a recovery sweep that takes over a frozen pass can never also unlink/finalize the same row (exactly one
//! actor removes the bytes). Each step is its own short transaction (or none, for the unlink), so no
//! write transaction is held across the filesystem op and a concurrent migrate's lease-insert is never
//! starved. GC acts ONLY on objects with an `object_presence` row — every migrated object has one, and an
//! object with no presence row is invisible to it.
//!
//! **Recovery sweep:** finalizes a STALE `deleting` (one a crashed GC left behind), each via a one-winner
//! re-claim that re-verifies the **read-authorization surface at delete time** (no `commit_object` trunk edge
//! AND no open-non-stale `proposal_object` root), so a crashed claim's row a commit edge or a pending proposal
//! re-rooted before recovery runs is spared, not reclaimed.
//! It deliberately does NOT re-check the lease: a lease over a *`deleting`* object is a migrate WAITING for
//! recovery to clear it (its `install_one` re-copies once the row reaches `absent`), so sparing it would
//! strand that waiter — and a lease alone is never readable, so finalizing it loses no readable bytes. Two
//! concurrent sweeps never both unlink, and — because a live `run_gc` now stamps accurate claim timestamps —
//! recovery only re-claims a row that is genuinely stale (a crashed/frozen GC); even then the claim-token
//! fence (the re-claim bumps `status_updated_at`, which the original claimant's owner-check then fails) means
//! exactly one actor unlinks. **Janitor:** sweeps expired/abandoned quarantines, rebuilding the
//! `rm -rf` path from the re-validated ids (never trusting the stored objdir string) and claiming each row
//! (delete-if-still-expired) BEFORE the unlink, so a re-ingest that reused the op id is never swept out from
//! under its in-flight migrate.
//!
//! **The keep-set re-check is at claim time, not the physical unlink.** Both the live claim and the recovery
//! re-claim re-verify the keep-set inside their transaction, then release the single-writer lock before the
//! out-of-transaction unlink. The ONLY writer of a `commit_object` edge is the promote/handoff path
//! (publish/revert/approve), which holds the candidate's lease across the edge write — and `claim_for_delete`
//! spares any live-leased object — so an edge can never appear in the (claim, unlink] window for an object the
//! claim just took: the **lease→edge handoff** closes it by construction. A pending proposal's
//! `proposal_object` root is likewise gated on the live `current` and re-checked at claim time. (The live GC
//! also has no `.await`/yield between the claim and the synchronous unlink, so on a single writer nothing
//! interleaves there.)
//!
//! **Power-loss note (self-healing):** with a relaxed `synchronous_commit` a fsync'd unlink can briefly get
//! ahead of the not-yet-durably-committed claim/finalize commits, so a crash can leave a `present` row over
//! already-gone bytes. It is reconciled by the next pass (the object is rooted by nothing, so it is
//! re-claimed and the re-unlink tolerates the already-gone object), and no *materializable* root is ever
//! created over it. When the pointer-move makes a long-idle `present` row load-bearing for dedup-reuse, the
//! destructive transitions should be made power-durable (a `synchronous_commit = on` / WAL-checkpoint barrier).

use topos_gitstore::{LargeObjectStore, Store};

use crate::authority::Authority;
use crate::db::{ClaimOutcome, Location};
use crate::error::Result;
use crate::id::{ObjectId, WorkspaceId};

/// How long a `deleting` row must sit before the recovery sweep treats it as a crashed GC's leftover. A live
/// `run_gc` stamps every claim with an accurate wall-clock (it advances `now` by the pass's real elapsed), so
/// a HEALTHY in-flight unlink is never this old and recovery does not race it. A GC frozen longer than this
/// (effectively crashed) IS taken over — and even then the claim-token fence (`finalize_delete` and
/// `confirm_deleting_owner` both gate on the claimant's `status_updated_at`) guarantees exactly one actor
/// unlinks + finalizes the row, so the takeover never collides with the original.
const RECOVERY_STALE_SECS: i64 = 60;

/// Run one GC pass over a workspace. Returns the number of objects reclaimed (claimed → unlinked →
/// finalized) this pass — a bounded result, so a single pass reclaims every currently-unrooted object.
pub(crate) async fn run_gc(authority: &Authority, ws: &WorkspaceId, now: i64) -> Result<usize> {
    let candidates = authority.db().present_objects(ws).await?;
    if candidates.is_empty() {
        return Ok(0);
    }
    // The git store is opened LAZILY (cached), only when a `git`-located object is actually unlinked — so a
    // workspace whose first migrate routed every blob to the large store (and crashed before `migrate_finish`
    // created the git repo) can still have its large-local rows reclaimed instead of aborting on a missing repo.
    let mut git_store: Option<Store> = None;
    let started = tokio::time::Instant::now();
    let mut reclaimed = 0;
    for object_id in candidates {
        // Stamp each claim with an ACCURATE wall-clock — `now` advanced by this pass's real elapsed, never
        // the pass-fixed `now` — so a long pass does not back-date a late claim. A back-dated `deleting` row
        // would look older than RECOVERY_STALE_SECS the instant it is claimed, and a concurrently-scheduled
        // recovery sweep would wrongly take it for a crashed-GC leftover and re-claim it. (Same `now +
        // monotonic elapsed` trick as `lifecycle::migrate`; the base `now` is still caller-supplied.) This
        // `claim_now` is also this actor's CLAIM TOKEN (the value the claim stamps into `status_updated_at`).
        let claim_now = now.saturating_add(started.elapsed().as_secs() as i64);
        // Claim — the guarded `present → deleting` (its own short txn; releases the write lock at once).
        // `location` selects which store the unlink targets; the claim's keep-set re-check is unchanged.
        let (location, git_oid) = match authority
            .db()
            .claim_for_delete(ws, object_id, claim_now)
            .await?
        {
            ClaimOutcome::Spared => continue,
            ClaimOutcome::Claimed { location, git_oid } => (location, git_oid),
        };
        // Re-confirm ownership IMMEDIATELY before the physical unlink (no `.await` between this and the
        // synchronous delete): if a recovery sweep re-claimed this row (because the pass froze past the stale
        // threshold), the bytes are now that sweeper's to remove — skip, so the two never both unlink and a
        // re-migrate's freshly re-installed bytes are never deleted out from under it.
        if !authority
            .db()
            .confirm_deleting_owner(ws, object_id, claim_now)
            .await?
        {
            continue;
        }
        // Unlink — remove the bytes OUTSIDE any transaction (the filesystem trails the DB), dispatching on
        // the recorded location: a loose git-object delete, or a large-object-store unlink keyed on the id.
        unlink_object(authority, ws, &mut git_store, location, object_id, git_oid)?;
        // Finalize — `deleting → absent` (its own short transaction), GATED on this actor's claim token so a
        // row a recovery sweep re-claimed is never finalized out from under it.
        let finalize_now = now.saturating_add(started.elapsed().as_secs() as i64);
        authority
            .db()
            .finalize_delete(ws, object_id, claim_now, finalize_now)
            .await?;
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
        // Opened lazily (cached), only for a `git`-located unlink — so a workspace with only large-local
        // rows and no git repo still recovers (see `run_gc`).
        let mut git_store: Option<Store> = None;
        // The stale list is advisory; the per-object claim is the one-winner guard (mirroring run_gc's
        // claim) — it keeps the row `deleting` across the unlink and hands the git locator to exactly one
        // sweeper, so two concurrent recoveries can't both unlink and a migrate can't reinstall mid-sweep.
        // It ALSO re-verifies the read-authorization surface at delete time (a `commit_object` trunk edge OR
        // an open-non-stale `proposal_object` root), so a stale `deleting` row that a promote or a pending
        // proposal re-rooted after the crashed claim is spared rather than unlinked — closing the recovery
        // byte-loss path. (It does NOT re-check the lease: a lease over a `deleting` object is a waiting
        // migrate recovery must unblock, not a reason to spare.)
        for object_id in authority.db().stale_deleting(&ws, older_than).await? {
            // `claim_stale_for_recovery` stamps `status_updated_at = now`, so `now` is THIS sweeper's claim
            // token (the value its finalize/owner-check gate on).
            let (location, git_oid) = match authority
                .db()
                .claim_stale_for_recovery(&ws, object_id, older_than, now)
                .await?
            {
                None => continue, // another sweeper already claimed it (or it was re-rooted / no longer stale)
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
            unlink_object(authority, &ws, &mut git_store, location, object_id, git_oid)?;
            authority
                .db()
                .finalize_delete(&ws, object_id, now, now)
                .await?;
            recovered += 1;
        }
    }
    Ok(recovered)
}

/// Sweep every expired/abandoned upload quarantine across all workspaces, removing its objdir whole. The
/// destructive `rm -rf` path is REBUILT from the re-validated `(WorkspaceId, OpId)` — never the stored
/// `objdir` string — so a poisoned path string can never escape the quarantine root. Each candidate is
/// **claimed (delete-if-still-expired) before the unlink**, so a re-ingest that reused the op id and
/// refreshed the row's expiry is never swept out from under its in-flight migrate.
pub(crate) async fn quarantine_janitor(authority: &Authority, now: i64) -> Result<usize> {
    let mut swept = 0;
    for ws in authority
        .db()
        .workspaces_with_expired_quarantine(now)
        .await?
    {
        for op_id in authority.db().expired_quarantine_ops(&ws, now).await? {
            // Atomically CLAIM the expired slot before touching the filesystem: this removes the tracking
            // row only if it is STILL expired, so a concurrent re-ingest that reused this op id (refreshing
            // `expires_at` into the future) is NOT claimed and its active, re-staged quarantine is spared.
            // Only the winner proceeds to the unlink.
            if !authority
                .db()
                .claim_expired_quarantine(&ws, &op_id, now)
                .await?
            {
                continue;
            }
            // Rebuild the rm -rf path from the re-validated ids (never the stored objdir string) and remove
            // the dir whole. The row is already gone, so a transient rm failure leaves only an orphan dir —
            // the same low-severity, disk-only residual a lost-WAL tracking row leaves (see
            // `lifecycle::ingest`) — never a wrongly-swept active quarantine.
            let dir = authority.workspace_quarantine_dir(&ws, &op_id);
            crate::lifecycle::remove_quarantine_dir(&dir);
            swept += 1;
        }
    }
    Ok(swept)
}

/// Unlink one reclaimed object's bytes from the store its `location` names — a loose git-object delete, or a
/// large-object-store unlink keyed on the object id (the large store is per-workspace, so the unlink can
/// only ever touch this workspace's bytes). Both are idempotent (re-unlinking an already-gone object is a
/// no-op), so the recovery sweep's re-run is safe. The DB transitions + the claim-token fence are unchanged
/// — only the physical target differs by location.
///
/// The git store is opened **lazily** into `git_store` (cached for the pass) on the first `git` unlink, so a
/// workspace that has only `large-local` objects — and therefore may have no git repo yet — never tries to
/// open one.
fn unlink_object(
    authority: &Authority,
    ws: &WorkspaceId,
    git_store: &mut Option<Store>,
    location: Location,
    object_id: ObjectId,
    git_oid: [u8; 20],
) -> Result<()> {
    match location {
        Location::Git => {
            let store = match git_store {
                Some(s) => s,
                None => git_store.insert(authority.open_store(ws)?),
            };
            store
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
