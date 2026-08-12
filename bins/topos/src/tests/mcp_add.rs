//! `add --kind mcp` and the publish half of the `kind = "mcp"` bundle: the doors a server comes in
//! through, the row each one records, and the gate that stands between a server document and the
//! workspace.
//!
//! What is under test here:
//!
//! - the LOCAL door — the ONLY door — applies immediately, and its row is `add`/`remove`'s exact
//!   file inverse even carrying `{ kind = "mcp" }`; an adopted folder is never deleted, by any arm
//!   of `remove`, because topos never created it;
//! - a folder whose kind is not readable off it refuses toward the `--kind` word rather than
//!   landing as the nearest guess;
//! - a registry-shaped name resolves WORKSPACE-FIRST: a connected catalog EMBEDDING it
//!   subscribes to that bundle by its catalog name (source disclosed, registry never dialed),
//!   several embedding it refuse toward `--workspace`, and a miss falls through to the
//!   registry — whose 404 then names both consulted sources;
//! - the registry name goes to the versions/latest endpoint as ONE encoded path segment, and its
//!   `{server, _meta}` envelope is unwrapped before a byte is stored;
//! - a document carrying a credential is REFUSED at every door, with the shared typed code, before
//!   anything is written — including at `publish`, where the refusal must land before the op WAL;
//! - the bundle kind rides the WAL onto the wire, and the landed publish's governance rewrite
//!   drops the local `kind` field cleanly (the catalog is the authority from then on).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::ops::{self};
use crate::plane::{InertFollow, InertPlane};
use crate::sessions::{self, SESSION_ACTIVE, Session};
use crate::sidecar::Layout;
use crate::test_support::MockHarness;
use topos_core::digest::ManifestEntry;
use topos_core::digest::{self, FileMode};
use topos_core::identity::Commit;
// The directory + governance fakes are the manifest suite's — one pair of stand-ins for the whole
// test tree, so a trait's growth is felt in one place.
use super::manifest_reconcile::{FakeDirectory, NoGovernance};

const HOST: &str = "acme.test";
const WS: &str = "w_eng";
const WS_NAME: &str = "eng";

// =================================================================================================
// The rig
// =================================================================================================

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-mcpa-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Canonical, so recorded and derived paths compare equal on macOS's symlinked $TMPDIR.
        let dir = dir.canonicalize().unwrap_or(dir);
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A no-op adapter: discovers nothing, touches no config.
fn no_harness() -> MockHarness {
    MockHarness::joining("")
}

struct Rig {
    home: Scratch,
    work: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: MockHarness,
}
impl Rig {
    fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(&format!("{tag}-home")),
            work: Scratch::new(&format!("{tag}-work")),
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1_700_000_000_000),
            harness: no_harness(),
        }
    }
    fn layout(&self) -> Layout {
        Layout::new(&self.home.0.join(".topos"))
    }
    fn ctx_at<'a>(&'a self, cwd: Option<&Path>) -> Ctx<'a> {
        Ctx {
            progress: crate::progress::silent(),
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: "d_test".into(),
            layout: self.layout(),
            harness: &self.harness,
            triggers: crate::ops::Triggers::active_only(&crate::ops::INERT_TRIGGER),
            plane: &InertPlane,
            follow: &InertFollow,
            roots: Some(crate::ctx::AgentRoots {
                home: self.home.0.clone(),
                cwd: cwd.map(Path::to_path_buf),
            }),
        }
    }
    fn manifest(&self) -> PathBuf {
        self.layout().home().join(crate::manifest::MANIFEST_FILE)
    }
    fn write_global(&self, body: &str) {
        let home = self.layout().home().to_path_buf();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(self.manifest(), body).unwrap();
    }
    fn global_text(&self) -> String {
        std::fs::read_to_string(self.manifest()).unwrap_or_default()
    }
    fn seed_session(&self) {
        sessions::upsert_session(
            &self.fs,
            &self.layout(),
            Session {
                host: HOST.into(),
                base_url: format!("https://{HOST}/api"),
                workspace_id: WS.into(),
                workspace_name: WS_NAME.into(),
                display_name: "Engineering".into(),
                session_id: "sn_1".into(),
                credential: "cred-1".into(),
                status: SESSION_ACTIVE.into(),
                logged_in_at: 1,
            },
        )
        .unwrap();
    }
}

/// A clean, shareable server document — one https streamable-http remote, one literal header.
fn good_server() -> String {
    r#"{"name":"io.github.acme/weather","description":"Conditions for a named place.",
        "version":"1.4.0","remotes":[{"type":"streamable-http",
        "url":"https://weather.acme.example/mcp","headers":[{"name":"X-Region","value":"eu-west-1"}]}]}"#
        .to_owned()
}

/// The same document with somebody's GitHub token smuggled into a header value.
fn server_with_secret() -> String {
    format!(
        r#"{{"name":"io.github.acme/weather","description":"Conditions for a named place.",
        "version":"1.4.0","remotes":[{{"type":"streamable-http",
        "url":"https://weather.acme.example/mcp","headers":[{{"name":"X-Token","value":"ghp_{}"}}]}}]}}"#,
        "A1b2C3d4E5".repeat(4)
    )
}

/// Write a local MCP bundle folder (the dir IS the bundle).
fn write_bundle(dir: &Path, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("server.json"), body).unwrap();
}

// =================================================================================================
// The LOCAL door — applies immediately, undo-led
// =================================================================================================

