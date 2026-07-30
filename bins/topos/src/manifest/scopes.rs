//! SCOPE resolution — which recipe governs a delivery, and what its rows demand.
//!
//! Scopes are UNBLENDED. A machine has at most two in play from any working directory:
//!
//! - the PERSON scope — the global manifest (`~/.topos/topos.toml`) when it exists, else the
//!   implicit recipe: one feed row per connected workspace. Delivers into the home harness dirs.
//! - the PROJECT scope — the NEAREST `topos.toml` walking up from the working directory, taken
//!   WHOLE (no merging with ancestors; each file governs its subtree minus any deeper file's).
//!   Delivers into the checkout's harness dirs.
//!
//! There is no cross-scope shadowing, subtraction, or layer arithmetic: each scope's delivered
//! set is the union of its own rows' resolutions (or the implicit feed rows), minus its `"off"`
//! switches, deduped by bundle identity — an explicit row's version/fields beat any SET's
//! delivery of the same identity, and sets deliver their curator's current truth. A row is
//! meaningful or harmless, never harmful; `status` discloses the inert ones.
//!
//! This module is PURE (no network): it loads and partitions the recipes. The reconcile dials
//! deliveries/catalogs and drives each partition; the verbs edit the files the partitions came
//! from.

use std::path::{Path, PathBuf};

use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::manifest::MANIFEST_FILE;
use crate::manifest::document::{
    BundleRow, EntryFields, EntryValue, KindDefaults, ManifestDoc, ManifestScope, PathSpec,
    parse_manifest,
};
use crate::manifest::keys::KeyShape;

/// Where a resolved item's bytes belong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedScope {
    /// Materialize inside THIS project (its harness dirs, kept out of commits).
    Project { dir: PathBuf },
    /// Materialize in the global home harness dirs.
    Person,
}

impl ResolvedScope {
    /// The receipt/section label: `"person"`, or the project directory's path.
    pub(crate) fn label(&self) -> String {
        match self {
            ResolvedScope::Person => "person".to_owned(),
            ResolvedScope::Project { dir } => dir.display().to_string(),
        }
    }
}

/// One demand row of a scope plan — the joined reference, its shape, its parsed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanRow {
    pub reference: String,
    pub shape: KeyShape,
    pub value: EntryValue,
}

impl PlanRow {
    fn from_bundle(row: &BundleRow) -> Self {
        PlanRow {
            reference: row.reference.clone(),
            shape: row.shape.clone(),
            value: row.value.clone(),
        }
    }

    /// The row's fields view — a plain version string reads as `version` alone.
    pub(crate) fn fields(&self) -> EntryFields {
        match &self.value {
            EntryValue::Fields(f) => f.clone(),
            EntryValue::Pin(p) => EntryFields {
                version: Some(p.clone()),
                ..EntryFields::default()
            },
            EntryValue::Star | EntryValue::Off => EntryFields::default(),
        }
    }

    /// The version pin the row spells, if any (`"*"` and absent both mean track-current).
    pub(crate) fn pin(&self) -> Option<String> {
        match &self.value {
            EntryValue::Pin(p) => Some(p.clone()),
            EntryValue::Fields(f) => f.version.clone().filter(|v| v != "*"),
            _ => None,
        }
    }

    /// The receipt name: the row's `name` override, else the reference's leaf.
    pub(crate) fn display_name(&self) -> String {
        if let EntryValue::Fields(f) = &self.value
            && let Some(n) = &f.name
        {
            return n.clone();
        }
        self.shape.leaf_name().to_owned()
    }
}

/// One scope's partitioned recipe — what the reconcile drives and `status` renders.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScopePlan {
    /// The file this plan came from; `None` = the implicit person recipe (no global file).
    pub file: Option<PathBuf>,
    /// Feed rows — `(host, workspace)` whose whole feed flows here (person scope only).
    pub feeds: Vec<(String, String)>,
    /// `"off"` switches (workspace bundles, person scope only).
    pub offs: Vec<PlanRow>,
    /// Explicit THING rows (workspace bundles, repo skills, local folders).
    pub things: Vec<PlanRow>,
    /// SET rows (channels, repo sets) — expanded by the reconcile; an explicit thing of the
    /// same identity beats a set's delivery.
    pub sets: Vec<PlanRow>,
    /// The `[defaults.<kind>]` sections, file order.
    pub defaults: Vec<(String, KindDefaults)>,
}

