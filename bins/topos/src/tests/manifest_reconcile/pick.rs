//! The agents PICK is the one thing placement follows: topos never touches an agent the person
//! did not pick, whatever is installed; a project pick writes no agent file under the home; and a
//! `dest` folder the row names is placed as typed, whatever the pick says.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::ops;

use super::rig::*;

/// Every path under `root` (dirs and files), relative and sorted; empty for an absent root.
fn tree(root: &Path) -> Vec<String> {
    fn walk(base: &Path, d: &Path, out: &mut Vec<String>) {
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

/// The agent folders under a home a placement could reach — every one snapshotted, the machine
/// store (`.topos`) deliberately excluded: topos's own bookkeeping is not an agent surface.
const AGENT_DIRS: [&str; 7] = [
    ".claude",
    ".cursor",
    ".codex",
    ".gemini",
    ".config",
    ".agents",
    ".openclaw",
];

fn agent_trees(home: &Path) -> Vec<(String, Vec<String>)> {
    AGENT_DIRS
        .iter()
        .map(|d| ((*d).to_owned(), tree(&home.join(d))))
        .collect()
}

/// A rig whose workspace catalog offers `deploy`.
fn deploy_rig(tag: &str) -> (Rig, FakePlane, FakeDirectory) {
    let rig = Rig::new(tag);
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new()))).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    (rig, plane, dir)
}

/// I1: a pick of `{claude-code}` on a machine where Cursor and Codex are INSTALLED writes nothing
/// under `~/.cursor`, `~/.codex`, `.cursor/` or `.codex/` — not a folder, not a file.
#[test]
fn a_pick_of_claude_code_writes_nothing_under_cursor_or_codex_dirs() {
    let (rig, plane, dir) = deploy_rig("i1");
    for d in [".cursor", ".codex"] {
        std::fs::create_dir_all(rig.home.0.join(d)).unwrap();
    }
    rig.pick(&["claude-code"]);
    rig.write_global(&format!(
        "[skills]\n\"{HOST}/{WS_NAME}/deploy\" = \"latest\"\n"
    ));
    let before = agent_trees(&rig.home.0);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(
        rig.skills().join("deploy/SKILL.md").exists(),
        "the picked agent got its copy: {:?}",
        out.data.skills
    );
    let after = agent_trees(&rig.home.0);
    for ((name, was), (_, is)) in before.iter().zip(&after) {
        if name == ".claude" {
            continue; // the picked one
        }
        assert_eq!(was, is, "{name} is untouched");
    }

    // The same pick, in a project: the checkout's own `.cursor/` and `.codex/` stay empty.
    let proj = project(
        "i1-co",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    for d in [".cursor", ".codex"] {
        std::fs::create_dir_all(proj.0.join(d)).unwrap();
    }
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        proj.0.join(".claude/skills/deploy/SKILL.md").exists(),
        "{:?}",
        out.data.skills
    );
    for d in [".cursor", ".codex"] {
        assert!(tree(&proj.0.join(d)).is_empty(), "{d}/ is untouched");
    }
    assert!(!proj.0.join(".agents").exists());
}

/// I2: a PROJECT pick writes no agent file under the home — no skills folder, whatever is
/// installed there — even with no machine pick at all.
#[test]
fn a_project_pick_writes_nothing_under_home() {
    let (rig, plane, dir) = deploy_rig("i2");
    std::fs::remove_file(crate::agents_pick::machine_path(&rig.layout())).unwrap();
    for d in [
        ".cursor",
        ".codex",
        ".gemini",
        ".config/opencode",
        ".agents/skills",
    ] {
        std::fs::create_dir_all(rig.home.0.join(d)).unwrap();
    }
    let proj = project(
        "i2-co",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    rig.project_pick(&proj.0, &["claude-code", "cursor"]);
    let before = agent_trees(&rig.home.0);
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    for rel in [".claude/skills/deploy", ".cursor/skills/deploy"] {
        assert!(
            proj.0.join(rel).join("SKILL.md").exists(),
            "{rel}: {:?}",
            out.data.skills
        );
    }
    assert_eq!(
        agent_trees(&rig.home.0),
        before,
        "no agent file under the home"
    );
}

/// I5: an explicit `dest` folder is placed AS TYPED — an unpicked agent's folder included — and
/// a row that names its destinations places nowhere else.
#[test]
fn a_dest_row_is_placed_as_typed_for_an_unpicked_folder() {
    let (rig, plane, dir) = deploy_rig("i5");
    rig.pick(&["claude-code"]);
    rig.write_global(&format!(
        "[skills]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.cursor/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(
        rig.home.0.join(".cursor/skills/deploy/SKILL.md").exists(),
        "the typed folder, picked or not: {:?}",
        out.data.skills
    );
    assert!(
        !rig.skills().join("deploy").exists(),
        "a row naming its destinations places nowhere else"
    );
}
