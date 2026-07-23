//! The placement-engine breadth suite: the target-set matrix (unscoped/scoped × detected × coverage
//! provenance, the no-detection fallback), the v1→v2 `map.json` upgrade, draft-anywhere (one edited
//! copy is THE draft; several divergent copies freeze typed), the shared `--agent` verb fn
//! (`follow --agent` scope update; `unfollow --agent` == `remove --agent` on a followed skill), and
//! the unknown-slug refusal. All over a real fs + a temp fake `$HOME` — the developer's machine is
//! never probed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::persisted::PlacementKind;
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::{AgentRoots, Ctx};
use crate::enroll::{self, FollowEntry, FollowModeDoc};
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::placement::{self, AgentScope};
use crate::plane::{FollowSource, InertFollow, InertPlane};
use crate::plane_http::FileFollow;
use crate::sidecar::Layout;
use crate::{doc, ops};

const WS: &str = "w_acme";

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-plc-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Canonicalized so path comparisons against the engine's recorded placements (which
        // canonicalize their sources) hold on a symlinked temp root (macOS /var → /private/var).
        Self(dir.canonicalize().unwrap())
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A harness stub whose no-discovery placement is `<agent home>/.claude/skills/<id>` — the active
/// adapter's native dir in the classic fallback and the shared-dir-first plan alike.
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

/// The suite rig: a topos home, a work dir (adopt sources), and a FAKE agent home whose detect dirs
/// the test controls — detection sees exactly what each test creates.
struct Rig {
    home: Scratch,
    work: Scratch,
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
            work: Scratch::new(&format!("{tag}-work")),
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
    /// Mark a harness as detected on this rig's fake machine (its detect dir exists).
    fn detect(&self, dot_dir: &str) {
        std::fs::create_dir_all(self.agent_home.0.join(dot_dir)).unwrap();
    }
    fn ctx<'a>(&'a self, follow: &'a dyn FollowSource, plane: &'a InertPlane) -> Ctx<'a> {
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
    fn ctx_no_roots<'a>(&'a self, follow: &'a dyn FollowSource, plane: &'a InertPlane) -> Ctx<'a> {
        Ctx {
            roots: None,
            ..self.ctx(follow, plane)
        }
    }
    /// Adopt a skill from the work dir under `name` and mark it FOLLOWED in `follows.json`.
    fn adopt_followed(&self, name: &str) -> crate::id::SkillId {
        let dir = self.work.0.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("# {name}\nbody\n")).unwrap();
        let inert_p = InertPlane;
        let inert_f = InertFollow;
        let ctx = self.ctx_no_roots(&inert_f, &inert_p);
        let data = ops::add(&ctx, &dir).unwrap();
        enroll::write_follows_merged(
            &self.fs,
            &self.layout(),
            &[FollowEntry {
                skill_id: data.skill_id.clone(),
                workspace_id: WS.to_owned(),
                mode: FollowModeDoc::Auto,
                review_required: false,
                following: true,
                excluded_here: false,
                agents: Vec::new(),
                excluded_agents: Vec::new(),
            }],
        )
        .unwrap();
        crate::id::SkillId::parse(&data.skill_id).unwrap()
    }
    /// The on-disk follow seam (reads the entries this suite's verbs mutate).
    fn follow_seam(&self) -> FileFollow {
        let follows = enroll::read_follows(&self.fs, &self.layout())
            .unwrap()
            .unwrap();
        FileFollow::new(enroll::follow_contexts(&follows))
    }
    fn read_map(&self, sid: &crate::id::SkillId) -> topos_types::persisted::PlacementMap {
        doc::read_map(&self.fs, &self.layout().published(sid).map)
            .unwrap()
            .unwrap()
    }
}

fn scoped<'a>(agents: &'a [String], excluded: &'a [String]) -> AgentScope<'a> {
    AgentScope { agents, excluded }
}

// ---------------------------------------------------------------------------------------------
// The target-set matrix.
// ---------------------------------------------------------------------------------------------

