//! The consent-satisfier truth-table, as a pure decision function.
//!
//! Given the situation a pull faces, this returns which satisfier (if any) authorizes applying the
//! new bytes, and what the client does. The whole point: **disclosure + integrity**, never a second
//! permission system. Posture (how much a human sits in the loop) is the harness's job — it is folded
//! into the [`Situation`] rows here, not modelled as a separate `topos` mode.
//!
//! The integrity floor applies to *every* apply, including `auto`: the byte-exact `bundle_digest` is
//! re-disclosed and re-bound on every pull ([`Decision::rebinds_digest`]). That is an integrity check,
//! not a fresh human ask — it simply never silently trusts bytes it didn't recompute.

/// The situation a pull faces. Each row folds in the relevant posture and skill-newness; this is the
/// closed set of the truth-table's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Situation {
    /// 1 · First receive from an `/i/` link (TOFU).
    FirstReceiveFromLink,
    /// 2 · A new version of a followed skill, `auto` armed.
    FollowedAutoNewVersion,
    /// 3 · A followed skill in confirm-each / `--manual`.
    FollowedConfirmEach,
    /// 4 · A `review-required` workspace, an **approved** proposal.
    ReviewRequiredApproved,
    /// 5 · A `review-required` workspace, an **unapproved** candidate.
    ReviewRequiredUnapproved,
    /// 6 · A peer / foreign skill, not previously followed.
    PeerForeignSkill,
    /// 7 · The recomputed digest ≠ the disclosed approval.
    DigestMismatch,
    /// 8 · The signed pointer fails, or the generation goes backward.
    PointerVerifyFailure,
    /// 9 · A local draft exists and the remote is newer.
    LocalDraftVsNewerRemote,
    /// 10 · An explicit local `pull <skill>@<hash>`.
    ExplicitLocalPull,
    /// 11 · A team `revert` on a followed skill.
    TeamRevertFollowed,
}

/// Which satisfier authorizes the apply (mlp-spec's three, plus the explicit-local and none cases).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Satisfier {
    /// (a) A direct human yes on the digest.
    DirectHuman,
    /// (b) The standing-following pre-authorization recorded once at `follow` (`auto`'s trust basis).
    StandingFollow,
    /// (c) A reviewer's delegated approval (only under `review-required`).
    ReviewerDelegated,
    /// An explicit local command the user typed — its own authorization.
    DirectLocalCommand,
    /// Nothing authorizes an apply.
    None,
}

/// What the client does as a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Offer; never auto-land (TOFU first-receive).
    OfferNeverAutoLand,
    /// Re-disclose + re-bind the digest, then apply (no fresh prompt).
    Apply,
    /// A one-tap offer (confirm-each).
    OneTapOffer,
    /// Apply with no local prompt; bytes are still re-verified (reviewer-delegated).
    ApplyNoLocalPrompt,
    /// `current` does not move.
    CurrentDoesNotMove,
    /// Refuse (a hard integrity stop).
    Refuse,
    /// Refuse and retain the last-known-good (pointer/generation failure).
    RefuseRetainLastGood,
    /// Snapshot the draft and open the DIVERGED panel.
    SnapshotDiverged,
    /// Materialize locally without lowering `observed`.
    MaterializeLocal,
    /// Treat as a newer forward generation (a team revert is a roll-*forward*).
    TreatAsForwardGeneration,
}

/// The truth-table's verdict for a situation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub satisfier: Satisfier,
    pub outcome: Outcome,
}

impl Decision {
    /// Whether this verdict actually applies new bytes (and therefore must re-bind the digest as the
    /// integrity floor). Refusals and "does not move" verdicts apply nothing.
    pub fn applies_bytes(self) -> bool {
        matches!(
            self.outcome,
            Outcome::Apply
                | Outcome::ApplyNoLocalPrompt
                | Outcome::MaterializeLocal
                | Outcome::TreatAsForwardGeneration
        )
    }

    /// The integrity floor: any apply re-discloses and re-binds the byte-exact digest before writing.
    pub fn rebinds_digest(self) -> bool {
        self.applies_bytes()
    }
}

