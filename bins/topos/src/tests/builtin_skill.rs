//! The BUILT-IN `topos` skill suite: placement through the one engine (shared-dir-first over the
//! detected agents), the force-sync (a hand edit is overwritten, snapshot-first; a binary change
//! refreshes every copy), the Foreign freeze (the sweep never writes a pre-existing dir — marked
//! or not; only the consented `follow topos --yes` adopts a MARKED downloaded copy,
//! snapshot-first), the provenance matcher's fail-closed shapes, the durable `remove topos`
//! opt-out (+ `follow topos` back), the `--agent` exclusion route, `list`'s `built-in` row, and
//! the end-to-end name reservation (`add`). All over a real fs + a temp fake `$HOME` — the
//! developer's machine is never probed.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use topos_core::digest::{self, FileMode, ManifestEntry};
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::{AgentRoots, Ctx};
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::ops;
use crate::plane::{InertFollow, InertPlane};
use crate::scan::{ScannedBundle, ScannedFile};
use crate::sidecar::Layout;

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-bin-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.canonicalize().unwrap())
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A harness stub whose native placement is `<agent home>/.claude/skills/<dir>` (the active
/// adapter is never detected here — no `.claude` detect dir is created — so plans come from the
/// registry-detected agents alone).
struct StubClaude {
    skills: PathBuf,
}
impl HarnessAdapter for StubClaude {
    fn id(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }
    fn discover(&self) -> Vec<DiscoveredPlacement> {
        Vec::new()
    }
    fn placement_for(
        &self,
        skill_id: &str,
        naming: topos_harness::PlacementNaming<'_>,
        _d: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget {
        PlacementTarget {
            dir: topos_harness::choose_skill_dir(
                &self.skills,
                skill_id,
                naming,
                &topos_harness::dir_taken,
                &|_| false,
            ),
        }
    }
    fn currency_kind(&self) -> CurrencyKind {
        CurrencyKind::ExplicitPullOnly
    }
    fn install_currency_trigger(&self) -> TriggerReport {
        stub_report()
    }
    fn remove_currency_trigger(&self) -> TriggerReport {
        stub_report()
    }
    fn uninstall_footprint(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}
fn stub_report() -> TriggerReport {
    TriggerReport {
        harness: HarnessId::ClaudeCode,
        currency_kind: CurrencyKind::ExplicitPullOnly,
        touched_path: None,
        marker_id: "test".into(),
        state: TriggerState::Inactive,
    }
}

struct Rig {
    home: Scratch,
    agent_home: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: StubClaude,
}

impl Rig {
    fn new(tag: &str) -> Self {
        let agent_home = Scratch::new(&format!("{tag}-agents"));
        let harness = StubClaude {
            skills: agent_home.0.join(".claude").join("skills"),
        };
        Self {
            home: Scratch::new(&format!("{tag}-home")),
            agent_home,
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1),
            harness,
        }
    }
    fn layout(&self) -> Layout {
        Layout::new(&self.home.0)
    }
    fn detect(&self, dot_dir: &str) {
        std::fs::create_dir_all(self.agent_home.0.join(dot_dir)).unwrap();
    }
    fn ctx<'a>(&'a self, follow: &'a InertFollow, plane: &'a InertPlane) -> Ctx<'a> {
        Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: "d_test".to_owned(),
            layout: self.layout(),
            harness: &self.harness,
            plane,
            follow,
            roots: Some(AgentRoots {
                home: self.agent_home.0.clone(),
                cwd: None,
            }),
        }
    }
    /// The shared convention dir's placed copy.
    fn shared_copy(&self) -> PathBuf {
        self.agent_home
            .0
            .join(".agents")
            .join("skills")
            .join("topos")
    }
}

/// A deterministic stand-in bundle (what a DIFFERENT binary would render).
fn fake_bundle(body: &str) -> ScannedBundle {
    let files = vec![ScannedFile {
        path: "SKILL.md".to_owned(),
        mode: FileMode::Regular,
        bytes: format!("---\nname: topos\n---\n{body}\n").into_bytes(),
    }];
    let entries: Vec<ManifestEntry> = files
        .iter()
        .map(|f| ManifestEntry {
            path: f.path.clone(),
            mode: f.mode,
            content_sha256: digest::sha256(&f.bytes),
        })
        .collect();
    let bundle_digest = digest::bundle_digest(&entries).unwrap();
    ScannedBundle {
        files,
        bundle_digest,
        name_hint: Some("topos".to_owned()),
    }
}