/// The add's inline converge fans out to exactly the engaged agents — the same set the next sweep
/// keeps (narrowing rides the row's own `dest`, which a fresh adopt does not have). Run at PROJECT
/// scope, where every surface is checkout-relative and hermetic by construction: the four
/// project-capable agents engage deterministically, and the breadth line names them all.
#[test]
fn the_add_converge_matches_the_sweeps_unnarrowed_reach() {
    let rig = Rig::new("narrow");
    rig.write_global("[bundles]\n");
    // claude-code + cursor detect off the fake home; codex + opencode engage through their
    // seeded project files.
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    let proj = Scratch::new("narrow-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::create_dir_all(proj.0.join(".codex")).unwrap();
    std::fs::write(proj.0.join(".codex/config.toml"), b"").unwrap();
    std::fs::write(proj.0.join("opencode.json"), b"").unwrap();
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), "[bundles]\n").unwrap();
    let src = proj.0.join("weather");
    write_bundle(&src, &good_server());
    let ctx = rig.ctx_at(Some(&proj.0));

    let data = ops::add_mcp(&ctx, &src.display().to_string(), false, &Default::default())
        .unwrap()
        .data;
    for rel in [
        ".mcp.json",
        ".codex/config.toml",
        ".cursor/mcp.json",
        "opencode.json",
    ] {
        let placed = std::fs::read_to_string(proj.0.join(rel))
            .unwrap_or_else(|e| panic!("{rel}: {e} ({data:?})"));
        assert!(
            placed.contains("https://weather.acme.example/mcp"),
            "{rel}: {placed}"
        );
    }
    // The breadth line is the whole engaged set — a row without dest reaches every agent.
    let mcp = data.mcp.expect("the typed block");
    assert_eq!(mcp.agents.len(), 4, "{:?}", mcp.agents);
}
/// ITEM PAIR (the add-time receipt's honesty): a fresh placement's line carries the harness's
/// reload note — how the change goes LIVE ("restart Cursor") — exactly as the update path's
/// receipt does; and when NO MCP-capable agent is set up, the receipt says the row waits for one
/// instead of closing with "each agent's own MCP config (named above)" over nothing named.
#[test]
fn the_add_receipt_carries_the_reload_note_and_says_when_nobody_is_reached() {
    // Arm 1: an agent is set up — its placement line names the reload step.
    let rig = Rig::new("reload-note");
    rig.write_global("[bundles]\n");
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    let src = rig.work.0.join("weather");
    write_bundle(&src, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = ops::add_mcp(&ctx, &src.display().to_string(), true, &Default::default())
        .unwrap()
        .data;
    let note = data.note.clone().unwrap_or_default();
    assert!(
        note.contains("~/.cursor/mcp.json: server entry — restart Cursor"),
        "the sub-line keys by the config file, `~`-abbreviated: {note}"
    );

    // Arm 2: NO agent is set up — the receipt (typed note AND the TTY closing line) says the row
    // waits, never "named above" with nothing named.
    let rig = Rig::new("nobody");
    rig.write_global("[bundles]\n");
    let src = rig.work.0.join("weather");
    write_bundle(&src, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = ops::add_mcp(&ctx, &src.display().to_string(), true, &Default::default())
        .unwrap()
        .data;
    assert!(
        data.mcp.as_ref().is_some_and(|m| m.agents.is_empty()),
        "{:?}",
        data.mcp
    );
    let note = data.note.clone().unwrap_or_default();
    assert!(
        note.contains("No MCP-capable agent is set up here yet"),
        "{note}"
    );
    // The TTY carries the same disclosure the typed note does — the receipt never closes over a
    // reach it did not have.
    let tty = crate::render::add_tty(&data);
    assert!(
        tty.contains("No MCP-capable agent is set up here yet"),
        "{tty}"
    );
    assert!(!tty.contains("named above"), "{tty}");
}
// =================================================================================================
// The LOCAL door
// =================================================================================================

/// A folder on this machine is the person's own material: it applies immediately, adopts exactly
/// as a plain `add <path>` does, and the row it records carries the kind.
#[test]
fn a_local_folder_applies_immediately_with_a_kind_row() {
    let rig = Rig::new("local");
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let data = ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default())
        .unwrap()
        .data;
    // It went through the ordinary adopt: a real record with a real version identity.
    assert!(data.skill_id.is_some(), "the folder was adopted");
    assert!(data.version_id.is_some(), "the adopt minted a version");

    let text = rig.global_text();
    assert!(
        text.contains(&format!("\"{}\" = {{ kind = \"mcp\" }}", dir.display())),
        "{text}"
    );
}

/// `add --kind mcp` and `remove` stay EXACT FILE INVERSES even when the row carries a kind: the whole
/// row is dropped, and the file returns to the bytes it had.
#[test]
fn add_mcp_and_remove_are_exact_file_inverses() {
    let rig = Rig::new("inverse");
    rig.write_global("[bundles]\n# a comment that must survive\n\"topos.sh/eng/deploy\" = \"*\"\n");
    let before = rig.global_text();
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default()).unwrap();
    assert_ne!(rig.global_text(), before, "the add changed the file");

    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(FakeDirectory::new(Vec::new(), Vec::new())),
        contribute: Box::new(RecordingPublish::default()),
        governance: Box::new(NoGovernance),
    };
    let outcome = ops::remove_global(
        &ctx,
        &session_connect,
        &[dir.display().to_string()],
        None,
        true,
        &Default::default(),
    )
    .unwrap();
    assert!(
        matches!(outcome, ops::RemoveOutcome::Applied(_)),
        "the removal applies"
    );
    assert_eq!(
        rig.global_text(),
        before,
        "remove is add's exact inverse, kind and all"
    );
    // An ADOPTED folder is the person's own material — no import record, never deleted.
    assert!(
        dir.join("server.json").exists(),
        "remove never deletes an adopted folder"
    );
}

/// A `~/`-spelled row resolves like every other local-path key: `remove` of an adopted mcp
/// bundle whose row the person re-spelled home-rooted still finds the tracked identity the adopt
/// recorded, and the inline converge takes its config entries out with the row — they do not
/// linger undisclosed for the next sweep.
#[test]
fn a_tilde_spelled_adopted_row_converges_its_config_entries_out_on_remove() {
    let rig = Rig::new("tilde");
    rig.write_global("[bundles]\n");
    // One MCP-capable agent set up in the fake home, so the add's converge places an entry.
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    let dir = rig.home.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default()).unwrap();
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("weather.acme.example"),
        "the add placed the entry"
    );

    // The person re-spells the row by hand, `~/`-rooted.
    let respelled = rig
        .global_text()
        .replace(&dir.display().to_string(), "~/weather");
    assert!(respelled.contains("\"~/weather\""), "{respelled}");
    std::fs::write(rig.manifest(), &respelled).unwrap();

    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(FakeDirectory::new(Vec::new(), Vec::new())),
        contribute: Box::new(RecordingPublish::default()),
        governance: Box::new(NoGovernance),
    };
    let outcome = ops::remove_global(
        &ctx,
        &session_connect,
        &["~/weather".to_owned()],
        None,
        true,
        &Default::default(),
    )
    .unwrap();
    let ops::RemoveOutcome::Applied(removed) = outcome else {
        panic!("the re-spelled row still removes");
    };
    // The inline converge answered on the receipt AND the entry actually left.
    let note = removed.items[0].note.clone().unwrap_or_default();
    assert!(
        note.contains("the server's entry was removed."),
        "the inline converge must reach a ~/ row's entries: {note}"
    );
    assert!(
        std::fs::read_to_string(&cursor)
            .map(|t| !t.contains("weather.acme.example"))
            .unwrap_or(true),
        "the config entry left with the row"
    );
}

