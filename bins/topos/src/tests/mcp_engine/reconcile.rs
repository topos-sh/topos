//! The reconcile integration (the delivery loop; Home-rooted real descriptors only — cursor,
//! openclaw — so no test can reach outside the fake home whatever the dev env sets. opencode's
//! user surface hangs off `$XDG_CONFIG_HOME` the way claude-code's / codex's / hermes's hang off
//! theirs, so person scope leaves it out; its coverage is the PROJECT test below, where every
//! surface is checkout-relative and hermetic by construction).

use std::collections::BTreeSet;

use topos_types::requests::{WireChannelEntry, WireChannelSkill};
use topos_types::results::TargetOutcome;

use crate::config_custody::ScopeEntries;
use crate::{ops, sync_status};

use super::rig::*;

/// **THE RECEIPT'S OWN SPELLING IS A LEGAL `dest`.** Claude Code's MCP surface is a topos-owned
/// plugin DIRECTORY, and every receipt names the `.mcp.json` inside it — while the teaching list
/// taught the directory, so pasting what you had just been shown was refused as "not a known MCP
/// config file". The file spelling is now the one the teaching prints AND one `dest` accepts, and
/// it delivers.
#[test]
fn the_claude_plugin_dirs_file_spelling_is_taught_and_delivers() {
    // The teaching list names the FILE — the same string the receipts print.
    let taught =
        crate::manifest::dest::known_mcp_files(crate::manifest::document::ManifestScope::Global);
    assert!(
        taught.contains(&"~/.claude/skills/topos-mcp/.mcp.json".to_owned()),
        "{taught:?}"
    );

    let rig = Rig::new("plugin-dest");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    let s = served_at("https://mcp.example/linear");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_linear", "linear", &s)]);
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_linear", "linear", &s)]);
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/linear\" = \
         {{ dest = [\"~/.claude/skills/topos-mcp/.mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    assert!(out.failed_bundles.is_empty(), "{:?}", out.warnings);
    assert!(
        !crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .chain(crate::message::legacy_lines(&out.advisories))
            .any(|w| w.contains("MCP_DEST")),
        "a known file earns no dest complaint: {:?} / {:?}",
        out.warnings,
        out.advisories
    );
    let placed = std::fs::read_to_string(rig.home.0.join(".claude/skills/topos-mcp/.mcp.json"))
        .expect("the plugin dir's config");
    assert!(
        placed.contains("topos-eng-linear") && placed.contains("https://mcp.example/linear"),
        "{placed}"
    );
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "linear")
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    let agents: BTreeSet<&str> = row.harnesses.iter().map(|h| h.agent.as_str()).collect();
    assert_eq!(
        agents,
        ["claude-code"].into(),
        "frozen to the one file the row names: {row:?}"
    );
}

#[test]
fn a_workspace_mcp_bundle_lands_in_configs_reports_harnesses_and_caches_kind() {
    let rig = Rig::new("deliver");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let s = served_at("https://mcp.example/linear");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_linear", "linear", &s)]);
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_linear", "linear", &s)]);
    // The explicit row delivers; its `dest` names the hermetic config files.
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/linear\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    // The delivery landed as a RECORD — the document this machine was given, under the revision
    // it came from. That record IS the sync for the kind: there is nothing content-addressed
    // behind it, so the lock beside it is the ordinary never-received one and no dir is placed.
    let record = server_record(&rig.fs, &rig.layout(), "s_linear").expect("the record");
    assert_eq!(record.revision_id, s.revision_id);
    assert_eq!(
        record.document["remotes"][0]["url"], "https://mcp.example/linear",
        "{record:?}"
    );
    let sid = crate::id::SkillId::parse("s_linear").unwrap();
    let sp = rig.layout().published(&sid);
    let lock: topos_types::persisted::Lock =
        crate::doc::read_doc(&rig.fs, &sp.lock).unwrap().unwrap();
    assert_eq!(lock.base_commit, crate::ops::inventory::ZERO_HEX);
    assert!(lock.files.is_empty(), "a connected server holds no files");
    let map = crate::doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    assert!(
        map.placements.is_empty(),
        "an mcp bundle places no dirs: {map:?}"
    );
    assert!(!rig.work.0.join("skills/linear").exists());
    assert!(
        !rig.home.0.join(".agents").exists(),
        "no shared-dir copy either"
    );

    // The two hermetic configs hold the entry.
    for path in [
        rig.home.0.join(".cursor/mcp.json"),
        rig.home.0.join(".openclaw/openclaw.json"),
    ] {
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        assert!(
            text.contains("topos-eng-linear") && text.contains("https://mcp.example/linear"),
            "{path:?}: {text}"
        );
    }

    // The receipt row carries the per-agent outcomes.
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "linear")
        .unwrap();
    let agents: BTreeSet<&str> = row.harnesses.iter().map(|h| h.agent.as_str()).collect();
    assert_eq!(agents, ["cursor", "openclaw"].into());
    assert!(
        row.harnesses.iter().all(|h| h.state.wrote()),
        "a fresh placement: this run wrote every one of them — {row:?}"
    );

    // The applied report carried the states over the wire — as the STANDING answer both it and
    // the cache exist to give: where the entries live, one word for a file whatever sweep last
    // touched it (`placed` is the RUN's own word, and stays on the receipt above).
    let reported = plane.reported.lock().unwrap().clone();
    let (_, version, harnesses) = reported
        .iter()
        .find(|(id, ..)| id == "s_linear")
        .unwrap_or_else(|| panic!("reported: {reported:?}"));
    assert_eq!(
        version, &s.revision_id,
        "what this installation holds is the catalog revision, in the wire's own spelling"
    );
    assert_eq!(harnesses.len(), 2, "{harnesses:?}");
    assert!(
        harnesses.iter().all(|h| h.state == TargetOutcome::Current),
        "the fleet's standing picture: {harnesses:?}"
    );

    // The offline cache carries the kind + the same per-agent standing states.
    let cache = sync_status::read(&rig.fs, &rig.layout()).unwrap();
    let ds = &cache.workspaces[WS].delivered["s_linear"];
    assert_eq!(ds.kind.as_deref(), Some("mcp"));
    assert_eq!(ds.harness_states.len(), 2, "{ds:?}");
    assert!(
        ds.harness_states
            .iter()
            .all(|h| h.state == TargetOutcome::Current),
        "{ds:?}"
    );

    // And `list` answers the kind + the per-agent detail offline.
    let list = ops::list_with(
        &ctx,
        &ops::ListRequest {
            name: Some("linear".into()),
            ..Default::default()
        },
        None,
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap();
    let detail = list.data.detail.unwrap();
    assert_eq!(detail.kind.as_deref(), Some("mcp"));
    assert_eq!(detail.harnesses.len(), 2, "{detail:?}");
    assert!(detail.placements.is_empty());
}