impl ScopePlan {
    /// Partition a parsed manifest into its plan.
    pub(crate) fn from_doc(doc: &ManifestDoc, file: Option<PathBuf>) -> Self {
        let mut plan = ScopePlan {
            file,
            defaults: doc.defaults.clone(),
            ..ScopePlan::default()
        };
        for row in &doc.rows {
            match (&row.shape, &row.value) {
                (KeyShape::Feed { host, workspace }, _) => {
                    plan.feeds.push((host.clone(), workspace.clone()));
                }
                (_, EntryValue::Off) => plan.offs.push(PlanRow::from_bundle(row)),
                (KeyShape::Channel { .. } | KeyShape::RepoSet { .. }, _) => {
                    plan.sets.push(PlanRow::from_bundle(row));
                }
                _ => plan.things.push(PlanRow::from_bundle(row)),
            }
        }
        plan
    }

    /// The IMPLICIT person recipe (no global file): one feed row per connected workspace —
    /// behaviorally identical to a file holding exactly those rows.
    pub(crate) fn implicit(workspaces: &[(String, String)]) -> Self {
        ScopePlan {
            feeds: workspaces.to_vec(),
            ..ScopePlan::default()
        }
    }

    /// Whether a FILE-backed plan is governing (only its rows deliver) — true whenever the
    /// global file exists, including empty or feed-less files (loudly disclosed elsewhere).
    pub(crate) fn file_backed(&self) -> bool {
        self.file.is_some()
    }

    /// The `"off"` row covering `(host, workspace, bundle)`, if one is spelled.
    pub(crate) fn off_for(&self, host: &str, workspace: &str, bundle: &str) -> Option<&PlanRow> {
        self.offs.iter().find(|r| {
            matches!(&r.shape, KeyShape::WorkspaceBundle { host: h, workspace: w, bundle: b }
                if h == host && w == workspace && b == bundle)
        })
    }

    /// Whether an explicit THING row already claims this workspace bundle (an explicit row's
    /// version/fields beat the feed's/a set's delivery of the same identity).
    pub(crate) fn explicit_claims(&self, host: &str, workspace: &str, bundle: &str) -> bool {
        self.things.iter().any(|r| {
            matches!(&r.shape, KeyShape::WorkspaceBundle { host: h, workspace: w, bundle: b }
                if h == host && w == workspace && b == bundle)
        })
    }

    /// Whether the feed of `(host, workspace)` flows in this plan.
    pub(crate) fn has_feed(&self, host: &str, workspace: &str) -> bool {
        self.feeds.iter().any(|(h, w)| h == host && w == workspace)
    }

    /// The regime sentence for one workspace: feed row present = adopting all assigned (with
    /// the off count); explicit rows only = explicit control. `None` when the plan neither
    /// feeds nor names the workspace.
    pub(crate) fn regime(
        &self,
        host: &str,
        workspace: &str,
        assigned_not_adopted: usize,
    ) -> Option<String> {
        let offs = self
            .offs
            .iter()
            .filter(|r| {
                matches!(&r.shape, KeyShape::WorkspaceBundle { host: h, workspace: w, .. }
                    if h == host && w == workspace)
            })
            .count();
        if self.has_feed(host, workspace) {
            return Some(if offs == 0 {
                "adopting all assigned".to_owned()
            } else {
                format!("adopting all assigned, {offs} off")
            });
        }
        let explicit = self
            .things
            .iter()
            .filter(|r| r.shape.workspace_key().as_deref() == Some(&format!("{host}/{workspace}")))
            .count()
            + self
                .sets
                .iter()
                .filter(|r| {
                    r.shape.workspace_key().as_deref() == Some(&format!("{host}/{workspace}"))
                })
                .count();
        if explicit > 0 {
            return Some(if assigned_not_adopted == 0 {
                format!("explicit: {explicit} bundles")
            } else {
                format!(
                    "explicit: {explicit} bundles; {assigned_not_adopted} assigned not adopted here"
                )
            });
        }
        None
    }
}

/// The person scope's recipe: the global file (parsed whole) when it exists, else the implicit
/// feed-per-workspace recipe built from the CONNECTED workspaces.
///
/// # Errors
/// A read failure, non-UTF-8 bytes, or a manifest the grammar refuses (typed, naming the fix).
pub(crate) fn person_plan(
    fs: &dyn FsOps,
    layout: &crate::sidecar::Layout,
    connected: &[(String, String)],
) -> Result<ScopePlan, ClientError> {
    let path = layout.home().join(MANIFEST_FILE);
    match fs.read_opt(&path)? {
        Some(bytes) => {
            let text = String::from_utf8(bytes)
                .map_err(|_| ClientError::Corrupt(format!("{}: not UTF-8", path.display())))?;
            let doc = parse_manifest(&text, ManifestScope::Global)
                .map_err(|e| ClientError::Corrupt(format!("{}: {e}", path.display())))?;
            Ok(ScopePlan::from_doc(&doc, Some(path)))
        }
        None => Ok(ScopePlan::implicit(connected)),
    }
}

