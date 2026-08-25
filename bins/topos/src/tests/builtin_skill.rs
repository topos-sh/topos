//! The BUILT-IN `topos` skill suite: placement through the one engine (one copy per picked
//! agent, at the pick's own scope), the force-sync (a hand edit is overwritten, snapshot-first; a binary change
//! refreshes every copy), the Foreign freeze (the sweep never writes a pre-existing dir — marked
//! or not; only the consented `follow topos --yes` adopts a MARKED downloaded copy,
//! snapshot-first), the provenance matcher's fail-closed shapes, the durable `remove topos`
//! opt-out (+ `follow topos` back), the forward-compatible state doc, `list`'s `built-in` row, and
//! the end-to-end name reservation (`add`). All over a real fs + a temp fake `$HOME` — the
//! developer's machine is never probed.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use topos_core::digest::{self, FileMode, ManifestEntry};

use crate::ctx::{AgentRoots, Ctx};
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::ops;
use crate::plane::{InertFollow, InertPlane};
use crate::scan::{ScannedBundle, ScannedFile};
use crate::sidecar::Layout;
use crate::test_support::MockHarness;

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
fn stub_claude(agent_home: &std::path::Path) -> MockHarness {
    MockHarness::ladder(agent_home.join(".claude").join("skills"))
}

struct Rig {
    home: Scratch,
    agent_home: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: MockHarness,
}

impl Rig {
    fn new(tag: &str) -> Self {
        let agent_home = Scratch::new(&format!("{tag}-agents"));
        let harness = stub_claude(&agent_home.0);
        let rig = Self {
            home: Scratch::new(&format!("{tag}-home")),
            agent_home,
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1),
            harness,
        };
        rig.pick(&["*"]);
        rig
    }
    fn layout(&self) -> Layout {
        Layout::new(&self.home.0)
    }
    /// Replace this machine's agents pick.
    fn pick(&self, agents: &[&str]) {
        crate::agents_pick::write_pick(&crate::agents_pick::machine_path(&self.layout()), agents);
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
            triggers: crate::ops::Triggers::active_only(&crate::ops::INERT_TRIGGER),
            plane,
            follow,
            roots: Some(AgentRoots {
                home: self.agent_home.0.clone(),
                cwd: None,
            }),
        }
    }
    /// Every path (dirs and files) under the agent home, relative and sorted — the "nothing
    /// under the home moved" witness.
    fn agent_home_tree(&self) -> Vec<String> {
        tree(&self.agent_home.0)
    }
    /// Cline's placed copy: its user skills root is the cross-agent `~/.agents/skills` folder.
    fn shared_copy(&self) -> PathBuf {
        self.agent_home
            .0
            .join(".agents")
            .join("skills")
            .join("topos")
    }
}

