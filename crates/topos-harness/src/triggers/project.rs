//! Project-scoped triggers — the same engines ([`super::cc_hooks`], [`super::file_drop`])
//! re-rooted at ONE checkout, behind the containment rail every project target passes.
//!
//! Four harnesses read a hook from inside a project: claude-code
//! (`<root>/.claude/settings.local.json`), cursor (`<root>/.cursor/hooks.json`), codex
//! (`<root>/.codex/hooks.json`) and opencode (`<root>/.opencode/plugin/topos.ts`). Each is the
//! user-level spec (or its one-file-name twin) over the checkout's own dir, so the entry, the
//! marker and the sweep line are byte-identical to the `-g` ones.
//!
//! **The rail.** A repo can commit `.cursor` as a symlink as easily as `.claude/skills`, and a
//! hook written through it would land wherever that symlink points. So every project adapter
//! here is wrapped in [`Contained`], which proves BOTH the hook file's parent dir and the file
//! itself resolve inside the checkout ([`within_root`]: no symlink component below the root, and
//! the deepest existing ancestor canonicalizes under the root) before any read, install or
//! remove. A target that fails the proof is refused with a degraded report naming the reason —
//! zero writes, and never a claim of presence.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, TriggerState};

use super::{TriggerAdapter, TriggerArtifact, TriggerReport};

/// The receipt reason a refused project target carries.
pub(crate) const ESCAPE_REASON: &str = "its path does not resolve inside this checkout (a symlink, \
                                        or a path that climbs out of it), so topos left it alone";

/// Whether `candidate` (a path lexically under `root`) resolves INSIDE `root`: every existing
/// component below the root is a plain entry (no symlink), and the deepest existing ancestor
/// canonicalizes under the canonicalized root. A root that does not exist proves nothing.
pub(crate) fn within_root(root: &Path, candidate: &Path) -> bool {
    let Ok(root_real) = root.canonicalize() else {
        return false;
    };
    let Ok(rel) = candidate.strip_prefix(root) else {
        return false;
    };
    // (a) Reject symlink components: every existing prefix below the root must be a plain entry.
    let mut prefix = root.to_path_buf();
    for comp in rel.components() {
        prefix = prefix.join(comp);
        match std::fs::symlink_metadata(&prefix) {
            Ok(meta) if meta.file_type().is_symlink() => return false,
            Ok(_) => {}
            Err(_) => break, // the rest does not exist yet — nothing left to lstat
        }
    }
    // (b) The canonicalized containment proof over the deepest existing ancestor.
    let mut probe = candidate.to_path_buf();
    let real_prefix = loop {
        match probe.canonicalize() {
            Ok(r) => break r,
            Err(_) => match probe.parent() {
                Some(up) => probe = up.to_path_buf(),
                None => return false,
            },
        }
    };
    real_prefix.starts_with(&root_real)
}

/// A project trigger behind the containment rail: the inner engine runs only once the hook
/// file's parent dir and the file itself are proven inside the checkout.
pub(crate) struct Contained<'a> {
    inner: Box<dyn TriggerAdapter + 'a>,
    root: PathBuf,
    /// The hook file the inner engine reads and writes — the leaf the rail proves, with its
    /// parent.
    leaf: PathBuf,
    /// The inner spec's marker, restated so a refusal reports in the same shape a degrade does.
    marker_id: &'static str,
}

impl<'a> Contained<'a> {
    pub(crate) fn new(
        inner: Box<dyn TriggerAdapter + 'a>,
        root: PathBuf,
        leaf: PathBuf,
        marker_id: &'static str,
    ) -> Self {
        Self {
            inner,
            root,
            leaf,
            marker_id,
        }
    }

    /// Whether the target passes the rail right now — probed on every call, never cached: a
    /// parent that became a symlink between two runs is met as itself.
    fn contained(&self) -> bool {
        let parent_ok = self
            .leaf
            .parent()
            .is_some_and(|parent| within_root(&self.root, parent));
        parent_ok && within_root(&self.root, &self.leaf)
    }

    fn refused(&self) -> TriggerReport {
        crate::trigger_report(
            self.inner.slug(),
            CurrencyKind::SessionStart,
            TriggerState::Degraded,
            None,
            self.marker_id,
            Some(ESCAPE_REASON),
        )
    }
}