/// **THE RECORD IS THE DELIVERY, AND A SECOND HELPING OF IT IS NOTHING.** Writing the document
/// down IS the sync for this kind, so the first sweep must leave it in the store — and a sweep
/// served the SAME revision again must move nothing at all: no config byte, and a row that rests
/// at `up to date`. A run that re-rendered every entry each sweep would churn every agent's config
/// file on the hook's timer, and every receipt would claim work that never happened.
#[test]
fn a_second_delivery_of_the_same_revision_writes_nothing_and_reads_up_to_date() {
    let rig = Rig::new("same-revision");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let s = served_at("https://mcp.example/a");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_a", "alpha", &s)]);
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_a", "alpha", &s)]);
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/alpha\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let first = sweep(&ctx, &plane, &dir);
    let record = server_record(&rig.fs, &rig.layout(), "s_a").expect("the first sweep recorded it");
    assert_eq!(record.revision_id, s.revision_id);
    assert_eq!(
        record.document["remotes"][0]["url"], "https://mcp.example/a",
        "{record:?}"
    );
    assert!(!record.pinned && !record.revoked, "{record:?}");
    let row = first
        .data
        .skills
        .iter()
        .find(|r| r.skill == "alpha")
        .unwrap();
    assert_eq!(row.action, topos_types::results::PullAction::Installed);

    // The same revision again: nothing to do anywhere.
    let files = [
        rig.home.0.join(".cursor/mcp.json"),
        rig.home.0.join(".openclaw/openclaw.json"),
    ];
    let before: Vec<Vec<u8>> = files.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let out = sweep(&ctx, &plane, &dir);
    let after: Vec<Vec<u8>> = files.iter().map(|p| std::fs::read(p).unwrap()).collect();
    assert_eq!(before, after, "a settled sweep writes no config byte");
    let row = out.data.skills.iter().find(|r| r.skill == "alpha").unwrap();
    assert_eq!(row.action, topos_types::results::PullAction::UpToDate);
    assert!(
        row.harnesses.iter().all(|h| !h.state.wrote()),
        "nothing was written, and every per-agent line says so: {row:?}"
    );
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
        out.unplaced_bundles.len(),
    );
    assert!(
        tty.starts_with("checked ") && tty.contains("Checked 1 bundle: all up to date."),
        "{tty}"
    );
}

/// **A REVISION THAT MOVES MOVES BOTH HALVES.** When the workspace publishes a new revision, the
/// sweep rewrites the RECORD and re-renders every agent's entry from it. The two must never
/// disagree: the record is the demand the next run reads, so a config carrying one document over a
/// record holding another would heal back to the old one the moment the network went away.
#[test]
fn a_moved_revision_rewrites_the_record_and_every_agents_entry() {
    let rig = Rig::new("moved-revision");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v1 = served_at("https://mcp.example/v1");
    let v2 = served_at("https://mcp.example/v2");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_a", "alpha", &v1)]);
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_a", "alpha", &v1)]);
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/alpha\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    plane.serves_servers(vec![delivered_mcp("s_a", "alpha", &v2)]);
    let dir2 = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_a", "alpha", &v2)]);
    let out = sweep(&ctx, &plane, &dir2);

    assert_eq!(
        server_record(&rig.fs, &rig.layout(), "s_a")
            .expect("the record")
            .revision_id,
        v2.revision_id,
        "the record follows the revision"
    );
    for path in [
        rig.home.0.join(".cursor/mcp.json"),
        rig.home.0.join(".openclaw/openclaw.json"),
    ] {
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        assert!(
            text.contains("https://mcp.example/v2") && !text.contains("https://mcp.example/v1"),
            "{path:?}: {text}"
        );
    }
    // The store never moved — there is no store to move — so the row's verb comes from the config
    // files: this run rewrote entries this scope had already placed, which is the ordinary
    // catch-up, never a second install.
    let row = out.data.skills.iter().find(|r| r.skill == "alpha").unwrap();
    assert_eq!(
        row.action,
        topos_types::results::PullAction::Refreshed,
        "{row:?}"
    );
}

/// **OFFLINE, THE RECORD IS THE WHOLE DEMAND.** With no delivery to be had and a catalog that
/// knows nothing, the sweep must still heal an entry a person deleted — from the document this
/// machine was last given and nothing else. That is the one thing the record exists for: the
/// bytes of a connected server never come back over the wire on a later run, so a machine that
/// could not converge from its own record could not converge at all.
#[test]
fn an_offline_sweep_heals_the_entries_from_the_record_alone() {
    let rig = Rig::new("offline-record");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let s = served_at("https://mcp.example/a");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_a", "alpha", &s)]);
    // An EMPTY catalog throughout: the feed row is the only thing that ever delivers this server,
    // so nothing below can have come from a lane.
    let dir = FakeDirectory::default();
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(cursor.exists(), "the feed placed it");

    // The entry is deleted by hand and the network dies.
    std::fs::remove_file(&cursor).unwrap();
    plane.serve_unreachable();
    let out = sweep(&ctx, &plane, &dir);

    let text = std::fs::read_to_string(&cursor).unwrap_or_else(|e| panic!("not healed: {e}"));
    assert!(
        text.contains("https://mcp.example/a") && text.contains("topos-eng-alpha"),
        "{text}"
    );
    let row = out.data.skills.iter().find(|r| r.skill == "alpha").unwrap();
    assert!(
        row.harnesses
            .iter()
            .any(|h| h.agent == "cursor" && h.state.wrote()),
        "the repaired entry says this run wrote it: {row:?}"
    );
}

/// **A WITHDRAWN REVISION IS SAID, AND STILL PLACED.** The catalog can pull a revision back after
/// publishing it; this machine is still holding it, and hiding that would leave a person to guess
/// which of "withdrawn" and "installed" is true here. So it is ONE disclosure line — never a
/// failure, never a per-agent line — and the placement happens exactly as it would have.
#[test]
fn a_withdrawn_revision_is_disclosed_once_and_still_placed() {
    let rig = Rig::new("withdrawn");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let s = served_at("https://mcp.example/a");
    let plane = FakePlane::new();
    // The row is explicit, so the CATALOG is the lane that answers for it — and the catalog
    // states the withdrawal exactly as the feed does.
    let mut entry = mcp_catalog_entry("s_a", "alpha", &s);
    entry.revoked = Some(true);
    let dir = FakeDirectory::of_servers(vec![entry]);
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/alpha\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    // Placed — in both hermetic config files, exactly as an unwithdrawn revision would be.
    for path in [
        rig.home.0.join(".cursor/mcp.json"),
        rig.home.0.join(".openclaw/openclaw.json"),
    ] {
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        assert!(text.contains("https://mcp.example/a"), "{path:?}: {text}");
    }
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(out.failed_bundles.is_empty(), "{:?}", out.failed_bundles);
    // Said ONCE, in the disclosure channel, in words a person can act on.
    let said: Vec<String> = crate::message::legacy_lines(&out.disclosures)
        .into_iter()
        .filter(|d| d.contains("MCP_REVISION_WITHDRAWN"))
        .collect();
    assert_eq!(
        said,
        vec![
            "MCP_REVISION_WITHDRAWN alpha: the version topos places here was withdrawn from the \
             catalog."
                .to_owned()
        ],
        "{:?}",
        out.disclosures
    );
    // And the record carries the fact, so every later run says the same thing offline.
    assert!(
        server_record(&rig.fs, &rig.layout(), "s_a")
            .expect("the record")
            .revoked
    );
}

