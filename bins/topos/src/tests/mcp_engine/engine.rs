//! The engine, over the synthetic six: the write boundary a plan/write window has to re-prove, the
//! entries plan's reach, and the converge itself — placement per dialect, drift and foreign
//! custody, the per-scope lock, and the intent journal's crash recovery.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use topos_harness::mcp::{self, AuthHint, McpDialect, McpEntry, plugin_dir};
use topos_harness::registry::{self, KnownHarness};
use topos_types::results::TargetOutcome;

use crate::config_custody::{self, ScopeEntries};
use crate::fs_seam::{FaultFs, FsOps as _, RealFs};
use crate::mcp_engine::{self, DemandedBundle, ScopeIo};
use crate::ops;
use crate::sidecar::Layout;

use super::rig::*;

/// **Z1 — the containment rail is re-run at the WRITE boundary.** A plan resolves a project
/// surface once and the converge writes it later; `replace_config` follows symlinks, so a path
/// component swapped for an outward symlink in that window would put managed bytes outside the
/// checkout. The proof therefore re-runs immediately before any byte moves: ZERO writes outside,
/// the `unprovable` state spoken with the same note the planner's withheld line carries, and the
/// escape disclosed.
#[test]
fn a_surface_symlinked_out_between_plan_and_write_is_refused_with_zero_writes() {
    let fs = RealFs;
    let project = Scratch::new("wb-project");
    let outside = Scratch::new("wb-outside");
    std::fs::create_dir_all(project.0.join(".cursor")).unwrap();
    let layout = Layout::new(&project.0.join(".topos"));
    let io = ScopeIo {
        fs: &fs,
        layout: &layout,
        home: project.0.clone(),
        project_root: Some(project.0.clone()),
    };
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.reach = Some(vec!["cursor".into()]);
    // The plan proves containment HERE, while `.cursor` is an ordinary dir inside the checkout.
    let demands = plan(&io, vec![d]);
    assert!(
        demands[0].plan.entries_for("cursor").is_some(),
        "the surface planned clean: {:?}",
        demands[0].plan
    );

    // ...and the checkout is rewritten under it before the write lands.
    std::fs::remove_dir_all(project.0.join(".cursor")).unwrap();
    std::os::unix::fs::symlink(&outside.0, project.0.join(".cursor")).unwrap();

    let out = mcp_engine::converge(&io, &demands, &synthetic(), &all_slugs(), &no_hold(), true);

    assert!(
        !outside.0.join("mcp.json").exists(),
        "no managed byte may land outside the checkout"
    );
    let st = state_of(&out, "s_a", "cursor");
    assert_eq!(st.state, TargetOutcome::Unprovable, "{st:?}");
    assert_eq!(
        st.note.as_deref(),
        Some("the config path does not resolve inside this checkout")
    );
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.starts_with("PLACEMENT_ESCAPES_PROJECT")),
        "{:?}",
        out.warnings
    );
}

/// **Z2 — a targeted converge whose whole reach is WITHHELD still speaks, and still recovers.**
/// The reach resolves to one harness whose surface no longer proves inside the checkout, so there
/// is nothing to write anywhere. Returning early there would drop the per-agent line the receipt
/// owes AND skip the intent journal's crash recovery, which runs inside the converge — so a crash
/// left by an earlier run would survive every targeted verb.
#[test]
fn a_targeted_converge_with_only_withheld_surfaces_reports_and_still_recovers() {
    let rig = Rig::new("withheld-targeted");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let proj = Scratch::new("withheld-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::create_dir_all(proj.0.join(".cursor")).unwrap();
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        format!("[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ dest = [\"./.cursor/mcp.json\"] }}\n"),
    )
    .unwrap();
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_linear", &v);
    plane.serves(vec![delivered_mcp("s_linear", "linear", &v)]);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_linear", "linear", &v)],
        channels: Vec::new(),
    };
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep(&ctx, &plane, &dir);
    assert!(
        proj.0.join(".cursor/mcp.json").exists(),
        "the sweep placed the one narrowed surface"
    );

    // The checkout is rewritten: the ONLY harness the record reaches no longer proves inside it.
    let outside = Scratch::new("withheld-outside");
    std::fs::remove_dir_all(proj.0.join(".cursor")).unwrap();
    std::os::unix::fs::symlink(&outside.0, proj.0.join(".cursor")).unwrap();

    // A crash-left intent standing in the scope journal — only a run that ENTERS the converge
    // resolves it.
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0).unwrap();
    let mut custody = crate::config_custody::ScopeEntries::load(&rig.fs, &playout).unwrap();
    let mut intents = std::collections::BTreeMap::new();
    intents.insert(
        crate::config_custody::placement_key("codex", "topos-eng-ghost"),
        crate::config_custody::PendingIntent {
            bundle_id: "s_ghost".to_owned(),
            version_id: "v1".to_owned(),
            file: proj.0.join(".codex/config.toml").display().to_string(),
            fingerprint: "deadbeef".to_owned(),
            owns_file: false,
        },
    );
    custody.journal(&rig.fs, &playout, intents).unwrap();
    assert!(
        crate::config_custody::ScopeEntries::load(&rig.fs, &playout)
            .unwrap()
            .has_pending(),
        "the crash-left intent stands before the verb"
    );

    let out = ops::pull(
        &ctx,
        ops::PullScope::One {
            name: "linear".into(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v.id)),
            store: ops::StoreScope::Here,
        },
    )
    .expect("the go-back applies");

    // It SPOKE: the per-agent line survives a run that wrote nothing.
    let row = &out.data.skills[0];
    let st = row
        .harnesses
        .iter()
        .find(|h| h.agent == "cursor")
        .unwrap_or_else(|| panic!("cursor state: {row:?}"));
    assert_eq!(st.state, TargetOutcome::Unprovable, "{st:?}");
    assert!(
        !outside.0.join("mcp.json").exists(),
        "nothing lands outside the checkout"
    );
    // And it RECOVERED: the journal the crash left is resolved by observation, not carried.
    assert!(
        !crate::config_custody::ScopeEntries::load(&rig.fs, &playout)
            .unwrap()
            .has_pending(),
        "the converge ran its crash recovery"
    );
}

/// **Z5 — a `dest` move is a wholly successful sweep, and it says so.** Moving a bundle's config
/// destination from one agent to another removes its entry from the old surface and deletes the
/// file topos wholly owned there. Both are work that WORKED: routed through the warning channel
/// the deletion made a clean run count itself FAILED and exit non-zero while `--json` still said
/// `ok: true`. It rides disclosures now — and the row reporting the surfaces the bundle left names
/// it the way a person knows it. For a LOCALLY ADOPTED bundle no delivery cache describes, that
/// name comes from the bundle's own record; the opaque store id is the last resort, not the first
/// answer.
#[test]
fn a_dest_move_is_a_clean_sweep_that_names_the_bundle_it_moved() {
    let rig = Rig::new("dest-move");
    seed_harness_dirs(&rig.home.0);
    rig.write_global("[bundles]\n");
    let dir = rig.home.0.join("demo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.json"),
        "{\"name\":\"io.test/demo\",\"description\":\"D.\",\"version\":\"1.0.0\",\
         \"remotes\":[{\"type\":\"streamable-http\",\"url\":\"https://demo.example/mcp\"}]}",
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    // Adopted in place: the store tracks the folder under an opaque id, and no delivery cache
    // will ever describe it — the shape whose receipt used to print that id at a person.
    ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default())
        .expect("the local mcp folder adopts");
    let plane = FakePlane::new();
    let fdir = FakeDirectory {
        skills: Vec::new(),
        channels: Vec::new(),
    };
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = {{ kind = \"mcp\", dest = [\"~/.cursor/mcp.json\"] }}\n",
        dir.display()
    ));
    sweep(&ctx, &plane, &fdir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(cursor.exists(), "the first sweep placed cursor");

    // The row's destination MOVES. The old surface loses the entry, and the file topos created
    // there goes with it.
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = {{ kind = \"mcp\", dest = [\"~/.openclaw/openclaw.json\"] }}\n",
        dir.display()
    ));
    let out = sweep(&ctx, &plane, &fdir);

    assert!(
        rig.home.0.join(".openclaw/openclaw.json").exists(),
        "the new destination gained the entry"
    );
    assert!(!cursor.exists(), "the wholly-owned old file was deleted");
    // NOTHING failed — the whole point: a successful move exits 0.
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    // The deletion is a DISCLOSURE, and it still names the file it deleted. The MACHINE lane keeps
    // the code (an agent branches on it); the TTY prints the sentence, because a person reading a
    // receipt should not have to skip a machine word to reach English.
    assert!(
        crate::message::legacy_lines(&out.disclosures)
            .into_iter()
            .any(|d| d.starts_with("MCP_FILE_REMOVED") && d.contains("mcp.json")),
        "{:?}",
        out.disclosures
    );
    let note_tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
    );
    assert!(
        note_tty.contains(
            "this file held only entries topos placed, so topos deleted it with the \
                           last one."
        ) && !note_tty.contains("MCP_FILE_REMOVED"),
        "the note reads as English on the TTY: {note_tty}"
    );
    // The row for the surfaces the bundle left names `demo` — never the store's id for it.
    let left = out
        .data
        .skills
        .iter()
        .find(|r| r.action == topos_types::results::PullAction::Removed)
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert_eq!(left.skill, "demo", "{left:?}");

    // BOTH ROWS STAND — the move and the surface it vacated are each true, and each worth seeing.
    let for_demo: Vec<_> = out
        .data
        .skills
        .iter()
        .filter(|r| r.skill == "demo")
        .collect();
    assert_eq!(for_demo.len(), 2, "{for_demo:?}");

    // …AND THE SUMMARY COUNTS ONE BUNDLE. The tally used to count rows, so one bundle moving
    // destinations reported "Checked 2 bundles" over a machine holding one — the receipt above it
    // naming that single bundle twice. Distinct bundles are counted once now, under the primary
    // outcome: the bundle was UPDATED, and the vacated surface is a detail of the move.
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
    );
    assert!(
        tty.contains("Checked 1 bundle: 1 updated."),
        "one bundle, counted once, under what happened to it: {tty}"
    );
}