/// The consent truth-table: the single decision the kernel makes about whether a pull may apply.
pub fn decide(situation: Situation) -> Decision {
    use Outcome::*;
    use Satisfier::*;
    let (satisfier, outcome) = match situation {
        Situation::FirstReceiveFromLink => (DirectHuman, OfferNeverAutoLand),
        Situation::FollowedAutoNewVersion => (StandingFollow, Apply),
        Situation::FollowedConfirmEach => (DirectHuman, OneTapOffer),
        Situation::ReviewRequiredApproved => (ReviewerDelegated, ApplyNoLocalPrompt),
        Situation::ReviewRequiredUnapproved => (None, CurrentDoesNotMove),
        Situation::PeerForeignSkill => (DirectHuman, OfferNeverAutoLand),
        Situation::DigestMismatch => (None, Refuse),
        Situation::PointerVerifyFailure => (None, RefuseRetainLastGood),
        Situation::LocalDraftVsNewerRemote => (None, SnapshotDiverged),
        Situation::ExplicitLocalPull => (DirectLocalCommand, MaterializeLocal),
        // The contract allows "(b) or (c)" here; a follower's own pull never holds a reviewer
        // delegation, so the standing-follow pre-auth is the correct satisfier for the pull-side
        // decision. (The reviewer's (c) lived on the plane when the revert was approved.)
        Situation::TeamRevertFollowed => (StandingFollow, TreatAsForwardGeneration),
    };
    Decision { satisfier, outcome }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row of the truth-table, asserted exactly as the contract specifies it.
    #[test]
    fn truth_table_matches_the_contract() {
        let rows = [
            (
                Situation::FirstReceiveFromLink,
                Satisfier::DirectHuman,
                Outcome::OfferNeverAutoLand,
            ),
            (
                Situation::FollowedAutoNewVersion,
                Satisfier::StandingFollow,
                Outcome::Apply,
            ),
            (
                Situation::FollowedConfirmEach,
                Satisfier::DirectHuman,
                Outcome::OneTapOffer,
            ),
            (
                Situation::ReviewRequiredApproved,
                Satisfier::ReviewerDelegated,
                Outcome::ApplyNoLocalPrompt,
            ),
            (
                Situation::ReviewRequiredUnapproved,
                Satisfier::None,
                Outcome::CurrentDoesNotMove,
            ),
            (
                Situation::PeerForeignSkill,
                Satisfier::DirectHuman,
                Outcome::OfferNeverAutoLand,
            ),
            (Situation::DigestMismatch, Satisfier::None, Outcome::Refuse),
            (
                Situation::PointerVerifyFailure,
                Satisfier::None,
                Outcome::RefuseRetainLastGood,
            ),
            (
                Situation::LocalDraftVsNewerRemote,
                Satisfier::None,
                Outcome::SnapshotDiverged,
            ),
            (
                Situation::ExplicitLocalPull,
                Satisfier::DirectLocalCommand,
                Outcome::MaterializeLocal,
            ),
            (
                Situation::TeamRevertFollowed,
                Satisfier::StandingFollow,
                Outcome::TreatAsForwardGeneration,
            ),
        ];
        for (situation, satisfier, outcome) in rows {
            let d = decide(situation);
            assert_eq!(d.satisfier, satisfier, "satisfier for {situation:?}");
            assert_eq!(d.outcome, outcome, "outcome for {situation:?}");
        }
    }

    #[test]
    fn tofu_never_auto_lands() {
        // A standing-follow pre-auth can NEVER satisfy a first-receive — only a direct human can.
        for s in [Situation::FirstReceiveFromLink, Situation::PeerForeignSkill] {
            let d = decide(s);
            assert_eq!(d.satisfier, Satisfier::DirectHuman);
            assert_eq!(d.outcome, Outcome::OfferNeverAutoLand);
            assert!(!d.applies_bytes());
        }
    }

    #[test]
    fn integrity_floor_holds_on_every_apply() {
        // Whenever bytes are applied — including auto and reviewer-delegated — the digest is re-bound.
        for s in [
            Situation::FollowedAutoNewVersion,
            Situation::ReviewRequiredApproved,
            Situation::ExplicitLocalPull,
            Situation::TeamRevertFollowed,
        ] {
            assert!(
                decide(s).rebinds_digest(),
                "{s:?} applies bytes without re-binding the digest"
            );
        }
    }

    #[test]
    fn integrity_failures_refuse() {
        assert_eq!(decide(Situation::DigestMismatch).outcome, Outcome::Refuse);
        assert_eq!(
            decide(Situation::PointerVerifyFailure).outcome,
            Outcome::RefuseRetainLastGood
        );
        assert!(!decide(Situation::DigestMismatch).applies_bytes());
        assert!(!decide(Situation::PointerVerifyFailure).applies_bytes());
    }
}
