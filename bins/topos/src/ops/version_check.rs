//! The PASSIVE version check — a best-effort, at-most-daily "a newer topos exists" nag on stderr.
//!
//! After a SUCCESSFUL eligible command, at most once per [`CHECK_INTERVAL_MS`], the binary probes
//! the public GitHub `releases/latest` 302 redirect — redirects DISABLED, the tag parsed from the
//! `Location` header; no API, no auth, no JSON body — and prints ONE stderr line when the latest
//! release is newer than this build. Quiet by construction:
//!
//! - **stdout is never touched** — a `--json` consumer keeps a byte-clean document; the nag is
//!   stderr prose, and nothing lands in the envelope;
//! - **every probe failure is silent** — and the stamp (`state/version_check.json`) is written
//!   BEFORE the probe, so an offline machine does not re-dial on every command;
//! - **the first eligible command only lays the stamp and never probes** — a fresh install was just
//!   installed current, and a short-lived test/e2e home never dials out;
//! - the quiet sweep (`update --quiet`, the session-start hook path), `self-update` itself (it just
//!   talked to releases), `uninstall` (the check would recreate the state dir the command deleted),
//!   a `TOPOS_INSTALL_BASE_URL` mirror (it cannot answer `/latest` — the self-updater refuses
//!   latest-resolution there for the same reason), and [`NO_UPDATE_CHECK_ENV`] all skip the check
//!   entirely — gated in the composition root before any state or network work.

use serde::{Deserialize, Serialize};
use topos_types::PERSISTED_SCHEMA_VERSION;

use crate::fs_seam::FsOps;
use crate::release::ReleaseProbe;
use crate::sidecar::Layout;

use super::self_update::{CURRENT_VERSION, version_gt};

/// At most one probe per day.
pub(crate) const CHECK_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;

/// The opt-out env var (`TOPOS_NO_UPDATE_CHECK=1`; any non-empty value disables the passive check).
pub(crate) const NO_UPDATE_CHECK_ENV: &str = "TOPOS_NO_UPDATE_CHECK";

/// `state/version_check.json` — when the passive check last ATTEMPTED its probe (epoch millis).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct VersionCheckStamp {
    #[serde(default)]
    schema_version: u32,
    /// Epoch millis of the last probe ATTEMPT (stamped before the probe — failures hold the cadence).
    #[serde(default)]
    last_check_at_ms: i64,
}

/// The environment gates, read once in the composition root: [`NO_UPDATE_CHECK_ENV`] (non-empty
/// disables) and a `TOPOS_INSTALL_BASE_URL` mirror override (a mirror serves no `releases/latest`
/// redirect, so probing GitHub would nag about releases the mirror may not carry).
pub(crate) fn version_check_env_allows() -> bool {
    env_gate(
        std::env::var_os(NO_UPDATE_CHECK_ENV).as_deref(),
        std::env::var_os("TOPOS_INSTALL_BASE_URL").as_deref(),
    )
}

/// The pure classification behind [`version_check_env_allows`] (unit-tested without `set_var`).
fn env_gate(no_check: Option<&std::ffi::OsStr>, mirror: Option<&std::ffi::OsStr>) -> bool {
    let unset = |v: Option<&std::ffi::OsStr>| v.is_none_or(|v| v.is_empty());
    unset(no_check) && unset(mirror)
}

/// Run the passive check. The composition root calls this AFTER a successful eligible command,
/// having already applied [`version_check_env_allows`] and the per-command gates. Returns the one
/// nag line for stderr, or `None` — silence is the contract for every failure shape.
pub(crate) fn version_nag(
    fs: &dyn FsOps,
    layout: &Layout,
    now_ms: i64,
    probe: &dyn ReleaseProbe,
) -> Option<String> {
    match read_stamp(fs, layout) {
        // First eligible command (or an unreadable stamp): lay the stamp, never probe — a fresh
        // install is current, and the rewrite heals a corrupt doc without turning it into a dial.
        StampRead::Absent => {
            write_stamp(fs, layout, now_ms);
            None
        }
        // A NEWER-schema stamp belongs to a later client sharing this home — never overwrite its
        // document, never probe on its behalf.
        StampRead::Foreign => None,
        // A FUTURE stamp (a backwards clock step) would otherwise suppress nothing and probe on
        // EVERY command until wall time catches up — re-stamp to now and skip instead.
        StampRead::At(last) if last > now_ms => {
            write_stamp(fs, layout, now_ms);
            None
        }
        // Inside the window: nothing to do.
        StampRead::At(last) if now_ms.saturating_sub(last) < CHECK_INTERVAL_MS => None,
        // Due: stamp FIRST (a failed probe must hold the daily cadence), then probe, then compare.
        StampRead::At(_) => {
            write_stamp(fs, layout, now_ms);
            let location = probe.latest_release_location()?;
            let tag = latest_tag_from_location(&location)?;
            let latest = tag.trim_start_matches('v');
            version_gt(latest, CURRENT_VERSION).then(|| nag_line(latest))
        }
    }
}