/// **The entries plan is what decides reach — and what says what reach COST.** Four facts, over
/// the one planner every demand is built through:
///
/// - no narrowing ⇒ every engaged harness with a surface at this scope gets a target;
/// - a narrowing admits exactly what it names, and stays SILENT about the rest — a row that never
///   asked for a harness is owed no disclosure about it;
/// - a harness with no surface AT THIS SCOPE is WITHHELD, carrying the phrase the receipt prints
///   (this is what keeps the per-agent `not-supported` lines alive now that the converge no longer
///   derives them);
/// - a harness that is neither detected nor already configured earns neither a target nor a line:
///   there is no agent here to reach, and nothing was withheld from anyone.
#[test]
fn the_entries_plan_carries_reach_and_names_what_it_withheld() {
    let home = Scratch::new("entries-plan");
    let fs = RealFs;
    let plan_at =
        |detected: &BTreeSet<String>, project: Option<&Path>, reach: Option<&[String]>| {
            crate::placement::entries_plan_at(&fs, &synthetic(), &home.0, detected, project, reach)
        };
    let slugs = |p: &crate::placement::PlacementPlan| -> Vec<String> {
        p.entries().map(|e| e.agent.clone()).collect()
    };
    let withheld = |p: &crate::placement::PlacementPlan| -> Vec<(String, TargetOutcome)> {
        p.withheld
            .iter()
            .map(|w| (w.agent.clone(), w.state))
            .collect()
    };

    // Every engaged harness, person scope: six targets, nothing withheld.
    let all = plan_at(&all_slugs(), None, None);
    assert_eq!(slugs(&all).len(), synthetic().len(), "{:?}", slugs(&all));
    assert!(withheld(&all).is_empty(), "{:?}", withheld(&all));
    // The target names the DRIVER file — the plugin dialect's own `.mcp.json`, not its dir.
    let plugin = all.entries_for("claude-code").expect("claude-code planned");
    assert!(
        plugin.file.ends_with(".mcp.json"),
        "{}",
        plugin.file.display()
    );

    // A narrowing admits exactly what it names — and says nothing about the rest.
    let narrowed = plan_at(&all_slugs(), None, Some(&["cursor".to_owned()]));
    assert_eq!(slugs(&narrowed), vec!["cursor".to_owned()]);
    assert!(withheld(&narrowed).is_empty(), "{:?}", withheld(&narrowed));

    // PROJECT scope: the two harnesses with no project surface are withheld, by name and phrase.
    let project = Scratch::new("entries-plan-co");
    let proj = plan_at(&all_slugs(), Some(&project.0), None);
    for slug in ["openclaw", "hermes-agent"] {
        assert!(
            proj.entries_for(slug).is_none(),
            "{slug} must not be planned"
        );
        let w = proj.withheld_for(slug).unwrap_or_else(|| panic!("{slug}"));
        assert_eq!(
            (w.state, w.note.as_str()),
            (TargetOutcome::Withheld, "no project-level config")
        );
    }
    assert!(proj.entries_for("cursor").is_some());

    // Nothing detected and no config on disk: no target, and nothing withheld from anyone.
    let cold = Scratch::new("entries-plan-cold");
    let cold_plan =
        crate::placement::entries_plan_at(&fs, &synthetic(), &cold.0, &BTreeSet::new(), None, None);
    assert!(slugs(&cold_plan).is_empty(), "{:?}", slugs(&cold_plan));
    assert!(
        withheld(&cold_plan).is_empty(),
        "{:?}",
        withheld(&cold_plan)
    );
}

// =================================================================================================
// The engine, over the synthetic six.
// =================================================================================================

#[test]
fn converge_places_into_all_six_dialects_byte_identical_to_the_drivers() {
    let home = Scratch::new("six");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let d = demand(
        "s_linear",
        "linear",
        Some("eng"),
        &server_json("https://mcp.example/linear"),
    );
    let io = person_io(&fs, &layout, &home.0);
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    // Every file dialect's bytes are EXACTLY what the pure driver renders from scratch — the
    // engine adds custody, never a dialect of its own.
    let entry = McpEntry {
        key: "topos-eng-linear".into(),
        url: "https://mcp.example/linear".into(),
        headers: vec![("X-Team".into(), "eng".into())],
        auth: AuthHint::Unknown,
    };
    for (suffix, dialect) in [
        (".codex/config.toml", McpDialect::CodexToml),
        (".cursor/mcp.json", McpDialect::CursorJson),
        (".opencode/opencode.json", McpDialect::OpencodeJson),
        (".openclaw/openclaw.json", McpDialect::OpenclawJson),
        (".hermes/config.yaml", McpDialect::HermesYaml),
    ] {
        let got = std::fs::read(home.0.join(suffix)).unwrap_or_else(|e| panic!("{suffix}: {e}"));
        let expect = match mcp::apply(
            dialect,
            None,
            std::slice::from_ref(&entry),
            &BTreeMap::new(),
        )
        .plan
        {
            mcp::EditPlan::Write(bytes) => bytes,
            other => panic!("{dialect:?}: {other:?}"),
        };
        assert_eq!(got, expect, "{suffix} differs from the driver's rendering");
    }
    // The plugin dir: both files, the .mcp.json byte-identical to the pure renderer.
    let plug = home.0.join(".claude/skills/topos-mcp");
    let rendered = plugin_dir::render_plugin_dir(std::slice::from_ref(&entry));
    assert_eq!(
        std::fs::read(plug.join(".claude-plugin/plugin.json")).unwrap(),
        rendered[0].1
    );
    assert_eq!(
        std::fs::read(plug.join(".mcp.json")).unwrap(),
        rendered[1].1
    );

    // Six states, all `placed` — this converge wrote every one of them — each carrying its
    // reload note (a fresh placement).
    for h in synthetic() {
        let st = state_of(&out, "s_linear", h.slug);
        assert!(st.state.wrote(), "{}", h.slug);
        assert_eq!(
            st.note.as_deref(),
            h.mcp().map(|m| m.reload_note),
            "{}",
            h.slug
        );
    }
    // The custody: one key, six entries, every fingerprint matching what the file provably holds.
    let custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert_eq!(custody.key_of("s_linear").unwrap(), "topos-eng-linear");
    assert_eq!(custody.row_count(), 6);
    assert!(custody.doc.pending.is_empty());
    for (k, _b, e) in custody.iter() {
        let slug = k.split('/').next().unwrap();
        let h = synthetic().into_iter().find(|h| h.slug == slug).unwrap();
        let dialect = h.mcp().unwrap().user.unwrap().dialect;
        let observed = mcp::observe(dialect, std::fs::read(&e.file).ok().as_deref());
        assert_eq!(
            observed.entries.get("topos-eng-linear"),
            Some(&e.fingerprint),
            "{k}: custody fingerprint must match the file"
        );
        assert!(e.owns_file, "{k}: a file we created is wholly ours");
    }

    // Idempotent: a second converge leaves every byte and reports Current (no reload note).
    let before: Vec<Vec<u8>> = synthetic()
        .iter()
        .map(|h| std::fs::read(h.mcp_user_path(&home.0).unwrap()).ok())
        .map(Option::unwrap_or_default)
        .collect();
    let out2 = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    for h in synthetic() {
        let st = state_of(&out2, "s_linear", h.slug);
        assert_eq!(
            (st.state, st.note.as_deref()),
            (TargetOutcome::Current, None),
            "{}",
            h.slug
        );
    }
    let after: Vec<Vec<u8>> = synthetic()
        .iter()
        .map(|h| std::fs::read(h.mcp_user_path(&home.0).unwrap()).ok())
        .map(Option::unwrap_or_default)
        .collect();
    assert_eq!(before, after, "the second converge moved bytes");
}