#[test]
fn unscoped_plan_is_shared_dir_first_plus_uncovered_natives() {
    let rig = Rig::new("matrix-unscoped");
    // Detected: cline (covered per its registry row), cursor (no coverage evidence), openclaw
    // (covered — live-probed), codex (probed NOT covered).
    rig.detect(".cline");
    rig.detect(".cursor");
    rig.detect(".openclaw");
    rig.detect(".codex");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);

    let plan = placement::plan_targets(
        &ctx,
        "topos_matrix",
        topos_harness::PlacementNaming {
            name: Some("deploy"),
            workspace_slug: Some("acme"),
        },
        AgentScope::default(),
        None,
        None,
    );

    // ONE shared copy (cline + openclaw are covered) + native copies for cursor and codex (probed
    // NOT covered — the Probed(false) provenance wins over the wishful default).
    let shared: Vec<_> = plan
        .targets
        .iter()
        .filter(|t| t.kind == PlacementKind::Shared)
        .collect();
    assert_eq!(shared.len(), 1);
    assert_eq!(
        shared[0].dir,
        rig.agent_home
            .0
            .join(".agents")
            .join("skills")
            .join("deploy")
    );
    let native_agents: Vec<&str> = plan
        .targets
        .iter()
        .filter(|t| t.kind == PlacementKind::Native)
        .filter_map(|t| t.agent.as_deref())
        .collect();
    assert!(native_agents.contains(&"cursor"), "{native_agents:?}");
    assert!(native_agents.contains(&"codex"), "{native_agents:?}");
    assert!(
        !native_agents.contains(&"cline") && !native_agents.contains(&"openclaw"),
        "covered agents ride the shared copy, not a native one: {native_agents:?}"
    );
    // The coverage provenance is disclosed: both covers here are live-probed (cline's registry
    // docs-level claim is upgraded by its source-verified override).
    let covers: Vec<(&str, bool)> = plan
        .shared_covers
        .iter()
        .map(|c| (c.slug.as_str(), c.docs_level))
        .collect();
    assert!(covers.contains(&("cline", false)), "{covers:?}");
    assert!(covers.contains(&("openclaw", false)), "{covers:?}");
}

#[test]
fn scoped_plan_is_native_only_never_the_shared_dir() {
    let rig = Rig::new("matrix-scoped");
    rig.detect(".cline");
    rig.detect(".cursor");
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx(&inert_f, &inert_p);

    // An include-list of a COVERED agent still places natively (a shared dir cannot express narrowing).
    let include = vec!["cline".to_owned()];
    let plan = placement::plan_targets(
        &ctx,
        "topos_scoped",
        topos_harness::PlacementNaming {
            name: Some("deploy"),
            workspace_slug: None,
        },
        scoped(&include, &[]),
        None,
        None,
    );
    assert!(plan.targets.iter().all(|t| t.kind == PlacementKind::Native));
    assert_eq!(plan.targets.len(), 1);
    assert_eq!(plan.targets[0].agent.as_deref(), Some("cline"));
    // cline's native user dir IS the shared convention dir — recorded honestly as ITS native dir.
    assert_eq!(
        plan.targets[0].dir,
        rig.agent_home
            .0
            .join(".agents")
            .join("skills")
            .join("deploy")
    );

    // A per-agent EXCLUSION alone also forces native-only mode for the rest.
    let excluded = vec!["cursor".to_owned()];
    let plan = placement::plan_targets(
        &ctx,
        "topos_scoped",
        topos_harness::PlacementNaming {
            name: Some("deploy"),
            workspace_slug: None,
        },
        scoped(&[], &excluded),
        None,
        None,
    );
    assert!(plan.targets.iter().all(|t| t.kind == PlacementKind::Native));
    assert_eq!(
        plan.targets
            .iter()
            .filter_map(|t| t.agent.as_deref())
            .collect::<Vec<_>>(),
        vec!["cline"],
        "the excluded agent contributes no target"
    );

    // An include-list naming only an UNDETECTED (but known) agent yields no target — placement
    // engages when the harness is detected; nothing falls back to the active adapter.
    let include = vec!["windsurf".to_owned()];
    let plan = placement::plan_targets(
        &ctx,
        "topos_scoped",
        topos_harness::PlacementNaming {
            name: Some("deploy"),
            workspace_slug: None,
        },
        scoped(&include, &[]),
        None,
        None,
    );
    assert!(plan.targets.is_empty(), "{:?}", plan.targets);
}

