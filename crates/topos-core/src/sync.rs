//! The pure sync transition: the four currency states, the anti-rollback floor + reused-tuple ALARM
//! evaluation, and the post-fetch heal.
//!
//! Every decision here is a deterministic function over EXPLICIT VALUES — no I/O, no clock, no RNG (the
//! kernel constraints). The client engine maps live disk state into these inputs, calls the functions,
//! and acts on the verdicts. This module never re-decides *consent* (that is [`crate::consent`], the one
//! policy) — it only classifies the sync situation that selects which `consent::Situation` the engine
//! feeds to `decide()`, and it holds the integrity floor a follower must never cross.

use core::cmp::Ordering;

/// The anti-replay generation counter `(epoch, seq)`.
///
/// A kernel-local mirror of the wire/persisted counter (the boundary type derives no ordering). `Ord` is
/// **derived on field order — `epoch` first, then `seq`** — so an `epoch` bump always dominates any
/// `seq`: `{epoch: 2, seq: 0} > {epoch: 1, seq: u64::MAX}`. The engine converts the wire counter to this
/// at the edge so every `<` / `==` comparison goes through this one epoch-dominant order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation {
    /// Bumped on every restore that could lose `seq`; an epoch bump is always a forward move.
    pub epoch: u64,
    /// Strictly increases per pointer move within an epoch.
    pub seq: u64,
}

/// One `(generation → commit)` the client has authenticated — the kernel mirror of the persisted tuple
/// (the hex commit ids are decoded to bytes at the edge). Used to detect a reused tuple naming different
/// bytes (the restore/rollback/compromise ALARM).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordedTuple {
    pub generation: Generation,
    pub commit_id: [u8; 32],
}

/// The four currency states, derived from `(work == base?)` and `(applied == observed?)`.
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

/// How an `applied < observed` skill should apply, refined once the TARGET bytes are known.
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

/// Refine an `applied < observed` skill once the target's digest is known (after the fetch).
///
/// The naive 2×2 [`decide_state`] cannot tell a *completed-but-unrecorded* apply (a crash between the
/// directory swap and the durable `applied` advance) from a *genuine* draft: both have `work != base`
/// and `applied < observed`, and the naive machine would show a FALSE "your draft diverged" panel to a
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

/// The verdict on a *signature-verified* `current` record, evaluated against the durable floor + history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloorVerdict {
    /// A strictly higher `(epoch, seq)` than `observed` — a legitimate forward move (raise the floor +
    /// record the tuple, then apply toward it).
    Forward,
    /// The same tuple as `observed`, naming the same commit — an ordinary re-serve. No floor change; the
    /// engine still drives `applied` toward `observed`.
    Replay,
    /// A tuple *below* the floor matching a recorded commit — a stale re-serve. No-op; never a downgrade.
    StaleReplay,
    /// A tuple `≤ observed` naming a DIFFERENT commit than recorded — a restore/rollback/compromise.
    /// A loud alarm; `current` must not move.
    ReusedTupleAlarm,
    /// A tuple *below* the floor naming a generation never recorded — refuse (no apply), no alarm: a
    /// replay of a pointer from before this client's recorded history.
    RefuseBelowFloor,
    /// A tuple *at* the floor with no recorded entry — impossible for a consistent client (`observed`
    /// is recorded the moment it is raised), so this is local corruption: fail closed, never a benign
    /// replay.
    CorruptNoRecord,
}

impl FloorVerdict {
    /// Whether this verdict raises the `observed` floor (ONLY a verified strictly-higher record does).
    #[must_use]
    pub fn raises_floor(self) -> bool {
        matches!(self, FloorVerdict::Forward)
    }

    /// Whether this verdict is a loud integrity alarm (`current` must not move; the engine surfaces it).
    #[must_use]
    pub fn is_alarm(self) -> bool {
        matches!(self, FloorVerdict::ReusedTupleAlarm)
    }
}

