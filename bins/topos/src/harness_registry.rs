//! `state/harness_registry.json` — the HARNESS TABLE's own auto-update clock, and the refresh that
//! rides it.
//!
//! Which directories an agent reads, and which file it keeps its MCP servers in, are facts about
//! other people's software: they change when a vendor ships, not when topos does. The table is data
//! for exactly that reason (`topos_harness::registry::format`), and this module is the lane that
//! keeps a machine's copy current between releases — download the published table, run the fences,
//! write it beside the sidecar, and let the NEXT process read it.
//!
//! It is the forge lane's discipline, for the same reasons:
//!
//! - **The forge cadence, reused** ([`CHECK_INTERVAL_MS`] plus a spread of up to
//!   [`CHECK_JITTER_MS`]) — hardcoded, no flag and no manifest key. One small file per machine per
//!   six hours, and the spread is what stops a fleet installed together arriving in waves.
//! - **The clock advances on failure exactly as on success.** A round that could not reach the
//!   host has still had its turn; otherwise every session start would retry, which is the traffic
//!   the interval exists to prevent.
//! - **Fail open, and stay quiet.** Nothing here can fail a sweep: an unreadable stamp means check
//!   now, an unreachable host means try in six hours, and a refused file means the machine keeps
//!   the table it already had. The person hears about it only if the file they are ASKED to read
//!   is broken, which is the loader's job, not this one's.
//!
//! **No hot reload.** A CLI process is one command long, so a table that lands during a sweep is
//! read by the next command — there is no in-flight table to swap, and no window where half the
//! run used one set of directories and half used another.
//!
//! Both env knobs mirror the client's existing conventions: [`REGISTRY_URL_ENV`] re-points the
//! download (a self-hosted plane serves the same path), and [`NO_REFRESH_ENV`] switches the lane
//! off entirely the way `TOPOS_NO_UPDATE_CHECK` switches off the passive version check.

use serde::{Deserialize, Serialize};
use topos_harness::registry;
use topos_types::PERSISTED_SCHEMA_VERSION;

use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// How long the lane waits between checks — the forge lane's interval, deliberately shared: both
/// ask one host about one small document, and neither is worth a request per session.
pub(crate) const CHECK_INTERVAL_MS: i64 = crate::forge_check::CHECK_INTERVAL_MS;

/// The widest spread added to [`CHECK_INTERVAL_MS`] when scheduling the next check.
pub(crate) const CHECK_JITTER_MS: i64 = crate::forge_check::CHECK_JITTER_MS;

/// Where the published table lives. A self-hosted plane serves the same path, which is why the
/// override below exists at all.
pub(crate) const DEFAULT_REGISTRY_URL: &str = "https://topos.sh/harness-registry.toml";

/// Re-point the download (a self-hosted plane, a mirror, a `file://`-shaped test double).
pub(crate) const REGISTRY_URL_ENV: &str = "TOPOS_HARNESS_REGISTRY_URL";

/// The opt-out (`TOPOS_NO_HARNESS_REGISTRY_UPDATE=1`; any non-empty value disables the lane) — the
/// same shape as `TOPOS_NO_UPDATE_CHECK`.
pub(crate) const NO_REFRESH_ENV: &str = "TOPOS_NO_HARNESS_REGISTRY_UPDATE";

/// The transport this lane needs: one GET of a small text document. Every failure is a sentence,
/// never a typed error — nothing downstream branches on WHY, it only records it.
pub(crate) trait RegistryFetch {
    /// Fetch `url` as text, or say what went wrong.
    ///
    /// # Errors
    /// The transport's own words: unreachable, a non-2xx status, a body past the ceiling, or bytes
    /// that are not UTF-8.
    fn get(&self, url: &str) -> Result<String, String>;
}

/// The download URL for this run: [`REGISTRY_URL_ENV`], else [`DEFAULT_REGISTRY_URL`].
pub(crate) fn registry_url() -> String {
    std::env::var(REGISTRY_URL_ENV)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY_URL.to_owned())
}

