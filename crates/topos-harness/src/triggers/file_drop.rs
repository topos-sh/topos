//! The one-topos-owned-file trigger base — a harness that loads trigger artifacts additively
//! from a directory gets ONE canonical topos-owned file (a hook definition, a plugin, a script)
//! at a harness-defined path.
//!
//! Install writes the canonical bytes (through [`ConfigStore::replace`], which creates parent
//! dirs) IF the path is absent or the existing file is marker-confirmed OURS — an ours-but-stale
//! file byte-migrates to canonical, so a fix reaches installs that predate it. A foreign file at
//! the path is adopt-or-leave (`AlreadyPresentUnmanaged`, ZERO writes). Remove unlinks ONLY a
//! marker-confirmed ours (a direct `std::fs::remove_file`, best-effort — the same discipline the
//! OpenClaw legacy-plugin scrub uses); `present` = the marker-confirmed file exists right now.
//! Ownership keys on the per-instance marker needle ALONE — a schema-version-agnostic substring
//! (a marker-id prefix, or the in-command sentinel where the format has no comment slot).
//!
//! Every canonical file leads with an ownership block naming the marker id, the fact that topos
//! manages it ("hand edits are overwritten"), and the removal command — a comment header where
//! the format has comments, a `"_comment"` field where it does not (JSON), and the in-command
//! sentinel where the harness's own schema leaves no safe slot for either (goose's `hooks.json`,
//! whose exact shape is source-verified and not ours to extend).

use std::path::PathBuf;

use topos_types::{CurrencyKind, TriggerState};

use crate::ConfigStore;

use super::{TriggerAdapter, TriggerOutcome, outcome};

/// One instance's parameterization of the shared file-drop machinery.
pub(crate) struct FileDropSpec {
    /// The registry slug.
    pub(crate) slug: &'static str,
    /// The structured marker identity reported in [`TriggerOutcome::marker_id`].
    pub(crate) marker_id: &'static str,
    /// The ownership needle — a file whose bytes contain it is OURS (version-agnostic: a marker
    /// id minus its schema number, or the in-command sentinel). Never matched against a foreign
    /// file's path — only its bytes.
    pub(crate) marker_needle: &'static str,
    /// What fires when this instance's trigger is provably live.
    pub(crate) live_kind: CurrencyKind,
    /// The receipt note riding a successful placement (the evidence level), if one is owed.
    pub(crate) note: Option<&'static str>,
}

/// A [`TriggerAdapter`] over one [`FileDropSpec`] instance + the resolved file path + the
/// canonical bytes + the [`ConfigStore`] port. The base reports `Active` on placement; an
/// instance whose harness gates the artifact behind its own consent (goose) wraps this and
/// demotes the report where the evidence is missing — the base itself has NO API that could
/// write another program's consent state.
pub(crate) struct FileDrop<'a> {
    spec: &'static FileDropSpec,
    path: PathBuf,
    canonical: Vec<u8>,
    cfg: &'a dyn ConfigStore,
}

impl<'a> FileDrop<'a> {
    /// Construct over the resolved artifact path + the instance's canonical content. Production
    /// resolves the path through the instance module's `resolve_*` helpers; tests pass injected
    /// paths so no real home is ever touched.
    pub(crate) fn new(
        spec: &'static FileDropSpec,
        path: PathBuf,
        canonical: &str,
        cfg: &'a dyn ConfigStore,
    ) -> Self {
        debug_assert!(
            canonical.contains(spec.marker_needle),
            "canonical bytes must carry the ownership needle"
        );
        Self {
            spec,
            path,
            canonical: canonical.as_bytes().to_vec(),
            cfg,
        }
    }

    fn is_ours(&self, bytes: &[u8]) -> bool {
        String::from_utf8_lossy(bytes).contains(self.spec.marker_needle)
    }

    fn out(
        &self,
        state: TriggerState,
        touched: bool,
        note: Option<&'static str>,
    ) -> TriggerOutcome {
        outcome(
            self.spec.slug,
            self.spec.live_kind,
            state,
            touched.then(|| self.path.to_string_lossy().into_owned()),
            self.spec.marker_id,
            note,
        )
    }

    fn write_canonical(&self) -> TriggerOutcome {
        match self.cfg.replace(&self.path, &self.canonical) {
            Ok(()) => self.out(TriggerState::Active, true, self.spec.note),
            Err(_) => self.out(TriggerState::Degraded, false, None),
        }
    }
}