#[test]
fn ensure_places_the_bundle_and_lists_it_as_built_in() {
    let rig = Rig::new("place");
    rig.detect(".cline"); // covered → rides the shared dir
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);

    let sync = ops::ensure_builtin(&ctx).unwrap();
    assert!(sync.changed, "first contact lands bytes");
    let shared = rig.shared_copy();
    let skill_md = std::fs::read_to_string(shared.join("SKILL.md")).unwrap();
    assert_eq!(
        skill_md,
        include_str!("../../../../skills/topos/SKILL.md"),
        "the placed SKILL.md IS the committed top-level source — one file, no stamp"
    );
    assert!(
        skill_md.contains("topos: builtin"),
        "the provenance marker rides the placed frontmatter"
    );
    assert_eq!(
        std::fs::read_to_string(shared.join("INSTALL.md")).unwrap(),
        include_str!("../../../../skills/topos/INSTALL.md"),
        "the placed INSTALL.md IS the committed top-level source"
    );
    let reference = std::fs::read_to_string(shared.join("reference.md")).unwrap();
    assert_eq!(
        reference,
        crate::cli_ref::cli_ref_md(),
        "the placed reference IS the generated docs/cli.md bytes — one renderer"
    );

    // A second sweep is a byte-silent no-op.
    let sync = ops::ensure_builtin(&ctx).unwrap();
    assert!(!sync.changed, "an in-sync sweep changes nothing");

    // `list` carries the row with the built-in source (never the bare-local no-columns shape).
    let out = ops::list(&ctx, None, false, None, None).unwrap();
    let row = out
        .data
        .tracked
        .iter()
        .find(|s| s.skill == "topos")
        .expect("the built-in rows in list");
    assert_eq!(row.source.as_deref(), Some("built-in"));
    assert!(!row.draft);
}

#[test]
fn a_hand_edit_is_overwritten_on_the_next_sweep() {
    let rig = Rig::new("force");
    rig.detect(".cline");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);
    ops::ensure_builtin(&ctx).unwrap();

    let placed = rig.shared_copy().join("SKILL.md");
    let original = std::fs::read_to_string(&placed).unwrap();
    std::fs::write(&placed, "# my edits\n").unwrap();

    let sync = ops::ensure_builtin(&ctx).unwrap();
    assert!(sync.changed, "the divergent copy is re-synced");
    assert_eq!(
        std::fs::read_to_string(&placed).unwrap(),
        original,
        "force-synced back to the binary's bytes — the built-in never carries a draft"
    );
}

#[test]
fn a_binary_change_refreshes_every_placed_copy() {
    let rig = Rig::new("upgrade");
    rig.detect(".cline");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);

    // "Old binary" placed one shape…
    let old = fake_bundle("old body");
    ops::builtin_ensure_with(&ctx, &old).unwrap();
    let placed = rig.shared_copy().join("SKILL.md");
    assert!(
        std::fs::read_to_string(&placed)
            .unwrap()
            .contains("old body")
    );

    // …the "new binary" re-commits and re-places (parents advance, no draft, no freeze).
    let new = fake_bundle("new body");
    let sync = ops::builtin_ensure_with(&ctx, &new).unwrap();
    assert!(sync.changed);
    assert!(
        std::fs::read_to_string(&placed)
            .unwrap()
            .contains("new body"),
        "the placed copy tracks the binary"
    );
    // And the refresh is idempotent.
    assert!(!ops::builtin_ensure_with(&ctx, &new).unwrap().changed);
}

