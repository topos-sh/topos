//! `state/trigger_registration.json` — WHICH agents this machine has already been offered an
//! auto-update trigger, and how the offer went.
//!
//! Placement reaches every agent detected here; the trigger is what keeps those copies current
//! without anybody asking for it. An agent installed AFTER topos was wired in had neither until
//! the next `login`/`add` — so the sweep registers it (`crate::ops::reconcile`), and THIS document
//! is what makes that a one-time offer rather than a standing policy: a row means the question has
//! been asked about that agent, and a person who then deleted the hook is left alone forever after.
//!
//! - **One row per harness slug**, written by every trigger installation there is — `login`'s and
//!   `add`'s unconditional breadth arming, and the sweep's own registration. A verb a person ran
//!   deliberately does not consult the record; it refreshes it.
//! - **A FAILED attempt is recorded too**, and retried on the forge lane's much slower rhythm
//!   ([`RETRY_INTERVAL_MS`] plus a spread). Some registrations dial the harness's own program, and
//!   one attempt per sweep would be a subprocess per session start — the traffic that rhythm
//!   exists to prevent. A registration that STANDS is never retried at all: that is the whole
//!   mechanism by which a hook somebody removed stays removed.
//! - **A document a NEWER build wrote is neither read nor written.** It is that build's record of
//!   what it has offered, and an older binary re-deciding from a shape it cannot see would offer
//!   again — resurrecting the hooks that build had been told to leave alone.
//! - **A document this build cannot parse at all is treated as ABSENT** (offer again, write
//!   afresh). Garbage is not evidence that anyone removed anything, and the other direction — a
//!   machine that can never register a new agent again — is the worse failure. The same fail-open
//!   rule the forge clock and the harness-table stamp keep.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use topos_types::{PERSISTED_SCHEMA_VERSION, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// How long a FAILED registration waits before the sweep offers again — the forge lane's interval,
/// deliberately shared: both are best-effort background work whose cost is paid outside this
/// process (a harness CLI spawn here, a request there), and neither is worth one per session.
pub(crate) const RETRY_INTERVAL_MS: i64 = crate::forge_check::CHECK_INTERVAL_MS;

/// The widest spread added to [`RETRY_INTERVAL_MS`] when scheduling a retry. Without it a fleet
/// installed together retries in lockstep, which is the same reason the forge clock jitters.
pub(crate) const RETRY_JITTER_MS: i64 = crate::forge_check::CHECK_JITTER_MS;

/// The whole document: what this machine has offered, per harness slug.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TriggerRegistrations {
    #[serde(default)]
    pub schema_version: u32,
    /// Per harness SLUG, its last registration attempt.
    #[serde(default)]
    pub agents: BTreeMap<String, Registration>,
}

/// One harness's last registration attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Registration {
    /// Epoch millis of the attempt, whatever came of it.
    #[serde(default)]
    pub at_ms: i64,
    /// Whether a trigger STOOD in that agent when the attempt finished.
    #[serde(default)]
    pub registered: bool,
    /// Epoch millis: the earliest a failed attempt may be made again. `0` on a registration that
    /// stands — it is never retried.
    #[serde(default)]
    pub retry_at_ms: i64,
}

impl TriggerRegistrations {
    /// Whether the SWEEP may offer `slug` its trigger right now: never offered, or offered and
    /// failed with the retry moment reached. A registration that stands is never re-offered.
    pub(crate) fn may_offer(&self, slug: &str, now_ms: i64) -> bool {
        match self.agents.get(slug) {
            None => true,
            Some(r) => !r.registered && retry_due(r.retry_at_ms, now_ms),
        }
    }
}

/// Whether a scheduled retry moment has come. A moment absurdly far ahead is treated as due rather
/// than obeyed — the same fail-open rule the forge clock keeps, so a backwards clock step (or a
/// corrupted value) cannot suspend registration until wall time catches up. The ceiling admits
/// everything [`next_retry`] can legitimately produce.
fn retry_due(at_ms: i64, now_ms: i64) -> bool {
    at_ms == 0
        || now_ms >= at_ms
        || at_ms > now_ms.saturating_add(RETRY_INTERVAL_MS + RETRY_JITTER_MS)
}

/// When a failed attempt may be made again: one interval from now, pushed out by an independent
/// spread.
fn next_retry(now_ms: i64, jitter_ms: i64) -> i64 {
    now_ms.saturating_add(RETRY_INTERVAL_MS.saturating_add(jitter_ms.clamp(0, RETRY_JITTER_MS)))
}