#[test]
fn a_stale_classic_adoption_reservation_rechooses_a_fresh_dir() {
    // The no-detection path validates adoption reservations exactly like the detection path: a
    // recorded never-materialized placement whose occupant CHANGED since the adoption is dropped
    // from the plan and its key re-chooses fresh — returned verbatim it would wedge every apply on
    // the never-clobber refusal.
    let rig = Rig::new("classic-stale");
    let occupied = rig
        .agent_home
        .0
        .join(".claude")
        .join("skills")
        .join("deploy");
    std::fs::create_dir_all(&occupied).unwrap();
    std::fs::write(occupied.join("SKILL.md"), "# someone else's deploy\n").unwrap();
    let mk_prior = |pre_existing: &str| topos_types::persisted::PlacementMap {
        schema_version: topos_types::PLACEMENT_MAP_SCHEMA_VERSION,
        placements: vec![occupied.to_string_lossy().into_owned()],
        applied_commit: "0".repeat(64),
        materialized_sha: "0".repeat(64),
        pre_existing_sha: None,
        swap_capability: topos_types::persisted::SwapCapability::Unsupported,
        placement_state: vec![topos_types::persisted::PlacementState {
            kind: PlacementKind::Native,
            agent: Some("claude-code".to_owned()),
            materialized_sha: None,
            pre_existing_sha: Some(pre_existing.to_owned()),
            swap_capability: topos_types::persisted::SwapCapability::Unsupported,
        }],
        harness: None,
        harness_layer: None,
        harness_slug: None,
    };
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    let ctx = rig.ctx_no_roots(&inert_f, &inert_p);
    let naming = topos_harness::PlacementNaming {
        name: Some("deploy"),
        workspace_slug: Some("acme"),
    };

    // STALE: the recorded adoption digest no longer matches the occupant → re-choose (the by-name
    // dir is occupied, so the fresh choice is the workspace-suffixed sibling).
    let stale = mk_prior(&"a".repeat(64));
    let plan = placement::plan_targets(
        &ctx,
        "topos_stale",
        naming,
        AgentScope::default(),
        Some(&stale),
        None,
    );
    assert_eq!(plan.targets.len(), 1);
    assert_eq!(
        plan.targets[0].dir,
        rig.agent_home
            .0
            .join(".claude")
            .join("skills")
            .join("deploy-acme"),
        "the stale reservation is not reused"
    );

    // VALID: a reservation whose occupant still matches its recorded digest is kept verbatim.
    let digest = crate::scan::scan(&occupied).unwrap().bundle_digest;
    let valid = mk_prior(&topos_core::digest::to_hex(&digest));
    let plan = placement::plan_targets(
        &ctx,
        "topos_stale",
        naming,
        AgentScope::default(),
        Some(&valid),
        None,
    );
    assert_eq!(plan.targets.len(), 1);
    assert_eq!(plan.targets[0].dir, occupied);
}

#[test]
fn a_scope_replan_adopts_a_byte_identical_native_occupant() {
    // FIX territory: scoping to an agent whose native dir ALREADY holds a byte-identical copy of
    // the landed version adopts that dir — never a namespaced (or id-named) duplicate — and the
    // adoption heals to a materialized placement so later updates ride the normal rail.
    let rig = Rig::new("scope-adopt");
    let sid = rig.adopt_followed("deploy");
    rig.detect(".cursor");
    let cursor = rig
        .agent_home
        .0
        .join(".cursor")
        .join("skills")
        .join("deploy");
    std::fs::create_dir_all(&cursor).unwrap();
    std::fs::write(cursor.join("SKILL.md"), "# deploy\nbody\n").unwrap(); // byte-identical

    let follow = rig.follow_seam();
    let inert_p = InertPlane;
    let ctx = rig.ctx(&follow, &inert_p);
    let out = ops::set_scope(
        &ctx,
        &["deploy".to_owned()],
        &["cursor".to_owned()],
        None,
        true,
    )
    .unwrap();
    assert!(matches!(out, ops::AgentScopeOutcome::Applied(_)));

    let map = rig.read_map(&sid);
    let i = map
        .placements
        .iter()
        .position(|p| Path::new(p) == cursor)
        .expect("the identical occupant IS the placement");
    let skills_root = rig.agent_home.0.join(".cursor").join("skills");
    let entries: Vec<String> = std::fs::read_dir(&skills_root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        entries,
        vec!["deploy".to_owned()],
        "no namespaced or id-named duplicate lands beside the adopted copy"
    );
    // Healed to materialized: the record advanced with no swap, the occupant byte-untouched.
    let lock: topos_types::persisted::Lock =
        doc::read_doc(&rig.fs, &rig.layout().published(&sid).lock)
            .unwrap()
            .unwrap();
    assert_eq!(
        map.placement_state[i].materialized_sha.as_deref(),
        Some(lock.bundle_digest.as_str())
    );
    assert_eq!(
        std::fs::read_to_string(cursor.join("SKILL.md")).unwrap(),
        "# deploy\nbody\n"
    );
}