/// The one stderr line: the newer version, the command that installs it, and the opt-out.
fn nag_line(latest: &str) -> String {
    format!(
        "topos: a newer topos is available: v{latest} (you have {CURRENT_VERSION}) — run `topos \
         self-update` ({NO_UPDATE_CHECK_ENV}=1 silences this check)"
    )
}

/// Parse the release tag out of a `releases/latest` redirect `Location` — the path segment after
/// `/releases/tag/`, query/fragment stripped. Anything else (an error page, an unexpected layout)
/// is a silent `None`.
fn latest_tag_from_location(location: &str) -> Option<String> {
    let (_, tag) = location.split_once("/releases/tag/")?;
    let tag = tag.split(['?', '#']).next().unwrap_or(tag);
    (!tag.is_empty() && !tag.contains('/')).then(|| tag.to_owned())
}

/// The stamp read, three-way: absent/unreadable (first-run), foreign (a newer client's doc), or a
/// timestamp.
enum StampRead {
    Absent,
    Foreign,
    At(i64),
}

fn read_stamp(fs: &dyn FsOps, layout: &Layout) -> StampRead {
    let Ok(Some(bytes)) = fs.read_opt(&layout.version_check_path()) else {
        return StampRead::Absent;
    };
    let Ok(stamp) = serde_json::from_slice::<VersionCheckStamp>(&bytes) else {
        return StampRead::Absent;
    };
    if stamp.schema_version > PERSISTED_SCHEMA_VERSION {
        StampRead::Foreign
    } else {
        StampRead::At(stamp.last_check_at_ms)
    }
}

