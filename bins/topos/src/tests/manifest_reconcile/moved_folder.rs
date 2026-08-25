//! **An agent's skills FOLDER moved.** Codex reads `<repo>/.codex/skills`, so that is where its
//! project copies go — one folder per agent, beside its hook and its MCP file, like every other
//! row. A project set up by an older topos has that copy in the cross-agent `.agents/skills`
//! instead, and the copy MOVES: the reconcile lands the new folder and retires the old recorded
//! one in the same run, so nothing stale stands and Codex — which reads both folders and does not
//! deduplicate — never lists the bundle twice.
//!
//! The two limits are here too: a folder ANOTHER picked agent still reads stays, and a folder
//! holding anything topos did not put there is never removed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use topos_types::persisted::{PlacementMap, PlacementState};

use super::rig::*;

/// A rig plus a checkout demanding `deploy`, with `agents` picked in the checkout.
fn checkout(tag: &str, agents: &[&str]) -> (Rig, FakePlane, FakeDirectory, Scratch) {
    let rig = Rig::new(tag);
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new()))).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let proj = project(
        &format!("{tag}-co"),
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    rig.project_pick(&proj.0, agents);
    (rig, plane, dir, proj)
}

/// The checkout's record for `deploy`.
fn map_of(rig: &Rig, proj: &Path) -> PlacementMap {
    let layout = crate::sidecar::existing_project_store(&rig.fs, proj).expect("a project store");
    crate::doc::read_map(&rig.fs, &layout.published(&sid("s_deploy")).map)
        .unwrap()
        .unwrap()
}

fn write_map(rig: &Rig, proj: &Path, map: &PlacementMap) {
    let layout = crate::sidecar::existing_project_store(&rig.fs, proj).expect("a project store");
    crate::doc::write_map(&rig.fs, &layout.published(&sid("s_deploy")).map, map).unwrap();
}

/// Put the checkout back the way an OLDER topos left it: the copy `agent` holds sits in the
/// cross-agent `.agents/skills` and the record says so. Returns the old dir.
fn rewind_to_shared_folder(rig: &Rig, proj: &Path, agent: &str) -> PathBuf {
    let mut map = map_of(rig, proj);
    let i = map
        .placement_state
        .iter()
        .position(|st| st.agent.as_deref() == Some(agent))
        .expect("a recorded copy for that agent");
    let old = proj.join(".agents/skills/deploy");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::rename(&map.placements[i], &old).unwrap();
    map.placements[i] = old.to_string_lossy().into_owned();
    write_map(rig, proj, &map);
    old
}

/// Codex's project copies land in `.codex/skills` — its own folder, beside its hook and its MCP
/// file — and topos creates no `.agents/` for it at all.
#[test]
fn codex_project_skills_land_in_dot_codex() {
    let (rig, plane, dir, proj) = checkout("codex-folder", &["codex"]);
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(
        std::fs::read(proj.0.join(".codex/skills/deploy/SKILL.md")).unwrap(),
        b"# deploy\n",
        "{:?}",
        out.data.skills
    );
    assert!(
        !proj.0.join(".agents").exists(),
        "nothing is written to the cross-agent folder"
    );
    let map = map_of(&rig, &proj.0);
    assert_eq!(
        map.placements,
        vec![
            proj.0
                .join(".codex/skills/deploy")
                .to_string_lossy()
                .into_owned()
        ]
    );
}