#[test]
fn remove_is_a_durable_opt_out_and_follow_brings_it_back() {
    let rig = Rig::new("optout");
    rig.detect(".cline");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);
    ops::ensure_builtin(&ctx).unwrap();
    let shared = rig.shared_copy();
    assert!(shared.exists());

    // The remove verb routes `topos` to the built-in opt-out (two-phase; the describe discloses
    // the way back).
    let dir_connect = |_: &str| -> Box<dyn crate::plane::DirectorySource> {
        unreachable!("the built-in removal is offline — no directory transport is ever built")
    };
    let connectors = ops::RemoveConnectors {
        directory: &dir_connect,
    };
    let targets = vec!["topos".to_owned()];
    match ops::remove(&ctx, &connectors, &targets, &[], None, false).unwrap() {
        ops::RemoveOutcome::Described { data, .. } => {
            let note = data.items[0].note.as_deref().unwrap_or_default();
            assert!(note.contains("topos follow topos"), "{note}");
        }
        _ => panic!("bare remove describes"),
    }
    match ops::remove(&ctx, &connectors, &targets, &[], None, true).unwrap() {
        ops::RemoveOutcome::Applied(data) => assert!(data.applied),
        _ => panic!("--yes applies"),
    }
    assert!(!shared.exists(), "the placed copy is gone");
    assert!(
        !rig.home.0.join("skills").join("topos").exists(),
        "the sidecar entry is gone"
    );

    // The next sweep does NOT resurrect it — the opt-out is durable.
    let sync = ops::ensure_builtin(&ctx).unwrap();
    assert!(!sync.changed);
    assert!(!shared.exists(), "no resurrection");

    // `follow topos` is the way back: describe first, then `--yes` re-places.
    match ops::builtin_follow(&ctx, &[], false).unwrap() {
        ops::AgentScopeOutcome::Described { data, yes_argv } => {
            assert_eq!(data.action, "restore");
            assert!(!data.applied);
            assert_eq!(yes_argv, vec!["topos", "follow", "topos", "--yes"]);
        }
        ops::AgentScopeOutcome::Applied(_) => panic!("bare follow describes"),
    }
    assert!(!shared.exists(), "a describe mutates nothing");
    match ops::builtin_follow(&ctx, &[], true).unwrap() {
        ops::AgentScopeOutcome::Applied(data) => assert!(data.applied),
        _ => panic!("--yes applies"),
    }
    assert!(shared.join("SKILL.md").exists(), "re-placed");
}

#[test]
fn a_scoped_follow_restores_and_records_the_include_list_in_one_act() {
    let rig = Rig::new("scoped-restore");
    rig.detect(".cline");
    rig.detect(".cursor");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);
    ops::ensure_builtin(&ctx).unwrap();

    // Opt out, then `follow topos --agent cursor --yes` — the restore lifts the opt-out AND
    // records the include-list in the same act (never a refusal toward a second command).
    let dir_connect =
        |_: &str| -> Box<dyn crate::plane::DirectorySource> { unreachable!("offline") };
    let connectors = ops::RemoveConnectors {
        directory: &dir_connect,
    };
    ops::remove(&ctx, &connectors, &["topos".to_owned()], &[], None, true).unwrap();

    let agents = vec!["cursor".to_owned()];
    match ops::builtin_follow(&ctx, &agents, true).unwrap() {
        ops::AgentScopeOutcome::Applied(data) => {
            assert_eq!(data.action, "restore");
            assert_eq!(data.agents, agents);
        }
        _ => panic!("--yes applies"),
    }
    let cursor_native = rig
        .agent_home
        .0
        .join(".cursor")
        .join("skills")
        .join("topos");
    assert!(
        cursor_native.join("SKILL.md").exists(),
        "the scoped restore lands the included agent's native copy"
    );
    assert!(
        !rig.shared_copy().exists(),
        "an include-list narrows to native-only — no shared copy"
    );
}

#[test]
fn a_pre_existing_foreign_topos_dir_is_never_written_and_never_deleted() {
    let rig = Rig::new("foreign");
    rig.detect(".cursor");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);

    // The user's OWN pre-existing skill dir at the exact reserved native path, before topos
    // ever places anything.
    let foreign = rig
        .agent_home
        .0
        .join(".cursor")
        .join("skills")
        .join("topos");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("SKILL.md"), "# the user's own topos skill\n").unwrap();

    // The sweep records the occupied dir as a frozen reservation and never writes into it.
    ops::ensure_builtin(&ctx).unwrap();
    assert_eq!(
        std::fs::read_to_string(foreign.join("SKILL.md")).unwrap(),
        "# the user's own topos skill\n",
        "a foreign dir is never overwritten"
    );

    // The CONSENTED restore does not adopt an UNMARKED dir either — no provenance marker, no
    // takeover, even under `follow topos --yes`.
    ops::builtin_follow(&ctx, &[], true).unwrap();
    assert_eq!(
        std::fs::read_to_string(foreign.join("SKILL.md")).unwrap(),
        "# the user's own topos skill\n",
        "the restore keeps the Foreign freeze on an unmarked occupant"
    );

    // And the opt-out cleans ONLY what the built-in materialized — the foreign dir survives.
    let dir_connect =
        |_: &str| -> Box<dyn crate::plane::DirectorySource> { unreachable!("offline") };
    let connectors = ops::RemoveConnectors {
        directory: &dir_connect,
    };
    ops::remove(&ctx, &connectors, &["topos".to_owned()], &[], None, true).unwrap();
    assert!(
        foreign.join("SKILL.md").exists(),
        "remove deletes only materialized built-in copies — never a dir it did not write"
    );
}

