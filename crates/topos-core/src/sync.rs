//! The pure sync transition: the four sync states and the post-fetch heal.
//!
//! Every decision here is a deterministic function over EXPLICIT VALUES — no I/O, no clock, no RNG (the
//! kernel constraints). The client engine maps live disk state into these inputs, calls the functions,
//! and acts on the verdicts. This module never re-decides *consent* (that is [`crate::consent`], the one
//! policy) — it only classifies the sync situation that selects which `consent::Situation` the engine
//! feeds to `decide()`. There is no rollback floor: the served pointer is the sync target, its
//! integrity is the content-addressed version id re-verified by digest on apply.

/// The four sync states, derived from `(work == base?)` and `(applied == observed?)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    /// ① clean follower, caught up — nothing to do.
    Current,
    /// ② clean follower, an update is pending — fast-forward (auto) / one-tap accept (confirm-each).
    Behind,
    /// ③ local edits, caught up — a draft ahead of `current`; never nagged.
    Draft,
    /// ④ local edits AND an update is pending — diverged; snapshot + surface, never auto-clobber.
    Diverged,
}

/// The 2×2 sync state, over the two booleans the engine computes from disk. Pure; no fetch — `Current`
/// and `Draft` mean there is nothing to apply (no fetch needed); `Behind`/`Diverged` drive a fetch.
#[must_use]
pub fn decide_state(work_eq_base: bool, applied_eq_observed: bool) -> SyncStatus {
    match (work_eq_base, applied_eq_observed) {
        (true, true) => SyncStatus::Current,
        (true, false) => SyncStatus::Behind,
        (false, true) => SyncStatus::Draft,
        (false, false) => SyncStatus::Diverged,
    }
}

/// How an `applied != observed` skill should apply, refined once the TARGET bytes are known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyClass {
    /// The working tree already equals the target bytes — a completed-but-unrecorded apply (a crash
    /// after the swap, or an idempotent re-pull). Advance `applied` with NO second swap.
    AlreadyAtTarget,
    /// A clean follower behind `current` — materialize the target.
    CleanForward,
    /// The working tree matches neither base nor target — a genuine local draft vs a newer remote.
    Diverged,
}

/// Refine an `applied != observed` skill once the target's digest is known (after the fetch).
///
/// The naive 2×2 [`decide_state`] cannot tell a *completed-but-unrecorded* apply (a crash between the
/// directory swap and the durable `applied` advance) from a *genuine* draft: both have `work != base`
/// and `applied != observed`, and the naive machine would show a FALSE "your draft diverged" panel to a
/// follower who never edited anything. This heal distinguishes them by the fetched target digest.
///
/// `work_eq_target` is checked FIRST so that the degenerate case `work == base == target` (a forward
/// move to byte-identical content) heals without a redundant swap. Because the digest is collision-
/// resistant, a genuine draft (different bytes) can never satisfy `work_eq_target`, so a real edit is
/// never silently healed away.
#[must_use]
pub fn refine_after_fetch(work_eq_base: bool, work_eq_target: bool) -> ApplyClass {
    if work_eq_target {
        ApplyClass::AlreadyAtTarget
    } else if work_eq_base {
        ApplyClass::CleanForward
    } else {
        ApplyClass::Diverged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_state_truth_table() {
        assert_eq!(decide_state(true, true), SyncStatus::Current);
        assert_eq!(decide_state(true, false), SyncStatus::Behind);
        assert_eq!(decide_state(false, true), SyncStatus::Draft);
        assert_eq!(decide_state(false, false), SyncStatus::Diverged);
    }

    #[test]
    fn refine_after_fetch_truth_table() {
        // work == target wins first — the degenerate base == target case heals without a swap.
        assert_eq!(refine_after_fetch(true, true), ApplyClass::AlreadyAtTarget);
        assert_eq!(refine_after_fetch(true, false), ApplyClass::CleanForward);
        // A crash-after-swap: work != base but == target → heal forward, NEVER a false Diverged.
        assert_eq!(refine_after_fetch(false, true), ApplyClass::AlreadyAtTarget);
        // A genuine draft: matches neither.
        assert_eq!(refine_after_fetch(false, false), ApplyClass::Diverged);
    }
}
