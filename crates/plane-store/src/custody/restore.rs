//! The operator backup/restore **epoch bump** — `restore-bump-epochs`, the orchestration half.
//!
//! After a plane's database is restored from a backup, every `current` pointer may have moved BACKWARD:
//! the restored rows can re-issue an `(epoch, seq)` tuple that was already served naming DIFFERENT bytes
//! once the team publishes again. That reuse is a CONCURRENCY hazard, not a trust one: the open-proposal
//! staleness predicate compares `(base_epoch, base_seq)` against the live pair, in-flight client writes
//! target an expected pair, and conditional GETs key on it — a reused tuple could make a pre-restore
//! proposal look fresh against a post-restore pointer that is not its base. The recovery move is
//! epoch-dominant ordering's designed escape hatch: rewrite every restored pointer **one epoch forward** —
//! the SAME commit, the SAME `seq` — so every tuple issued after the restore is new and ordinary forward
//! sync resumes.
//!
//! This is an operator helper, not a protocol change: it touches ONLY the `current` table (no receipt, no
//! provenance, no proposal, no generation-advance logic changes anywhere else). Running it twice bumps
//! twice — one more ordinary forward move for followers — so it is deliberately unguarded against a re-run.

use topos_types::Generation;

use crate::authority::Authority;
use crate::error::Result;
use crate::id::{CommitId, SkillId, WorkspaceId};

/// One bumped `current` pointer, as [`Authority::restore_bump_epochs`] reports it: the scope, the commit
/// the pointer names (UNCHANGED by the bump), and the old and new generations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochBumpReport {
    /// The workspace the bumped pointer belongs to.
    pub workspace_id: WorkspaceId,
    /// The skill whose `current` was bumped.
    pub skill_id: SkillId,
    /// The commit the pointer names — byte-identical before and after the bump.
    pub commit: CommitId,
    /// The generation the row held when it was locked.
    pub old: Generation,
    /// The bumped generation: `epoch = max(old.epoch + 1, epoch_at_least)`, `seq` unchanged.
    pub new: Generation,
}

impl Authority {
    /// Bump every selected skill's `current` pointer one epoch forward — the operator recovery step
    /// after restoring the plane's database from a backup. Per row: `new_epoch = max(epoch + 1,
    /// epoch_at_least)` (the floor lets an operator who restored once before, from an even older backup,
    /// jump past every epoch ever served), the commit and `seq` unchanged — epoch-dominant ordering makes
    /// `(e+1, s)` beat `(e, anything)`, so every follower's next pull is an ordinary forward move, and the
    /// next publish lands at `(new_epoch, s + 1)`.
    ///
    /// One `SERIALIZABLE` transaction over the WHOLE selection (`workspaces = None` ⇒ every workspace),
    /// with the selected rows locked `FOR UPDATE`: a concurrent publish either lands first (the
    /// serialization retry re-reads and bumps the new row) or CONFLICTs normally against the bumped pair —
    /// no torn state. The runbook still says stop the plane first. All-or-nothing: a bump that would exceed
    /// the I-JSON safe-integer bound (2^53 − 1) fails typed with nothing written.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`](crate::AuthorityError::Internal) if a bumped epoch would exceed the
    /// safe-integer bound or on a database fault;
    /// [`AuthorityError::Integrity`](crate::AuthorityError::Integrity) if a stored row is corrupt.
    pub async fn restore_bump_epochs(
        &self,
        workspaces: Option<&[WorkspaceId]>,
        epoch_at_least: Option<u64>,
        now: i64,
    ) -> Result<Vec<EpochBumpReport>> {
        self.db()
            .restore_bump_epochs_txn(workspaces, epoch_at_least, now)
            .await
    }
}