/// ITEM PAIR (saying no kind at all): a plain `topos add ./folder` — and publish's auto-add — on a
/// folder whose root holds `server.json` and no `SKILL.md` refuses with the `--kind mcp` hint
/// instead of silently adopting a SKILL that delivers raw JSON into skills dirs. The kind door
/// still adopts the same folder, and a folder carrying a SKILL.md stays an ordinary skill adopt.
#[test]
fn a_server_bundle_at_a_skill_door_refuses_toward_the_mcp_flag() {
    let rig = Rig::new("miskind");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // The plain add door.
    let err = ops::add(&ctx, &dir).expect_err("refused");
    assert_eq!(err.code(), "KIND_REQUIRED");
    assert!(err.detail().contains("--kind mcp"), "{}", err.detail());
    assert!(err.detail().contains("SKILL.md"), "{}", err.detail());
    assert!(
        !rig.layout().skills_dir().exists(),
        "nothing was adopted: {}",
        err.detail()
    );

    // The publish door's auto-add pre-step refuses the same way, before anything lands.
    let err =
        ops::ensure_tracked(&ctx, None, dir.to_str().unwrap()).expect_err("publish refuses too");
    assert_eq!(err.code(), "KIND_REQUIRED");

    // The `--kind mcp` door still adopts that exact folder.
    ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default())
        .expect("the flagged door adopts");

    // A SKILL.md BESIDE a server.json used to read as a skill — the guard armed only on the
    // absence of SKILL.md, so the folder was adopted silently and the server never landed. Two
    // markers name two kinds, so the door asks instead of guessing (the full shape, and that an
    // explicit word still wins, is `a_folder_holding_both_markers_refuses_and_names_both_kinds`).
    let both = rig.work.0.join("notes");
    std::fs::create_dir_all(&both).unwrap();
    std::fs::write(both.join("SKILL.md"), b"# notes\n").unwrap();
    std::fs::write(both.join("server.json"), b"{}\n").unwrap();
    let err = ops::add(&ctx, &both).expect_err("two markers name two kinds");
    assert_eq!(err.code(), "KIND_REQUIRED");
    assert!(
        err.detail()
            .contains("holds both a SKILL.md and a root server.json"),
        "{}",
        err.detail()
    );
}

/// `--kind skill` on a `server.json`-rooted folder ADOPTS IT AS A SKILL. The guard exists to stop
/// a SILENT mis-kind, not to overrule a person who said which kind they meant: the explicit word
/// wins, the bytes land in skills folders, and the row records no kind (the default IS skill).
#[test]
fn an_explicit_skill_word_adopts_a_server_folder_as_a_skill() {
    let rig = Rig::new("explicit-skill");
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let scope = ops::add_scope(&ctx, true).unwrap();

    // Silence still refuses — the guard's whole reason for standing.
    let err = ops::adopt_path(&ctx, &scope, &dir, ops::KindDeclared::No)
        .expect_err("unflagged, the server folder refuses");
    assert_eq!(err.code(), "KIND_REQUIRED");

    // The person's own word wins.
    let data = ops::adopt_path(&ctx, &scope, &dir, ops::KindDeclared::Yes)
        .expect("`--kind skill` adopts the folder as a skill");
    assert_eq!(data.name, "weather");
    assert!(
        data.mcp.is_none(),
        "a skill adopt carries no server block: {:?}",
        data.mcp
    );
    // The record is a SKILL record — the durable marker says so, so no later sweep re-kinds it.
    let sid = crate::id::SkillId::parse(data.skill_id.as_deref().expect("an adopted id")).unwrap();
    assert_eq!(
        crate::bundle_kind::kind_marker(&rig.fs, &rig.layout(), &sid).as_deref(),
        Some("skill")
    );
}

/// The two fetch-shaped spellings are REFUSED, and the refusal teaches the one covered path in a
/// sentence: the server goes into the workspace on the web, and every machine adds it by name.
/// Nothing is dialed and nothing is written.
#[test]
fn a_registry_name_and_a_document_link_refuse_toward_the_workspace() {
    let rig = Rig::new("no-fetch");
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::add_mcp(&ctx, "io.github.acme/weather", true, &Default::default())
        .expect_err("the registry is not a door");
    let detail = err.detail();
    assert!(detail.contains("registry server name"), "{detail}");
    assert!(detail.contains("does not fetch"), "{detail}");
    assert!(detail.contains("on the web"), "{detail}");

    let err = ops::add_mcp(
        &ctx,
        "https://acme.example/server.json",
        true,
        &Default::default(),
    )
    .expect_err("a link is not a door");
    let detail = err.detail();
    assert!(detail.contains("server document"), "{detail}");
    assert!(detail.contains("does not fetch"), "{detail}");
    assert!(detail.contains("on the web"), "{detail}");

    assert_eq!(rig.global_text(), "[bundles]\n", "nothing was recorded");
}

/// THE KIND-AWARE FILE VERBS. `diff`, `update --reset` and `update --keep-mine` all act on a
/// bundle's PLACED FILES, and a config-placed bundle has none — so all three refuse, in ONE voice,
/// naming what the verb acts on and a command that answers instead.
///
/// What each did before is the reason: `diff` answered an empty diff (which reads as "nothing has
/// changed" over a bundle it never looked at, hand-edited entry and all); `--reset --yes` reported
/// "local edits discarded" one command after its own preview said there were none, and discarded
/// nothing; `--keep-mine` answered in a skill's merge vocabulary.
#[test]
fn the_file_verbs_refuse_over_a_config_placed_bundle() {
    let rig = Rig::new("fileverbs");
    rig.write_global("[bundles]\n");
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    let src = rig.work.0.join("weather");
    write_bundle(&src, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_mcp(&ctx, &src.display().to_string(), true, &Default::default())
        .expect("the server adopts");

    let says_it_all = |e: &crate::error::ClientError, verb: &str| {
        let d = e.detail();
        assert_eq!(e.code(), "INVALID_ARGUMENT", "{d}");
        assert!(d.contains("'weather' is an MCP server bundle"), "{d}");
        assert!(d.contains(verb), "{d}");
        assert!(d.contains("places none"), "{d}");
        // Never a vague line: the refusal names a command that answers.
        assert!(d.contains("`topos list weather`"), "{d}");
    };

    let err = ops::diff(
        &ctx,
        "weather",
        None,
        ops::DiffBudget(None),
        &Default::default(),
        ops::StoreScope::Here,
    )
    .expect_err("diff refuses instead of answering an empty diff");
    says_it_all(&err, "`diff` compares");

    // Bare, and with EITHER targeted selector: the kind is asked ahead of the selection, whose
    // vocabulary is skills folders — `-a cursor` used to resolve the SKILLS dir and refuse about a
    // folder that was never the point.
    for sel in [
        ops::Selection::default(),
        ops::Selection::one(Some("cursor"), None),
        ops::Selection::one(None, Some("~/.cursor/mcp.json")),
    ] {
        for yes in [false, true] {
            let err = ops::reset(
                &ctx,
                &["weather".to_owned()],
                yes,
                ops::StoreScope::Machine,
                &sel,
            )
            .expect_err("reset refuses instead of reporting a discard it did not do");
            says_it_all(&err, "`--reset` discards your edits to");
        }
    }

    let err = pull_keep_mine(&ctx, "weather").expect_err("keep-mine refuses");
    says_it_all(&err, "`--keep-mine` ends a stopped merge over");
}

/// `update <name> --keep-mine` as the app dispatches it.
fn pull_keep_mine(ctx: &Ctx<'_>, name: &str) -> Result<ops::PullOutcome, ClientError> {
    ops::pull(
        ctx,
        ops::PullScope::One {
            name: name.to_owned(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
            store: ops::StoreScope::Machine,
        },
    )
}

/// The same three verbs over an ORDINARY SKILL are untouched — the refusal is keyed on the kind,
/// not bolted onto the verbs.
#[test]
fn the_file_verbs_still_serve_a_skill() {
    let rig = Rig::new("fileverbs-skill");
    rig.write_global("[bundles]\n");
    let src = rig.work.0.join("notes");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# notes\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let scope = ops::add_scope(&ctx, true).unwrap();
    ops::adopt_path(&ctx, &scope, &src, ops::KindDeclared::No).expect("the skill adopts");

    // A clean skill diffs (to nothing) rather than refusing.
    ops::diff(
        &ctx,
        "notes",
        None,
        ops::DiffBudget(None),
        &Default::default(),
        ops::StoreScope::Here,
    )
    .expect("a skill still diffs");
    // And a reset reaches its own two-phase describe, not a kind refusal.
    let out = ops::reset(
        &ctx,
        &["notes".to_owned()],
        false,
        ops::StoreScope::Machine,
        &Default::default(),
    )
    .expect("a skill still resets");
    assert!(
        matches!(out, ops::ResetOutcome::Described { .. }),
        "{out:?}"
    );
}

/// A folder with no root `server.json` is not an MCP bundle — the refusal says exactly that
/// instead of adopting a skill under a flag that promised a server.
#[test]
fn a_folder_without_a_server_json_refuses() {
    let rig = Rig::new("noserver");
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), b"# not a server\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err =
        ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default()).expect_err("refused");
    assert!(err.detail().contains("server.json"), "{}", err.detail());
    assert_eq!(rig.global_text(), "[bundles]\n", "nothing was recorded");
}