/// An older project's copy MOVES: one converge lands `.codex/skills` and retires the recorded
/// `.agents/skills` copy — the folder included, since topos put everything in it — so Codex,
/// which reads both, is left with exactly one.
#[test]
fn an_older_projects_codex_copy_moves_out_of_dot_agents() {
    let (rig, plane, dir, proj) = checkout("codex-move", &["codex"]);
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep(&ctx, &plane, &dir);
    let old = rewind_to_shared_folder(&rig, &proj.0, "codex");
    assert!(old.join("SKILL.md").exists(), "the premise: the old copy");
    assert!(!proj.0.join(".codex/skills/deploy").exists());

    let out = sweep(&ctx, &plane, &dir);

    let moved = proj.0.join(".codex/skills/deploy");
    assert_eq!(
        std::fs::read(moved.join("SKILL.md")).unwrap(),
        b"# deploy\n",
        "the bundle stands in the folder Codex reads: {:?}",
        out.data.skills
    );
    assert!(!old.exists(), "the old copy is gone");
    assert!(
        !proj.0.join(".agents").exists(),
        "and so is the folder topos made for it"
    );
    let map = map_of(&rig, &proj.0);
    assert_eq!(
        map.placements,
        vec![moved.to_string_lossy().into_owned()],
        "one copy on record, never two"
    );
}

/// The folder stays for whoever still reads it. With Codex and an agent whose row still names
/// `.agents/skills` both picked, the shared copy is that agent's from here on — Codex's own copy
/// moves, and nothing is deleted.
#[test]
fn a_shared_agents_folder_stays_for_another_picked_agent() {
    let (rig, plane, dir, proj) = checkout("codex-shared", &["codex", "gemini-cli"]);
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep(&ctx, &plane, &dir);
    // The older world: ONE copy in the folder both agents read, recorded for Codex (the first
    // picked row to claim it), and nothing under `.codex/`.
    let shared = proj.0.join(".agents/skills/deploy");
    let mut map = map_of(&rig, &proj.0);
    let keep = map
        .placement_state
        .iter()
        .position(|st| st.agent.as_deref() == Some("gemini-cli"))
        .expect("gemini-cli's copy");
    let drop_dir = PathBuf::from(&map.placements[1 - keep]);
    std::fs::remove_dir_all(&drop_dir).unwrap();
    map.placements = vec![map.placements[keep].clone()];
    map.placement_state = vec![PlacementState {
        agent: Some("codex".to_owned()),
        ..map.placement_state[keep].clone()
    }];
    write_map(&rig, &proj.0, &map);
    assert_eq!(map.placements, vec![shared.to_string_lossy().into_owned()]);

    let out = sweep(&ctx, &plane, &dir);

    assert_eq!(
        std::fs::read(shared.join("SKILL.md")).unwrap(),
        b"# deploy\n",
        "the folder the other picked agent reads is untouched: {:?}",
        out.data.skills
    );
    assert_eq!(
        std::fs::read(proj.0.join(".codex/skills/deploy/SKILL.md")).unwrap(),
        b"# deploy\n",
        "and Codex has its own"
    );
}

/// A `.agents/skills` holding anything topos did not put there is never removed. Codex's own copy
/// leaves it; everything else stays exactly as it was.
#[test]
fn a_foreign_file_in_dot_agents_is_never_deleted() {
    let (rig, plane, dir, proj) = checkout("codex-foreign", &["codex"]);
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep(&ctx, &plane, &dir);
    let old = rewind_to_shared_folder(&rig, &proj.0, "codex");
    let by_hand = proj.0.join(".agents/skills/written-by-hand");
    std::fs::create_dir_all(&by_hand).unwrap();
    std::fs::write(by_hand.join("SKILL.md"), b"# mine\n").unwrap();
    let note = proj.0.join(".agents/note.md");
    std::fs::write(&note, b"read me\n").unwrap();

    sweep(&ctx, &plane, &dir);

    assert!(!old.exists(), "topos's own copy left");
    assert_eq!(
        std::fs::read(by_hand.join("SKILL.md")).unwrap(),
        b"# mine\n",
        "a folder topos never wrote is untouched"
    );
    assert_eq!(std::fs::read(&note).unwrap(), b"read me\n");
    assert!(
        proj.0.join(".codex/skills/deploy/SKILL.md").exists(),
        "and Codex's copy landed"
    );
}