/// Every path under `root` (dirs and files), relative and sorted; empty for an absent root.
fn tree(root: &std::path::Path) -> Vec<String> {
    fn walk(base: &std::path::Path, d: &std::path::Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            out.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
            if p.is_dir() {
                walk(base, &p, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn sid() -> crate::id::SkillId {
    crate::id::SkillId::parse("topos").unwrap()
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

/// The one paragraph an agent reads about a stopped merge — BEFORE it ever meets one, since these
/// bytes are compiled into the binary and placed in every agent's skills directory. It is asserted
/// as a whole and whitespace-normalized: the file hard-wraps its prose, so a wrap an editor moved
/// must not read as a change of copy, while a changed word must.
#[test]
fn the_built_in_teaches_the_whole_stopped_merge_loop() {
    let skill_md = include_str!("../../../../skills/topos/SKILL.md");
    let flat = skill_md.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains(
            "Updates never destroy drafts — they merge around them. Where you and the team \
             changed the same lines the update stops: every agent folder keeps your version \
             untouched (never markers), and a marked-up copy of both sides goes to a folder of \
             Topos's own, named on the receipt. Finish it with `topos update <name> --keep-mine` \
             — edit that folder first to commit your merge, or leave it alone to keep your \
             wording on the contested lines and take the team's other changes — or `topos update \
             <name> --reset` to take the team's version. Publishing is blocked until you pick \
             one. A settled draft is copied onto the skill's other copies in the same scope."
        ),
        "{flat}"
    );
    // The model this replaced is gone: nothing freezes, and the exit that finishes a stopped merge
    // is named rather than left for the reader to discover.
    assert!(!flat.contains("freezes the copy"), "{flat}");
}

/// The four DETAIL files the entry document defers to by name, paired with the committed source
/// the binary embeds. One list, so a fifth file cannot be added to the bundle and forgotten here.
fn detail_files() -> [(&'static str, &'static str); 4] {
    [
        (
            "distilling.md",
            include_str!("../../../../skills/topos/distilling.md"),
        ),
        (
            "manifest.md",
            include_str!("../../../../skills/topos/manifest.md"),
        ),
        ("mcp.md", include_str!("../../../../skills/topos/mcp.md")),
        (
            "team-setup.md",
            include_str!("../../../../skills/topos/team-setup.md"),
        ),
    ]
}

/// THE BUILT-IN FOLLOWS THE PICK AT ITS SCOPE. A project pick places it into the picked agents'
/// PROJECT skills dirs through the project's own store — every file, force-synced — and not a
/// byte lands under the home. Its custody is the project store's, so the same `placement_dirs`
/// the machine clean reads answers for the project copy.
#[test]
fn the_built_in_lands_in_the_picked_agents_project_dirs() {
    let rig = Rig::new("project-pick");
    rig.detect(".cline"); // installed and machine-picked; the project pick is what counts here
    let proj = Scratch::new("project-pick-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    crate::agents_pick::write_pick(
        &crate::agents_pick::project_path(&proj.0),
        &["claude-code", "codex"],
    );
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);
    let home_before = rig.agent_home_tree();

    let sync = ops::ensure_builtin_in_project(&ctx, &proj.0).unwrap();
    assert!(sync.changed, "first contact lands bytes");
    let copies = [
        proj.0.join(".claude/skills/topos"),
        proj.0.join(".agents/skills/topos"),
    ];
    for dir in &copies {
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            include_str!("../../../../skills/topos/SKILL.md"),
            "{}",
            dir.display()
        );
        for (name, source) in detail_files() {
            assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), source);
        }
        assert_eq!(
            std::fs::read(dir.join(".gitignore")).unwrap(),
            crate::scan::IGNORE_SENTINEL,
            "a project copy self-ignores"
        );
    }
    assert!(
        !proj.0.join(".cursor").exists(),
        "an unpicked agent gets no folder"
    );
    assert_eq!(
        rig.agent_home_tree(),
        home_before,
        "nothing under the home moved"
    );
    assert!(
        !rig.shared_copy().exists() && !rig.layout().skill_dir(&sid()).exists(),
        "no machine copy, no machine record"
    );
    // The record — and the opt-out — are the PROJECT store's.
    let store = crate::sidecar::existing_project_store(&rig.fs, &proj.0).expect("the store");
    assert!(store.published(&sid()).map.exists());
    let sctx = crate::ops::ctx_with_layout(&ctx, &store);
    let mut placed = ops::builtin_placement_dirs(&sctx).unwrap();
    placed.sort();
    let mut expected: Vec<String> = copies
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    expected.sort();
    assert_eq!(placed, expected);
    // A second run is a byte-silent no-op.
    assert!(
        !ops::ensure_builtin_in_project(&ctx, &proj.0)
            .unwrap()
            .changed
    );
}

/// With NO pick nothing is placed, machine or project: the record is minted, the plan is empty,
/// and no agent folder is born.
#[test]
fn the_built_in_places_nothing_with_no_pick() {
    let rig = Rig::new("no-pick");
    rig.pick(&[]);
    rig.detect(".cline");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);
    let home_before = rig.agent_home_tree();
    let sync = ops::ensure_builtin(&ctx).unwrap();
    assert!(!sync.changed);
    assert_eq!(rig.agent_home_tree(), home_before);
    assert!(
        rig.layout().skill_dir(&sid()).exists(),
        "the record stands, waiting for a pick"
    );
    let proj = Scratch::new("no-pick-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    let sync = ops::ensure_builtin_in_project(&ctx, &proj.0).unwrap();
    assert!(!sync.changed);
    assert!(!proj.0.join(".claude").exists() && !proj.0.join(".agents").exists());
}

