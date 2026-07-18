//! The quiet sweep's self-throttle — the TTL + single-flight gate the hook path passes BEFORE any
//! engine or network work, and the hook-JSON stdout the sweep emits so a harness can act on it.
//!
//! Auto-update hooks now fire on every session-start-shaped event (startup, resume, clear, compact —
//! and, on other harnesses, session resets or a 1-minute cron), so the sweep must be cheap to
//! invoke redundantly. Two mechanisms, both client-local under `~/.topos/`:
//!
//! - **Single-flight** — `locks/currency.lock`: the quiet path TRY-locks it; a held lock means
//!   another sweep is already in flight, so this invocation exits 0 silently. An explicit bare
//!   `topos update` takes the same lock BLOCKING (it must run, but never concurrently).
//! - **TTL** — `state/quiet_sweep.json` records when the last bare sweep COMPLETED; a quiet
//!   invocation within the window (default [`DEFAULT_TTL_SECS`]; `--ttl` flag > `TOPOS_UPDATE_TTL`
//!   env > default; `0` disables) exits 0 silently. The stamp is written AFTER a sweep completes —
//!   a crash mid-sweep leaves the old stamp, so the next session retries instead of going quiet
//!   for a window. Explicit (non-quiet) sweeps ignore the TTL and refresh the stamp.
//!
//! The gate reads fail OPEN (an unreadable stamp runs the sweep — throttling is an optimization,
//! staleness is the failure that matters) while every write stays crash-safe through the shared
//! [`crate::doc`] machinery.

use serde::{Deserialize, Serialize};
use topos_types::PERSISTED_SCHEMA_VERSION;
use topos_types::results::{PullAction, PullData};

use crate::error::ClientError;
use crate::fs_seam::{FsOps, LockGuard};
use crate::sidecar::Layout;

/// The default quiet-sweep TTL (seconds): inside this window a hook-path `update --quiet` is a
/// silent no-op. Five minutes keeps a busy multi-session machine at a handful of sweeps per hour
/// while a fresh update still lands within minutes everywhere.
pub(crate) const DEFAULT_TTL_SECS: u64 = 300;

/// The env override for the default TTL (seconds). The `--ttl` flag wins over it; an unparsable
/// value is ignored (the hook must never fail on a typo'd environment).
pub(crate) const TTL_ENV_VAR: &str = "TOPOS_UPDATE_TTL";

/// `state/quiet_sweep.json` — when the last bare sweep completed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct QuietSweepStamp {
    #[serde(default)]
    schema_version: u32,
    /// Epoch millis of the last COMPLETED bare sweep (quiet or explicit).
    #[serde(default)]
    last_sweep_at_ms: i64,
}

/// The quiet gate's verdict.
pub(crate) enum QuietGate {
    /// Sweep now; the guard holds the single-flight lock until dropped.
    Run(LockGuard),
    /// Skip silently (exit 0, zero output).
    Skip(SkipReason),
}

/// Why a quiet invocation skipped (diagnostics only — stdout stays empty either way).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// Another sweep holds `locks/currency.lock` right now.
    InFlight,
    /// The last completed sweep is within the TTL window.
    Fresh,
}

/// Resolve the effective TTL in milliseconds: the `--ttl` flag, else the env, else the default.
/// `0` means "no throttle".
pub(crate) fn resolve_ttl_ms(flag_secs: Option<u64>) -> u64 {
    let secs = flag_secs
        .or_else(|| {
            std::env::var(TTL_ENV_VAR)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        })
        .unwrap_or(DEFAULT_TTL_SECS);
    secs.saturating_mul(1000)
}

/// Pass the quiet gate: single-flight first (a held lock skips regardless of TTL), then the TTL
/// read. Gate reads fail OPEN — an unreadable/foreign-schema stamp runs the sweep.
pub(crate) fn quiet_gate(
    fs: &dyn FsOps,
    layout: &Layout,
    now_ms: i64,
    ttl_ms: u64,
) -> Result<QuietGate, ClientError> {
    fs.create_dir_all(&layout.locks_dir())?;
    let Some(guard) = fs.try_lock_exclusive(&layout.currency_lock_file())? else {
        return Ok(QuietGate::Skip(SkipReason::InFlight));
    };
    if ttl_ms > 0
        && let Some(last) = read_stamp(fs, layout)
        // A FUTURE stamp (a backwards clock step, or a corrupted value) must never suppress
        // sweeps until wall time catches up — only a past stamp inside the window skips.
        && last <= now_ms
        && now_ms.saturating_sub(last) < i64::try_from(ttl_ms).unwrap_or(i64::MAX)
    {
        return Ok(QuietGate::Skip(SkipReason::Fresh));
    }
    Ok(QuietGate::Run(guard))
}