/// ITEM PAIR (sibling files, the adopt door): an MCP candidate is EXACTLY `server.json` +
/// `README.md` — a stray script beside the document refuses (naming the allowed set) before
/// anything is adopted, and an allowed README's bytes still run the credential scan. Before the
/// fix the adopt gate read `server.json` alone and let both through.
#[test]
fn a_stray_sibling_refuses_at_the_adopt_gate() {
    let rig = Rig::new("siblings");
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    std::fs::write(dir.join("evil.sh"), b"#!/bin/sh\necho pwned\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err =
        ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default()).expect_err("refused");
    assert_eq!(err.code(), "MCP_INVALID");
    assert!(
        err.detail().contains("server.json, README.md"),
        "{}",
        err.detail()
    );
    // AND IT SAYS WHICH FOLDER. The gate's sentences are shared with the web tier and speak only
    // about a document, so on the CLI the refusal used to name no folder at all — a rule with
    // nothing to apply it to, on a machine that may hold several server bundles.
    assert!(
        err.detail().starts_with(&format!("{}: ", dir.display())),
        "the refusal leads with the folder it judged: {}",
        err.detail()
    );
    assert_eq!(rig.global_text(), "[bundles]\n", "nothing was recorded");

    // A `topos-mcp.toml` is a stray like any other.
    std::fs::remove_file(dir.join("evil.sh")).unwrap();
    std::fs::write(dir.join("topos-mcp.toml"), b"# config\n").unwrap();
    let err =
        ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default()).expect_err("refused");
    assert_eq!(err.code(), "MCP_INVALID");
    assert!(err.detail().contains("topos-mcp.toml"), "{}", err.detail());
    assert_eq!(rig.global_text(), "[bundles]\n", "nothing was recorded");

    // The allowed pair adopts — and the store gains the durable kind marker beside its docs.
    std::fs::remove_file(dir.join("topos-mcp.toml")).unwrap();
    std::fs::write(dir.join("README.md"), b"How to use this server.\n").unwrap();
    let data = ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default())
        .unwrap()
        .data;
    let sid = crate::id::SkillId::parse(data.skill_id.as_deref().unwrap()).unwrap();
    let marker = std::fs::read_to_string(rig.layout().published(&sid).kind).expect("kind.json");
    assert!(marker.contains("\"mcp\""), "{marker}");
}

/// The sibling scan is a CREDENTIAL gate too: a token in the README refuses exactly like one in
/// the document.
#[test]
fn an_allowed_readme_with_a_credential_refuses_at_the_adopt_gate() {
    let rig = Rig::new("readme-secret");
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    std::fs::write(
        dir.join("README.md"),
        format!("Set GITHUB_TOKEN to ghp_{}.\n", "A1b2C3d4E5".repeat(4)),
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err =
        ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default()).expect_err("refused");
    assert_eq!(err.code(), "MCP_SECRET_REFUSED");
    assert!(err.detail().contains("README.md"), "{}", err.detail());
    assert_eq!(rig.global_text(), "[bundles]\n", "nothing was recorded");
}

/// A local document carrying a credential is refused at the same gate, before it is adopted.
#[test]
fn a_local_secret_is_refused_before_the_adopt() {
    let rig = Rig::new("secret-local");
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &server_with_secret());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err =
        ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default()).expect_err("refused");
    assert_eq!(err.code(), "MCP_SECRET_REFUSED");
    assert_eq!(rig.global_text(), "[bundles]\n");
    assert!(
        !rig.layout().skills_dir().exists(),
        "no record was minted for a refused document"
    );
}

/// `--kind mcp` never re-labels a governed reference: a workspace bundle already carries its kind, so
/// the refusal points at the plain `add`.
#[test]
fn a_workspace_reference_refuses_toward_the_plain_add() {
    let rig = Rig::new("wsref");
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::add_mcp(&ctx, "@eng/weather", true, &Default::default()).expect_err("refused");
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    assert!(
        err.detail().contains("topos add @eng/weather"),
        "{}",
        err.detail()
    );
}

// =================================================================================================
// The PUBLISH half — the gate, the WAL, and the wire
// =================================================================================================

/// A `ContributeSource` that records every publish body it is handed and answers OK.
#[derive(Clone, Default)]
struct RecordingPublish {
    seen: Arc<Mutex<Vec<topos_types::requests::PublishRequest>>>,
}
impl crate::plane::ContributeSource for RecordingPublish {
    fn publish(
        &self,
        b: topos_types::requests::PublishRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        use base64::Engine as _;
        self.seen.lock().unwrap().push(b.clone());
        let entries: Vec<ManifestEntry> = b
            .candidate
            .files
            .iter()
            .map(|f| ManifestEntry {
                path: f.path.clone(),
                mode: match f.mode {
                    topos_types::requests::WireFileMode::Regular => FileMode::Regular,
                    topos_types::requests::WireFileMode::Executable => FileMode::Executable,
                },
                content_sha256: digest::sha256(
                    &base64::engine::general_purpose::STANDARD
                        .decode(&f.content_base64)
                        .unwrap(),
                ),
            })
            .collect();
        let tree = digest::bundle_digest(&entries).unwrap();
        let id = topos_core::identity::commit_id(&Commit {
            parents: &[],
            tree,
            author: &b.candidate.author,
            message: &b.candidate.message,
        })
        .unwrap();
        Ok(crate::plane::WriteReceipt {
            receipt: Some(topos_types::Receipt {
                schema_version: 1,
                op_id: b.op_id.clone(),
                command: "publish".to_owned(),
                outcome: topos_types::TerminalOutcome::Ok,
                workspace_id: b.workspace_id.clone(),
                skill_id: Some(b.skill_id.clone()),
                version_id: Some(topos_core::digest::to_hex(&id)),
                bundle_digest: Some(topos_core::digest::to_hex(&tree)),
                expected_generation: Some(b.expected),
                current_generation: Some(1),
                created_at: "2026-08-03T00:00:00.000Z".to_owned(),
                details: None,
            }),
            error: None,
            wire_record: Some(topos_types::WireCurrentRecord {
                schema_version: topos_types::WIRE_SCHEMA_VERSION,
                scope: topos_types::PointerScope {
                    workspace_id: b.workspace_id,
                    skill_id: b.skill_id,
                },
                record: topos_types::CurrentRecord {
                    version_id: topos_core::digest::to_hex(&id),
                    generation: 1,
                },
            }),
        })
    }
    fn propose(
        &self,
        _b: topos_types::requests::ProposeRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("this suite publishes directly")
    }
    fn revert(
        &self,
        _b: topos_types::requests::RevertRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("this suite publishes directly")
    }
    fn review(
        &self,
        _b: topos_types::requests::ReviewRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("this suite publishes directly")
    }
}