#[test]
fn no_detection_keeps_the_classic_single_placement() {
    let rig = Rig::new("matrix-classic");
    // No detect dirs at all — and also the roots-absent form (a test ctx / no $HOME).
    let inert_f = InertFollow;
    let inert_p = InertPlane;
    for ctx in [
        rig.ctx(&inert_f, &inert_p),
        rig.ctx_no_roots(&inert_f, &inert_p),
    ] {
        let plan = placement::plan_targets(
            &ctx,
            "topos_classic",
            topos_harness::PlacementNaming {
                name: Some("deploy"),
                workspace_slug: None,
            },
            AgentScope::default(),
            None,
            None,
        );
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].kind, PlacementKind::Native);
        assert_eq!(
            plan.targets[0].dir,
            rig.agent_home
                .0
                .join(".claude")
                .join("skills")
                .join("deploy"),
            "the active adapter's placement — today's behavior"
        );
        assert!(plan.shared_covers.is_empty());
    }
}

#[test]
fn an_adopted_agentless_placement_is_always_managed() {
    // The author's adopt-in-place dir (kind native, no agent slug) stays a managed target even when
    // detection fans placement out — updates must keep landing in the author's own working dir.
    let rig = Rig::new("matrix-adopted");
    rig.detect(".cline");
    let inert_p = InertPlane;
    let sid = rig.adopt_followed("deploy");
    let map = rig.read_map(&sid);
    let follow = rig.follow_seam();
    let ctx = rig.ctx(&follow, &inert_p);
    let lock: topos_types::persisted::Lock =
        doc::read_doc(&rig.fs, &rig.layout().published(&sid).lock)
            .unwrap()
            .unwrap();
    let plan = placement::plan_for_skill(&ctx, sid.as_str(), &lock, &map);
    let adopted_dir = rig.work.0.join("deploy");
    assert!(
        plan.targets.iter().any(|t| t.dir == adopted_dir),
        "the adopted source dir stays managed: {:?}",
        plan.targets
    );
    assert!(
        plan.targets.iter().any(|t| t.kind == PlacementKind::Shared),
        "the shared copy is planned alongside it"
    );

    // A purely-LOCAL skill (tracked, never followed) never fans out: its recorded placement is the
    // user's own working location, and nothing distributes it.
    let inert_f = InertFollow;
    let local_ctx = rig.ctx(&inert_f, &inert_p);
    let local_plan = placement::plan_for_skill(&local_ctx, sid.as_str(), &lock, &map);
    assert!(
        local_plan
            .targets
            .iter()
            .all(|t| t.kind == PlacementKind::Native),
        "{:?}",
        local_plan.targets
    );
    assert_eq!(local_plan.targets.len(), map.placements.len());
}

// ---------------------------------------------------------------------------------------------
// The v1 → v2 map upgrade.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_v1_map_upgrades_losslessly_in_memory_and_rewrites_as_v2() {
    let rig = Rig::new("upgrade");
    let path = rig.home.0.join("map.json");
    let sha = "ab".repeat(32);
    let v1 = format!(
        r#"{{
  "schema_version": 1,
  "placements": ["/home/u/.claude/skills/deploy"],
  "applied_commit": "{sha}",
  "materialized_sha": "{sha}",
  "swap_capability": "atomic_exchange",
  "harness": "claude-code",
  "harness_layer": "user",
  "harness_slug": "claude-code"
}}
"#
    );
    std::fs::write(&path, v1).unwrap();

    let map = doc::read_map(&rig.fs, &path).unwrap().unwrap();
    // Lossless: the single placement's state is synthesized from the map-level fields.
    assert_eq!(map.placement_state.len(), 1);
    let st = &map.placement_state[0];
    assert_eq!(st.kind, PlacementKind::Native);
    assert_eq!(st.agent.as_deref(), Some("claude-code"));
    assert_eq!(st.materialized_sha.as_deref(), Some(sha.as_str()));
    assert_eq!(
        st.swap_capability,
        topos_types::persisted::SwapCapability::AtomicExchange
    );
    // A write re-emits the doc at ITS schema ceiling.
    doc::write_map(&rig.fs, &path, &map).unwrap();
    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        raw["schema_version"],
        serde_json::json!(topos_types::PLACEMENT_MAP_SCHEMA_VERSION)
    );

    // A v1 baseline (all-zero materialized sha) upgrades to a NEVER-MATERIALIZED state.
    let zero = "0".repeat(64);
    let baseline = format!(
        r#"{{
  "schema_version": 1,
  "placements": ["/home/u/.claude/skills/deploy"],
  "applied_commit": "{zero}",
  "materialized_sha": "{zero}",
  "swap_capability": "unsupported"
}}
"#
    );
    std::fs::write(&path, baseline).unwrap();
    let map = doc::read_map(&rig.fs, &path).unwrap().unwrap();
    assert!(map.placement_state[0].materialized_sha.is_none());
}

