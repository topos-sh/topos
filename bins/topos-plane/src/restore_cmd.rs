//! The backup/restore epoch bump — the leak-free core of the bin's `restore-bump-epoch` subcommand.
//!
//! After the plane's database is restored from a backup, `current` can sit at an `(epoch, seq)` a later
//! publish already reused under different bytes — a reused tuple that would confuse the proposal-staleness
//! predicate and any in-flight CAS / conditional GET. [`PlaneState::restore_bump_epochs`] rewrites every
//! selected pointer ONE EPOCH FORWARD (same commit, same seq), so every stored generation is fresh again and
//! ordinary forward sync resumes. Exposed on [`PlaneState`] like
//! [`set_review_required`](PlaneState::set_review_required): plain/owned types in and out, both failure
//! modes stringified — a composing plane (or the OSS bin) drives it without naming a `plane_store` type.

use plane_store::WorkspaceId;

use crate::state::PlaneState;
use crate::wire;

/// One bumped `current` pointer, as the restore bump reports it — plain/owned fields only (the leak-free
/// mirror of the authority's report; the bin prints it, a composing plane may log it).
#[derive(Debug, Clone)]
pub struct EpochBumpSummary {
    /// The workspace the bumped pointer belongs to.
    pub workspace_id: String,
    /// The skill whose `current` was bumped.
    pub skill_id: String,
    /// The commit the pointer names (UNCHANGED by the bump), lowercase hex.
    pub commit_hex: String,
    /// The epoch the row held before the bump.
    pub old_epoch: u64,
    /// The seq the row held before the bump (preserved by it).
    pub old_seq: u64,
    /// The bumped epoch: `max(old_epoch + 1, --epoch-at-least)`.
    pub new_epoch: u64,
    /// The bumped seq — identical to `old_seq` (the next publish lands at `(new_epoch, old_seq + 1)`).
    pub new_seq: u64,
}

impl PlaneState {
    /// Bump every selected skill's `current` pointer one epoch forward — the operator recovery step
    /// after a database restore. `workspaces = None` bumps every workspace on the plane; `Some(ids)` only
    /// the named ones (each id parsed here at the edge — a malformed one fails with a clear error naming
    /// the bad value, touching nothing). `epoch_at_least` floors the new epoch (max semantics), for an
    /// operator who restored once before from an even older backup. One serializable transaction over the
    /// whole selection; running it twice bumps twice (harmless — every stored generation just moves forward).
    ///
    /// This is an OPERATOR capability, deliberately public on the lib (the bin's subcommand needs it,
    /// like [`PlaneState::set_review_required`]): it rewrites the stored record directly — no device
    /// credential, no review gate, no receipt row (the returned report, printed by the subcommand, is
    /// the audit trail). A downstream composition must wire it only to operator surfaces, never to a
    /// request handler.
    ///
    /// # Errors
    /// Returns an [`anyhow::Error`] if a workspace id is invalid, a bumped epoch would exceed the
    /// safe-integer bound (nothing written), or the authority write fails.
    pub async fn restore_bump_epochs(
        &self,
        workspaces: Option<&[String]>,
        epoch_at_least: Option<u64>,
    ) -> anyhow::Result<Vec<EpochBumpSummary>> {
        let parsed: Option<Vec<WorkspaceId>> = match workspaces {
            None => None,
            Some(ids) => Some(
                ids.iter()
                    .map(|id| {
                        WorkspaceId::parse(id).map_err(|error| {
                            anyhow::anyhow!("invalid workspace id `{id}`: {error}")
                        })
                    })
                    .collect::<anyhow::Result<_>>()?,
            ),
        };
        let (_, now) = wire::now_utc();
        let reports = self
            .authority()
            .restore_bump_epochs(parsed.as_deref(), epoch_at_least, now)
            .await
            .map_err(|error| anyhow::anyhow!("bumping restored current pointers: {error}"))?;
        Ok(reports
            .into_iter()
            .map(|r| EpochBumpSummary {
                workspace_id: r.workspace_id.as_str().to_owned(),
                skill_id: r.bundle_id.as_str().to_owned(),
                commit_hex: topos_core::digest::to_hex(r.commit.as_bytes()),
                old_epoch: r.old.epoch,
                old_seq: r.old.seq,
                new_epoch: r.new.epoch,
                new_seq: r.new.seq,
            })
            .collect())
    }
}
