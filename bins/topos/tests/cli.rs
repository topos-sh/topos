//! End-to-end coverage of the binary composition root the in-crate unit tests can't reach: real argv
//! parsing (clap), `TOPOS_HOME` resolution, the recover + first-use identity startup, and the `--json`
//! envelope on stdout.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_topos")
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pr-describe")
}

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("topos-cli-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Canonical from birth: `$TMPDIR` sits behind the macOS `/var` symlink, and the binary answers
    // in resolved paths — a fixture spelled the other way would compare unequal to its own dirs.
    dir.canonicalize().unwrap()
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

/// The WORKING DIRECTORY every run stands in — a dedicated empty dir beside the sidecar, created
/// on demand. The scope rules read the cwd (the nearest `topos.toml` at or above it), so a test
/// must never inherit the crate's own folder: what a project `add` writes would depend on the
/// checkout it ran in. A test that means to act on a project scope mints the file here with
/// `topos init`, exactly as a person would.
fn work_dir(home: &Path) -> PathBuf {
    let name = home.file_name().and_then(|n| n.to_str()).unwrap_or("run");
    let dir = home.with_file_name(format!("{name}-cwd"));
    std::fs::create_dir_all(&dir).expect("the run's working directory");
    dir
}

/// The agents pick, seeded ONCE per `$TOPOS_HOME` as every agent installed here (the hermetic
/// homes decide what is installed) — a run that never picked would place nothing. BOTH scopes,
/// because neither reads the other's file: the machine's under `$TOPOS_HOME`, and the working
/// directory's own, so a run that stands in a project scope has a pick there too. A test about
/// the pick itself writes the file first; the seed never overwrites one.
fn seed_pick(topos_home: &Path) {
    const PICK: &[u8] = b"{\n  \"schema_version\": 1,\n  \"agents\": [\n    \"*\"\n  ]\n}\n";
    let machine = topos_home.join("agents.json");
    if !machine.exists() {
        std::fs::create_dir_all(topos_home).expect("the sidecar home");
        std::fs::write(&machine, PICK).expect("the agents pick");
    }
    let store = work_dir(topos_home).join(".topos");
    let project = store.join("agents.json");
    if !project.exists() {
        std::fs::create_dir_all(&store).expect("the project store");
        std::fs::write(&project, PICK).expect("the project's agents pick");
    }
}

fn run(home: &Path, args: &[&str]) -> (bool, serde_json::Value) {
    // Hermetic: point the Claude config home at an isolated (empty) dir so a test never reads or writes
    // the real `~/.claude`.
    run_in(home, &home.join(".claude-isolated"), args)
}

fn run_in(home: &Path, claude: &Path, args: &[&str]) -> (bool, serde_json::Value) {
    seed_pick(home);
    let out = Command::new(bin())
        .env("TOPOS_HOME", home)
        .env("CLAUDE_CONFIG_DIR", claude)
        .env_remove("CLAUDECODE")
        .current_dir(work_dir(home))
        .args(args)
        .output()
        .expect("spawn topos");
    let value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("non-JSON stdout: {}", String::from_utf8_lossy(&out.stdout)));
    (out.status.success(), value)
}

/// The raw process runner (exit code + both streams), for the surface tests below. `debug` sets
/// `TOPOS_DEBUG=1`; it is scrubbed otherwise so an ambient value can't skew an assertion.
fn run_raw(home: &Path, args: &[&str], debug: bool) -> std::process::Output {
    seed_pick(home);
    let mut cmd = Command::new(bin());
    cmd.env("TOPOS_HOME", home)
        .env("CLAUDE_CONFIG_DIR", home.join(".claude-isolated"))
        .env_remove("CLAUDECODE")
        .current_dir(work_dir(home))
        .args(args);
    if debug {
        cmd.env("TOPOS_DEBUG", "1");
    } else {
        cmd.env_remove("TOPOS_DEBUG");
    }
    cmd.output().expect("spawn topos")
}

#[test]
fn end_to_end_add_then_list_over_json() {
    let home = scratch("home");
    let src = scratch("src");
    let skill = src.join("pr-describe");
    copy_tree(&fixture(), &skill);

    // add — drives clap, TOPOS_HOME, the recover+identity startup, and the success envelope.
    // MACHINE-WIDE (`-g`): the source sits outside any checkout, and `list` inventories this
    // machine's own store, so the scope the assertions below are about is the machine's.
    let (ok, v) = run(&home, &["--json", "add", "-g", skill.to_str().unwrap()]);
    assert!(ok, "add should exit 0");
    assert_eq!(v["command"], "add");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["name"], "pr-describe");
    assert_eq!(v["data"]["tracked"], true);
    let version = v["data"]["version_id"]
        .as_str()
        .expect("version_id")
        .to_owned();
    assert_eq!(version.len(), 64);
    assert!(
        home.join("identity/host.json").exists(),
        "first use minted the device identity"
    );

    // list — the machine scope (the here-scope out here) carries the adopted row via the same
    // wiring: the manifest line `add -g` recorded resolves against the machine store.
    let (ok, v) = run(&home, &["--json", "list"]);
    assert!(ok);
    let scopes = v["data"]["scopes"].as_array().expect("scopes array");
    let machine = scopes
        .iter()
        .find(|s| s["scope"] == "machine")
        .expect("the machine scope");
    let rows = machine["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["skill"], "pr-describe");
    assert_eq!(rows[0]["version_id"], version);

    // An unknown deep-dive name answers the NOT-MANAGED success (exit 0, `managed: false`) —
    // "nothing manages it" is the whole answer, never an error.
    let (ok, v) = run(&home, &["--json", "list", "nope"]);
    assert!(ok, "an unknown name answers not-managed, exit 0");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["detail"]["name"], "nope");
    assert_eq!(v["data"]["detail"]["managed"], false);

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn json_envelope_apply_receipt_on_ungated_arms_describe_on_gated() {
    // The reduced consent gate's `--json` compat flip, on the REAL binary: a bare run of an
    // UNGATED arm answers the APPLY receipt (with the undo-led next action), while a still-gated
    // arm keeps the describe envelope (with the `--yes` apply next action).
    let home = scratch("yes-scope");
    let src = scratch("yes-scope-src");
    let skill = src.join("pr-describe");
    copy_tree(&fixture(), &skill);
    let (ok, v) = run(&home, &["--json", "add", "-g", skill.to_str().unwrap()]);
    assert!(ok, "add should exit 0");
    // The add recorded a manifest line (an out-of-tree path ref, asked for machine-wide, lands in
    // the personal manifest) and disclosed its inverse — `remove -g <path>` — in the receipt.
    let undo: Vec<String> = v["data"]["undo"]
        .as_array()
        .expect("the add receipt carries its undo argv")
        .iter()
        .map(|t| t.as_str().expect("argv token").to_owned())
        .collect();
    assert_eq!(undo[..2], ["topos".to_owned(), "remove".to_owned()]);
    let path_token = undo
        .last()
        .expect("the row reference ends the argv")
        .clone();

    // UNGATED (the manifest-line remove): dropping the line is immediate and reversible — the
    // bare run APPLIES with the receipt document (not a `describe` wrapper), `bytes_kept` (the
    // tracked bytes stay in the sidecar), and the undo-led next action (`add` the row back).
    // The receipt's own undo argv IS the invocation (an out-of-tree path asked for machine-wide
    // records in that file, so the inverse carries `-g`).
    let mut remove_argv: Vec<&str> = vec!["--json"];
    remove_argv.extend(undo.iter().skip(1).map(String::as_str));
    let (ok, v) = run(&home, &remove_argv);
    assert!(ok, "the manifest-line remove applies: {v}");
    assert_eq!(v["command"], "remove");
    assert!(v["data"].get("describe").is_none(), "an apply receipt: {v}");
    assert_eq!(v["data"]["applied"], true);
    assert_eq!(v["data"]["items"][0]["kind"], "manifest-removed");
    assert_eq!(v["data"]["items"][0]["bytes_kept"], true);
    assert_eq!(v["next_actions"][0]["code"], "UNDO");
    let undo_back: Vec<&str> = v["next_actions"][0]["argv"]
        .as_array()
        .expect("the undo argv")
        .iter()
        .map(|t| t.as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        &undo_back[..2],
        ["topos", "add"],
        "the inverse is an add: {v}"
    );
    assert_eq!(
        undo_back.last(),
        Some(&path_token.as_str()),
        "the inverse re-adds the same row: {v}"
    );

    // The row remove's eager reconcile settled the leftover record too, but the ADOPTED source
    // dir is the person's own and never leaves with it.
    assert!(
        skill.join("SKILL.md").exists(),
        "an adopted source dir is never deleted by a row remove"
    );

    // GATED (the built-in's durable opt-out — a permanent, whole-machine act): the bare run
    // answers the DESCRIBE envelope, `applied: false`, the `--yes` apply next action; nothing is
    // deleted.
    let (ok, v) = run(&home, &["--json", "remove", "topos"]);
    assert!(ok, "the gated describe exits 0: {v}");
    assert_eq!(v["command"], "remove");
    assert_eq!(v["data"]["describe"]["applied"], false);
    // The built-in's own kind, not the local-delete one it used to borrow: its bytes ship inside
    // the binary and `topos add topos` places them again, so calling this a permanent delete was
    // the receipt telling a person their only copy was going.
    assert_eq!(v["data"]["describe"]["items"][0]["kind"], "builtin-opt-out");
    assert_eq!(v["next_actions"][0]["code"], "APPLY_DESCRIBED");
    let argv = v["next_actions"][0]["argv"].as_array().expect("argv");
    assert_eq!(argv.last().and_then(|t| t.as_str()), Some("--yes"));

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn the_undo_a_row_removal_prints_runs_and_re_links_the_record_it_kept() {
    // The receipt above stops at the argv; this runs it. `remove <path>` keeps the bytes, so the
    // store record outlives the row that asked for it — and the undo the receipt prints has to be
    // a command that WORKS against exactly that state, not one the already-tracked refusal meets.
    let home = scratch("relink");
    let src = scratch("relink-src");
    let skill = src.join("pr-describe");
    copy_tree(&fixture(), &skill);

    let (ok, v) = run(&home, &["--json", "add", "-g", skill.to_str().unwrap()]);
    assert!(ok, "add should exit 0: {v}");
    let skill_id = v["data"]["skill_id"].as_str().expect("skill id").to_owned();
    let version = v["data"]["version_id"]
        .as_str()
        .expect("version")
        .to_owned();
    let manifest = PathBuf::from(v["data"]["manifest"].as_str().expect("the manifest file"));
    let manifest_before = std::fs::read_to_string(&manifest).expect("the manifest text");
    let records = |home: &Path| {
        std::fs::read_dir(home.join("skills"))
            .map(|d| d.count())
            .unwrap_or(0)
    };
    let records_before = records(&home);
    // The add's own inverse, taken verbatim from its receipt (the row key is the canonical path,
    // which is not always the path that was typed).
    let remove_argv: Vec<String> = v["data"]["undo"]
        .as_array()
        .expect("the add receipt carries its undo argv")
        .iter()
        .map(|t| t.as_str().expect("argv token").to_owned())
        .collect();

    // Drop the row; take the undo argv IT prints, verbatim.
    let mut argv: Vec<&str> = vec!["--json"];
    argv.extend(remove_argv.iter().skip(1).map(String::as_str));
    let (ok, v) = run(&home, &argv);
    assert!(ok, "the row removal applies: {v}");
    assert_eq!(v["data"]["items"][0]["bytes_kept"], true);
    assert_eq!(v["next_actions"][0]["code"], "UNDO");
    let undo: Vec<String> = v["next_actions"][0]["argv"]
        .as_array()
        .expect("the undo argv")
        .iter()
        .map(|t| t.as_str().expect("argv token").to_owned())
        .collect();

    // Run it exactly as printed (the binary name off the front, `--json` on for the assertions).
    let mut argv: Vec<&str> = vec!["--json"];
    argv.extend(undo.iter().skip(1).map(String::as_str));
    let (ok, v) = run(&home, &argv);
    assert!(ok, "the printed undo must run: {v}");
    assert_eq!(v["command"], "add");
    assert_eq!(v["data"]["skill_id"], skill_id, "the same record answers");
    assert_eq!(v["data"]["version_id"], version, "at the version it left");
    assert_eq!(v["data"]["tracked"], true);

    // The row is back exactly as it was, and nothing was minted to put it there.
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        manifest_before,
        "the manifest is byte-identical to its pre-remove state"
    );
    assert_eq!(
        records(&home),
        records_before,
        "no second record was minted"
    );

    // …and the removal works again — the pair is still an exact inverse.
    let mut argv: Vec<&str> = vec!["--json"];
    argv.extend(remove_argv.iter().skip(1).map(String::as_str));
    let (ok, v) = run(&home, &argv);
    assert!(ok, "the row removal applies again: {v}");
    assert_eq!(v["data"]["applied"], true);
    assert!(
        !std::fs::read_to_string(&manifest)
            .unwrap()
            .contains("pr-describe"),
        "the row is gone again"
    );

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&home);
}

/// **An add writes no hook and places nothing.** Adopting a skill from Claude Code's own skills
/// dir records the row and nothing else: no `settings.json` entry, no built-in skill placed
/// beside it — both follow the agents pick, which an add never touches. The retained
/// `topos pull --quiet` alias still exits 0 and stays silent on stdout.
#[test]
fn end_to_end_claude_code_adopt_writes_no_hook_and_pull_is_silent() {
    let home = scratch("cc-home");
    let claude = scratch("cc-claude");
    // A real Claude Code skill under the isolated config home.
    let skill = claude.join("skills").join("pr-describe");
    std::fs::create_dir_all(&skill).unwrap();
    let skill_md = skill.join("SKILL.md");
    std::fs::write(
        &skill_md,
        "# pr-describe\n\nWrite a clear PR description.\n",
    )
    .unwrap();
    let before = std::fs::read(&skill_md).unwrap();

    // add → recognized as Claude Code and recorded; no hook, no other write outside ~/.topos/.
    // MACHINE-WIDE: the skill lives in the agent's own config home, not in a checkout.
    let (ok, v) = run_in(
        &home,
        &claude,
        &["--json", "add", "-g", skill.to_str().unwrap()],
    );
    assert!(ok, "add should exit 0");
    assert_eq!(v["data"]["name"], "pr-describe");
    assert!(
        v["data"].get("currency").is_none(),
        "no hook report — none was written: {v}"
    );
    assert!(v["data"].get("triggers").is_none(), "{v}");
    assert!(
        !claude.join("settings.json").exists(),
        "the harness config is never opened by an add"
    );
    assert!(
        !claude.join("skills").join("topos").exists(),
        "an add places no built-in skill"
    );

    // Adopt-in-place wrote nothing into the skill dir.
    assert_eq!(
        std::fs::read(&skill_md).unwrap(),
        before,
        "the skill file is byte-identical"
    );

    // list shows it in the machine scope.
    let (ok, v) = run_in(&home, &claude, &["--json", "list"]);
    assert!(ok);
    let machine_rows = v["data"]["scopes"]
        .as_array()
        .and_then(|s| s.iter().find(|x| x["scope"] == "machine").cloned())
        .expect("the machine scope");
    assert_eq!(machine_rows["rows"][0]["skill"], "pr-describe");

    // The installed hook runs `topos update --quiet --hook claude-code`; the field's already-armed
    // hooks run the retained `topos pull --quiet` alias. BOTH must exit 0 and emit NOTHING on stdout
    // (a SessionStart hook's stdout is injected into the session). Exercise the alias here (it must
    // keep working).
    let out = Command::new(bin())
        .env("TOPOS_HOME", &home)
        .env("CLAUDE_CONFIG_DIR", &claude)
        .env_remove("CLAUDECODE")
        .args(["pull", "--quiet"])
        .output()
        .expect("spawn topos pull");
    assert!(out.status.success(), "the pull alias exits 0");
    assert!(
        out.stdout.is_empty(),
        "pull --quiet emits nothing on stdout"
    );

    // A second add of the same dir is refused (already tracked), not silently duplicated.
    let (ok, v) = run_in(
        &home,
        &claude,
        &["--json", "add", "-g", skill.to_str().unwrap()],
    );
    assert!(!ok, "re-adding the same dir exits nonzero");
    assert_eq!(v["error"]["code"], "ALREADY_TRACKED");

    let _ = std::fs::remove_dir_all(&claude);
    let _ = std::fs::remove_dir_all(&home);
}