/// The marked downloaded copy every takeover test starts from, laid at cursor's native `topos`
/// dir before topos ever places anything.
fn lay_downloaded_copy(rig: &Rig) -> (PathBuf, &'static str) {
    let downloaded = rig
        .agent_home
        .0
        .join(".cursor")
        .join("skills")
        .join("topos");
    std::fs::create_dir_all(&downloaded).unwrap();
    let stale_skill =
        "---\nname: topos\nmetadata:\n  topos: builtin\n---\n# a stale downloaded copy\n";
    std::fs::write(downloaded.join("SKILL.md"), stale_skill).unwrap();
    std::fs::write(downloaded.join("reference.md"), "stale reference\n").unwrap();
    (downloaded, stale_skill)
}

#[test]
fn the_sweep_never_writes_a_marked_downloaded_copy() {
    let rig = Rig::new("sweep-freeze");
    rig.detect(".cursor");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);
    let (downloaded, stale_skill) = lay_downloaded_copy(&rig);

    // The silent sweep leaves it byte-untouched — marker or not, a dir the record says topos
    // never wrote is never written by a sweep. Adoption needs the consented `follow topos --yes`.
    let sync = ops::ensure_builtin(&ctx).unwrap();
    assert!(
        !sync.changed,
        "no bytes landed — the occupied dir is frozen"
    );
    assert_eq!(
        std::fs::read_to_string(downloaded.join("SKILL.md")).unwrap(),
        stale_skill,
        "the marked downloaded copy is never overwritten by the sweep"
    );
    assert!(
        !downloaded.join("INSTALL.md").exists(),
        "nothing of the binary's bundle lands"
    );

    // Durable across repeat sweeps.
    assert!(!ops::ensure_builtin(&ctx).unwrap().changed);
    assert_eq!(
        std::fs::read_to_string(downloaded.join("SKILL.md")).unwrap(),
        stale_skill
    );
}

#[test]
fn follow_yes_adopts_a_marked_downloaded_copy_snapshot_first() {
    let rig = Rig::new("takeover");
    rig.detect(".cursor");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);
    let (downloaded, stale_skill) = lay_downloaded_copy(&rig);
    ops::ensure_builtin(&ctx).unwrap(); // the sweep has recorded the frozen reservation

    // The bare `follow topos` DESCRIBES the adoption `--yes` will perform — and writes nothing.
    match ops::builtin_follow(&ctx, &[], false).unwrap() {
        ops::AgentScopeOutcome::Described { data, .. } => {
            let notes = &data.items[0].notes;
            assert!(
                notes
                    .iter()
                    .any(|n| n.contains("adopts the downloaded copy")
                        && n.contains(&downloaded.display().to_string())
                        && n.contains("snapshot-first")),
                "the describe disclosed the takeover: {notes:?}"
            );
        }
        _ => panic!("bare follow describes"),
    }
    assert_eq!(
        std::fs::read_to_string(downloaded.join("SKILL.md")).unwrap(),
        stale_skill,
        "a describe mutates nothing"
    );

    // `--yes` ADOPTS it: the dir is force-synced to the binary's bundle.
    match ops::builtin_follow(&ctx, &[], true).unwrap() {
        ops::AgentScopeOutcome::Applied(data) => assert!(data.applied),
        _ => panic!("--yes applies"),
    }
    assert_eq!(
        std::fs::read_to_string(downloaded.join("SKILL.md")).unwrap(),
        include_str!("../../../../skills/topos/SKILL.md"),
        "the stale copy is replaced by the binary's bytes"
    );
    assert_eq!(
        std::fs::read_to_string(downloaded.join("reference.md")).unwrap(),
        crate::cli_ref::cli_ref_md(),
        "the reference tracks the binary"
    );
    assert!(
        downloaded.join("INSTALL.md").exists(),
        "the full bundle lands"
    );

    // Snapshot-first: the pre-existing bytes were committed into the sidecar store before the
    // overwrite (genesis + the snapshot), never lost.
    let sid = crate::id::SkillId::parse("topos").unwrap();
    let sp = rig.layout().published(&sid);
    let versions = topos_gitstore::Store::open(&sp.store)
        .unwrap()
        .list_versions()
        .unwrap();
    assert_eq!(
        versions.len(),
        2,
        "the genesis version + the pre-adopt snapshot"
    );

    // Adopted means MANAGED from here on: a repeat sweep is a no-op, and a later hand edit is
    // force-synced like any built-in copy.
    assert!(!ops::ensure_builtin(&ctx).unwrap().changed);
    std::fs::write(downloaded.join("SKILL.md"), "# my edits\n").unwrap();
    assert!(ops::ensure_builtin(&ctx).unwrap().changed);
    assert_eq!(
        std::fs::read_to_string(downloaded.join("SKILL.md")).unwrap(),
        include_str!("../../../../skills/topos/SKILL.md"),
    );
}