/// **One file, two spellings, one custody.** A machine can reach the same config file through a
/// symlinked home (`$CLAUDE_CONFIG_DIR` aimed at a link, a `/tmp` that is really `/private/tmp`),
/// and a lexical path compare then reads topos's own entry as somebody else's: no prior, so the
/// drivers call it foreign and leave it, the row is reported as a stale path, and every later
/// update refuses to touch a file topos wrote. Both sides resolve through the seam now, so the
/// second spelling finds the entry it placed under the first — CURRENT, no warning, no drift.
#[test]
fn a_surface_reached_through_a_symlinked_home_is_still_topos_own_entry() {
    let real = Scratch::new("canon-home");
    let links = Scratch::new("canon-links");
    let linked_home = links.0.join("home");
    std::os::unix::fs::symlink(&real.0, &linked_home).unwrap();
    let fs = RealFs;
    // ONE store, whichever spelling the surfaces are resolved through.
    let layout = Layout::new(&real.0.join(".topos"));
    let d = demand(
        "s_linear",
        "linear",
        Some("eng"),
        &server_json("https://mcp.example/linear"),
    );

    // Placed through the LINK.
    let io = person_io(&fs, &layout, &linked_home);
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(state_of(&out, "s_linear", "cursor").state.wrote());

    // The row records the RESOLVED spelling — one file, one name for it.
    let custody = ScopeEntries::load(&fs, &layout).unwrap();
    let row = custody
        .row(&config_custody::placement_key("cursor", "topos-eng-linear"))
        .expect("cursor row");
    assert_eq!(Path::new(&row.file), real.0.join(".cursor/mcp.json"));

    // Reached through the RESOLVED path: the same entry, already in order.
    let io2 = person_io(&fs, &layout, &real.0);
    let out2 = mcp_engine::converge(
        &io2,
        &plan(&io2, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(
        out2.warnings.is_empty(),
        "no stale-path disclosure for one file: {:?}",
        out2.warnings
    );
    for h in synthetic() {
        assert_eq!(
            state_of(&out2, "s_linear", h.slug).state,
            TargetOutcome::Current,
            "{}",
            h.slug
        );
    }

    // …and back through the LINK once more: still current, still one row per surface.
    let out3 = mcp_engine::converge(
        &io,
        &plan(&io, vec![d]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(out3.warnings.is_empty(), "{:?}", out3.warnings);
    assert_eq!(
        state_of(&out3, "s_linear", "cursor").state,
        TargetOutcome::Current
    );
    assert_eq!(
        ScopeEntries::load(&fs, &layout).unwrap().row_count(),
        synthetic().len(),
        "one row per surface, never one per spelling"
    );
}

#[test]
fn removal_converges_everywhere_and_deletes_only_wholly_owned_files() {
    let home = Scratch::new("removal");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    // Cursor's config PRE-EXISTS with a user server — never ours to delete. The rest are born
    // from this converge (wholly ours).
    let cursor = home.0.join(".cursor/mcp.json");
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    std::fs::write(
        &cursor,
        b"{\n  \"mcpServers\": {\n    \"mine\": { \"url\": \"https://user.example\" }\n  }\n}\n",
    )
    .unwrap();
    let d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    let io = person_io(&fs, &layout, &home.0);
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    // Drop the demand: every entry leaves; the wholly-owned files are DELETED, the user's cursor
    // file keeps its own server minus ours; the plugin dir is pruned whole.
    let out = mcp_engine::converge(
        &person_io(&fs, &layout, &home.0),
        &[],
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(out.removed.len(), 6, "{out:?}");
    assert!(
        !home.0.join(".codex/config.toml").exists(),
        "wholly-owned codex file deleted"
    );
    assert!(!home.0.join(".opencode/opencode.json").exists());
    assert!(!home.0.join(".openclaw/openclaw.json").exists());
    assert!(!home.0.join(".hermes/config.yaml").exists());
    assert!(
        !home.0.join(".claude/skills/topos-mcp").exists(),
        "plugin dir pruned"
    );
    let kept = std::fs::read_to_string(&cursor).unwrap();
    assert!(
        kept.contains("\"mine\""),
        "the user's server survives: {kept}"
    );
    assert!(!kept.contains("topos-eng-alpha"), "ours is gone: {kept}");
    // The custody: no entries, and the name RESERVED — a key names an OAuth trust surface, and a
    // harness keeps its sign-in in a keychain no config file can be read to rule out. The name
    // goes back only to a mint that proves it is for the same server, which is what the next
    // assertion is: nothing standing anywhere, and the same address the retired key pointed at.
    let mut custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert_eq!(custody.row_count(), 0);
    assert_eq!(
        custody
            .doc
            .retired
            .get("topos-eng-alpha")
            .map(String::as_str),
        Some("s_a"),
        "the reservation stands until a mint proves it may have it: {:?}",
        custody.doc.retired
    );
    let nothing_standing = std::collections::BTreeSet::new();
    assert_eq!(
        custody.mint_key(
            "s_other",
            "alpha",
            Some("eng"),
            &config_custody::KeyMint {
                address: mcp::canonical_address("https://mcp.example/a").as_deref(),
                standing: Some(&nothing_standing),
            }
        ),
        "topos-eng-alpha"
    );
}

/// **A retired name goes back only to the SAME SERVER, and only with nothing left under it.**
///
/// A key names an OAuth trust surface: a harness files its sign-in under the server NAME, in a
/// keychain that outlives every config entry. So "no entry stands under the key anywhere" is not
/// enough to hand the name on — removing a config entry does not remove the grant behind it, and a
/// different bundle minted onto that name could arrive already trusted. What does settle it is
/// WHICH SERVER the new entry points at: the same address is the same server, and inheriting a
/// sign-in for the server you are about to talk to is what a sign-in is for.
///
/// Five branches, over the real converge: the relocation that gets its plain name back, the
/// different server that does not, a survivor in a surface topos writes, a survivor in a file topos
/// only READS (the harness reads servers from it too, so a name standing there is exactly as
/// inheritable), and that same file unreadable — where absence is unprovable and nothing goes back.
#[test]
fn a_retired_name_goes_back_only_to_the_same_server_with_nothing_standing_under_it() {
    static WITH_CONFLICTS: &[KnownHarness] = &[
        registry::home_rooted_mcp_row_with_conflicts(
            "claude-code",
            "Claude Code",
            ".claude/skills/topos-mcp",
            McpDialect::ClaudePluginDir,
            None,
            "reload claude",
            &[registry::home_rooted_conflict_path(
                ".claude.json",
                McpDialect::ClaudeProjectJson,
                "projects.*.mcpServers",
            )],
        ),
        registry::home_rooted_mcp_row(
            "cursor",
            "Cursor",
            ".cursor/mcp.json",
            McpDialect::CursorJson,
            None,
            "restart cursor",
        ),
    ];
    let table: Vec<&'static KnownHarness> = WITH_CONFLICTS.iter().collect();
    let slugs: BTreeSet<String> = table.iter().map(|h| h.slug.to_owned()).collect();
    const A: &str = "https://mcp.example/a";
    const B: &str = "https://mcp.example/b";

    // One scope with `alpha` placed at A and then dropped: its key retires, and the reservation
    // stands whatever else is true. The caller then arranges what is left under the name.
    let retired_scope = |tag: &str| -> (Scratch, Layout) {
        let home = Scratch::new(tag);
        let layout = Layout::new(&home.0.join(".topos"));
        let fs = RealFs;
        let io = person_io(&fs, &layout, &home.0);
        let mut d = demand("s_a", "alpha", Some("eng"), &server_json(A));
        d.reach = Some(vec!["cursor".into()]);
        mcp_engine::converge(
            &io,
            &[d.planned(&io, &table, &slugs)],
            &table,
            &slugs,
            &no_hold(),
            true,
        );
        mcp_engine::converge(&io, &[], &table, &slugs, &no_hold(), true);
        let custody = ScopeEntries::load(&fs, &layout).unwrap();
        assert_eq!(
            custody
                .doc
                .retired
                .get("topos-eng-alpha")
                .map(String::as_str),
            Some("s_a"),
            "{tag}: the key retires reserved: {:?}",
            custody.doc.retired
        );
        (home, layout)
    };
    // A NEW bundle asks for that name, pointing at `address`. Answers the key it actually got.
    let ask = |home: &Scratch, layout: &Layout, address: &str| -> String {
        let fs = RealFs;
        let io = person_io(&fs, layout, &home.0);
        let mut d = demand("s_b", "alpha", Some("eng"), &server_json(address));
        d.reach = Some(vec!["cursor".into()]);
        mcp_engine::converge(
            &io,
            &[d.planned(&io, &table, &slugs)],
            &table,
            &slugs,
            &no_hold(),
            true,
        );
        let custody = ScopeEntries::load(&fs, layout).unwrap();
        custody.key_of("s_b").expect("s_b holds a key").to_owned()
    };

    // 1. THE RELOCATION: the same server arriving as a new bundle, nothing left under the name.
    let (home, layout) = retired_scope("retire-same");
    assert_eq!(ask(&home, &layout, A), "topos-eng-alpha");
    assert!(
        ScopeEntries::load(&RealFs, &layout)
            .unwrap()
            .doc
            .retired
            .is_empty(),
        "the reservation went back with the name"
    );

    // 2. A DIFFERENT SERVER at the same natural name: the reservation stands and the mint suffixes.
    let (home, layout) = retired_scope("retire-other");
    assert_eq!(ask(&home, &layout, B), "topos-eng-alpha-2");
    assert!(
        ScopeEntries::load(&RealFs, &layout)
            .unwrap()
            .doc
            .retired
            .contains_key("topos-eng-alpha"),
        "another server may not have the name a sign-in may still be filed under"
    );

    // 3. AN ENTRY STANDING IN A SURFACE TOPOS WRITES, under the retired name and with no row
    //    behind it — a hand-copied entry, or one an older topos left. Same server or not, the name
    //    stays reserved while something is there to inherit.
    let (home, layout) = retired_scope("retire-survivor");
    let cursor = home.0.join(".cursor/mcp.json");
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    let leftover = "{\n  \"mcpServers\": { \"topos-eng-alpha\": { \"url\": \"https://mcp.example/old\" } }\n}\n";
    std::fs::write(&cursor, leftover).unwrap();
    assert_eq!(ask(&home, &layout, A), "topos-eng-alpha-2");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("https://mcp.example/old"),
        "topos never edited the entry it does not own"
    );

    // 4. AN ENTRY STANDING IN A FILE TOPOS ONLY READS. Claude Code reads servers from
    //    `~/.claude.json` as well as from the dir topos owns, so a name standing there is exactly
    //    as inheritable as one in a surface topos writes — and counting only the writable surfaces
    //    handed the name back over an entry that was still there.
    let (home, layout) = retired_scope("retire-conflict-path");
    let claude_json = home.0.join(".claude.json");
    let survivor = "{\n  \"projects\": {\n    \"/work/api\": { \"mcpServers\": { \"topos-eng-alpha\": { \"url\": \"https://mcp.example/a\" } } }\n  }\n}\n";
    std::fs::write(&claude_json, survivor).unwrap();
    assert_eq!(ask(&home, &layout, A), "topos-eng-alpha-2");
    assert_eq!(
        std::fs::read_to_string(&claude_json).unwrap(),
        survivor,
        "topos never edited the entry it does not own"
    );

    // 5. THAT SAME FILE UNREADABLE. Absence is then unprovable, and a reservation dropped on a file
    //    nobody could read is a name handed over an entry that may still be standing under it.
    let (home, layout) = retired_scope("retire-unparseable");
    std::fs::write(home.0.join(".claude.json"), "{").unwrap();
    assert_eq!(ask(&home, &layout, A), "topos-eng-alpha-2");
}

#[test]
fn a_hand_edited_entry_is_drift_never_clobbered_and_survives_removal_disclosed() {
    let home = Scratch::new("drift");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    let cursor_only = |d: &DemandedBundle| {
        let mut d = d.clone();
        d.reach = Some(vec!["cursor".into()]);
        d
    };
    let io = person_io(&fs, &layout, &home.0);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![cursor_only(&d)]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    // The user edits OUR entry by hand.
    let cursor = home.0.join(".cursor/mcp.json");
    let edited = std::fs::read_to_string(&cursor)
        .unwrap()
        .replace("https://mcp.example/a", "https://my-fork.example");
    std::fs::write(&cursor, &edited).unwrap();

    // The next converge reads it as DRIFT: untouched, reported, the custody keeps the OLD
    // fingerprint so drift survives re-runs.
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![cursor_only(&d)]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        state_of(&out, "s_a", "cursor").state,
        TargetOutcome::Drifted
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        edited,
        "bytes untouched"
    );

    // Removal LEAVES the drifted entry and disclosed it (never destroys a hand edit).
    let out = mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), true);
    assert!(
        out.removed
            .iter()
            .any(|r| r.state.state == TargetOutcome::Drifted),
        "{out:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        edited,
        "removal left the edit"
    );
}

#[test]
fn a_foreign_topos_prefixed_entry_is_never_touched_or_claimed() {
    let home = Scratch::new("foreign");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    // Someone else's `topos-…` entry occupies the very key we would mint.
    let cursor = home.0.join(".cursor/mcp.json");
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    let foreign = "{\n  \"mcpServers\": {\n    \"topos-eng-alpha\": { \"url\": \"https://foreign.example\" }\n  }\n}\n";
    std::fs::write(&cursor, foreign).unwrap();
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.reach = Some(vec!["cursor".into()]);
    let io = person_io(&fs, &layout, &home.0);
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        state_of(&out, "s_a", "cursor").state,
        TargetOutcome::Conflicting
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        foreign,
        "foreign bytes untouched"
    );
    let custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert!(
        !custody.holds("cursor/topos-eng-alpha"),
        "a foreign entry never enters the custody"
    );
}

