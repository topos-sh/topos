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

/// Whether the sweep CHANGED placement bytes in some agent dir — installed new bytes (a first
/// install, or a fast-forward), rewrote a copy that stood behind the applied version, landed a
/// merge or a conflict tree, cleaned a withdrawn or by-choice-removed skill's dirs, or copied a
/// settled draft onto sibling folders. Offers, holds, freezes, and up-to-date rows change nothing
/// on disk.
pub(crate) fn sweep_changed_bytes(data: &PullData) -> bool {
    data.skills.iter().any(|s| {
        matches!(
            s.action,
            PullAction::FastForwarded
                | PullAction::Installed
                | PullAction::Refreshed
                | PullAction::Removed
                | PullAction::Merged
                | PullAction::Conflicted
                | PullAction::Withdrawn
                | PullAction::DraftSynced
        )
    })
}

/// Which stdout dialect the quiet sweep speaks, chosen by the calling trigger's `--hook <harness>`
/// marker.
///
/// The SessionStart hook-output document is NOT one universal shape. Some agents validate hook
/// stdout against a strict schema that permits only `hookEventName` + `additionalContext` and
/// REJECTS any other key (Codex does exactly this — an unknown field paints the whole hook as
/// failed at session start). Others understand a reload extension that makes freshly pulled skill
/// bytes live in the same session. So the conservative shape is the DEFAULT every trigger gets,
/// and an agent that understands the extension opts in by naming itself in its own registered
/// command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookDialect {
    /// Claude Code: the reload extension (`reloadSkills`) is understood, so pulled bytes go live
    /// same-session.
    ClaudeCode,
    /// Everyone else: only the two schema-universal keys, and nothing at all to say when there is
    /// nothing a person must read (empty stdout is accepted everywhere).
    Conservative,
}

impl HookDialect {
    /// Resolve the dialect from the `--hook <harness>` marker. Unknown names and a missing marker
    /// both fall back to [`HookDialect::Conservative`] — this runs inside a session-start hook, so
    /// an unrecognized value must degrade quietly, never fail the sweep.
    pub(crate) fn from_slug(slug: Option<&str>) -> Self {
        match slug {
            Some("claude-code") => Self::ClaudeCode,
            _ => Self::Conservative,
        }
    }
}

