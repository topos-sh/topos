//! **A project's OpenCode servers sit under the agent's own folder.** OpenCode reads a project
//! config at `<checkout>/.opencode/opencode.json` as well as one at the checkout root, merging the
//! folder's copy last; topos writes the folder's, beside the trigger file it already puts there,
//! and leaves the checkout root clean.
//!
//! What this module holds to:
//!
//! - a project's entries land in `.opencode/opencode.json`, whether or not that folder exists yet,
//! - a checkout an older topos set up — its entries in the root `opencode.json` — has them retired
//!   from that file in the same run they move, so no server is registered twice at once,
//! - the root file goes with the last entry topos owned there, and NEVER while it holds anything
//!   else,
//! - unrelated OpenCode settings in the config topos writes — the model, the agents, the
//!   permissions — survive a write byte for byte,
//! - and `dest = ["opencode.json"]` still writes the root file, which is the way to a config every
//!   session under the repo reads the same way.

use std::collections::BTreeSet;
use std::path::Path;

use topos_harness::mcp::McpDialect;
use topos_harness::mcp::descriptor::{EnvRef, mcp_harness};
use topos_harness::registry::{KnownHarness, home_rooted_mcp_row_with_caps};

use crate::mcp_engine::{self, DemandedBundle, ScopeIo};

use super::rig::*;

/// The real OpenCode row. Its project surfaces are checkout-relative, so every test here stays
/// inside its own scratch checkout.
fn opencode_row() -> Vec<&'static KnownHarness> {
    vec![mcp_harness("opencode").expect("opencode has an MCP surface")]
}

/// **OpenCode as an OLDER topos knew it**: the project config at the checkout ROOT. No row spells
/// this any more; the fixture is how a test reproduces the checkout such a release left behind, so
/// the retirement can be driven over a real one.
static OLD_OPENCODE: KnownHarness = home_rooted_mcp_row_with_caps(
    "opencode",
    "OpenCode",
    ".opencode/opencode.json",
    McpDialect::OpencodeJson,
    Some(("opencode.json", McpDialect::OpencodeJson)),
    "restart opencode",
    true,
    true,
    EnvRef::BraceEnv,
);

fn only_opencode() -> BTreeSet<String> {
    ["opencode".to_owned()].into()
}

/// A project-scope `ScopeIo` over the checkout's own store.
fn project_io<'a>(
    fs: &'a crate::fs_seam::RealFs,
    layout: &'a crate::sidecar::Layout,
    home: &Path,
    project: &Path,
) -> ScopeIo<'a> {
    ScopeIo {
        project_root: Some(project.to_path_buf()),
        ..person_io(fs, layout, home)
    }
}

/// A checkout with a git dir, its own store, and its own pick of OpenCode.
fn checkout(rig: &Rig, tag: &str) -> Scratch {
    let proj = Scratch::new(tag);
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    rig.project_pick(&proj.0, &["opencode"]);
    proj
}

/// The one server every test here places.
fn linear() -> DemandedBundle {
    demand(
        "b1",
        "linear",
        Some("eng"),
        &server_json("https://mcp.example/linear"),
    )
}