// =================================================================================================
// The collision pre-flight: what already stands where a placement would go.
// =================================================================================================

/// The one line a collision earns, as a person reads it.
fn warning_lines(out: &mcp_engine::ConvergeOutcome) -> Vec<String> {
    crate::message::legacy_lines(&out.warnings)
}

/// **A server the agent already has, under a name topos would never recognize, is not placed
/// over.** Claude Code (and others) de-duplicate by URL, so a second entry for one server is at
/// best invisible and at worst the one that loses — topos reporting `current` over it would be
/// lying about what the agent runs. The foreign entry is never touched; the refusal names it, the
/// file, and what removing it buys; and the placement lands by itself on the next sweep once it
/// is gone.
#[test]
fn a_foreign_entry_for_the_same_server_refuses_the_placement_and_says_how_to_free_it() {
    let home = Scratch::new("collide-url");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let cursor = home.0.join(".cursor/mcp.json");
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    // THEIR entry: another name, the same server (spelled differently — one address, one server).
    let theirs = "{\n  \"mcpServers\": {\n    \"linear\": { \"url\": \"HTTPS://MCP.Example:443/a/\" }\n  }\n}\n";
    std::fs::write(&cursor, theirs).unwrap();
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a/"),
    );
    d.reach = Some(vec!["cursor".into()]);
    let io = person_io(&fs, &layout, &home.0);
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );

    let state = state_of(&out, "s_a", "cursor");
    assert_eq!(state.state, TargetOutcome::Conflicting);
    assert_eq!(
        state.file.as_deref(),
        Some(cursor.display().to_string().as_str()),
        "the state points at the file holding the entry in the way"
    );
    assert_eq!(
        warning_lines(&out),
        vec![format!(
            "MCP_ENTRY_CONFLICT not placed in cursor: an entry for this server already exists \
             (linear in {}) and topos does not manage it. Remove it to let topos manage this \
             server, then run 'topos update'.",
            cursor.display()
        )]
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        theirs,
        "topos never edits an entry it does not own — not even to move it aside"
    );
    let custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert_eq!(custody.row_count(), 0, "nothing was recorded as placed");

    // A SECOND sweep says exactly the same thing: the collision is re-decided from the file every
    // run, never remembered.
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        state_of(&out, "s_a", "cursor").state,
        TargetOutcome::Conflicting
    );
    assert_eq!(warning_lines(&out).len(), 1);

    // The person removes their entry. Nothing else changes, and the next sweep installs.
    std::fs::write(&cursor, "{\n  \"mcpServers\": {}\n}\n").unwrap();
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(
        state_of(&out, "s_a", "cursor").state.wrote(),
        "the collision cleared itself: {out:?}"
    );
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("topos-eng-alpha")
    );
}

/// A `topos-`-prefixed entry with no record behind it gets a DIFFERENT sentence: the prefix is a
/// spelling, not a provenance, so the line says what it might be and never claims it. It blocks
/// the placement exactly the same — and topos still deletes nothing.
#[test]
fn a_topos_looking_entry_with_no_record_is_named_a_possible_leftover_never_claimed() {
    let home = Scratch::new("collide-leftover");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let cursor = home.0.join(".cursor/mcp.json");
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    let leftover = "{\n  \"mcpServers\": {\n    \"topos-eng-alpha\": { \"url\": \"https://elsewhere.example\" }\n  }\n}\n";
    std::fs::write(&cursor, leftover).unwrap();
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.reach = Some(vec!["cursor".into()]);
    let io = person_io(&fs, &layout, &home.0);
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        state_of(&out, "s_a", "cursor").state,
        TargetOutcome::Conflicting
    );
    assert_eq!(
        warning_lines(&out),
        vec![format!(
            "MCP_ENTRY_LEFTOVER possible leftover from an earlier topos version: topos-eng-alpha \
             in {} is no longer managed. Remove it with: delete the \"topos-eng-alpha\" entry from \
             that file.",
            cursor.display()
        )]
    );
    assert_eq!(std::fs::read_to_string(&cursor).unwrap(), leftover);
}

/// **A file the harness ALSO reads counts.** Claude Code keeps servers in `~/.claude.json` too —
/// per project, in a slot topos never writes — and prefers them over the plugin dir topos owns. An
/// entry there for the same server would make a placement that reports itself installed and never
/// runs, so it refuses instead. The other harnesses are untouched by it: a conflict path belongs
/// to the row that names it.
#[test]
fn an_entry_in_a_file_the_harness_also_reads_blocks_the_placement_there_and_nowhere_else() {
    static WITH_CONFLICTS: &[KnownHarness] = &[
        registry::home_rooted_mcp_row_with_conflicts(
            "claude-code",
            "Claude Code",
            ".claude/skills/topos-mcp",
            McpDialect::ClaudePluginDir,
            None,
            "reload claude",
            &[registry::home_rooted_conflict_path(
                ".claude.json",
                McpDialect::ClaudeProjectJson,
                "projects.*.mcpServers",
            )],
        ),
        registry::home_rooted_mcp_row(
            "cursor",
            "Cursor",
            ".cursor/mcp.json",
            McpDialect::CursorJson,
            None,
            "restart cursor",
        ),
    ];
    let table: Vec<&'static KnownHarness> = WITH_CONFLICTS.iter().collect();
    let slugs: BTreeSet<String> = table.iter().map(|h| h.slug.to_owned()).collect();

    let home = Scratch::new("collide-elsewhere");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let claude_json = home.0.join(".claude.json");
    let user_entry = "{\n  \"mcpServers\": { \"unrelated\": { \"url\": \"https://other.example\" } },\n  \"projects\": {\n    \"/work/api\": { \"mcpServers\": { \"alpha\": { \"url\": \"https://mcp.example/a\" } } }\n  }\n}\n";
    std::fs::write(&claude_json, user_entry).unwrap();

    let d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    let io = person_io(&fs, &layout, &home.0);
    let out = mcp_engine::converge(
        &io,
        &[d.clone().planned(&io, &table, &slugs)],
        &table,
        &slugs,
        &no_hold(),
        true,
    );
    assert_eq!(
        state_of(&out, "s_a", "claude-code").state,
        TargetOutcome::Conflicting,
        "{out:?}"
    );
    assert_eq!(
        warning_lines(&out),
        vec![format!(
            "MCP_ENTRY_CONFLICT not placed in claude-code: an entry for this server already exists \
             (alpha in {}) and topos does not manage it. Remove it to let topos manage this \
             server, then run 'topos update'.",
            claude_json.display()
        )]
    );
    assert!(
        !home.0.join(".claude/skills/topos-mcp").exists(),
        "nothing was written for the blocked harness"
    );
    assert_eq!(
        std::fs::read_to_string(&claude_json).unwrap(),
        user_entry,
        "the file topos only READS is never edited"
    );
    // Cursor names no such file, so its placement is untouched by somebody else's claude config.
    assert!(state_of(&out, "s_a", "cursor").state.wrote());
    assert!(
        std::fs::read_to_string(home.0.join(".cursor/mcp.json"))
            .unwrap()
            .contains("topos-eng-alpha")
    );
}