/// **WHAT THIS INSTALLATION HOLDS, PER KIND.** One report, two spellings: a connected server holds
/// a catalog revision (`mcpr_…`) and a file bundle holds the commit of its bytes. They ride the
/// same field because a report is one snapshot of what a machine holds — and what a version IS
/// belongs to the kind, so a reader that assumed 64 hex characters would drop every server on the
/// fleet page.
#[test]
fn the_applied_report_carries_a_revision_for_a_server_and_a_commit_for_a_skill() {
    let rig = Rig::new("report-kinds");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let s = served_at("https://mcp.example/a");
    let v = mk_version(&[("SKILL.md", b"# deploy\n")]);
    let plane = FakePlane::new().with_version("s_dep", &v);
    plane.serves_servers(vec![delivered_mcp("s_a", "alpha", &s)]);
    plane.serves(vec![delivered_skill("s_dep", "deploy", &v)]);
    let dir = FakeDirectory::default();
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    let reported = plane.reported.lock().unwrap().clone();
    let held = |id: &str| {
        reported
            .iter()
            .find(|(reported_id, ..)| reported_id == id)
            .unwrap_or_else(|| panic!("{id} reported: {reported:?}"))
            .1
            .clone()
    };
    assert_eq!(held("s_a"), s.revision_id);
    assert_eq!(held("s_dep"), topos_core::digest::to_hex(&v.id));
}

/// **A DOCUMENT THIS MACHINE CANNOT READ FAILS THE BUNDLE.** The delivery answer carries the
/// document itself, so a row whose document cannot be read at all is a row nothing can ever be
/// placed for. It is a FAILURE, not a skip: the bundle is counted, the line names it, and its
/// standing entries are HELD rather than read as undemanded — a bundle that vanished quietly
/// would leave the receipt saying "all up to date" over a machine that received nothing.
#[test]
fn a_served_document_that_cannot_be_read_fails_the_bundle() {
    let rig = Rig::new("unreadable-document");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let s = served_at("https://mcp.example/a");
    let mut served = delivered_mcp("s_a", "alpha", &s);
    served.document = b"<not a document>".to_vec();
    let plane = FakePlane::new();
    plane.serves_servers(vec![served]);
    // The FEED delivers it (the catalog knows nothing), so the bytes this machine tries to record
    // are the ones the delivery carried.
    let dir = FakeDirectory::default();
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    // Nothing was recorded and nothing was placed.
    assert!(server_record(&rig.fs, &rig.layout(), "s_a").is_none());
    assert!(!rig.home.0.join(".cursor/mcp.json").exists());
    assert!(!rig.home.0.join(".openclaw/openclaw.json").exists());
    // The bundle is COUNTED, under its identity, and the line names it.
    assert_eq!(
        out.failed_bundles,
        [("person".to_owned(), "s_a".to_owned())]
            .into_iter()
            .collect(),
        "{:?}",
        out.warnings
    );
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.contains("alpha")),
        "{:?}",
        out.warnings
    );
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
        out.unplaced_bundles.len(),
    );
    assert!(
        !tty.contains("already up to date"),
        "a run that received nothing never claims it was current: {tty}"
    );
}

/// ITEM (the subscribe receipt's typed block): `topos add` of a workspace mcp bundle — the same
/// act a workspace mcp reference rides — delivers in the same invocation and folds
/// the typed `mcp` block from the document that LANDED: the embedded identity, the endpoint,
/// the narrowed agents — and NO `bundle` folder, because a workspace bundle's bytes live in the
/// store.
#[test]
fn a_workspace_mcp_subscribe_receipt_carries_the_typed_block() {
    // PROJECT scope, so the breadth is hermetic without narrowing (a fresh subscribe row spells
    // no `dest`, and project surfaces are checkout-relative whatever the dev env sets): the four
    // project-capable agents engage deterministically — claude-code + cursor via detection under
    // the fake home, codex + opencode via their seeded project files.
    let rig = Rig::new("sub-receipt");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    let proj = Scratch::new("sub-receipt-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::create_dir_all(proj.0.join(".codex")).unwrap();
    std::fs::write(proj.0.join(".codex/config.toml"), b"").unwrap();
    std::fs::write(proj.0.join("opencode.json"), b"").unwrap();
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), "schema = 1\n").unwrap();
    let s = served_at("https://mcp.example/linear");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_linear", "linear", &s)]);
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_linear", "linear", &s)]);
    rig.write_global("schema = 1\n");
    let ctx = rig.ctx_at(Some(&proj.0));

    let outcome = ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("{HOST}/{WS_NAME}/linear"),
        false,
        false,
        &Default::default(),
        None,
    )
    .unwrap();
    let ops::AddRefOutcome::Applied { data, .. } = outcome else {
        panic!("a workspace subscribe applies");
    };
    let mcp = data.mcp.expect("the receipt carries the typed block");
    assert_eq!(
        mcp.server, "io.test/x",
        "the EMBEDDED identity, not the catalog name"
    );
    assert_eq!(mcp.url, "https://mcp.example/linear");
    assert_eq!(mcp.bundle, None, "no folder — the bytes live in the store");
    assert_eq!(
        mcp.agents.len(),
        4,
        "the project-scope breadth line: {:?}",
        mcp.agents
    );
    let note = data.note.clone().unwrap_or_default();
    assert!(note.contains("MCP server io.test/x"), "{note}");
}

#[test]
fn offline_sweeps_still_heal_configs_from_the_store() {
    let rig = Rig::new("offline");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let s = served_at("https://mcp.example/a");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_a", "alpha", &s)]);
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_a", "alpha", &s)]);
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/alpha\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(cursor.exists());

    // The network dies AND the entry is lost locally (the file deleted by hand): the next sweep
    // converges from the RECORD the last delivery wrote — no dial needed.
    std::fs::remove_file(&cursor).unwrap();
    plane.serve_unreachable();
    let out = sweep(&ctx, &plane, &dir);
    let text = std::fs::read_to_string(&cursor).unwrap_or_else(|e| panic!("not healed: {e}"));
    assert!(text.contains("https://mcp.example/a"), "{text}");
    // And the healed placement is disclosed on the row.
    let row = out.data.skills.iter().find(|s| s.skill == "alpha").unwrap();
    assert!(
        row.harnesses
            .iter()
            .any(|h| h.agent == "cursor" && h.state.wrote()),
        "the repaired entry says this run wrote it: {row:?}"
    );
}