/// Whether the environment allows this lane to run at all ([`NO_REFRESH_ENV`] unset or empty).
pub(crate) fn refresh_env_allows() -> bool {
    std::env::var_os(NO_REFRESH_ENV).is_none_or(|v| v.is_empty())
}

/// `state/harness_registry.json` — when the lane last had its turn, and how it went.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RegistryStamp {
    #[serde(default)]
    schema_version: u32,
    /// Epoch millis of the last check ATTEMPT, whatever came of it.
    #[serde(default)]
    checked_at_ms: i64,
    /// Epoch millis of the next scheduled check.
    #[serde(default)]
    next_check_at_ms: i64,
    /// The version of the downloaded table on disk, as of the last round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached_version: Option<String>,
    /// What the last round concluded — the same sentence [`Refresh`] carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_result: Option<String>,
}

/// What one round of the lane concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refresh {
    /// Not due yet, or switched off — nothing was dialed.
    Waited,
    /// Dialed, and the machine already holds what the host is serving (or a newer one).
    Unchanged,
    /// Dialed, and a newer table landed. The NEXT process reads it.
    Updated(String),
    /// Dialed, and something was wrong with the answer. The clock advanced anyway.
    Refused(String),
}

/// Run the lane if its clock says so. Never fails: the worst outcome is [`Refresh::Refused`] with
/// the machine keeping exactly the table it already had.
///
/// `jitter_ms` is the caller's spread (the injected id source's, so a test schedules an exact
/// instant). The stamp is written BEFORE the fetch, so a round interrupted by anything at all has
/// still spent its turn.
pub(crate) fn refresh_if_due(
    fs: &dyn FsOps,
    layout: &Layout,
    now_ms: i64,
    jitter_ms: i64,
    fetch: &dyn RegistryFetch,
) -> Refresh {
    if !refresh_env_allows() {
        return Refresh::Waited;
    }
    let stamp = match read_stamp(fs, layout) {
        // A later client sharing this home owns the document — do not overwrite it, and do not
        // dial on its behalf.
        StampRead::Foreign => return Refresh::Waited,
        StampRead::Absent => RegistryStamp::default(),
        StampRead::At(stamp) => stamp,
    };
    if !due(&stamp, now_ms) {
        return Refresh::Waited;
    }
    let next = next_due(now_ms, jitter_ms);
    write_stamp(fs, layout, now_ms, next, stamp.cached_version.clone(), None);

    let outcome = run(fs, layout, fetch);
    let cached_version = match &outcome {
        Refresh::Updated(v) => Some(v.clone()),
        _ => stamp.cached_version.clone(),
    };
    write_stamp(
        fs,
        layout,
        now_ms,
        next,
        cached_version,
        Some(result_word(&outcome)),
    );
    outcome
}

/// One round, past the clock: fetch, fence, compare, write.
fn run(fs: &dyn FsOps, layout: &Layout, fetch: &dyn RegistryFetch) -> Refresh {
    let url = registry_url();
    let text = match fetch.get(&url) {
        Ok(text) => text,
        Err(e) => return Refresh::Refused(e),
    };
    let parsed = match registry::parse_registry(&text, registry::Origin::Downloaded) {
        Ok(parsed) => parsed,
        Err(e) => return Refresh::Refused(e.message().to_owned()),
    };
    let version = parsed.version();

    let path = registry::cache_path(layout.home());
    let on_disk = fs
        .read_opt(&path)
        .ok()
        .flatten()
        .and_then(|bytes| String::from_utf8(bytes).ok());
    if let Some(current) = &on_disk
        && let Ok(current_parsed) = registry::parse_registry(current, registry::Origin::Downloaded)
    {
        // The anti-rollback rule, non-cryptographic and deliberate: a version must go FORWARD, and
        // a version that stayed put must not have changed its bytes underneath us. Neither is an
        // attack defence — the delivery is plain TLS — but a publisher who edits without bumping
        // has made two machines disagree while claiming to agree, and that is worth refusing.
        match version.cmp(current_parsed.version()) {
            std::cmp::Ordering::Less => {
                return Refresh::Refused(format!(
                    "{url} serves harness registry {version}, older than the {} already \
                     downloaded — kept the newer one",
                    current_parsed.version()
                ));
            }
            std::cmp::Ordering::Equal if current.as_str() != text.as_str() => {
                return Refresh::Refused(format!(
                    "{url} serves harness registry {version} with different bytes from the copy \
                     already downloaded — a changed table needs a new version"
                ));
            }
            std::cmp::Ordering::Equal => return Refresh::Unchanged,
            std::cmp::Ordering::Greater => {}
        }
    } else if version <= registry::bundled_version() {
        // Nothing downloaded yet (or what is there is junk) and the table built into this topos is
        // already at least as new — there is nothing to keep.
        return Refresh::Unchanged;
    }

    if let Some(dir) = path.parent()
        && fs.create_dir_all(dir).is_err()
    {
        return Refresh::Refused(format!("could not create {}", dir.display()));
    }
    match crate::atomic::atomic_write(fs, &path, text.as_bytes()) {
        Ok(()) => Refresh::Updated(version.to_string()),
        Err(e) => Refresh::Refused(format!(
            "could not write {}: {}",
            path.display(),
            e.detail()
        )),
    }
}

