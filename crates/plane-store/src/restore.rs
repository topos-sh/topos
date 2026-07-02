//! The operator backup/restore **epoch bump** — `restore-bump-epochs`, the orchestration half.
//!
//! After a plane's database is restored from a backup, every `current` pointer may have moved BACKWARD:
//! the restored rows can re-issue an `(epoch, seq)` tuple that followers already recorded — and once the
//! team publishes again, that reused tuple names DIFFERENT bytes, which every follower's anti-rollback
//! floor surfaces as a loud reused-tuple ALARM (nothing is ever clobbered, but the fleet wedges on manual
//! attention). The recovery move is epoch-dominant ordering's designed escape hatch: re-sign every restored
//! pointer **one epoch forward** — the SAME commit, the SAME `seq` — so the next record every follower sees
//! is *strictly higher* and ordinary forward sync resumes, whether they had already alarmed or not.
//!
//! This is an operator helper, not a protocol change: it touches ONLY the `current` table (no receipt, no
//! provenance, no proposal, no generation-advance logic changes anywhere else). Running it twice bumps
//! twice — one more ordinary forward move for followers — so it is deliberately unguarded against a re-run.

use topos_types::Generation;

use crate::authority::Authority;
use crate::error::Result;
use crate::id::{CommitId, SkillId, WorkspaceId};

/// One re-signed `current` pointer, as [`Authority::restore_bump_epochs`] reports it: the scope, the commit
/// the pointer names (UNCHANGED by the bump), the old and new generations, and the signing key id — the
/// operator's tripwire that the restored data directory still holds the pre-incident plane key (a different
/// `key_id` here means followers pinned another key and will refuse the re-signed records).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochBumpReport {
    /// The workspace the bumped pointer belongs to.
    pub workspace_id: WorkspaceId,
    /// The skill whose `current` was re-signed.
    pub skill_id: SkillId,
    /// The commit the pointer names — byte-identical before and after the bump.
    pub commit: CommitId,
    /// The generation the row held when it was locked.
    pub old: Generation,
    /// The re-signed generation: `epoch = max(old.epoch + 1, epoch_at_least)`, `seq` unchanged.
    pub new: Generation,
    /// The plane signing key id the fresh signature carries.
    pub key_id: String,
}

impl Authority {
    /// Re-sign every selected skill's `current` pointer one epoch forward — the operator recovery step
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
    /// the JCS safe-integer bound (2^53 − 1) fails typed with nothing written or signed.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`](crate::AuthorityError::Internal) if no plane key is configured
    /// ([`with_plane_key`](Self::with_plane_key)), a bumped epoch would exceed the safe-integer bound, or on
    /// a database fault; [`AuthorityError::Integrity`](crate::AuthorityError::Integrity) if a stored row is
    /// corrupt.
    pub async fn restore_bump_epochs(
        &self,
        workspaces: Option<&[WorkspaceId]>,
        epoch_at_least: Option<u64>,
        now: i64,
    ) -> Result<Vec<EpochBumpReport>> {
        let signer = self.plane_signer()?;
        self.db()
            .restore_bump_epochs_txn(workspaces, epoch_at_least, now, signer)
            .await
    }
}