impl TriggerAdapter for Contained<'_> {
    fn slug(&self) -> &'static str {
        self.inner.slug()
    }

    fn install(&self) -> TriggerReport {
        if self.contained() {
            self.inner.install()
        } else {
            self.refused()
        }
    }

    fn remove(&self) -> TriggerReport {
        if self.contained() {
            self.inner.remove()
        } else {
            self.refused()
        }
    }

    /// Presence is never claimed through a path the rail refuses.
    fn present(&self) -> bool {
        self.contained() && self.inner.present()
    }

    fn pending_step(&self) -> Option<&'static str> {
        self.inner.pending_step()
    }

    fn artifacts(&self) -> Vec<TriggerArtifact> {
        if self.contained() {
            self.inner.artifacts()
        } else {
            Vec::new()
        }
    }

    fn offline_probe_refusal(&self) -> Option<&'static str> {
        self.inner.offline_probe_refusal()
    }

    /// The file a person must look at — named whether or not the rail admits it, because a
    /// refused target is exactly the one a receipt has to point at.
    fn config_file(&self) -> Option<PathBuf> {
        Some(self.leaf.clone())
    }

    fn scrub_needs_live_harness(&self) -> bool {
        self.inner.scrub_needs_live_harness()
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{DiskConfig, TempHome};
    use super::super::{TriggerScope, adapter_for_slug_at, project_hook_file};
    use super::*;

    /// A `CommandRunner` whose binary is absent — no project adapter dials one, and none may.
    struct NoCli;
    impl crate::CommandRunner for NoCli {
        fn run(&self, _p: &str, _a: &[&str]) -> std::io::Result<crate::RunOutput> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "absent"))
        }
    }

    /// The four project-capable slugs, in registry-table order.
    const PROJECT_CAPABLE: [&str; 4] = ["claude-code", "codex", "cursor", "opencode"];

    /// A real checkout dir (the rail canonicalizes, so the root has to exist) — canonicalized
    /// itself so the receipts' paths compare plainly on a symlinked temp dir.
    fn checkout() -> (TempHome, PathBuf) {
        let home = TempHome::new();
        let root = home.0.canonicalize().unwrap();
        (home, root)
    }

    fn project<'a>(slug: &str, root: &Path, cfg: &'a DiskConfig) -> Box<dyn TriggerAdapter + 'a> {
        adapter_for_slug_at(
            slug,
            &TriggerScope::Project(root.to_path_buf()),
            Path::new("/no-such-home"),
            cfg,
            &NoCli,
        )
        .unwrap_or_else(|| panic!("{slug} has a project trigger"))
    }

    #[test]
    fn claude_code_project_hook_lands_in_settings_local_json() {
        let (_home, root) = checkout();
        let cfg = DiskConfig;
        let a = project("claude-code", &root, &cfg);
        let report = a.install();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.marker_id, "topos:claude-code:currency:2");
        let file = root.join(".claude").join("settings.local.json");
        assert_eq!(report.touched_path.as_deref(), Some(file.to_str().unwrap()));
        assert_eq!(project_hook_file("claude-code", &root), Some(file.clone()));
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("topos install --quiet --hook claude-code"),
            "the same dialect-marked sweep as the user-level hook: {text}"
        );
        assert!(text.contains("# topos:currency"), "sentinel-keyed: {text}");
        assert!(
            !root.join(".claude").join("settings.json").exists(),
            "the committed settings file is never touched"
        );
        assert!(a.present());
        assert_eq!(a.artifacts(), vec![TriggerArtifact::Path(file.clone())]);
        assert_eq!(a.remove().state, TriggerState::Inactive);
        assert!(!a.present());
    }

    #[test]
    fn cursor_project_hook_seeds_version_1_and_no_trailing_comment() {
        let (_home, root) = checkout();
        let cfg = DiskConfig;
        let a = project("cursor", &root, &cfg);
        assert_eq!(a.install().state, TriggerState::Active);
        let file = root.join(".cursor").join("hooks.json");
        assert_eq!(project_hook_file("cursor", &root), Some(file.clone()));
        let text = std::fs::read_to_string(&file).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["version"], 1, "the numeric schema version is seeded");
        let command = doc["hooks"]["sessionStart"][0]["command"]
            .as_str()
            .expect("the flat entry's command");
        assert!(
            !command.contains('#'),
            "no trailing comment on the cursor command: {command}"
        );
        assert!(
            command.contains("topos install --quiet --from cursor"),
            "{command}"
        );
        assert!(a.present());
    }

    #[test]
    fn codex_project_hook_touches_no_config_toml() {
        let (_home, root) = checkout();
        let cfg = DiskConfig;
        let a = project("codex", &root, &cfg);
        let report = a.install();
        assert_eq!(
            report.state,
            TriggerState::Inactive,
            "codex never claims Active"
        );
        assert_eq!(
            report.note.as_deref(),
            Some("Codex runs it once you trust this repo")
        );
        let file = root.join(".codex").join("hooks.json");
        assert_eq!(project_hook_file("codex", &root), Some(file.clone()));
        assert!(file.is_file());
        assert!(
            !root.join(".codex").join("config.toml").exists(),
            "config.toml is never created"
        );
        // Every file under `.codex` is the one hook file.
        let entries: Vec<_> = std::fs::read_dir(root.join(".codex"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec!["hooks.json"]);
        assert!(a.present());
    }

    #[test]
    fn opencode_project_plugin_lands_under_plugin() {
        let (_home, root) = checkout();
        let cfg = DiskConfig;
        let a = project("opencode", &root, &cfg);
        assert_eq!(a.install().state, TriggerState::Active);
        let file = root.join(".opencode").join("plugin").join("topos.ts");
        assert_eq!(project_hook_file("opencode", &root), Some(file.clone()));
        assert!(file.is_file(), "the singular `plugin/` dir");
        assert!(!root.join(".opencode").join("plugins").exists());
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .starts_with("// topos:opencode:currency:1")
        );
        assert!(a.present());
        assert_eq!(a.remove().state, TriggerState::Inactive);
        assert!(!file.exists());
    }

    /// The parent dir of the hook file (`.cursor`, `.claude`, …) is a symlink out of the checkout:
    /// install, remove and the presence probe all refuse — zero writes anywhere, the reason named.
    #[test]
    fn a_symlinked_project_hook_parent_refuses() {
        for slug in PROJECT_CAPABLE {
            let (_home, root) = checkout();
            let (_elsewhere, outside) = checkout();
            let leaf = project_hook_file(slug, &root).unwrap();
            // The nearest dir under the root on the leaf's path becomes the symlink.
            let rel = leaf.strip_prefix(&root).unwrap();
            let first = root.join(rel.components().next().unwrap());
            std::os::unix::fs::symlink(&outside, &first).unwrap();

            let cfg = DiskConfig;
            let a = project(slug, &root, &cfg);
            let report = a.install();
            assert_eq!(report.state, TriggerState::Degraded, "{slug}");
            assert_eq!(report.note.as_deref(), Some(ESCAPE_REASON), "{slug}");
            assert!(report.touched_path.is_none(), "{slug}: nothing written");
            assert!(
                std::fs::read_dir(&outside).unwrap().next().is_none(),
                "{slug}: nothing landed through the symlink"
            );
            assert!(!a.present(), "{slug}: presence is never claimed");
            assert!(a.artifacts().is_empty(), "{slug}");
            assert_eq!(a.remove().state, TriggerState::Degraded, "{slug}");
            assert_eq!(
                a.config_file(),
                Some(leaf),
                "{slug}: the receipt still names the file"
            );
        }
    }

    /// The hook file itself is a symlink to a file outside the checkout: refused the same way,
    /// and the file it points at keeps its bytes.
    #[test]
    fn a_symlinked_project_hook_leaf_refuses() {
        for slug in PROJECT_CAPABLE {
            let (_home, root) = checkout();
            let (_elsewhere, outside) = checkout();
            let leaf = project_hook_file(slug, &root).unwrap();
            std::fs::create_dir_all(leaf.parent().unwrap()).unwrap();
            let target = outside.join("theirs");
            std::fs::write(&target, b"{}\n").unwrap();
            std::os::unix::fs::symlink(&target, &leaf).unwrap();

            let cfg = DiskConfig;
            let a = project(slug, &root, &cfg);
            let report = a.install();
            assert_eq!(report.state, TriggerState::Degraded, "{slug}");
            assert_eq!(report.note.as_deref(), Some(ESCAPE_REASON), "{slug}");
            assert_eq!(
                std::fs::read(&target).unwrap(),
                b"{}\n",
                "{slug}: the file behind the symlink is untouched"
            );
            assert!(!a.present(), "{slug}");
            assert_eq!(a.remove().state, TriggerState::Degraded, "{slug}");
            assert!(leaf.symlink_metadata().unwrap().file_type().is_symlink());
        }
    }

    /// The rail, on its own: plain paths under the root pass whether or not they exist yet; a
    /// symlink component, a path outside, and a missing root all fail.
    #[test]
    fn within_root_admits_plain_paths_and_refuses_symlinks_and_escapes() {
        let (_home, root) = checkout();
        assert!(within_root(&root, &root.join(".cursor").join("hooks.json")));
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        assert!(within_root(
            &root,
            &root.join(".claude").join("settings.local.json")
        ));
        assert!(!within_root(&root, Path::new("/tmp/elsewhere")));
        assert!(!within_root(&root, &root.join("..").join("x")));
        let (_other, outside) = checkout();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert!(!within_root(&root, &root.join("link").join("hooks.json")));
        assert!(!within_root(
            Path::new("/no/such/root"),
            Path::new("/no/such/root/x")
        ));
    }
}