// ---------------------------------------------------------------------------------------------
// Draft-anywhere.
// ---------------------------------------------------------------------------------------------

/// Give a followed skill a SECOND managed placement by scoping the machine: detect an agent and run
/// the scope-change reconcile so the record fans out. Returns the two dirs.
fn fan_out(rig: &Rig, sid: &crate::id::SkillId) -> (PathBuf, PathBuf) {
    rig.detect(".cursor");
    let follow = rig.follow_seam();
    let inert_p = InertPlane;
    let ctx = rig.ctx(&follow, &inert_p);
    let lock: topos_types::persisted::Lock =
        doc::read_doc(&rig.fs, &rig.layout().published(sid).lock)
            .unwrap()
            .unwrap();
    // The unscoped scope-change reconcile appends + converges the cursor native dir.
    ops::apply_scope_change(&ctx, sid, &lock, AgentScope::default()).unwrap();
    let map = rig.read_map(sid);
    assert_eq!(map.placements.len(), 2, "{:?}", map.placements);
    let adopted = rig.work.0.join("deploy");
    let cursor = map
        .placements
        .iter()
        .find(|p| Path::new(p) != adopted)
        .map(PathBuf::from)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(cursor.join("SKILL.md")).unwrap(),
        "# deploy\nbody\n"
    );
    (adopted, cursor)
}

#[test]
fn one_edited_copy_anywhere_is_the_draft_and_two_divergent_copies_freeze() {
    let rig = Rig::new("draft-anywhere");
    let sid = rig.adopt_followed("deploy");
    let (adopted, cursor) = fan_out(&rig, &sid);
    let follow = rig.follow_seam();
    let inert_p = InertPlane;
    let ctx = rig.ctx(&follow, &inert_p);
    let lock: topos_types::persisted::Lock =
        doc::read_doc(&rig.fs, &rig.layout().published(&sid).lock)
            .unwrap()
            .unwrap();

    // Edit the NON-adopted copy: the work tree points at THAT dir (draft-anywhere).
    std::fs::write(cursor.join("SKILL.md"), "# deploy\nedited in cursor\n").unwrap();
    let map = rig.read_map(&sid);
    let wt = placement::work_tree_dir(&ctx, &lock.name, &map).unwrap();
    assert_eq!(wt, cursor);

    // A byte-identical second edit is still ONE logical draft (no freeze).
    std::fs::write(adopted.join("SKILL.md"), "# deploy\nedited in cursor\n").unwrap();
    assert_eq!(
        placement::work_tree_dir(&ctx, &lock.name, &map).unwrap(),
        adopted.clone()
    );

    // DIVERGENT edits freeze typed: nothing overwritten, every edited path disclosed, reset named.
    std::fs::write(adopted.join("SKILL.md"), "# deploy\na DIFFERENT edit\n").unwrap();
    let err = placement::work_tree_dir(&ctx, &lock.name, &map).unwrap_err();
    assert_eq!(err.code(), "PLACEMENTS_DIVERGED");
    let msg = err.to_string();
    assert!(
        msg.contains(&adopted.display().to_string()) && msg.contains(&cursor.display().to_string()),
        "per-path disclosure: {msg}"
    );
    assert!(
        msg.contains("update deploy --reset"),
        "the way out is named: {msg}"
    );
    // The freeze reached the sweep surface too — and neither copy was touched.
    assert_eq!(
        std::fs::read_to_string(adopted.join("SKILL.md")).unwrap(),
        "# deploy\na DIFFERENT edit\n"
    );
    assert_eq!(
        std::fs::read_to_string(cursor.join("SKILL.md")).unwrap(),
        "# deploy\nedited in cursor\n"
    );
}

// ---------------------------------------------------------------------------------------------
// The shared `--agent` verb fn.
// ---------------------------------------------------------------------------------------------