/// The session lane's READ transport: the inert plane plus an empty delivery, which is all these
/// publish flows ever ask of it.
#[derive(Debug, Clone, Copy)]
struct NoDelivery;
impl crate::plane::PlaneSource for NoDelivery {
    fn get_current(
        &self,
        _skill_id: &str,
        _known: Option<crate::plane::KnownCurrent>,
    ) -> Result<crate::plane::PointerFetch, crate::plane::PlaneError> {
        Err(crate::plane::PlaneError::NotFound)
    }
    fn fetch_version(
        &self,
        _skill_id: &str,
        _version_id: [u8; 32],
    ) -> Result<crate::plane::FetchedVersion, crate::plane::PlaneError> {
        Err(crate::plane::PlaneError::NotFound)
    }
}
impl crate::plane::DeliverySource for NoDelivery {
    fn fetch_delivery(
        &self,
        _workspace_id: &str,
    ) -> Result<crate::plane::DeliverySnapshot, crate::plane::PlaneError> {
        Err(crate::plane::PlaneError::NotFound)
    }
    fn report_applied(
        &self,
        _workspace_id: &str,
        _applied: &[crate::plane::AppliedSkillReport],
    ) -> Result<(), crate::plane::PlaneError> {
        Ok(())
    }
}

/// Publish `name` through the session lane, capturing the wire bodies.
fn publish_through(
    ctx: &Ctx<'_>,
    recorder: &RecordingPublish,
    name: &str,
) -> Result<ops::PublishOutcome, ClientError> {
    let rec = recorder.clone();
    let session_connect = move |_s: &Session| ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(FakeDirectory::new(Vec::new(), Vec::new())),
        contribute: Box::new(rec.clone()),
        governance: Box::new(NoGovernance),
    };
    ops::publish(
        ctx,
        Some(&session_connect),
        None,
        name,
        false,
        None,
        None,
        None,
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
}

/// The gate stands BEFORE the op WAL: a `kind = "mcp"` bundle whose document carries a credential
/// is refused, no op file is written, and nothing reaches the transport.
#[test]
fn a_secret_bearing_mcp_bundle_never_reaches_the_wal() {
    let rig = Rig::new("pub-secret");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    // Adopt it CLEAN, so the record and the row exist; then the draft gains the credential — the
    // author's own edit, which is exactly the moment the gate has to fire.
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default()).unwrap();
    std::fs::write(dir.join("server.json"), server_with_secret()).unwrap();

    let recorder = RecordingPublish::default();
    let err = publish_through(&ctx, &recorder, "weather").expect_err("refused");
    assert_eq!(err.code(), "MCP_SECRET_REFUSED");
    assert!(
        recorder.seen.lock().unwrap().is_empty(),
        "nothing was sent to the plane"
    );
    let ops_dir = rig.layout().ops_dir();
    let wal: Vec<_> = std::fs::read_dir(&ops_dir)
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(wal.is_empty(), "no op record was written: {wal:?}");
}

/// ITEM PAIR (sibling files, the publish door): the preflight gates the WHOLE candidate — a stray
/// script a draft gained beside `server.json` refuses BEFORE the op WAL, naming the allowed set.
/// Before the fix the preflight validated `server.json` alone and shipped the stray file.
#[test]
fn a_stray_sibling_never_reaches_the_wal() {
    let rig = Rig::new("pub-siblings");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    // Adopt it CLEAN; then the draft gains the stray file — the author's own edit, exactly where
    // the gate has to fire.
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default()).unwrap();
    std::fs::write(dir.join("evil.sh"), b"#!/bin/sh\necho pwned\n").unwrap();

    let recorder = RecordingPublish::default();
    let err = publish_through(&ctx, &recorder, "weather").expect_err("refused");
    assert_eq!(err.code(), "MCP_INVALID");
    assert!(err.detail().contains("evil.sh"), "{}", err.detail());
    assert!(
        recorder.seen.lock().unwrap().is_empty(),
        "nothing was sent to the plane"
    );
    let ops_dir = rig.layout().ops_dir();
    let wal: Vec<_> = std::fs::read_dir(&ops_dir)
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(wal.is_empty(), "no op record was written: {wal:?}");
}

/// A [`RecordingPublish`] whose protocol card declares an OLD server — one that predates MCP
/// bundle kinds and would silently record a SKILL.
#[derive(Clone, Default)]
struct OldServerPublish {
    inner: RecordingPublish,
}
impl crate::plane::ContributeSource for OldServerPublish {
    fn publish(
        &self,
        b: topos_types::requests::PublishRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        self.inner.publish(b)
    }
    fn propose(
        &self,
        b: topos_types::requests::ProposeRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        self.inner.propose(b)
    }
    fn revert(
        &self,
        b: topos_types::requests::RevertRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        self.inner.revert(b)
    }
    fn review(
        &self,
        b: topos_types::requests::ReviewRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        self.inner.review(b)
    }
    fn protocol_card(&self) -> Option<topos_types::requests::WireProtocolCard> {
        Some(topos_types::requests::WireProtocolCard {
            schema_version: 1,
            card: "topos-protocol-card".to_owned(),
            api_base_url: format!("https://{HOST}/api"),
            server_version: Some("0.1.9".to_owned()),
        })
    }
}

/// ITEM PAIR (pre-MCP server): publishing a `kind = "mcp"` bundle to a server whose card predates
/// MCP support refuses with the typed server-too-old shape BEFORE the op WAL — nothing is minted,
/// nothing reaches the wire — instead of the server silently recording a SKILL while the client
/// receipt claims an mcp bundle landed.
#[test]
fn an_mcp_publish_to_a_pre_mcp_server_refuses_before_the_wal() {
    let rig = Rig::new("pub-oldsrv");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default()).unwrap();

    let recorder = OldServerPublish::default();
    let rec = recorder.clone();
    let session_connect = move |_s: &Session| ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(FakeDirectory::new(Vec::new(), Vec::new())),
        contribute: Box::new(rec.clone()),
        governance: Box::new(NoGovernance),
    };
    let err = ops::publish(
        &ctx,
        Some(&session_connect),
        None,
        "weather",
        false,
        None,
        None,
        None,
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .expect_err("a pre-MCP server refuses the mcp publish");
    assert_eq!(err.code(), "SERVER_TOO_OLD");
    assert!(err.detail().contains("0.1.9"), "{}", err.detail());
    assert!(err.detail().contains("MCP"), "{}", err.detail());
    assert!(
        recorder.inner.seen.lock().unwrap().is_empty(),
        "nothing was sent to the plane"
    );
    let ops_dir = rig.layout().ops_dir();
    let wal: Vec<_> = std::fs::read_dir(&ops_dir)
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(wal.is_empty(), "no op record was written: {wal:?}");
}