/// **Two topos bundles pointing at one server are not a collision.** Both entries are topos's, both
/// are recorded, and the ownership record is what says so — the pre-flight only ever asks about
/// entries nobody can prove are ours. And a foreign duplicate appearing LATER never uninstalls a
/// placement that already stands: blocking a key topos holds here would take its own entry out
/// through the drivers' removal.
#[test]
fn two_topos_bundles_on_one_server_both_place_and_a_later_duplicate_never_unplaces_them() {
    let home = Scratch::new("collide-siblings");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let url = "https://mcp.example/shared";
    let one = {
        let mut d = demand("s_a", "alpha", Some("eng"), &server_json(url));
        d.reach = Some(vec!["cursor".into()]);
        d
    };
    let two = {
        let mut d = demand("s_b", "beta", Some("eng"), &server_json(url));
        d.reach = Some(vec!["cursor".into()]);
        d
    };
    let io = person_io(&fs, &layout, &home.0);
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![one.clone(), two.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(state_of(&out, "s_a", "cursor").state.wrote());
    assert!(state_of(&out, "s_b", "cursor").state.wrote());
    let cursor = home.0.join(".cursor/mcp.json");
    let both = std::fs::read_to_string(&cursor).unwrap();
    assert!(
        both.contains("topos-eng-alpha") && both.contains("topos-eng-beta"),
        "{both}"
    );

    // Somebody adds their own entry for the same server afterwards. Both placements STAY — the
    // one thing a collision must never do is uninstall what it found standing.
    let with_theirs = both.replace(
        "\"mcpServers\": {",
        &format!("\"mcpServers\": {{\n    \"mine\": {{ \"url\": \"{url}\" }},"),
    );
    std::fs::write(&cursor, &with_theirs).unwrap();
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![one, two]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    for bundle in ["s_a", "s_b"] {
        assert_eq!(
            state_of(&out, bundle, "cursor").state,
            TargetOutcome::Current,
            "{bundle}: {out:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        with_theirs,
        "nothing moved: not theirs, not ours"
    );
}

#[test]
fn a_suspect_header_fails_the_demand_closed_with_a_warning() {
    let home = Scratch::new("gate");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand("s_a", "alpha", Some("eng"), "");
    d.server_json = br#"{"name":"io.test/a","description":"A.","version":"1.0.0","remotes":[{"type":"streamable-http","url":"https://a.example","headers":[{"name":"Authorization","isSecret":true}]}]}"#.to_vec();
    d.reach = Some(vec!["cursor".into()]);
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let line = crate::message::legacy_lines(&out.warnings)
        .into_iter()
        .find(|w| w.contains("MCP_SECRET_REFUSED"))
        .unwrap_or_else(|| panic!("the typed refusal is named: {:?}", out.warnings));
    // ONE code per line. The gate's code used to be printed INSIDE this line's own
    // (`MCP_UNPLACEABLE alpha: MCP_SECRET_REFUSED: …`), so a reader met two machine words
    // before the first English one and a parser found two codes on one line.
    assert!(
        line.starts_with("MCP_SECRET_REFUSED alpha: "),
        "the code leads, once: {line}"
    );
    assert!(!line.contains("MCP_UNPLACEABLE"), "{line}");
    assert_eq!(
        line.split_whitespace()
            .filter(
                |t| t.starts_with("MCP_") && t.chars().all(|c| c.is_ascii_uppercase() || c == '_')
            )
            .count(),
        1,
        "exactly one machine code on the line: {line}"
    );
    assert_eq!(
        state_of(&out, "s_a", "cursor").state,
        TargetOutcome::Unprovable
    );
    assert!(
        !home.0.join(".cursor/mcp.json").exists(),
        "nothing was placed"
    );
}

/// A user's sibling top-level key in the plugin dir's `.mcp.json` is content topos did not
/// write: the surface backs off whole — unprovable, byte-identical, disclosed — and the key is
/// NEVER destroyed, neither by an update rewrite nor by removal deleting the file.
#[test]
fn a_sibling_key_in_the_plugin_mcp_json_backs_the_surface_off_and_survives() {
    let home = Scratch::new("plugin-sibling");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let claude_only = |d: &DemandedBundle| {
        let mut d = d.clone();
        d.reach = Some(vec!["claude-code".into()]);
        d
    };
    let v1 = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/v1"),
    );
    mcp_engine::converge(
        &io,
        &plan(&io, vec![claude_only(&v1)]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    // The user adds a sibling top-level key beside mcpServers.
    let mcp_path = home.0.join(".claude/skills/topos-mcp/.mcp.json");
    let mut root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&mcp_path).unwrap()).unwrap();
    root.as_object_mut()
        .unwrap()
        .insert("theme".to_owned(), serde_json::json!("dark"));
    let edited = serde_json::to_string_pretty(&root).unwrap() + "\n";
    std::fs::write(&mcp_path, &edited).unwrap();

    // An update (the url moved) must not rewrite the file over the sibling key.
    let v2 = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/v2"),
    );
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![claude_only(&v2)]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let now = std::fs::read_to_string(&mcp_path).unwrap();
    assert!(
        now.contains("\"theme\""),
        "the user's sibling key survives an update: {now}"
    );
    assert_eq!(now, edited, "the surface backs off byte-identical");
    assert_eq!(
        state_of(&out, "s_a", "claude-code").state,
        TargetOutcome::Unprovable
    );

    // A removal (the demand drops) must not delete the file over the sibling key either.
    mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), true);
    let kept = std::fs::read_to_string(&mcp_path)
        .unwrap_or_else(|e| panic!("the plugin .mcp.json was deleted over a user key: {e}"));
    assert!(kept.contains("\"theme\""), "{kept}");
}

/// The constant `.claude-plugin/plugin.json` rides the foreign-occupant rule too: a hand-edited
/// manifest is never rewritten by the next entry change and never deleted by the last entry's
/// removal — kept byte-identical, the dir left standing over it, both disclosed. (A pristine
/// manifest still heals and prunes exactly as before — see the six-dialect and removal tests.)
#[test]
fn a_hand_edited_plugin_manifest_survives_update_and_removal_disclosed() {
    let home = Scratch::new("plugin-manifest");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let claude_only = |d: &DemandedBundle| {
        let mut d = d.clone();
        d.reach = Some(vec!["claude-code".into()]);
        d
    };
    let v1 = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/v1"),
    );
    mcp_engine::converge(
        &io,
        &plan(&io, vec![claude_only(&v1)]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    // The user edits the manifest by hand.
    let manifest = home
        .0
        .join(".claude/skills/topos-mcp/.claude-plugin/plugin.json");
    let edited = std::fs::read_to_string(&manifest)
        .unwrap()
        .replace("Topos MCP", "My MCP");
    std::fs::write(&manifest, &edited).unwrap();

    // An entry UPDATE (the url moved) rewrites the .mcp.json — and must not touch the manifest.
    let v2 = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/v2"),
    );
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![claude_only(&v2)]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(state_of(&out, "s_a", "claude-code").state.wrote());
    let mcp_path = home.0.join(".claude/skills/topos-mcp/.mcp.json");
    assert!(
        std::fs::read_to_string(&mcp_path).unwrap().contains("/v2"),
        "the entry itself still updates"
    );
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        edited,
        "the hand-edited manifest survives an entry update byte-identical"
    );
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.contains("MCP_PLUGIN_MANIFEST_KEPT")),
        "the kept manifest is disclosed: {:?}",
        out.warnings
    );

    // The LAST entry's removal deletes the wholly-owned .mcp.json — and keeps the edited
    // manifest, the dir standing over it, disclosed.
    let out = mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), true);
    assert!(!mcp_path.exists(), "the wholly-owned entries file leaves");
    assert_eq!(
        std::fs::read_to_string(&manifest)
            .unwrap_or_else(|e| panic!("the hand-edited manifest was destroyed on removal: {e}")),
        edited,
        "the hand edit survives the last entry's removal"
    );
    assert!(
        home.0.join(".claude/skills/topos-mcp").exists(),
        "the dir stays with its foreign occupant"
    );
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.contains("MCP_PLUGIN_MANIFEST_KEPT")),
        "{:?}",
        out.warnings
    );
}

/// The heal path stays: a hand-DELETED manifest (absent, so provably nobody's) is re-written
/// pristine beside entries that remain.
#[test]
fn a_hand_deleted_plugin_manifest_heals_back_beside_remaining_entries() {
    let home = Scratch::new("plugin-heal");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.reach = Some(vec!["claude-code".into()]);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let manifest = home
        .0
        .join(".claude/skills/topos-mcp/.claude-plugin/plugin.json");
    std::fs::remove_file(&manifest).unwrap();

    // The next converge (nothing changed — a Leave) re-heals the constant file.
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        std::fs::read(&manifest).unwrap_or_else(|e| panic!("the manifest was not healed: {e}")),
        plugin_dir::manifest_bytes()
    );
}

/// Every converge entry point serializes on the scope's `locks/mcp.lock`: a run that starts
/// while another process holds it WAITS instead of interleaving the custody + config
/// read-modify-write (flock contends across open file descriptions, so a second in-process
/// guard stands in for a sibling process here).
#[test]
fn converges_serialize_on_the_per_scope_mcp_lock() {
    let home = Scratch::new("mcp-lock");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    std::fs::create_dir_all(layout.locks_dir()).unwrap();
    let held = fs
        .lock_exclusive(&layout.locks_dir().join("mcp.lock"))
        .unwrap();

    let home_path = home.0.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let fs = RealFs;
        let layout = Layout::new(&home_path.join(".topos"));
        let io = ScopeIo {
            fs: &fs,
            layout: &layout,
            home: home_path.clone(),
            project_root: None,
        };
        let mut d = demand(
            "s_a",
            "alpha",
            Some("eng"),
            &server_json("https://mcp.example/a"),
        );
        d.reach = Some(vec!["cursor".into()]);
        let out = mcp_engine::converge(
            &io,
            &plan(&io, vec![d.clone()]),
            &synthetic(),
            &all_slugs(),
            &no_hold(),
            true,
        );
        tx.send(out).unwrap();
    });

    // While the lock is held the converge must not complete (and must not have written).
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(400))
            .is_err(),
        "a converge must wait for the scope's mcp lock"
    );
    assert!(
        !home.0.join(".cursor/mcp.json").exists(),
        "no config byte moves while another converge holds the lock"
    );
    drop(held);
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the released lock lets the converge finish");
    worker.join().unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(state_of(&out, "s_a", "cursor").state.wrote());
    assert!(home.0.join(".cursor/mcp.json").exists());
}

/// A surface path move (an env-override change) leaves the custody row recorded at the OLD file:
/// the converge must not silently drop or re-point it — the row is a disclosed stale class,
/// warned with the old path, the row and the old file's bytes both left in place.
#[test]
fn a_moved_surface_path_discloses_the_stale_row_and_never_drops_it() {
    let home = Scratch::new("moved");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.reach = Some(vec!["cursor".into()]);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let old_file = home.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&old_file).unwrap();

    // The surface resolves ELSEWHERE now (the descriptor's suffix moved).
    static MOVED: &[KnownHarness] = &[registry::home_rooted_mcp_row(
        "cursor",
        "Cursor",
        ".cursor-next/mcp.json",
        McpDialect::CursorJson,
        None,
        "restart cursor",
    )];
    // An undemanded removal run over the moved surface: the row is NOT dropped, the old file is
    // NOT touched, and the stale class is disclosed naming the old path.
    let out = mcp_engine::converge(
        &io,
        &[],
        &MOVED.iter().collect::<Vec<_>>(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.contains("MCP_ENTRY_STALE_PATH") && w.contains(".cursor/mcp.json")),
        "{:?}",
        out.warnings
    );
    assert_eq!(
        std::fs::read_to_string(&old_file).unwrap(),
        placed,
        "the old file's bytes stay untouched"
    );
    let custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert!(
        custody.has_entries_for("s_a"),
        "the stale row is kept, never silently dropped: {custody:?}"
    );
    assert_eq!(
        Path::new(
            &custody
                .row(&config_custody::placement_key("cursor", "topos-eng-alpha"))
                .unwrap()
                .file
        ),
        old_file,
        "the row still names the old file"
    );
}

/// A hand-deleted plugin dir must not leave phantom custody entries: the next removal-allowed
/// converge stale-drops the rows the surface no longer holds, so the key retires and the bundle
/// can actually leave.
#[test]
fn a_hand_deleted_plugin_dir_sheds_its_ledger_entries_on_the_next_converge() {
    let home = Scratch::new("plugin-deleted");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.reach = Some(vec!["claude-code".into()]);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(
        ScopeEntries::load(&fs, &layout)
            .unwrap()
            .has_entries_for("s_a")
    );
    // The user deletes the whole plugin dir by hand.
    std::fs::remove_dir_all(home.0.join(".claude/skills/topos-mcp")).unwrap();

    // The demand drops: the custody must shed the phantom rows and give up the key. Nothing
    // stands under the name anywhere (the dir went with the entry), so it is not reserved either.
    mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), true);
    let custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert!(
        !custody.has_entries_for("s_a"),
        "phantom entries survive a hand-deleted plugin dir: {custody:?}"
    );
    assert_eq!(
        custody.key_of("s_a"),
        None,
        "the bundle keeps no live key: {custody:?}"
    );
    assert!(
        custody.doc.retired.values().any(|b| b == "s_a"),
        "and the name it minted stays reserved for the server it named: {custody:?}"
    );
}