#[test]
fn ensure_places_the_bundle_and_lists_it_as_built_in() {
    let rig = Rig::new("place");
    rig.detect(".cline"); // picked through the machine's wildcard; its folder is `~/.agents/skills`
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
    // The four DETAIL files SKILL.md defers to by name. Each must land, or the deferral
    // ("read `manifest.md` next to this file") sends the agent to a file that is not there.
    for (name, source) in detail_files() {
        assert_eq!(
            std::fs::read_to_string(shared.join(name)).unwrap(),
            source,
            "the placed {name} IS the committed top-level source"
        );
        assert!(
            skill_md.contains(&format!("`{name}`")),
            "SKILL.md sends the agent to {name} by name"
        );
    }

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

/// The four detail files ride the SAME machinery as SKILL.md, end to end: force-synced back when
/// one is deleted or hand-edited, and gone with the dir when the device opts out. They are the
/// bundle's own bytes, not attachments — a sweep that restored the entry document and left a
/// deleted `manifest.md` missing would leave every deferral in it dangling.
#[test]
fn the_detail_files_are_force_synced_and_go_with_the_opt_out() {
    let rig = Rig::new("details");
    rig.detect(".cline");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);
    ops::ensure_builtin(&ctx).unwrap();
    let shared = rig.shared_copy();

    // One deleted, one hand-edited — the two ways a placed file diverges.
    std::fs::remove_file(shared.join("manifest.md")).unwrap();
    std::fs::write(shared.join("mcp.md"), "# mine now\n").unwrap();

    let sync = ops::ensure_builtin(&ctx).unwrap();
    assert!(sync.changed, "the divergent copy is re-synced");
    for (name, source) in detail_files() {
        assert_eq!(
            std::fs::read_to_string(shared.join(name)).unwrap(),
            source,
            "{name} is force-synced back to the binary's bytes"
        );
    }

    // `topos remove topos --yes` — the durable opt-out takes the whole placed dir, detail files
    // included, and the next sweep re-places nothing.
    ops::builtin_remove(&ctx).unwrap();
    assert!(!shared.exists(), "the placed dir goes whole");
    let sync = ops::ensure_builtin(&ctx).unwrap();
    assert!(!sync.changed && !shared.exists(), "the opt-out is durable");

    // `topos add topos` is its literal inverse — every file comes back.
    let sync = ops::restore_builtin(&ctx).unwrap();
    assert!(sync.changed, "the restore lands bytes again");
    for (name, source) in detail_files() {
        assert_eq!(
            std::fs::read_to_string(shared.join(name)).unwrap(),
            source,
            "{name} comes back with the restore"
        );
    }
}

/// **`topos add topos` SAYS WHAT IT DID.** Every other `add` answer names the file it recorded
/// into; this one used to say "Restored the built-in `topos` skill on this machine" and name
/// nothing at all, leaving a person holding a receipt for an act with no visible trace — and
/// `topos status -g` one command later said nothing was demanded machine-wide. The receipt now
/// names the folders that took a copy and why no manifest line appeared.
#[test]
fn the_builtin_restores_receipt_names_the_folders_and_why_there_is_no_manifest_line() {
    let rig = Rig::new("restore-receipt");
    rig.detect(".cline");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);

    // THE FIRST PLACEMENT — bytes moved, and the folder they moved into is named.
    let sync = ops::restore_builtin(&ctx).unwrap();
    let folders = crate::ops::builtin_placement_dirs(&ctx).unwrap();
    assert!(sync.changed && !folders.is_empty(), "the restore places");
    let data = serde_json::json!({
        "restored": true,
        "changed": sync.changed,
        "folders": folders.clone(),
    });
    let text = crate::render::builtin_add_tty(&data);
    assert!(
        text.starts_with("Placed the built-in topos bundle on this machine:\n  "),
        "{text}"
    );
    for folder in &folders {
        assert!(text.contains(folder.trim_start_matches('/')), "{text}");
    }
    assert!(
        text.ends_with(
            "No manifest line records it — the bundle ships with the topos \
             binary.\nundo: topos remove topos --yes"
        ),
        "{text}"
    );
    // The dead copy is gone: it called the bundle a skill and named nothing.
    assert!(!text.contains("Restored the built-in"), "{text}");

    // A SECOND RUN changes nothing, and says that instead of claiming a placement.
    let sync = ops::restore_builtin(&ctx).unwrap();
    assert!(!sync.changed, "an already-placed built-in just re-syncs");
    let text = crate::render::builtin_add_tty(&serde_json::json!({
        "restored": true,
        "changed": false,
        "folders": folders,
    }));
    assert!(
        text.starts_with("The built-in topos bundle is already in place:\n  "),
        "{text}"
    );

    // NOTHING TOOK A COPY: no folder is named, no undo is offered for a copy that is not there,
    // and the receipt never claims a placement the filesystem does not bear out.
    let text = crate::render::builtin_add_tty(&serde_json::json!({
        "restored": true,
        "changed": true,
        "folders": Vec::<String>::new(),
    }));
    assert_eq!(
        text,
        "No agent folder on this machine took a copy of the built-in topos bundle.\nNo manifest \
         line records it — the bundle ships with the topos binary."
    );
}

