//! The **agents pick** — WHICH agents topos touches on this machine: per project, or machine-wide.
//!
//! topos never touches an agent the person did not pick. The pick is one small JSON document:
//! `<project>/.topos/agents.json` for a project (personal, per clone, never committed: the
//! `.topos/` store ignores itself), `<machine store>/agents.json` for the machine. Shape:
//! `{"schema_version": 1, "agents": ["claude-code", "codex"]}`. The one token [`WILDCARD`]
//! stands for every agent installed on this machine, resolved at run time against detection
//! ([`registry::detected_harnesses`]); it stands ALONE, never beside a named agent.
//!
//! ## The effective pick
//!
//! In a project: the project file, else the machine file, else none. Machine-wide (`-g`): the
//! machine file alone. NO pick means NOTHING is placed (no folder, no config entry): a pick is
//! explicit, and detection serves only the wildcard's expansion and the questions that name what
//! is installed. A picked agent is written whether or not its detect dir exists.
//!
//! ## Consumers
//!
//! Every writer asks ONE of two shapes: [`picked_harnesses`] (the rows, table order) for the dir
//! planners, [`picked_slugs`] (the set) for the config-entry planners. Both answer the effective
//! pick for the scope the caller names, read from the ctx's MACHINE store
//! ([`crate::sidecar::Layout::machine_home`]) whichever store the ctx itself stands in. The
//! planners are infallible, so an unreadable pick file counts as no pick there (nothing placed,
//! fail closed); [`effective`] hands the error to the verbs that show the pick.
//!
//! ## The migration seed (the seam)
//!
//! An older machine holds no pick file; its registered auto-update hooks say which agents it
//! used. The one-shot seed that writes the machine pick from that record lands in this module as
//! `seed_from_trigger_record`, writing through [`write`] at [`PickScope::Machine`]; nothing here
//! reads the record yet.

// The engine consumes this module in the next commit (the pick-driven planners); until then only
// its own tests do, and the expectation below fails the gate the moment that stops being true.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "consumed by the placement engine in the next commit"
    )
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use topos_harness::registry::{self, KnownHarness};
use topos_types::PERSISTED_SCHEMA_VERSION;

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::{self, Layout};

/// The pick document's file name, at both scopes.
pub(crate) const PICK_FILE: &str = "agents.json";

/// The one token that is not a slug: every agent installed on this machine, resolved when read.
pub(crate) const WILDCARD: &str = "*";

/// The sentinel registry row no pick may name: it has no detection probe and is no agent.
const UNIVERSAL: &str = "universal";

/// The persisted pick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentsPick {
    pub schema_version: u32,
    /// Registry slugs, or the lone [`WILDCARD`].
    pub agents: Vec<String>,
}

impl AgentsPick {
    /// A pick of exactly these slugs (not validated here; see [`validate`] and [`write`]).
    pub(crate) fn new(agents: Vec<String>) -> Self {
        Self {
            schema_version: PERSISTED_SCHEMA_VERSION,
            agents,
        }
    }

    /// The wildcard pick: every agent installed on this machine.
    pub(crate) fn everything() -> Self {
        Self::new(vec![WILDCARD.to_owned()])
    }

    /// Whether this pick carries the wildcard (a hand-edited file may carry it beside a name; it
    /// still reads as the wildcard, which is the wider answer).
    pub(crate) fn is_wildcard(&self) -> bool {
        self.agents.iter().any(|a| a == WILDCARD)
    }
}

/// Where a pick is written: the machine store, or one project checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickScope {
    Machine,
    Project(PathBuf),
}

/// Which file an effective pick came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickSource {
    Project(PathBuf),
    Machine(PathBuf),
}

/// The pick in force for one scope, and the file that holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Effective {
    pub pick: AgentsPick,
    pub source: PickSource,
}