/// Take the single-flight lock BLOCKING — the explicit bare sweep's entry (it always runs, never
/// concurrently with another sweep).
pub(crate) fn sweep_lock(fs: &dyn FsOps, layout: &Layout) -> Result<LockGuard, ClientError> {
    fs.create_dir_all(&layout.locks_dir())?;
    Ok(fs.lock_exclusive(&layout.currency_lock_file())?)
}

/// The stamp's last-completed time, or `None` when absent/unreadable/foreign (fail open). A
/// NEWER-schema stamp (written by a later client before a downgrade) is foreign — its semantics
/// are unknown, so it never throttles.
fn read_stamp(fs: &dyn FsOps, layout: &Layout) -> Option<i64> {
    let bytes = fs.read_opt(&layout.quiet_sweep_path()).ok()??;
    let stamp: QuietSweepStamp = serde_json::from_slice(&bytes).ok()?;
    (stamp.schema_version <= PERSISTED_SCHEMA_VERSION).then_some(stamp.last_sweep_at_ms)
}

/// Record a completed bare sweep (best-effort: a failed stamp write must never fail the sweep that
/// already succeeded — the next invocation just sweeps again).
pub(crate) fn stamp_sweep(fs: &dyn FsOps, layout: &Layout, now_ms: i64) {
    let stamp = QuietSweepStamp {
        schema_version: PERSISTED_SCHEMA_VERSION,
        last_sweep_at_ms: now_ms,
    };
    let _ = fs.create_dir_all(&layout.state_dir());
    let _ = crate::doc::write_doc(fs, &layout.quiet_sweep_path(), &stamp);
}

/// Whether the sweep CHANGED placement bytes in some agent dir — installed new bytes
/// (fast-forward), landed a merge or a conflict tree, or cleaned a withdrawn skill's dirs. Offers,
/// holds, freezes, and up-to-date rows change nothing on disk.
pub(crate) fn sweep_changed_bytes(data: &PullData) -> bool {
    data.skills.iter().any(|s| {
        matches!(
            s.action,
            PullAction::FastForwarded
                | PullAction::Merged
                | PullAction::Conflicted
                | PullAction::Withdrawn
        )
    })
}

