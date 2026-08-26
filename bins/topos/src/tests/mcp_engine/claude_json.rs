//! **Claude Code's servers live in Claude Code's own configuration.** One file — `~/.claude.json`
//! at the default, `$CLAUDE_CONFIG_DIR/.claude.json` when that variable moves the config dir —
//! holding two entries objects: top-level `mcpServers`, which every project sees, and
//! `projects.<a checkout's absolute path>.mcpServers`, which only a session started in that exact
//! directory sees. topos writes the one for the scope it is acting in, under the name the bundle
//! gives the server, and leaves every other byte of the file alone.
//!
//! What this module holds to:
//!
//! - a PROJECT scope writes the checkout's own slot, keyed by the checkout's own path, and writes
//!   nothing inside the checkout — the one thing a project pick puts under `$HOME`,
//! - a MACHINE scope writes the top-level slot and creates no per-project object at all,
//! - the plugin FOLDER an older topos owned is retired in the same run the entry moves, so no
//!   server is ever registered twice under two names,
//! - everything else in that file — the sign-in blob, trust decisions, another tool's server,
//!   another project's entries — survives a write byte for byte,
//! - a server topos does not own is never written over, whatever it is called,
//! - `dest = [".mcp.json"]` still writes a committed `.mcp.json` in the checkout, which is the way
//!   out of a slot keyed by one exact directory,
//! - a removal takes exactly the keys of the bundle being removed,
//! - and — because one agent now has more than one file — every entry is READ, WRITTEN, VERIFIED
//!   and REMOVED at the surface that holds it: the collision pre-flight looks in the slot topos is
//!   about to write, a removal reaches the file custody recorded, crash recovery observes the file
//!   its own journal named, and two scopes writing that one file wait on one lock.

use std::collections::BTreeSet;
use std::path::Path;

use topos_harness::mcp::McpDialect;
use topos_harness::mcp::descriptor::mcp_harness;
use topos_harness::registry::{KnownHarness, home_rooted_mcp_row};

use crate::mcp_engine::{self, ScopeIo};

use super::rig::*;

/// The real Claude Code row — Home-rooted for both scopes once `$CLAUDE_CONFIG_DIR` is unset, so
/// every test here stays inside its own fake home.
fn claude_row() -> Vec<&'static KnownHarness> {
    vec![mcp_harness("claude-code").expect("claude-code has an MCP surface")]
}

/// **Claude Code as an OLDER topos knew it**: its machine entries in a topos-OWNED plugin folder
/// under the agent's skills root, which registered every server under a plugin-scoped name. No
/// row spells this any more; the fixture is how a test reproduces the machine such a release left
/// behind, so the retirement can be driven over a real one.
static OLD_CLAUDE: KnownHarness = home_rooted_mcp_row(
    "claude-code",
    "Claude Code",
    ".claude/skills/topos-mcp",
    McpDialect::ClaudePluginDir,
    Some((".mcp.json", McpDialect::ClaudeProjectJson)),
    "reload claude",
);

