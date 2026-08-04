//! `state/forge_check.json` — the MACHINE's forge auto-update clock and per-source check log.
//!
//! A `topos.toml` row pointing at a repository is kept current the same way a workspace-delivered
//! bundle is: the background sweep checks it, and a change lands silently. It just cannot ride the
//! workspace lane's cadence. Delivery asks ONE server about every bundle at once, so a five-minute
//! rhythm costs one request; a forge has no such endpoint, so the same rhythm would cost one
//! request per row per five minutes against a public allowance an entire office shares. This
//! module is the separate, much slower clock that makes the lane affordable:
//!
//! - **[`CHECK_INTERVAL_MS`] plus a spread of up to [`CHECK_JITTER_MS`]**, hardcoded. There is no
//!   flag, no environment variable and no manifest key for it — the workspace knobs govern the
//!   workspace cadence and stop there. The spread matters as much as the interval: without it a
//!   fleet installed together drifts into lockstep and arrives at one host in waves.
//! - **The clock advances on failure exactly as on success.** A round that could not reach the
//!   forge has still had its turn; leaving the due time untouched would turn every session start
//!   into a fresh attempt, which is precisely the traffic the interval exists to prevent.
//! - **A source the forge says is GONE is asked about once**, then left alone until the row that
//!   names it changes. The record keys the refusal to the ref it was about, so editing the row is
//!   what re-opens the question.
//!
//! Every check's time and outcome is recorded here whatever it was, so `status` and `list` can
//! answer "is this even working?" from local state alone. Reads FAIL OPEN — an unreadable document
//! means check now, which costs one small request rather than a wrong answer about what is
//! installed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use topos_types::PERSISTED_SCHEMA_VERSION;

use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// How long the silent sweep waits between forge checks.
pub(crate) const CHECK_INTERVAL_MS: i64 = 6 * 60 * 60 * 1000;

/// The widest spread added to [`CHECK_INTERVAL_MS`] when scheduling the next check.
pub(crate) const CHECK_JITTER_MS: i64 = 60 * 60 * 1000;

/// The furthest out a host's own backoff signal may push the next check. A header asking to be
/// left alone for a week is a header topos declines to take literally — but anything up to a day
/// is honored, so the sanity ceiling on a stored due time has to allow for it.
pub(crate) const MAX_BACKOFF_MS: i64 = 24 * 60 * 60 * 1000;

/// How long a source may go unchecked before the silent sweep is willing to SAY so. Far longer
/// than the interval on purpose: at six-hourly checks this is dozens of consecutive failures, so
/// the line means "this has really stopped working", not "a train went into a tunnel". It matches
/// the window the workspace lane's own staleness warning uses.
pub(crate) const STALE_AFTER_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// The whole document: a host-requested floor, and the last outcome per question.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ForgeCheck {
    #[serde(default)]
    pub schema_version: u32,
    /// Epoch millis: the earliest ANY forge may be dialed, set only when a host asks to be left
    /// alone for a while. A backoff is a fact about the HOST, so it holds across every question
    /// aimed at it — unlike the cadence, which each question keeps for itself.
    #[serde(default)]
    pub backoff_until_ms: i64,
    /// Per QUESTION — `<host>/<owner>/<repo>#<ref>`, the ref empty for a floating row — how its
    /// last check went. Keyed by the question rather than the source because one repository can be
    /// named at two refs, and those are two different facts: a floating row that works says nothing
    /// about a pinned row whose commit is gone, and a per-source key would let the first quietly
    /// erase the second's final verdict.
    #[serde(default)]
    pub sources: BTreeMap<String, SourceCheck>,
}

/// The key one row's check is filed under: which source, at which ref.
pub(crate) fn question(source: &str, git_ref: &str) -> String {
    format!("{source}#{git_ref}")
}

/// The source half of a [`question`] key. Origins carry no `#`, so the split is unambiguous.
pub(crate) fn source_of(key: &str) -> &str {
    key.split('#').next().unwrap_or(key)
}