/// The quiet hook's ONE stdout document when the sweep changed bytes: the SessionStart hook-output
/// JSON telling Claude Code to re-scan its skill dirs (`reloadSkills`), with any person-facing
/// lines riding `additionalContext` (context-injected — exactly where plain hook stdout lands, so
/// nothing a person must see is lost to the JSON shape). Harnesses that ignore hook stdout
/// (Hermes session hooks, a silent cron) simply discard it — the command stays byte-identical
/// across every adapter. With NO byte changes the caller keeps today's plain-lines behavior.
pub(crate) fn reload_skills_json(person_lines: &[String]) -> String {
    let mut inner = serde_json::Map::new();
    inner.insert(
        "hookEventName".to_owned(),
        serde_json::Value::String("SessionStart".to_owned()),
    );
    inner.insert("reloadSkills".to_owned(), serde_json::Value::Bool(true));
    if !person_lines.is_empty() {
        inner.insert(
            "additionalContext".to_owned(),
            serde_json::Value::String(person_lines.join("\n")),
        );
    }
    let doc = serde_json::json!({ "hookSpecificOutput": inner });
    doc.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use topos_types::results::PullSkill;

    /// A self-cleaning temp `~/.topos` home (RAII).
    struct TempHome(PathBuf);
    impl TempHome {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("topos-gate-{}-{n}", std::process::id()));
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

    fn skill_row(action: PullAction) -> PullSkill {
        PullSkill {
            skill: "s".into(),
            workspace_id: None,
            observed: 1,
            applied: 1,
            action,
            offer: None,
            conflict: None,
            merge: None,
            merge_preview: None,
        }
    }

    #[test]
    fn gate_runs_with_no_stamp_then_skips_inside_the_ttl() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        let ttl = 300_000;

        // No stamp yet → run.
        match quiet_gate(&fs, &layout, 1_000_000, ttl).unwrap() {
            QuietGate::Run(guard) => drop(guard),
            QuietGate::Skip(r) => panic!("must run with no stamp, skipped: {r:?}"),
        }

        // A completed sweep stamps; the next invocation inside the window skips…
        stamp_sweep(&fs, &layout, 1_000_000);
        match quiet_gate(&fs, &layout, 1_000_000 + ttl_i64(ttl) - 1, ttl).unwrap() {
            QuietGate::Skip(SkipReason::Fresh) => {}
            _ => panic!("must skip inside the TTL"),
        }
        // …and one past the window runs again.
        match quiet_gate(&fs, &layout, 1_000_000 + ttl_i64(ttl), ttl).unwrap() {
            QuietGate::Run(guard) => drop(guard),
            QuietGate::Skip(r) => panic!("must run past the TTL, skipped: {r:?}"),
        }
    }

    fn ttl_i64(ttl: u64) -> i64 {
        i64::try_from(ttl).unwrap()
    }

    #[test]
    fn ttl_zero_disables_the_throttle() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        stamp_sweep(&fs, &layout, 5_000);
        match quiet_gate(&fs, &layout, 5_001, 0).unwrap() {
            QuietGate::Run(guard) => drop(guard),
            QuietGate::Skip(r) => panic!("--ttl 0 must sweep now, skipped: {r:?}"),
        }
    }

    #[test]
    fn a_held_lock_skips_as_in_flight_regardless_of_ttl() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        let _held = sweep_lock(&fs, &layout).unwrap();
        match quiet_gate(&fs, &layout, 0, 0).unwrap() {
            QuietGate::Skip(SkipReason::InFlight) => {}
            _ => panic!("a held single-flight lock must skip"),
        }
    }

    #[test]
    fn an_unreadable_stamp_fails_open_and_sweeps() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        std::fs::create_dir_all(layout.state_dir()).unwrap();
        std::fs::write(layout.quiet_sweep_path(), b"{ not json ").unwrap();
        match quiet_gate(&fs, &layout, 1, 300_000).unwrap() {
            QuietGate::Run(guard) => drop(guard),
            QuietGate::Skip(r) => panic!("an unreadable stamp must fail open, skipped: {r:?}"),
        }
    }

    #[test]
    fn a_future_stamp_never_throttles() {
        // A backwards clock step (or a corrupted value) leaves the stamp AHEAD of now: it must
        // never suppress sweeps until wall time catches up — a future stamp runs.
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        stamp_sweep(&fs, &layout, 10_000_000);
        match quiet_gate(&fs, &layout, 1_000, 300_000).unwrap() {
            QuietGate::Run(guard) => drop(guard),
            QuietGate::Skip(r) => panic!("a future stamp must fail open, skipped: {r:?}"),
        }
    }

    #[test]
    fn a_newer_schema_stamp_is_foreign_and_never_throttles() {
        // A later client's stamp (before a downgrade) has unknown semantics — never trusted.
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        std::fs::create_dir_all(layout.state_dir()).unwrap();
        let future_schema = format!(
            "{{\"schema_version\": {}, \"last_sweep_at_ms\": 1000}}",
            topos_types::PERSISTED_SCHEMA_VERSION + 1
        );
        std::fs::write(layout.quiet_sweep_path(), future_schema).unwrap();
        match quiet_gate(&fs, &layout, 1_001, 300_000).unwrap() {
            QuietGate::Run(guard) => drop(guard),
            QuietGate::Skip(r) => panic!("a newer-schema stamp must fail open, skipped: {r:?}"),
        }
    }

    #[test]
    fn ttl_resolution_prefers_flag_over_env_over_default() {
        // The flag wins outright (no env read when the flag is present).
        assert_eq!(resolve_ttl_ms(Some(60)), 60_000);
        assert_eq!(resolve_ttl_ms(Some(0)), 0);
        // No flag → the env would be consulted; with neither, the default. (The env row itself is
        // not exercised here — `set_var` is unsafe under the workspace's forbid(unsafe_code) and a
        // process-global mutation would race sibling tests; the parse path is covered below.)
        assert_eq!(resolve_ttl_ms(None), DEFAULT_TTL_SECS * 1000);
        // The env parse rule: an unparsable value is ignored (the default wins) — proven on the
        // parser the env path uses.
        assert_eq!("300x".trim().parse::<u64>().ok(), None);
        assert_eq!(" 42 ".trim().parse::<u64>().ok(), Some(42));
    }

    #[test]
    fn changed_bytes_detection_matches_the_placement_writing_actions() {
        for (action, changed) in [
            (PullAction::UpToDate, false),
            (PullAction::FastForwarded, true),
            (PullAction::Offered, false),
            (PullAction::Diverged, false),
            (PullAction::Merged, true),
            (PullAction::Conflicted, true),
            (PullAction::Held, false),
            (PullAction::Withdrawn, true),
            (PullAction::Detached, false),
            (PullAction::Excluded, false),
        ] {
            let data = PullData {
                skills: vec![skill_row(action)],
                proposals_awaiting: 0,
                notices: Vec::new(),
                sync: Vec::new(),
            };
            assert_eq!(sweep_changed_bytes(&data), changed, "{action:?}");
        }
        let empty = PullData {
            skills: Vec::new(),
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
        };
        assert!(!sweep_changed_bytes(&empty));
    }

    #[test]
    fn reload_skills_json_is_the_documented_session_start_shape() {
        let bare = reload_skills_json(&[]);
        let v: serde_json::Value = serde_json::from_str(&bare).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_eq!(v["hookSpecificOutput"]["reloadSkills"], true);
        assert!(
            v["hookSpecificOutput"].get("additionalContext").is_none(),
            "no person lines → no context field"
        );

        let with_lines = reload_skills_json(&["topos: a".to_owned(), "topos: b".to_owned()]);
        let v: serde_json::Value = serde_json::from_str(&with_lines).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"], "topos: a\ntopos: b",
            "person-facing lines ride the context injection, never lost"
        );
    }
}
