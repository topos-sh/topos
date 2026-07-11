//! The `publish` auto-add pre-step ([`ops::ensure_tracked`]): the dispatch that adopts an untracked LOCAL
//! source (a discovered name, a `<name>@<harness>`, or a `<dir>`) before publishing, refuses a remote /
//! unsupported source typed, and hands an already-tracked skill straight through. Local-only — no plane,
//! no network — so it is exercised directly over the injected fs/clock/id seams.

use std::path::{Path, PathBuf};

use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::ops;
use crate::sidecar::Layout;

const DEVICE_ID: &str = "d_test";
const FIXED_MILLIS: u64 = 1_700_000_000_000;

/// A no-op adapter: discovers nothing (so a plain temp source is never harness-tagged) and touches no config.
#[derive(Debug)]
struct NoHarness;
impl HarnessAdapter for NoHarness {
    fn id(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }
    fn discover(&self) -> Vec<DiscoveredPlacement> {
        Vec::new()
    }
    fn placement_for(
        &self,
        skill_id: &str,
        _n: topos_harness::PlacementNaming<'_>,
        _: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget {
        PlacementTarget {
            dir: PathBuf::from(skill_id),
        }
    }
    fn currency_kind(&self) -> CurrencyKind {
        CurrencyKind::SessionStart
    }
    fn install_currency_trigger(&self) -> TriggerReport {
        report()
    }
    fn remove_currency_trigger(&self) -> TriggerReport {
        report()
    }
    fn uninstall_footprint(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}
fn report() -> TriggerReport {
    TriggerReport {
        harness: HarnessId::ClaudeCode,
        currency_kind: CurrencyKind::SessionStart,
        touched_path: None,
        marker_id: "test:none".to_owned(),
        state: TriggerState::Inactive,
    }
}

/// A self-cleaning temp directory.
struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-pa-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A deterministic context over a `~/.topos/` home.
struct Rig {
    home: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: NoHarness,
    plane: crate::plane::InertPlane,
    follow: crate::plane::InertFollow,
}
impl Rig {
    fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(tag),
            fs: RealFs,
            ids: SeqIds::new("t"),
            clock: FixedClock(FIXED_MILLIS),
            harness: NoHarness,
            plane: crate::plane::InertPlane,
            follow: crate::plane::InertFollow,
        }
    }
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: DEVICE_ID.to_owned(),
            layout: Layout::new(&self.home.0),
            harness: &self.harness,
            plane: &self.plane,
            follow: &self.follow,
        }
    }
}

/// Write a minimal skill bundle (a named `SKILL.md`) at `parent/<name>/`, returning its directory.
fn mk_skill(parent: &Path, name: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: test\n---\n# {name}\n"),
    )
    .unwrap();
    dir
}

// -------------------------------------------------------------------------------------------------
// The refusals — a source `publish` does NOT adopt.
// -------------------------------------------------------------------------------------------------