/// The NEAREST project manifest covering `cwd` — the file wins WHOLE; the walk stops at the
/// first hit (and below `home`, where a manifest would shadow the person scope confusingly).
/// `Ok(None)` when no file covers the directory.
///
/// # Errors
/// As [`person_plan`], for the file that was found.
pub(crate) fn nearest_project_plan(
    fs: &dyn FsOps,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<Option<(PathBuf, ScopePlan)>, ClientError> {
    let Some(dir) = nearest_manifest_dir(fs, cwd, home) else {
        return Ok(None);
    };
    let path = dir.join(MANIFEST_FILE);
    let Some(bytes) = fs.read_opt(&path)? else {
        return Ok(None);
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| ClientError::Corrupt(format!("{}: not UTF-8", path.display())))?;
    let doc = parse_manifest(&text, ManifestScope::Project)
        .map_err(|e| ClientError::Corrupt(format!("{}: {e}", path.display())))?;
    Ok(Some((dir, ScopePlan::from_doc(&doc, Some(path)))))
}

/// The nearest directory holding a `topos.toml`, walking up from `cwd`; the walk stops below
/// `home` (a manifest AT the home directory never governs a project scope).
pub(crate) fn nearest_manifest_dir(
    fs: &dyn FsOps,
    cwd: &Path,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        if home.is_some_and(|h| d == h) {
            return None;
        }
        if fs.exists(&d.join(MANIFEST_FILE)) {
            return Some(d);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

/// EVERY directory holding a `topos.toml` up the chain (nearest first) — NOT a resolution input
/// (nearest wins whole); the cross-store surfaces (name resolution over visited project stores,
/// lazy store recovery) still visit each candidate.
pub(crate) fn manifest_dirs_up(fs: &dyn FsOps, cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        if home.is_some_and(|h| d == h) {
            break;
        }
        if fs.exists(&d.join(MANIFEST_FILE)) {
            out.push(d.clone());
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    out
}

/// The resolved placement override for one row's delivery into one harness: the row's own
/// `path` beats the `[defaults.<kind>]` path beats nothing (the registry mapping) — and within
/// each level the harness-named key beats that level's `default`.
pub(crate) fn path_override(
    row_path: Option<&PathSpec>,
    defaults: &[(String, KindDefaults)],
    kind: &str,
    harness_slug: &str,
) -> Option<String> {
    let level = |spec: &PathSpec| -> Option<String> {
        match spec {
            PathSpec::One(dir) => Some(dir.clone()),
            PathSpec::PerHarness { default, per } => per
                .iter()
                .find(|(h, _)| h == harness_slug)
                .map(|(_, d)| d.clone())
                .or_else(|| default.clone()),
        }
    };
    if let Some(spec) = row_path
        && let Some(dir) = level(spec)
    {
        return Some(dir);
    }
    defaults
        .iter()
        .find(|(k, _)| k == kind)
        .and_then(|(_, kd)| kd.path.as_ref())
        .and_then(level)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> String {
        "0123456789abcdef".repeat(4)
    }

    fn global_plan(text: &str) -> ScopePlan {
        let doc = parse_manifest(text, ManifestScope::Global).unwrap();
        ScopePlan::from_doc(&doc, Some(PathBuf::from("/home/.topos/topos.toml")))
    }

    #[test]
    fn a_plan_partitions_feeds_offs_things_and_sets() {
        let d = digest();
        let plan = global_plan(&format!(
            "[bundles]\n\
             \"topos.sh/acme\" = \"*\"\n\
             \"topos.sh/acme/noisy\" = \"off\"\n\
             \"topos.sh/beta/deploy\" = \"{d}\"\n\
             \"topos.sh/beta/channels/eng\" = \"*\"\n\
             \"github.com/o/r\" = \"*\"\n\
             \"github.com/o/r/s\" = \"*\"\n\
             \"./tools/x\" = \"*\"\n"
        ));
        assert_eq!(plan.feeds, vec![("topos.sh".into(), "acme".into())]);
        assert_eq!(plan.offs.len(), 1);
        // Things: the pinned bundle, the repo skill, the local folder.
        assert_eq!(plan.things.len(), 3);
        // Sets: the channel and the repo set.
        assert_eq!(plan.sets.len(), 2);
        assert!(plan.file_backed());
        assert!(plan.has_feed("topos.sh", "acme"));
        assert!(!plan.has_feed("topos.sh", "beta"));
        assert!(plan.off_for("topos.sh", "acme", "noisy").is_some());
        assert!(plan.off_for("topos.sh", "acme", "quiet").is_none());
        assert!(plan.explicit_claims("topos.sh", "beta", "deploy"));
        assert!(!plan.explicit_claims("topos.sh", "acme", "noisy"));
    }

    #[test]
    fn the_implicit_recipe_is_one_feed_row_per_workspace() {
        let plan = ScopePlan::implicit(&[
            ("topos.sh".into(), "acme".into()),
            ("topos.example.com".into(), "platform".into()),
        ]);
        assert!(!plan.file_backed());
        assert_eq!(plan.feeds.len(), 2);
        assert!(plan.things.is_empty() && plan.sets.is_empty() && plan.offs.is_empty());
    }

    #[test]
    fn regimes_phrase_per_workspace() {
        let plan = global_plan(
            "[bundles]\n\
             \"topos.sh/acme\" = \"*\"\n\
             \"topos.sh/acme/noisy\" = \"off\"\n\
             \"topos.sh/beta/deploy\" = \"*\"\n",
        );
        assert_eq!(
            plan.regime("topos.sh", "acme", 0).as_deref(),
            Some("adopting all assigned, 1 off")
        );
        assert_eq!(
            plan.regime("topos.sh", "beta", 2).as_deref(),
            Some("explicit: 1 bundles; 2 assigned not adopted here")
        );
        assert_eq!(plan.regime("topos.sh", "gamma", 0), None);
    }

    #[test]
    fn plan_rows_answer_pin_fields_and_names() {
        let d = digest();
        let plan = global_plan(&format!(
            "[bundles]\n\
             \"topos.sh/acme/a\" = \"{d}\"\n\
             \"topos.sh/acme/b\" = {{ version = \"*\", name = \"local-b\" }}\n\
             \"topos.sh/acme/c\" = \"*\"\n"
        ));
        let by_leaf = |l: &str| {
            plan.things
                .iter()
                .find(|r| r.shape.leaf_name() == l)
                .unwrap()
        };
        assert_eq!(by_leaf("a").pin().as_deref(), Some(d.as_str()));
        assert_eq!(by_leaf("b").pin(), None);
        assert_eq!(by_leaf("b").display_name(), "local-b");
        assert_eq!(by_leaf("c").display_name(), "c");
        assert_eq!(by_leaf("c").fields(), EntryFields::default());
    }

    #[test]
    fn path_override_levels_merge_per_key() {
        let defaults = vec![(
            "skill".to_owned(),
            KindDefaults {
                harness: None,
                path: Some(PathSpec::PerHarness {
                    default: Some(".agents/skills".into()),
                    per: vec![("claude-code".into(), ".claude/skills".into())],
                }),
            },
        )];
        // The entry's own path wins whole for its keys.
        let row = PathSpec::One("custom/".into());
        assert_eq!(
            path_override(Some(&row), &defaults, "skill", "claude-code").as_deref(),
            Some("custom/")
        );
        // No entry path: the kind default's harness key, else its default.
        assert_eq!(
            path_override(None, &defaults, "skill", "claude-code").as_deref(),
            Some(".claude/skills")
        );
        assert_eq!(
            path_override(None, &defaults, "skill", "codex").as_deref(),
            Some(".agents/skills")
        );
        // An unknown kind falls through to the registry (None).
        assert_eq!(path_override(None, &defaults, "knowledge", "codex"), None);
        // A per-harness entry table without the key and without a default falls to the kind level.
        let row = PathSpec::PerHarness {
            default: None,
            per: vec![("cursor".into(), "x/".into())],
        };
        assert_eq!(
            path_override(Some(&row), &defaults, "skill", "codex").as_deref(),
            Some(".agents/skills")
        );
    }

    #[test]
    fn nearest_wins_whole_and_the_chain_is_still_visitable() {
        use crate::fs_seam::RealFs;
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("topos-scopes-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        let nested = repo.join("services/api");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            repo.join(MANIFEST_FILE),
            "[bundles]\n\"topos.sh/acme/repo-wide\" = \"*\"\n",
        )
        .unwrap();
        std::fs::write(
            nested.join(MANIFEST_FILE),
            "[bundles]\n\"topos.sh/acme/api-only\" = \"*\"\n",
        )
        .unwrap();
        // The NEAREST file wins whole — the ancestor's rows never blend in.
        let (dir, plan) = nearest_project_plan(&RealFs, &nested, Some(&root))
            .unwrap()
            .unwrap();
        assert_eq!(dir, nested);
        assert_eq!(plan.things.len(), 1);
        assert_eq!(plan.things[0].reference, "topos.sh/acme/api-only");
        // From between the two files, the outer one governs.
        let (dir, plan) = nearest_project_plan(&RealFs, &repo.join("services"), Some(&root))
            .unwrap()
            .unwrap();
        assert_eq!(dir, repo);
        assert_eq!(plan.things[0].reference, "topos.sh/acme/repo-wide");
        // The store-visiting walk still sees BOTH manifest dirs.
        let dirs = manifest_dirs_up(&RealFs, &nested, Some(&root));
        assert_eq!(dirs, vec![nested.clone(), repo.clone()]);
        // No file anywhere → None.
        let bare = root.join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(
            nearest_project_plan(&RealFs, &bare, Some(&root))
                .unwrap()
                .is_none()
        );
    }
}
