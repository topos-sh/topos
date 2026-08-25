//! The agents PICK is the one thing placement follows: topos never touches an agent the person
//! did not pick, whatever is installed; a project pick writes exactly ONE agent file under the
//! home and nothing else; and a `dest` folder the row names is placed as typed, whatever the pick
//! says.

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

    // The same pick, in a project: the checkout's own `.cursor/` and `.codex/` stay empty. The
    // pick is the checkout's own — the machine's never reaches it.
    let proj = project(
        "i1-co",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    rig.project_pick(&proj.0, &["claude-code"]);
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

/// I2: **a PROJECT pick writes exactly one thing under the home, and it is enumerated here.**
///
/// Every skills folder a checkout gets is in the checkout: no machine skills root is written,
/// whatever is installed there, and with no machine pick at all. The ONE exception is deliberate:
/// Claude Code keeps a checkout's own MCP servers in its own configuration file, under a slot
/// keyed by that checkout's path — there is no per-project file for them to go in. So the
/// allowance is that file by name, and the assertion below lists it rather than loosening.
#[test]
fn a_project_pick_writes_exactly_one_file_under_home() {
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
    // A SERVER as well as a skill, because the server is the one thing with somewhere to go under
    // the home: without it this would pass by never exercising the allowance.
    let dir = dir.with_server(catalog_server(
        "s_linear",
        "linear",
        "https://mcp.example/linear",
    ));
    let proj = project(
        "i2-co",
        &format!(
            "workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n\n[mcp]\nlinear = \
             \"latest\"\n"
        ),
    );
    rig.project_pick(&proj.0, &["claude-code", "cursor"]);
    let before = tree(&rig.home.0);
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    for rel in [".claude/skills/deploy", ".cursor/skills/deploy"] {
        assert!(
            proj.0.join(rel).join("SKILL.md").exists(),
            "{rel}: {:?}",
            out.data.skills
        );
    }
    // The server landed for the checkout, in the checkout's own slot.
    let claude = std::fs::read_to_string(rig.home.0.join(".claude.json")).expect("claude's config");
    let key = proj
        .0
        .canonicalize()
        .unwrap_or_else(|_| proj.0.clone())
        .to_string_lossy()
        .into_owned();
    assert!(
        claude.contains(&key) && claude.contains("linear"),
        "the checkout's own slot: {claude}"
    );

    // WHAT IS ALLOWED, enumerated: every path under the home that was not there before is either
    // topos's own store or that one file. No agent folder, no skills root, nothing else.
    let after = tree(&rig.home.0);
    let added: Vec<&String> = after.iter().filter(|p| !before.contains(p)).collect();
    let allowed = |p: &str| p == ".claude.json" || p == ".topos" || p.starts_with(".topos/");
    assert!(
        added.iter().all(|p| allowed(p)),
        "a project pick wrote something else under the home: {added:?}"
    );
    assert!(
        added.iter().any(|p| *p == ".claude.json"),
        "the allowance is exercised, not merely permitted: {added:?}"
    );
    assert_eq!(
        agent_trees(&rig.home.0),
        agent_trees_before(&before),
        "no agent FOLDER under the home moved"
    );
}

/// The agent-folder half of [`agent_trees`], recomputed from a whole-home snapshot — so I2 can
/// take one snapshot and still say the folder rule and the one-file rule separately.
fn agent_trees_before(before: &[String]) -> Vec<(String, Vec<String>)> {
    AGENT_DIRS
        .iter()
        .map(|d| {
            let prefix = format!("{d}/");
            (
                (*d).to_owned(),
                before
                    .iter()
                    .filter(|p| p.starts_with(&prefix))
                    .map(|p| p[prefix.len()..].to_owned())
                    .collect(),
            )
        })
        .collect()
}

/// The PROJECT SWEEP keeps the built-in in place for the checkout's own pick. No manifest row
/// names it, so the undemanded clean must not read it as a dropped row: the reconcile leaves the
/// project store's built-in record alone (no `removed` row, the copy standing), under the
/// hand-run `Here` posture and the hook's both-scopes posture alike, and the sweep's own seam
/// re-converges it beside the reconcile.
#[test]
fn a_project_sweep_keeps_the_built_in_for_the_checkouts_pick() {
    let (rig, plane, dir) = deploy_rig("i6");
    let proj = project(
        "i6-co",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    rig.project_pick(&proj.0, &["claude-code"]);
    let ctx = rig.ctx_at(Some(&proj.0));
    assert!(
        ops::ensure_builtin_for_project_pick(&ctx, &proj.0)
            .unwrap()
            .changed,
        "the checkout's own pick places the built-in"
    );
    let copy = proj.0.join(".claude/skills/topos");
    assert!(copy.join("SKILL.md").exists());

    let out = sweep(&ctx, &plane, &dir);
    assert!(
        proj.0.join(".claude/skills/deploy/SKILL.md").exists(),
        "{:?}",
        out.data.skills
    );
    assert!(
        copy.join("SKILL.md").exists(),
        "the built-in survives the project clean"
    );
    assert!(
        !out.data.skills.iter().any(|r| r.skill == "topos"),
        "no row over the built-in: {:?}",
        out.data.skills
    );
    let out = sweep_both(&ctx, &plane, &dir);
    assert!(copy.join("SKILL.md").exists());
    assert!(
        !out.data.skills.iter().any(|r| r.skill == "topos"),
        "{:?}",
        out.data.skills
    );
    // The sweep's seam puts a missing copy back.
    std::fs::remove_dir_all(&copy).unwrap();
    assert!(
        ops::ensure_builtin_for_project_pick(&ctx, &proj.0)
            .unwrap()
            .changed
    );
    assert!(copy.join("SKILL.md").exists(), "re-converged");
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

/// `init -a` / `agents add` carry the reconcile's outcome on the receipt instead of dropping it:
/// a row the manifest stopped carrying is a `removed` entry naming what left; a row the
/// reconcile could not carry forward is a failure line and counts under `failed_bundles`; a
/// picked agent's skills folder that is a symlink out of the checkout is the built-in
/// placement's own refusal, on the same receipt; and a `.gitignore` that is a symlink out of the
/// checkout is one line, after the pick, the copies and the hooks landed, never an abort.
#[test]
fn apply_pick_carries_the_reconciles_outcome() {
    use crate::agents_pick::PickScope;
    use topos_types::MessageKind;
    use topos_types::results::PickRemoved;

    let rig = Rig::new("pick-outcome");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new()))).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.pick(&["claude-code"]);
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let apply = |ctx: &crate::ctx::Ctx<'_>, scope: &PickScope, gitignore: bool| {
        ops::agents::apply_pick(
            ctx,
            &connect(&plane, &dir),
            None,
            scope,
            gitignore,
            "topos agents --gitignore",
        )
    };
    let receipt = apply(&ctx, &PickScope::Machine, false).unwrap();
    let placed = rig.skills().join("deploy");
    assert!(placed.join("SKILL.md").exists(), "{receipt:?}");
    assert!(receipt.removed.is_empty() && receipt.warnings.is_empty());
    assert_eq!(receipt.failed_bundles, 0);

    // The feed row dropped: the next pick run removes the copy and SAYS so.
    rig.write_global("");
    let receipt = apply(&ctx, &PickScope::Machine, false).unwrap();
    assert_eq!(
        receipt.removed,
        vec![PickRemoved {
            bundle: format!("@{WS_NAME}/deploy"),
            destinations: vec![rig.pretty(&placed)],
        }]
    );
    assert!(!placed.exists());
    // The reconcile's advisory about the dropped feed rides too; nothing failed.
    assert!(
        receipt
            .warnings
            .iter()
            .all(|m| m.kind == MessageKind::Advisory),
        "{:?}",
        receipt.warnings
    );
    assert_eq!(receipt.failed_bundles, 0);

    // A row pointing at a folder that is gone: the failure line rides the receipt and the
    // bundle is counted, so the caller exits non-zero exactly as `update` would.
    let proj = project(
        "pick-outcome-fail",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\nnope = {{ path = \"./nope\" }}\n"),
    );
    rig.project_pick(&proj.0, &["claude-code"]);
    let ctx = rig.ctx_at(Some(&proj.0));
    let receipt = apply(&ctx, &PickScope::Project(proj.0.clone()), false).unwrap();
    assert_eq!(receipt.failed_bundles, 1, "{receipt:?}");
    let failure = receipt
        .warnings
        .iter()
        .find(|m| m.kind == MessageKind::Failure)
        .unwrap_or_else(|| panic!("{:?}", receipt.warnings));
    assert_eq!(failure.code.as_deref(), Some("PATH_MISSING"));
    assert!(failure.text.contains("./nope"), "{}", failure.text);
    assert!(
        proj.0.join(".claude/skills/topos/SKILL.md").exists(),
        "the built-in still landed for the picked agent"
    );

    // The picked agent's skills folder is a symlink out of the checkout: the built-in's
    // placement is refused by the containment rail, and the receipt carries the refusal.
    let proj = project(
        "pick-outcome-escape",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n"),
    );
    let outside = Scratch::new("pick-outcome-outside");
    std::os::unix::fs::symlink(&outside.0, proj.0.join(".claude")).unwrap();
    rig.project_pick(&proj.0, &["claude-code"]);
    let ctx = rig.ctx_at(Some(&proj.0));
    let receipt = apply(&ctx, &PickScope::Project(proj.0.clone()), false).unwrap();
    let refused = receipt
        .warnings
        .iter()
        .find(|m| m.code.as_deref() == Some("PLACEMENT_ESCAPES_PROJECT"))
        .unwrap_or_else(|| panic!("{:?}", receipt.warnings));
    assert!(
        refused
            .text
            .contains("does not resolve inside this checkout (claude-code)"),
        "{}",
        refused.text
    );
    assert!(
        tree(&outside.0).is_empty(),
        "nothing written through the link: {:?}",
        tree(&outside.0)
    );

    // `.gitignore` is a symlink out of the checkout: not edited, said as a line; the receipt
    // itself (and everything it stands for) still comes back.
    let proj = project(
        "pick-outcome-gitignore",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n"),
    );
    let target = outside.0.join("elsewhere-gitignore");
    std::fs::write(&target, b"").unwrap();
    std::os::unix::fs::symlink(&target, proj.0.join(".gitignore")).unwrap();
    rig.project_pick(&proj.0, &["claude-code"]);
    let ctx = rig.ctx_at(Some(&proj.0));
    let receipt = apply(&ctx, &PickScope::Project(proj.0.clone()), true).unwrap();
    assert!(receipt.gitignored.is_empty());
    let line = receipt
        .warnings
        .iter()
        .find(|m| m.code.as_deref() == Some("GITIGNORE_NOT_EDITED"))
        .unwrap_or_else(|| panic!("{:?}", receipt.warnings));
    assert_eq!(line.kind, MessageKind::Advisory, "a line, never a failure");
    assert_eq!(
        line.text,
        "`.gitignore` is a symlink out of this checkout; not edited"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"", "byte-identical");
    assert!(
        proj.0.join(".claude/skills/topos/SKILL.md").exists(),
        "the pick landed before the line was said"
    );
    assert_eq!(receipt.failed_bundles, 0);
}