/// THE RECEIPT'S OWN VERB, AND WHAT IT COUNTS. A run that puts a hand-deleted config entry back
/// CHANGED this machine, and the header must say so: the store never moved (the sync reads "up to
/// date"), so a header built from the sync's answer alone said `checked` over a run that rewrote a
/// person's agent config. A genuinely idle sweep still says `checked`, and the repaired row reads
/// as the ordinary catch-up it is.
///
/// The row is then held to ONE truth on both channels, over TWO config files of which this run
/// wrote exactly one: the destination column and `destinations` name that ONE file (never the one
/// that merely already held the entry), the file list prints ONCE (the per-agent lines carry it,
/// and say what happened in each), and the JSON tells the two files apart exactly as the TTY does
/// — `placed` beside `current`, never one word for both.
#[test]
fn a_repaired_config_entry_makes_the_run_an_update_not_a_check() {
    let rig = Rig::new("repair");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let s = served_at("https://mcp.example/a");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_a", "alpha", &s)]);
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_a", "alpha", &s)]);
    // TWO hermetic config files: only one of them is deleted below, so the receipt has something
    // to be wrong about.
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/alpha\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let tty = |out: &ops::PullOutcome| {
        crate::render::pull_tty(
            &out.data,
            &out.decisions,
            &out.warnings,
            &out.advisories,
            &out.disclosures,
            out.failed_bundles.len(),
            out.unplaced_bundles.len(),
        )
    };
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(cursor.exists());

    // A sweep that finds everything in place only LOOKED.
    let idle = tty(&sweep(&ctx, &plane, &dir));
    assert!(idle.starts_with("checked "), "{idle}");

    // ONE entry is deleted by hand; the next sweep writes THAT ONE back.
    std::fs::remove_file(&cursor).unwrap();
    let out = sweep(&ctx, &plane, &dir);
    assert!(cursor.exists(), "the config was not repaired");
    let row = out.data.skills.iter().find(|s| s.skill == "alpha").unwrap();
    assert_eq!(
        row.action,
        topos_types::results::PullAction::Refreshed,
        "{row:?}"
    );
    // THE ROW BLOCK, byte for byte. The column counts the config files it HEADS — the same two
    // its detail lines name — never the subset this run wrote: a count over a longer list is a
    // number the reader has to reconcile with the lines under it. Which file moved is the lines'
    // own job, and they say it.
    assert_eq!(
        tty(&out),
        "updated machine-wide\n\
         alpha   updated (2 config files)\n    \
             ~/.openclaw/openclaw.json: unchanged\n    \
             ~/.cursor/mcp.json: created — restart Cursor\n\
         Checked 1 bundle: 1 updated."
    );
    // The wire says the same thing the receipt does: the written file, and only it, is the
    // destination — and the two files' states differ.
    assert_eq!(row.destinations, vec!["~/.cursor/mcp.json".to_owned()]);
    let states: Vec<(&str, TargetOutcome)> = row
        .harnesses
        .iter()
        .map(|h| (h.agent.as_str(), h.state))
        .collect();
    assert_eq!(
        states,
        vec![
            ("openclaw", TargetOutcome::Current),
            // The entry was hand-DELETED, so this run put one where none stood: `created` at the
            // target, while the ROW above is still an `updated` — the bundle was already
            // installed here. Two levels, both true.
            ("cursor", TargetOutcome::Created),
        ],
        "the rewritten file and the merely-found one are distinguishable: {row:?}"
    );
}

/// A FIRST-EVER PLACEMENT IS AN INSTALL. A brand-new `kind = "mcp"` manifest line syncs
/// store-only — nothing to place, so the sync answers `up to date` — and the converge then writes
/// the config entries for the first time. That is not a repair of anything: the row must lead
/// with `+` and read `installed`, exactly as a delivered mcp bundle's first row does. The scope's
/// custody is the durable signal, so the SAME bundle re-healed later reads `updated` instead.
#[test]
fn a_first_ever_mcp_placement_reads_installed_not_a_repair() {
    let rig = Rig::new("first");
    seed_harness_dirs(&rig.home.0);
    let src = rig.home.0.join("weather");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("server.json"),
        server_json("https://w.example/mcp").as_bytes(),
    )
    .unwrap();
    rig.write_global(&format!(
        "[mcp]\nweather = {{ path = \"{}\", kind = \"mcp\", {SAFE} }}\n",
        src.display()
    ));
    let plane = FakePlane::new();
    let dir = FakeDirectory::default();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let tty = |out: &ops::PullOutcome| {
        crate::render::pull_tty(
            &out.data,
            &out.decisions,
            &out.warnings,
            &out.advisories,
            &out.disclosures,
            out.failed_bundles.len(),
            out.unplaced_bundles.len(),
        )
    };

    let out = sweep(&ctx, &plane, &dir);
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "weather")
        .unwrap();
    assert_eq!(
        row.action,
        topos_types::results::PullAction::Installed,
        "{row:?}"
    );
    assert_eq!(
        tty(&out),
        // Agent lines follow the ONE harness table's row order.
        "updated machine-wide\n\
         + weather   installed (2 config files)\n    \
             ~/.openclaw/openclaw.json: created — picked up automatically; sign in with \
             `openclaw mcp login <name>`\n    \
             ~/.cursor/mcp.json: created — restart Cursor\n\
         Checked 1 bundle: 1 installed."
    );

    // The next sweep has nothing to do at all, and a later re-heal of the SAME bundle is the
    // repair — `updated`, never a second install.
    assert!(tty(&sweep(&ctx, &plane, &dir)).starts_with("checked "));
    std::fs::remove_file(rig.home.0.join(".cursor/mcp.json")).unwrap();
    let healed = sweep(&ctx, &plane, &dir);
    let row = healed
        .data
        .skills
        .iter()
        .find(|s| s.skill == "weather")
        .unwrap();
    assert_eq!(
        row.action,
        topos_types::results::PullAction::Refreshed,
        "a bundle this scope had already placed re-heals as a repair: {row:?}"
    );
}

#[test]
fn a_channel_drop_removes_the_entries_everywhere() {
    let rig = Rig::new("chdrop");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let s = served_at("https://mcp.example/a");
    let plane = FakePlane::new();
    let dir = FakeDirectory {
        servers: vec![mcp_catalog_entry("s_a", "alpha", &s)],
        channels: vec![WireChannelEntry {
            name: "tools".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: vec![WireChannelSkill {
                skill_id: "s_a".into(),
                name: "alpha".into(),
            }],
        }],
        ..FakeDirectory::default()
    };
    rig.write_global(&format!(
        "[channels]\n\"{HOST}/{WS_NAME}/tools\" = {{ {SAFE_CHANNEL} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("topos-eng-alpha")
    );

    // The channel stops carrying the bundle: the next sweep's removal convergence clears every
    // config entry (the channel still exists and expands — to nothing).
    let dir = FakeDirectory {
        servers: vec![mcp_catalog_entry("s_a", "alpha", &s)],
        channels: vec![WireChannelEntry {
            name: "tools".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: Vec::new(),
        }],
        ..FakeDirectory::default()
    };
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !cursor.exists()
            || !std::fs::read_to_string(&cursor)
                .unwrap()
                .contains("topos-eng-alpha"),
        "the entry left cursor's config"
    );
    // The receipt says it as a `removed` ROW counting the config FILES the entries left —
    // destinations, never agents.
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "alpha" && s.action == topos_types::results::PullAction::Removed)
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert_eq!(row.kind.as_deref(), Some("mcp"));
    assert!(
        row.destinations
            .iter()
            .any(|d| d.ends_with(".cursor/mcp.json")),
        "{:?}",
        row.destinations
    );
    let custody = ScopeEntries::load(&rig.fs, &rig.layout()).unwrap();
    assert!(!custody.has_entries_for("s_a"));
}