/// The health panel on a fresh machine — no global manifest, not connected, the built-in placed
/// by the first sweep. `status -g --json` says `builtin_in_place: true`; the terminal face must
/// say the same thing in words, or the panel reads as if the machine held nothing one command
/// away from a `list -g` that shows the bundle.
#[test]
fn status_names_the_built_in_on_the_terminal_where_no_manifest_governs_the_machine() {
    let root = scratch("status-builtin");
    let topos_home = root.join("topos");
    let disc_home = root.join("home");
    let claude = disc_home.join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    let run = |args: &[&str]| -> std::process::Output {
        Command::new(bin())
            .env("TOPOS_HOME", &topos_home)
            .env("HOME", &disc_home)
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env_remove("CLAUDECODE")
            .env("TOPOS_NO_UPDATE_CHECK", "1")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("CODEX_HOME")
            .env_remove("HERMES_HOME")
            .env_remove("VIBE_HOME")
            .env_remove("AUTOHAND_HOME")
            .env_remove("APPDATA")
            .env_remove("FLATPAK_XDG_CONFIG_HOME")
            .current_dir(&disc_home)
            .args(args)
            .output()
            .expect("spawn topos")
    };
    // The bare sweep places the built-in; nothing else is demanded (no manifest, no session).
    let swept = run(&["update"]);
    assert!(
        swept.status.success(),
        "{}",
        String::from_utf8_lossy(&swept.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&run(&["status", "-g", "--json"]).stdout).unwrap();
    let machine = &json["data"]["scopes"][0];
    assert_eq!(machine["scope"], "machine", "{json}");
    assert_eq!(machine["builtin_in_place"], true, "{json}");
    assert!(machine.get("manifest").is_none(), "{json}");
    // The same fact, in words, on the terminal face — under the no-manifest header, said once.
    let tty = String::from_utf8_lossy(&run(&["status", "-g"]).stdout).into_owned();
    assert!(tty.contains("not connected"), "{tty}");
    assert!(
        tty.contains(
            "Machine-wide — no global manifest\n  the built-in topos bundle is in place; no \
             workspace demands anything machine-wide\n  nothing pending"
        ),
        "{tty}"
    );
    assert_eq!(tty.matches("machine-wide").count(), 1, "{tty}");
}

/// Run `topos` with discovery pinned to a hermetic, EMPTY `$HOME` (so no real harness on the dev's machine
/// is "present") and a Claude config home holding the laid skills. The other per-harness home overrides are
/// scrubbed so discovery resolves ONLY the injected Claude Code dir — a name never resolves to a stray real
/// skill.
fn run_disc(
    topos_home: &Path,
    disc_home: &Path,
    claude: &Path,
    args: &[&str],
) -> serde_json::Value {
    run_disc_at(topos_home, disc_home, claude, disc_home, args)
}

/// [`run_disc`] from a chosen working directory — for the flows whose whole point is standing in a
/// CHECKOUT (a project `topos.toml` covers the cwd) while discovery still resolves the same
/// hermetic roots.
fn run_disc_at(
    topos_home: &Path,
    disc_home: &Path,
    claude: &Path,
    cwd: &Path,
    args: &[&str],
) -> serde_json::Value {
    seed_pick(topos_home);
    let out = Command::new(bin())
        .env("TOPOS_HOME", topos_home)
        .env("HOME", disc_home)
        .current_dir(cwd)
        .env("CLAUDE_CONFIG_DIR", claude)
        .env_remove("CLAUDECODE")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("CODEX_HOME")
        .env_remove("HERMES_HOME")
        .env_remove("VIBE_HOME")
        .env_remove("AUTOHAND_HOME")
        .env_remove("APPDATA")
        .env_remove("FLATPAK_XDG_CONFIG_HOME")
        .args(args)
        .output()
        .expect("spawn topos");
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("non-JSON stdout: {}", String::from_utf8_lossy(&out.stdout)))
}