#[test]
fn the_provenance_matcher_accepts_only_the_published_metadata_shape() {
    // TRUE: the published shape — the marker nested under a top-level `metadata:` key inside a
    // TERMINATED leading frontmatter block (the committed source is the canonical instance).
    assert!(ops::builtin_marker_in_frontmatter(include_str!(
        "../../../../skills/topos/SKILL.md"
    )));
    assert!(ops::builtin_marker_in_frontmatter(
        "---\nname: topos\nmetadata:\n  topos: builtin\n---\n# body\n"
    ));

    // FALSE: the marker line inside another key's block scalar.
    assert!(!ops::builtin_marker_in_frontmatter(
        "---\nname: mine\ndescription: |\n  topos: builtin\n---\n# body\n"
    ));
    // FALSE: a root-level `topos: builtin` key — not a `metadata:` entry.
    assert!(!ops::builtin_marker_in_frontmatter(
        "---\nname: mine\ntopos: builtin\n---\n# body\n"
    ));
    // FALSE: an UNTERMINATED frontmatter block — the whole file would otherwise scan as header.
    assert!(!ops::builtin_marker_in_frontmatter(
        "---\nname: mine\nmetadata:\n  topos: builtin\n"
    ));
    // FALSE: the marker indented under a LATER top-level key (context left `metadata:`).
    assert!(!ops::builtin_marker_in_frontmatter(
        "---\nmetadata:\n  kind: skill\nnotes: |\n  topos: builtin\n---\n# body\n"
    ));
    // FALSE: the marker NESTED DEEPER under `metadata:` — inside a sub-key's block scalar, not a
    // direct entry (the direct-child indent is fixed by the first indented line, here `notes:`).
    assert!(!ops::builtin_marker_in_frontmatter(
        "---\nmetadata:\n  notes: |\n    topos: builtin\n---\n# body\n"
    ));
    // TRUE: a sibling key AFTER a block-scalar sub-key ends the scalar at the direct-child indent
    // (YAML sibling semantics) — still a direct `metadata:` entry.
    assert!(ops::builtin_marker_in_frontmatter(
        "---\nmetadata:\n  notes: |\n    scribble\n  topos: builtin\n---\n# body\n"
    ));
    // FALSE: a tab in the marker line's leading whitespace — not the published shape.
    assert!(!ops::builtin_marker_in_frontmatter(
        "---\nmetadata:\n\ttopos: builtin\n---\n# body\n"
    ));
    // FALSE: no leading frontmatter at all.
    assert!(!ops::builtin_marker_in_frontmatter(
        "# a plain file\ntopos: builtin\n"
    ));
}

#[test]
fn per_agent_exclusion_flips_to_native_only_and_cleans_that_agent() {
    let rig = Rig::new("scope");
    rig.detect(".cline"); // covered → shared
    rig.detect(".cursor"); // probed NOT covered → native
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);
    ops::ensure_builtin(&ctx).unwrap();
    let shared = rig.shared_copy();
    let cursor_native = rig
        .agent_home
        .0
        .join(".cursor")
        .join("skills")
        .join("topos");
    assert!(shared.exists());

    // Excluding cline: any scope narrows placement to native-only — the shared copy (which served
    // cline) is cleaned, and the remaining agent (cursor) gets its own native copy.
    let targets = vec!["topos".to_owned()];
    let agents = vec!["cline".to_owned()];
    match ops::exclude_agents(&ctx, "remove", &targets, &agents, None).unwrap() {
        ops::AgentScopeOutcome::Applied(data) => assert!(data.applied),
        _ => panic!("the per-agent exclusion applies immediately"),
    }
    assert!(!shared.exists(), "narrowing cleans the shared copy");
    assert!(
        cursor_native.join("SKILL.md").exists(),
        "the remaining agent keeps a native copy"
    );

    // The exclusion is durable across sweeps: nothing re-places the shared copy.
    ops::ensure_builtin(&ctx).unwrap();
    assert!(!shared.exists(), "no sweep resurrects an exclusion");
    assert!(cursor_native.join("SKILL.md").exists());
}