#[test]
fn a_project_row_lands_only_in_project_surfaces_and_openclaw_hermes_read_not_supported() {
    let rig = Rig::new("project");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    // claude-code + codex + opencode engage hermetically at PROJECT scope: their project surfaces
    // are checkout-relative. Seed codex's and opencode's project files so they engage without a
    // detect dir (opencode's sits at the checkout ROOT).
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    let proj = Scratch::new("project-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::create_dir_all(proj.0.join(".codex")).unwrap();
    std::fs::write(proj.0.join(".codex/config.toml"), b"").unwrap();
    std::fs::write(proj.0.join("opencode.json"), b"").unwrap();
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[mcp]\nlinear = \"latest\"\n"),
    )
    .unwrap();
    let s = served_at("https://mcp.example/linear");
    let plane = FakePlane::new();
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_linear", "linear", &s)]);
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);

    // The FOUR project surfaces (and nothing under the home).
    for rel in [
        ".mcp.json",
        ".codex/config.toml",
        ".cursor/mcp.json",
        "opencode.json",
    ] {
        let text =
            std::fs::read_to_string(proj.0.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert!(text.contains("topos-eng-linear"), "{rel}: {text}");
    }
    assert!(
        !rig.home.0.join(".cursor/mcp.json").exists(),
        "person scope untouched"
    );

    // openclaw / hermes have no project-level config: withheld, honestly.
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "linear")
        .unwrap();
    for slug in ["openclaw", "hermes-agent"] {
        let st = row
            .harnesses
            .iter()
            .find(|h| h.agent == slug)
            .unwrap_or_else(|| panic!("{slug} state: {row:?}"));
        assert_eq!(st.state, TargetOutcome::Withheld, "{slug}");
        assert_eq!(
            st.note.as_deref(),
            Some("no project-level config"),
            "{slug}"
        );
    }
    // The custody lives in the PROJECT store.
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0).unwrap();
    assert!(playout.config_custody_path().exists());
    assert!(!rig.layout().config_custody_path().exists());
}

#[test]
fn a_project_config_symlink_escaping_the_checkout_is_refused_and_disclosed() {
    let rig = Rig::new("escape");
    rig.seed_session();
    let outside = Scratch::new("escape-outside");
    let proj = Scratch::new("escape-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    // `.cursor` is a committed symlink aiming OUT of the checkout.
    std::os::unix::fs::symlink(&outside.0, proj.0.join(".cursor")).unwrap();
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[mcp]\nlinear = {{ dest = [\".cursor/mcp.json\"] }}\n"),
    )
    .unwrap();
    let s = served_at("https://mcp.example/linear");
    let plane = FakePlane::new();
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_linear", "linear", &s)]);
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.contains("PLACEMENT_ESCAPES_PROJECT")),
        "{:?}",
        out.warnings
    );
    assert!(
        std::fs::read_dir(&outside.0).unwrap().next().is_none(),
        "nothing may land outside the checkout"
    );
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "linear")
        .unwrap();
    assert!(
        row.harnesses
            .iter()
            .any(|h| h.agent == "cursor" && h.state == TargetOutcome::Unprovable),
        "{row:?}"
    );
}

#[test]
fn a_rows_dest_files_narrow_the_placement_and_unknown_files_warn_once() {
    let rig = Rig::new("narrow");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let s = served_at("https://mcp.example/a");
    let plane = FakePlane::new();
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_a", "alpha", &s)]);
    // BOTH hermetic agents are detected; the row's dest names cursor's file alone, plus a file
    // no harness claims.
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/alpha\" = {{ dest = [\"~/.cursor/mcp.json\", \"~/.notepad/mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(rig.home.0.join(".cursor/mcp.json").exists());
    assert!(
        !rig.home.0.join(".openclaw/openclaw.json").exists(),
        "a dest row is frozen to the files it names — detection adds nothing"
    );
    // The line is an ADVISORY — the bundle itself delivered, so it must never join the counted
    // failure channel.
    assert!(
        !crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.contains("MCP_DEST_UNKNOWN")),
        "an advisory about a delivered bundle is not a counted failure: {:?}",
        out.warnings
    );
    let unknown: Vec<String> = crate::message::legacy_lines(&out.advisories)
        .into_iter()
        .filter(|w| w.contains("MCP_DEST_UNKNOWN"))
        .collect();
    assert_eq!(
        unknown.len(),
        1,
        "one warning per unknown file: {unknown:?}"
    );
    assert!(
        unknown[0].contains(
            "the dest entry `~/.notepad/mcp.json` in your topos.toml is not a known MCP config \
             file, so it was skipped."
        ),
        "{unknown:?}"
    );
    assert!(
        unknown[0].contains("~/.codex/config.toml"),
        "the refusal lists the known files: {unknown:?}"
    );
    // The warning's subject is the BUNDLE the row delivers — never the bare scope label standing
    // where the name belongs.
    assert!(
        unknown[0].contains("\"alpha\""),
        "the warning names the bundle: {unknown:?}"
    );
    // And the counts reconcile: ONE bundle was checked. A delivered bundle with a config nit is
    // not a second, failed bundle — the warning still prints, uncounted.
    let clean = sweep(&ctx, &plane, &dir);
    let receipt = crate::render::pull_tty(
        &clean.data,
        &clean.decisions,
        &clean.warnings,
        &clean.advisories,
        &clean.disclosures,
        clean.failed_bundles.len(),
        clean.unplaced_bundles.len(),
    );
    assert!(
        receipt.contains("warning: \"alpha\" (person): the dest entry `~/.notepad/mcp.json`"),
        "still warned: {receipt}"
    );
    assert!(!receipt.contains("MCP_DEST_UNKNOWN"), "{receipt}");
    assert!(
        receipt.contains("Checked 1 bundle: all up to date."),
        "{receipt}"
    );
    assert!(
        !receipt.contains("failed"),
        "a delivered bundle is not a failure: {receipt}"
    );
}

