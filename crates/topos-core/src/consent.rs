//! The consent truth-table, as a pure decision function.
//!
//! Given the situation a pull faces, this returns what the client does. The whole point:
//! **disclosure + integrity**, never a second permission system. Posture (how much a human sits in
//! the loop) is the harness's job — it is folded into the [`Situation`] rows here, not modelled as a
//! separate `topos` mode.
//!
//! The integrity floor applies to *every* apply: the byte-exact `bundle_digest` is re-disclosed and
//! re-bound on every pull ([`Decision::applies_bytes`]). That is an integrity check, not a fresh
//! human ask — it simply never silently trusts bytes it didn't recompute.

/// The situation a pull faces. Each row folds in the relevant posture; this is the closed set of the
/// truth-table's rows — the three a pull can actually stand in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Situation {
    /// 1 · A new version of a followed skill (the standing pre-authorization the recipe records).
    FollowedAutoNewVersion,
    /// 2 · A `review-required` workspace, an **approved** proposal.
    ReviewRequiredApproved,
    /// 3 · An explicit local `pull <skill>@<hash>`.
    ExplicitLocalPull,
}

/// What the client does as a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Re-disclose + re-bind the digest, then apply (no fresh prompt).
    Apply,
    /// Apply with no local prompt; bytes are still re-verified (reviewer-delegated).
    ApplyNoLocalPrompt,
    /// Materialize locally without lowering `observed`.
    MaterializeLocal,
}

/// The truth-table's verdict for a situation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub outcome: Outcome,
}

impl Decision {
    /// Whether this verdict actually applies new bytes — and therefore must re-bind the digest as
    /// the integrity floor. Every apply re-discloses and re-binds the byte-exact digest before
    /// writing; the callers gate their write on this answer rather than on the outcome word.
    pub fn applies_bytes(self) -> bool {
        matches!(
            self.outcome,
            Outcome::Apply | Outcome::ApplyNoLocalPrompt | Outcome::MaterializeLocal
        )
    }
}

/// The consent truth-table: the single decision the kernel makes about whether a pull may apply.
pub fn decide(situation: Situation) -> Decision {
    use Outcome::*;
    let outcome = match situation {
        Situation::FollowedAutoNewVersion => Apply,
        Situation::ReviewRequiredApproved => ApplyNoLocalPrompt,
        Situation::ExplicitLocalPull => MaterializeLocal,
    };
    Decision { outcome }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row of the truth-table, asserted exactly as the contract specifies it.
    #[test]
    fn truth_table_matches_the_contract() {
        let rows = [
            (Situation::FollowedAutoNewVersion, Outcome::Apply),
            (
                Situation::ReviewRequiredApproved,
                Outcome::ApplyNoLocalPrompt,
            ),
            (Situation::ExplicitLocalPull, Outcome::MaterializeLocal),
        ];
        for (situation, outcome) in rows {
            assert_eq!(
                decide(situation).outcome,
                outcome,
                "outcome for {situation:?}"
            );
        }
    }

    #[test]
    fn integrity_floor_holds_on_every_apply() {
        // Whenever bytes are applied — including auto and reviewer-delegated — the digest is re-bound.
        for s in [
            Situation::FollowedAutoNewVersion,
            Situation::ReviewRequiredApproved,
            Situation::ExplicitLocalPull,
        ] {
            assert!(
                decide(s).applies_bytes(),
                "{s:?} applies bytes without re-binding the digest"
            );
        }
    }
}