/// A clean mcp bundle publishes with its KIND: the op record carries it, the wire body carries it,
/// and the receipt names it — so the workspace records what the bundle IS.
#[test]
fn the_bundle_kind_rides_the_wal_onto_the_wire_and_the_receipt() {
    let rig = Rig::new("pub-kind");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default()).unwrap();

    let recorder = RecordingPublish::default();
    let outcome = publish_through(&ctx, &recorder, "weather").unwrap();
    let ops::PublishOutcome::Published(data) = outcome else {
        panic!("the publish LANDED");
    };
    assert_eq!(
        data.kind.as_deref(),
        Some("mcp"),
        "the receipt names the kind"
    );
    let sent = recorder.seen.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].kind.as_deref(),
        Some("mcp"),
        "the wire carries the kind"
    );

    // The governance transfer rewrote the local path row to the workspace reference — and the
    // workspace row carries NO kind field: from here the catalog is the authority.
    let text = rig.global_text();
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}/weather\" = \"*\"")),
        "{text}"
    );
    assert!(
        !text.contains("kind"),
        "the local kind field is gone: {text}"
    );
    assert!(
        !text.contains(&dir.display().to_string()),
        "the path row is gone: {text}"
    );
}

/// An ordinary SKILL publish is untouched by any of this: no kind on the wire, no kind on the
/// receipt — an absent tag reads as `"skill"` by the request's own default.
#[test]
fn an_ordinary_skill_publish_carries_no_kind() {
    let rig = Rig::new("pub-skill");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("deploy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), b"# deploy\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let mut data = ops::add(&ctx, &dir).unwrap();
    let scope = crate::ops::add_scope(&ctx, true).unwrap();
    crate::ops::note_added_path_in(&ctx, &mut data, &scope.target, &dir).unwrap();

    let recorder = RecordingPublish::default();
    let outcome = publish_through(&ctx, &recorder, "deploy").unwrap();
    let ops::PublishOutcome::Published(pd) = outcome else {
        panic!("the publish LANDED");
    };
    assert_eq!(pd.kind, None);
    assert_eq!(recorder.seen.lock().unwrap()[0].kind, None);
}

/// `-a` on `add --kind mcp` narrows the row to the named agents' config FILES, and `remove <name> -a`
/// SUBTRACTS one file: its entry leaves that config in the same invocation, the other agent's
/// entry stays, and the row keeps the rest — receipts counting config files.
#[test]
fn mcp_selection_narrows_add_and_remove_by_config_file() {
    let rig = Rig::new("mcp-dest");
    rig.write_global("[bundles]\n");
    // Two MCP-capable agents set up in the fake home.
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    std::fs::create_dir_all(rig.home.0.join(".codex")).unwrap();
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let sel = ops::Selection {
        agents: vec!["cursor".to_owned(), "codex".to_owned()],
        dests: Vec::new(),
    };
    let data = ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &sel)
        .unwrap()
        .data;
    assert_eq!(
        data.dest,
        vec![
            "~/.cursor/mcp.json".to_owned(),
            "~/.codex/config.toml".to_owned()
        ]
    );
    let text = rig.global_text();
    assert!(
        text.contains(r#"dest = ["~/.cursor/mcp.json", "~/.codex/config.toml"]"#),
        "{text}"
    );
    let cursor = rig.home.0.join(".cursor/mcp.json");
    let codex = rig.home.0.join(".codex/config.toml");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("weather.acme.example"),
        "cursor's file got the entry"
    );
    assert!(
        std::fs::read_to_string(&codex)
            .unwrap()
            .contains("weather.acme.example"),
        "codex's file got the entry"
    );

    // Narrow remove: subtract codex's config file — its entry leaves NOW, cursor's stays, and
    // the row keeps the remaining file.
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(FakeDirectory::new(Vec::new(), Vec::new())),
        contribute: Box::new(RecordingPublish::default()),
        governance: Box::new(NoGovernance),
    };
    let narrow = ops::Selection {
        agents: vec!["codex".to_owned()],
        dests: Vec::new(),
    };
    let outcome = ops::remove_global(
        &ctx,
        &session_connect,
        &["weather".to_owned()],
        None,
        false,
        &narrow,
    )
    .unwrap();
    let ops::RemoveOutcome::Applied(removed) = outcome else {
        panic!("a narrow applies immediately");
    };
    let text = rig.global_text();
    assert!(
        text.contains(r#"dest = ["~/.cursor/mcp.json"]"#),
        "the row keeps the remaining file: {text}"
    );
    assert!(
        std::fs::read_to_string(&codex)
            .map(|t| !t.contains("weather.acme.example"))
            .unwrap_or(true),
        "the codex entry left with the subtraction"
    );
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("weather.acme.example"),
        "cursor's entry stays"
    );
    // The receipt counts config files and names what remains.
    let u = &removed.uninstalled[0];
    assert_eq!(u.destinations, vec!["~/.codex/config.toml".to_owned()]);
    assert_eq!(u.kind.as_deref(), Some("mcp"));
    assert_eq!(u.remaining, Some(1));
    let tty = crate::render::remove_applied_tty(&removed);
    assert!(
        tty.contains("removed (~/.codex/config.toml) — 1 config file remains"),
        "{tty}"
    );
}

// =================================================================================================
// A record's KIND is fixed at adoption — the re-link door cannot change it
// =================================================================================================

/// THE GATE BYPASS THIS CLOSES. `remove <path>` keeps the bytes and the record, so the folder can
/// be edited and named to `add` again — and the re-link arm hands back the SAME record. If that
/// record's durable marker still said `skill` while the new row said `mcp`, every later reader
/// would trust the marker: `publish` would skip the MCP gate entirely and ship the server document
/// — credentials and all — as a skill. So a standing marker that disagrees with the requested kind
/// is a refusal that names both kinds and the command that frees the folder.
#[test]
fn a_skill_record_cannot_be_re_linked_as_an_mcp_bundle() {
    let rig = Rig::new("relink-skill-to-mcp");
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), b"# weather\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // Adopted as a SKILL; the row is then dropped by hand, which is what leaves the record
    // unclaimed and the folder re-addable.
    ops::add(&ctx, &dir).expect("the skill adopts");
    rig.write_global("[bundles]\n");

    // The folder is converted into a perfectly valid MCP bundle.
    std::fs::remove_file(dir.join("SKILL.md")).unwrap();
    write_bundle(&dir, &good_server());

    let err = ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default())
        .expect_err("a record's kind is fixed at adoption");
    let detail = err.detail();
    assert!(
        detail.contains("already tracked here as a skill bundle"),
        "{detail}"
    );
    assert!(detail.contains("mcp bundle"), "{detail}");
    assert!(detail.contains("topos remove weather"), "{detail}");
    assert_eq!(rig.global_text(), "[bundles]\n", "nothing was recorded");

    // And the marker still says what the record IS — never overwritten, never left to disagree.
    let sid = crate::id::SkillId::parse(
        &crate::ops::tracked_skill_at(&ctx, &dir.canonicalize().unwrap())
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let marker = std::fs::read_to_string(rig.layout().published(&sid).kind).unwrap();
    assert!(marker.contains("\"skill\""), "{marker}");
}