/// A dest row is FROZEN to what it names — so a row that names ONLY files no harness claims
/// (one typo) costs the bundle every agent. That is fail-closed and stays fail-closed: nothing is
/// placed anywhere. What must not stay is the SILENCE. It is a counted warning naming the entry
/// and the files that would have worked, the receipt row says the bundle reaches no agent instead
/// of printing a bare install, and `list` says the same rather than "no entries recorded yet" —
/// which would promise entries that are never coming.
#[test]
fn a_dest_naming_only_unknown_files_reaches_no_agent_and_says_so() {
    let rig = Rig::new("dest-none");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let s = served_at("https://mcp.example/a");
    let plane = FakePlane::new();
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_a", "alpha", &s)]);
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/alpha\" = {{ dest = [\"~/.codex/config.yaml\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    // Fail-closed: not one config file was written, detected agents and all.
    assert!(!rig.home.0.join(".cursor/mcp.json").exists());
    assert!(!rig.home.0.join(".openclaw/openclaw.json").exists());
    assert!(!rig.home.0.join(".codex/config.toml").exists());

    // LOUD: the counted channel, not the advisory one — the bundle delivered nowhere.
    let loud: Vec<String> = crate::message::legacy_lines(&out.warnings)
        .into_iter()
        .filter(|w| w.contains("MCP_DEST_NO_AGENT"))
        .collect();
    assert_eq!(loud.len(), 1, "{:?}", out.warnings);
    assert!(loud[0].contains("\"alpha\" reaches no agent"), "{loud:?}");
    assert!(
        loud[0].contains(
            "the dest entry \"~/.codex/config.yaml\" in your topos.toml is not a known MCP config \
             file."
        ),
        "{loud:?}"
    );
    assert!(
        loud[0].contains("~/.codex/config.toml") && loud[0].contains("~/.cursor/mcp.json"),
        "the line teaches the files that would have worked: {loud:?}"
    );
    assert!(
        !crate::message::legacy_lines(&out.advisories)
            .into_iter()
            .any(|w| w.contains("MCP_DEST")),
        "a bundle reaching nothing is never filed as an advisory: {:?}",
        out.advisories
    );
    // The TTY prints the message's TEXT, which carries no code at all — and the legacy line is
    // that text with the code put back on the front. It used to lead with the scope label, so the
    // receipt said `person:` — the resolver's word for the machine-wide scope, never a person's.
    let typed: Vec<&topos_types::Message> = out
        .warnings
        .iter()
        .filter(|w| w.code.as_deref() == Some("MCP_DEST_NO_AGENT"))
        .collect();
    assert_eq!(typed.len(), 1, "{:?}", out.warnings);
    assert_eq!(
        typed[0].text,
        loud[0]["MCP_DEST_NO_AGENT ".len()..],
        "{loud:?}"
    );
    assert!(
        typed[0].text.starts_with("\"alpha\" reaches no agent"),
        "{loud:?}"
    );

    // The ROW says it too — on the settled sweep, where the row is otherwise up to date and a
    // compact receipt would have dropped it entirely.
    let clean = sweep(&ctx, &plane, &dir);
    let receipt = crate::render::pull_tty(
        &clean.data,
        &clean.decisions,
        &clean.warnings,
        &clean.advisories,
        &clean.disclosures,
        clean.failed_bundles.len(),
        clean.unplaced_bundles.len(),
    );
    assert!(
        receipt.contains("alpha") && receipt.contains("reaches no agent"),
        "{receipt}"
    );

    // And so does the deep dive, offline.
    let list = ops::list_with(
        &ctx,
        &ops::ListRequest {
            name: Some("alpha".into()),
            ..Default::default()
        },
        None,
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap();
    let detail = list.data.detail.clone().unwrap();
    assert!(
        detail
            .mcp_unreachable
            .as_deref()
            .is_some_and(|w| w.contains("~/.codex/config.yaml")),
        "{detail:?}"
    );
    let text = crate::render::list_tty(&list);
    assert!(
        text.contains("an MCP server bundle that reaches no agent"),
        "{text}"
    );
    assert!(
        !text.contains("no agent config entries recorded yet"),
        "nothing is coming — the line must not promise entries: {text}"
    );
}

/// **A live entry outranks the row's arithmetic.** An entry topos placed and then found
/// hand-edited is LEFT in place — drift is never clobbered — and it keeps its custody entry. If the
/// row's `dest` is later changed to files topos cannot edit, the reach arithmetic says "no agent"
/// while that entry is still sitting in the config, quite possibly still being loaded by the agent.
/// `list` must not tell that lie: the per-agent states say where the bytes actually are, and the
/// sweep's own MCP_DEST_NO_AGENT warning stays the causality carrier.
#[test]
fn a_drifted_entry_keeps_list_from_claiming_the_bundle_reaches_no_agent() {
    let rig = Rig::new("dest-none-drift");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let s = served_at("https://mcp.example/a");
    let plane = FakePlane::new();
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_a", "alpha", &s)]);
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/alpha\" = {{ dest = [\"~/.cursor/mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");

    // The person edits the placed entry by hand.
    let edited = std::fs::read_to_string(&cursor)
        .unwrap()
        .replace("https://mcp.example/a", "https://my-fork.example");
    std::fs::write(&cursor, &edited).unwrap();

    // …and then the row's dest is changed to a file topos cannot edit. The sweep warns, and leaves
    // the hand edit exactly where it is.
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/alpha\" = {{ dest = [\"~/.codex/config.yaml\"] }}\n"
    ));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.contains("MCP_DEST_NO_AGENT")),
        "the causality still rides the sweep warning: {:?}",
        out.warnings
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        edited,
        "the hand edit is never clobbered"
    );

    let list = ops::list_with(
        &ctx,
        &ops::ListRequest {
            name: Some("alpha".into()),
            ..Default::default()
        },
        None,
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap();
    let detail = list.data.detail.clone().unwrap();
    assert_eq!(
        detail.mcp_unreachable, None,
        "something of this bundle's is still in a config: {detail:?}"
    );
    assert!(
        detail.harnesses.iter().any(|h| h.agent == "cursor"),
        "the states name the agent whose config still holds it: {detail:?}"
    );
    let text = crate::render::list_tty(&list);
    assert!(
        !text.contains("reaches no agent"),
        "an entry still in a config file is not 'no agent': {text}"
    );
}

/// Two bundles, ONE typo'd spelling: the first still delivers (the typo is an advisory beside a
/// working row), the second reaches nothing. The once-per-run dedupe must not let the first one's
/// advisory swallow the second one's warning — they answer different questions about different
/// bundles, and the swallowed one is the one that means "this reaches nobody".
#[test]
fn one_bundles_dest_advisory_never_swallows_anothers_reaches_no_agent() {
    let rig = Rig::new("dest-dedupe");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let sa = served_at("https://mcp.example/a");
    let sb = served_at("https://mcp.example/b");
    let plane = FakePlane::new();
    let dir = FakeDirectory::of_servers(vec![
        mcp_catalog_entry("s_a", "alpha", &sa),
        mcp_catalog_entry("s_b", "beta", &sb),
    ]);
    rig.write_global(&format!(
        "[mcp]\n\
         \"{HOST}/{WS_NAME}/alpha\" = {{ dest = [\"~/.cursor/mcp.json\", \"~/.typo/mcp.json\"] }}\n\
         \"{HOST}/{WS_NAME}/beta\" = {{ dest = [\"~/.typo/mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    assert!(
        crate::message::legacy_lines(&out.advisories)
            .into_iter()
            .any(|w| w.contains("MCP_DEST_UNKNOWN") && w.contains("\"alpha\"")),
        "the delivering bundle's dropped entry stays an advisory: {:?}",
        out.advisories
    );
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.contains("MCP_DEST_NO_AGENT") && w.contains("\"beta\" reaches no agent")),
        "the bundle reaching nothing is warned in its own right: {:?}",
        out.warnings
    );
}