/// One question's last check.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceCheck {
    /// Epoch millis of the last check ATTEMPT, whatever came of it.
    #[serde(default)]
    pub checked_at_ms: i64,
    /// Epoch millis of the last check that got a real answer. `None` = never yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at_ms: Option<i64>,
    /// The commit the last answering check saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The last check's failure; absent when it answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<CheckFailure>,
    /// Per SCOPE label, when that scope next asks this question.
    ///
    /// Per question AND per scope, because both halves are load-bearing. A sweep only ever visits
    /// the checkout it starts in plus the machine manifest, so one clock for the whole lane lets
    /// the first checkout active after each due moment spend the turn while a second project is
    /// never checked at all. And a scope that has never asked has never had a turn: a freshly
    /// cloned project must fetch its rows now, not sit empty until an interval another checkout
    /// started runs out. A scope that HAS asked waits, whatever the answer was — a failed check
    /// has still had its turn, which is what stops one dead network becoming a request per session.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub next_check_at: BTreeMap<String, i64>,
    /// Per SCOPE label, the convergence this question last reached there — the archive was
    /// fetched, every member it holds is tracked at that head, and nothing was left to converge.
    ///
    /// It exists because "what is installed" cannot always answer "is there anything to do". A
    /// repository that drops its last skill leaves retained custody records still naming the OLD
    /// commit, and a repository that never held one leaves nothing at all; in both cases the
    /// tracked-imports comparison says "behind" forever, and the archive is downloaded again on
    /// every cadence to rediscover that there is nothing in it. Per scope, because scopes converge
    /// independently and one having finished says nothing about the other.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settled_at: BTreeMap<String, Settled>,
}

/// What one scope's completed convergence consisted of: the head it reached, and the members that
/// head holds.
///
/// The MEMBERS are what make the mark re-provable. It lives in machine state, which outlives any
/// one checkout — delete a project's store, or reclone at the same path, and the mark is still
/// there while the bytes it vouches for are not. So it is never trusted on its own: honoring it
/// means checking that this scope's store still tracks every member it names, at that head. A head
/// that holds NO members needs nothing proved, which is exactly the case the mark exists for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Settled {
    /// The commit the scope converged to.
    pub head: String,
    /// The members that commit holds — every one of which must still be tracked here for the mark
    /// to mean anything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
}

/// A failed check, kept in enough detail to decide whether to ask again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CheckFailure {
    /// The public reason — safe to show verbatim (a host and what went wrong, never a secret).
    pub reason: String,
    /// The forge answered ABOUT this source and the answer was final: gone, private, or never
    /// real. Retrying the same reference gets the same answer, so it is not retried.
    #[serde(default)]
    pub gone: bool,
    /// The forge ANSWERED at all. A rate-limited or erroring host is a different thing from one
    /// that could not be dialed, and the line a person reads must not call the first the second.
    #[serde(default)]
    pub reached: bool,
    /// The row's ref at the time (a pin, or empty for a floating row). An edit that changes what
    /// the row asks for changes this key, and the question re-opens.
    #[serde(default)]
    pub git_ref: String,
    /// Whether the one-time refusal line has already been said.
    #[serde(default)]
    pub reported: bool,
}

impl SourceCheck {
    /// Whether asking this question again can still change the answer. A settled `gone` verdict is
    /// the one thing that stops the lane from dialing — and it is filed under the exact question it
    /// was about, so a row edited to ask something different is asking something different.
    pub(crate) fn worth_dialing(&self) -> bool {
        match &self.failure {
            Some(f) => !f.gone,
            None => true,
        }
    }
}