impl TriggerAdapter for FileDrop<'_> {
    fn slug(&self) -> &'static str {
        self.spec.slug
    }

    fn install(&self) -> TriggerOutcome {
        match self.cfg.read(&self.path) {
            // Unreadable (e.g. a permission error) — degrade honestly, never blind-overwrite.
            Err(_) => self.out(TriggerState::Degraded, false, None),
            Ok(None) => self.write_canonical(),
            Ok(Some(bytes)) if self.is_ours(&bytes) => {
                if bytes == self.canonical {
                    self.out(TriggerState::Active, false, self.spec.note) // canonical → true no-op
                } else {
                    self.write_canonical() // ours-but-stale → byte-migrate in place
                }
            }
            // A foreign file on our path — adopt-or-leave, never overwritten.
            Ok(Some(_)) => self.out(TriggerState::AlreadyPresentUnmanaged, false, None),
        }
    }

    fn remove(&self) -> TriggerOutcome {
        match self.cfg.read(&self.path) {
            Err(_) => self.out(TriggerState::Degraded, false, None),
            Ok(None) => self.out(TriggerState::Inactive, false, None), // already clean
            Ok(Some(bytes)) if !self.is_ours(&bytes) => {
                self.out(TriggerState::AlreadyPresentUnmanaged, false, None) // never unlink foreign
            }
            Ok(Some(_)) => match std::fs::remove_file(&self.path) {
                Ok(()) => self.out(TriggerState::Inactive, true, None),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.out(TriggerState::Inactive, false, None)
                }
                // A failed unlink leaves the artifact live — disclosed, never claimed clean.
                Err(_) => self.out(TriggerState::Degraded, false, None),
            },
        }
    }

    fn present(&self) -> bool {
        matches!(self.cfg.read(&self.path), Ok(Some(bytes)) if self.is_ours(&bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{DiskConfig, ErrConfig, MemConfig, TempHome};
    use super::*;

    /// A dedicated test spec, so the base tests stay independent of any instance's content.
    static TSPEC: FileDropSpec = FileDropSpec {
        slug: "cline", // any supported slug; the base never interprets it
        marker_id: "topos:test:currency:1",
        marker_needle: "topos:test:currency",
        live_kind: CurrencyKind::SessionStart,
        note: Some("vendor docs, unverified"),
    };

    const CANON: &str = "# topos:test:currency:1 — Managed by topos\nrun the sweep\n";
    const PATH: &str = "/h/hooks/t.sh";

    fn drop_at<'a>(cfg: &'a dyn ConfigStore, path: &str) -> FileDrop<'a> {
        FileDrop::new(&TSPEC, PathBuf::from(path), CANON, cfg)
    }

    #[test]
    fn install_places_migrates_and_noops_by_the_marker_alone() {
        // Absent → placed byte-exact, Active + the spec note.
        let cfg = MemConfig::default();
        let a = drop_at(&cfg, PATH);
        let report = a.install();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.kind, CurrencyKind::SessionStart);
        assert_eq!(report.note.as_deref(), Some("vendor docs, unverified"));
        assert_eq!(report.touched_path.as_deref(), Some(PATH));
        assert_eq!(cfg.text(PATH).as_deref(), Some(CANON));

        // Canonical already → a true no-op.
        let again = a.install();
        assert!(again.touched_path.is_none());
        assert_eq!(cfg.writes(), 1, "rerun writes nothing");

        // Ours-but-stale (needle present, bytes differ) → byte-migrated to canonical.
        cfg.set(PATH, "# topos:test:currency:0 old schema\nold body\n");
        let migrated = a.install();
        assert_eq!(migrated.state, TriggerState::Active);
        assert_eq!(cfg.text(PATH).as_deref(), Some(CANON), "migrated in place");
        assert_eq!(cfg.writes(), 2);
    }

    #[test]
    fn a_foreign_file_is_adopt_or_leave_for_install_and_remove() {
        let cfg = MemConfig::with_file(PATH, "#!/bin/sh\nsomebody else's hook\n");
        let a = drop_at(&cfg, PATH);
        let report = a.install();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0, "a foreign file is never overwritten");
        let report = a.remove();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(
            cfg.text(PATH).as_deref(),
            Some("#!/bin/sh\nsomebody else's hook\n"),
            "a foreign file is never unlinked"
        );
        assert!(!a.present(), "a foreign file is never claimed as ours");
    }

    #[test]
    fn remove_unlinks_only_ours_then_is_idempotent() {
        let home = TempHome::new();
        let cfg = DiskConfig;
        let path = home.0.join("hooks").join("t.sh");
        let a = FileDrop::new(&TSPEC, path.clone(), CANON, &cfg);
        a.install();
        assert!(path.is_file());
        assert!(a.present());

        let report = a.remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.kind, CurrencyKind::ExplicitPullOnly);
        assert!(report.touched_path.is_some(), "the unlink is disclosed");
        assert!(!path.exists());
        assert!(!a.present());

        // Idempotent: a second remove is a clean no-op.
        let again = a.remove();
        assert_eq!(again.state, TriggerState::Inactive);
        assert!(again.touched_path.is_none());
    }

    #[test]
    fn an_unreadable_store_degrades_with_zero_writes() {
        let cfg = ErrConfig;
        let a = drop_at(&cfg, PATH);
        assert_eq!(a.install().state, TriggerState::Degraded);
        assert_eq!(a.remove().state, TriggerState::Degraded);
        assert!(!a.present(), "presence is never claimed on faith");
    }
}