#[test]
fn a_remote_source_is_refused_pointing_at_add_first() {
    let rig = Rig::new("remote");
    let err = ops::ensure_tracked(&rig.ctx(), None, "vercel-labs/skills").unwrap_err();
    assert!(matches!(err, ClientError::InvalidArgument(_)), "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains("LOCAL skills only"), "{msg}");
    assert!(msg.contains("topos add"), "{msg}");
}

#[test]
fn an_unsupported_source_is_refused_typed() {
    let rig = Rig::new("unsupported");
    // An ssh/git URL is a recognized-but-unsupported shape — its own guidance rides the typed error.
    let err = ops::ensure_tracked(&rig.ctx(), None, "git@github.com:o/r.git").unwrap_err();
    assert!(matches!(err, ClientError::InvalidArgument(_)), "{err:?}");
}

// -------------------------------------------------------------------------------------------------
// The `<dir>` arm.
// -------------------------------------------------------------------------------------------------

#[test]
fn an_untracked_dir_is_adopted_in_place_and_disclosed() {
    let rig = Rig::new("dir-new");
    let src = Scratch::new("dir-new-src");
    let dir = mk_skill(&src.0, "deploy");

    let (name, added) = ops::ensure_tracked(&rig.ctx(), None, dir.to_str().unwrap()).unwrap();
    assert_eq!(name, "deploy");
    let added = added.expect("a fresh dir adopt discloses the add");
    assert_eq!(added.name, "deploy");
    // A plain temp dir under no harness dir carries no harness slug.
    assert_eq!(added.harness_slug, None);
    // It is now tracked — a second ensure_tracked publishes it without re-adopting.
    let (name2, added2) = ops::ensure_tracked(&rig.ctx(), None, dir.to_str().unwrap()).unwrap();
    assert_eq!(name2, "deploy");
    assert!(added2.is_none(), "an already-tracked dir is not re-added");
}

// -------------------------------------------------------------------------------------------------
// The `<name>` / `<name>@<harness>` arm.
// -------------------------------------------------------------------------------------------------

#[test]
fn an_already_tracked_name_is_the_fast_path() {
    let rig = Rig::new("name-tracked");
    let src = Scratch::new("name-tracked-src");
    let dir = mk_skill(&src.0, "lint");
    ops::add(&rig.ctx(), &dir).unwrap();

    let (name, added) = ops::ensure_tracked(&rig.ctx(), None, "lint").unwrap();
    assert_eq!(name, "lint");
    assert!(added.is_none(), "a tracked name is published, not re-added");
}

#[test]
fn a_harness_suffix_that_disagrees_with_the_tracked_skill_is_refused() {
    let rig = Rig::new("mismatch");
    let src = Scratch::new("mismatch-src");
    // Adopt a plain dir (no harness attribution → harness_slug None).
    let dir = mk_skill(&src.0, "deploy");
    ops::add(&rig.ctx(), &dir).unwrap();

    let err = ops::ensure_tracked(&rig.ctx(), None, "deploy@claude-code").unwrap_err();
    match err {
        ClientError::HarnessMismatch {
            name,
            requested,
            tracked,
        } => {
            assert_eq!(name, "deploy");
            assert_eq!(requested, "claude-code");
            assert_eq!(tracked, "<none>");
        }
        other => panic!("expected HarnessMismatch, got {other:?}"),
    }
}

#[test]
fn an_exact_tracked_name_wins_before_source_classification() {
    // A tracked skill whose NAME looks like a source shape must publish by its LITERAL name — never
    // misclassified as remote/`@harness`/path and re-resolved. Exact tracked-name match wins first.
    let rig = Rig::new("exact-shape");

    // (a) A `/`-namespaced name (frontmatter) — `classify` would otherwise read it as a remote `owner/repo`.
    let src_slash = Scratch::new("exact-slash-src");
    let slash_dir = src_slash.0.join("bundle");
    std::fs::create_dir_all(&slash_dir).unwrap();
    std::fs::write(
        slash_dir.join("SKILL.md"),
        "---\nname: team/deploy\ndescription: test\n---\n# team/deploy\n",
    )
    .unwrap();
    let a = ops::add(&rig.ctx(), &slash_dir).unwrap();
    assert_eq!(
        a.name, "team/deploy",
        "adopted under its literal frontmatter name"
    );
    let (name, added) = ops::ensure_tracked(&rig.ctx(), None, "team/deploy").unwrap();
    assert_eq!(
        name, "team/deploy",
        "the literal `/`-name resolves, never a remote `owner/repo`"
    );
    assert!(
        added.is_none(),
        "an exact tracked name is published, not re-added"
    );

    // (b) An `@`-containing name (dir basename) — `classify`+`split_target` would otherwise read it as
    // `<name>@<harness>`.
    let src_at = Scratch::new("exact-at-src");
    let at_dir = mk_skill(&src_at.0, "foo@bar");
    ops::add(&rig.ctx(), &at_dir).unwrap();
    let (name2, added2) = ops::ensure_tracked(&rig.ctx(), None, "foo@bar").unwrap();
    assert_eq!(
        name2, "foo@bar",
        "the literal `@`-name resolves, not `foo`@`bar`"
    );
    assert!(added2.is_none());
}

#[test]
fn an_untracked_name_resolves_against_discovery_and_adopts() {
    let rig = Rig::new("name-disc");
    // Seed a discoverable, untracked skill in a harness dir under a discovery-roots home.
    let disc_home = Scratch::new("name-disc-home");
    let skill_dir = disc_home.0.join(".cursor").join("skills");
    mk_skill(&skill_dir, "my-skill");
    let roots = ops::DiscoveryRoots {
        home: disc_home.0.clone(),
        cwd: None,
    };

    let (name, added) = ops::ensure_tracked(&rig.ctx(), Some(&roots), "my-skill").unwrap();
    assert_eq!(name, "my-skill");
    let added = added.expect("resolving a discovered name discloses the add");
    assert_eq!(added.name, "my-skill");
    // Now tracked → the same name is the fast path (no roots needed).
    let (again, added2) = ops::ensure_tracked(&rig.ctx(), None, "my-skill").unwrap();
    assert_eq!(again, "my-skill");
    assert!(added2.is_none());
}

#[test]
fn an_untracked_name_without_discovery_roots_is_a_usage_error() {
    let rig = Rig::new("no-roots");
    let err = ops::ensure_tracked(&rig.ctx(), None, "ghost").unwrap_err();
    assert!(matches!(err, ClientError::InvalidArgument(_)), "{err:?}");
    assert!(err.to_string().contains("$HOME"), "{err}");
}

#[test]
fn an_ambiguous_tracked_name_passes_through_for_the_workspace_filter() {
    let rig = Rig::new("ambig");
    // Two DISTINCT skills sharing one name → resolve_skill is ambiguous; ensure_tracked must NOT auto-add
    // over it — it hands the bare name to the ordinary (`--workspace`-filtered) resolve downstream.
    let src_a = Scratch::new("ambig-a");
    let src_b = Scratch::new("ambig-b");
    ops::add(&rig.ctx(), &mk_skill(&src_a.0, "dup")).unwrap();
    ops::add(&rig.ctx(), &mk_skill(&src_b.0, "dup")).unwrap();

    let (name, added) = ops::ensure_tracked(&rig.ctx(), None, "dup").unwrap();
    assert_eq!(name, "dup");
    assert!(
        added.is_none(),
        "an ambiguous tracked name is never auto-added"
    );
}