/// Whether a report says a trigger STANDS in that agent.
///
/// [`TriggerState::Degraded`] is the one state that wrote nothing — the fail-closed answer of an
/// adapter that could not prove the shape it was editing — so it is the one worth attempting
/// again. Every other state is an ANSWER: live, registered with the harness's own consent still
/// owed, or a trigger of the person's own left untouched. None of those is a question to re-ask.
pub(crate) fn stands(report: &TriggerReport) -> bool {
    report.state != TriggerState::Degraded
}

/// The document, or `None` when a NEWER build owns it (act on nothing, write nothing).
///
/// Absent or unparseable both answer with the default — see the module note on why garbage fails
/// toward offering rather than toward silence.
pub(crate) fn read(fs: &dyn FsOps, layout: &Layout) -> Option<TriggerRegistrations> {
    let Ok(Some(bytes)) = fs.read_opt(&layout.trigger_registration_path()) else {
        return Some(TriggerRegistrations::default());
    };
    let Ok(doc) = serde_json::from_slice::<TriggerRegistrations>(&bytes) else {
        return Some(TriggerRegistrations::default());
    };
    if doc.schema_version > PERSISTED_SCHEMA_VERSION {
        return None;
    }
    Some(doc)
}

/// Record what a round of trigger installations came to, over the ctx's own clock and spread — the
/// ONE call site shape both writers use (`login`/`add`'s breadth arming, and the sweep's
/// registration).
pub(crate) fn record_reports<'r>(
    ctx: &Ctx<'_>,
    reports: impl IntoIterator<Item = &'r TriggerReport>,
) {
    let outcomes: Vec<(&str, bool)> = reports
        .into_iter()
        .map(|r| (r.agent.as_str(), stands(r)))
        .collect();
    let now_ms = i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX);
    let jitter_ms = i64::try_from(
        ctx.ids
            .jitter_below(u64::try_from(RETRY_JITTER_MS).unwrap_or(0)),
    )
    .unwrap_or(0);
    record(ctx.fs, &ctx.layout, now_ms, jitter_ms, &outcomes);
}

/// Record a round's outcomes, merged over what is already on file — a harness this round never
/// touched keeps its row.
///
/// Best-effort: a record that cannot be written costs one repeated offer later, which is the
/// direction a failure here should fall. The whole read-modify-write runs under [`lock`], because
/// every write merges over the last one and two rounds racing would each write a document missing
/// the other's rows.
pub(crate) fn record(
    fs: &dyn FsOps,
    layout: &Layout,
    now_ms: i64,
    jitter_ms: i64,
    outcomes: &[(&str, bool)],
) {
    if outcomes.is_empty() {
        return;
    }
    let Ok(_guard) = lock(fs, layout) else {
        return;
    };
    let Some(mut doc) = read(fs, layout) else {
        return; // a newer build's document is that build's to write
    };
    for (slug, registered) in outcomes {
        doc.agents.insert(
            (*slug).to_owned(),
            Registration {
                at_ms: now_ms,
                registered: *registered,
                retry_at_ms: if *registered {
                    0
                } else {
                    next_retry(now_ms, jitter_ms)
                },
            },
        );
    }
    doc.schema_version = PERSISTED_SCHEMA_VERSION;
    if fs.create_dir_all(&layout.state_dir()).is_ok() {
        let _ = crate::doc::write_doc(fs, &layout.trigger_registration_path(), &doc);
    }
}