/// Converge one scope over a table, and answer the outcome.
fn converge_over(
    io: &ScopeIo<'_>,
    table: &[&'static KnownHarness],
    rows: Vec<DemandedBundle>,
) -> mcp_engine::ConvergeOutcome {
    let demands: Vec<_> = rows
        .into_iter()
        .map(|r| r.planned(io, table, &only_opencode()))
        .collect();
    mcp_engine::converge(io, &demands, table, &only_opencode(), &no_hold(), true)
}

fn read_json(path: &Path) -> serde_json::Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// **A project's servers go under the agent's own folder** — created for the write when it is not
/// there yet — and nothing lands loose in the checkout root.
#[test]
fn opencode_project_entries_land_in_the_dot_opencode_config() {
    let rig = Rig::new("oc-project");
    let proj = checkout(&rig, "oc-project-co");
    assert!(
        !proj.0.join(".opencode").exists(),
        "the folder is the writer's to create"
    );
    let layout = crate::sidecar::project_store_layout(&proj.0);
    let io = project_io(&rig.fs, &layout, &rig.home.0, &proj.0);
    let out = converge_over(&io, &opencode_row(), vec![linear()]);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    let config = proj.0.join(".opencode").join("opencode.json");
    let doc = read_json(&config);
    assert_eq!(
        doc["mcp"]["topos-eng-linear"]["url"], "https://mcp.example/linear",
        "the entry is in the agent's own folder: {doc}"
    );
    assert!(
        !proj.0.join("opencode.json").exists(),
        "and nothing loose in the checkout root"
    );
}

/// **A checkout an older topos set up has its root entry retired in the run it moves.** The next
/// converge writes the entry where it belongs, takes the old one out, and takes the root file with
/// it — so the server is never registered twice, under two names, at once.
#[test]
fn an_older_projects_root_opencode_json_entry_is_retired_when_it_moves() {
    let rig = Rig::new("oc-retire");
    let proj = checkout(&rig, "oc-retire-co");
    let layout = crate::sidecar::project_store_layout(&proj.0);
    let io = project_io(&rig.fs, &layout, &rig.home.0, &proj.0);

    // The checkout an older topos left: the entry in the root file, recorded as topos's own.
    converge_over(&io, &[&OLD_OPENCODE], vec![linear()]);
    let root = proj.0.join("opencode.json");
    assert!(
        read_json(&root)["mcp"]["topos-eng-linear"].is_object(),
        "the fixture reproduced the old checkout"
    );

    // This topos, over the same store.
    let out = converge_over(&io, &opencode_row(), vec![linear()]);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    let doc = read_json(&proj.0.join(".opencode").join("opencode.json"));
    assert_eq!(
        doc["mcp"]["topos-eng-linear"]["url"], "https://mcp.example/linear",
        "{doc}"
    );
    assert!(
        !root.exists(),
        "the root file held only what topos wrote, so it left with the entry"
    );
    // And the move is not reported as a loss: the bundle is placed, not removed.
    assert!(
        out.removed.is_empty(),
        "an entry that moved did not leave: {:?}",
        out.removed
    );
}

/// **Everything else in that config survives a write.** `.opencode/opencode.json` is OpenCode's
/// own project configuration — the model, the agents it defines, the permissions it grants — and
/// topos edits one slot inside it.
#[test]
fn unrelated_keys_in_the_opencode_config_survive_a_write() {
    let rig = Rig::new("oc-survive");
    let proj = checkout(&rig, "oc-survive-co");
    let config = proj.0.join(".opencode").join("opencode.json");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let before = "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"model\": \
                  \"anthropic/claude-opus-4\",\n  \"agent\": {\n    \"reviewer\": {\n      \
                  \"prompt\": \"Review the diff.\"\n    }\n  },\n  \"permission\": {\n    \
                  \"edit\": \"ask\"\n  },\n  \"mcp\": {\n    \"their-server\": {\n      \"type\": \
                  \"remote\",\n      \"url\": \"https://theirs.example\"\n    }\n  }\n}\n";
    std::fs::write(&config, before).unwrap();

    let layout = crate::sidecar::project_store_layout(&proj.0);
    let io = project_io(&rig.fs, &layout, &rig.home.0, &proj.0);
    let out = converge_over(&io, &opencode_row(), vec![linear()]);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    let after = std::fs::read_to_string(&config).unwrap();
    // Every line the edit did not own is still there, verbatim.
    for line in before.lines() {
        assert!(after.contains(line), "{line} was lost:\n{after}");
    }
    let doc = read_json(&config);
    assert_eq!(doc["model"], "anthropic/claude-opus-4");
    assert_eq!(doc["agent"]["reviewer"]["prompt"], "Review the diff.");
    assert_eq!(doc["permission"]["edit"], "ask");
    assert_eq!(doc["mcp"]["their-server"]["url"], "https://theirs.example");
    // …and the bundle's entry stands beside all of it.
    assert_eq!(
        doc["mcp"]["topos-eng-linear"]["url"], "https://mcp.example/linear",
        "{doc}"
    );
}

/// **A root `opencode.json` holding anything else is never deleted.** The entry topos owned there
/// still leaves when the row moves; the file, and every byte of it topos did not write, stay.
#[test]
fn a_root_opencode_json_with_foreign_content_is_never_deleted() {
    let rig = Rig::new("oc-foreign");
    let proj = checkout(&rig, "oc-foreign-co");
    let layout = crate::sidecar::project_store_layout(&proj.0);
    let io = project_io(&rig.fs, &layout, &rig.home.0, &proj.0);

    // The old checkout again — and this time the root file is one a person also writes in.
    converge_over(&io, &[&OLD_OPENCODE], vec![linear()]);
    let root = proj.0.join("opencode.json");
    let planted = std::fs::read_to_string(&root).unwrap().replacen(
        "\"mcp\": {",
        "\"model\": \"anthropic/claude-opus-4\",\n  \"mcp\": {\n    \"their-server\": { \
             \"type\": \"remote\", \"url\": \"https://theirs.example\" },",
        1,
    );
    std::fs::write(&root, &planted).unwrap();

    let out = converge_over(&io, &opencode_row(), vec![linear()]);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    assert!(root.exists(), "a file holding somebody else's bytes stays");
    let doc = read_json(&root);
    assert_eq!(doc["model"], "anthropic/claude-opus-4");
    assert_eq!(doc["mcp"]["their-server"]["url"], "https://theirs.example");
    assert!(
        doc["mcp"].get("topos-eng-linear").is_none(),
        "topos's own entry left the old file: {doc}"
    );
    assert_eq!(
        read_json(&proj.0.join(".opencode").join("opencode.json"))["mcp"]["topos-eng-linear"]["url"],
        "https://mcp.example/linear",
        "…and stands in the folder the row names now"
    );
}

/// **`dest = ["opencode.json"]` still writes the root file.** OpenCode reads it too, and a config
/// at the repository root is the one a person may want committed; naming the file is how a row
/// asks for it, and naming it means that file and not the folder's.
#[test]
fn an_explicit_dest_still_writes_the_root_file() {
    let rig = Rig::new("oc-dest");
    rig.seed_session();
    let proj = Scratch::new("oc-dest-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    rig.project_pick(&proj.0, &["opencode"]);
    let s = served_at("https://mcp.example/linear");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_linear", "linear", &s)]);
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_linear", "linear", &s)]);
    rig.write_global("schema = 1\n");
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        format!(
            "workspace = \"{HOST}/{WS_NAME}\"\n\n[mcp]\nlinear = {{ dest = [\"opencode.json\"] \
             }}\n"
        ),
    )
    .unwrap();

    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.failed_bundles.is_empty(), "{:?}", out.warnings);

    let named =
        std::fs::read_to_string(proj.0.join("opencode.json")).expect("the file the row named");
    assert!(named.contains("topos-eng-linear"), "{named}");
    assert!(
        !proj.0.join(".opencode").join("opencode.json").exists(),
        "the file the row named, and not the folder's config beside it"
    );
}