#[test]
fn the_name_is_reserved_end_to_end_client_side() {
    let rig = Rig::new("reserve");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);

    // `add` refuses adopting any dir under the reserved name…
    let dir = rig.agent_home.0.join("topos");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), "# mine\n").unwrap();
    let err = ops::add(&ctx, &dir).expect_err("reserved");
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    assert!(
        crate::render::safe_message(&err).contains("reserved"),
        "{err:?}"
    );

    // …and the one naming discipline never hands the reserved dir to another skill, even free.
    let root = rig.agent_home.0.join(".agents").join("skills");
    let chosen = topos_harness::choose_skill_dir(
        &root,
        "topos_abc123",
        topos_harness::PlacementNaming {
            name: Some("topos"),
            workspace_slug: Some("acme"),
        },
        &topos_harness::dir_taken,
        &|_| false,
    );
    assert_eq!(
        chosen,
        root.join("topos-acme"),
        "a foreign skill named `topos` disambiguates like a collision"
    );
    // The built-in itself (skill id == the reserved name) keeps the plain dir.
    let own = topos_harness::choose_skill_dir(
        &root,
        "topos",
        topos_harness::PlacementNaming {
            name: Some("topos"),
            workspace_slug: None,
        },
        &topos_harness::dir_taken,
        &|_| false,
    );
    assert_eq!(own, root.join("topos"));
}

#[test]
fn a_star_restore_resets_the_per_agent_exclusions_to_the_default() {
    // Exclude one agent, opt out whole-device, then restore with `--agent '*'`: the reset returns
    // the DEFAULT placement — the include-list AND the per-agent exclusions clear (the same fold
    // the ordinary scope update applies), so the previously excluded agent is served again. With
    // the exclusion wrongly retained, the restore would stay narrowed native-only and cline's
    // shared copy would never come back.
    let rig = Rig::new("star-restore");
    rig.detect(".cline");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);
    ops::ensure_builtin(&ctx).unwrap();
    let shared = rig.shared_copy();
    assert!(shared.exists());

    // The per-agent exclusion (any scope narrows to native-only; cline was the shared copy's
    // audience, so nothing serves it now).
    let targets = vec!["topos".to_owned()];
    match ops::exclude_agents(&ctx, "remove", &targets, &["cline".to_owned()], None).unwrap() {
        ops::AgentScopeOutcome::Applied(data) => assert!(data.applied),
        _ => panic!("the per-agent exclusion applies immediately"),
    }
    assert!(
        !shared.exists(),
        "the exclusion cleaned cline's serving copy"
    );

    // The whole-device opt-out, then the `'*'` restore.
    let dir_connect = |_: &str| -> Box<dyn crate::plane::DirectorySource> {
        unreachable!("the built-in removal is offline — no directory transport is ever built")
    };
    let connectors = ops::RemoveConnectors {
        directory: &dir_connect,
    };
    match ops::remove(&ctx, &connectors, &targets, &[], None, true).unwrap() {
        ops::RemoveOutcome::Applied(data) => assert!(data.applied),
        _ => panic!("--yes applies the opt-out"),
    }
    match ops::builtin_follow(&ctx, &["*".to_owned()], true).unwrap() {
        ops::AgentScopeOutcome::Applied(data) => assert!(data.applied),
        _ => panic!("--yes applies the restore"),
    }
    assert!(
        shared.join("SKILL.md").exists(),
        "the default placement serves cline again — the reset dropped the exclusion"
    );
    // Durable: the next sweep keeps the default placement (no retained exclusion re-narrows it).
    ops::ensure_builtin(&ctx).unwrap();
    assert!(shared.join("SKILL.md").exists());
}
