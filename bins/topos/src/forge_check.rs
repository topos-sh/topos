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

/// The whole document: when the next scheduled check is due, and the last outcome per source.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ForgeCheck {
    #[serde(default)]
    pub schema_version: u32,
    /// Epoch millis: the earliest a SCHEDULED check may dial a forge again. Written after every
    /// round that dialed, whatever its outcome.
    #[serde(default)]
    pub next_check_at_ms: i64,
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
/// Best-effort: a machine that cannot record the forgetting simply asks again later, which is the
/// safe direction.
pub(crate) fn forget(fs: &dyn FsOps, layout: &Layout, source: &str) {
    let mut doc = read(fs, layout);
    let before = doc.sources.len();
    doc.sources
        .retain(|key, check| source_of(key) != source || check.failure.is_none());
    if doc.sources.len() == before {
        return;
    }
    doc.schema_version = PERSISTED_SCHEMA_VERSION;
    let _ = fs.create_dir_all(&layout.state_dir());
    let _ = crate::doc::write_doc(fs, &layout.forge_check_path(), &doc);
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

/// Whether a SCHEDULED round may dial now.
///
/// A due time absurdly far ahead is treated as due rather than obeyed: a backwards clock step (or
/// a corrupted value) must not suspend auto-updates until wall time catches up. The ceiling admits
/// everything [`next_due`] can legitimately produce — the interval, its spread, AND a host's own
/// backoff — because a due time this code really wrote must never be mistaken for corruption and
/// dialed through.
pub(crate) fn due(doc: &ForgeCheck, now_ms: i64) -> bool {
    let ceiling = now_ms.saturating_add(CHECK_INTERVAL_MS + CHECK_JITTER_MS + MAX_BACKOFF_MS);
    now_ms >= doc.next_check_at_ms || doc.next_check_at_ms > ceiling
}

/// When the next scheduled check falls due: one interval from now, pushed out by an independent
/// spread, and never earlier than a backoff the host itself asked for.
pub(crate) fn next_due(now_ms: i64, jitter_ms: i64, host_backoff_ms: Option<i64>) -> i64 {
    let own = now_ms.saturating_add(CHECK_INTERVAL_MS.saturating_add(jitter_ms.max(0)));
    // A host's backoff only ever DELAYS: obeying one that fell earlier than our own interval would
    // let a server talk topos into checking more often than it decided to.
    own.max(host_backoff_ms.unwrap_or(i64::MIN))
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

/// Persist the round: the new due time and the per-source outcomes it produced, merged over what
/// was already recorded (a source no row named this run keeps its history).
///
/// # Errors
/// The filesystem failure writing the document. The caller treats it as a disclosure, never as a
/// reason to fail a sweep that otherwise worked.
pub(crate) fn record_round(
    fs: &dyn FsOps,
    layout: &Layout,
    next_check_at_ms: i64,
    outcomes: &[(String, SourceCheck)],
) -> Result<(), ClientError> {
    let mut doc = read(fs, layout);
    doc.schema_version = PERSISTED_SCHEMA_VERSION;
    doc.next_check_at_ms = next_check_at_ms;
    for (source, check) in outcomes {
        doc.sources.insert(source.clone(), check.clone());
    }
    fs.create_dir_all(&layout.state_dir())?;
    crate::doc::write_doc(fs, &layout.forge_check_path(), &doc)
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

    #[test]
    fn a_fresh_machine_is_due_and_a_recorded_round_is_not() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        let now = 1_700_000_000_000;

        // Nothing recorded → check now.
        assert!(due(&read(&fs, &layout), now), "a fresh machine checks");

        record_round(&fs, &layout, next_due(now, 0, None), &[]).unwrap();
        let doc = read(&fs, &layout);
        assert_eq!(doc.next_check_at_ms, now + CHECK_INTERVAL_MS);
        assert!(!due(&doc, now), "inside the interval nothing dials");
        assert!(
            !due(&doc, now + CHECK_INTERVAL_MS - 1),
            "one millisecond short is still inside"
        );
        assert!(due(&doc, now + CHECK_INTERVAL_MS), "the interval elapses");
    }

    #[test]
    fn the_spread_only_ever_delays_and_stays_inside_one_hour() {
        let now = 1_700_000_000_000;
        assert_eq!(next_due(now, 0, None), now + CHECK_INTERVAL_MS);
        assert_eq!(
            next_due(now, CHECK_JITTER_MS, None),
            now + CHECK_INTERVAL_MS + CHECK_JITTER_MS
        );
        // A negative draw can never pull the next check earlier than the plain interval.
        assert_eq!(next_due(now, -5_000, None), now + CHECK_INTERVAL_MS);
    }

    #[test]
    fn a_hosts_backoff_may_delay_the_next_check_but_never_hasten_it() {
        let now = 1_700_000_000_000;
        let far = now + CHECK_INTERVAL_MS * 3;
        assert_eq!(
            next_due(now, 0, Some(far)),
            far,
            "a longer backoff is obeyed"
        );
        assert_eq!(
            next_due(now, 0, Some(now + 60_000)),
            now + CHECK_INTERVAL_MS,
            "a shorter one never talks topos into checking sooner"
        );
    }

    #[test]
    fn a_far_future_due_time_never_suspends_the_lane() {
        // A backwards clock step (or a corrupt value) must not stop auto-updates until wall time
        // catches up — the same fail-open rule the sweep's own throttle keeps.
        let now = 1_700_000_000_000;
        let doc = ForgeCheck {
            next_check_at_ms: now + 400 * 24 * 60 * 60 * 1000,
            ..ForgeCheck::default()
        };
        assert!(due(&doc, now));

        // But a due time this code really wrote is OBEYED, however far out — a host that asked
        // for a day must get a day, not be dialed through as if the file were corrupt.
        let honored = ForgeCheck {
            next_check_at_ms: next_due(now, CHECK_JITTER_MS, Some(now + MAX_BACKOFF_MS)),
            ..ForgeCheck::default()
        };
        assert!(
            !due(&honored, now),
            "a host's longest legitimate backoff holds"
        );
    }

    #[test]
    fn an_unreadable_or_foreign_document_fails_open() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        std::fs::create_dir_all(layout.state_dir()).unwrap();

        std::fs::write(layout.forge_check_path(), b"{ not json ").unwrap();
        assert!(due(&read(&fs, &layout), 0), "garbage means check");

        let foreign = format!(
            "{{\"schema_version\": {}, \"next_check_at_ms\": 9999999999999}}",
            PERSISTED_SCHEMA_VERSION + 1
        );
        std::fs::write(layout.forge_check_path(), foreign).unwrap();
        let doc = read(&fs, &layout);
        assert_eq!(doc, ForgeCheck::default(), "a newer build's doc is foreign");
        assert!(due(&doc, 0));
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
        let now = 1_700_000_000_000;
        record_round(
            &fs,
            &layout,
            next_due(now, 0, None),
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
            doc.next_check_at_ms,
            now + CHECK_INTERVAL_MS,
            "forgetting is not a round; the clock is untouched"
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
        };
        record_round(
            &fs,
            &layout,
            next_due(now, 0, None),
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
            next_due(later, 0, None),
            &[("github.com/o/a".to_owned(), ok(later, "ccc"))],
        )
        .unwrap();
        let doc = read(&fs, &layout);
        assert_eq!(doc.next_check_at_ms, later + CHECK_INTERVAL_MS);
        assert_eq!(doc.sources["github.com/o/a"].commit.as_deref(), Some("ccc"));
        assert_eq!(doc.sources["github.com/o/b"].commit.as_deref(), Some("bbb"));
        assert_eq!(doc.sources["github.com/o/b"].checked_at_ms, now);
    }
}