/// **A state doc written by another binary still loads.** `state/builtin.json` is durable and
/// carries the ONE thing a person decided — the opt-out — so a file holding keys this shape no
/// longer has must read as the opt-out it recorded, never as a parse failure that bricks the sweep.
#[test]
fn a_state_doc_carrying_unknown_keys_still_loads_the_opt_out() {
    let rig = Rig::new("state-fwd");
    rig.detect(".cline");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);
    let path = rig.layout().builtin_state_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        format!(
            "{{\n  \"schema_version\": {},\n  \"removed\": true,\n  \"agents\": [\"claude-code\"],\
             \n  \"excluded_agents\": [\"cursor\"]\n}}\n",
            topos_types::PERSISTED_SCHEMA_VERSION
        ),
    )
    .unwrap();

    let sync = ops::ensure_builtin(&ctx).unwrap();
    assert!(
        !sync.changed && !rig.shared_copy().exists(),
        "the recorded opt-out is honored — the sweep re-places nothing"
    );
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
    let nosess = |_: &crate::sessions::Session| -> ops::SessionTransports {
        unreachable!("a local log builds no session transports")
    };
    let history = ops::log(
        &ctx,
        &ops::LogConnectors { session: &nosess },
        "topos",
        None,
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

    // …INCLUDING the dir topos's own MCP surface owns. A folder named `topos-mcp` would otherwise
    // adopt fine and land in the opaque-id dir, while the receipt printed `+ topos-mcp installed`
    // — a rename nobody asked for and nothing disclosed. An add is a question a person asked, so
    // it gets an answer.
    let mcp_dir = rig.agent_home.0.join("topos-mcp");
    std::fs::create_dir_all(&mcp_dir).unwrap();
    std::fs::write(mcp_dir.join("SKILL.md"), "# mine\n").unwrap();
    let err = ops::add(&ctx, &mcp_dir).expect_err("reserved");
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    let message = crate::render::safe_message(&err);
    assert!(
        message.contains("the name `topos-mcp` is reserved for topos's own MCP surface")
            && message.contains("--skill <name>"),
        "the refusal teaches which name is taken and how to adopt anyway: {message}"
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

/// THE BUILT-IN IS A BUNDLE TOO. It mints its own record — its own store init, its own docs — so
/// the "every bundle's store records what it is" invariant only holds if this path stamps the
/// marker as well. Without it every fresh home carried a `skills/topos/` with lock, map, store and
/// sync but no `kind.json`, and the built-in was the one record whose kind had to be inferred.
#[test]
fn the_built_in_record_carries_its_kind_marker() {
    let rig = Rig::new("builtin-kind");
    rig.detect(".cline");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);

    ops::ensure_builtin(&ctx).unwrap();

    let sid = crate::id::SkillId::parse("topos").unwrap();
    let marker = std::fs::read_to_string(rig.layout().published(&sid).kind)
        .expect("the built-in's store records what it is");
    assert!(marker.contains("\"skill\""), "{marker}");

    // And the ONE classifier answers from it, like every other record.
    assert_eq!(
        crate::bundle_kind::classify(&ctx, "topos", &[]),
        crate::bundle_kind::RecordKind::Known(crate::bundle_kind::BundleKind::Skill),
    );
}