/// Forget every verdict standing against a SOURCE, at any ref.
///
/// A verdict is a record of what a forge said, and a forge that has just handed over the bytes has
/// said something newer. Without this a repository that was deleted and later restored would stay
/// permanently un-updated: `topos add … --yes` would fetch it, install it, and write the row, while
/// the automatic update went on refusing to dial the source the person just proved is alive.
///
/// A machine that cannot record the forgetting DISCARDS the whole document instead. Keeping it
/// would keep the verdict, and the verdict is the thing that stops the lane asking — so failing
/// quietly the other way would leave a source the person just proved alive permanently
/// un-updated. Losing the document costs one round of re-checking everything, which is the
/// direction a failure here should fall.
pub(crate) fn forget(fs: &dyn FsOps, layout: &Layout, source: &str) {
    let drop_it = || {
        let _ = fs.remove_file(&layout.forge_check_path());
    };
    let Ok(_guard) = lock(fs, layout) else {
        drop_it();
        return;
    };
    let mut doc = read(fs, layout);
    let before = doc.sources.len();
    doc.sources
        .retain(|key, check| source_of(key) != source || check.failure.is_none());
    if doc.sources.len() == before {
        return;
    }
    doc.schema_version = PERSISTED_SCHEMA_VERSION;
    if fs.create_dir_all(&layout.state_dir()).is_err()
        || crate::doc::write_doc(fs, &layout.forge_check_path(), &doc).is_err()
    {
        drop_it();
    }
}

/// Read the document. FAILS OPEN: absent, unreadable, or written by a build this one does not
/// understand all answer with the default — a due clock and no recorded refusals. Checking once
/// more than necessary costs a few hundred bytes; refusing to check because a state file went bad
/// would silently stop the auto-update this module exists to run.
pub(crate) fn read(fs: &dyn FsOps, layout: &Layout) -> ForgeCheck {
    let Ok(Some(bytes)) = fs.read_opt(&layout.forge_check_path()) else {
        return ForgeCheck::default();
    };
    let Ok(doc) = serde_json::from_slice::<ForgeCheck>(&bytes) else {
        return ForgeCheck::default();
    };
    if doc.schema_version > PERSISTED_SCHEMA_VERSION {
        return ForgeCheck::default();
    }
    doc
}

/// Whether a SCHEDULED round may dial this QUESTION, from this SCOPE, now.
///
/// A turn this scope has never taken is due immediately — a row somebody just committed should not wait
/// out an interval belonging to somebody else's row. A due time absurdly far ahead is treated as
/// due rather than obeyed: a backwards clock step (or a corrupted value) must not suspend
/// auto-updates until wall time catches up. The ceiling admits everything [`next_due`] can
/// legitimately produce, so a due time this code really wrote is never mistaken for corruption.
pub(crate) fn due(doc: &ForgeCheck, key: &str, scope: &str, now_ms: i64) -> bool {
    // A host that asked for time gets it, whatever any single question's own clock says.
    if now_ms < doc.backoff_until_ms && doc.backoff_until_ms <= host_floor_ceiling(now_ms) {
        return false;
    }
    let Some(at) = doc
        .sources
        .get(key)
        .and_then(|c| c.next_check_at.get(scope))
    else {
        return true;
    };
    let ceiling = host_floor_ceiling(now_ms);
    now_ms >= *at || *at > ceiling
}

/// The furthest ahead a due time this code wrote can legitimately sit.
fn host_floor_ceiling(now_ms: i64) -> i64 {
    now_ms.saturating_add(CHECK_INTERVAL_MS + CHECK_JITTER_MS + MAX_BACKOFF_MS)
}

/// When a question next falls due: one interval from now, pushed out by an independent spread.
///
/// A host's own backoff is NOT folded in here — it is a fact about the host, so it is kept as the
/// document's machine-wide floor and applied to every question aimed at that host. Folding it into
/// one question's clock would let a rate limit expire for one row and still be in force for
/// another, which is not what the host said.
pub(crate) fn next_due(now_ms: i64, jitter_ms: i64) -> i64 {
    now_ms.saturating_add(CHECK_INTERVAL_MS.saturating_add(jitter_ms.max(0)))
}

/// Whether a source has gone unchecked long enough to be worth interrupting a session over. A
/// source that has NEVER answered is not stale — there is nothing to be stale from, and warning on
/// a first run teaches people to ignore the line.
pub(crate) fn is_stale(answered_at_ms: Option<i64>, now_ms: i64) -> bool {
    let Some(last) = answered_at_ms else {
        return false;
    };
    now_ms.saturating_sub(last) > STALE_AFTER_MS
}