/// AN `add` NEVER NARROWS — and on a bundle a CHANNEL already delivers, the honest row is no row
/// at all: the demand stands, and an explicit `dest` row would cost the bundle every agent the
/// channel reaches that the person did not happen to name. So the invocation writes nothing and
/// converges instead, which is what actually puts the newly installed agent's copy there.
///
/// Both answers ride one fixture: a surface that was MISSING the copy gets it, and the destination
/// that was already current says so and closes on `nothing changed`.
#[test]
fn a_set_delivered_add_writes_no_row_and_converges_the_missing_copy() {
    let rig = Rig::new("set-delivered");
    rig.seed_session();
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    // A checkout UNDER the home, the ordinary shape: the converge's paths come back
    // `~`-abbreviated, and the receipt still writes them against the folder it is about.
    let proj = Scratch(rig.home.0.join("work/api"));
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::create_dir_all(proj.0.join(".codex")).unwrap();
    std::fs::write(proj.0.join(".codex/config.toml"), b"").unwrap();
    let s = served_at("https://mcp.example/sentry");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_sentry", "sentry", &s)]);
    let dir = FakeDirectory {
        servers: vec![mcp_catalog_entry("s_sentry", "sentry", &s)],
        channels: vec![WireChannelEntry {
            name: "everyone".into(),
            mode: "open".into(),
            builtin: true,
            included: true,
            skills: vec![WireChannelSkill {
                skill_id: "s_sentry".into(),
                name: "sentry".into(),
            }],
        }],
        ..FakeDirectory::default()
    };
    rig.write_global("schema = 1\n");
    let manifest = proj.0.join(crate::manifest::MANIFEST_FILE);
    let channel_row =
        format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[channels]\neveryone = \"latest\"\n");
    std::fs::write(&manifest, &channel_row).unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep(&ctx, &plane, &dir);
    assert!(proj.0.join(".mcp.json").exists());
    assert!(proj.0.join(".codex/config.toml").exists());

    // A NEW agent is installed after the channel row was written.
    std::fs::write(proj.0.join("opencode.json"), b"").unwrap();
    let add = |agent: &str| {
        let outcome = ops::add_reference(
            &ctx,
            &connect(&plane, &dir),
            None,
            &format!("{HOST}/{WS_NAME}/sentry"),
            false,
            false,
            &crate::ops::dest_select::Selection::new(&[agent.to_owned()], &[]),
            None,
        )
        .unwrap();
        let ops::AddRefOutcome::Applied { data, .. } = outcome else {
            panic!("a set-delivered add applies");
        };
        data
    };

    // A NON-ASKED surface is not rewritten by an add about another one: the converge is
    // idempotent, and a byte moving here would be a config file edited for nobody's benefit.
    let untouched = proj.0.join(".mcp.json");
    let before_bytes = std::fs::read(&untouched).unwrap();

    let data = add("opencode");
    assert_eq!(
        std::fs::read(&untouched).unwrap(),
        before_bytes,
        "claude-code's surface was not asked about and is byte-identical"
    );
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        channel_row,
        "no row is written — the channel already demands it"
    );
    assert_eq!(data.manifest, None);
    assert!(data.undo.is_empty(), "nothing was recorded to invert");
    assert_eq!(
        crate::render::add_tty(&data),
        format!(
            "sentry already reaches every agent here through channels/everyone — no row to \
             record.\nPlaced the copy that was missing:\n  opencode: opencode.json — \
             created\nsource: {HOST}/{WS_NAME}/sentry"
        ),
        "{data:?}"
    );
    let placed = std::fs::read_to_string(proj.0.join("opencode.json")).unwrap();
    assert!(placed.contains("https://mcp.example/sentry"), "{placed}");

    // The destination that already had it: the answer is about THAT agent, and it closes on the
    // ordinary `nothing changed`.
    let data = add("claude-code");
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        channel_row,
        "still no row"
    );
    assert_eq!(
        crate::render::add_tty(&data),
        "sentry already reaches claude-code through channels/everyone (.mcp.json — \
         current).\nnothing changed",
        "{data:?}"
    );
    // `--json` rides the ordinary add envelope, carrying the same facts typed.
    let json = serde_json::to_value(&data).unwrap();
    assert_eq!(json["set_delivery"]["set"], "channels/everyone");
    assert_eq!(json["set_delivery"]["asked"][0], "claude-code");
    assert!(json.get("manifest").is_none(), "{json}");

    // AN AGENT THIS MACHINE DOES NOT RUN gets no row either — and no sentence claiming the bundle
    // reaches it. The surface reads in the standing `not placed` vocabulary, with its reason.
    let data = add("cursor");
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        channel_row,
        "still no row"
    );
    assert_eq!(
        crate::render::add_tty(&data),
        "sentry already reaches every agent here through channels/everyone — no row to \
         record.\n  cursor: not placed — it is not set up here\nnothing changed",
        "{data:?}"
    );
}

/// A DELIBERATE NARROWING STAYS LOUD. A row edited by hand to name one config file costs every
/// other agent its entry, and that is the one thing on an update receipt a person cannot infer
/// from what arrived. It leads: what the bundle still delivers to, then one `agent: file` line per
/// entry that left — and the bare `- <name> removed (2 config files)` row goes, because it is the
/// same list a second time without the half that matters.
///
/// Before this, the whole loss read as `removed (2 config files)` under a name, with nothing
/// saying the server still stood anywhere.
#[test]
fn a_hand_narrowed_row_leads_the_receipt_with_the_entries_it_retired() {
    let rig = Rig::new("narrow-lead");
    rig.seed_session();
    // PROJECT scope, where every surface is checkout-relative and hermetic: claude-code detects
    // under the fake home, codex + opencode through their seeded project files.
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    let proj = Scratch::new("narrow-lead-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::create_dir_all(proj.0.join(".codex")).unwrap();
    std::fs::write(proj.0.join(".codex/config.toml"), b"").unwrap();
    std::fs::write(proj.0.join("opencode.json"), b"").unwrap();
    let s = served_at("https://mcp.example/sentry");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_sentry", "sentry", &s)]);
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_sentry", "sentry", &s)]);
    rig.write_global("schema = 1\n");
    let row = |body: &str| {
        std::fs::write(
            proj.0.join(crate::manifest::MANIFEST_FILE),
            format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[mcp]\nsentry = {body}\n"),
        )
        .unwrap();
    };
    row("\"latest\"");
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep(&ctx, &plane, &dir);
    assert!(proj.0.join(".mcp.json").exists());
    assert!(proj.0.join("opencode.json").exists());

    row("{ dest = [\"opencode.json\"] }");
    let out = sweep(&ctx, &plane, &dir);
    let receipt = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
        out.unplaced_bundles.len(),
    );
    assert!(
        receipt.contains(
            "sentry now delivers only to project/opencode.json — removed its entries from:\n  \
             claude-code: project/.mcp.json\n  codex: project/.codex/config.toml\n"
        ),
        "{receipt}"
    );
    assert!(
        !receipt.contains("removed (2 config files)"),
        "the loss is said once: {receipt}"
    );
    // AND THE SUMMARY AGREES WITH IT. `removed` over a bundle the lead just said still delivers
    // contradicted the receipt two lines above; the word for a shrunken reach is `narrowed`.
    assert!(
        receipt.contains("Checked 1 bundle: 1 narrowed."),
        "{receipt}"
    );
}