/// `<machine store>/agents.json` — read through the layout's MACHINE root, so a project-store
/// layout answers the same file.
pub(crate) fn machine_path(layout: &Layout) -> PathBuf {
    layout.machine_home().join(PICK_FILE)
}

/// `<project>/.topos/agents.json` — a sibling of the store's `state/`, inside the self-ignoring
/// `.topos/` dir.
pub(crate) fn project_path(project_dir: &Path) -> PathBuf {
    project_dir.join(sidecar::PROJECT_STORE_DIR).join(PICK_FILE)
}

/// The pick file for a scope.
pub(crate) fn path_for(layout: &Layout, scope: &PickScope) -> PathBuf {
    match scope {
        PickScope::Machine => machine_path(layout),
        PickScope::Project(dir) => project_path(dir),
    }
}

/// Read one pick file; `None` when absent.
///
/// # Errors
/// As [`crate::doc::read_doc`]: an unreadable or newer-schema file fails closed.
pub(crate) fn read(fs: &dyn FsOps, path: &Path) -> Result<Option<AgentsPick>, ClientError> {
    crate::doc::read_doc(fs, path)
}

/// Write a pick at `scope`, atomically, after [`validate`]. The machine store's root is created
/// when absent; a project's `.topos/` store is minted through [`sidecar::ensure_project_store`],
/// so the pick always sits beside the store's self-ignore file. Returns the path written.
///
/// # Errors
/// The [`validate`] refusals; [`sidecar::ensure_project_store`]'s containment refusal; the
/// filesystem failure.
pub(crate) fn write(
    fs: &dyn FsOps,
    layout: &Layout,
    scope: &PickScope,
    pick: &AgentsPick,
) -> Result<PathBuf, ClientError> {
    validate(&pick.agents)?;
    match scope {
        PickScope::Machine => fs.create_dir_all(layout.machine_home())?,
        PickScope::Project(dir) => {
            sidecar::ensure_project_store(fs, dir)?;
        }
    }
    let path = path_for(layout, scope);
    crate::doc::write_doc(fs, &path, pick)?;
    Ok(path)
}

/// Validate a pick's entries: every slug is a known harness other than the sentinel row, and the
/// wildcard, when present, is the only entry.
///
/// # Errors
/// [`ClientError::UnknownAgent`] for a slug the registry does not know (or the sentinel row),
/// naming the known slugs in the `-a` refusal's own words; [`ClientError::InvalidArgument`] for
/// a wildcard beside a named agent.
pub(crate) fn validate(agents: &[String]) -> Result<(), ClientError> {
    if agents.iter().any(|a| a == WILDCARD) && agents.len() > 1 {
        return Err(ClientError::InvalidArgument(format!(
            "\"{WILDCARD}\" already means every agent installed on this machine; name it alone"
        )));
    }
    for slug in agents {
        if slug == WILDCARD {
            continue;
        }
        if slug == UNIVERSAL || registry::known_harness(slug).is_none() {
            return Err(crate::ops::dest_select::unknown_agent(slug, false));
        }
    }
    Ok(())
}

/// The pick in force: the project's own file when `project_dir` names a checkout that has one,
/// else the machine file, else `None`.
///
/// # Errors
/// As [`read`].
pub(crate) fn effective(
    fs: &dyn FsOps,
    layout: &Layout,
    project_dir: Option<&Path>,
) -> Result<Option<Effective>, ClientError> {
    if let Some(dir) = project_dir {
        let path = project_path(dir);
        if let Some(pick) = read(fs, &path)? {
            return Ok(Some(Effective {
                pick,
                source: PickSource::Project(path),
            }));
        }
    }
    let path = machine_path(layout);
    Ok(read(fs, &path)?.map(|pick| Effective {
        pick,
        source: PickSource::Machine(path),
    }))
}