/// The quiet hook's ONE stdout document, in the caller's dialect — or `None` when there is nothing
/// to say (the caller then prints NOTHING, the shape every agent accepts).
///
/// This is the sweep's only way to reach a session, both when bytes moved and when they did not.
/// Raw text on stdout is not a substitute: `additionalContext` is the field that actually INJECTS
/// text into the session, and an agent validating hook output against a strict schema is free to
/// discard anything that is not the document it expects. A person-facing fact — an ended session's
/// freeze, a stale/unreachable warning — must never depend on that discarding not happening.
///
/// The two axes are independent, and keeping them independent is the point:
///
/// - `changed` — bytes actually moved on disk. It is the ONLY thing that may set `reloadSkills`,
///   because that field asks the agent to re-scan its skill dirs; saying it on a sweep that
///   changed nothing would be a lie with a cost.
/// - `person_lines` — facts a person must not miss. They ride `additionalContext` whenever they
///   exist, in EITHER dialect, changed or not.
///
/// So [`ClaudeCode`] speaks when either axis is live (with `reloadSkills` only on `changed`), and
/// [`Conservative`] speaks only when there are lines — it can carry nothing else, so a
/// changed-but-silent sweep has nothing to tell it. Agents that ignore hook stdout entirely
/// (Hermes session hooks, a silent cron) simply discard whatever arrives.
///
/// [`ClaudeCode`]: HookDialect::ClaudeCode
/// [`Conservative`]: HookDialect::Conservative
pub(crate) fn hook_output_json(
    dialect: HookDialect,
    changed: bool,
    person_lines: &[String],
) -> Option<String> {
    let reload = changed && dialect == HookDialect::ClaudeCode;
    if !reload && person_lines.is_empty() {
        return None;
    }
    let mut inner = serde_json::Map::new();
    inner.insert(
        "hookEventName".to_owned(),
        serde_json::Value::String("SessionStart".to_owned()),
    );
    if reload {
        inner.insert("reloadSkills".to_owned(), serde_json::Value::Bool(true));
    }
    if !person_lines.is_empty() {
        inner.insert(
            "additionalContext".to_owned(),
            serde_json::Value::String(person_lines.join("\n")),
        );
    }
    let doc = serde_json::json!({ "hookSpecificOutput": inner });
    Some(doc.to_string())
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
            synced_placements: None,
            destinations: Vec::new(),
            kept: Vec::new(),
            display: None,
            note: None,
            scope: None,
            harnesses: Vec::new(),
            kind: None,
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
            (PullAction::Installed, true),
            // A refresh rewrote a folder that had fallen behind — bytes moved on disk.
            (PullAction::Refreshed, true),
            (PullAction::Removed, true),
            (PullAction::Offered, false),
            (PullAction::Diverged, false),
            (PullAction::Merged, true),
            (PullAction::Conflicted, true),
            (PullAction::Held, false),
            (PullAction::Withdrawn, true),
            (PullAction::Excluded, false),
            // A released row writes a marker, never placement bytes — the resolution deletes
            // nothing on disk.
            (PullAction::Released, false),
            (PullAction::DraftSynced, true),
        ] {
            let data = PullData {
                skills: vec![skill_row(action)],
                proposals_awaiting: 0,
                notices: Vec::new(),
                sync: Vec::new(),
                scope: None,
            };
            assert_eq!(sweep_changed_bytes(&data), changed, "{action:?}");
        }
        let empty = PullData {
            skills: Vec::new(),
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            scope: None,
        };
        assert!(!sweep_changed_bytes(&empty));
    }

    /// The WHOLE decision surface: dialect × `changed` × person-lines, with the exact bytes. A
    /// hook document is a wire a harness parses, so every cell is pinned byte-for-byte.
    #[test]
    fn the_hook_document_matrix_over_dialect_changed_and_lines() {
        let lines = ["topos: a".to_owned(), "topos: b".to_owned()];
        const CC: HookDialect = HookDialect::ClaudeCode;
        const CO: HookDialect = HookDialect::Conservative;

        for (dialect, changed, has_lines, want) in [
            // Claude Code — `reloadSkills` tracks `changed` and NOTHING else.
            (
                CC,
                true,
                false,
                Some(
                    r#"{"hookSpecificOutput":{"hookEventName":"SessionStart","reloadSkills":true}}"#,
                ),
            ),
            (
                CC,
                true,
                true,
                Some(
                    r#"{"hookSpecificOutput":{"additionalContext":"topos: a\ntopos: b","hookEventName":"SessionStart","reloadSkills":true}}"#,
                ),
            ),
            // THE case the two-axis split exists for: nothing moved, but a person must read
            // something. The lines go out; asking for a re-scan would be a lie with a cost.
            (
                CC,
                false,
                true,
                Some(
                    r#"{"hookSpecificOutput":{"additionalContext":"topos: a\ntopos: b","hookEventName":"SessionStart"}}"#,
                ),
            ),
            (CC, false, false, None),
            // Conservative — never `reloadSkills`, on any axis; it can carry only the two keys a
            // strict validator permits, so a changed-but-silent sweep has nothing to tell it.
            (CO, true, false, None),
            (
                CO,
                true,
                true,
                Some(
                    r#"{"hookSpecificOutput":{"additionalContext":"topos: a\ntopos: b","hookEventName":"SessionStart"}}"#,
                ),
            ),
            (
                CO,
                false,
                true,
                Some(
                    r#"{"hookSpecificOutput":{"additionalContext":"topos: a\ntopos: b","hookEventName":"SessionStart"}}"#,
                ),
            ),
            (CO, false, false, None),
        ] {
            let given: &[String] = if has_lines { &lines } else { &[] };
            let got = hook_output_json(dialect, changed, given);
            let case = format!("{dialect:?} changed={changed} lines={has_lines}");
            assert_eq!(got.as_deref(), want, "{case}");

            let Some(doc) = got else { continue };
            let v: serde_json::Value = serde_json::from_str(&doc).unwrap();
            let inner = v["hookSpecificOutput"].as_object().unwrap();
            assert_eq!(inner["hookEventName"], "SessionStart", "{case}");
            // `reloadSkills` appears if and ONLY if bytes moved under a dialect that grasps it.
            assert_eq!(
                inner.get("reloadSkills").is_some(),
                changed && dialect == CC,
                "{case}: the reload extension tracks changed bytes under ClaudeCode alone"
            );
            // Person-facing lines are never dropped, in either dialect, changed or not.
            assert_eq!(
                inner.get("additionalContext").and_then(|c| c.as_str()),
                has_lines.then_some("topos: a\ntopos: b"),
                "{case}: person lines ride the context injection or nothing does"
            );
            if dialect == CO {
                assert!(
                    inner.len() <= 2,
                    "{case}: only the two keys a strict validator permits"
                );
            }
        }
    }

    /// The quiet path's SOFT failure (auth/transport: warn, still exit 0) is a person-facing line
    /// like any other, so it takes the same route — a document, never raw stdout a strict-schema
    /// agent may discard instead of inject. Nothing landed, so `changed` is false and NO dialect
    /// may claim a reload. (The caller's exit status is unaffected; only the shape is.)
    #[test]
    fn a_soft_failure_warning_is_a_document_in_both_dialects_never_raw_text() {
        let line = "topos: update skipped — the server is unreachable".to_owned();
        for dialect in [HookDialect::ClaudeCode, HookDialect::Conservative] {
            let doc = hook_output_json(dialect, false, std::slice::from_ref(&line)).unwrap_or_else(
                || panic!("{dialect:?}: a soft-failure warning must still be said"),
            );
            let v: serde_json::Value = serde_json::from_str(&doc).unwrap();
            let inner = v["hookSpecificOutput"].as_object().unwrap();
            assert_eq!(inner["hookEventName"], "SessionStart", "{dialect:?}");
            assert_eq!(
                inner["additionalContext"], line,
                "{dialect:?}: the warning rides the context injection verbatim"
            );
            assert!(
                !inner.contains_key("reloadSkills"),
                "{dialect:?}: a skipped update landed nothing — never ask for a re-scan"
            );
        }
    }

    #[test]
    fn an_unknown_or_absent_hook_marker_falls_back_to_conservative() {
        assert_eq!(
            HookDialect::from_slug(Some("claude-code")),
            HookDialect::ClaudeCode
        );
        for slug in [
            None,
            Some(""),
            Some("codex"),
            Some("Claude-Code"),
            Some("x"),
        ] {
            assert_eq!(
                HookDialect::from_slug(slug),
                HookDialect::Conservative,
                "{slug:?} must fail closed onto the schema-conservative dialect"
            );
        }
    }
}
