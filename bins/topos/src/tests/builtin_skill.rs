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
            progress: crate::progress::silent(),
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

    // The store carries the built-in's record (the manifest-resolved inventory shows only what
    // the scopes deliver — the built-in is force-synced custody, not a manifest row).
    let held = ctx
        .fs
        .read_dir(&ctx.layout.skills_dir())
        .unwrap()
        .into_iter()
        .filter_map(|e| e.file_name().and_then(|n| n.to_str().map(str::to_owned)))
        .filter_map(|id| crate::id::SkillId::parse(&id).ok())
        .filter_map(|sid| {
            crate::doc::read_doc::<topos_types::persisted::Lock>(
                ctx.fs,
                &ctx.layout.published(&sid).lock,
            )
            .ok()
            .flatten()
        })
        .any(|lock| lock.name == "topos" && crate::ops::is_builtin(&lock.skill_id));
    assert!(held, "the built-in's store record stands");
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

/// The edit a force-sync overwrites is KEPT in the store — and named on no surface.
///
/// Both halves matter. Keeping it is the standing promise: no byte differing from its baseline is
/// destroyed without a snapshot behind it, and this one is recoverable from the store forever.
/// Surfacing it would be a lie of a different kind — the bundle ships with the binary, so there is
/// no verb that puts those bytes back, nothing to publish (the name is reserved workspace-side) and
/// no manifest row to reset. A `(draft)` on the row, a `drafts ahead` count, or a version in the
/// history would each name work a person could come back to, and none of them can be come back to.
#[test]
fn an_overwritten_edit_is_kept_in_the_store_and_named_on_no_surface() {
    let rig = Rig::new("edit-quiet");
    rig.detect(".cline");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);
    ops::builtin_ensure_with(&ctx, &fake_bundle("old body")).unwrap();

    // A hand edit. While it is still ON DISK — the window where the copy really does differ from
    // the record — the inventory and the health panel say nothing about it.
    let dir = rig.shared_copy();
    std::fs::write(dir.join("SKILL.md"), "---\nname: topos\n---\nmy own edit\n").unwrap();
    let edited = crate::scan::scan(&dir).unwrap();
    assert_quiet_surfaces(&ctx);

    // Then a binary change — the force-sync overwrites the copy, snapshot-first.
    assert!(
        ops::builtin_ensure_with(&ctx, &fake_bundle("new body"))
            .unwrap()
            .changed
    );

    // KEPT: exactly one stored version renders the bytes that were on disk, byte for byte.
    let sid = crate::id::SkillId::parse("topos").unwrap();
    let store = topos_gitstore::Store::open(&ctx.layout.published(&sid).store).unwrap();
    let recovered: Vec<_> = store
        .list_versions()
        .unwrap()
        .into_iter()
        .filter_map(|v| store.render_verified(v, edited.bundle_digest).ok())
        .collect();
    assert_eq!(recovered.len(), 1, "the edit is recoverable from the store");
    assert_eq!(
        recovered[0]
            .files
            .iter()
            .map(|f| (f.path.clone(), f.bytes.clone()))
            .collect::<Vec<_>>(),
        edited
            .files
            .iter()
            .map(|f| (f.path.clone(), f.bytes.clone()))
            .collect::<Vec<_>>(),
    );

    // And with the snapshot now standing in the store, the same surfaces stay quiet.
    assert_quiet_surfaces(&ctx);

    // The history walks the built-in's own lineage; the snapshot hangs OFF it and never appears.
    let dir_conn = |_: &str| -> Box<dyn crate::plane::DirectorySource> {
        unreachable!("a local log builds no directory transport")
    };
    let nosess = |_: &crate::sessions::Session| -> ops::SessionTransports {
        unreachable!("a local log builds no session transports")
    };
    let history = ops::log(
        &ctx,
        &ops::LogConnectors {
            directory: &dir_conn,
            session: &nosess,
        },
        "topos",
        crate::ops::RowPage::unlimited(),
    )
    .unwrap();
    let snapshot_id = topos_core::digest::to_hex(&recovered_version(&store, &edited));
    assert!(
        history
            .events
            .iter()
            .any(|e| e.get("action").and_then(|v| v.as_str()) == Some("version")),
        "the built-in's own versions ARE in the history — this is not an empty read: {:?}",
        history.events
    );
    for event in &history.events {
        let rendered = event.to_string();
        assert!(!rendered.contains(&snapshot_id), "{rendered}");
        assert!(!rendered.contains("draft"), "{rendered}");
    }
}

/// `list` (the row AND the deep dive) and `status` say nothing about an edit to the built-in.
/// `status` is manifest-row driven and the built-in has no row anywhere, so it is asserted here as
/// the standing guarantee it is, not as a thing that once leaked.
fn assert_quiet_surfaces(ctx: &Ctx<'_>) {
    let page = crate::ops::RowPage::unlimited();
    let listed = ops::list_with(ctx, &ops::ListRequest::default(), None, None, page).unwrap();
    let row = listed
        .data
        .scopes
        .iter()
        .flat_map(|s| &s.rows)
        .find(|r| r.skill == "topos")
        .expect("the built-in is listed");
    assert!(!row.draft, "{row:?}");
    assert_eq!(row.status, None, "{row:?}");
    let text = crate::render::list_tty(&listed);
    assert!(!text.contains("(draft)"), "{text}");

    let dive = ops::list_with(
        ctx,
        &ops::ListRequest {
            name: Some("topos".to_owned()),
            ..ops::ListRequest::default()
        },
        None,
        None,
        page,
    )
    .unwrap();
    assert!(
        !crate::render::list_tty(&dive).contains("local edits"),
        "the deep dive claims no edits either"
    );

    let status = ops::status_snapshot(ctx, ops::ScopeView::All).unwrap();
    assert!(
        !status
            .scopes
            .iter()
            .flat_map(|s| &s.attention)
            .any(|a| a.kind == "drafts-ahead"),
        "{status:?}"
    );
}

/// The stored version whose bytes ARE the edited copy (the snapshot) — the one `render_verified`
/// accepts against that copy's digest.
fn recovered_version(store: &topos_gitstore::Store, edited: &ScannedBundle) -> [u8; 32] {
    store
        .list_versions()
        .unwrap()
        .into_iter()
        .find(|v| store.render_verified(*v, edited.bundle_digest).is_ok())
        .expect("the snapshot is in the store")
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