/// The exclusive lock every mutation of this document holds.
///
/// # Errors
/// The filesystem failure creating or taking the lock.
fn lock(
    fs: &dyn FsOps,
    layout: &Layout,
) -> Result<crate::fs_seam::LockGuard, crate::error::ClientError> {
    fs.create_dir_all(&layout.locks_dir())?;
    Ok(fs.lock_exclusive(&layout.trigger_registration_lock_file())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use topos_types::CurrencyKind;

    /// A self-cleaning temp `~/.topos` home (RAII).
    struct TempHome(PathBuf);
    impl TempHome {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("topos-treg-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn layout(&self) -> Layout {
            Layout::new(&self.0)
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const NOW: i64 = 1_700_000_000_000;

    fn report(agent: &str, state: TriggerState) -> TriggerReport {
        TriggerReport {
            agent: agent.to_owned(),
            currency_kind: CurrencyKind::ExplicitPullOnly,
            touched_path: None,
            marker_id: "test".to_owned(),
            state,
            note: None,
        }
    }

    #[test]
    fn an_agent_is_offered_once_and_never_again() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        assert!(
            read(&fs, &layout).unwrap().may_offer("cursor", NOW),
            "an agent nothing was ever recorded about is offered"
        );

        record(&fs, &layout, NOW, 0, &[("cursor", true)]);
        let doc = read(&fs, &layout).unwrap();
        assert!(
            !doc.may_offer("cursor", NOW),
            "a registration that stands is never re-offered"
        );
        // THE POINT: not a year later either. The record is what makes a hook somebody deleted
        // stay deleted.
        assert!(!doc.may_offer("cursor", NOW + 365 * 24 * 60 * 60 * 1000));
        assert!(
            doc.may_offer("cline", NOW),
            "and nobody else's turn is spent by it"
        );
    }

    #[test]
    fn a_failed_attempt_waits_the_slow_clock_out() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        record(&fs, &layout, NOW, 0, &[("openclaw", false)]);
        let doc = read(&fs, &layout).unwrap();
        assert!(!doc.may_offer("openclaw", NOW), "not on the next sweep");
        assert!(
            !doc.may_offer("openclaw", NOW + RETRY_INTERVAL_MS - 1),
            "one ms short"
        );
        assert!(
            doc.may_offer("openclaw", NOW + RETRY_INTERVAL_MS),
            "the interval elapses"
        );
        // The spread only ever DELAYS, and stays inside the one hour the forge clock allows.
        record(&fs, &layout, NOW, RETRY_JITTER_MS, &[("openclaw", false)]);
        let doc = read(&fs, &layout).unwrap();
        assert!(!doc.may_offer("openclaw", NOW + RETRY_INTERVAL_MS));
        assert!(doc.may_offer("openclaw", NOW + RETRY_INTERVAL_MS + RETRY_JITTER_MS));
    }

    #[test]
    fn a_round_merges_over_the_rows_it_did_not_touch() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        record(&fs, &layout, NOW, 0, &[("cursor", true), ("cline", false)]);
        let later = NOW + RETRY_INTERVAL_MS;
        record(&fs, &layout, later, 0, &[("cline", true)]);
        let doc = read(&fs, &layout).unwrap();
        assert_eq!(
            doc.agents["cursor"].at_ms, NOW,
            "untouched row keeps its history"
        );
        assert!(doc.agents["cursor"].registered);
        assert_eq!(doc.agents["cline"].at_ms, later);
        assert!(doc.agents["cline"].registered);
        assert_eq!(
            doc.agents["cline"].retry_at_ms, 0,
            "a standing registration schedules no retry"
        );
    }

    #[test]
    fn garbage_offers_again_and_a_newer_builds_document_is_left_alone() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        std::fs::create_dir_all(layout.state_dir()).unwrap();

        std::fs::write(layout.trigger_registration_path(), b"{ not json ").unwrap();
        assert!(
            read(&fs, &layout).unwrap().may_offer("cursor", NOW),
            "garbage is not evidence that anybody removed anything"
        );

        let foreign = format!(
            "{{\"schema_version\": {}, \"agents\": {{}}}}",
            PERSISTED_SCHEMA_VERSION + 1
        );
        std::fs::write(layout.trigger_registration_path(), &foreign).unwrap();
        assert!(read(&fs, &layout).is_none(), "a newer build owns it");
        record(&fs, &layout, NOW, 0, &[("cursor", true)]);
        assert_eq!(
            std::fs::read_to_string(layout.trigger_registration_path()).unwrap(),
            foreign,
            "and it is not written over"
        );
    }

    /// The only state that wrote nothing is the one worth attempting again. A registration whose
    /// harness still owes its own consent step HAS an artifact in place, so re-offering it would
    /// nag about a step no sweep can complete.
    #[test]
    fn only_a_degraded_report_counts_as_a_failed_attempt() {
        for state in [
            TriggerState::Active,
            TriggerState::Inactive,
            TriggerState::AlreadyPresentUnmanaged,
        ] {
            assert!(stands(&report("cursor", state)), "{state:?}");
        }
        assert!(!stands(&report("openclaw", TriggerState::Degraded)));
    }
}