/// **The whole flip, end to end.** A workspace that reaches a server on the agent's behalf
/// delivers a document saying so; the sweep carries the session that delivery ran under into the
/// demand, and the entry lands as THIS binary relaying that address — the document recorded
/// exactly as it arrived, the credential never in it, never on the wire, and never in the
/// rendered config either: the relay reads it from the session store at call time.
#[test]
fn a_delivered_gateway_server_is_placed_as_this_binarys_own_relay() {
    let rig = Rig::new("gateway-sweep");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let s = served(&gateway_server_json("https://gw.example/sn_1/srv_linear"));
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_linear", "linear", &s)]);
    let dir = FakeDirectory::of_servers(vec![mcp_catalog_entry("s_linear", "linear", &s)]);
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/linear\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.failed_bundles.is_empty(), "{:?}", out.warnings);

    // The record holds the delivered bytes and nothing else: no credential arrived with them.
    let record = server_record(&rig.fs, &rig.layout(), "s_linear").expect("the record");
    let stored = serde_json::to_string(&record.document).unwrap();
    assert!(
        !stored.contains("cred-1") && stored.contains("sh.topos/gateway"),
        "{stored}"
    );

    // Both hermetic configs run the relay at the workspace's address — this test binary's own
    // path, since the sweep names the program that is running it — and carry no credential.
    let this_binary = std::env::current_exe().expect("this test binary has a path");
    let this_binary = this_binary.to_str().expect("a UTF-8 path");
    for path in [
        rig.home.0.join(".cursor/mcp.json"),
        rig.home.0.join(".openclaw/openclaw.json"),
    ] {
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        assert!(
            text.contains("https://gw.example/sn_1/srv_linear"),
            "{path:?}: {text}"
        );
        assert!(
            text.contains("relay") && text.contains(this_binary),
            "the entry runs this binary's relay: {path:?}: {text}"
        );
        assert!(
            !text.contains("cred-1"),
            "no rendered byte carries the session credential: {path:?}: {text}"
        );
        assert!(
            !text.contains("\"auth\""),
            "the document's own word was `oauth`, and OpenClaw spells that key — an entry the \
             workspace already signs has no sign-in left to start: {path:?}: {text}"
        );
    }
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "linear")
        .unwrap();
    assert!(
        row.harnesses.iter().all(|h| h.state.wrote()),
        "a fresh placement everywhere the row reaches: {row:?}"
    );
}

/// A CONNECTED SERVER THE FEED ALREADY DELIVERS answers a bare `add` exactly as a skill on the
/// same feed does. `topos list --remote` prints `topos add <name>` under every catalog row; for a
/// server that command used to fail with
/// `'deepwiki' has no recorded placement to re-fork — adopt it by path instead` (exit 1), because
/// the keep-as-yours ladder read a server bundle's empty placement map as agent folders a
/// withdrawal had cleaned. A server has no folder to re-fork — what it puts here is one entry per
/// agent MCP config — so the ladder hands it back and the ordinary standing resolution answers.
#[test]
fn a_bare_add_of_a_feed_delivered_server_answers_the_way_a_skill_does() {
    let rig = Rig::new("bare-add-server");
    rig.seed_session();
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    let s = served_at("https://mcp.example/deepwiki");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_deep", "deepwiki", &s)]);
    let dir = FakeDirectory {
        servers: vec![mcp_catalog_entry("s_deep", "deepwiki", &s)],
        ..FakeDirectory::default()
    };
    // The machine's own file after a login: the feed row, and nothing else.
    rig.write_global(&format!(
        "schema = 1\n\n[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n"
    ));
    let ctx = rig.ctx_at(None);
    sweep_machine(&ctx, &plane, &dir);

    // The fork ladder CLAIMS NOTHING here — it is a question about a folder's bytes, and there is
    // no folder.
    assert!(
        ops::keep_as_yours(&ctx, "deepwiki", false)
            .unwrap()
            .is_none(),
        "a server bundle is never a re-fork case"
    );

    let roots = crate::ops::DiscoveryRoots {
        home: rig.home.0.clone(),
        cwd: None,
    };
    let plan = ops::plan_bare_add(
        &ctx,
        &connect(&plane, &dir),
        &roots,
        "deepwiki",
        ops::BareAdd {
            subscribe: true,
            dest_selected: false,
            global: true,
            workspace: None,
        },
    )
    .unwrap();
    let ops::BareAddPlan::SetDelivered {
        name,
        reference,
        set,
    } = plan
    else {
        panic!("the feed line is what delivers it: {plan:?}");
    };
    assert_eq!(name, "deepwiki");
    assert_eq!(reference, format!("{HOST}/{WS_NAME}/deepwiki"));
    assert_eq!(set, format!("{HOST}/{WS_NAME}"));
    // The FINAL answer — the same sentence, word for word, that a skill on this feed gets.
    let data = ops::set_delivered_answer(&name, &reference, set, true);
    assert_eq!(
        crate::render::add_tty(&data),
        format!(
            "deepwiki already reaches every agent on this machine through {HOST}/{WS_NAME} — no \
             row to record.\nnothing changed"
        )
    );
    assert_eq!(data.manifest, None, "no file was edited");
    assert!(data.undo.is_empty(), "nothing was recorded to invert");
}

/// THE ADDRESS FORM of a bundle the feed already delivers writes nothing and says so — for a
/// connected server as for a skill. The redundancy check read only the delivery answer's FILE
/// bundles, so a server slipped past it and got a redundant `[mcp]` row beside the feed line that
/// was already delivering it. And the answer LEADS with what happened: an `added deepwiki`
/// headline over a note retracting it announces an act that did not occur.
#[test]
fn an_address_add_of_a_feed_delivered_server_writes_nothing_and_leads_with_that() {
    let rig = Rig::new("address-add-server");
    rig.seed_session();
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    let s = served_at("https://mcp.example/deepwiki");
    let plane = FakePlane::new();
    plane.serves_servers(vec![delivered_mcp("s_deep", "deepwiki", &s)]);
    let dir = FakeDirectory {
        servers: vec![mcp_catalog_entry("s_deep", "deepwiki", &s)],
        ..FakeDirectory::default()
    };
    rig.write_global(&format!(
        "schema = 1\n\n[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n"
    ));
    let ctx = rig.ctx_at(None);
    sweep_machine(&ctx, &plane, &dir);
    let manifest = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    let before = std::fs::read_to_string(&manifest).unwrap();

    let outcome = ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("{HOST}/{WS_NAME}/deepwiki"),
        true,
        false,
        &crate::ops::dest_select::Selection::new(&[], &[]),
        None,
    )
    .unwrap();
    let ops::AddRefOutcome::Applied { data, .. } = outcome else {
        panic!("a redundant add applies, it does not describe");
    };
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        before,
        "no row is written — the feed row already delivers it"
    );
    assert_eq!(data.manifest, None, "no file was edited");
    assert!(data.undo.is_empty(), "nothing was recorded to invert");
    assert!(data.unchanged, "the answer says nothing happened");
    assert_eq!(
        crate::render::add_tty(&data),
        format!(
            "{WS_NAME}\'s feed already delivers \'deepwiki\' here — a bare row would add \
             nothing, so nothing was written\nsource: {HOST}/{WS_NAME}/deepwiki"
        )
    );
}