/// The sentence a round records in its stamp.
fn result_word(outcome: &Refresh) -> String {
    match outcome {
        Refresh::Waited => "waited".to_owned(),
        Refresh::Unchanged => "current".to_owned(),
        Refresh::Updated(v) => format!("updated to {v}"),
        Refresh::Refused(why) => format!("refused: {why}"),
    }
}

/// Whether a scheduled round may dial now. A turn never taken is due immediately; a due time
/// absurdly far ahead is treated as due rather than obeyed, so a backwards clock step cannot
/// suspend the lane until wall time catches up.
fn due(stamp: &RegistryStamp, now_ms: i64) -> bool {
    let ceiling = now_ms.saturating_add(CHECK_INTERVAL_MS + CHECK_JITTER_MS);
    stamp.next_check_at_ms == 0
        || now_ms >= stamp.next_check_at_ms
        || stamp.next_check_at_ms > ceiling
}

/// When the lane next falls due: one interval from now, pushed out by an independent spread.
fn next_due(now_ms: i64, jitter_ms: i64) -> i64 {
    now_ms.saturating_add(CHECK_INTERVAL_MS.saturating_add(jitter_ms.clamp(0, CHECK_JITTER_MS)))
}

/// The stamp read, three-way: absent/unreadable (fail open — check now), foreign (a newer client's
/// document), or the document.
enum StampRead {
    Absent,
    Foreign,
    At(RegistryStamp),
}

fn read_stamp(fs: &dyn FsOps, layout: &Layout) -> StampRead {
    let Ok(Some(bytes)) = fs.read_opt(&layout.harness_registry_path()) else {
        return StampRead::Absent;
    };
    let Ok(stamp) = serde_json::from_slice::<RegistryStamp>(&bytes) else {
        return StampRead::Absent;
    };
    if stamp.schema_version > PERSISTED_SCHEMA_VERSION {
        StampRead::Foreign
    } else {
        StampRead::At(stamp)
    }
}