/// F1's byte-loss belt, per dialect: a NON-prefixed user entry added to a file topos CREATED is
/// invisible to the drivers' states — the last-entry removal must still never delete the file
/// over it.
#[test]
fn a_user_entry_added_to_a_topos_created_file_survives_last_entry_removal() {
    type Inject = Box<dyn Fn(&str) -> String>;
    let json_inject = |path: &[&str]| -> Inject {
        let path: Vec<String> = path.iter().map(|s| (*s).to_owned()).collect();
        Box::new(move |text: &str| {
            let mut root: serde_json::Value = serde_json::from_str(text).unwrap();
            let mut slot = &mut root;
            for key in &path {
                slot = slot.get_mut(key).unwrap();
            }
            slot.as_object_mut().unwrap().insert(
                "mine".to_owned(),
                serde_json::json!({ "url": "https://user.example" }),
            );
            let mut out = serde_json::to_string_pretty(&root).unwrap();
            out.push('\n');
            out
        })
    };
    let cases: Vec<(&str, &str, &str, Inject)> = vec![
        (
            "cursor",
            ".cursor/mcp.json",
            "mine",
            json_inject(&["mcpServers"]),
        ),
        (
            "opencode",
            ".opencode/opencode.json",
            "mine",
            json_inject(&["mcp"]),
        ),
        (
            "openclaw",
            ".openclaw/openclaw.json",
            "mine",
            json_inject(&["mcp", "servers"]),
        ),
        (
            "codex",
            ".codex/config.toml",
            "model",
            Box::new(|text: &str| format!("model = \"o5\"\n{text}")) as Inject,
        ),
        (
            "hermes-agent",
            ".hermes/config.yaml",
            "their",
            Box::new(|text: &str| format!("{text}  their: {{url: \"https://user.example\"}}\n"))
                as Inject,
        ),
    ];
    for (slug, rel, user_marker, inject) in cases {
        let home = Scratch::new(&format!("keep-{slug}"));
        let fs = RealFs;
        let layout = Layout::new(&home.0.join(".topos"));
        let io = person_io(&fs, &layout, &home.0);
        let mut d = demand(
            "s_a",
            "alpha",
            Some("eng"),
            &server_json("https://mcp.example/a"),
        );
        d.reach = Some(vec![slug.to_owned()]);
        mcp_engine::converge(
            &io,
            &plan(&io, vec![d.clone()]),
            &synthetic(),
            &all_slugs(),
            &no_hold(),
            true,
        );
        let file = home.0.join(rel);
        let created = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{slug}: {e}"));
        // The USER adds a plain (non-topos) entry to the file topos created.
        std::fs::write(&file, inject(&created)).unwrap();

        // The demand drops: our entry leaves, the FILE STAYS with the user's content.
        let out = mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), true);
        assert!(out.warnings.is_empty(), "{slug}: {:?}", out.warnings);
        let kept = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("{slug}: the file was deleted over a user entry: {e}"));
        assert!(
            kept.contains(user_marker),
            "{slug}: the user's entry survives: {kept}"
        );
        assert!(
            !kept.contains("topos-eng-alpha"),
            "{slug}: ours is gone: {kept}"
        );
        let custody = ScopeEntries::load(&fs, &layout).unwrap();
        assert!(!custody.has_entries_for("s_a"), "{slug}");
    }
}

/// F1 belt two: the moment a converge OBSERVES user content in a file topos created, the
/// custody's whole-file-ownership flag goes false — a later removal can never trust a flag the
/// file has outgrown.
#[test]
fn a_converge_that_sees_user_content_flips_owns_file_false() {
    let home = Scratch::new("flip");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.reach = Some(vec!["cursor".into()]);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let lk = config_custody::placement_key("cursor", "topos-eng-alpha");
    assert!(
        ScopeEntries::load(&fs, &layout)
            .unwrap()
            .row(&lk)
            .unwrap()
            .owns_file,
        "created → wholly owned"
    );
    // The user adds a plain entry; the next converge (demand unchanged, a LEAVE) must record
    // that the file is no longer wholly ours.
    let cursor = home.0.join(".cursor/mcp.json");
    let mut root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cursor).unwrap()).unwrap();
    root["mcpServers"].as_object_mut().unwrap().insert(
        "mine".to_owned(),
        serde_json::json!({ "url": "https://user.example" }),
    );
    std::fs::write(&cursor, serde_json::to_string_pretty(&root).unwrap() + "\n").unwrap();
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        state_of(&out, "s_a", "cursor").state,
        TargetOutcome::Current
    );
    assert!(
        !ScopeEntries::load(&fs, &layout)
            .unwrap()
            .row(&lk)
            .unwrap()
            .owns_file,
        "the flag stops lying at the first sighting"
    );
}

/// F4: the engine resolves the SAME remote the publish gate approved — the first with
/// `type == "streamable-http"` AND a url, in one predicate. A url-less streamable remote ahead
/// of the real one must not fail (or redirect) the demand.
#[test]
fn the_engine_places_the_remote_the_gate_approved_not_the_first_typed_one() {
    let home = Scratch::new("remote-pick");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand("s_two", "two", Some("eng"), "");
    d.server_json = br#"{"name":"io.test/two","description":"Two remotes.","version":"1.0.0","remotes":[{"type":"streamable-http"},{"type":"streamable-http","url":"https://second.example/mcp"}]}"#.to_vec();
    d.reach = Some(vec!["cursor".into()]);
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(state_of(&out, "s_two", "cursor").state.wrote());
    let text = std::fs::read_to_string(home.0.join(".cursor/mcp.json")).unwrap();
    assert!(text.contains("https://second.example/mcp"), "{text}");
}

#[test]
fn holds_and_targeted_runs_never_remove_standing_entries() {
    let home = Scratch::new("hold");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.reach = Some(vec!["cursor".into()]);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let cursor = home.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&cursor).unwrap();

    // Undemanded but HELD (its workspace was unreachable): byte-identical, custody kept.
    let hold: HashSet<String> = ["s_a".to_owned()].into();
    mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &hold, true);
    assert_eq!(std::fs::read_to_string(&cursor).unwrap(), placed);
    assert!(
        ScopeEntries::load(&fs, &layout)
            .unwrap()
            .has_entries_for("s_a")
    );

    // Undemanded on a run that may NOT remove (a targeted update): same freeze.
    mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), false);
    assert_eq!(std::fs::read_to_string(&cursor).unwrap(), placed);
    assert!(
        ScopeEntries::load(&fs, &layout)
            .unwrap()
            .has_entries_for("s_a")
    );
}

#[test]
fn intent_journal_recovery_heals_both_crash_orders_through_the_engine() {
    let home = Scratch::new("recover");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.reach = Some(vec!["cursor".into()]);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let cursor = home.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&cursor).unwrap();
    let real_fp =
        mcp::observe(McpDialect::CursorJson, Some(placed.as_bytes())).entries["topos-eng-alpha"]
            .clone();

    // ORDER (b): the config write LANDED, the custody promotion did not — on disk the custody
    // still carries the old fingerprint plus the journaled intent. Without recovery the entry
    // would read as user drift forever.
    let mut custody = ScopeEntries::load(&fs, &layout).unwrap();
    let lk = config_custody::placement_key("cursor", "topos-eng-alpha");
    let mut stale = custody.row(&lk).unwrap().clone();
    stale.fingerprint = "0".repeat(64);
    custody.put(lk.clone(), "s_a".to_owned(), stale);
    custody
        .journal(
            &fs,
            &layout,
            std::iter::once((
                lk.clone(),
                config_custody::PendingIntent {
                    bundle_id: "s_a".into(),
                    version_id: "v1".into(),
                    file: cursor.display().to_string(),
                    fingerprint: real_fp.clone(),
                    owns_file: true,
                },
            ))
            .collect(),
        )
        .unwrap();
    assert!(custody.flush(&fs, &layout).is_empty());
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        (
            state_of(&out, "s_a", "cursor").state,
            state_of(&out, "s_a", "cursor").note.as_deref()
        ),
        (TargetOutcome::Current, None),
        "recovery promoted the landed write instead of reading it as drift"
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        placed,
        "no rewrite"
    );
    let custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert!(custody.doc.pending.is_empty());
    assert_eq!(custody.row(&lk).unwrap().fingerprint, real_fp);

    // ORDER (a): the intent was journaled, the config write never landed. Recovery drops the
    // intent; the standing entry (which matches the file) stays authoritative.
    let mut custody = ScopeEntries::load(&fs, &layout).unwrap();
    custody
        .journal(
            &fs,
            &layout,
            std::iter::once((
                lk.clone(),
                config_custody::PendingIntent {
                    bundle_id: "s_a".into(),
                    version_id: "v2".into(),
                    file: cursor.display().to_string(),
                    fingerprint: "f".repeat(64),
                    owns_file: true,
                },
            ))
            .collect(),
        )
        .unwrap();
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        state_of(&out, "s_a", "cursor").state,
        TargetOutcome::Current
    );
    let custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert!(custody.doc.pending.is_empty());
    assert_eq!(custody.row(&lk).unwrap().fingerprint, real_fp);
}