fn only_claude() -> BTreeSet<String> {
    ["claude-code".to_owned()].into()
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

/// A checkout with a git dir, its own store, and its own pick of Claude Code.
fn checkout(rig: &Rig, tag: &str) -> Scratch {
    let proj = Scratch::new(tag);
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    rig.project_pick(&proj.0, &["claude-code"]);
    proj
}

/// The key Claude Code files a checkout's own servers under: its absolute path with symlinks
/// resolved — what a process started there reports as its working directory.
fn project_key(project: &Path) -> String {
    project
        .canonicalize()
        .unwrap_or_else(|_| project.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// **A project's servers go in the checkout's own slot — and nowhere else.** Keyed by the
/// checkout's own path, so another project's session never sees them; absent from the top-level
/// slot, which every project would; and not a byte written inside the repository.
#[test]
fn a_project_mcp_entry_lands_in_the_local_scope_slot() {
    let rig = Rig::new("cj-project");
    let proj = checkout(&rig, "cj-project-co");
    let layout = crate::sidecar::project_store_layout(&proj.0);
    let io = project_io(&rig.fs, &layout, &rig.home.0, &proj.0);
    let demands: Vec<_> = vec![demand(
        "b1",
        "linear",
        Some("eng"),
        &server_json("https://mcp.example/linear"),
    )]
    .into_iter()
    .map(|r| r.planned(&io, &claude_row(), &only_claude()))
    .collect();
    let out = mcp_engine::converge(
        &io,
        &demands,
        &claude_row(),
        &only_claude(),
        &no_hold(),
        true,
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    let doc = claude_json(&rig.home.0);
    let slot = &doc["projects"][project_key(&proj.0)]["mcpServers"];
    assert_eq!(
        slot["topos-eng-linear"]["url"], "https://mcp.example/linear",
        "the entry is in THIS checkout's slot: {doc}"
    );
    assert!(
        doc.get("mcpServers").is_none(),
        "a project pick never writes the slot every project reads: {doc}"
    );
    // The whole point of the move: a project writes nothing into the repository.
    assert!(!proj.0.join(".mcp.json").exists());
    let in_repo: Vec<String> = std::fs::read_dir(&proj.0)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        in_repo.iter().all(|n| n == ".git" || n == ".topos"),
        "only the git dir and topos's own store: {in_repo:?}"
    );
}

/// **A machine-wide server goes in the slot every project reads** — and the per-project object is
/// not invented for it.
#[test]
fn a_machine_mcp_entry_lands_in_user_scope() {
    let rig = Rig::new("cj-machine");
    let layout = rig.layout();
    let io = person_io(&rig.fs, &layout, &rig.home.0);
    let demands: Vec<_> = vec![demand(
        "b1",
        "linear",
        Some("eng"),
        &server_json("https://mcp.example/linear"),
    )]
    .into_iter()
    .map(|r| r.planned(&io, &claude_row(), &only_claude()))
    .collect();
    let out = mcp_engine::converge(
        &io,
        &demands,
        &claude_row(),
        &only_claude(),
        &no_hold(),
        true,
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    let doc = claude_json(&rig.home.0);
    assert_eq!(
        doc["mcpServers"]["topos-eng-linear"]["url"], "https://mcp.example/linear",
        "{doc}"
    );
    assert!(
        doc.get("projects").is_none(),
        "a machine-wide entry invents no per-project object: {doc}"
    );
}

/// **The plugin folder is retired in the run the entry moves.** A machine that took its entry from
/// an older topos holds it in a topos-owned folder that renamed every server it carried. The next
/// converge writes the entry where it belongs, takes the old one out, and takes the folder with
/// it — so the server is never registered twice, under two names, at once.
#[test]
fn the_plugin_folder_is_retired_when_the_entry_moves() {
    let rig = Rig::new("cj-retire");
    let layout = rig.layout();
    let io = person_io(&rig.fs, &layout, &rig.home.0);
    let row = || {
        demand(
            "b1",
            "linear",
            Some("eng"),
            &server_json("https://mcp.example/linear"),
        )
    };

    // The machine an older topos left: the entry in the plugin folder, recorded as topos's own.
    let old_table = vec![&OLD_CLAUDE];
    let old_demands: Vec<_> = vec![row()]
        .into_iter()
        .map(|r| r.planned(&io, &old_table, &only_claude()))
        .collect();
    mcp_engine::converge(
        &io,
        &old_demands,
        &old_table,
        &only_claude(),
        &no_hold(),
        true,
    );
    let folder = rig.home.0.join(".claude/skills/topos-mcp");
    assert!(
        folder.join(".mcp.json").exists() && folder.join(".claude-plugin/plugin.json").exists(),
        "the fixture reproduced the old machine"
    );

    // This topos, over the same store.
    let demands: Vec<_> = vec![row()]
        .into_iter()
        .map(|r| r.planned(&io, &claude_row(), &only_claude()))
        .collect();
    let out = mcp_engine::converge(
        &io,
        &demands,
        &claude_row(),
        &only_claude(),
        &no_hold(),
        true,
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    assert_eq!(
        claude_json(&rig.home.0)["mcpServers"]["topos-eng-linear"]["url"],
        "https://mcp.example/linear"
    );
    assert!(
        !folder.exists(),
        "the folder held only what topos wrote, so it left with the entry"
    );
    // And the move is not reported as a loss: the bundle is placed, not removed.
    assert!(
        out.removed.is_empty(),
        "an entry that moved did not leave: {:?}",
        out.removed
    );
}

/// **Everything else in that file survives a write.** It is Claude Code's whole configuration —
/// the sign-in blob, the trust decision for each directory, another tool's servers, another
/// project's entries — and topos edits one slot inside it.
#[test]
fn unrelated_keys_in_claude_json_survive_a_write() {
    let rig = Rig::new("cj-survive");
    let proj = checkout(&rig, "cj-survive-co");
    let other = "/somewhere/else";
    let before = format!(
        "{{\n  \"userID\": \"u_abcdef\",\n  \"hasCompletedOnboarding\": true,\n  \"mcpServers\": \
         {{\n    \"someone-elses\": {{\n      \"type\": \"stdio\",\n      \"command\": \"echo\"\n    \
         }}\n  }},\n  \"projects\": {{\n    \"{other}\": {{\n      \"hasTrustDialogAccepted\": \
         true,\n      \"mcpServers\": {{\n        \"their-server\": {{\n          \"type\": \
         \"http\",\n          \"url\": \"https://theirs.example\"\n        }}\n      }}\n    }}\n  \
         }}\n}}\n"
    );
    std::fs::write(rig.home.0.join(".claude.json"), &before).unwrap();

    let layout = crate::sidecar::project_store_layout(&proj.0);
    let io = project_io(&rig.fs, &layout, &rig.home.0, &proj.0);
    let demands: Vec<_> = vec![demand(
        "b1",
        "linear",
        Some("eng"),
        &server_json("https://mcp.example/linear"),
    )]
    .into_iter()
    .map(|r| r.planned(&io, &claude_row(), &only_claude()))
    .collect();
    let out = mcp_engine::converge(
        &io,
        &demands,
        &claude_row(),
        &only_claude(),
        &no_hold(),
        true,
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    let after = std::fs::read_to_string(rig.home.0.join(".claude.json")).unwrap();
    // Every line the edit did not own is still there, verbatim.
    for line in before.lines() {
        assert!(after.contains(line), "{line} was lost:\n{after}");
    }
    let doc = claude_json(&rig.home.0);
    assert_eq!(doc["userID"], "u_abcdef");
    assert_eq!(doc["hasCompletedOnboarding"], true);
    assert_eq!(doc["mcpServers"]["someone-elses"]["command"], "echo");
    assert_eq!(doc["projects"][other]["hasTrustDialogAccepted"], true);
    assert_eq!(
        doc["projects"][other]["mcpServers"]["their-server"]["url"],
        "https://theirs.example"
    );
    // …and this checkout got its own slot beside all of it.
    assert_eq!(
        doc["projects"][project_key(&proj.0)]["mcpServers"]["topos-eng-linear"]["url"],
        "https://mcp.example/linear"
    );
}

/// **A server topos does not own is never written over.** Not by key, and not by the address it
/// dials under some other name: the placement is refused, the entry is left byte-identical, and
/// the reason is said with the way out.
#[test]
fn a_foreign_server_with_the_same_name_is_never_clobbered() {
    let rig = Rig::new("cj-foreign");
    let proj = checkout(&rig, "cj-foreign-co");
    let key = project_key(&proj.0);
    let before = format!(
        "{{\n  \"projects\": {{\n    \"{key}\": {{\n      \"mcpServers\": {{\n        \
         \"topos-eng-linear\": {{\n          \"type\": \"http\",\n          \"url\": \
         \"https://not-ours.example\"\n        }}\n      }}\n    }}\n  }}\n}}\n"
    );
    std::fs::write(rig.home.0.join(".claude.json"), &before).unwrap();

    let layout = crate::sidecar::project_store_layout(&proj.0);
    let io = project_io(&rig.fs, &layout, &rig.home.0, &proj.0);
    let demands: Vec<_> = vec![demand(
        "b1",
        "linear",
        Some("eng"),
        &server_json("https://mcp.example/linear"),
    )]
    .into_iter()
    .map(|r| r.planned(&io, &claude_row(), &only_claude()))
    .collect();
    let out = mcp_engine::converge(
        &io,
        &demands,
        &claude_row(),
        &only_claude(),
        &no_hold(),
        true,
    );

    assert_eq!(
        std::fs::read_to_string(rig.home.0.join(".claude.json")).unwrap(),
        before,
        "not a byte moved"
    );
    assert_eq!(
        state_of(&out, "b1", "claude-code").state,
        topos_types::results::TargetOutcome::Conflicting,
        "{out:?}"
    );
}

/// **`dest = [".mcp.json"]` still writes a committed `.mcp.json`.** The slot the default reach
/// writes is keyed by one exact directory, which a session started in a subdirectory does not
/// see; naming the file is the way out, and naming it means the file and not the slot.
#[test]
fn an_explicit_dest_still_writes_mcp_json() {
    let rig = Rig::new("cj-dest");
    rig.seed_session();
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    let proj = Scratch::new("cj-dest-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    rig.project_pick(&proj.0, &["claude-code"]);
    let s = served_at("https://mcp.example/linear");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_linear", "linear", &s)]);
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_linear", "linear", &s)]);
    rig.write_global("schema = 1\n");
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        format!(
            "workspace = \"{HOST}/{WS_NAME}\"\n\n[mcp]\nlinear = {{ dest = [\".mcp.json\"] }}\n"
        ),
    )
    .unwrap();

    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.failed_bundles.is_empty(), "{:?}", out.warnings);

    let committed =
        std::fs::read_to_string(proj.0.join(".mcp.json")).expect("the file the row named");
    assert!(committed.contains("topos-eng-linear"), "{committed}");
    assert!(
        claude_project_servers(&rig.home.0, &proj.0).is_empty(),
        "the file the row named, and not the slot beside it"
    );
}

/// **Two rows reaching two of one agent's files each land in their own.** One project can hold a
/// row that takes the default surface and a row whose `dest` names the committed file beside it;
/// the plan names the FILE, so neither row's entry ends up in the other's — and neither file is
/// reported as the other's stale leftover, because both are surfaces this agent keeps servers in.
#[test]
fn two_rows_reaching_two_files_each_land_in_their_own() {
    let rig = Rig::new("cj-both");
    rig.seed_session();
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    let proj = Scratch::new("cj-both-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    rig.project_pick(&proj.0, &["claude-code"]);
    let linear = served_at("https://mcp.example/linear");
    let sentry = served_at("https://mcp.example/sentry");
    let plane = FakePlane::new();
    plane.serves_servers(vec![
        delivered_mcp("s_linear", "linear", &linear),
        delivered_mcp("s_sentry", "sentry", &sentry),
    ]);
    let dir = FakeDirectory::of_servers(vec![
        mcp_catalog_entry("s_linear", "linear", &linear),
        mcp_catalog_entry("s_sentry", "sentry", &sentry),
    ]);
    rig.write_global("schema = 1\n");
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        format!(
            "workspace = \"{HOST}/{WS_NAME}\"\n\n[mcp]\nlinear = \"latest\"\nsentry = {{ dest = \
             [\".mcp.json\"] }}\n"
        ),
    )
    .unwrap();

    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.failed_bundles.is_empty(), "{:?}", out.warnings);
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .all(|w| !w.contains("MCP_ENTRY_STALE_PATH")),
        "one of this agent's files is not a stale copy of the other: {:?}",
        out.warnings
    );

    let slot = claude_project_servers(&rig.home.0, &proj.0);
    assert!(
        slot.contains("topos-eng-linear") && !slot.contains("topos-eng-sentry"),
        "the default row, and only it, took the checkout's slot: {slot}"
    );
    let committed = std::fs::read_to_string(proj.0.join(".mcp.json")).expect("the named file");
    assert!(
        committed.contains("topos-eng-sentry") && !committed.contains("topos-eng-linear"),
        "the named row, and only it, took the committed file: {committed}"
    );
}

/// **A removal takes exactly its own keys.** Another bundle's entry beside it, a server somebody
/// else added, and every unrelated key in Claude Code's configuration are all still there
/// afterwards — the file is not topos's to empty.
#[test]
fn removing_a_bundle_removes_only_its_own_keys() {
    let rig = Rig::new("cj-remove");
    let layout = rig.layout();
    let io = person_io(&rig.fs, &layout, &rig.home.0);
    std::fs::write(
        rig.home.0.join(".claude.json"),
        "{\n  \"userID\": \"u_abcdef\",\n  \"mcpServers\": {\n    \"someone-elses\": {\n      \
         \"type\": \"stdio\",\n      \"command\": \"echo\"\n    }\n  }\n}\n",
    )
    .unwrap();
    let rows = vec![
        demand(
            "b1",
            "linear",
            Some("eng"),
            &server_json("https://mcp.example/linear"),
        ),
        demand(
            "b2",
            "sentry",
            Some("eng"),
            &server_json("https://mcp.example/sentry"),
        ),
    ];
    let demands: Vec<_> = rows
        .into_iter()
        .map(|r| r.planned(&io, &claude_row(), &only_claude()))
        .collect();
    mcp_engine::converge(
        &io,
        &demands,
        &claude_row(),
        &only_claude(),
        &no_hold(),
        true,
    );
    let doc = claude_json(&rig.home.0);
    assert!(doc["mcpServers"]["topos-eng-linear"].is_object(), "{doc}");
    assert!(doc["mcpServers"]["topos-eng-sentry"].is_object(), "{doc}");

    let out = mcp_engine::remove_bundle(&io, &claude_row(), &only_claude(), "b1", "linear");
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    let doc = claude_json(&rig.home.0);
    assert!(
        doc["mcpServers"].get("topos-eng-linear").is_none(),
        "the removed bundle's key went: {doc}"
    );
    assert_eq!(
        doc["mcpServers"]["topos-eng-sentry"]["url"], "https://mcp.example/sentry",
        "the other bundle stands: {doc}"
    );
    assert_eq!(doc["mcpServers"]["someone-elses"]["command"], "echo");
    assert_eq!(doc["userID"], "u_abcdef");
}

// =================================================================================================
// SURFACE IDENTITY — an entry is read, written, verified and removed at the surface that HOLDS it.
// One file per harness per scope is over: the slot a checkout's entries sit in is inside a machine
// file, the file a row's own `dest` names is beside it, and the folder an older topos owned is
// still on disk. Every question below used to be answered by asking the descriptor for its DEFAULT
// surface, which is a guess the moment an agent has two.
// =================================================================================================

/// **A foreign server in the checkout's OWN slot is a collision.** The pre-flight reads the slot
/// topos is about to write, not the dialect's default one: a server somebody added by hand to this
/// project's entries, under their own name, is exactly what a placement must not be inserted
/// beside. Reading the top-level slot instead looked straight past it and wrote a duplicate.
#[test]
fn a_foreign_server_in_the_checkouts_own_slot_is_a_collision() {
    let rig = Rig::new("cj-slot-clash");
    let proj = checkout(&rig, "cj-slot-clash-co");
    let layout = crate::sidecar::project_store_layout(&proj.0);
    let io = project_io(&rig.fs, &layout, &rig.home.0, &proj.0);
    // Somebody dialed this very server by hand, in THIS checkout's slot, under their own name.
    // The top-level slot — the one a default read would have looked at — is empty.
    let mut doc = serde_json::json!({ "mcpServers": {} });
    doc["projects"] = serde_json::json!({
        project_key(&proj.0): {
            "mcpServers": {
                "their-linear": { "type": "http", "url": "https://mcp.example/linear" }
            }
        }
    });
    std::fs::write(
        rig.home.0.join(".claude.json"),
        serde_json::to_string_pretty(&doc).unwrap(),
    )
    .unwrap();

    let demands: Vec<_> = vec![demand(
        "b1",
        "linear",
        Some("eng"),
        &server_json("https://mcp.example/linear"),
    )]
    .into_iter()
    .map(|r| r.planned(&io, &claude_row(), &only_claude()))
    .collect();
    let out = mcp_engine::converge(
        &io,
        &demands,
        &claude_row(),
        &only_claude(),
        &no_hold(),
        true,
    );

    assert_eq!(
        state_of(&out, "b1", "claude-code").state,
        topos_types::results::TargetOutcome::Conflicting,
        "{out:?}"
    );
    let slot = claude_project_servers(&rig.home.0, &proj.0);
    assert!(
        slot.contains("their-linear") && !slot.contains("topos-eng-linear"),
        "no duplicate beside the entry already dialing that server: {slot}"
    );
}

/// **A `dest`-placed entry leaves through the file that HOLDS it.** The row named
/// `.mcp.json`, so that is where the entry is — and a removal that resolved the agent's default
/// surface instead found no entry of ours there, reported success, and left the entry standing in
/// a file custody still claimed. Driven through `remove_bundle`, which is the one mechanic the
/// `remove` verb, `agents remove` and `uninstall` all run.
#[test]
fn a_dest_placed_entry_is_removed_from_the_file_that_holds_it() {
    let rig = Rig::new("cj-dest-remove");
    rig.seed_session();
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    let proj = Scratch::new("cj-dest-remove-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    rig.project_pick(&proj.0, &["claude-code"]);
    let s = served_at("https://mcp.example/linear");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_linear", "linear", &s)]);
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_linear", "linear", &s)]);
    rig.write_global("schema = 1\n");
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        format!(
            "workspace = \"{HOST}/{WS_NAME}\"\n\n[mcp]\nlinear = {{ dest = [\".mcp.json\"] }}\n"
        ),
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.failed_bundles.is_empty(), "{:?}", out.warnings);
    let committed = proj.0.join(".mcp.json");
    assert!(
        std::fs::read_to_string(&committed)
            .unwrap()
            .contains("topos-eng-linear"),
        "the fixture placed where the row named"
    );

    let layout = crate::sidecar::project_store_layout(&proj.0)
        .under_machine(crate::sidecar::Layout::new(&rig.home.0.join(".topos")).home());
    let io = project_io(&rig.fs, &layout, &rig.home.0, &proj.0);
    let removed =
        mcp_engine::remove_bundle(&io, &claude_row(), &only_claude(), "s_linear", "linear");
    assert!(removed.warnings.is_empty(), "{:?}", removed.warnings);
    assert!(
        !std::fs::read_to_string(&committed)
            .unwrap_or_default()
            .contains("topos-eng-linear"),
        "the entry left the file that held it: {:?}",
        std::fs::read_to_string(&committed)
    );
    assert!(
        !crate::config_custody::ScopeEntries::load(&rig.fs, &layout)
            .unwrap()
            .holds(&crate::config_custody::placement_key(
                "claude-code",
                "topos-eng-linear"
            )),
        "and custody no longer claims it"
    );
}

/// **`agents remove` reaches the same file.** Dropping the agent from a project's pick takes its
/// entries out of every file this scope recorded them in — the committed one a row's own `dest`
/// named included.
#[test]
fn dropping_the_agent_removes_its_dest_placed_entry_too() {
    let rig = Rig::new("cj-dest-agents");
    rig.seed_session();
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    let proj = Scratch::new("cj-dest-agents-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    rig.project_pick(&proj.0, &["claude-code"]);
    let s = served_at("https://mcp.example/linear");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_linear", "linear", &s)]);
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_linear", "linear", &s)]);
    rig.write_global("schema = 1\n");
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        format!(
            "workspace = \"{HOST}/{WS_NAME}\"\n\n[mcp]\nlinear = {{ dest = [\".mcp.json\"] }}\n"
        ),
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep(&ctx, &plane, &dir);
    let committed = proj.0.join(".mcp.json");
    assert!(
        std::fs::read_to_string(&committed)
            .unwrap()
            .contains("topos-eng-linear"),
        "the fixture placed where the row named"
    );

    rig.project_pick(&proj.0, &[]);
    let receipt = crate::ops::agents::apply_pick(
        &ctx,
        &connect(&plane, &dir),
        None,
        &crate::agents_pick::PickScope::Project(proj.0.clone()),
        false,
        "topos agents --gitignore",
    )
    .expect("the pick applies");
    assert!(
        !std::fs::read_to_string(&committed)
            .unwrap_or_default()
            .contains("topos-eng-linear"),
        "{receipt:?}"
    );
}

/// **A leftover in the retired plugin folder leaves with the teardown.** An entry an older topos
/// wrote into the folder it owned is topos's by custody wherever it sits, so `uninstall` takes it
/// — and the describe names the same file the apply touches. Resolving only the agent's current
/// default surface left the folder, its entry and its manifest on disk after topos was gone.
#[test]
fn a_plugin_folder_leftover_leaves_with_the_teardown() {
    let rig = Rig::new("cj-teardown");
    let layout = rig.layout();
    let io = person_io(&rig.fs, &layout, &rig.home.0);
    // The machine an older topos left: the entry in the plugin folder, recorded as topos's own.
    let old_table = vec![&OLD_CLAUDE];
    let demands: Vec<_> = vec![demand(
        "b1",
        "linear",
        Some("eng"),
        &server_json("https://mcp.example/linear"),
    )]
    .into_iter()
    .map(|r| r.planned(&io, &old_table, &only_claude()))
    .collect();
    mcp_engine::converge(&io, &demands, &old_table, &only_claude(), &no_hold(), true);
    let folder = rig.home.0.join(".claude/skills/topos-mcp");
    assert!(
        folder.join(".mcp.json").exists(),
        "the fixture reproduced it"
    );

    // The DESCRIBE names it…
    let owned = mcp_engine::recorded_surfaces(
        &io,
        &topos_harness::mcp::descriptor::mcp_harnesses_for_teardown(),
    )
    .owned;
    assert!(
        owned
            .iter()
            .any(|f| std::path::Path::new(f) == folder.join(".mcp.json")),
        "the teardown promises the file it will touch: {owned:?}"
    );

    // …and the APPLY takes it.
    let ctx = rig.ctx_at(None);
    crate::ops::uninstall(&ctx, None, true).expect("the teardown runs");
    assert!(
        !folder.exists(),
        "the folder topos owned went with its last entry"
    );
}

/// **A crash between the config write and the custody promotion heals at the surface the intent
/// NAMED** — for the checkout's slot inside Claude Code's own configuration, and for the committed
/// file a row's `dest` named. Recovery observes the intent's OWN file under that file's own slot;
/// resolving the agent's default surface instead observed a slot the write never touched, judged a
/// landed write never to have landed, and dropped the intent — which leaves a live entry with no
/// row behind it: unowned, unremovable, and read as somebody else's on every run afterwards.
#[test]
fn a_crash_window_heals_at_the_surface_the_intent_named() {
    for dest_named in [false, true] {
        let tag = if dest_named { "dest" } else { "slot" };
        let rig = Rig::new(&format!("cj-crash-{tag}"));
        let proj = checkout(&rig, &format!("cj-crash-{tag}-co"));
        let layout = crate::sidecar::project_store_layout(&proj.0);
        let io = project_io(&rig.fs, &layout, &rig.home.0, &proj.0);
        let row = || {
            let mut d = demand(
                "b1",
                "linear",
                Some("eng"),
                &server_json("https://mcp.example/linear"),
            );
            d.reach = Some(vec![crate::placement::Reach {
                slug: "claude-code".to_owned(),
                dest_named,
            }]);
            d
        };
        let demands: Vec<_> = vec![row()]
            .into_iter()
            .map(|r| r.planned(&io, &claude_row(), &only_claude()))
            .collect();
        let out = mcp_engine::converge(
            &io,
            &demands,
            &claude_row(),
            &only_claude(),
            &no_hold(),
            true,
        );
        assert!(out.warnings.is_empty(), "{tag}: {:?}", out.warnings);
        let file = state_of(&out, "b1", "claude-code")
            .file
            .clone()
            .expect("the entry names the file it landed in");
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("topos-eng-linear"),
            "{tag}: the fixture placed the entry"
        );

        // THE CRASH WINDOW: the config write landed and the custody promotion did not, so on disk
        // the entry stands with NO row of its own and the intent is still journaled. That journal
        // is the only thing left saying the entry is topos's.
        let key = crate::config_custody::placement_key("claude-code", "topos-eng-linear");
        let mut custody = crate::config_custody::ScopeEntries::load(&rig.fs, &layout).unwrap();
        let live = custody
            .row(&key)
            .expect("the placement was recorded")
            .clone();
        custody.remove(&key);
        custody
            .journal(
                &rig.fs,
                &layout,
                std::iter::once((
                    key.clone(),
                    crate::config_custody::PendingIntent {
                        bundle_id: "b1".into(),
                        version_id: "v1".into(),
                        file: live.file.clone(),
                        fingerprint: live.fingerprint.clone(),
                        owns_file: live.owns_file,
                    },
                ))
                .collect(),
            )
            .unwrap();
        assert!(custody.flush(&rig.fs, &layout).is_empty());

        // The next run recovers by OBSERVING that file: the entry is topos's again. Nothing
        // demands it any more, so this run also takes it back out — which is the whole proof that
        // custody was restored, since an entry topos does not own is one it never touches.
        let out = mcp_engine::converge(&io, &[], &claude_row(), &only_claude(), &no_hold(), true);
        assert!(out.warnings.is_empty(), "{tag}: {:?}", out.warnings);
        assert!(
            !std::fs::read_to_string(&file)
                .unwrap_or_default()
                .contains("topos-eng-linear"),
            "{tag}: the recovered entry was still topos's to remove: {:?}",
            std::fs::read_to_string(&file)
        );
        let custody = crate::config_custody::ScopeEntries::load(&rig.fs, &layout).unwrap();
        assert!(
            custody.doc.pending.is_empty(),
            "{tag}: the journal is settled"
        );
        assert!(
            !custody.holds(&key),
            "{tag}: and nothing is left claiming it"
        );
    }
}

/// **Two scopes writing ONE file wait on ONE lock.** The machine's servers and a checkout's live
/// in the same `~/.claude.json`, in different slots, so a machine sweep and a project sweep are
/// two writers of one file. Each locking its own store serializes nothing: both read one preimage,
/// each edits its own slot, and the second atomic replace silently discards the first edit while
/// both custody journals record success. Here the other writer holds the FILE's own lock across
/// its edit, and the project converge waits — so both edits stand.
#[test]
fn two_scopes_writing_one_file_wait_on_one_lock() {
    use crate::fs_seam::FsOps;

    let rig = Rig::new("cj-lock");
    let proj = checkout(&rig, "cj-lock-co");
    let machine_layout = rig.layout();
    let project_layout =
        crate::sidecar::project_store_layout(&proj.0).under_machine(machine_layout.home());
    let machine_io = person_io(&rig.fs, &machine_layout, &rig.home.0);
    let project_io = project_io(&rig.fs, &project_layout, &rig.home.0, &proj.0);
    let claude = rig.home.0.join(".claude.json");
    std::fs::write(&claude, "{\n  \"mcpServers\": {}\n}\n").unwrap();

    // The two scopes name the SAME lock for that one file — and a different file, a different
    // lock (a lock every config file shared would serialize the whole machine).
    let lock = mcp_engine::surface_lock_path(&project_io, &claude);
    assert_eq!(
        lock,
        mcp_engine::surface_lock_path(&machine_io, &claude),
        "one file, one lock, whichever scope is writing it"
    );
    assert_ne!(
        lock,
        mcp_engine::surface_lock_path(&machine_io, &proj.0.join(".mcp.json"))
    );

    // The other writer: takes the file's lock, spends a beat over its read-modify-write (the
    // window a lost update lives in), and only then releases.
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let (holding, held) = std::sync::mpsc::channel();
    let other = {
        let claude = claude.clone();
        let lock = lock.clone();
        std::thread::spawn(move || {
            let fs = crate::fs_seam::RealFs;
            let guard = fs.lock_exclusive(&lock).expect("the file's lock");
            holding.send(()).unwrap();
            let mut doc: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&claude).unwrap()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(150));
            doc["mcpServers"]["someone-elses"] =
                serde_json::json!({ "type": "stdio", "command": "echo" });
            std::fs::write(&claude, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
            drop(guard);
        })
    };
    held.recv().unwrap();

    let demands: Vec<_> = vec![demand(
        "b1",
        "linear",
        Some("eng"),
        &server_json("https://mcp.example/linear"),
    )]
    .into_iter()
    .map(|r| r.planned(&project_io, &claude_row(), &only_claude()))
    .collect();
    let out = mcp_engine::converge(
        &project_io,
        &demands,
        &claude_row(),
        &only_claude(),
        &no_hold(),
        true,
    );
    other.join().unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    let doc = claude_json(&rig.home.0);
    assert_eq!(
        doc["mcpServers"]["someone-elses"]["command"], "echo",
        "the other writer's edit survived: {doc}"
    );
    assert!(
        claude_project_servers(&rig.home.0, &proj.0).contains("topos-eng-linear"),
        "and so did this one's: {doc}"
    );
}