/// Record the round (best-effort: a stamp that cannot be written costs one extra check later,
/// which is the direction a failure here should fall).
fn write_stamp(
    fs: &dyn FsOps,
    layout: &Layout,
    now_ms: i64,
    next_check_at_ms: i64,
    cached_version: Option<String>,
    last_result: Option<String>,
) {
    let stamp = RegistryStamp {
        schema_version: PERSISTED_SCHEMA_VERSION,
        checked_at_ms: now_ms,
        next_check_at_ms,
        cached_version,
        last_result,
    };
    let _ = fs.create_dir_all(&layout.state_dir());
    let _ = crate::doc::write_doc(fs, &layout.harness_registry_path(), &stamp);
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
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
            let dir = std::env::temp_dir().join(format!("topos-hreg-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn layout(&self) -> Layout {
            Layout::new(&self.0)
        }
        fn cached(&self) -> PathBuf {
            registry::cache_path(&self.0)
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A canned host, counting its calls.
    struct FakeHost {
        answer: Result<String, String>,
        calls: RefCell<u32>,
    }
    impl FakeHost {
        fn serving(body: &str) -> Self {
            Self {
                answer: Ok(body.to_owned()),
                calls: RefCell::new(0),
            }
        }
        fn failing() -> Self {
            Self {
                answer: Err("could not reach topos.sh".to_owned()),
                calls: RefCell::new(0),
            }
        }
    }
    impl RegistryFetch for FakeHost {
        fn get(&self, _url: &str) -> Result<String, String> {
            *self.calls.borrow_mut() += 1;
            self.answer.clone()
        }
    }

    /// A host that must never be dialed.
    struct NeverHost;
    impl RegistryFetch for NeverHost {
        fn get(&self, _url: &str) -> Result<String, String> {
            panic!("the lane must not dial on this path")
        }
    }

    /// A minimal valid table naming one row this build knows, at `version`.
    fn table(version: &str, dir: &str) -> String {
        format!(
            "schema_version = 1\nversion = \"{version}\"\nmin_engine_version = 1\n\n\
             [[harness]]\nslug = \"cursor\"\ndisplay_name = \"Cursor\"\nuser_dirs = \
             [\"home/{dir}\"]\nproject_dir = \".agents/skills\"\ndetect_dirs = [\"home/.cursor\"]\n"
        )
    }

    /// A version strictly newer than whatever this build was compiled with.
    fn newer() -> String {
        format!("{}.1", registry::bundled_version())
    }

    const NOW: i64 = 1_700_000_000_000;

    #[test]
    fn a_first_run_dials_and_a_newer_table_lands_for_the_next_process() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        let host = FakeHost::serving(&table(&newer(), ".cursor/skills"));
        assert_eq!(
            refresh_if_due(&fs, &layout, NOW, 0, &host),
            Refresh::Updated(newer())
        );
        assert_eq!(*host.calls.borrow(), 1);
        assert_eq!(
            std::fs::read_to_string(home.cached()).unwrap(),
            table(&newer(), ".cursor/skills"),
            "the bytes on disk are the bytes served"
        );
    }

    #[test]
    fn inside_the_interval_nothing_is_dialed_and_the_clock_is_kept() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        let host = FakeHost::serving(&table(&newer(), ".cursor/skills"));
        assert!(matches!(
            refresh_if_due(&fs, &layout, NOW, 0, &host),
            Refresh::Updated(_)
        ));
        assert_eq!(
            refresh_if_due(&fs, &layout, NOW + CHECK_INTERVAL_MS - 1, 0, &NeverHost),
            Refresh::Waited,
            "one millisecond short"
        );
        // The interval elapses and the lane takes its turn again.
        let host = FakeHost::serving(&table(&newer(), ".cursor/skills"));
        assert_eq!(
            refresh_if_due(&fs, &layout, NOW + CHECK_INTERVAL_MS, 0, &host),
            Refresh::Unchanged
        );
        assert_eq!(*host.calls.borrow(), 1);
    }

    #[test]
    fn the_spread_only_ever_delays_and_stays_inside_one_hour() {
        assert_eq!(next_due(NOW, 0), NOW + CHECK_INTERVAL_MS);
        assert_eq!(
            next_due(NOW, CHECK_JITTER_MS),
            NOW + CHECK_INTERVAL_MS + CHECK_JITTER_MS
        );
        assert_eq!(
            next_due(NOW, -5_000),
            NOW + CHECK_INTERVAL_MS,
            "never earlier"
        );
        assert_eq!(
            next_due(NOW, 10 * CHECK_JITTER_MS),
            NOW + CHECK_INTERVAL_MS + CHECK_JITTER_MS,
            "and never further out than the spread allows"
        );
    }

    #[test]
    fn a_failed_round_advances_the_clock_exactly_as_a_successful_one() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        let host = FakeHost::failing();
        assert_eq!(
            refresh_if_due(&fs, &layout, NOW, 0, &host),
            Refresh::Refused("could not reach topos.sh".to_owned())
        );
        assert_eq!(*host.calls.borrow(), 1);
        // …and the very next command does not re-dial.
        assert_eq!(
            refresh_if_due(&fs, &layout, NOW + 1, 0, &NeverHost),
            Refresh::Waited
        );
        assert!(!home.cached().exists(), "nothing was written");
    }

    #[test]
    fn an_older_or_byte_changed_same_version_table_is_refused_and_the_copy_on_disk_stands() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        let good = table(
            &format!("{}.5", registry::bundled_version()),
            ".cursor/skills",
        );
        assert!(matches!(
            refresh_if_due(&fs, &layout, NOW, 0, &FakeHost::serving(&good)),
            Refresh::Updated(_)
        ));

        // A LOWER version: refused, and the newer copy stands byte-for-byte.
        let older = table(
            &format!("{}.1", registry::bundled_version()),
            ".other/skills",
        );
        let out = refresh_if_due(
            &fs,
            &layout,
            NOW + CHECK_INTERVAL_MS,
            0,
            &FakeHost::serving(&older),
        );
        assert!(
            matches!(&out, Refresh::Refused(w) if w.contains("older")),
            "{out:?}"
        );
        assert_eq!(std::fs::read_to_string(home.cached()).unwrap(), good);

        // The SAME version with different bytes: an error, never a silent accept.
        let mutated = table(
            &format!("{}.5", registry::bundled_version()),
            ".moved/skills",
        );
        let out = refresh_if_due(
            &fs,
            &layout,
            NOW + 2 * CHECK_INTERVAL_MS,
            0,
            &FakeHost::serving(&mutated),
        );
        assert!(
            matches!(&out, Refresh::Refused(w) if w.contains("different bytes")),
            "{out:?}"
        );
        assert_eq!(std::fs::read_to_string(home.cached()).unwrap(), good);

        // The same version with the same bytes is simply nothing to do.
        assert_eq!(
            refresh_if_due(
                &fs,
                &layout,
                NOW + 3 * CHECK_INTERVAL_MS,
                0,
                &FakeHost::serving(&good)
            ),
            Refresh::Unchanged
        );
    }

    #[test]
    fn a_table_this_build_already_beats_is_not_kept() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        let out = refresh_if_due(
            &fs,
            &layout,
            NOW,
            0,
            &FakeHost::serving(&table(
                registry::bundled_version().as_str(),
                ".cursor/skills",
            )),
        );
        assert_eq!(out, Refresh::Unchanged);
        assert!(!home.cached().exists(), "nothing was written");
    }

    #[test]
    fn a_table_the_fences_refuse_never_reaches_disk() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        let cases = [
            // A grammar this build does not read.
            table(&newer(), ".cursor/skills").replace("schema_version = 1", "schema_version = 2"),
            // An engine this build does not provide.
            table(&newer(), ".cursor/skills")
                .replace("min_engine_version = 1", "min_engine_version = 9"),
            // A dir that escapes its root.
            table(&newer(), "../../etc"),
            // Not a table at all.
            "<!doctype html>".to_owned(),
        ];
        for (i, body) in cases.into_iter().enumerate() {
            let at = NOW + i as i64 * CHECK_INTERVAL_MS;
            let out = refresh_if_due(&fs, &layout, at, 0, &FakeHost::serving(&body));
            assert!(matches!(out, Refresh::Refused(_)), "case {i}: {out:?}");
            assert!(!home.cached().exists(), "case {i}: nothing was written");
        }
    }

    /// A fault at ANY step of the write leaves the copy on disk byte-for-byte PRE or POST — never a
    /// torn table, which the loader would refuse and warn about on every command until someone
    /// deleted it by hand.
    #[test]
    fn a_fault_mid_write_leaves_the_copy_on_disk_pre_or_post_never_torn() {
        let pre = table(
            &format!("{}.1", registry::bundled_version()),
            ".cursor/skills",
        );
        let post = table(
            &format!("{}.2", registry::bundled_version()),
            ".moved/skills",
        );
        for fail_at in 1..=8 {
            let home = TempHome::new();
            let layout = home.layout();
            std::fs::create_dir_all(home.cached().parent().unwrap()).unwrap();
            std::fs::write(home.cached(), &pre).unwrap();

            let ff = crate::fs_seam::FaultFs::new(fail_at);
            let _ = refresh_if_due(&ff, &layout, NOW, 0, &FakeHost::serving(&post));

            let now = std::fs::read_to_string(home.cached()).unwrap();
            assert!(
                now == pre || now == post,
                "fail_at={fail_at}: the table on disk is neither the pre nor the post bytes"
            );
        }
    }

    #[test]
    fn an_unreadable_stamp_fails_open_and_a_newer_clients_stamp_is_left_alone() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        std::fs::create_dir_all(layout.state_dir()).unwrap();

        std::fs::write(layout.harness_registry_path(), b"{ not json ").unwrap();
        let host = FakeHost::serving(&table(&newer(), ".cursor/skills"));
        assert!(
            matches!(
                refresh_if_due(&fs, &layout, NOW, 0, &host),
                Refresh::Updated(_)
            ),
            "garbage means check now"
        );

        let foreign = format!(
            "{{\"schema_version\": {}, \"next_check_at_ms\": 1}}",
            PERSISTED_SCHEMA_VERSION + 1
        );
        std::fs::write(layout.harness_registry_path(), &foreign).unwrap();
        assert_eq!(
            refresh_if_due(&fs, &layout, NOW + CHECK_INTERVAL_MS, 0, &NeverHost),
            Refresh::Waited
        );
        assert_eq!(
            std::fs::read_to_string(layout.harness_registry_path()).unwrap(),
            foreign,
            "a foreign stamp is left byte-intact"
        );
    }

    #[test]
    fn a_far_future_due_time_never_suspends_the_lane() {
        // A backwards clock step (or a corrupt value) must not switch the lane off until wall time
        // catches up — the same fail-open rule the forge clock keeps.
        let stamp = RegistryStamp {
            next_check_at_ms: NOW + 400 * 24 * 60 * 60 * 1000,
            ..RegistryStamp::default()
        };
        assert!(due(&stamp, NOW));
        // …but a due time this code really wrote is obeyed.
        let honored = RegistryStamp {
            next_check_at_ms: next_due(NOW, CHECK_JITTER_MS),
            ..RegistryStamp::default()
        };
        assert!(!due(&honored, NOW));
    }

    #[test]
    fn the_round_is_recorded_where_status_could_read_it() {
        let home = TempHome::new();
        let (fs, layout) = (RealFs, home.layout());
        let host = FakeHost::serving(&table(&newer(), ".cursor/skills"));
        assert!(matches!(
            refresh_if_due(&fs, &layout, NOW, 0, &host),
            Refresh::Updated(_)
        ));
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(layout.harness_registry_path()).unwrap())
                .unwrap();
        assert_eq!(doc["checked_at_ms"].as_i64(), Some(NOW));
        assert_eq!(
            doc["next_check_at_ms"].as_i64(),
            Some(NOW + CHECK_INTERVAL_MS)
        );
        assert_eq!(doc["cached_version"].as_str(), Some(newer().as_str()));
        assert_eq!(
            doc["last_result"].as_str(),
            Some(format!("updated to {}", newer()).as_str())
        );
    }

    #[test]
    fn the_url_and_the_opt_out_read_the_documented_names() {
        // Read-only assertions: the suite never sets process env (another test would see it).
        assert_eq!(REGISTRY_URL_ENV, "TOPOS_HARNESS_REGISTRY_URL");
        assert_eq!(NO_REFRESH_ENV, "TOPOS_NO_HARNESS_REGISTRY_UPDATE");
        assert_eq!(
            DEFAULT_REGISTRY_URL,
            "https://topos.sh/harness-registry.toml"
        );
        if std::env::var_os(REGISTRY_URL_ENV).is_none() {
            assert_eq!(registry_url(), DEFAULT_REGISTRY_URL);
        }
        if std::env::var_os(NO_REFRESH_ENV).is_none() {
            assert!(refresh_env_allows());
        }
    }
}