/// ONE JOURNAL, MANY SURFACES. A converge walks every engaged harness in one run, and each
/// surface that writes journals its own intents — but there is only one journal, and journalling
/// REPLACES it. So intents a surface left standing (its record write failed, so the work has not
/// landed) must survive the surfaces that come after it.
///
/// The scenario: bundle X places on surface A, X's `entries.json` write FAILS there, and its
/// intents are re-journalled. Bundle Y then converges on surface B in the same run. Without the
/// guard, B's `journal()` overwrites X's intents in memory and on disk, B promotes and flushes,
/// and X is left live in A's config with no row and no journal — a state nothing can heal.
///
/// With the guard, B is refused for this run (the fail-closed posture the converge-start recovery
/// already takes), X's intents stay on disk, and the NEXT clean run heals X's row and places Y.
#[test]
fn a_later_surface_never_journals_over_intents_an_earlier_one_left_standing() {
    let probe = {
        let home = Scratch::new("xsurface-probe");
        let layout = Layout::new(&home.0.join(".topos"));
        let fault = FaultFs::new(0);
        let io = ScopeIo {
            fs: &fault,
            layout: &layout,
            home: home.0.clone(),
            project_root: None,
        };
        mcp_engine::converge(
            &io,
            &plan(&io, two_bundle_demands()),
            &synthetic(),
            &all_slugs(),
            &no_hold(),
            true,
        );
        fault.ops_attempted()
    };
    assert!(probe > 0);

    let mut proven = 0usize;
    for fail_at in 1..=probe {
        let fs = RealFs;
        let home = Scratch::new(&format!("xsurface-{fail_at}"));
        let layout = Layout::new(&home.0.join(".topos"));
        let codex = home.0.join(".codex/config.toml");
        // Both bundles hold RECORDS of their own, so their row writes are real I/O that can fail.
        for id in ["s_x", "s_y"] {
            let sid = crate::id::SkillId::parse(id).unwrap();
            std::fs::create_dir_all(layout.skill_dir(&sid)).unwrap();
        }
        {
            let fault = FaultFs::new(fail_at);
            let io = ScopeIo {
                fs: &fault,
                layout: &layout,
                home: home.0.clone(),
                project_root: None,
            };
            let _ = mcp_engine::converge(
                &io,
                &plan(&io, two_bundle_demands()),
                &synthetic(),
                &all_slugs(),
                &no_hold(),
                true,
            );
        }
        // The ordering this guards: X's entry is LIVE in its config file, and X's record does not
        // know about it.
        let x_live = std::fs::read_to_string(&codex)
            .map(|t| t.contains("topos-eng-ex"))
            .unwrap_or(false);
        let x_recorded = !crate::config_custody::entries_of(&fs, &layout, "s_x").is_empty();
        if !x_live || x_recorded {
            continue;
        }
        proven += 1;
        // THE INVARIANT: X's intents survived every later surface in that run.
        let doc = crate::config_custody::read(&fs, &layout).expect("decipherable");
        assert!(
            doc.pending.values().any(|i| i.bundle_id == "s_x"),
            "fail_at={fail_at}: a later surface journalled over X's outstanding intents — the \
             entry is live with no row and nothing to recover from: {doc:?}"
        );
        // And the next clean run heals it.
        {
            let io = ScopeIo {
                fs: &fs,
                layout: &layout,
                home: home.0.clone(),
                project_root: None,
            };
            mcp_engine::converge(
                &io,
                &plan(&io, two_bundle_demands()),
                &synthetic(),
                &all_slugs(),
                &no_hold(),
                true,
            );
        }
        assert!(
            !crate::config_custody::entries_of(&fs, &layout, "s_x").is_empty(),
            "fail_at={fail_at}: the next run must heal X's row"
        );
        assert!(
            crate::config_custody::read(&fs, &layout)
                .unwrap()
                .pending
                .is_empty(),
            "fail_at={fail_at}: and clear the journal once it has"
        );
    }
    assert!(
        proven > 0,
        "no fault point produced the live-entry / unrecorded-row ordering this guards"
    );
}

/// Two bundles that converge on DIFFERENT surfaces in one run — the cross-surface fixture.
fn two_bundle_demands() -> Vec<DemandedBundle> {
    // X rides the EARLIER surface in table order (codex) and Y the later one (cursor), so Y's
    // journal write genuinely comes after X has left intents standing — the ordering the guard is
    // about. Reversed, X is the last surface and nothing follows it to overwrite anything.
    let mut x = demand(
        "s_x",
        "ex",
        Some("eng"),
        &server_json("https://mcp.example/x"),
    );
    x.reach = Some(vec!["codex".into()]);
    let mut y = demand(
        "s_y",
        "why",
        Some("eng"),
        &server_json("https://mcp.example/y"),
    );
    y.reach = Some(vec!["cursor".into()]);
    vec![x, y]
}

/// A REMOVAL must not swallow a crash left by an earlier run. `remove` is often the FIRST command
/// after a crash, and it journals intents of its own — and journalling REPLACES the journal
/// wholesale, so a recovery promotion left only in memory can be overwritten before it is durable.
/// `remove_bundle` therefore flushes its recovery pass before converging anything.
///
/// What this test guards is the OUTCOME, not that ordering: a crash-left intent, a removal as the
/// first command, one injected failure anywhere in its run, and then a clean retry — which must
/// finish the removal. An entry that survives the retry is stranded forever, because nothing is
/// left to prove it was topos's.
///
/// NOTE, honestly: this does not discriminate the early flush on its own. `remove_bundle`'s
/// trailing flush already persists a recovered promotion under a single injected fault, so the
/// early flush is defense in depth — it makes the durability ordering explicit instead of
/// incidental to where the last write happens to sit.
#[test]
fn a_removal_never_swallows_a_crash_left_intent_before_it_is_durable() {
    let probe = {
        let home = Scratch::new("rm-recover-probe");
        let layout = Layout::new(&home.0.join(".topos"));
        let fault = FaultFs::new(0);
        let io = ScopeIo {
            fs: &fault,
            layout: &layout,
            home: home.0.clone(),
            project_root: None,
        };
        let mut d = demand(
            "s_a",
            "alpha",
            Some("eng"),
            &server_json("https://mcp.example/a"),
        );
        d.reach = Some(vec!["cursor".into()]);
        mcp_engine::converge(
            &io,
            &plan(&io, vec![d.clone()]),
            &synthetic(),
            &all_slugs(),
            &no_hold(),
            true,
        );
        fault.ops_attempted()
    };

    for fail_at in 1..=probe {
        let fs = RealFs;
        let home = Scratch::new(&format!("rm-recover-{fail_at}"));
        let layout = Layout::new(&home.0.join(".topos"));
        let cursor = home.0.join(".cursor/mcp.json");
        let mut d = demand(
            "s_a",
            "alpha",
            Some("eng"),
            &server_json("https://mcp.example/a"),
        );
        d.reach = Some(vec!["cursor".into()]);
        // A clean placement first.
        {
            let io = ScopeIo {
                fs: &fs,
                layout: &layout,
                home: home.0.clone(),
                project_root: None,
            };
            mcp_engine::converge(
                &io,
                &plan(&io, vec![d.clone()]),
                &synthetic(),
                &all_slugs(),
                &no_hold(),
                true,
            );
        }
        // A CRASH-LEFT intent: the previous run journalled and died before promoting. Its
        // fingerprint is what the file provably holds, so recovery must promote it.
        let real_fp = mcp::observe(
            McpDialect::CursorJson,
            std::fs::read(&cursor).ok().as_deref(),
        )
        .entries["topos-eng-alpha"]
            .clone();
        let mut custody = ScopeEntries::load(&fs, &layout).unwrap();
        let ck = config_custody::placement_key("cursor", "topos-eng-alpha");
        custody
            .journal(
                &fs,
                &layout,
                std::iter::once((
                    ck.clone(),
                    config_custody::PendingIntent {
                        bundle_id: "s_a".into(),
                        version_id: "v-crashed".into(),
                        file: cursor.display().to_string(),
                        fingerprint: real_fp.clone(),
                        owns_file: true,
                    },
                ))
                .collect(),
            )
            .unwrap();
        assert!(
            !crate::config_custody::read(&fs, &layout)
                .unwrap()
                .pending
                .is_empty()
        );

        // The removal runs as the FIRST command, and something in it fails.
        {
            let fault = FaultFs::new(fail_at);
            let io = ScopeIo {
                fs: &fault,
                layout: &layout,
                home: home.0.clone(),
                project_root: None,
            };
            let _ = mcp_engine::remove_bundle(&io, &synthetic(), &all_slugs(), "s_a", "a");
        }

        // The custody document must still be decipherable whatever failed.
        crate::config_custody::read(&fs, &layout).expect("decipherable");

        // THE INVARIANT, stated where a person would feel it: a clean retry FINISHES the removal.
        // If the crash-left promotion was swallowed — cleared in memory, then overwritten on disk
        // by the removal's own journal write — the entry loses its record, and no later run can
        // prove it was topos's: the retry leaves it in the config file forever.
        {
            let io = ScopeIo {
                fs: &fs,
                layout: &layout,
                home: home.0.clone(),
                project_root: None,
            };
            mcp_engine::remove_bundle(&io, &synthetic(), &all_slugs(), "s_a", "a");
        }
        let left = std::fs::read_to_string(&cursor).unwrap_or_default();
        assert!(
            !left.contains("topos-eng-alpha"),
            "fail_at={fail_at}: a clean retry must finish the removal — the entry is stranded: \
             {left}"
        );
    }
}