/// THE MIRROR. An MCP record whose folder is turned back into a skill refuses the same way — the
/// rule is about the record, not about which kind is "bigger".
#[test]
fn an_mcp_record_cannot_be_re_linked_as_a_skill() {
    let rig = Rig::new("relink-mcp-to-skill");
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default())
        .expect("the server adopts");
    rig.write_global("[bundles]\n");

    // Now it is a skill on disk — so the unflagged-mcp guard passes and the re-link door is next.
    std::fs::remove_file(dir.join("server.json")).unwrap();
    std::fs::write(dir.join("SKILL.md"), b"# weather\n").unwrap();

    let scope = ops::add_scope(&ctx, true).unwrap();
    let err = ops::adopt_path(&ctx, &scope, &dir, ops::KindDeclared::No)
        .expect_err("the record is an mcp one");
    let detail = err.detail();
    assert!(
        detail.contains("already tracked here as a mcp bundle"),
        "{detail}"
    );
    assert!(detail.contains("skill bundle"), "{detail}");
    assert_eq!(rig.global_text(), "[bundles]\n", "nothing was recorded");
}

/// A `--kind` WORD THAT CONTRADICTS THE CATALOG IS REFUSED. The local-folder door already refuses
/// a server bundle named without the word — but the workspace door read the word and threw it
/// away, so `add --kind skill <a workspace's MCP bundle>` delivered a tool endpoint into the
/// person's agents without a syllable about it. The catalog is the authority on what a bundle is,
/// which is exactly why the flag can only agree with it or be wrong.
#[test]
fn a_kind_word_contradicting_the_catalog_refuses_at_the_workspace_door() {
    use super::manifest_reconcile::{catalog_entry, mk_version};
    let rig = Rig::new("kind-contradict");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let v = mk_version(&[("server.json", FileMode::Regular, good_server().as_bytes())]);
    let mut entry = catalog_entry("s_linear", "linear", &v);
    entry.kind = "mcp".into();
    let fdir = FakeDirectory::new(vec![entry], Vec::new());
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(NoDelivery),
        directory: Box::new(fdir.clone()),
        contribute: Box::new(RecordingPublish::default()),
        governance: Box::new(NoGovernance),
    };

    let add = |declared: Option<crate::bundle_kind::BundleKind>| {
        ops::add_reference(
            &ctx,
            &session_connect,
            None,
            &format!("{HOST}/{WS_NAME}/linear"),
            true,
            true,
            &Default::default(),
            declared,
        )
    };

    // The contradiction refuses — naming both kinds, and the flag as the thing to drop.
    let err = add(Some(crate::bundle_kind::BundleKind::Skill))
        .expect_err("`--kind skill` on a catalog MCP bundle refuses");
    let msg = crate::render::safe_message(&err);
    assert!(
        msg.contains("is an MCP server in the catalog, not a skill")
            && msg.contains("needs no `--kind` at all"),
        "{msg}"
    );
    assert_eq!(
        rig.global_text(),
        "[bundles]\n",
        "nothing was written: {msg}"
    );

    // The word that AGREES passes this gate — the flag is redundant here, never wrong. (What the
    // add then does with it is the ordinary subscribe path, covered elsewhere.)
    let agreed = add(Some(crate::bundle_kind::BundleKind::Mcp));
    let agreed_msg = agreed
        .err()
        .map(|e| crate::render::safe_message(&e))
        .unwrap_or_default();
    assert!(
        !agreed_msg.contains("in the catalog, not"),
        "the word that matches the catalog is never the thing refused: {agreed_msg}"
    );
}