/// The registry rows a pick names, in TABLE order, deduplicated: the wildcard expands to the
/// agents installed here ([`registry::detected_harnesses`]); a named slug resolves whether or not
/// its detect dir exists. The sentinel row and a slug the table does not know resolve to nothing.
pub(crate) fn resolve(
    pick: &AgentsPick,
    home: &Path,
    cwd: Option<&Path>,
) -> Vec<&'static KnownHarness> {
    let rows = if pick.is_wildcard() {
        registry::detected_harnesses(home, cwd)
    } else {
        registry::known_harnesses()
            .iter()
            .filter(|h| pick.agents.iter().any(|a| a == h.slug))
            .collect()
    };
    rows.into_iter().filter(|h| h.slug != UNIVERSAL).collect()
}

/// **The dir planners' question**: the rows the effective pick names for the scope `project_dir`
/// stands for (`None` = the machine scope), over the ctx's machine store. Empty with no machine
/// roots (nothing to resolve against), with no pick, or with a pick that cannot be read.
pub(crate) fn picked_harnesses(
    ctx: &Ctx<'_>,
    project_dir: Option<&Path>,
) -> Vec<&'static KnownHarness> {
    let Some(roots) = &ctx.roots else {
        return Vec::new();
    };
    let Ok(Some(found)) = effective(ctx.fs, &ctx.layout, project_dir) else {
        return Vec::new();
    };
    resolve(
        &found.pick,
        &roots.home,
        project_dir.or(roots.cwd.as_deref()),
    )
}

/// **The entries planners' question**: [`picked_harnesses`] as a slug set.
pub(crate) fn picked_slugs(ctx: &Ctx<'_>, project_dir: Option<&Path>) -> BTreeSet<String> {
    picked_harnesses(ctx, project_dir)
        .iter()
        .map(|h| h.slug.to_owned())
        .collect()
}