/// A DRIFTED entry survives the removal of the record that owned it. Drift is never clobbered, so
/// a removal legitimately leaves the entry standing — and a classic `remove` then deletes the
/// record directory, taking `entries.json` with it. If the row went with it, the hand-edited entry
/// would sit in the person's config forever with nothing left to prove it was ever topos's: no
/// later sweep could disclose it and none could ever take it out.
///
/// So the surviving rows move to the scope document first. They stay disclosable, the bundle keeps
/// its key (it still has entries, so retirement has not fired), and once the hand edit is reverted
/// an ordinary sweep removes the entry — the eventual cleanup, restored.
#[test]
fn a_drifted_entry_outlives_the_record_and_is_still_cleaned_up_later() {
    let fs = RealFs;
    let home = Scratch::new("drift-outlives");
    let layout = Layout::new(&home.0.join(".topos"));
    let cursor = home.0.join(".cursor/mcp.json");
    let sid = crate::id::SkillId::parse("s_a").unwrap();
    // A bundle WITH a record of its own — the case the detach exists for.
    std::fs::create_dir_all(layout.skill_dir(&sid)).unwrap();

    let io = ScopeIo {
        fs: &fs,
        layout: &layout,
        home: home.0.clone(),
        project_root: None,
    };
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.reach = Some(vec!["cursor".into()]);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let pristine = std::fs::read_to_string(&cursor).unwrap();
    assert!(pristine.contains("topos-eng-alpha"));
    assert!(
        !crate::config_custody::entries_of(&fs, &layout, "s_a").is_empty(),
        "the row lives in the record while the record lives"
    );

    // The person hand-edits the entry: it is now DRIFTED and will never be clobbered.
    std::fs::write(
        &cursor,
        pristine.replace("mcp.example/a", "mcp.example/hand"),
    )
    .unwrap();

    // The classic remove: converge the removal, move what survived, then delete the record.
    let out = mcp_engine::remove_bundle(&io, &synthetic(), &all_slugs(), "s_a", "a");
    assert!(
        out.removed
            .iter()
            .any(|r| r.state.state == TargetOutcome::Drifted),
        "the hand-edited entry is left in place and disclosed: {out:?}"
    );
    // A detach that CANNOT land must say so: losing custody of a drifted row is the person's
    // business, not a silent fact. The faulted attempt reports, and the warning is the shape the
    // receipt folds into its lines.
    {
        let fault = FaultFs::new(1);
        let faulted = ScopeIo {
            fs: &fault,
            layout: &layout,
            home: home.0.clone(),
            project_root: None,
        };
        let warnings = mcp_engine::detach_bundle_rows(&faulted, "s_a");
        assert!(
            crate::message::legacy_lines(&warnings)
                .iter()
                .any(|w| w.contains("MCP_CUSTODY_WRITE_FAILED")
                    || w.contains("MCP_LOCK_UNAVAILABLE")),
            "a detach that cannot land must report it: {warnings:?}"
        );
    }

    mcp_engine::detach_bundle_rows(&io, "s_a");
    std::fs::remove_dir_all(layout.skill_dir(&sid)).unwrap();

    // The row outlived its record, in the scope document, under the same bundle identity.
    let doc = crate::config_custody::read(&fs, &layout).unwrap();
    assert!(
        doc.unrecorded.contains_key("s_a"),
        "the surviving row moved out of the record before it was deleted: {doc:?}"
    );
    assert!(
        doc.keys.contains_key("s_a"),
        "the key is NOT retired while an entry of its still stands: {doc:?}"
    );
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("mcp.example/hand"),
        "the hand edit is untouched"
    );

    // A later sweep with the bundle undemanded still SEES it — and still leaves the drift alone.
    let out = mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), true);
    assert!(
        out.removed
            .iter()
            .any(|r| r.bundle_id == "s_a" && r.state.state == TargetOutcome::Drifted),
        "a sweep still discloses the entry it can no longer remove: {out:?}"
    );
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("mcp.example/hand")
    );

    // The person reverts the hand edit. Now the entry matches what topos recorded, so the next
    // ordinary sweep takes it out — the cleanup that the lost row would have made impossible.
    std::fs::write(&cursor, &pristine).unwrap();
    mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), true);
    assert!(
        !std::fs::read_to_string(&cursor)
            .unwrap_or_default()
            .contains("topos-eng-alpha"),
        "the reverted entry is finally removed"
    );
    let doc = crate::config_custody::read(&fs, &layout).unwrap();
    assert!(!doc.unrecorded.contains_key("s_a"), "{doc:?}");
    assert!(
        !doc.keys.contains_key("s_a"),
        "and only now does the key leave the bundle: {doc:?}"
    );
    assert!(
        doc.retired.values().any(|b| b == "s_a"),
        "and the name it minted stays reserved — a sign-in filed under it outlives the entry: \
         {doc:?}"
    );
}

/// THE JOURNAL MUST DESCRIBE EXACTLY THE WORK THAT HAS NOT LANDED. A config write lands, then the
/// write of the bundle's own custody document FAILS. The promotion is therefore only in memory —
/// so the intents that produced it are still outstanding, and the scope document written on that
/// failure path must still carry them. Clearing the journal there would strand a live entry with
/// no record, permanently: every later run reads it as a hand edit and refuses to touch it.
///
/// Sweeps the fault point to find the exact ordering (config landed, record not), then asserts the
/// journal survived on disk and that a clean re-run's recovery promotes the row.
#[test]
fn a_failed_record_write_keeps_its_intents_in_the_durable_journal() {
    let probe = {
        let home = Scratch::new("keep-intent-probe");
        let layout = Layout::new(&home.0.join(".topos"));
        let fault = FaultFs::new(0);
        let io = ScopeIo {
            fs: &fault,
            layout: &layout,
            home: home.0.clone(),
            project_root: None,
        };
        let mut d = demand(
            "s_a",
            "alpha",
            Some("eng"),
            &server_json("https://mcp.example/a"),
        );
        d.reach = Some(vec!["cursor".into()]);
        mcp_engine::converge(
            &io,
            &plan(&io, vec![d.clone()]),
            &synthetic(),
            &all_slugs(),
            &no_hold(),
            true,
        );
        fault.ops_attempted()
    };
    assert!(probe > 0);

    let mut proven = 0usize;
    for fail_at in 1..=probe {
        let home = Scratch::new(&format!("keep-intent-{fail_at}"));
        let layout = Layout::new(&home.0.join(".topos"));
        let cursor = home.0.join(".cursor/mcp.json");
        let mut d = demand(
            "s_a",
            "alpha",
            Some("eng"),
            &server_json("https://mcp.example/a"),
        );
        d.reach = Some(vec!["cursor".into()]);
        {
            let fault = FaultFs::new(fail_at);
            let io = ScopeIo {
                fs: &fault,
                layout: &layout,
                home: home.0.clone(),
                project_root: None,
            };
            let _ = mcp_engine::converge(
                &io,
                &plan(&io, vec![d.clone()]),
                &synthetic(),
                &all_slugs(),
                &no_hold(),
                true,
            );
        }
        // The ordering this test is about: the ENTRY is in the config file, but the bundle's own
        // custody document does not record it.
        let landed = std::fs::read_to_string(&cursor)
            .map(|t| t.contains("topos-eng-alpha"))
            .unwrap_or(false);
        let fs = RealFs;
        let recorded = !crate::config_custody::entries_of(&fs, &layout, "s_a").is_empty();
        if !landed || recorded {
            continue;
        }
        proven += 1;
        // THE INVARIANT: the journal on disk still describes the unlanded work.
        let doc = crate::config_custody::read(&fs, &layout).expect("the scope doc is decipherable");
        assert!(
            !doc.pending.is_empty(),
            "fail_at={fail_at}: the config write landed and the record did not — the journal must \
             still carry the intent, or the entry is stranded forever"
        );
        // And recovery finishes it: a clean re-run promotes the row into the record.
        {
            let io = ScopeIo {
                fs: &fs,
                layout: &layout,
                home: home.0.clone(),
                project_root: None,
            };
            mcp_engine::converge(
                &io,
                &plan(&io, vec![d.clone()]),
                &synthetic(),
                &all_slugs(),
                &no_hold(),
                true,
            );
        }
        assert!(
            !crate::config_custody::entries_of(&fs, &layout, "s_a").is_empty(),
            "fail_at={fail_at}: recovery must promote the journaled row"
        );
        assert!(
            crate::config_custody::read(&fs, &layout)
                .unwrap()
                .pending
                .is_empty(),
            "fail_at={fail_at}: and clear the journal once it has"
        );
    }
    assert!(
        proven > 0,
        "no fault point produced the landed-config / unrecorded-custody ordering this guards"
    );
}

#[test]
fn a_fault_at_any_write_never_tears_state_and_the_next_converge_heals() {
    // The fault sweep through the fs seam: fail exactly one mutating op, at every op the converge
    // performs, and prove the invariant pair — nothing torn (the custody stays decipherable), and
    // a clean re-run always ends fully placed with a file-matching custody.
    let probe = {
        let home = Scratch::new("fault-probe");
        let layout = Layout::new(&home.0.join(".topos"));
        let fault = FaultFs::new(0);
        let io = ScopeIo {
            fs: &fault,
            layout: &layout,
            home: home.0.clone(),
            project_root: None,
        };
        let mut d = demand(
            "s_a",
            "alpha",
            Some("eng"),
            &server_json("https://mcp.example/a"),
        );
        d.reach = Some(vec!["cursor".into()]);
        mcp_engine::converge(
            &io,
            &plan(&io, vec![d.clone()]),
            &synthetic(),
            &all_slugs(),
            &no_hold(),
            true,
        );
        fault.ops_attempted()
    };
    assert!(probe > 0);
    for fail_at in 1..=probe {
        let home = Scratch::new(&format!("fault-{fail_at}"));
        let layout = Layout::new(&home.0.join(".topos"));
        let mut d = demand(
            "s_a",
            "alpha",
            Some("eng"),
            &server_json("https://mcp.example/a"),
        );
        d.reach = Some(vec!["cursor".into()]);
        {
            let fault = FaultFs::new(fail_at);
            let io = ScopeIo {
                fs: &fault,
                layout: &layout,
                home: home.0.clone(),
                project_root: None,
            };
            let _ = mcp_engine::converge(
                &io,
                &plan(&io, vec![d.clone()]),
                &synthetic(),
                &all_slugs(),
                &no_hold(),
                true,
            );
        }
        // The clean re-run heals whatever the fault interrupted.
        let fs = RealFs;
        let io = person_io(&fs, &layout, &home.0);
        let out = mcp_engine::converge(
            &io,
            &plan(&io, vec![d.clone()]),
            &synthetic(),
            &all_slugs(),
            &no_hold(),
            true,
        );
        // Fully placed either way: `placed` where this clean re-run did the writing, `current`
        // where the faulted run had already landed the entry and recovery promoted it. The
        // file-vs-custody check below is what proves the placement itself.
        let st = state_of(&out, "s_a", "cursor").state;
        assert!(
            st.wrote() || st == TargetOutcome::Current,
            "fail_at={fail_at}: {out:?}"
        );
        let bytes = std::fs::read(home.0.join(".cursor/mcp.json")).unwrap();
        let observed = mcp::observe(McpDialect::CursorJson, Some(&bytes));
        let custody = ScopeEntries::load(&fs, &layout).unwrap();
        assert!(custody.doc.pending.is_empty(), "fail_at={fail_at}");
        assert_eq!(
            observed.entries.get("topos-eng-alpha"),
            Some(
                &custody
                    .row(&config_custody::placement_key("cursor", "topos-eng-alpha"))
                    .unwrap()
                    .fingerprint
            ),
            "fail_at={fail_at}: the healed custody matches the file"
        );
    }
}