#[test]
fn unknown_agent_slugs_refuse_naming_the_valid_ones() {
    let rig = Rig::new("unknown-slug");
    let _sid = rig.adopt_followed("deploy");
    let follow = rig.follow_seam();
    let inert_p = InertPlane;
    let ctx = rig.ctx(&follow, &inert_p);
    let err = ops::exclude_agents(
        &ctx,
        "unfollow",
        &["deploy".to_owned()],
        &["not-a-real-agent".to_owned()],
        None,
        false,
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    let msg = err.to_string();
    assert!(
        msg.contains("cursor") && msg.contains("claude-code") && msg.contains("openclaw"),
        "the registry's valid slugs are named: {msg}"
    );
}

#[test]
fn exclude_agents_cleans_exactly_that_agents_dir_and_records_the_exclusion() {
    let rig = Rig::new("exclude");
    let sid = rig.adopt_followed("deploy");
    let (adopted, cursor) = fan_out(&rig, &sid);
    // Edit the leaving copy first — the clean must snapshot it (never a lost byte).
    std::fs::write(
        cursor.join("SKILL.md"),
        "# deploy\nan edit about to leave\n",
    )
    .unwrap();

    let follow = rig.follow_seam();
    let inert_p = InertPlane;
    let ctx = rig.ctx(&follow, &inert_p);

    // DESCRIBE first: nothing mutates.
    let out = ops::exclude_agents(
        &ctx,
        "unfollow",
        &["deploy".to_owned()],
        &["cursor".to_owned()],
        None,
        false,
    )
    .unwrap();
    let ops::AgentScopeOutcome::Described { data, yes_argv } = out else {
        panic!("expected a describe");
    };
    assert!(!data.applied);
    assert_eq!(data.items[0].cleaned, vec![cursor.display().to_string()]);
    assert!(yes_argv.contains(&"--agent".to_owned()) && yes_argv.contains(&"--yes".to_owned()));
    assert!(cursor.exists(), "the describe cleans nothing");

    // APPLY: the cursor dir is cleaned; the adopted dir stays; the exclusion is durable.
    let out = ops::exclude_agents(
        &ctx,
        "unfollow",
        &["deploy".to_owned()],
        &["cursor".to_owned()],
        None,
        true,
    )
    .unwrap();
    assert!(matches!(out, ops::AgentScopeOutcome::Applied(_)));
    assert!(!cursor.exists(), "the excluded agent's dir is cleaned");
    assert!(adopted.exists(), "the adopted working copy stays");
    let follows = enroll::read_follows(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert_eq!(
        follows.follows[0].excluded_agents,
        vec!["cursor".to_owned()]
    );
    let map = rig.read_map(&sid);
    assert!(
        !map.placements.iter().any(|p| Path::new(p) == cursor),
        "the record dropped the cleaned placement"
    );
    // The edit that was in the leaving dir is retained in the sidecar store (snapshot-first).
    let store = topos_gitstore::Store::open(&rig.layout().published(&sid).store).unwrap();
    assert!(
        store.list_versions().unwrap().len() >= 2,
        "genesis + the snapshot"
    );
}

#[test]
fn remove_agent_routes_through_the_same_exclusion_fn() {
    let rig = Rig::new("remove-agent");
    let sid = rig.adopt_followed("deploy");
    let (_adopted, cursor) = fan_out(&rig, &sid);
    let follow = rig.follow_seam();
    let inert_p = InertPlane;
    let ctx = rig.ctx(&follow, &inert_p);

    // `remove deploy --agent cursor --yes` — ONE implementation with `unfollow --agent` (the verbs
    // alias it), so the observable outcome is identical: exclusion recorded, dir cleaned, record shrunk.
    let dir_connect = |_: &str| -> Box<dyn crate::plane::DirectorySource> {
        unreachable!("the per-agent exclusion is offline — no directory transport is ever built")
    };
    let connectors = ops::RemoveConnectors {
        directory: &dir_connect,
    };
    let out = ops::remove(
        &ctx,
        &connectors,
        &["deploy".to_owned()],
        &["cursor".to_owned()],
        None,
        true,
    )
    .unwrap();
    assert!(matches!(
        out,
        ops::RemoveOutcome::AgentScope(ops::AgentScopeOutcome::Applied(_))
    ));
    assert!(!cursor.exists());
    let follows = enroll::read_follows(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert_eq!(
        follows.follows[0].excluded_agents,
        vec!["cursor".to_owned()]
    );
}

#[test]
fn follow_agent_scope_update_narrows_then_star_clears_back_to_unscoped() {
    let rig = Rig::new("scope-update");
    let sid = rig.adopt_followed("deploy");
    let (adopted, cursor) = fan_out(&rig, &sid);
    rig.detect(".cline"); // a covered agent appears — relevant on the '*' return to unscoped

    // NARROW to cursor only. The adopted (agent-less) working dir is never scope-cleaned.
    {
        let follow = rig.follow_seam();
        let inert_p = InertPlane;
        let ctx = rig.ctx(&follow, &inert_p);
        let out = ops::set_scope(
            &ctx,
            &["deploy".to_owned()],
            &["cursor".to_owned()],
            None,
            true,
        )
        .unwrap();
        assert!(matches!(out, ops::AgentScopeOutcome::Applied(_)));
    }
    let follows = enroll::read_follows(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert_eq!(follows.follows[0].agents, vec!["cursor".to_owned()]);
    assert!(cursor.exists() && adopted.exists());

    // '*' CLEARS the include-list back to unscoped — the shared copy lands (cline is covered).
    {
        let follow = rig.follow_seam();
        let inert_p = InertPlane;
        let ctx = rig.ctx(&follow, &inert_p);
        let out =
            ops::set_scope(&ctx, &["deploy".to_owned()], &["*".to_owned()], None, true).unwrap();
        assert!(matches!(out, ops::AgentScopeOutcome::Applied(_)));
    }
    let follows = enroll::read_follows(&rig.fs, &rig.layout())
        .unwrap()
        .unwrap();
    assert!(follows.follows[0].agents.is_empty(), "unscoped again");
    let shared = rig
        .agent_home
        .0
        .join(".agents")
        .join("skills")
        .join("deploy");
    assert!(
        shared.exists(),
        "the shared copy landed on the unscoped return"
    );
    assert_eq!(
        std::fs::read_to_string(shared.join("SKILL.md")).unwrap(),
        "# deploy\nbody\n"
    );
}

#[test]
fn a_divergent_freeze_surfaces_from_the_sweep_and_reset_is_the_way_out() {
    let rig = Rig::new("freeze-reset");
    let sid = rig.adopt_followed("deploy");
    let (adopted, cursor) = fan_out(&rig, &sid);
    std::fs::write(adopted.join("SKILL.md"), "# deploy\nedit A\n").unwrap();
    std::fs::write(cursor.join("SKILL.md"), "# deploy\nedit B\n").unwrap();

    let follow = rig.follow_seam();
    let inert_p = InertPlane;
    let ctx = rig.ctx(&follow, &inert_p);

    // `update --reset deploy --yes` snapshots BOTH edits, then re-materializes base everywhere.
    let out = ops::reset(&ctx, &["deploy".to_owned()], true).unwrap();
    assert!(matches!(out, ops::ResetOutcome::Applied(_)));
    for dir in [&adopted, &cursor] {
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            "# deploy\nbody\n",
            "{}",
            dir.display()
        );
    }
    // Both distinct edits are retained in the store (genesis + 2 snapshots).
    let store = topos_gitstore::Store::open(&rig.layout().published(&sid).store).unwrap();
    assert!(store.list_versions().unwrap().len() >= 3);
}

#[test]
fn withdrawal_cleanup_never_deletes_a_foreign_occupied_target() {
    // A placement can be RECORDED but never materialized (a newly planned target awaiting its
    // first apply) — and someone else may occupy that dir before topos ever writes it. Those
    // bytes were never snapshotted and were never ours: the whole-skill clean (an upstream
    // withdrawal / a device exclusion) must leave them in place, exactly like the scope-change
    // cleanup does.
    let rig = Rig::new("withdraw-foreign");
    let sid = rig.adopt_followed("deploy");
    let (adopted, cursor) = fan_out(&rig, &sid);

    // Append a recorded-but-never-materialized target…
    let foreign_dir = rig
        .agent_home
        .0
        .join(".factory")
        .join("skills")
        .join("deploy");
    let mut map = rig.read_map(&sid);
    map.placements.push(foreign_dir.display().to_string());
    map.placement_state
        .push(topos_types::persisted::PlacementState {
            kind: topos_types::persisted::PlacementKind::Native,
            agent: Some("droid".to_owned()),
            materialized_sha: None,
            pre_existing_sha: None,
            swap_capability: topos_types::persisted::SwapCapability::Unsupported,
        });
    doc::write_map(&rig.fs, &rig.layout().published(&sid).map, &map).unwrap();
    // …that a foreign write occupies before topos's first apply.
    std::fs::create_dir_all(&foreign_dir).unwrap();
    std::fs::write(foreign_dir.join("SKILL.md"), "# someone else's bytes\n").unwrap();

    let follow = rig.follow_seam();
    let inert_p = InertPlane;
    let ctx = rig.ctx(&follow, &inert_p);
    ops::snapshot_and_clean(&ctx, &sid, ops::WithdrawReason::RemoveExclusion).unwrap();

    assert!(
        foreign_dir.join("SKILL.md").exists(),
        "a foreign-occupied, never-materialized target is not ours to delete"
    );
    assert_eq!(
        std::fs::read_to_string(foreign_dir.join("SKILL.md")).unwrap(),
        "# someone else's bytes\n"
    );
    assert!(!adopted.exists(), "the managed adopted copy was cleaned");
    assert!(!cursor.exists(), "the managed native copy was cleaned");
}

// ---------------------------------------------------------------------------------------------
// The drift-scan stat cache (`crate::stat_cache`) — equivalence with the cache off, and swap
// invalidation via the recorded materialized-sha basis.
// ---------------------------------------------------------------------------------------------

/// The clean-vs-modified verdict is identical with the cache ON and OFF — the cache only spares
/// reads, never the verdict (the `TOPOS_NO_STAT_CACHE` kill switch is this equivalence in prod).
#[test]
fn stat_cache_verdicts_match_with_the_cache_on_or_off() {
    let rig = Rig::new("cache-eq");
    let sid = rig.adopt_followed("deploy");
    let follow = rig.follow_seam();
    let inert_p = InertPlane;
    let ctx = rig.ctx(&follow, &inert_p);
    let map = rig.read_map(&sid);

    // A clean placement reads clean in BOTH modes (the cache-on run also warms the cache).
    for on in [false, true, true] {
        let scans = placement::scan_placements_cached(&ctx, &map, on).unwrap();
        assert!(
            matches!(scans[0].status, placement::ScanStatus::Clean { .. }),
            "clean verdict must hold with cache_on={on}"
        );
    }

    // Edit the placement (a different size, so the change can never hide) — Modified in BOTH modes,
    // and the warm cache (populated above at the OLD bytes) still catches it.
    let adopted = rig.work.0.join("deploy");
    std::fs::write(
        adopted.join("SKILL.md"),
        "# deploy\nan edit of a new length\n",
    )
    .unwrap();
    for on in [false, true] {
        let scans = placement::scan_placements_cached(&ctx, &map, on).unwrap();
        match &scans[0].status {
            placement::ScanStatus::Modified { scanned } => {
                assert!(!scanned.files.is_empty(), "Modified must carry full bytes");
            }
            _ => panic!("expected Modified with cache_on={on}"),
        }
    }
}

/// A directory swap (new bytes AND an advanced recorded materialized sha) invalidates the bucket
/// through the `basis` mismatch: the stale rows are dropped, the dir re-hashed to the new version,
/// and the bucket's generation bumps — the visible marker the swap hook fired.
#[test]
fn stat_cache_swap_invalidation_rebuilds_and_bumps_the_generation() {
    let rig = Rig::new("cache-swap");
    let sid = rig.adopt_followed("deploy");
    let follow = rig.follow_seam();
    let inert_p = InertPlane;
    let ctx = rig.ctx(&follow, &inert_p);
    let mut map = rig.read_map(&sid);
    let dir = map.placements[0].clone();

    // Warm the cache at version A.
    let scans = placement::scan_placements_cached(&ctx, &map, true).unwrap();
    assert!(matches!(
        scans[0].status,
        placement::ScanStatus::Clean { .. }
    ));
    let cache_a = crate::stat_cache::load(&rig.fs, &rig.layout());
    let bucket_a = &cache_a.placements[&dir];
    let gen_a = bucket_a.generation;
    let basis_a = bucket_a.basis.clone().unwrap();

    // Simulate the materialize dir-swap: NEW bytes on disk, and the recorded materialized sha moves.
    std::fs::write(
        Path::new(&dir).join("SKILL.md"),
        "# deploy\nversion B bytes\n",
    )
    .unwrap();
    let digest_b =
        topos_core::digest::to_hex(&crate::scan::scan(Path::new(&dir)).unwrap().bundle_digest);
    assert_ne!(basis_a, digest_b, "the swap must move the basis");
    map.placement_state[0].materialized_sha = Some(digest_b.clone());
    map.materialized_sha = digest_b.clone();

    // The new bytes match the new recorded sha → Clean at B (A's rows were invalidated, not trusted).
    let scans = placement::scan_placements_cached(&ctx, &map, true).unwrap();
    assert!(matches!(
        scans[0].status,
        placement::ScanStatus::Clean { .. }
    ));
    let cache_b = crate::stat_cache::load(&rig.fs, &rig.layout());
    let bucket_b = &cache_b.placements[&dir];
    assert_eq!(bucket_b.basis.as_deref(), Some(digest_b.as_str()));
    assert!(
        bucket_b.generation > gen_a,
        "the swap invalidation must bump the generation ({} !> {})",
        bucket_b.generation,
        gen_a
    );
}