/// Lay a pick file down directly (no validation, no store minting) — the rigs' seed.
#[cfg(test)]
pub(crate) fn write_pick(path: &Path, agents: &[&str]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let pick = AgentsPick::new(agents.iter().map(|s| (*s).to_owned()).collect());
    let mut bytes = serde_json::to_vec_pretty(&pick).unwrap();
    bytes.push(b'\n');
    std::fs::write(path, bytes).unwrap();
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::ctx::AgentRoots;
    use crate::fs_seam::RealFs;
    use crate::ids::test_sources::{FixedClock, SeqIds};
    use crate::plane::{InertFollow, InertPlane};
    use crate::test_support::MockHarness;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("topos-pick-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let dir = dir.canonicalize().unwrap_or(dir);
            Self(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A fake `$HOME` with a machine store at `<home>/.topos` and a project checkout beside it.
    struct Rig {
        home: Scratch,
        project: Scratch,
        fs: RealFs,
        ids: SeqIds,
        clock: FixedClock,
        harness: MockHarness,
    }
    impl Rig {
        fn new(tag: &str) -> Self {
            let project = Scratch::new(&format!("{tag}-proj"));
            std::fs::create_dir_all(project.0.join(".git")).unwrap();
            Self {
                home: Scratch::new(&format!("{tag}-home")),
                project,
                fs: RealFs,
                ids: SeqIds::new("s"),
                clock: FixedClock(1),
                harness: MockHarness::joining(""),
            }
        }
        fn layout(&self) -> Layout {
            Layout::new(&self.home.0.join(".topos"))
        }
        fn ctx(&self) -> Ctx<'_> {
            Ctx {
                progress: crate::progress::silent(),
                fs: &self.fs,
                ids: &self.ids,
                clock: &self.clock,
                device_id: "d_test".into(),
                layout: self.layout(),
                harness: &self.harness,
                triggers: crate::ops::Triggers::active_only(&crate::ops::INERT_TRIGGER),
                plane: &InertPlane,
                follow: &InertFollow,
                roots: Some(AgentRoots {
                    home: self.home.0.clone(),
                    cwd: Some(self.project.0.clone()),
                }),
            }
        }
        fn install(&self, dot_dir: &str) {
            std::fs::create_dir_all(self.home.0.join(dot_dir)).unwrap();
        }
    }

    fn slugs(rows: &[&'static KnownHarness]) -> Vec<&'static str> {
        rows.iter().map(|h| h.slug).collect()
    }

    #[test]
    fn a_project_pick_wins_over_the_machine_pick() {
        let rig = Rig::new("project-wins");
        let ctx = rig.ctx();
        let machine = AgentsPick::new(vec!["claude-code".to_owned()]);
        write(&rig.fs, &rig.layout(), &PickScope::Machine, &machine).unwrap();

        // Machine file alone: the project inherits it, and `-g` reads it.
        let inherited = effective(&rig.fs, &rig.layout(), Some(&rig.project.0))
            .unwrap()
            .expect("the machine pick reaches the project");
        assert_eq!(inherited.pick, machine);
        assert_eq!(
            inherited.source,
            PickSource::Machine(rig.layout().home().join(PICK_FILE))
        );
        assert_eq!(
            slugs(&picked_harnesses(&ctx, Some(&rig.project.0))),
            ["claude-code"]
        );

        // A project file of its own outranks it, for the project only.
        let project = AgentsPick::new(vec!["codex".to_owned(), "cursor".to_owned()]);
        let path = write(
            &rig.fs,
            &rig.layout(),
            &PickScope::Project(rig.project.0.clone()),
            &project,
        )
        .unwrap();
        assert_eq!(path, rig.project.0.join(".topos").join(PICK_FILE));
        assert_eq!(
            std::fs::read_to_string(rig.project.0.join(".topos").join(".gitignore")).unwrap(),
            "*\n",
            "the pick sits inside the self-ignoring store"
        );
        let own = effective(&rig.fs, &rig.layout(), Some(&rig.project.0))
            .unwrap()
            .unwrap();
        assert_eq!(own.pick, project);
        assert_eq!(own.source, PickSource::Project(path));
        assert_eq!(
            slugs(&picked_harnesses(&ctx, Some(&rig.project.0))),
            ["codex", "cursor"],
            "table order, whatever order the file spells"
        );
        assert_eq!(
            picked_slugs(&ctx, None),
            ["claude-code".to_owned()].into(),
            "the machine scope never reads a project file"
        );

        // A project with no file of its own still inherits the machine pick.
        let other = Scratch::new("project-wins-other");
        assert_eq!(
            slugs(&picked_harnesses(&ctx, Some(&other.0))),
            ["claude-code"]
        );
    }

    #[test]
    fn a_wildcard_pick_resolves_to_the_installed_agents() {
        let rig = Rig::new("wildcard");
        rig.install(".claude");
        rig.install(".cursor");
        write(
            &rig.fs,
            &rig.layout(),
            &PickScope::Machine,
            &AgentsPick::everything(),
        )
        .unwrap();
        let ctx = rig.ctx();
        assert_eq!(
            slugs(&picked_harnesses(&ctx, None)),
            ["claude-code", "cursor"]
        );
        // Resolved when READ: an agent installed later is picked without the file changing.
        rig.install(".codex");
        assert_eq!(
            slugs(&picked_harnesses(&ctx, None)),
            ["claude-code", "codex", "cursor"]
        );
        // A named pick is explicit: it resolves whether or not the agent is installed.
        let named = AgentsPick::new(vec!["gemini-cli".to_owned()]);
        assert_eq!(slugs(&resolve(&named, &rig.home.0, None)), ["gemini-cli"]);
    }

    #[test]
    fn an_unknown_slug_refuses_naming_the_known_ones() {
        let err = validate(&["claude-code".to_owned(), "claude".to_owned()]).unwrap_err();
        assert_eq!(err.code(), "UNKNOWN_AGENT");
        let text = err.to_string();
        assert!(
            text.starts_with("unknown agent: claude — known: "),
            "{text}"
        );
        assert!(text.contains("aider-desk"), "{text}");
        // Nothing is written on a refusal.
        let rig = Rig::new("unknown");
        let bad = AgentsPick::new(vec!["claude".to_owned()]);
        assert!(write(&rig.fs, &rig.layout(), &PickScope::Machine, &bad).is_err());
        assert!(!machine_path(&rig.layout()).exists());
    }

    #[test]
    fn the_universal_row_is_never_picked() {
        let err = validate(&["universal".to_owned()]).unwrap_err();
        assert_eq!(err.code(), "UNKNOWN_AGENT");
        // A hand-written file naming it resolves to no row at all.
        let pick = AgentsPick::new(vec!["universal".to_owned(), "codex".to_owned()]);
        assert_eq!(
            slugs(&resolve(&pick, Path::new("/nowhere"), None)),
            ["codex"]
        );
    }

    #[test]
    fn the_wildcard_must_stand_alone() {
        let err = validate(&["*".to_owned(), "codex".to_owned()]).unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
        assert_eq!(
            err.to_string(),
            "\"*\" already means every agent installed on this machine; name it alone"
        );
        assert!(validate(&["*".to_owned()]).is_ok());
        assert!(validate(&[]).is_ok(), "an empty pick is a pick of nothing");
    }

    #[test]
    fn no_pick_and_no_roots_both_pick_nothing() {
        let rig = Rig::new("nothing");
        rig.install(".claude");
        let ctx = rig.ctx();
        assert!(picked_harnesses(&ctx, None).is_empty(), "no file: no pick");
        assert!(picked_slugs(&ctx, Some(&rig.project.0)).is_empty());
        write(
            &rig.fs,
            &rig.layout(),
            &PickScope::Machine,
            &AgentsPick::everything(),
        )
        .unwrap();
        assert_eq!(slugs(&picked_harnesses(&ctx, None)), ["claude-code"]);
        let rootless = Ctx {
            roots: None,
            ..rig.ctx()
        };
        assert!(
            picked_harnesses(&rootless, None).is_empty(),
            "no machine roots: nothing to resolve a pick against"
        );
    }

    /// The planners run under a ctx re-rooted at a PROJECT store; the machine pick must still be
    /// found from there (the re-rooted layout remembers the machine store).
    #[test]
    fn a_project_store_ctx_still_reads_the_machine_pick() {
        let rig = Rig::new("re-rooted");
        write(
            &rig.fs,
            &rig.layout(),
            &PickScope::Machine,
            &AgentsPick::new(vec!["claude-code".to_owned()]),
        )
        .unwrap();
        let ctx = rig.ctx();
        let store = sidecar::ensure_project_store(&rig.fs, &rig.project.0).unwrap();
        let sctx = crate::ops::ctx_with_layout(&ctx, &store);
        assert_eq!(sctx.layout.machine_home(), rig.layout().home());
        assert_eq!(machine_path(&sctx.layout), machine_path(&rig.layout()));
        assert_eq!(
            slugs(&picked_harnesses(&sctx, Some(&rig.project.0))),
            ["claude-code"]
        );
        // A raw project layout nobody re-rooted into answers its own root: no machine document
        // lives there, so it finds no pick rather than someone else's.
        let raw = sidecar::project_store_layout(&rig.project.0);
        assert_eq!(raw.machine_home(), raw.home());
        assert!(effective(&rig.fs, &raw, None).unwrap().is_none());
    }

    #[test]
    fn an_unreadable_pick_fails_closed_for_the_planners_and_loud_for_the_verbs() {
        let rig = Rig::new("corrupt");
        rig.install(".claude");
        let path = machine_path(&rig.layout());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{\"schema_version\": 99, \"agents\": [\"*\"]}\n").unwrap();
        let ctx = rig.ctx();
        assert!(picked_harnesses(&ctx, None).is_empty());
        assert!(effective(&rig.fs, &rig.layout(), None).is_err());
    }
}