/// A skill this very checkout DELIVERS is not an untracked discovery. The tracked-paths scan used
/// to read the machine store alone while the discovery scan spanned the home AND the checkout, so
/// a skill added into the project (`--dest .claude/skills`, custody in the checkout's own store)
/// came back as adoptable — and the wrong count then leaked into the bare listing's summary line.
#[test]
fn a_project_scoped_skill_is_not_offered_back_as_untracked() {
    let home = scratch("proj-untracked-home");
    let disc = scratch("proj-untracked-disc"); // an EMPTY discovery HOME
    let claude = scratch("proj-untracked-claude");
    // The checkout sits OUTSIDE `$HOME` — the manifest walk stops at the home directory.
    let proj = scratch("proj-untracked-repo");
    let src = proj.join("src").join("writer");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("SKILL.md"),
        "---\nname: writer\ndescription: writes it down\n---\n\nWrite it down.\n",
    )
    .unwrap();

    let at = |args: &[&str]| run_disc_at(&home, &disc, &claude, &proj, args);
    let v = at(&["--json", "init"]);
    assert_eq!(v["ok"], true, "{v}");
    let v = at(&["--json", "add", "./src/writer", "--dest", ".claude/skills"]);
    assert_eq!(v["ok"], true, "{v}");
    let v = at(&["--json", "update"]);
    assert_eq!(v["ok"], true, "{v}");
    assert!(
        proj.join(".claude/skills/writer/SKILL.md").is_file(),
        "the project copy landed"
    );

    // The discovery listing finds the placed copy — and does not offer it as untracked.
    let v = at(&["--json", "list", "--untracked"]);
    let empty = Vec::new();
    let untracked = v["data"]["untracked"].as_array().unwrap_or(&empty);
    assert!(
        !untracked.iter().any(|u| u["name"] == "writer"),
        "the checkout's own delivery is tracked, not adoptable: {untracked:?}"
    );
    // …and the bare listing's summary line does not count it either.
    let v = at(&["--json", "list"]);
    assert!(
        v["data"].get("untracked_summary").is_none(),
        "nothing is being withheld: {v}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&disc);
    let _ = std::fs::remove_dir_all(&claude);
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn end_to_end_add_by_name_resolves_a_discovered_skill() {
    let home = scratch("byname-home");
    let disc = scratch("byname-disc"); // an EMPTY discovery HOME — only the Claude config below is present
    let claude = scratch("byname-claude");
    let skill = claude.join("skills").join("deploy");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "# deploy\n\nShip it.\n").unwrap();

    // `topos list --untracked` discovers it, with the FOLDER it sits in and the installed agents
    // that read that folder (the `@<slug>` tokens); the bare list carries only the one summary line.
    let v = run_disc(&home, &disc, &claude, &["--json", "list", "--untracked"]);
    let untracked = v["data"]["untracked"].as_array().expect("untracked array");
    assert_eq!(untracked.len(), 1, "{untracked:?}");
    assert_eq!(untracked[0]["name"], "deploy");
    assert_eq!(
        untracked[0]["folder"],
        claude.join("skills").to_string_lossy().into_owned()
    );
    assert_eq!(untracked[0]["readers"], serde_json::json!(["claude-code"]));
    let v = run_disc(&home, &disc, &claude, &["--json", "list"]);
    assert_eq!(v["data"]["untracked_summary"]["skills"], 1, "{v}");

    // The runs below all stand IN `$HOME`, where the project walk stops by rule — no `topos.toml`
    // can cover this folder, so the project-scoped add refuses and names the two ways out. The
    // machine-wide file is the one that applies here, and `-g` is how you say so.
    let v = run_disc(&home, &disc, &claude, &["--json", "add", "deploy"]);
    assert_eq!(v["ok"], false, "{v}");
    assert_eq!(v["error"]["code"], "NO_MANIFEST");
    let codes: Vec<&str> = v["next_actions"]
        .as_array()
        .expect("the two ways out")
        .iter()
        .map(|a| a["code"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        codes,
        ["INIT_PROJECT_MANIFEST", "RETRY_MACHINE_WIDE"],
        "{v}"
    );
    let retry: Vec<&str> = v["next_actions"][1]["argv"]
        .as_array()
        .expect("the retry argv")
        .iter()
        .map(|t| t.as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        retry,
        ["topos", "add", "-g", "deploy"],
        "the retry is this very invocation, machine-wide: {v}"
    );

    // `topos add -g deploy` — resolves the NAME against that inventory and adopts it (Claude Code
    // recognized, auto-update armed), with no path typed.
    let v = run_disc(&home, &disc, &claude, &["--json", "add", "-g", "deploy"]);
    assert_eq!(v["command"], "add");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["data"]["name"], "deploy");
    assert_eq!(v["data"]["tracked"], true);

    // Now tracked → discovery no longer lists it as untracked, and the machine scope carries it.
    let v = run_disc(&home, &disc, &claude, &["--json", "list", "--untracked"]);
    assert!(
        v["data"]["untracked"]
            .as_array()
            .is_none_or(|u| u.is_empty()),
        "{v}"
    );
    let v = run_disc(&home, &disc, &claude, &["--json", "list"]);
    let machine_rows = v["data"]["scopes"]
        .as_array()
        .and_then(|s| s.iter().find(|x| x["scope"] == "machine").cloned())
        .expect("the machine scope");
    assert_eq!(machine_rows["rows"][0]["skill"], "deploy");
    let v = run_disc(&home, &disc, &claude, &["--json", "add", "-g", "deploy"]);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "ALREADY_TRACKED");
    // The DISAMBIGUATED re-add reports the SAME code (an agent branches identically whether or not it
    // typed `@harness`) — not HARNESS_NOT_FOUND just because discovery excludes the tracked placement.
    let v = run_disc(
        &home,
        &disc,
        &claude,
        &["--json", "add", "-g", "deploy@claude-code"],
    );
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "ALREADY_TRACKED");

    // A name nowhere in the inventory fails closed with the discovery-specific code (not NO_SUCH_SKILL,
    // which is about tracked skills).
    let v = run_disc(&home, &disc, &claude, &["--json", "add", "-g", "ghost"]);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "NO_UNTRACKED_SKILL");

    // A path-shaped positional is treated as a PATH (adopt in place), NEVER resolved as the same-named
    // discovered skill: a `./`-prefixed token pointing at nothing here fails as a path adopt (an fs
    // error), so it can't sneak in as the discovered "deploy".
    let v = run_disc(&home, &disc, &claude, &["--json", "add", "-g", "./deploy"]);
    assert_eq!(v["ok"], false, "{v}");
    assert_ne!(
        v["error"]["code"], "ALREADY_TRACKED",
        "a path is adopted as a path, not resolved to the discovered skill"
    );

    let _ = std::fs::remove_dir_all(&claude);
    let _ = std::fs::remove_dir_all(&disc);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_bad_review_hash_is_invalid_argument_on_both_surfaces() {
    let home = scratch("badhash");

    // --json: the stable code + the verbatim usage guidance ride the envelope (never CORRUPT_STATE).
    let out = run_raw(&home, &["review", "docs@abc", "--approve", "--json"], false);
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON stdout");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    assert_eq!(v["error"]["outcome"], "PERMANENT_FAILURE");
    assert_eq!(v["error"]["retryable"], false);
    let msg = v["error"]["context"]["message"].as_str().expect("message");
    assert!(msg.contains("64-char lowercase hex"), "{msg}");

    // TTY: the same guidance verbatim on stderr.
    let out = run_raw(&home, &["review", "docs@abc", "--approve"], false);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("64-char lowercase hex"), "{stderr}");

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn review_verdict_exclusivity_is_a_clap_usage_error_but_no_verdict_is_the_describe() {
    let home = scratch("verdict");
    let target = format!("docs@{}", "ab".repeat(32));

    // Two verdict flags → a standard clap conflict (the optional `verdict` ArgGroup stays mutually
    // exclusive; usage + help hint, exit 2 — never an envelope).
    let out = run_raw(&home, &["review", &target, "--approve", "--reject"], false);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--approve"), "{stderr}");
    assert!(stderr.contains("Usage"), "{stderr}");

    // NO verdict is allowed by clap (the group is OPTIONAL) — a bare target is the two-phase describe.
    // With no session there is nothing to describe (the proposal lives on the plane), so it refuses
    // toward `topos login`, exit 1 — a runtime domain refusal, not a clap usage error.
    let out = run_raw(&home, &["review", &target, "--json"], false);
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON stdout");
    assert_eq!(v["error"]["code"], "SESSION_REQUIRED");

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_corrupt_sidecar_doc_still_reports_corrupt_state() {
    let home = scratch("corrupt");
    // A hand-corrupted lock.json under a plausible skill dir: name resolution must fail CLOSED as
    // corruption — the usage-error remap above must never reclassify a persisted-doc failure.
    let skill_dir = home.join("skills").join("someskill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("lock.json"), b"{ not json").unwrap();

    let out = run_raw(&home, &["diff", "someskill", "--json"], false);
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON stdout");
    assert_eq!(v["error"]["code"], "CORRUPT_STATE");
    assert_eq!(
        v["error"]["context"]["message"],
        "topos's own state on this machine is unreadable"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_folder_that_cannot_be_read_refuses_by_name_and_is_detailed_in_the_log() {
    let home = scratch("iodiag");
    // A REGULAR FILE where a bundle folder belongs: the walk's `read_dir` fails with the OS's own
    // "not a directory". That is not a transient io fault — the same path answers the same way
    // forever — so it earns the scan family's typed refusal, which names the folder and the reason
    // VERBATIM (a rejected folder is useless news without the thing that caused it). The redaction
    // rail the untyped io family still rides is unit-tested at its own producer.
    let not_a_dir = home.join("not-a-folder");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(&not_a_dir, b"just a file\n").unwrap();
    let named = not_a_dir.display().to_string();
    // A folder's manifest first: the scope resolves BEFORE the source is read, so without one the
    // add would refuse on the scope rather than reaching the read this test is about.
    let out = run_raw(&home, &["init", "--json"], false);
    assert!(out.status.success(), "init should exit 0");

    let out = run_raw(&home, &["add", &named, "--json"], false);
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON stdout");
    assert_eq!(v["error"]["code"], "SCAN_REJECTED");
    assert_eq!(v["error"]["outcome"], "PERMANENT_FAILURE");
    assert_eq!(v["error"]["retryable"], false);
    let message = v["error"]["context"]["message"]
        .as_str()
        .expect("a message");
    assert!(
        message.starts_with("the skill directory was rejected — the folder root: "),
        "{message}"
    );
    assert!(message.contains("not a directory"), "{message}");

    // The diagnostics log still carries the whole chain, path and all.
    let log = std::fs::read_to_string(home.join("log.jsonl")).expect("the diagnostics log exists");
    let event: serde_json::Value = log
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .find(|e: &serde_json::Value| e["action"] == "error")
        .expect("an error event landed");
    assert_eq!(event["verb"], "add");
    assert_eq!(event["code"], "SCAN_REJECTED");

    // TTY: the same sentence, and never the retry invitation a transient fault earns.
    let out = run_raw(&home, &["add", &named], false);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error: the skill directory was rejected — the folder root: "),
        "{stderr}"
    );
    assert!(
        !stderr.contains("running it again is safe"),
        "a permanent refusal never invites a retry: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn update_keep_mine_flag_shapes_are_usage_errors() {
    let home = scratch("keepmineusage");

    // Missing <skill> target → the runtime usage error (INVALID_ARGUMENT, exit 1). Reached here
    // through the `pull` alias the field's armed hooks still run.
    let out = run_raw(&home, &["pull", "--keep-mine", "--json"], false);
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON stdout");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");

    // Combined with @<hash> → the runtime usage error rides the envelope as INVALID_ARGUMENT.
    let target = format!("docs@{}", "ab".repeat(32));
    let out = run_raw(&home, &["update", &target, "--keep-mine", "--json"], false);
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON stdout");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");

    let _ = std::fs::remove_dir_all(&home);
}

/// The built-in `topos` skill is refused BY NAME on every targeted arm of `update` — the reconcile,
/// the go-back, `--keep-mine`, `--force`, and `--reset`.
///
/// `--reset` is the one that used to slip through: it dispatches ahead of the reconcile, so it never
/// met the guard the other arms enforced and went looking for a manifest row and a version to take
/// — neither of which exists for a bundle that ships with the binary. One guard, ahead of all of
/// them, so the answer cannot depend on which flag was typed.
#[test]
fn update_never_takes_the_builtin_by_name() {
    let home = scratch("builtin-update");
    for args in [
        &["update", "topos", "--json"][..],
        &["update", "topos", "--reset", "--json"][..],
        &["update", "topos", "--reset", "--yes", "--json"][..],
        &["update", "topos", "--force", "--json"][..],
        &["update", "topos", "--keep-mine", "--json"][..],
        &["update", "topos@abc1234", "--json"][..],
        // Named beside another target, it is still the built-in being asked for.
        &["update", "deploy", "topos", "--reset", "--json"][..],
    ] {
        let out = run_raw(&home, args, false);
        assert!(!out.status.success(), "{args:?}");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON stdout");
        assert_eq!(v["error"]["code"], "INVALID_ARGUMENT", "{args:?}: {v}");
        let message = v["error"]["context"]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(message.contains("built-in skill"), "{args:?}: {message}");
        assert!(message.contains("topos self-update"), "{args:?}: {message}");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// `--dest`/`-a` on `update` names the copy `--reset` drops the edits of, and nothing else. Without
/// `--reset` it is REFUSED by name: silently ignoring it would read as a narrowed update while the
/// reconcile converged every copy the manifest asks for. The refusal fires before any store is
/// touched, so the machine it names is untouched too.
#[test]
fn update_dest_without_reset_is_refused_by_name() {
    let home = scratch("destnoreset");
    for args in [
        &["update", "deploy", "--dest", ".claude/skills", "--json"][..],
        &["update", "deploy", "-a", "codex", "--json"][..],
    ] {
        let out = run_raw(&home, args, false);
        assert!(!out.status.success(), "{args:?}");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON stdout");
        assert_eq!(v["error"]["code"], "INVALID_ARGUMENT", "{v}");
        let message = v["error"]["context"]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(message.contains("--reset"), "{message}");
    }
    // The two spellings are ONE choice: these verbs act on a single copy, so naming both refuses
    // at the parser rather than resolving a set the act has no meaning for.
    let out = run_raw(
        &home,
        &[
            "update",
            "deploy",
            "--reset",
            "-a",
            "codex",
            "--dest",
            ".claude/skills",
        ],
        false,
    );
    assert!(!out.status.success());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_no_session_add_refusal_carries_the_structural_login_action() {
    // The most common refusal: `add @acme/foo` with no session. The envelope must carry the fix
    // STRUCTURALLY — a LOGIN_WORKSPACE next action whose argv holds the CONCRETE address as one
    // token — beside the prose, and the process must exit nonzero.
    let home = scratch("nosess-add");
    let out = run_raw(&home, &["add", "@acme/foo", "--json"], false);
    assert!(!out.status.success(), "an add failure exits nonzero");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON stdout");
    assert_eq!(v["error"]["code"], "SESSION_REQUIRED");
    let actions = v["next_actions"].as_array().expect("next_actions");
    assert!(!actions.is_empty(), "the refusal is never prose-only: {v}");
    assert_eq!(actions[0]["code"], "LOGIN_WORKSPACE");
    let argv: Vec<&str> = actions[0]["argv"]
        .as_array()
        .expect("argv")
        .iter()
        .map(|t| t.as_str().unwrap_or_default())
        .collect();
    assert_eq!(argv, vec!["topos", "login", "acme", "--json"]);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn reference_shaped_grammar_refusals_never_reach_the_path_arms() {
    // A reference-shaped token the grammar refuses surfaces its TYPED refusal (teaching the
    // correct shape) with a nonzero exit — never the filesystem-path arm's canonicalize error or
    // the discovery arm's NO_UNTRACKED_SKILL wrong-fix answer.
    let home = scratch("grammar");
    for (arg, teaches) in [
        // A malformed @-sugar (charset) teaches the three valid shapes.
        ("@Acme/Deploy", "@<workspace>/<bundle>"),
        ("#backend", "channels"),
        ("@acme/deploy@abc1234", "64-character"),
        ("topos.example.com/eng/deploy@abc1234", "64-character"),
    ] {
        let out = run_raw(&home, &["add", arg, "--json"], false);
        assert!(!out.status.success(), "{arg}: an add failure exits nonzero");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON stdout");
        assert_eq!(v["error"]["code"], "INVALID_ARGUMENT", "{arg}: {v}");
        let msg = v["error"]["context"]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(
            msg.contains(teaches),
            "{arg}: the refusal teaches the shape: {msg}"
        );
        assert!(
            !msg.contains("canonicalize"),
            "{arg}: never the path arm: {msg}"
        );
        assert!(
            !msg.contains("untracked"),
            "{arg}: never the discovery arm: {msg}"
        );
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// The hermetic sweep runner: a scratch `$HOME` (so no real harness on the dev's machine is
/// "present" and nothing lands in a real skills dir), an injected Claude config home, the other
/// per-harness home overrides scrubbed — returning BOTH raw streams, because the hook document IS
/// the stdout under test.
fn sweep_raw(
    topos_home: &Path,
    disc_home: &Path,
    claude: &Path,
    args: &[&str],
) -> std::process::Output {
    seed_pick(topos_home);
    sweep_raw_unseeded(topos_home, disc_home, claude, args)
}

/// [`sweep_raw`] over a `$TOPOS_HOME` exactly as the test laid it out — no pick seeded — for
/// the tests about the pick's absence (the migration seed).
fn sweep_raw_unseeded(
    topos_home: &Path,
    disc_home: &Path,
    claude: &Path,
    args: &[&str],
) -> std::process::Output {
    Command::new(bin())
        .env("TOPOS_HOME", topos_home)
        .env("HOME", disc_home)
        .current_dir(disc_home)
        .env("CLAUDE_CONFIG_DIR", claude)
        .env_remove("CLAUDECODE")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("CODEX_HOME")
        .env_remove("HERMES_HOME")
        .env_remove("VIBE_HOME")
        .env_remove("AUTOHAND_HOME")
        .env_remove("APPDATA")
        .env_remove("FLATPAK_XDG_CONFIG_HOME")
        .args(args)
        .output()
        .expect("spawn topos update --quiet")
}

#[test]
fn the_quiet_sweeps_hook_document_follows_the_calling_triggers_dialect() {
    // The REAL command path, over the REAL argv: this is the seam the helper's unit tests cannot
    // reach — a hard-coded dialect, or a `--hook` value parsed and then dropped on the floor,
    // would leave every one of those green while shipping the bug back.
    //
    // Forcing a CHANGED-BYTES sweep needs no server: a FRESH `TOPOS_HOME`'s first bare sweep
    // places the built-in `topos` skill, and that placement is a byte change. Each arm therefore
    // gets its own untouched home, and asserts the placement really happened — otherwise a sweep
    // that quietly changed nothing would make the whole assertion vacuous.
    for (tag, marker, wants_reload) in [
        ("cc", vec!["--hook", "claude-code"], true),
        ("bare", vec![], false),
        ("codex", vec!["--hook", "codex"], false),
    ] {
        let home = scratch(&format!("dialect-{tag}-home"));
        let disc = scratch(&format!("dialect-{tag}-disc"));
        let claude = scratch(&format!("dialect-{tag}-claude"));

        let mut args = vec!["update", "--quiet", "--ttl", "0"];
        args.extend_from_slice(&marker);
        let out = sweep_raw(&home, &disc, &claude, &args);

        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "{tag}: a session-start sweep always exits 0 — stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Non-vacuity: the sweep DID change bytes, so a hook document was genuinely in play.
        assert!(
            claude
                .join("skills")
                .join("topos")
                .join("SKILL.md")
                .exists(),
            "{tag}: the sweep must have placed the built-in skill (a changed-bytes sweep)"
        );

        if wants_reload {
            let v: serde_json::Value = serde_json::from_str(stdout.trim())
                .unwrap_or_else(|_| panic!("{tag}: the hook document must be JSON: {stdout:?}"));
            assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
            assert_eq!(
                v["hookSpecificOutput"]["reloadSkills"], true,
                "{tag}: the declared harness opts INTO the reload extension"
            );
        } else {
            // The whole point: an agent that never declared itself must never be handed a field
            // its schema may reject — one unknown key fails the entire session-start hook.
            assert!(
                !stdout.contains("reloadSkills"),
                "{tag}: an undeclared harness must never receive the reload extension: {stdout:?}"
            );
            assert!(
                stdout.is_empty(),
                "{tag}: with nothing a person must read, the conservative dialect stays silent: \
                 {stdout:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&disc);
        let _ = std::fs::remove_dir_all(&claude);
    }
}

/// A legacy machine's trigger record, as the last build wrote it: one registered row.
fn legacy_record(topos_home: &Path, slug: &str) -> PathBuf {
    let record = topos_home.join("state").join("trigger_registration.json");
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(
        &record,
        format!(
            "{{\"schema_version\": 1, \"agents\": {{\"{slug}\": {{\"at_ms\": 1, \"registered\": true, \"retry_at_ms\": 0}}}}}}\n"
        ),
    )
    .unwrap();
    record
}

/// **The sweep registers nobody; the seed carries the old answer into the pick.** Over the REAL
/// argv, the real detection roots and the real config writes — the seam the unit tests cannot
/// reach. A machine that predates the pick (no `agents.json`, a legacy record saying cursor was
/// registered) runs a quiet hook sweep: no agent gains a hook, whatever is installed; the legacy
/// record becomes the machine pick `["cursor"]` and is deleted; the sweep's stdout stays empty.
/// A non-quiet verb on such a machine says the one line, once.
#[test]
fn the_quiet_sweep_never_registers_an_agent_and_the_seed_moves_the_record_into_the_pick() {
    let home = scratch("seed-home");
    let disc = scratch("seed-disc");
    let claude = scratch("seed-claude");
    // Two agents installed on the machine; the legacy record names only cursor.
    std::fs::create_dir_all(disc.join(".cursor")).unwrap();
    std::fs::create_dir_all(disc.join(".gemini")).unwrap();
    let record = legacy_record(&home, "cursor");
    let pick = home.join("agents.json");
    let cursor_hooks = disc.join(".cursor").join("hooks.json");

    let out = sweep_raw_unseeded(
        &home,
        &disc,
        &claude,
        &["update", "--quiet", "--ttl", "0", "--hook", "claude-code"],
    );
    assert!(
        out.status.success(),
        "a session-start sweep always exits 0 — stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!cursor_hooks.exists(), "the sweep registers no agent");
    assert!(
        !disc.join(".gemini").join("settings.json").exists(),
        "not the other installed one either"
    );
    assert!(
        !claude.join("settings.json").exists(),
        "and not the agent whose hook fired the sweep"
    );
    let seeded: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pick).expect("the seed wrote the machine pick"))
            .unwrap();
    assert_eq!(seeded["agents"], serde_json::json!(["cursor"]));
    assert!(!record.exists(), "the legacy record is gone");
    // The hook document is the ordinary one for a sweep that placed bytes (the first sweep
    // lands the built-in for the seeded pick): the seed is never a line a person must read.
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !stdout.contains("additionalContext"),
        "the seed adds no line to the hook's stdout: {stdout:?}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("Using "),
        "and under --quiet it says nothing on stderr either"
    );
    // The seeded pick is what the sweep placed for: cursor's skills dir holds the built-in,
    // the other installed agents' dirs stay untouched.
    assert!(
        disc.join(".cursor")
            .join("skills")
            .join("topos")
            .join("SKILL.md")
            .exists(),
        "the picked agent gets the machine's bundles"
    );
    assert!(!disc.join(".gemini").join("skills").exists());

    // A NON-QUIET verb on a fresh legacy machine says the one line, on stderr, once.
    let home2 = scratch("seed-home-2");
    legacy_record(&home2, "cursor");
    let out = sweep_raw_unseeded(&home2, &disc, &claude, &["list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains(
            "Using Cursor on this machine (from what an earlier topos set up here). Change: \
             topos agents add|remove -g"
        ),
        "{stderr:?}"
    );
    let again = sweep_raw_unseeded(&home2, &disc, &claude, &["list"]);
    assert!(
        !String::from_utf8_lossy(&again.stderr).contains("Using "),
        "said once"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&home2);
    let _ = std::fs::remove_dir_all(&disc);
    let _ = std::fs::remove_dir_all(&claude);
}

/// A throwaway HTTP server that answers EVERY request `404 Not Found` — the shape a workspace this
/// device no longer reaches presents (unlinked, seat removed, workspace gone). Std-only; the guard
/// shuts the listener when dropped.
struct Gone404 {
    port: u16,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Gone404 {
    fn start() -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a 404 server");
        let port = listener.local_addr().unwrap().port();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let Ok(mut c) = conn else { return };
                let _ = c.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let mut buf = [0_u8; 4096];
                let _ = c.read(&mut buf);
                let _ = c.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = c.flush();
            }
        });
        Self { port, stop }
    }
}

impl Drop for Gone404 {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// Write a session row pointing at `port` — an ACTIVE login whose server answers 404, which is how
/// "this device no longer has access" reaches the sweep.
fn write_gone_session(topos_home: &Path, port: u16) {
    let identity = topos_home.join("identity");
    std::fs::create_dir_all(&identity).unwrap();
    let doc = serde_json::json!({
        "schema_version": 1,
        "sessions": [{
            "host": "gone.example",
            "base_url": format!("http://127.0.0.1:{port}"),
            "workspace_id": "w_gone",
            "workspace_name": "gonews",
            "display_name": "Gone WS",
            "session_id": "sn_gone",
            "credential": "tok_gone",
            "status": "active",
            "logged_in_at": 1_000,
        }],
    });
    let path = identity.join("sessions.json");
    std::fs::write(&path, doc.to_string()).unwrap();
    // The sessions doc holds a bearer credential, so the client reads it through the private-file
    // primitives and REFUSES anything looser than 0600 — a fixture must be born the same way.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[test]
fn a_person_facing_line_reaches_every_dialect_as_an_injectable_document() {
    // The fact under test: a quiet sweep that changed NOTHING but has something a person must not
    // miss (here, a workspace this device can no longer reach — its skills are frozen in place)
    // must still deliver that text through `additionalContext`. Raw stdout is not a substitute: an
    // agent that validates hook output against a strict schema is free to discard anything else,
    // and the warning would evaporate silently — which is exactly the defect.
    //
    // `reloadSkills` must NOT ride along: nothing moved, so asking Claude Code to re-scan its
    // skill dirs would be a lie. Both halves are asserted per dialect.
    for (tag, marker, dialect_reloads) in [
        ("bare", vec![], false),
        ("codex", vec!["--hook", "codex"], false),
        ("cc", vec!["--hook", "claude-code"], true),
    ] {
        let home = scratch(&format!("notice-{tag}-home"));
        let disc = scratch(&format!("notice-{tag}-disc"));
        let claude = scratch(&format!("notice-{tag}-claude"));

        // Settle the built-in FIRST, with no session in play, so the sweep under test is a genuine
        // no-change sweep — otherwise the built-in placement would set `changed` and the
        // `reloadSkills` half of this test would prove nothing.
        let warmup = sweep_raw(&home, &disc, &claude, &["update", "--quiet", "--ttl", "0"]);
        assert!(warmup.status.success(), "{tag}: warm-up sweep exits 0");
        assert!(
            claude
                .join("skills")
                .join("topos")
                .join("SKILL.md")
                .exists(),
            "{tag}: the warm-up placed the built-in, so the next sweep changes nothing"
        );

        let server = Gone404::start();
        write_gone_session(&home, server.port);

        let mut args = vec!["update", "--quiet", "--ttl", "0"];
        args.extend_from_slice(&marker);
        let out = sweep_raw(&home, &disc, &claude, &args);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "{tag}: a session-start sweep always exits 0 — stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // It arrives as a DOCUMENT, not raw text.
        let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
            panic!("{tag}: the line must arrive as an injectable document, got raw: {stdout:?}")
        });
        let inner = v["hookSpecificOutput"]
            .as_object()
            .unwrap_or_else(|| panic!("{tag}: no hookSpecificOutput: {stdout:?}"));
        assert_eq!(inner["hookEventName"], "SessionStart", "{tag}");
        let ctx = inner
            .get("additionalContext")
            .and_then(|c| c.as_str())
            .unwrap_or_else(|| panic!("{tag}: the fact must ride additionalContext: {stdout:?}"));
        assert!(
            ctx.contains("no longer has access"),
            "{tag}: the person-facing fact itself must be in the injected text: {ctx:?}"
        );
        // Nothing moved — so no dialect may claim a reload, not even the one that understands it.
        assert!(
            !stdout.contains("reloadSkills"),
            "{tag}: a no-change sweep must never ask for a skill re-scan (dialect reloads: \
             {dialect_reloads}): {stdout:?}"
        );

        drop(server);
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&disc);
        let _ = std::fs::remove_dir_all(&claude);
    }
}

/// `-s` picks WHICH skills to take from a repository that holds several. An MCP server bundle is
/// ONE document with nothing to select from, so the pair is refused by name — the refusal moved
/// out of clap (where the argument grammar could state it) into the runtime dispatch, and its
/// coverage did not move with it. Asserted through real argv, which is the only place the pairing
/// exists at all.
#[test]
fn selecting_members_of_an_mcp_source_is_refused_by_name() {
    let home = scratch("mcp-select");
    let (ok, v) = run(
        &home,
        &[
            "add",
            "-s",
            "weather",
            "--kind",
            "mcp",
            "./anything",
            "--json",
        ],
    );
    assert!(!ok, "the pairing exits nonzero: {v}");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    let msg = v["error"]["context"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        msg.contains("-s") && msg.contains("one document") && msg.contains("drop `-s`"),
        "the refusal names the flag and why it cannot apply: {msg}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_home_reached_through_a_symlink_lists_one_spelling() {
    // `$HOME` is whatever the environment says — often a symlink — while the working directory the
    // kernel reports is already resolved. One listing used to show the same home under two
    // spellings, which reads as two machines. Only a real process proves it: the roots resolve
    // from the environment.
    let scratch = scratch("symhome");
    let real = scratch.join("real");
    for (dir, name) in [(".cursor/skills", "one"), (".augment/skills", "two")] {
        let d = real.join(dir).join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), format!("---\nname: {name}\n---\nhi\n")).unwrap();
    }
    let link = scratch.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let real_c = real.canonicalize().unwrap();

    let out = Command::new(bin())
        .env("TOPOS_HOME", scratch.join("topos"))
        .env("HOME", &link)
        .env("CLAUDE_CONFIG_DIR", scratch.join(".claude-isolated"))
        .env_remove("CLAUDECODE")
        .current_dir(work_dir(&scratch))
        .args(["list", "--untracked"])
        .output()
        .expect("spawn topos");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    let folders: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("  /") && l.trim_end().ends_with(':'))
        .collect();
    assert_eq!(folders.len(), 2, "both agents' folders are listed: {text}");
    for line in &folders {
        assert!(
            line.trim_start().starts_with(real_c.to_str().unwrap()),
            "every folder under the ONE resolved home: {text}"
        );
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn the_machine_file_prints_as_itself_under_a_symlinked_home() {
    // `$HOME` through a symlink, `TOPOS_HOME` unset — so the sidecar home is derived from `$HOME`
    // and then compared, lexically, against the detection root. Resolve one side only and the
    // strip misses: the machine file starts printing as an absolute path on every surface that
    // names it. Only a real process proves it — both roots come from the environment.
    let scratch = scratch("symhome-manifest");
    let real = scratch.join("real");
    std::fs::create_dir_all(real.join(".topos")).unwrap();
    std::fs::write(real.join(".topos").join("topos.toml"), "schema = 1\n").unwrap();
    let cwd = real.join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let link = scratch.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let run = |args: &[&str]| -> String {
        let out = Command::new(bin())
            .env_remove("TOPOS_HOME")
            .env("HOME", &link)
            .env("CLAUDE_CONFIG_DIR", scratch.join(".claude-isolated"))
            .env_remove("CLAUDECODE")
            .current_dir(link.join("cwd"))
            .args(args)
            .output()
            .expect("spawn topos");
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    for args in [["list"].as_slice(), ["status"].as_slice()] {
        let text = run(args);
        assert!(
            text.contains("~/.topos/topos.toml"),
            "{args:?} names the machine file as itself: {text}"
        );
        assert!(
            !text.contains(real.to_str().unwrap()),
            "{args:?} leaked an absolute home path: {text}"
        );
    }
    // The wire says the same thing — the manifest label is one derivation, not a render-side one.
    let v: serde_json::Value = serde_json::from_str(&run(&["list", "--json"])).expect("json");
    assert_eq!(
        v["data"]["scopes"][0]["manifest"], "~/.topos/topos.toml",
        "{v}"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_gone_stderr_reader_silences_only_stderr() {
    // `topos add … 2> >(head)`: the DIAGNOSTICS reader left, which says nothing about the command
    // or about stdout. A shared closed-pipe latch made this exit 0 — reporting a failed command as
    // a success — and swallowed the outcome still being written to stdout.
    let home = scratch("stderr-pipe");
    let mut child = Command::new(bin())
        .env("TOPOS_HOME", &home)
        .env("CLAUDE_CONFIG_DIR", home.join(".claude-isolated"))
        .env_remove("CLAUDECODE")
        // The debug channel puts a line on stderr BEFORE the envelope reaches stdout, which is the
        // exact order that made a shared latch swallow the answer.
        .env("TOPOS_DEBUG", "1")
        .current_dir(work_dir(&home))
        .args(["add", "-g", "./nope", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn topos");
    drop(child.stderr.take().expect("stderr is piped"));
    let out = child.wait_with_output().expect("the run ends");
    assert!(
        !out.status.success(),
        "a failed command stays failed: {:?}",
        out.status.code()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|_| panic!("non-JSON stdout: {stdout}"));
    assert_eq!(v["ok"], false, "{v}");
    assert_eq!(v["error"]["code"], "SOURCE_MISSING", "{v}");
    let _ = std::fs::remove_dir_all(&home);
}

/// `topos --skill` and `topos --schema` — the two stand-alone DOCUMENT flags. Each prints ONE
/// document on stdout, exits 0, and leaves the machine exactly as it found it: no sidecar tree, no
/// device identity, no envelope wrapping. Only a real process proves the last part, because the
/// interception has to happen before the composition root mints anything.
#[test]
fn the_document_flags_print_their_bytes_and_touch_nothing() {
    let home = scratch("documents");
    // A sidecar path that does NOT exist yet — every other verb would create it on first use.
    let sidecar = home.join("sidecar");
    let run_doc = |args: &[&str]| -> std::process::Output {
        Command::new(bin())
            .env("TOPOS_HOME", &sidecar)
            .env("HOME", &home)
            .env("CLAUDE_CONFIG_DIR", home.join(".claude-isolated"))
            .env_remove("CLAUDECODE")
            .current_dir(work_dir(&home))
            .args(args)
            .output()
            .expect("spawn topos")
    };

    // `--skill` IS the committed source, byte for byte — the same bytes placement would write, so
    // an agent that can only be handed text gets exactly what a placed copy holds.
    let out = run_doc(&["--skill"]);
    assert!(out.status.success(), "--skill exits 0");
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf-8"),
        include_str!("../../../skills/topos/SKILL.md"),
        "--skill prints the embedded SKILL.md verbatim — no envelope, no banner"
    );

    // `--schema` is ONE JSON document over every committed schema, keyed by file stem.
    let out = run_doc(&["--schema"]);
    assert!(out.status.success(), "--schema exits 0");
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("--schema prints JSON");
    let schemas = doc["schemas"].as_object().expect("one `schemas` object");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/schemas");
    let committed = std::fs::read_dir(&dir)
        .expect("contracts/schemas is readable")
        .filter_map(|e| e.ok()?.file_name().to_str().map(str::to_owned))
        .filter(|n| n.ends_with(".schema.json"))
        .count();
    assert_eq!(
        schemas.len(),
        committed,
        "every committed schema is carried"
    );
    for name in [
        "json-envelope",
        "auth-status-data",
        "uninstall-describe-data",
        "uninstall-data",
    ] {
        assert!(schemas.contains_key(name), "{name} is in the document");
    }

    // Neither flag touched the machine: no sidecar tree was created, so nothing was read, minted,
    // or recovered on the way to printing.
    assert!(
        !sidecar.exists(),
        "the document flags create no sidecar state"
    );

    // A document asked for beside SOMETHING ELSE has no answer that keeps both promises, so the
    // run is a usage error naming the conflict — never raw markdown on the `--json` lane a parser
    // is reading, and never a document printed over a verb that silently never ran.
    for (args, conflicts_with) in [
        (vec!["--skill", "--json"], "--json"),
        (vec!["--schema", "--json"], "--json"),
        (vec!["--skill", "list"], "'list'"),
        (vec!["--schema", "status"], "'status'"),
        (vec!["--skill", "list", "--json"], "'list'"),
    ] {
        let out = run_doc(&args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} is a usage error, not an answer"
        );
        assert!(out.stdout.is_empty(), "{args:?} prints no document");
        let err = String::from_utf8(out.stderr).expect("utf-8");
        assert!(err.contains(args[0]), "{args:?}: {err}");
        assert!(err.contains(conflicts_with), "{args:?}: {err}");
    }
    assert!(
        !sidecar.exists(),
        "a refused combination creates no sidecar state either"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_reader_that_leaves_ends_the_run_cleanly() {
    // `topos list | head` — the reader takes what it wants and closes the pipe mid-print. The
    // process must not die printing into it: `println!` panics on a broken pipe (exit 101, a
    // panic message on stderr), and `unsafe_code` is forbidden here, so the SIGPIPE cure is not
    // available. Only a real process proves it — the write has to meet a closed file descriptor.
    let home = scratch("brokenpipe");
    // A discovery root with two skills, so the listing is several lines long: the panic needs a
    // write AFTER the reader is gone.
    let skills = home.join(".claude-isolated").join("skills");
    for name in ["alpha", "beta"] {
        let dir = skills.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\n---\nhi\n"),
        )
        .unwrap();
    }
    for args in [
        ["list", "--untracked"].as_slice(),
        ["list", "--untracked", "--json"].as_slice(),
    ] {
        let mut child = Command::new(bin())
            .env("TOPOS_HOME", &home)
            .env("HOME", &home)
            .env("CLAUDE_CONFIG_DIR", home.join(".claude-isolated"))
            .env_remove("CLAUDECODE")
            .current_dir(work_dir(&home))
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn topos");
        // THE READER LEAVES: dropping our end of the pipe is what `head` does after its last line.
        drop(child.stdout.take().expect("stdout is piped"));
        let out = child.wait_with_output().expect("the run ends");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panicked"), "{args:?} panicked: {err}");
        assert!(
            out.status.success(),
            "{args:?} exits clean, not {:?}",
            out.status.code()
        );
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// **`verify` reports through the PROCESS exit code**, not only through its document — which is the
/// whole point of the verb for a script.
///
/// A live check that RAN is a successful command whatever it found: the envelope stays `ok: true`
/// and the payload carries the verdict, while the process exits `4` for a server nothing could
/// reach. A REFUSAL (a name nothing here tracks) is a different thing and keeps the ordinary `1`,
/// because nothing was verified and there is no verdict to report.
///
/// The address is a loopback port bound only long enough to learn its number and then closed: the
/// dial is refused instantly, with no name lookup and no network.
///
/// **`$HOME` is the scratch dir, not the developer's.** A server bundle's `add` converges every
/// MCP-capable agent this machine has — and the machine roots the placement engine detects against
/// come from `$HOME`, not from `TOPOS_HOME`. Inheriting the real one would write a fixture entry
/// into somebody's actual `~/.codex/config.toml`. Nothing detects inside an empty home, so the
/// adopt here reaches exactly nothing, which is what this test wants anyway.
#[test]
fn verify_exits_with_the_verdicts_own_code_and_keeps_one_for_a_refusal() {
    use std::net::{Ipv4Addr, TcpListener};

    let home = scratch("verify-home");
    let hermetic = |args: &[&str]| -> std::process::Output {
        Command::new(bin())
            .env("TOPOS_HOME", &home)
            .env("HOME", &home)
            .env("CLAUDE_CONFIG_DIR", home.join(".claude-isolated"))
            .env_remove("CLAUDECODE")
            .env_remove("TOPOS_DEBUG")
            .current_dir(work_dir(&home))
            .args(args)
            .output()
            .expect("spawn topos")
    };
    let port = {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port");
        listener.local_addr().unwrap().port()
    };
    let bundle = scratch("verify-bundle").join("weather");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(
        bundle.join("server.json"),
        format!(
            r#"{{"name":"io.github.acme/weather","description":"Conditions for a named place.",
                "version":"1.4.0","remotes":[{{"type":"streamable-http",
                "url":"https://127.0.0.1:{port}/mcp"}}]}}"#
        ),
    )
    .unwrap();

    // A server only THIS machine runs is a line in the machine's own recipe — nothing is shared,
    // so there is nobody to ask and no record to mint, and `verify` reads the row the same way
    // the next `update` would.
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("topos.toml"),
        format!("[mcp]\nweather = \"{}\"\n", bundle.to_str().unwrap()),
    )
    .unwrap();

    let out = hermetic(&["--json", "verify", "weather"]);
    assert_eq!(
        out.status.code(),
        Some(4),
        "the verdict IS the exit code: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("non-JSON stdout: {}", String::from_utf8_lossy(&out.stdout)));
    assert_eq!(v["command"], "verify");
    assert_eq!(v["ok"], true, "a check that ran is a successful command");
    assert_eq!(v["data"]["name"], "weather");
    assert_eq!(v["data"]["target"], "address");
    assert_eq!(v["data"]["state"], "not_reachable");
    assert_eq!(v["data"]["exit_code"], 4);
    assert!(
        v["data"]["line"]
            .as_str()
            .is_some_and(|l| l.starts_with("not reachable: ")),
        "{}",
        v["data"]["line"]
    );

    // A refusal is not a verdict: exit 1, `ok: false`, and no payload at all.
    let out = hermetic(&["--json", "verify", "no-such-bundle"]);
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("an error envelope");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "NO_SUCH_SKILL");

    let _ = std::fs::remove_dir_all(&home);
}

// =================================================================================================
// The agents pick: `init -a`, `agents`, the pick rule, and the scopes with no pick.
// =================================================================================================

/// A hermetic machine for the pick tests: a `$HOME` holding exactly the agents `installed` names
/// (their detect dirs, empty), topos's own store at `$HOME/.topos`, and one git checkout beside
/// it. Every run stands in the checkout unless told otherwise.
struct PickRig {
    root: PathBuf,
    home: PathBuf,
    project: PathBuf,
}

fn detect_dir(slug: &str) -> &'static str {
    match slug {
        "claude-code" => ".claude",
        "codex" => ".codex",
        "cursor" => ".cursor",
        "gemini-cli" => ".gemini",
        "opencode" => ".config/opencode",
        other => panic!("no detect dir known for {other}"),
    }
}

fn pick_rig(tag: &str, installed: &[&str]) -> PickRig {
    let root = scratch(tag);
    let home = root.join("home");
    for slug in installed {
        std::fs::create_dir_all(home.join(detect_dir(slug))).unwrap();
    }
    let project = root.join("repo");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    PickRig {
        root,
        home,
        project,
    }
}

impl PickRig {
    fn topos_home(&self) -> PathBuf {
        self.home.join(".topos")
    }

    /// Run `topos` in `cwd` over this machine: `$HOME` is the rig's, `$TOPOS_HOME` its store,
    /// every per-harness override scrubbed, stdin closed (never a terminal).
    fn run_at(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
        let mut cmd = Command::new(bin());
        cmd.env("TOPOS_HOME", self.topos_home())
            .env("HOME", &self.home)
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("TOPOS_NO_UPDATE_CHECK", "1")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("CODEX_HOME")
            .env_remove("HERMES_HOME")
            .env_remove("VIBE_HOME")
            .env_remove("AUTOHAND_HOME")
            .env_remove("APPDATA")
            .env_remove("FLATPAK_XDG_CONFIG_HOME")
            .env_remove("CLAUDECODE")
            .env_remove("TOPOS_DEBUG")
            .stdin(std::process::Stdio::null())
            .current_dir(cwd)
            .args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().expect("spawn topos")
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.run_at(&self.project, args, &[])
    }

    fn stdout(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn json(&self, args: &[&str]) -> (std::process::ExitStatus, serde_json::Value) {
        let out = self.run(args);
        let value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
            panic!(
                "non-JSON stdout for {args:?}: {}",
                String::from_utf8_lossy(&out.stdout)
            )
        });
        (out.status, value)
    }

    /// **Claude Code's own configuration on this rig's machine** — where both its machine-wide
    /// and its per-checkout servers live. The rig isolates `$CLAUDE_CONFIG_DIR`, so the file sits
    /// inside it exactly as the agent puts it there.
    fn claude_json(&self) -> PathBuf {
        self.home.join(".claude").join(".claude.json")
    }

    /// The key Claude Code files this checkout's own servers under: its absolute path, symlinks
    /// resolved, which is what a process started there reports as its working directory.
    fn project_key(&self) -> String {
        self.project
            .canonicalize()
            .unwrap_or_else(|_| self.project.clone())
            .to_string_lossy()
            .into_owned()
    }

    fn project_pick(&self) -> Option<serde_json::Value> {
        std::fs::read(self.project.join(".topos").join("agents.json"))
            .ok()
            .map(|b| serde_json::from_slice(&b).unwrap())
    }

    fn machine_pick(&self) -> Option<serde_json::Value> {
        std::fs::read(self.topos_home().join("agents.json"))
            .ok()
            .map(|b| serde_json::from_slice(&b).unwrap())
    }

    fn write_machine_pick(&self, agents: &[&str]) {
        std::fs::create_dir_all(self.topos_home()).unwrap();
        std::fs::write(
            self.topos_home().join("agents.json"),
            serde_json::to_vec_pretty(&serde_json::json!({"schema_version": 1, "agents": agents}))
                .unwrap(),
        )
        .unwrap();
    }

    fn write_project_pick(&self, agents: &[&str]) {
        std::fs::create_dir_all(self.project.join(".topos")).unwrap();
        std::fs::write(
            self.project.join(".topos").join("agents.json"),
            serde_json::to_vec_pretty(&serde_json::json!({"schema_version": 1, "agents": agents}))
                .unwrap(),
        )
        .unwrap();
    }

    /// A project manifest with no rows (the template `init` writes), by hand.
    fn write_manifest(&self, body: &str) {
        std::fs::write(self.project.join("topos.toml"), body).unwrap();
    }

    /// Every file under `dir`, relative and sorted, minus anything under a `skip` prefix.
    fn files_under(dir: &Path, skip: &[&str]) -> Vec<String> {
        fn walk(base: &Path, d: &Path, skip: &[&str], out: &mut Vec<String>) {
            let Ok(rd) = std::fs::read_dir(d) else { return };
            for entry in rd {
                let entry = entry.unwrap();
                let path = entry.path();
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                if skip
                    .iter()
                    .any(|s| rel == *s || rel.starts_with(&format!("{s}/")))
                {
                    continue;
                }
                if entry.file_type().unwrap().is_dir() {
                    walk(base, &path, skip, out);
                } else {
                    out.push(rel);
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, skip, &mut out);
        out.sort();
        out
    }
}

fn agents_of(pick: &serde_json::Value) -> Vec<String> {
    pick["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_owned())
        .collect()
}

/// The spec's file set, exactly: `init -a claude-code -a codex` in a fresh checkout on a machine
/// with cursor installed too writes the manifest, the pick, the two picked agents' skills
/// folders (the built-in bundle, same bytes in both), their two project hooks, and NOTHING else —
/// nothing for cursor in the checkout, nothing under any agent dir in the home.
#[test]
fn init_with_agents_writes_exactly_the_spec_file_set() {
    let rig = pick_rig("init-spec", &["claude-code", "codex", "cursor"]);
    let home_before = PickRig::files_under(&rig.home, &[".topos"]);
    assert!(home_before.is_empty(), "the agent dirs start empty");

    let receipt = rig.stdout(&["init", "-a", "claude-code", "-a", "codex"]);
    assert_eq!(
        receipt,
        "Using Claude Code and Codex in this project.\n\
         Installed 1 skill to .claude/skills and .codex/skills.\n\
         Auto-update hooks: .claude/settings.local.json, .codex/hooks.json (Codex runs it once \
         you trust this repo).\n\
         New files are not ignored by git. To keep them out: topos init --gitignore\n\
         Other agents on this machine, untouched: cursor.\n"
    );

    // The checkout, minus topos's own store and git: exactly the spec table's files.
    let files = PickRig::files_under(&rig.project, &[".topos/state", ".git"]);
    let claude_copy: Vec<String> = files
        .iter()
        .filter(|f| f.starts_with(".claude/skills/topos/"))
        .map(|f| f.trim_start_matches(".claude/skills/topos/").to_owned())
        .collect();
    let codex_copy: Vec<String> = files
        .iter()
        .filter(|f| f.starts_with(".codex/skills/topos/"))
        .map(|f| f.trim_start_matches(".codex/skills/topos/").to_owned())
        .collect();
    assert!(
        claude_copy.contains(&"SKILL.md".to_owned()),
        "the built-in landed: {files:?}"
    );
    assert_eq!(claude_copy, codex_copy, "the same bytes into every folder");
    let rest: Vec<&String> = files
        .iter()
        .filter(|f| {
            !f.starts_with(".claude/skills/topos/") && !f.starts_with(".codex/skills/topos/")
        })
        .collect();
    assert_eq!(
        rest,
        [
            ".claude/settings.local.json",
            ".codex/hooks.json",
            ".topos/.gitignore",
            ".topos/agents.json",
            "topos.toml",
        ],
        "{files:?}"
    );
    assert!(
        !rig.project.join(".cursor").exists(),
        "an unpicked agent gets no folder"
    );
    assert!(
        !rig.project.join(".gitignore").exists(),
        "never edited unasked"
    );
    assert_eq!(
        agents_of(&rig.project_pick().unwrap()),
        ["claude-code", "codex"]
    );
    assert!(
        rig.machine_pick().is_none(),
        "a project pick writes no machine pick"
    );
    // Nothing under any agent dir in the home (the pick is the project's).
    assert!(
        PickRig::files_under(&rig.home, &[".topos"]).is_empty(),
        "{:?}",
        PickRig::files_under(&rig.home, &[".topos"])
    );
    // Idempotent: the same receipt, the same files.
    let again = rig.stdout(&["init", "-a", "claude-code", "-a", "codex"]);
    assert_eq!(again, receipt);
    assert_eq!(
        PickRig::files_under(&rig.project, &[".topos/state", ".git"]),
        files
    );
    let _ = std::fs::remove_dir_all(&rig.root);
}

#[test]
fn init_on_an_existing_manifest_only_adds_the_pick() {
    let rig = pick_rig("init-existing", &["claude-code", "cursor"]);
    let manifest = "# a teammate's file\nschema = 1\n";
    rig.write_manifest(manifest);
    let (status, v) = rig.json(&["--json", "init", "-a", "claude-code"]);
    assert!(status.success(), "{v}");
    assert_eq!(v["command"], "init");
    assert_eq!(v["data"]["created"], false, "{v}");
    assert_eq!(
        v["data"]["pick"]["agents"],
        serde_json::json!(["claude-code"])
    );
    assert_eq!(v["data"]["pick"]["scope"], "project");
    assert_eq!(
        v["data"]["pick"]["untouched"],
        serde_json::json!(["cursor"])
    );
    assert_eq!(
        std::fs::read_to_string(rig.project.join("topos.toml")).unwrap(),
        manifest,
        "the file stays byte-identical"
    );
    assert_eq!(agents_of(&rig.project_pick().unwrap()), ["claude-code"]);
    assert!(rig.project.join(".claude/skills/topos/SKILL.md").exists());
    assert!(rig.project.join(".claude/settings.local.json").exists());
    assert!(!rig.project.join(".cursor").exists());
    // A second agent JOINS the pick on a file of its own.
    let (status, v) = rig.json(&["--json", "init", "-a", "cursor"]);
    assert!(status.success(), "{v}");
    assert_eq!(
        agents_of(&rig.project_pick().unwrap()),
        ["claude-code", "cursor"]
    );
    assert!(rig.project.join(".cursor/skills/topos/SKILL.md").exists());
    let _ = std::fs::remove_dir_all(&rig.root);
}

#[test]
fn agents_lists_the_pick_and_the_installed_agents() {
    let rig = pick_rig(
        "agents-list",
        &["claude-code", "codex", "cursor", "gemini-cli"],
    );
    rig.stdout(&["init", "-a", "claude-code", "-a", "codex"]);
    assert_eq!(
        rig.stdout(&["agents"]),
        "This project (.topos/agents.json): claude-code, codex\n\
         Installed on this machine: claude-code, codex, cursor, gemini-cli\n\
         Change: topos agents add <agent> · topos agents remove <agent>\n"
    );
    let (status, v) = rig.json(&["--json", "agents"]);
    assert!(status.success());
    assert_eq!(v["command"], "agents");
    assert_eq!(v["data"]["scope"], "project");
    assert_eq!(v["data"]["source"], "project");
    assert_eq!(v["data"]["pick_path"], ".topos/agents.json");
    assert_eq!(
        v["data"]["agents"],
        serde_json::json!(["claude-code", "codex"])
    );
    assert_eq!(
        v["data"]["installed"],
        serde_json::json!(["claude-code", "codex", "cursor", "gemini-cli"])
    );
    let hooks: Vec<(&str, bool)> = v["data"]["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["agent"].as_str().unwrap(), h["armed"] == true))
        .collect();
    assert_eq!(hooks, [("claude-code", true), ("codex", true)], "{v}");
    // Outside any project: the machine pick, or the one line saying there is none.
    let out = rig.run_at(&rig.home, &["agents"], &[]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "This machine: no agents picked yet: topos init -g -a <agent>\n\
         Installed on this machine: claude-code, codex, cursor, gemini-cli\n"
    );
    let _ = std::fs::remove_dir_all(&rig.root);
}

#[test]
fn agents_add_extends_and_installs() {
    let rig = pick_rig("agents-add", &["claude-code", "codex", "cursor"]);
    rig.stdout(&["init", "-a", "claude-code"]);
    let receipt = rig.stdout(&["agents", "add", "cursor"]);
    assert!(
        receipt.starts_with("Using Claude Code and Cursor in this project.\n"),
        "{receipt}"
    );
    assert!(
        receipt.contains("Auto-update hooks: .claude/settings.local.json, .cursor/hooks.json"),
        "{receipt}"
    );
    assert!(
        receipt.contains("To keep them out: topos agents --gitignore"),
        "{receipt}"
    );
    assert!(
        receipt.ends_with("Other agents on this machine, untouched: codex.\n"),
        "{receipt}"
    );
    assert_eq!(
        agents_of(&rig.project_pick().unwrap()),
        ["claude-code", "cursor"]
    );
    assert!(rig.project.join(".cursor/skills/topos/SKILL.md").exists());
    let hooks: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.project.join(".cursor/hooks.json")).unwrap())
            .unwrap();
    assert_eq!(hooks["version"], 1, "{hooks}");
    assert!(!rig.project.join(".codex").exists());
    // Outside a project, `add` without `-g` has no file to edit.
    let (status, v) = {
        let out = rig.run_at(&rig.home, &["--json", "agents", "add", "codex"], &[]);
        (
            out.status,
            serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),
        )
    };
    assert!(!status.success());
    assert_eq!(v["error"]["code"], "NO_MANIFEST", "{v}");
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// A project with one MCP server row, picked for claude-code and codex.
fn mcp_project(rig: &PickRig) {
    let server = rig.project.join("weather");
    std::fs::create_dir_all(&server).unwrap();
    std::fs::write(
        server.join("server.json"),
        r#"{"name":"io.github.acme/weather","description":"Conditions for a named place.",
        "version":"1.4.0","remotes":[{"type":"streamable-http",
        "url":"https://weather.acme.example/mcp"}]}"#,
    )
    .unwrap();
    rig.write_manifest("schema = 1\n\n[mcp]\nweather = { path = \"./weather\", kind = \"mcp\" }\n");
}

#[test]
fn agents_remove_without_yes_describes_the_loss() {
    let rig = pick_rig("agents-remove-describe", &["claude-code", "codex"]);
    mcp_project(&rig);
    let receipt = rig.stdout(&["init", "-a", "claude-code", "-a", "codex"]);
    assert!(
        receipt.contains("1 MCP server to ~/.claude/.claude.json and .codex/config.toml"),
        "the server landed for the picked agents: {receipt}"
    );
    let before = PickRig::files_under(&rig.project, &[".git"]);

    let describe = rig.stdout(&["agents", "remove", "codex"]);
    assert!(
        describe.starts_with(
            "This removes codex from this project. It deletes:\n  .codex/skills/topos\n"
        ),
        "{describe}"
    );
    assert!(
        describe.contains("  .codex/hooks.json (the auto-update hook entry)\n"),
        "{describe}"
    );
    assert!(
        describe.ends_with("Re-run with --yes: topos agents remove codex --yes\n"),
        "{describe}"
    );
    assert_eq!(
        PickRig::files_under(&rig.project, &[".git"]),
        before,
        "a describe changes nothing"
    );
    assert_eq!(
        agents_of(&rig.project_pick().unwrap()),
        ["claude-code", "codex"]
    );
    let (status, v) = rig.json(&["--json", "agents", "remove", "codex"]);
    assert!(status.success(), "{v}");
    assert_eq!(v["data"]["describe"]["applied"], false, "{v}");
    assert_eq!(
        v["data"]["describe"]["removed"],
        serde_json::json!(["codex"])
    );
    assert_eq!(
        v["data"]["describe"]["agents"],
        serde_json::json!(["claude-code"])
    );
    assert_eq!(
        v["data"]["describe"]["removed_dirs"],
        serde_json::json!([".codex/skills/topos"])
    );
    assert_eq!(
        v["data"]["describe"]["hooks"],
        serde_json::json!([".codex/hooks.json"])
    );
    assert_eq!(v["next_actions"][0]["code"], "APPLY_DESCRIBED");
    assert_eq!(
        v["next_actions"][0]["argv"],
        serde_json::json!(["topos", "agents", "remove", "codex", "--yes"])
    );
    let _ = std::fs::remove_dir_all(&rig.root);
}

#[test]
fn agents_remove_deletes_the_hook_the_entries_and_the_copies() {
    let rig = pick_rig("agents-remove-apply", &["claude-code", "codex"]);
    mcp_project(&rig);
    rig.stdout(&["init", "-a", "claude-code", "-a", "codex"]);
    let claude_json = std::fs::read_to_string(rig.claude_json()).unwrap();
    assert!(claude_json.contains("weather"), "{claude_json}");
    let codex_config = rig.project.join(".codex/config.toml");
    let codex_had_entry = std::fs::read_to_string(&codex_config)
        .map(|t| t.contains("weather"))
        .unwrap_or(false);

    let receipt = rig.stdout(&["agents", "remove", "codex", "--yes"]);
    assert!(
        receipt.starts_with("Removed codex from this project. Deleted:\n  .codex/skills/topos\n"),
        "{receipt}"
    );
    assert!(
        receipt.contains("  .codex/hooks.json (the auto-update hook entry)\n"),
        "{receipt}"
    );
    assert!(
        receipt.ends_with("Agents in this project: claude-code\n"),
        "{receipt}"
    );
    if codex_had_entry {
        assert!(
            receipt.contains("the MCP entries in .codex/config.toml"),
            "{receipt}"
        );
        assert!(
            !std::fs::read_to_string(&codex_config)
                .map(|t| t.contains("weather"))
                .unwrap_or(false),
            "codex's entry left"
        );
    }
    assert!(
        !rig.project.join(".codex/skills/topos").exists(),
        "the copy is gone"
    );
    // The hook entry leaves — and with it the file and the folder, because everything in them
    // was topos's own.
    assert!(!rig.project.join(".codex/hooks.json").exists());
    assert!(!rig.project.join(".codex").exists(), "no empty folder left");
    assert!(
        rig.project.join(".claude/skills/topos/SKILL.md").exists(),
        "claude-code keeps its copy"
    );
    assert!(rig.project.join(".claude/settings.local.json").exists());
    assert_eq!(
        std::fs::read_to_string(rig.claude_json()).unwrap(),
        claude_json,
        "claude-code's entry stays byte-identical"
    );
    assert_eq!(agents_of(&rig.project_pick().unwrap()), ["claude-code"]);
    // Removing the last picked agent leaves an empty pick, spelled out.
    let receipt = rig.stdout(&["agents", "remove", "claude-code", "--yes"]);
    assert!(
        receipt.ends_with("No agents picked for this project now.\n"),
        "{receipt}"
    );
    assert!(!rig.project.join(".claude/skills/topos").exists());
    assert!(!rig.project.join(".claude/settings.local.json").exists());
    assert!(
        !rig.project.join(".claude").exists(),
        "no empty folder left"
    );
    assert!(
        !std::fs::read_to_string(rig.claude_json()).is_ok_and(|t| t.contains(&rig.project_key())),
        "the checkout's own slot leaves with its last entry"
    );
    assert!(agents_of(&rig.project_pick().unwrap()).is_empty());
    // A slug not in the pick is refused by name; nothing changes.
    let (status, v) = rig.json(&["--json", "agents", "remove", "cursor"]);
    assert!(!status.success());
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT", "{v}");
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// Taking an agent out leaves no folder topos made standing empty — and never touches one
/// holding anything of the person's. `git status` after a remove is as clean as before the add.
#[test]
fn agents_remove_leaves_no_trace_of_a_folder_topos_created() {
    let rig = pick_rig("agents-remove-no-trace", &["cursor"]);
    // The project: `.cursor` did not exist here at all before the add.
    rig.stdout(&["init", "-a", "cursor"]);
    assert!(rig.project.join(".cursor/skills/topos/SKILL.md").is_file());
    assert!(rig.project.join(".cursor/hooks.json").is_file());
    rig.stdout(&["agents", "remove", "cursor", "--yes"]);
    assert!(
        !rig.project.join(".cursor").exists(),
        "the folder topos made is gone whole: {:?}",
        PickRig::files_under(&rig.project, &[".git"])
    );

    // The machine, the same way.
    let out = rig.run_at(&rig.home, &["init", "-g", "-a", "cursor"], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(rig.home.join(".cursor/hooks.json").is_file());
    let out = rig.run_at(
        &rig.home,
        &["agents", "remove", "-g", "cursor", "--yes"],
        &[],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!rig.home.join(".cursor").exists());
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// The other half of the same rule: a file or folder holding anything that is not topos's is
/// edited, never deleted.
#[test]
fn agents_remove_keeps_a_folder_holding_anything_of_yours() {
    let rig = pick_rig("agents-remove-keeps-yours", &["cursor"]);
    rig.stdout(&["init", "-a", "cursor"]);
    // A hook of the person's own, beside topos's, in the file topos wrote.
    let hooks = rig.project.join(".cursor/hooks.json");
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
    doc["hooks"]["sessionStart"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"command": "echo mine"}));
    std::fs::write(&hooks, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    // And a file of theirs in the skills folder topos delivers into.
    std::fs::write(rig.project.join(".cursor/skills/NOTES.md"), "mine\n").unwrap();

    rig.stdout(&["agents", "remove", "cursor", "--yes"]);
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
    let entries = doc["hooks"]["sessionStart"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "only topos's entry left: {doc}");
    assert_eq!(entries[0]["command"], "echo mine");
    assert_eq!(
        std::fs::read_to_string(rig.project.join(".cursor/skills/NOTES.md")).unwrap(),
        "mine\n",
        "their file, and the folder holding it, stay"
    );
    assert!(!rig.project.join(".cursor/skills/topos").exists());
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// **A project's pick is its own, and `agents remove` says so.** With a machine pick standing and
/// no project file, this checkout picks nobody: there is no agent here to remove, and the machine
/// pick is never edited from inside a project.
#[test]
fn agents_remove_in_a_project_with_no_pick_of_its_own_refuses() {
    let rig = pick_rig(
        "agents-remove-no-project-pick",
        &["claude-code", "codex", "cursor"],
    );
    rig.write_machine_pick(&["claude-code", "codex"]);
    rig.write_manifest("schema = 1\n");
    assert!(rig.project_pick().is_none());
    let out = rig.run(&["agents", "remove", "codex"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("codex is not one of this project's agents; `topos agents` lists them"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(rig.project_pick().is_none(), "no file was minted");
    assert_eq!(
        agents_of(&rig.machine_pick().unwrap()),
        ["claude-code", "codex"],
        "the machine pick is untouched"
    );
    // `-g` is how the machine's own list is edited.
    rig.stdout(&["agents", "remove", "-g", "codex", "--yes"]);
    assert_eq!(agents_of(&rig.machine_pick().unwrap()), ["claude-code"]);
    let _ = std::fs::remove_dir_all(&rig.root);
}

#[test]
fn agents_remove_on_a_wildcard_pick_materializes_first() {
    let rig = pick_rig(
        "agents-remove-wildcard",
        &["claude-code", "codex", "cursor"],
    );
    rig.write_manifest("schema = 1\n");
    rig.write_project_pick(&["*"]);
    let receipt = rig.stdout(&["agents", "remove", "cursor"]);
    assert!(
        receipt.starts_with("Removed cursor from this project."),
        "{receipt}"
    );
    assert_eq!(
        agents_of(&rig.project_pick().unwrap()),
        ["claude-code", "codex"],
        "the wildcard is spelled out as what is installed, minus the removed agent"
    );
    let _ = std::fs::remove_dir_all(&rig.root);
}

#[cfg(unix)]
#[test]
fn agents_remove_leaves_the_pick_when_cleanup_fails() {
    use std::os::unix::fs::PermissionsExt;
    let rig = pick_rig("agents-remove-fails", &["claude-code", "codex"]);
    rig.stdout(&["init", "-a", "claude-code", "-a", "codex"]);
    // The copy's parent is made read-only, so deleting the copy fails.
    let agents_dir = rig.project.join(".codex/skills");
    let copy = agents_dir.join("topos");
    assert!(copy.exists());
    std::fs::set_permissions(&agents_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = rig.run(&["agents", "remove", "codex", "--yes"]);
    let restore = || {
        let _ = std::fs::set_permissions(&agents_dir, std::fs::Permissions::from_mode(0o755));
    };
    if out.status.success() {
        restore();
        panic!(
            "the removal must fail when a copy cannot be deleted: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    restore();
    assert_eq!(
        agents_of(&rig.project_pick().unwrap()),
        ["claude-code", "codex"],
        "the pick is written last, so a failed cleanup leaves it: {stderr}"
    );
    assert!(copy.exists(), "the copy is still there");
    assert!(!stderr.is_empty(), "the failure is said");
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// A hook removal that DEGRADES is an incomplete cleanup, never a silent one: the entry may
/// still be live in a file topos could not parse (or reach through the rail), and the presence
/// probe fails closed there, so it would say nothing. The pick stays, the receipt names the file
/// and the reason, and the run fails.
#[test]
fn agents_remove_keeps_the_pick_when_a_hook_removal_degrades() {
    let rig = pick_rig("agents-remove-degrades", &["claude-code", "cursor"]);
    rig.stdout(&["init", "-a", "claude-code", "-a", "cursor"]);
    let hook = rig.project.join(".cursor/hooks.json");
    assert!(std::fs::read_to_string(&hook).unwrap().contains("topos"));
    // The hook file is no longer JSON: nothing in it can be proven, or removed.
    let malformed = b"{ not json\n";
    std::fs::write(&hook, malformed).unwrap();

    let (status, v) = rig.json(&["--json", "agents", "remove", "cursor", "--yes"]);
    assert!(!status.success(), "{v}");
    assert_eq!(v["error"]["code"], "AGENT_REMOVE_INCOMPLETE", "{v}");
    let out = rig.run(&["agents", "remove", "cursor", "--yes"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("cursor stays picked"),
        "the pick stays, said: {stderr}"
    );
    assert!(
        stderr.contains(".cursor/hooks.json (topos could not prove which entry in it is its own)"),
        "the file and the reason: {stderr}"
    );
    assert_eq!(
        agents_of(&rig.project_pick().unwrap()),
        ["claude-code", "cursor"],
        "the pick is unchanged: {stderr}"
    );
    assert_eq!(
        std::fs::read(&hook).unwrap(),
        malformed,
        "a file topos cannot read is never written"
    );
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// A slug the harness table no longer knows (a row a newer downloaded table dropped) is kept in
/// the pick as written: `topos agents` marks it, an `add` beside it is never blocked by it, and
/// `agents remove` can always take it out. Only the slugs a verb ADDS are checked against the
/// table.
#[test]
fn a_slug_the_table_dropped_can_still_be_removed_and_never_blocks_an_add() {
    let rig = pick_rig("dropped-slug", &["claude-code", "codex", "cursor"]);
    rig.write_manifest("schema = 1\n");
    rig.write_project_pick(&["claude-code", "old-agent"]);
    let listing = rig.stdout(&["agents"]);
    assert!(
        listing.starts_with(
            "This project (.topos/agents.json): claude-code, old-agent (not in this table)\n"
        ),
        "{listing}"
    );
    let (status, v) = rig.json(&["--json", "agents"]);
    assert!(status.success());
    assert_eq!(
        v["data"]["agents"],
        serde_json::json!(["claude-code", "old-agent"])
    );
    assert_eq!(v["data"]["not_in_table"], serde_json::json!(["old-agent"]));

    rig.stdout(&["agents", "add", "codex"]);
    assert_eq!(
        agents_of(&rig.project_pick().unwrap()),
        ["claude-code", "old-agent", "codex"],
        "the add lands beside the dropped slug"
    );
    // A slug the table does not know that is NOT in the pick is still refused on an add.
    let (status, v) = rig.json(&["--json", "agents", "add", "never-here"]);
    assert!(!status.success());
    assert_eq!(v["error"]["code"], "UNKNOWN_AGENT", "{v}");

    let receipt = rig.stdout(&["agents", "remove", "old-agent", "--yes"]);
    assert!(
        receipt.starts_with("Removed old-agent from this project."),
        "{receipt}"
    );
    assert_eq!(
        agents_of(&rig.project_pick().unwrap()),
        ["claude-code", "codex"]
    );
    // One that is neither in the table nor in the pick is refused by name, as before.
    let (status, v) = rig.json(&["--json", "agents", "remove", "never-here"]);
    assert!(!status.success());
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT", "{v}");
    // The machine file the same way.
    rig.write_machine_pick(&["cursor", "old-agent"]);
    let out = rig.run_at(
        &rig.home,
        &["agents", "remove", "-g", "old-agent", "--yes"],
        &[],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(agents_of(&rig.machine_pick().unwrap()), ["cursor"]);
    let _ = std::fs::remove_dir_all(&rig.root);
}

#[test]
fn gitignore_append_is_idempotent() {
    let rig = pick_rig("gitignore", &["claude-code", "codex"]);
    std::fs::write(rig.project.join(".gitignore"), "node_modules\n").unwrap();
    let receipt = rig.stdout(&["init", "-a", "claude-code", "-a", "codex", "--gitignore"]);
    assert!(
        receipt.contains("Added to .gitignore: .claude/, .codex/.\n"),
        "{receipt}"
    );
    assert!(!receipt.contains("not ignored by git"), "{receipt}");
    let expected = "node_modules\n.claude/\n.codex/\n";
    assert_eq!(
        std::fs::read_to_string(rig.project.join(".gitignore")).unwrap(),
        expected
    );
    // Again: nothing appended, nothing said about it, and no hint either (they are ignored).
    let again = rig.stdout(&["init", "-a", "claude-code", "-a", "codex", "--gitignore"]);
    assert!(!again.contains("Added to .gitignore"), "{again}");
    assert!(!again.contains("not ignored by git"), "{again}");
    assert_eq!(
        std::fs::read_to_string(rig.project.join(".gitignore")).unwrap(),
        expected
    );
    // `topos agents --gitignore` is the same append over the standing pick.
    let listed = rig.stdout(&["agents", "--gitignore"]);
    assert!(!listed.contains("Added to .gitignore"), "{listed}");
    assert_eq!(
        std::fs::read_to_string(rig.project.join(".gitignore")).unwrap(),
        expected
    );
    assert!(
        !rig.project.join(".git/info/exclude").exists(),
        "never touched"
    );
    let _ = std::fs::remove_dir_all(&rig.root);
}

#[test]
fn gitignore_is_refused_with_global() {
    let rig = pick_rig("gitignore-global", &["claude-code"]);
    let (status, v) = rig.json(&["--json", "init", "-g", "-a", "claude-code", "--gitignore"]);
    assert!(!status.success());
    assert_eq!(v["error"]["code"], "GITIGNORE_NEEDS_PROJECT", "{v}");
    assert!(
        !rig.topos_home().join("topos.toml").exists(),
        "nothing was written"
    );
    assert!(rig.machine_pick().is_none());
    let (status, v) = rig.json(&["--json", "agents", "-g", "--gitignore"]);
    assert!(!status.success());
    assert_eq!(v["error"]["code"], "GITIGNORE_NEEDS_PROJECT", "{v}");
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// **The refusal names the installed agents and the command.** Two lines, byte for byte, and the
/// same two whether a person or an agent typed it — topos never prompts. The `-g` spelling is the
/// machine scope's own.
#[test]
fn the_refusal_names_the_installed_agents_and_the_command() {
    let rig = pick_rig("refusal-copy", &["claude-code", "codex", "cursor"]);
    let out = rig.run(&["init"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Several agents are installed here: claude-code, codex, cursor.\n\
         Pick with: topos init -a claude-code [-a codex]\n"
    );
    assert!(out.stdout.is_empty());
    assert!(
        !rig.project.join("topos.toml").exists(),
        "nothing is written on a refusal"
    );
    assert!(rig.project_pick().is_none());
    // The machine scope: the same first line, its own second.
    let out = rig.run(&["init", "-g"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Several agents are installed here: claude-code, codex, cursor.\n\
         Pick with: topos init -g -a claude-code [-a codex]\n"
    );
    assert!(rig.machine_pick().is_none());
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// The `--json` envelope is unchanged: the code, the installed list, the one command, and the
/// runnable `PICK_AGENTS` action — all at exit 2.
#[test]
fn the_json_refusal_carries_the_list_and_the_command() {
    let rig = pick_rig("refusal-json", &["claude-code", "cursor"]);
    let (status, v) = rig.json(&["--json", "init"]);
    assert_eq!(status.code(), Some(2));
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "PICK_REQUIRED", "{v}");
    assert_eq!(
        v["data"],
        serde_json::json!({"installed": ["claude-code", "cursor"], "next": "topos init -a <agent>"})
    );
    assert_eq!(v["next_actions"][0]["code"], "PICK_AGENTS");
    assert_eq!(
        v["next_actions"][0]["argv"],
        serde_json::json!(["topos", "init", "-a", "<agent>"])
    );
    // The machine scope spells its way out with `-g`.
    let (status, v) = rig.json(&["--json", "init", "-g"]);
    assert_eq!(status.code(), Some(2));
    assert_eq!(v["data"]["next"], "topos init -g -a <agent>", "{v}");
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// **Every entry point answers the same way.** `init`, `install`, `update` and `add` all record a
/// decision before they place anything, so with several agents installed and none picked here
/// each one exits 2 with the same two lines and writes nothing.
#[test]
fn several_agents_and_no_pick_exits_2_on_every_entry_point() {
    let rig = pick_rig("no-pick-every-verb", &["claude-code", "codex", "cursor"]);
    rig.write_manifest("schema = 1\n");
    let expected = "Several agents are installed here: claude-code, codex, cursor.\n\
                    Pick with: topos init -a claude-code [-a codex]\n";
    let source = rig.root.join("a-skill");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("SKILL.md"), "# a\n").unwrap();
    let source = source.display().to_string();
    for args in [
        vec!["init"],
        vec!["install"],
        vec!["update"],
        vec!["add", source.as_str()],
    ] {
        let out = rig.run(&args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stderr), expected, "{args:?}");
        assert!(rig.project_pick().is_none(), "{args:?}");
        assert!(rig.machine_pick().is_none(), "{args:?}");
        assert!(
            PickRig::files_under(&rig.project, &[".git"]) == ["topos.toml"],
            "{args:?}: nothing was placed"
        );
    }
    let _ = std::fs::remove_dir_all(&rig.root);
}

#[test]
fn inside_claude_code_the_rule_picks_claude_code_silently() {
    let rig = pick_rig("ask-claudecode", &["claude-code", "cursor"]);
    let out = rig.run_at(&rig.project, &["init"], &[("CLAUDECODE", "1")]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("Using Claude Code in this project.\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Other agents on this machine, untouched: cursor.\n"),
        "{stdout}"
    );
    assert_eq!(agents_of(&rig.project_pick().unwrap()), ["claude-code"]);
    assert!(rig.project.join("topos.toml").exists());
    assert!(!rig.project.join(".cursor").exists());
    let _ = std::fs::remove_dir_all(&rig.root);
}

#[test]
fn one_installed_agent_is_still_used_and_said() {
    let rig = pick_rig("ask-one", &["cursor"]);
    let receipt = rig.stdout(&["init"]);
    assert!(
        receipt.starts_with("Using Cursor in this project.\n"),
        "{receipt}"
    );
    assert!(
        receipt.contains("Auto-update hooks: .cursor/hooks.json.\n"),
        "{receipt}"
    );
    assert!(!receipt.contains("untouched"), "{receipt}");
    assert_eq!(agents_of(&rig.project_pick().unwrap()), ["cursor"]);
    assert!(rig.project.join(".cursor/skills/topos/SKILL.md").exists());
    // The same rule for the machine scope: `init -g` on a one-agent machine picks it.
    let receipt = rig.stdout(&["init", "-g"]);
    assert!(
        receipt.starts_with("Using Cursor on this machine.\n"),
        "{receipt}"
    );
    assert_eq!(agents_of(&rig.machine_pick().unwrap()), ["cursor"]);
    assert!(rig.home.join(".cursor/skills/topos/SKILL.md").exists());
    assert!(rig.home.join(".cursor/hooks.json").exists());
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// **A teammate's fresh clone picks for itself.** A machine-wide pick is that person's answer for
/// their machine, not for a checkout they have never picked agents for: `install` there refuses
/// with the two lines and places nothing. `init -a` is what makes the clone a scope of its own,
/// and then the same `install` lands everything.
#[test]
fn a_fresh_clone_picks_for_itself() {
    let rig = pick_rig("fresh-clone", &["claude-code", "cursor"]);
    rig.write_machine_pick(&["claude-code"]);
    // A clone: the manifest names one machine-local MCP server (the one bundle a checkout
    // converges from its own bytes, no workspace needed); no project pick of its own.
    mcp_project(&rig);
    let out = rig.run(&["install"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Several agents are installed here: claude-code, cursor.\n\
         Pick with: topos init -a claude-code [-a cursor]\n"
    );
    assert!(
        !rig.project.join(".mcp.json").exists() && !rig.project.join(".cursor").exists(),
        "nothing is placed in a checkout with no pick"
    );
    assert!(
        rig.project_pick().is_none(),
        "and no pick is invented for it"
    );
    assert_eq!(
        agents_of(&rig.machine_pick().unwrap()),
        ["claude-code"],
        "the machine pick is untouched"
    );
    // The clone picks, and the same `install` lands the server entry.
    rig.stdout(&["init", "-a", "claude-code"]);
    rig.stdout(&["install"]);
    let claude_json = std::fs::read_to_string(rig.claude_json())
        .expect("the project's own pick gets the project's server entry");
    assert!(
        claude_json.contains("weather") && claude_json.contains(&rig.project_key()),
        "the entry is in THIS checkout's own slot: {claude_json}"
    );
    assert!(
        !rig.project.join(".mcp.json").exists(),
        "and nothing is written in the repo root"
    );
    assert!(
        !rig.project.join(".cursor").exists(),
        "the unpicked agent gets nothing"
    );
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// **A project never inherits the machine pick.** The machine names cursor; the checkout has no
/// pick of its own. `init` there refuses, writes nothing, and leaves no cursor folder anywhere in
/// the project.
#[test]
fn a_project_never_inherits_the_machine_pick() {
    let rig = pick_rig("no-inherit", &["claude-code", "cursor"]);
    rig.write_machine_pick(&["cursor"]);
    let out = rig.run(&["init"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Several agents are installed here: claude-code, cursor.\n\
         Pick with: topos init -a claude-code [-a cursor]\n"
    );
    assert!(
        !rig.project.join("topos.toml").exists(),
        "no manifest was born"
    );
    assert!(rig.project_pick().is_none());
    assert!(
        PickRig::files_under(&rig.project, &[".git"]).is_empty(),
        "not a byte in the checkout: {:?}",
        PickRig::files_under(&rig.project, &[".git"])
    );
    assert!(
        !rig.project.join(".cursor").exists(),
        "the machine's agent is not the project's"
    );
    assert_eq!(agents_of(&rig.machine_pick().unwrap()), ["cursor"]);
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// The PROJECT SWEEP keeps the built-in in place for the checkout's picked agents. No manifest
/// row names it, so a sweep that judged it by rows would retire it as undemanded: after
/// `init -a claude-code -a codex` and `agents remove codex`, a plain `install` and the hook's
/// `install --quiet` both leave Claude Code's copy standing, name no removal, and put back a copy
/// that went missing. The same holds with no `agents remove` at all.
#[test]
fn a_project_sweep_keeps_the_built_in_for_the_pick() {
    let rig = pick_rig("sweep-keeps-builtin", &["claude-code", "codex"]);
    rig.stdout(&["init", "-a", "claude-code", "-a", "codex"]);
    let claude_copy = rig.project.join(".claude/skills/topos/SKILL.md");
    let codex_copy = rig.project.join(".codex/skills/topos/SKILL.md");
    assert!(claude_copy.is_file() && codex_copy.is_file());

    // No remove yet: the sweep after an init is already the shape that must keep the copies.
    let receipt = rig.stdout(&["install"]);
    assert!(!receipt.contains("removed"), "{receipt}");
    assert!(claude_copy.is_file() && codex_copy.is_file(), "{receipt}");

    rig.stdout(&["agents", "remove", "codex", "--yes"]);
    assert!(claude_copy.is_file(), "the remaining agent keeps its copy");
    assert!(!codex_copy.exists(), "the leaving agent's copy is gone");

    let receipt = rig.stdout(&["install"]);
    assert!(!receipt.contains("removed"), "{receipt}");
    assert!(
        claude_copy.is_file(),
        "a plain install keeps the built-in for the pick: {receipt}"
    );
    assert!(
        !codex_copy.exists(),
        "and does not bring the removed one back"
    );
    let (status, v) = rig.json(&["--json", "install"]);
    assert!(status.success(), "{v}");
    let rows = v["data"]["skills"].as_array().cloned().unwrap_or_default();
    assert!(
        !rows
            .iter()
            .any(|r| r["skill"] == "topos" || r["action"] == "removed"),
        "{rows:?}"
    );

    let out = rig.run(&["install", "--quiet", "--ttl", "0", "--hook", "claude-code"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        claude_copy.is_file(),
        "the hook's sweep keeps it too: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!codex_copy.exists());

    // The sweep re-converges it: a copy that went missing comes back.
    std::fs::remove_dir_all(rig.project.join(".claude/skills/topos")).unwrap();
    rig.stdout(&["install"]);
    assert!(claude_copy.is_file(), "the sweep put the copy back");
    assert!(!codex_copy.exists());
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// `remove topos` acts at the store the pick stands in where you are: inside a checkout with a
/// pick of its own it takes the PROJECT copies and records the opt-out in the project store
/// (the next sweep re-places nothing there); `-g` keeps the machine's. `add topos` restores at
/// the same scope.
#[test]
fn remove_topos_in_a_checkout_with_its_own_pick_acts_on_the_project() {
    let rig = pick_rig("remove-topos-project", &["claude-code"]);
    let out = rig.run_at(&rig.home, &["init", "-g", "-a", "claude-code"], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    rig.stdout(&["init", "-a", "claude-code"]);
    let home_copy = rig.home.join(".claude/skills/topos/SKILL.md");
    let project_copy = rig.project.join(".claude/skills/topos/SKILL.md");
    assert!(home_copy.is_file() && project_copy.is_file());

    // The describe names the project's copy, not the home's.
    let (status, v) = rig.json(&["--json", "remove", "topos"]);
    assert!(status.success(), "{v}");
    let dirs = v["data"]["describe"]["items"][0]["dest_dirs"].clone();
    assert_eq!(
        dirs,
        serde_json::json!([rig.project.join(".claude/skills/topos").to_string_lossy()]),
        "{v}"
    );
    let receipt = rig.stdout(&["remove", "topos", "--yes"]);
    assert!(receipt.starts_with("Removed 'topos'"), "{receipt}");
    assert!(!project_copy.exists(), "the project copy is gone");
    assert!(home_copy.is_file(), "the machine's copy stays");
    // The opt-out is the project's: neither sweep brings the copy back.
    rig.stdout(&["install"]);
    let out = rig.run(&["install", "--quiet", "--ttl", "0", "--hook", "claude-code"]);
    assert!(out.status.success());
    assert!(
        !project_copy.exists(),
        "the project sweep honors the opt-out"
    );
    assert!(home_copy.is_file());

    // `-g` from the same checkout is the machine's opt-out.
    let receipt = rig.stdout(&["remove", "-g", "topos", "--yes"]);
    assert!(receipt.starts_with("Removed 'topos'"), "{receipt}");
    assert!(!home_copy.exists(), "the machine's copy is gone");

    // The restores, each at its own scope.
    let receipt = rig.stdout(&["add", "topos"]);
    assert!(receipt.contains(".claude/skills/topos"), "{receipt}");
    assert!(project_copy.is_file(), "the project copy is back");
    assert!(!home_copy.exists(), "the machine's opt-out stands");
    rig.stdout(&["add", "-g", "topos"]);
    assert!(home_copy.is_file(), "the machine's copy is back");
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// `uninstall --yes` scrubs the hooks of the checkout it ran in; another checkout keeps its hook
/// (the receipt says so), and the `topos install --quiet` that hook fires is INERT once the
/// machine store is gone: exit 0, nothing on either stream, nothing minted anywhere.
#[test]
fn the_quiet_sweep_mints_nothing_after_an_uninstall() {
    let rig = pick_rig("quiet-after-uninstall", &["claude-code"]);
    rig.stdout(&["init", "-a", "claude-code"]);
    let other = rig.root.join("other");
    std::fs::create_dir_all(other.join(".git")).unwrap();
    let out = rig.run_at(&other, &["init", "-a", "claude-code"], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let repo_hook = rig.project.join(".claude/settings.local.json");
    let other_hook = other.join(".claude/settings.local.json");
    assert!(
        std::fs::read_to_string(&repo_hook)
            .unwrap()
            .contains("topos install --quiet")
    );
    assert!(
        std::fs::read_to_string(&other_hook)
            .unwrap()
            .contains("topos install --quiet")
    );

    let out = rig.run(&["uninstall", "--yes"]);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!rig.topos_home().exists(), "the store is gone");
    assert!(
        !repo_hook.exists(),
        "the checkout the command ran in loses its hook entry, and the file that held nothing \
         else goes with it"
    );
    assert!(
        rig.project.join(".claude/skills/topos/SKILL.md").is_file(),
        "uninstall still deletes no skill byte, so the folder around them stays"
    );
    assert!(
        stdout.contains(&format!(
            "claude-code: scrubbed this checkout's auto-update hook from {}",
            repo_hook.display()
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "Other checkouts you set up keep their auto-update hook until you run `topos agents remove` there."
        ),
        "{stdout}"
    );
    assert!(
        std::fs::read_to_string(&other_hook)
            .unwrap()
            .contains("topos install --quiet"),
        "the other checkout keeps its hook"
    );

    // The other checkout's hook fires. With no store there is nothing to do and nothing to say.
    let other_before = PickRig::files_under(&other, &[]);
    let home_before = PickRig::files_under(&rig.home, &[]);
    let out = rig.run_at(
        &other,
        &["install", "--quiet", "--ttl", "0", "--hook", "claude-code"],
        &[],
    );
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!rig.topos_home().exists(), "the sweep minted no store");
    assert_eq!(PickRig::files_under(&other, &[]), other_before);
    assert_eq!(PickRig::files_under(&rig.home, &[]), home_before);
    let _ = std::fs::remove_dir_all(&rig.root);
}

#[test]
fn install_in_a_project_with_no_pick_exits_2_naming_init() {
    let rig = pick_rig("install-no-pick", &["claude-code", "cursor"]);
    rig.write_manifest("schema = 1\n");
    let out = rig.run(&["install"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Several agents are installed here: claude-code, cursor.\n\
         Pick with: topos init -a claude-code [-a cursor]\n"
    );
    let (status, v) = rig.json(&["--json", "update"]);
    assert_eq!(status.code(), Some(2));
    assert_eq!(v["error"]["code"], "PICK_REQUIRED", "{v}");
    assert_eq!(v["data"]["next"], "topos init -a <agent>");
    assert!(rig.project_pick().is_none() && rig.machine_pick().is_none());
    assert!(!rig.project.join(".claude").exists() && !rig.project.join(".cursor").exists());
    let _ = std::fs::remove_dir_all(&rig.root);
}

#[test]
fn the_quiet_sweep_with_no_pick_stays_silent() {
    let rig = pick_rig("quiet-no-pick", &["claude-code", "cursor"]);
    rig.write_manifest("schema = 1\n");
    let out = rig.run(&["update", "--quiet", "--ttl", "0", "--hook", "claude-code"]);
    assert!(
        out.status.success(),
        "a session-start sweep always exits 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("picked"),
        "nothing is asked: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(rig.project_pick().is_none() && rig.machine_pick().is_none());
    assert!(!rig.project.join(".claude").exists() && !rig.project.join(".cursor").exists());
    assert!(
        PickRig::files_under(&rig.home, &[".topos"]).is_empty(),
        "nothing placed under the home either"
    );
    let _ = std::fs::remove_dir_all(&rig.root);
}

#[test]
fn status_lists_only_picked_agents() {
    let rig = pick_rig("status-picked", &["claude-code", "cursor"]);
    rig.stdout(&["init", "-a", "claude-code"]);
    let tty = rig.stdout(&["status"]);
    assert!(
        tty.contains("\nAgents (this project, .topos/agents.json): claude-code\n"),
        "{tty}"
    );
    assert!(
        tty.contains("auto-update triggers:\n  claude-code: "),
        "{tty}"
    );
    assert!(
        !tty.contains("cursor"),
        "an unpicked agent is not a row: {tty}"
    );
    let (status, v) = rig.json(&["--json", "status"]);
    assert!(status.success());
    assert_eq!(
        v["data"]["agents"],
        serde_json::json!(["claude-code"]),
        "{v}"
    );
    assert_eq!(v["data"]["agents_source"], "project");
    assert_eq!(v["data"]["agents_path"], ".topos/agents.json");
    let rows: Vec<&str> = v["data"]["triggers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["agent"].as_str().unwrap())
        .collect();
    assert_eq!(rows, ["claude-code"]);
    // With no pick at all, the panel says so and names the command.
    let bare = pick_rig("status-no-pick", &["claude-code", "cursor"]);
    bare.write_manifest("schema = 1\n");
    let tty = bare.stdout(&["status"]);
    assert!(
        tty.contains("\nAgents (this project): no agents picked yet: topos init -a <agent>\n"),
        "{tty}"
    );
    assert!(!tty.contains("auto-update triggers"), "{tty}");
    let tty = bare.stdout(&["status", "-g"]);
    assert!(
        tty.contains("\nAgents (this machine): no agents picked yet: topos init -g -a <agent>\n"),
        "{tty}"
    );
    // A MACHINE pick standing beside a checkout that has none: the panel says what governs HERE,
    // never the machine's list under a project heading.
    bare.write_machine_pick(&["cursor"]);
    let tty = bare.stdout(&["status"]);
    assert!(
        tty.contains("\nAgents (this project): no agents picked yet: topos init -a <agent>\n"),
        "{tty}"
    );
    assert!(!tty.contains("cursor"), "{tty}");
    let tty = bare.stdout(&["status", "-g"]);
    assert!(
        tty.contains("\nAgents (this machine, ~/.topos/agents.json): cursor\n"),
        "{tty}"
    );
    let _ = std::fs::remove_dir_all(&rig.root);
    let _ = std::fs::remove_dir_all(&bare.root);
}

/// `add -a <agent>` NAMES AGENTS THIS SCOPE PICKED — it narrows the pick, never widens it. An
/// agent outside it refuses with the command that picks it, on both surfaces, and nothing lands
/// in that agent's folder.
#[test]
fn add_dash_a_outside_the_pick_refuses_naming_agents_add() {
    let rig = pick_rig("add-not-picked", &["claude-code", "codex"]);
    rig.stdout(&["init", "-a", "claude-code"]);
    let src = rig.project.join("src/writer");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("SKILL.md"),
        "---\nname: writer\ndescription: writes it down\n---\n\nWrite it down.\n",
    )
    .unwrap();

    let (status, v) = rig.json(&["--json", "add", "./src/writer", "-a", "codex"]);
    assert!(!status.success(), "{v}");
    assert_eq!(v["error"]["code"], "AGENT_NOT_PICKED", "{v}");
    assert_eq!(
        v["error"]["context"]["message"],
        "codex is not one of this project's agents. Add it: topos agents add codex",
        "{v}"
    );
    assert_eq!(v["next_actions"][0]["code"], "PICK_AGENTS", "{v}");
    assert_eq!(
        v["next_actions"][0]["argv"],
        serde_json::json!(["topos", "agents", "add", "codex"]),
        "{v}"
    );
    assert!(
        !rig.project.join(".codex/skills/writer").exists(),
        "the unpicked agent's folder was never written into"
    );

    // The TTY closes the way every argv refusal does — nothing was read past the flags.
    let out = rig.run(&["add", "./src/writer", "-a", "codex"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "error: codex is not one of this project's agents. Add it: topos agents add codex"
        ) && stderr.contains("nothing changed"),
        "{stderr}"
    );

    // The agent this project DID pick lands the copy.
    let (status, v) = rig.json(&["--json", "add", "./src/writer", "-a", "claude-code"]);
    assert!(status.success(), "{v}");
    assert!(rig.project.join(".claude/skills/writer/SKILL.md").is_file());
    let _ = std::fs::remove_dir_all(&rig.root);
}

// =================================================================================================
// What a pick run says about what it could not do, and what never derives one.
// =================================================================================================

/// The quiet hook sweep NEVER derives a pick, at either scope — not even with exactly one agent
/// installed: a project pick registered the hook that fired it, and nobody picked anything for
/// the machine. Nothing is minted under `~/.topos`, nothing lands under any agent dir, and the
/// sweep stays byte-silent, exit 0.
#[test]
fn the_quiet_sweep_never_derives_a_machine_pick_from_a_project_pick() {
    let rig = pick_rig("quiet-one-agent", &["claude-code"]);
    rig.stdout(&["init", "-a", "claude-code"]);
    assert!(rig.project_pick().is_some() && rig.machine_pick().is_none());
    let home_before = PickRig::files_under(&rig.home, &[".topos"]);
    let out = rig.run(&["install", "--quiet", "--ttl", "0", "--hook", "claude-code"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    assert!(rig.machine_pick().is_none(), "no machine pick was minted");
    assert_eq!(PickRig::files_under(&rig.home, &[".topos"]), home_before);
    assert!(!rig.topos_home().join("agents.json").exists());
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// `init -a` carries the reconcile's outcome: a row pointing at a folder that is gone prints its
/// failure line under what landed, exits non-zero, and says `ok: false` under `--json`, exactly
/// as `update` does on the same tree — the pick, the copy and the hook still landed.
#[test]
fn init_with_agents_says_what_the_reconcile_could_not_do() {
    let rig = pick_rig("init-outcome", &["claude-code"]);
    rig.write_manifest("schema = 1\n\n[skills]\nnope = { path = \"./nope\" }\n");
    let out = rig.run(&["init", "-a", "claude-code"]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(
            "Installed 1 skill to .claude/skills.\n\
             warning: \"./nope\": your topos.toml asks for this folder by this project, and the \
             folder is gone. Run 'topos remove ./nope --yes' to drop the line.\n\
             Auto-update hooks: .claude/settings.local.json."
        ),
        "{stdout}"
    );
    assert!(rig.project.join(".claude/skills/topos/SKILL.md").exists());
    let (status, v) = rig.json(&["--json", "init", "-a", "claude-code"]);
    assert_eq!(status.code(), Some(1));
    assert_eq!(v["ok"], false, "{v}");
    assert_eq!(v["data"]["pick"]["failed_bundles"], 1, "{v}");
    assert_eq!(
        v["data"]["pick"]["warnings"][0]["code"], "PATH_MISSING",
        "{v}"
    );
    assert_eq!(v["messages"][0]["kind"], "failure", "{v}");
    assert!(
        v["warnings"][0]
            .as_str()
            .unwrap()
            .starts_with("PATH_MISSING \"./nope\": your topos.toml asks for this folder"),
        "{v}"
    );
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// The receipt's own hint works: `init -a`, then `init --gitignore` on the existing manifest with
/// the standing pick appends the picked agents' folders (and prints the pick receipt again, with
/// the appended lines instead of the hint).
#[test]
fn init_gitignore_appends_over_a_standing_pick() {
    let rig = pick_rig("init-gitignore-later", &["claude-code", "codex"]);
    let first = rig.stdout(&["init", "-a", "claude-code", "-a", "codex"]);
    assert!(
        first
            .contains("New files are not ignored by git. To keep them out: topos init --gitignore"),
        "{first}"
    );
    assert!(!rig.project.join(".gitignore").exists());
    let again = rig.stdout(&["init", "--gitignore"]);
    assert!(
        again.contains("Added to .gitignore: .claude/, .codex/.\n"),
        "{again}"
    );
    assert!(!again.contains("not ignored by git"), "{again}");
    assert!(!again.contains("already exists"), "{again}");
    assert_eq!(
        std::fs::read_to_string(rig.project.join(".gitignore")).unwrap(),
        ".claude/\n.codex/\n"
    );
    // A plain `init` on the same tree is still the no-op receipt.
    let plain = rig.stdout(&["init"]);
    assert!(plain.contains("already exists"), "{plain}");
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// Every `add` runs the pick rule before it records a row or lands a byte, at the scope the row
/// would land in: several agents installed and no pick, piped, is exit 2 naming `topos init -a`
/// with NOTHING half-done — no row, no pick, no folder; one installed is used and said, and then
/// the add proceeds.
#[test]
fn add_runs_the_pick_rule_before_anything_lands() {
    let rig = pick_rig("add-no-pick", &["claude-code", "codex"]);
    rig.write_manifest("schema = 1\n");
    let hello = rig.root.join("hello");
    std::fs::create_dir_all(&hello).unwrap();
    std::fs::write(
        hello.join("SKILL.md"),
        "---\nname: hello\ndescription: says hello\n---\nHello.\n",
    )
    .unwrap();
    let out = rig.run(&["add", hello.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Several agents are installed here: claude-code, codex.\n\
         Pick with: topos init -a claude-code [-a codex]\n"
    );
    assert_eq!(
        std::fs::read_to_string(rig.project.join("topos.toml")).unwrap(),
        "schema = 1\n",
        "the row is not recorded"
    );
    assert!(rig.project_pick().is_none() && rig.machine_pick().is_none());
    assert!(!rig.project.join(".claude").exists() && !rig.project.join(".codex").exists());
    assert!(
        !rig.project.join(".topos").exists(),
        "no store minted either"
    );
    // The same answer for a bare name and under `--json`.
    let (status, v) = rig.json(&["--json", "add", "hello"]);
    assert_eq!(status.code(), Some(2));
    assert_eq!(v["error"]["code"], "PICK_REQUIRED", "{v}");
    assert_eq!(v["data"]["next"], "topos init -a <agent>", "{v}");
    // The machine scope, with `-g`: the same rule, spelled for the machine.
    let out = rig.run(&["add", "-g", hello.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Several agents are installed here: claude-code, codex.\n\
         Pick with: topos init -g -a claude-code [-a codex]\n"
    );
    assert!(
        !rig.topos_home().join("topos.toml").exists() || {
            !std::fs::read_to_string(rig.topos_home().join("topos.toml"))
                .unwrap()
                .contains("hello")
        }
    );

    // One agent installed: used, said on stderr, recorded — and the add goes through.
    let one = pick_rig("add-one-agent", &["codex"]);
    one.write_manifest("schema = 1\n");
    let out = one.run(&["add", hello.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Using Codex in this project.\n"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("added hello to this project"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(agents_of(&one.project_pick().unwrap()), ["codex"]);
    assert!(
        std::fs::read_to_string(one.project.join("topos.toml"))
            .unwrap()
            .contains("hello"),
        "the row is recorded"
    );
    let _ = std::fs::remove_dir_all(&rig.root);
    let _ = std::fs::remove_dir_all(&one.root);
}

/// **A machine with no agent installed is not asked anything.** There is nothing to choose from,
/// so nothing is picked, nothing is placed, and the verbs go on doing the rest of their work:
/// a build box that holds no agent at all still has to run `topos install` and record rows. The
/// answer is one sentence, said once per run — on stderr where the receipt has no line for it,
/// as the receipt's own first line for `init`, and as a coded line under `--json`.
#[test]
fn zero_installed_agents_is_not_an_ask() {
    let rig = pick_rig("no-agent-installed", &[]);
    rig.write_manifest("schema = 1\n");
    let said = "No agent topos knows is installed here; nothing placed.\n";

    // `install`: exit 0, the sentence once, no pick file, nothing in any agent folder.
    let out = rig.run(&["install"]);
    assert!(
        out.status.success(),
        "install goes on: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), said);
    assert!(rig.project_pick().is_none() && rig.machine_pick().is_none());

    // `add`: the row IS recorded — the manifest is the demand, and demand does not need an agent.
    let hello = rig.root.join("hello");
    std::fs::create_dir_all(&hello).unwrap();
    std::fs::write(
        hello.join("SKILL.md"),
        "---\nname: hello\ndescription: says hello\n---\nHello.\n",
    )
    .unwrap();
    let out = rig.run(&["add", hello.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "the add goes through: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), said);
    assert!(
        std::fs::read_to_string(rig.project.join("topos.toml"))
            .unwrap()
            .contains("hello"),
        "the row is recorded"
    );
    assert!(rig.project_pick().is_none() && rig.machine_pick().is_none());
    // Nothing an agent reads was written, in the checkout or under `$HOME`.
    for dir in [".claude", ".codex", ".cursor", ".agents", ".opencode"] {
        assert!(!rig.project.join(dir).exists(), "{dir} in the checkout");
        assert!(!rig.home.join(dir).exists(), "{dir} under HOME");
    }

    // `init` in a folder of its own: the receipt says the manifest it wrote, then the sentence
    // in place of a `Using` line.
    let fresh = rig.root.join("fresh");
    std::fs::create_dir_all(fresh.join(".git")).unwrap();
    let out = rig.run_at(&fresh, &["init"], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let receipt = String::from_utf8_lossy(&out.stdout);
    assert!(receipt.starts_with("Created "), "{receipt}");
    assert!(receipt.ends_with(said), "{receipt}");
    assert_eq!(
        receipt.matches("nothing placed").count(),
        1,
        "said once: {receipt}"
    );
    assert!(fresh.join("topos.toml").exists(), "the manifest is written");
    assert!(String::from_utf8_lossy(&out.stderr).is_empty());

    // `--json`: the one document carries the same sentence as a coded line, and the run is `ok`.
    let fresh2 = rig.root.join("fresh2");
    std::fs::create_dir_all(fresh2.join(".git")).unwrap();
    let out = rig.run_at(&fresh2, &["--json", "init"], &[]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(
        v["messages"][0],
        serde_json::json!({
            "code": "NO_AGENT_INSTALLED",
            "kind": "disclosure",
            "text": "No agent topos knows is installed here; nothing placed."
        }),
        "{v}"
    );
    assert_eq!(v["data"]["pick"]["agents"], serde_json::json!([]), "{v}");
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// **`--frozen` never asks, and never refuses over a pick no clone can carry.** A CI runner
/// checks out a project, exports a machine token and runs `topos install --frozen`. It has no
/// agents pick and cannot be given one: the pick is personal and topos git-ignores the file it
/// lives in. Asking was a prompt nobody could answer, and refusing was a red pipeline over a
/// question with no answer — so a frozen run picks nobody, fetches and verifies what the project
/// names, places nothing, exits 0, and says so once. The ask itself is untouched: a person at a
/// keyboard still gets it.
#[test]
fn frozen_install_with_no_pick_fetches_and_places_nothing() {
    let rig = pick_rig("frozen-no-pick", &["claude-code", "cursor"]);
    write_weather(&rig);
    let said = "No agents picked here; nothing placed. Pick with: topos init -a <agent>\n";

    // SEVERAL installed, none picked: the run goes through and states the fact.
    let out = rig.run(&["install", "--frozen"]);
    assert!(
        out.status.success(),
        "frozen goes through: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), said);
    assert_no_agent_files(&rig);
    assert!(
        rig.project_pick().is_none() && rig.machine_pick().is_none(),
        "a run that asked nothing decided nothing"
    );

    // `--json`: the same fact as a coded line on the one document, and the run is `ok`.
    let (status, v) = rig.json(&["--json", "install", "--frozen"]);
    assert!(status.success(), "{v}");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(
        v["messages"][0],
        serde_json::json!({
            "code": "NO_PICK",
            "kind": "disclosure",
            "text": "No agents picked here; nothing placed. Pick with: topos init -a <agent>"
        }),
        "{v}"
    );
    assert_no_agent_files(&rig);

    // Inside an agent that announces itself, a frozen run STILL picks nobody. The ask's
    // agent-friendly shortcut is a guess about this machine, and a posture whose promise is "the
    // same bytes everywhere" must not answer differently depending on what started it.
    let out = rig.run_at(
        &rig.project,
        &["install", "--frozen"],
        &[("CLAUDECODE", "1")],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), said);
    assert!(rig.project_pick().is_none() && rig.machine_pick().is_none());
    assert_no_agent_files(&rig);

    // WITHOUT `--frozen` the refusal stands: a person ran this, and the way out is theirs to take.
    let out = rig.run(&["install"]);
    assert_eq!(out.status.code(), Some(2), "the refusal is unchanged");
    assert!(
        String::from_utf8_lossy(&out.stderr).starts_with("Several agents are installed here: "),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // ZERO installed: the sentence is the agentless one, and the run still goes on.
    let bare = pick_rig("frozen-zero-agents", &[]);
    write_weather(&bare);
    let out = bare.run(&["install", "--frozen"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "No agent topos knows is installed here; nothing placed.\n"
    );
    assert_no_agent_files(&bare);
    assert!(bare.project_pick().is_none() && bare.machine_pick().is_none());
    let (status, v) = bare.json(&["--json", "install", "--frozen"]);
    assert!(status.success(), "{v}");
    assert_eq!(v["messages"][0]["code"], "NO_AGENT_INSTALLED", "{v}");

    let _ = std::fs::remove_dir_all(&rig.root);
    let _ = std::fs::remove_dir_all(&bare.root);
}

/// The other half of the same rule: with a pick standing, `install --frozen` places exactly what
/// it always placed and says nothing about picking.
#[test]
fn frozen_install_with_a_pick_places_as_before() {
    let rig = pick_rig("frozen-picked", &["claude-code", "cursor"]);
    write_weather(&rig);
    rig.run(&["init", "-a", "claude-code"]);
    assert!(rig.project_pick().is_some(), "the pick stands");

    let out = rig.run(&["install", "--frozen"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("nothing placed"), "{err}");
    assert!(
        std::fs::read_to_string(rig.claude_json()).is_ok_and(|t| t.contains(&rig.project_key())),
        "the picked agent's config holds the entry"
    );
    assert!(
        rig.project.join(".claude/skills/topos").exists(),
        "the picked agent's skills folder holds what topos delivers"
    );
    // The agent nobody picked is untouched, frozen or not.
    assert!(!rig.project.join(".cursor").exists());
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// A project whose only demand is a local MCP server bundle — something that PLACES when an agent
/// is picked (a config entry) and places nothing when none is.
fn write_weather(rig: &PickRig) {
    std::fs::create_dir_all(rig.project.join("weather")).unwrap();
    std::fs::write(
        rig.project.join("weather/server.json"),
        r#"{"name":"io.github.acme/weather","description":"Conditions.","version":"1.4.0","remotes":[{"type":"streamable-http","url":"https://weather.acme.example/mcp"}]}"#,
    )
    .unwrap();
    rig.write_manifest("schema = 1\n\n[mcp]\nweather = { path = \"./weather\", kind = \"mcp\" }\n");
}

/// Nothing an agent reads was written, in the checkout or under `$HOME`.
fn assert_no_agent_files(rig: &PickRig) {
    for dir in [".claude", ".codex", ".cursor", ".agents", ".opencode"] {
        assert!(!rig.project.join(dir).exists(), "{dir} in the checkout");
        assert!(
            !rig.home.join(dir).join("skills").exists(),
            "{dir} under HOME"
        );
    }
    assert!(
        !rig.project.join(".mcp.json").exists(),
        "an MCP entry landed"
    );
}

/// **`add` checks its SOURCE before the pick rule.** A path that is not on this machine is
/// refused with its own code, on a machine with several agents and no pick: the pick rule
/// records a decision, so it must not run — and answer about agents — for a command that was
/// never going to place a byte.
#[test]
fn add_refuses_a_missing_source_before_the_pick_rule() {
    let rig = pick_rig("add-missing-source", &["claude-code", "codex"]);
    rig.write_manifest("schema = 1\n");
    let (status, v) = rig.json(&["--json", "add", "./nope"]);
    assert_eq!(status.code(), Some(1), "{v}");
    assert_eq!(v["error"]["code"], "SOURCE_MISSING", "{v}");
    assert!(
        rig.project_pick().is_none() && rig.machine_pick().is_none(),
        "a refusal picks nothing"
    );
    // The machine spelling answers the same way.
    let (status, v) = rig.json(&["--json", "add", "-g", "./nope"]);
    assert_eq!(status.code(), Some(1), "{v}");
    assert_eq!(v["error"]["code"], "SOURCE_MISSING", "{v}");
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// An MCP row whose own `dest` names an UNPICKED agent's file gets no entry (topos never touches
/// an agent the person did not pick) and is never silent about it: the pick receipt and every
/// sweep carry the line with the command that picks the agent, the row counts `not placed`,
/// and the `--json` row carries the withheld state.
#[test]
fn an_mcp_dest_naming_an_unpicked_agent_is_withheld_and_said() {
    let rig = pick_rig("mcp-dest-unpicked", &["claude-code", "codex", "cursor"]);
    std::fs::create_dir_all(rig.project.join("weather")).unwrap();
    std::fs::write(
        rig.project.join("weather/server.json"),
        r#"{"name":"io.github.acme/weather","description":"Conditions.","version":"1.4.0","remotes":[{"type":"streamable-http","url":"https://weather.acme.example/mcp"}]}"#,
    )
    .unwrap();
    rig.write_manifest(
        "schema = 1\n\n[mcp]\nweather = { path = \"./weather\", kind = \"mcp\", dest = [\".cursor/mcp.json\"] }\n",
    );
    let line = "warning: not placed in cursor: cursor is not one of this project's agents. Add \
                it: topos agents add cursor";
    let receipt = rig.stdout(&["init", "-a", "claude-code"]);
    assert!(receipt.contains(line), "{receipt}");
    assert!(!rig.project.join(".cursor").exists(), "no entry, no folder");
    let sweep = rig.stdout(&["update"]);
    assert!(sweep.contains(line), "{sweep}");
    assert!(sweep.contains("1 not placed"), "{sweep}");
    let (status, v) = rig.json(&["--json", "update"]);
    assert!(status.success(), "an advisory fails nothing: {v}");
    // A bundle nothing holds anywhere has no `up to date` row to hide behind: the row stands
    // down and the message carries the whole answer.
    assert_eq!(v["data"]["skills"], serde_json::json!([]), "{v}");
    assert_eq!(
        v["messages"][0],
        serde_json::json!({
            "code": "MCP_AGENT_NOT_PICKED",
            "kind": "advisory",
            "text": "not placed in cursor: cursor is not one of this project's agents. Add it: \
                     topos agents add cursor"
        }),
        "{v}"
    );
    // Picking cursor is the whole fix: the entry lands and the line goes.
    let picked = rig.stdout(&["agents", "add", "cursor"]);
    assert!(!picked.contains("not placed"), "{picked}");
    assert!(rig.project.join(".cursor/mcp.json").exists());
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// A picked agent's skills folder that is a symlink out of the checkout: the built-in's
/// placement is refused by the containment rail, and the pick receipt says so beside the hook
/// refusal — nothing is written through the link, and the run exits non-zero like `update`.
#[test]
fn a_symlinked_skills_folder_is_said_on_the_pick_receipt() {
    let rig = pick_rig("pick-symlink-claude", &["claude-code"]);
    let outside = rig.root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, rig.project.join(".claude")).unwrap();
    let out = rig.run(&["init", "-a", "claude-code"]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Using Claude Code in this project.\nwarning: ")
            && stdout.contains(
                ".claude/skills does not resolve inside this checkout (claude-code) — a symlink"
            ),
        "{stdout}"
    );
    assert!(
        stdout.contains("claude-code: auto-update hook not registered"),
        "{stdout}"
    );
    assert!(PickRig::files_under(&outside, &[]).is_empty());
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// A pick file topos cannot parse names ITSELF and its scope, and the run fails closed before
/// any converge (copies and hooks stay as they are).
#[test]
fn an_unreadable_pick_file_names_itself() {
    let rig = pick_rig("pick-corrupt", &["claude-code", "codex"]);
    rig.stdout(&["init", "-a", "claude-code"]);
    std::fs::write(rig.project.join(".topos/agents.json"), "{not json").unwrap();
    let out = rig.run(&["update"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with(
            "error: this project's .topos/agents.json is unreadable: missing/invalid \
             schema_version"
        ) && stderr.contains(". Fix the file or delete it\n"),
        "{stderr}"
    );
    assert!(rig.project.join(".claude/skills/topos/SKILL.md").exists());
    let (status, v) = rig.json(&["--json", "agents"]);
    assert_eq!(status.code(), Some(1));
    assert_eq!(v["error"]["code"], "CORRUPT_STATE", "{v}");
    // The machine file, the same way.
    std::fs::remove_file(rig.project.join(".topos/agents.json")).unwrap();
    std::fs::write(
        rig.topos_home().join("agents.json"),
        "{\"schema_version\": 1, \"agents\": 5}",
    )
    .unwrap();
    let out = rig.run(&["agents", "-g"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .starts_with("error: ~/.topos/agents.json is unreadable: document parse:"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// A `.gitignore` that is a symlink out of the checkout is never written through, and never an
/// abort either: the pick, the copies and the hooks land, the receipt closes with the one line,
/// exit 0. `topos agents --gitignore` says the same over the list.
#[test]
fn a_gitignore_symlink_is_a_line_not_an_abort() {
    let rig = pick_rig("gitignore-symlink", &["claude-code"]);
    let target = rig.root.join("outside-gitignore");
    std::fs::write(&target, b"").unwrap();
    std::os::unix::fs::symlink(&target, rig.project.join(".gitignore")).unwrap();
    let receipt = rig.stdout(&["init", "-a", "claude-code", "--gitignore"]);
    assert!(
        receipt.contains(
            "Installed 1 skill to .claude/skills.\n\
             warning: `.gitignore` is a symlink out of this checkout; not edited\n\
             Auto-update hooks: .claude/settings.local.json."
        ),
        "{receipt}"
    );
    assert!(!receipt.contains("Added to .gitignore"), "{receipt}");
    assert_eq!(std::fs::read(&target).unwrap(), b"", "byte-identical");
    assert_eq!(agents_of(&rig.project_pick().unwrap()), ["claude-code"]);
    assert!(rig.project.join(".claude/skills/topos/SKILL.md").exists());
    let listed = rig.stdout(&["agents", "--gitignore"]);
    assert!(
        listed.ends_with("warning: `.gitignore` is a symlink out of this checkout; not edited\n"),
        "{listed}"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"");
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// The migration seed's one line spells display names and says where the pick came from in
/// words that hold for every source it unions (hooks, config entries, placed copies).
#[test]
fn the_seed_line_spells_display_names() {
    let rig = pick_rig("seed-line", &["claude-code", "cursor"]);
    let state = rig.topos_home().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("trigger_registration.json"),
        r#"{"schema_version":1,"agents":{"cursor":{"at_ms":1,"registered":true,"retry_at_ms":0}}}"#,
    )
    .unwrap();
    let out = rig.run(&["list", "-g"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Using Cursor on this machine (from what an earlier topos set up here). Change: topos \
         agents add|remove -g\n"
    );
    assert_eq!(agents_of(&rig.machine_pick().unwrap()), ["cursor"]);
    let _ = std::fs::remove_dir_all(&rig.root);
}