/// Evaluate a SIGNATURE-VERIFIED `current` record against the durable floor `observed` + the recorded
/// `(generation → commit)` history.
///
/// Pure. The caller must have already (1) verified the plane signature with [`crate::sign::verify_pointer`]
/// and (2) confirmed the record's `(workspace_id, skill_id)` scope matches the followed skill — a record
/// that fails either is a pointer-verify failure, never reaches here. `recorded` must be unique by
/// generation (the engine validates that on read); the lookup is by exact generation.
///
/// `observed` is the anti-rollback FLOOR: a record at `(epoch,seq) ≤ observed` is never a forward move,
/// and only a strictly-higher verified record returns [`FloorVerdict::Forward`] (so an attacker who can
/// only replay older signed records can neither lower the floor nor poison it).
#[must_use]
pub fn evaluate_floor(
    served: Generation,
    served_commit: [u8; 32],
    observed: Generation,
    recorded: &[RecordedTuple],
) -> FloorVerdict {
    let found = recorded.iter().find(|t| t.generation == served);
    match served.cmp(&observed) {
        Ordering::Greater => FloorVerdict::Forward,
        Ordering::Equal => match found {
            Some(t) if t.commit_id == served_commit => FloorVerdict::Replay,
            Some(_) => FloorVerdict::ReusedTupleAlarm,
            None => FloorVerdict::CorruptNoRecord,
        },
        Ordering::Less => match found {
            Some(t) if t.commit_id == served_commit => FloorVerdict::StaleReplay,
            Some(_) => FloorVerdict::ReusedTupleAlarm,
            None => FloorVerdict::RefuseBelowFloor,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn c(n: u8) -> [u8; 32] {
        [n; 32]
    }
    fn g(epoch: u64, seq: u64) -> Generation {
        Generation { epoch, seq }
    }

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

    #[test]
    fn generation_ordering_is_epoch_dominant() {
        // An epoch bump beats any seq (the operational restore guarantee).
        assert!(g(2, 0) > g(1, u64::MAX));
        // Within an epoch, seq orders.
        assert!(g(1, 9) < g(1, 10));
        assert_eq!(g(1, 7), g(1, 7));
    }

    #[test]
    fn evaluate_floor_forward_raises_and_records() {
        let recorded = vec![RecordedTuple {
            generation: g(1, 7),
            commit_id: c(7),
        }];
        let v = evaluate_floor(g(1, 8), c(8), g(1, 7), &recorded);
        assert_eq!(v, FloorVerdict::Forward);
        assert!(v.raises_floor());
        assert!(!v.is_alarm());
        // An epoch bump is a forward move even with a lower seq.
        assert_eq!(
            evaluate_floor(g(2, 0), c(9), g(1, 7), &recorded),
            FloorVerdict::Forward
        );
    }

    #[test]
    fn evaluate_floor_same_tuple_same_commit_is_replay() {
        let recorded = vec![RecordedTuple {
            generation: g(1, 10),
            commit_id: c(10),
        }];
        let v = evaluate_floor(g(1, 10), c(10), g(1, 10), &recorded);
        assert_eq!(v, FloorVerdict::Replay);
        assert!(!v.raises_floor());
        assert!(!v.is_alarm());
    }

    #[test]
    fn evaluate_floor_same_tuple_different_commit_is_alarm() {
        let recorded = vec![RecordedTuple {
            generation: g(1, 10),
            commit_id: c(10),
        }];
        let v = evaluate_floor(g(1, 10), c(99), g(1, 10), &recorded);
        assert_eq!(v, FloorVerdict::ReusedTupleAlarm);
        assert!(v.is_alarm());
        assert!(!v.raises_floor());
    }

    #[test]
    fn evaluate_floor_at_floor_without_record_is_corrupt() {
        // `observed` is always recorded the moment it is raised; its absence is local corruption.
        let v = evaluate_floor(g(1, 10), c(10), g(1, 10), &[]);
        assert_eq!(v, FloorVerdict::CorruptNoRecord);
        assert!(!v.raises_floor());
    }

    /// Interleaving G — restore-replay. The plane is restored to an OLD seq WITHOUT an epoch bump and
    /// re-serves a signed tuple naming a different commit than the client recorded → loud ALARM. With an
    /// epoch bump it is a legitimate forward move.
    #[test]
    fn evaluate_floor_restore_replay_alarm_then_epoch_bump_forward() {
        let recorded = vec![
            RecordedTuple {
                generation: g(1, 8),
                commit_id: c(8),
            },
            RecordedTuple {
                generation: g(1, 10),
                commit_id: c(10),
            },
        ];
        // (1,8) re-served naming a DIFFERENT commit than recorded[(1,8)] → ALARM.
        assert_eq!(
            evaluate_floor(g(1, 8), c(88), g(1, 10), &recorded),
            FloorVerdict::ReusedTupleAlarm
        );
        // (1,8) re-served naming the SAME recorded commit → ordinary stale re-serve.
        assert_eq!(
            evaluate_floor(g(1, 8), c(8), g(1, 10), &recorded),
            FloorVerdict::StaleReplay
        );
        // (2,8) — an epoch bump above the floor → legitimate forward move.
        assert_eq!(
            evaluate_floor(g(2, 8), c(88), g(1, 10), &recorded),
            FloorVerdict::Forward
        );
    }

    /// The floor rules re-stated as a straight-line MODEL, deliberately naive and shaped differently
    /// from the kernel (lookup-first, explicit `(epoch, seq)` tuple comparison — which independently
    /// pins the epoch-dominant order) so the two implementations can only agree by both being right.
    fn floor_model(
        served: Generation,
        served_commit: [u8; 32],
        observed: Generation,
        recorded: &[RecordedTuple],
    ) -> FloorVerdict {
        let above = (served.epoch, served.seq) > (observed.epoch, observed.seq);
        let at = (served.epoch, served.seq) == (observed.epoch, observed.seq);
        if above {
            return FloorVerdict::Forward;
        }
        let known = recorded
            .iter()
            .find(|t| t.generation.epoch == served.epoch && t.generation.seq == served.seq);
        match known {
            Some(t) if t.commit_id != served_commit => FloorVerdict::ReusedTupleAlarm,
            Some(_) if at => FloorVerdict::Replay,
            Some(_) => FloorVerdict::StaleReplay,
            None if at => FloorVerdict::CorruptNoRecord,
            None => FloorVerdict::RefuseBelowFloor,
        }
    }

    #[test]
    fn evaluate_floor_agrees_with_the_brute_force_model_over_small_spaces() {
        // Generative + exhaustive: for each seeded random recorded history (unique by generation, over
        // a 3×3 generation grid and 3 commits), sweep EVERY (served, observed, served_commit) combo and
        // demand the kernel's verdict equal the model's — coverage over the whole small input space
        // instead of hand-picked rows.
        use crate::testgen::Rng;

        let gens: Vec<Generation> = (0..3u64)
            .flat_map(|e| (0..3u64).map(move |s| g(e, s)))
            .collect();
        let commits = [c(1), c(2), c(3)];
        let mut rng = Rng::new(0xF10_0DED_CAB1_E501);
        let mut cases = 0usize;
        for _ in 0..60 {
            let mut recorded = alloc::vec::Vec::new();
            for &generation in &gens {
                if rng.next() & 1 == 0 {
                    recorded.push(RecordedTuple {
                        generation,
                        commit_id: commits[(rng.next() % 3) as usize],
                    });
                }
            }
            for &observed in &gens {
                for &served in &gens {
                    for &served_commit in &commits {
                        assert_eq!(
                            evaluate_floor(served, served_commit, observed, &recorded),
                            floor_model(served, served_commit, observed, &recorded),
                            "served {served:?} commit {served_commit:?} observed {observed:?} \
                             recorded {recorded:?}"
                        );
                        cases += 1;
                    }
                }
            }
        }
        assert!(cases >= 10_000, "swept {cases} cases");
    }

    /// Interleaving F — downgrade after a failed apply. `observed = (1,10)`; a served signed `(1,9)`
    /// (never recorded — the client authenticated 10 directly) is below the floor → refuse, no
    /// downgrade. A recorded lower tuple re-served with its own commit is a stale no-op. Either way the
    /// floor holds and the engine keeps retrying toward `observed`.
    #[test]
    fn evaluate_floor_downgrade_below_floor_is_refused() {
        let recorded = vec![
            RecordedTuple {
                generation: g(1, 8),
                commit_id: c(8),
            },
            RecordedTuple {
                generation: g(1, 10),
                commit_id: c(10),
            },
        ];
        // (1,9) never recorded, below the floor → refuse, no apply, no alarm.
        let v = evaluate_floor(g(1, 9), c(9), g(1, 10), &recorded);
        assert_eq!(v, FloorVerdict::RefuseBelowFloor);
        assert!(!v.raises_floor());
        assert!(!v.is_alarm());
        // (1,8) recorded, same commit, below the floor → stale no-op (no downgrade).
        assert_eq!(
            evaluate_floor(g(1, 8), c(8), g(1, 10), &recorded),
            FloorVerdict::StaleReplay
        );
    }
}