/// Persist the round: the outcomes it produced (each already carrying its own next due time),
/// merged over what was already recorded — a question no row named this run keeps its history.
///
/// `host_backoff_ms` raises the machine-wide floor when a host asked for one; it only ever moves
/// FORWARD, so a quiet round can never talk a standing backoff down.
///
/// The whole read-modify-write runs under [`lock`]. This document is machine-wide and every write
/// merges over the last one, so two updates racing — two targeted runs, or one beside a sweep —
/// would otherwise each write a document missing the other's outcomes, and the loser's scheduling,
/// verdicts and settlement marks would vanish.
///
/// # Errors
/// The filesystem failure taking the lock or writing the document. The caller treats it as a
/// disclosure, never as a reason to fail a sweep that otherwise worked.
pub(crate) fn record_round(
    fs: &dyn FsOps,
    layout: &Layout,
    host_backoff_ms: Option<i64>,
    outcomes: &[(String, SourceCheck)],
) -> Result<(), ClientError> {
    let _guard = lock(fs, layout)?;
    let mut doc = read(fs, layout);
    doc.schema_version = PERSISTED_SCHEMA_VERSION;
    if let Some(at) = host_backoff_ms {
        doc.backoff_until_ms = doc.backoff_until_ms.max(at);
    }
    for (key, check) in outcomes {
        doc.sources.insert(key.clone(), check.clone());
    }
    fs.create_dir_all(&layout.state_dir())?;
    crate::doc::write_doc(fs, &layout.forge_check_path(), &doc)
}