/// Record a probe attempt (best-effort: a failed stamp write must never surface — the check is
/// passive by contract; the worst case is one extra probe on a later command).
fn write_stamp(fs: &dyn FsOps, layout: &Layout, now_ms: i64) {
    let stamp = VersionCheckStamp {
        schema_version: PERSISTED_SCHEMA_VERSION,
        last_check_at_ms: now_ms,
    };
    let _ = fs.create_dir_all(&layout.state_dir());
    let _ = crate::doc::write_doc(fs, &layout.version_check_path(), &stamp);
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::fs_seam::RealFs;

    /// A self-cleaning temp `~/.topos` home (RAII).
    struct TempHome(PathBuf);
    impl TempHome {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("topos-vchk-{}-{n}", std::process::id()));
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

    /// A probe that must NEVER be dialed — the throttle/grace tests inject it.
    struct NeverProbe;
    impl ReleaseProbe for NeverProbe {
        fn latest_release_location(&self) -> Option<String> {
            panic!("the probe must not be dialed on this path")
        }
    }

    /// A canned probe counting its calls.
    struct FakeProbe {
        location: Option<String>,
        calls: Cell<u32>,
    }
    impl FakeProbe {
        fn answering(tag: &str) -> Self {
            Self {
                location: Some(format!(
                    "https://github.com/topos-sh/topos/releases/tag/{tag}"
                )),
                calls: Cell::new(0),
            }
        }
        fn failing() -> Self {
            Self {
                location: None,
                calls: Cell::new(0),
            }
        }
    }
    impl ReleaseProbe for FakeProbe {
        fn latest_release_location(&self) -> Option<String> {
            self.calls.set(self.calls.get() + 1);
            self.location.clone()
        }
    }

    fn stamped_at(_fs: &RealFs, layout: &Layout) -> i64 {
        let bytes = std::fs::read(layout.version_check_path()).expect("stamp exists");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        v["last_check_at_ms"].as_i64().expect("stamp timestamp")
    }

    const DAY: i64 = CHECK_INTERVAL_MS;

    #[test]
    fn first_run_lays_the_stamp_and_never_probes() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        // NeverProbe panics if dialed — the first eligible command must only stamp.
        assert_eq!(version_nag(&fs, &layout, 1_000, &NeverProbe), None);
        assert_eq!(stamped_at(&fs, &layout), 1_000);
    }

    #[test]
    fn within_the_window_never_probes() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        assert_eq!(version_nag(&fs, &layout, 1_000, &NeverProbe), None);
        // One millisecond short of due: still silent, still un-dialed, stamp untouched.
        assert_eq!(
            version_nag(&fs, &layout, 1_000 + DAY - 1, &NeverProbe),
            None
        );
        assert_eq!(stamped_at(&fs, &layout), 1_000);
    }

    #[test]
    fn a_due_probe_failure_is_silent_and_still_restamps() {
        // The even-on-failure guarantee: an offline machine stamps BEFORE the probe, so the next
        // command inside the fresh window does not re-dial.
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        assert_eq!(version_nag(&fs, &layout, 1_000, &NeverProbe), None);
        let probe = FakeProbe::failing();
        assert_eq!(version_nag(&fs, &layout, 1_000 + DAY, &probe), None);
        assert_eq!(probe.calls.get(), 1);
        assert_eq!(stamped_at(&fs, &layout), 1_000 + DAY);
        // …and the immediately-following command is throttled by the refreshed stamp.
        assert_eq!(
            version_nag(&fs, &layout, 1_000 + DAY + 1, &NeverProbe),
            None
        );
    }

    #[test]
    fn a_due_probe_with_a_newer_release_nags_once() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        assert_eq!(version_nag(&fs, &layout, 1_000, &NeverProbe), None);
        let probe = FakeProbe::answering("v99.9.9");
        let line = version_nag(&fs, &layout, 1_000 + DAY, &probe).expect("a newer release nags");
        assert!(line.contains("v99.9.9"), "{line}");
        assert!(line.contains("topos self-update"), "{line}");
        assert!(line.contains(NO_UPDATE_CHECK_ENV), "{line}");
        // The next command is inside the fresh window — no second nag, no second dial.
        assert_eq!(
            version_nag(&fs, &layout, 1_000 + DAY + 1, &NeverProbe),
            None
        );
    }

    #[test]
    fn a_current_or_older_latest_stays_silent() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        for tag in [format!("v{CURRENT_VERSION}"), "v0.0.0".to_owned()] {
            assert_eq!(version_nag(&fs, &layout, 1_000, &NeverProbe), None);
            let probe = FakeProbe::answering(&tag);
            assert_eq!(
                version_nag(&fs, &layout, 1_000 + DAY, &probe),
                None,
                "{tag}"
            );
            assert_eq!(probe.calls.get(), 1, "{tag}");
            // Reset the home for the second iteration.
            let _ = std::fs::remove_file(layout.version_check_path());
        }
    }

    #[test]
    fn a_future_stamp_restamps_and_skips() {
        // A backwards clock step leaves the stamp AHEAD of now. Probing on every command until wall
        // time catches up would be the failure mode — re-stamp to now and stay silent instead.
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        assert_eq!(version_nag(&fs, &layout, 10_000_000, &NeverProbe), None);
        assert_eq!(version_nag(&fs, &layout, 1_000, &NeverProbe), None);
        assert_eq!(stamped_at(&fs, &layout), 1_000);
    }

    #[test]
    fn a_newer_schema_stamp_is_left_untouched_and_never_probed() {
        // A later client owns this document — do not overwrite it, do not dial on its behalf.
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        std::fs::create_dir_all(layout.state_dir()).unwrap();
        let foreign = format!(
            "{{\"schema_version\": {}, \"last_check_at_ms\": 1}}",
            PERSISTED_SCHEMA_VERSION + 1
        );
        std::fs::write(layout.version_check_path(), &foreign).unwrap();
        assert_eq!(version_nag(&fs, &layout, 5 * DAY, &NeverProbe), None);
        let after = std::fs::read_to_string(layout.version_check_path()).unwrap();
        assert_eq!(after, foreign, "a foreign stamp must be left byte-intact");
    }

    #[test]
    fn an_unreadable_stamp_heals_as_first_run_without_probing() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        std::fs::create_dir_all(layout.state_dir()).unwrap();
        std::fs::write(layout.version_check_path(), b"{ not json ").unwrap();
        assert_eq!(version_nag(&fs, &layout, 42, &NeverProbe), None);
        assert_eq!(stamped_at(&fs, &layout), 42, "the rewrite heals the doc");
    }

    #[test]
    fn location_parse_accepts_the_github_shape_only() {
        assert_eq!(
            latest_tag_from_location("https://github.com/topos-sh/topos/releases/tag/v0.2.0"),
            Some("v0.2.0".to_owned())
        );
        // Query/fragment junk is stripped; a pathless or nested tail is refused.
        assert_eq!(
            latest_tag_from_location("https://x/releases/tag/v1.2.3?foo=1#frag"),
            Some("v1.2.3".to_owned())
        );
        for bad in [
            "https://github.com/topos-sh/topos/releases",
            "https://github.com/topos-sh/topos/releases/tag/",
            "https://github.com/topos-sh/topos/releases/tag/v1/extra",
            "",
        ] {
            assert_eq!(latest_tag_from_location(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn env_gate_disables_on_any_nonempty_value() {
        use std::ffi::OsStr;
        // Unset / empty on both → the check may run.
        assert!(env_gate(None, None));
        assert!(env_gate(Some(OsStr::new("")), Some(OsStr::new(""))));
        // The documented opt-out (`=1`) — and any other non-empty value — disables.
        assert!(!env_gate(Some(OsStr::new("1")), None));
        assert!(!env_gate(Some(OsStr::new("true")), None));
        // A mirror override disables too (it cannot answer `/latest`).
        assert!(!env_gate(None, Some(OsStr::new("https://mirror.example"))));
    }

    #[test]
    fn the_nag_line_is_one_line() {
        assert!(!nag_line("9.9.9").contains('\n'));
    }
}