/// A FOLDER THAT IS BOTH. The server-folder guard armed only when there was NO `SKILL.md` — so a
/// folder holding both markers sailed past it, was adopted as a skill, and the server never
/// landed: half of what the person pointed at, with nothing said about the other half. Nothing in
/// the bytes says which kind it is, so the door refuses and names both words rather than picking
/// one. An explicit `--kind` still wins, because the guard exists to stop a SILENT mis-kind.
#[test]
fn a_folder_holding_both_markers_refuses_and_names_both_kinds() {
    let rig = Rig::new("both-markers");
    rig.write_global("[bundles]\n");
    let dir = rig.work.0.join("weather");
    write_bundle(&dir, &good_server());
    std::fs::write(dir.join("SKILL.md"), b"# weather\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::add(&ctx, &dir).expect_err("neither kind is guessable");
    assert_eq!(err.code(), "KIND_REQUIRED");
    let detail = err.detail();
    assert!(
        detail.contains("holds both a SKILL.md and a root server.json")
            && detail.contains("--kind skill")
            && detail.contains("--kind mcp"),
        "the refusal names both real answers: {detail}"
    );
    assert_eq!(rig.global_text(), "[bundles]\n", "nothing was recorded");
    // The machine channel offers both, in the same order the sentence does.
    let actions = crate::render::next_actions("add", &[], &err);
    let kinds: Vec<&str> = actions
        .iter()
        .map(|a| {
            let i = a
                .argv
                .iter()
                .position(|w| w == "--kind")
                .expect("each offered command names a kind");
            a.argv[i + 1].as_str()
        })
        .collect();
    assert_eq!(kinds, vec!["skill", "mcp"], "{actions:?}");

    // The declared word still wins — silence was the only thing being guarded.
    let scope = ops::add_scope(&ctx, true).unwrap();
    ops::adopt_path(&ctx, &scope, &dir, ops::KindDeclared::Yes)
        .expect("an explicit kind adopts the folder");
}

// =================================================================================================
// The TEARDOWN — `uninstall` and the config entries only the sidecar's ledger can account for
// =================================================================================================

/// An MCP entry topos placed is LIVE WIRING: it points a running agent at a server. The ledger
/// that proves which entries are topos's lives in `~/.topos/`, so an uninstall that deleted the
/// tree and left the entries left them unaccountable forever — no later run could tell them from
/// a hand edit. The teardown now retires them first, through the SAME owned-entry mechanics
/// `remove` uses: a clean entry goes, a hand-edited one stays byte-identical, and BOTH facts are
/// named in the preview before anything happens.
#[test]
fn the_teardown_retires_its_own_mcp_entries_and_leaves_a_hand_edited_one() {
    let rig = Rig::new("teardown-mcp");
    rig.write_global("[bundles]\n");
    // Two USER-scope MCP surfaces, both detected off the fake home.
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    std::fs::create_dir_all(rig.home.0.join(".codex")).unwrap();
    // A line of the person's own, so the file is not WHOLLY topos-owned: the last-entry-deletion
    // rule then keeps it, and the assertion is about the ENTRY rather than the file.
    std::fs::write(rig.home.0.join(".codex/config.toml"), b"model = \"o3\"\n").unwrap();
    let src = rig.work.0.join("weather");
    write_bundle(&src, &good_server());
    let ctx = rig.ctx_at(None);
    ops::add_mcp(&ctx, &src.display().to_string(), true, &Default::default()).unwrap();

    let cursor = rig.home.0.join(".cursor/mcp.json");
    let codex = rig.home.0.join(".codex/config.toml");
    for f in [&cursor, &codex] {
        assert!(
            std::fs::read_to_string(f)
                .unwrap()
                .contains("https://weather.acme.example/mcp"),
            "the add placed an entry in {}",
            f.display()
        );
    }
    // HAND-EDIT the cursor entry: its bytes no longer fingerprint to what topos wrote, so it is
    // the person's now — a teardown may not take it.
    let drifted = std::fs::read_to_string(&cursor)
        .unwrap()
        .replace("weather.acme.example", "weather.mine.example");
    std::fs::write(&cursor, &drifted).unwrap();

    // The PREVIEW names every file it will open, and says which entries it will leave.
    let described = ops::uninstall(&ctx, None, false).unwrap();
    let ops::UninstallOutcome::Described { describe, yes_argv } = described else {
        panic!("a bare uninstall describes")
    };
    assert_eq!(
        describe.mcp_files,
        vec![codex.to_string_lossy().into_owned()],
        "the clean surface is named for removal"
    );
    assert_eq!(
        describe.mcp_drifted,
        vec![cursor.to_string_lossy().into_owned()],
        "the hand-edited surface is named as one it will leave"
    );
    let preview = crate::render::uninstall_describe_tty(&describe, &yes_argv);
    assert!(
        preview.contains(&format!(
            "  · remove topos-placed MCP server entries from {}",
            codex.display()
        )),
        "{preview}"
    );
    assert!(
        preview.contains(&format!(
            "  · leave the hand-edited entries in {} in place",
            cursor.display()
        )),
        "{preview}"
    );
    // Nothing has changed yet.
    assert_eq!(std::fs::read_to_string(&cursor).unwrap(), drifted);

    let applied = ops::uninstall(&ctx, None, true).unwrap();
    let ops::UninstallOutcome::Applied { applied, messages } = applied else {
        panic!("--yes applies")
    };
    assert!(messages.is_empty(), "{messages:?}");
    assert!(!rig.layout().home().exists(), "the sidecar tree is gone");
    let codex_after = std::fs::read_to_string(&codex).unwrap();
    assert!(
        !codex_after.contains("weather.acme.example"),
        "the entry topos owned is gone from the clean surface: {codex_after}"
    );
    assert!(
        codex_after.contains("model = \"o3\""),
        "every byte that was not topos's is still there: {codex_after}"
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        drifted,
        "the hand-edited entry is byte-identical — a teardown clobbers nothing"
    );
    assert_eq!(
        applied.mcp_files,
        vec![codex.to_string_lossy().into_owned()]
    );
    assert_eq!(
        applied.mcp_drifted,
        vec![cursor.to_string_lossy().into_owned()]
    );
}

// =================================================================================================
// The add's DELIVERY half — what an inline converge that could not write says, and exits
// =================================================================================================

/// An add whose converge reached NO agent is not a success with a note about it: the row landed
/// and nothing was delivered. It used to print `ok: true`, exit 0, carry an empty `warnings`, and
/// bury the cause in one prose blob that repeated it once per surface — and mislabelled it, saying
/// a config file "could not be read" when the config read perfectly well and topos's OWN ledger
/// was the thing it could not write.
#[test]
fn an_add_that_reached_no_agent_says_so_once_and_fails() {
    let rig = Rig::new("ledger-locked");
    rig.write_global("[bundles]\n");
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    std::fs::create_dir_all(rig.home.0.join(".codex")).unwrap();
    std::fs::write(rig.home.0.join(".codex/config.toml"), b"model = \"o3\"\n").unwrap();
    let ctx = rig.ctx_at(None);
    // One clean add first, so the scope's state dir exists to be locked down.
    let first = rig.work.0.join("weather");
    write_bundle(&first, &good_server());
    ops::add_mcp(
        &ctx,
        &first.display().to_string(),
        true,
        &Default::default(),
    )
    .unwrap();

    // The ownership ledger's directory goes read-only: topos can still READ its record and every
    // agent config, and can write neither the journal nor the record.
    let state = rig.layout().state_dir();
    let restore = std::fs::metadata(&state).unwrap().permissions();
    let mut locked = restore.clone();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        locked.set_mode(0o500);
    }
    std::fs::set_permissions(&state, locked).unwrap();

    let second = rig.work.0.join("tides");
    write_bundle(&second, &good_server().replace("weather", "tides"));
    let added = ops::add_mcp(
        &ctx,
        &second.display().to_string(),
        true,
        &Default::default(),
    )
    .expect("the ROW still lands — the demand is durable whatever the configs did");
    std::fs::set_permissions(&state, restore).unwrap();

    // Every failed surface rides the typed channel, one message each.
    assert!(
        !added.messages.is_empty(),
        "the converge failures are not silent"
    );
    for m in &added.messages {
        assert_eq!(m.kind, topos_types::MessageKind::Failure, "{m:?}");
    }
    assert!(added.reached_nobody, "nothing was placed anywhere");
    // The per-surface line names the CONFIG FILE and the real fault — topos's own record, not a
    // file it read without trouble.
    let cursor = rig.home.0.join(".cursor/mcp.json");
    let surface = added
        .messages
        .iter()
        .find(|m| m.text.starts_with(&format!("{}:", cursor.display())))
        .unwrap_or_else(|| panic!("a line for {}: {:?}", cursor.display(), added.messages));
    assert!(
        surface
            .text
            .contains("topos could not save its record of the entries it owns there ("),
        "{}",
        surface.text
    );
    assert!(
        surface
            .text
            .ends_with("), so it wrote nothing to that file."),
        "{}",
        surface.text
    );
    for m in &added.messages {
        assert!(
            !m.text.contains("could not be read"),
            "the config read fine — the ledger is the fault: {}",
            m.text
        );
    }
    // The receipt states BOTH facts and the working remedy — and the cause exactly ONCE, however
    // many surfaces met it.
    let note = added.data.note.clone().unwrap_or_default();
    assert!(
        note.contains("\"tides\" was recorded, and it reached no agent — "),
        "{note}"
    );
    assert!(
        note.ends_with(". Run 'topos update' to finish the delivery."),
        "{note}"
    );
    assert_eq!(
        note.matches("topos could not save its record of the entries it owns")
            .count(),
        1,
        "the shared cause is stated once, not once per surface: {note}"
    );
}