/// The exclusive lock every mutation of this document holds. Machine-wide state merged on each
/// write needs one writer at a time, or the merges race and one of them is simply lost.
///
/// # Errors
/// The filesystem failure creating or taking the lock.
fn lock(fs: &dyn FsOps, layout: &Layout) -> Result<crate::fs_seam::LockGuard, ClientError> {
    fs.create_dir_all(&layout.locks_dir())?;
    Ok(fs.lock_exclusive(&layout.forge_check_lock_file())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A self-cleaning temp `~/.topos` home (RAII).
    struct TempHome(PathBuf);
    impl TempHome {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("topos-fchk-{}-{n}", std::process::id()));
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

    const Q: &str = "github.com/o/r#";
    const Q2: &str = "github.com/o/other#";

    const SCOPE: &str = "person";

    fn answered(at: i64, due_at: i64) -> SourceCheck {
        SourceCheck {
            checked_at_ms: at,
            answered_at_ms: Some(at),
            commit: Some("aaa".to_owned()),
            failure: None,
            next_check_at: [(SCOPE.to_owned(), due_at)].into_iter().collect(),
            settled_at: BTreeMap::new(),
        }
    }

    #[test]
    fn a_question_never_asked_is_due_and_a_recorded_one_waits_its_own_interval() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        let now = 1_700_000_000_000;

        // Nothing recorded → ask now. A row somebody just committed does not wait out an interval
        // that belongs to somebody else's row.
        assert!(
            due(&read(&fs, &layout), Q, SCOPE, now),
            "a fresh question asks"
        );

        record_round(
            &fs,
            &layout,
            None,
            &[(Q.to_owned(), answered(now, next_due(now, 0)))],
        )
        .unwrap();
        let doc = read(&fs, &layout);
        assert!(!due(&doc, Q, SCOPE, now), "inside its interval it does not");
        assert!(
            !due(&doc, Q, SCOPE, now + CHECK_INTERVAL_MS - 1),
            "one ms short"
        );
        assert!(
            due(&doc, Q, SCOPE, now + CHECK_INTERVAL_MS),
            "the interval elapses"
        );

        // THE POINT: another question is untouched by it. A sweep only ever visits the checkout it
        // starts in, so one clock for the whole lane would let this question's turn be spent by a
        // row in a project the sweep never looked at.
        assert!(
            due(&doc, Q2, SCOPE, now),
            "a different question keeps its own schedule"
        );
    }

    #[test]
    fn the_spread_only_ever_delays_and_stays_inside_one_hour() {
        let now = 1_700_000_000_000;
        assert_eq!(next_due(now, 0), now + CHECK_INTERVAL_MS);
        assert_eq!(
            next_due(now, CHECK_JITTER_MS),
            now + CHECK_INTERVAL_MS + CHECK_JITTER_MS
        );
        // A negative draw can never pull the next check earlier than the plain interval.
        assert_eq!(next_due(now, -5_000), now + CHECK_INTERVAL_MS);
    }

    #[test]
    fn a_hosts_backoff_holds_every_question_aimed_at_it() {
        // A backoff is a fact about the HOST, so it is the document's floor rather than one
        // question's clock: a rate limit that expired for one row but not another is not what the
        // host said.
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        let now = 1_700_000_000_000;
        let far = now + 3 * CHECK_INTERVAL_MS;
        record_round(
            &fs,
            &layout,
            Some(far),
            &[(Q.to_owned(), answered(now, next_due(now, 0)))],
        )
        .unwrap();
        let doc = read(&fs, &layout);
        assert!(
            !due(&doc, Q, SCOPE, now + CHECK_INTERVAL_MS),
            "the floor holds it"
        );
        assert!(
            !due(&doc, Q2, SCOPE, now),
            "and holds a question never asked too"
        );
        assert!(due(&doc, Q, SCOPE, far), "until the host said to come back");

        // It only ever moves FORWARD: a later quiet round cannot talk a standing backoff down.
        record_round(&fs, &layout, Some(now + 1_000), &[]).unwrap();
        assert_eq!(read(&fs, &layout).backoff_until_ms, far);
    }

    #[test]
    fn a_far_future_due_time_never_suspends_a_question() {
        // A backwards clock step (or a corrupt value) must not stop auto-updates until wall time
        // catches up — the same fail-open rule the sweep's own throttle keeps.
        let now = 1_700_000_000_000;
        let mut doc = ForgeCheck::default();
        doc.sources
            .insert(Q.to_owned(), answered(now, now + 400 * 24 * 60 * 60 * 1000));
        assert!(due(&doc, Q, SCOPE, now));

        // But a due time this code really wrote is OBEYED, however far out.
        let mut honored = ForgeCheck::default();
        honored
            .sources
            .insert(Q.to_owned(), answered(now, next_due(now, CHECK_JITTER_MS)));
        honored.backoff_until_ms = now + MAX_BACKOFF_MS;
        assert!(
            !due(&honored, Q, SCOPE, now),
            "a host's longest backoff holds"
        );
    }

    #[test]
    fn an_unreadable_or_foreign_document_fails_open() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        std::fs::create_dir_all(layout.state_dir()).unwrap();

        std::fs::write(layout.forge_check_path(), b"{ not json ").unwrap();
        assert!(due(&read(&fs, &layout), Q, SCOPE, 0), "garbage means check");

        let foreign = format!(
            "{{\"schema_version\": {}, \"next_check_at_ms\": 9999999999999}}",
            PERSISTED_SCHEMA_VERSION + 1
        );
        std::fs::write(layout.forge_check_path(), foreign).unwrap();
        let doc = read(&fs, &layout);
        assert_eq!(doc, ForgeCheck::default(), "a newer build's doc is foreign");
        assert!(due(&doc, Q, SCOPE, 0));
    }

    fn gone(git_ref: &str) -> SourceCheck {
        SourceCheck {
            failure: Some(CheckFailure {
                reason: "gone".into(),
                gone: true,
                reached: true,
                git_ref: git_ref.to_owned(),
                reported: true,
            }),
            ..SourceCheck::default()
        }
    }

    #[test]
    fn a_gone_verdict_stops_the_dialing_for_the_question_it_was_about() {
        assert!(
            !gone("").worth_dialing(),
            "the same question is not re-asked"
        );
        let transient = SourceCheck {
            failure: Some(CheckFailure {
                reason: "could not connect".into(),
                gone: false,
                reached: false,
                git_ref: String::new(),
                reported: false,
            }),
            ..SourceCheck::default()
        };
        assert!(
            transient.worth_dialing(),
            "a network fault is always retried"
        );
        assert!(SourceCheck::default().worth_dialing());

        // A row edited to ask something else is asking something else — a different key entirely,
        // which nothing has been recorded against.
        assert_eq!(question("github.com/o/r", ""), "github.com/o/r#");
        assert_eq!(
            question("github.com/o/r", "abc1234"),
            "github.com/o/r#abc1234"
        );
        assert_eq!(source_of("github.com/o/r#abc1234"), "github.com/o/r");
        assert_eq!(source_of("github.com/o/r"), "github.com/o/r");
    }

    #[test]
    fn a_source_that_answers_again_is_forgiven_every_verdict_against_it() {
        // A repository deleted and later restored: `add … --yes` fetches it, which is newer
        // evidence than any refusal on file. Without the forgetting, automatic updates would stay
        // switched off for that source forever.
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        record_round(
            &fs,
            &layout,
            None,
            &[
                (question("github.com/o/r", ""), gone("")),
                (question("github.com/o/r", "abc1234"), gone("abc1234")),
                (question("github.com/o/other", ""), gone("")),
            ],
        )
        .unwrap();

        forget(&fs, &layout, "github.com/o/r");
        let doc = read(&fs, &layout);
        assert!(
            !doc.sources.contains_key(&question("github.com/o/r", "")),
            "{:?}",
            doc.sources
        );
        assert!(
            !doc.sources
                .contains_key(&question("github.com/o/r", "abc1234")),
            "every ref of that source is forgiven: {:?}",
            doc.sources
        );
        assert!(
            doc.sources
                .contains_key(&question("github.com/o/other", "")),
            "and nobody else's verdict is touched: {:?}",
            doc.sources
        );
        assert_eq!(
            doc.backoff_until_ms, 0,
            "forgetting is not a round; no schedule is touched"
        );
    }

    #[test]
    fn staleness_needs_an_answer_to_be_stale_from() {
        let now = 1_700_000_000_000;
        assert!(!is_stale(None, now), "never answered is not stale");
        assert!(!is_stale(Some(now - STALE_AFTER_MS), now), "exactly at");
        assert!(is_stale(Some(now - STALE_AFTER_MS - 1), now), "past it");
        assert!(
            !is_stale(Some(now - CHECK_INTERVAL_MS), now),
            "one interval"
        );
    }

    #[test]
    fn a_round_merges_outcomes_over_the_recorded_history() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        let now = 1_700_000_000_000;
        let ok = |at: i64, commit: &str| SourceCheck {
            checked_at_ms: at,
            answered_at_ms: Some(at),
            commit: Some(commit.to_owned()),
            failure: None,
            next_check_at: [(SCOPE.to_owned(), next_due(at, 0))].into_iter().collect(),
            settled_at: BTreeMap::new(),
        };
        record_round(
            &fs,
            &layout,
            None,
            &[
                ("github.com/o/a".to_owned(), ok(now, "aaa")),
                ("github.com/o/b".to_owned(), ok(now, "bbb")),
            ],
        )
        .unwrap();
        // A later round names only ONE source: the other keeps its history rather than vanishing.
        let later = now + CHECK_INTERVAL_MS;
        record_round(
            &fs,
            &layout,
            None,
            &[("github.com/o/a".to_owned(), ok(later, "ccc"))],
        )
        .unwrap();
        let doc = read(&fs, &layout);
        assert_eq!(
            doc.sources["github.com/o/a"].next_check_at[SCOPE],
            later + CHECK_INTERVAL_MS
        );
        assert_eq!(doc.sources["github.com/o/a"].commit.as_deref(), Some("ccc"));
        assert_eq!(doc.sources["github.com/o/b"].commit.as_deref(), Some("bbb"));
        assert_eq!(doc.sources["github.com/o/b"].checked_at_ms, now);
    }
}
