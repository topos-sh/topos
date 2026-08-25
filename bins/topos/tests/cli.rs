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

fn run(home: &Path, args: &[&str]) -> (bool, serde_json::Value) {
    // Hermetic: point the Claude config home at an isolated (empty) dir so a test never reads or writes
    // the real `~/.claude`.
    run_in(home, &home.join(".claude-isolated"), args)
}

fn run_in(home: &Path, claude: &Path, args: &[&str]) -> (bool, serde_json::Value) {
    let out = Command::new(bin())
        .env("TOPOS_HOME", home)
        .env("CLAUDE_CONFIG_DIR", claude)
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
    let mut cmd = Command::new(bin());
    cmd.env("TOPOS_HOME", home)
        .env("CLAUDE_CONFIG_DIR", home.join(".claude-isolated"))
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

#[test]
fn end_to_end_claude_code_adopt_arms_currency_and_pull_is_silent() {
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

    // add → recognized as Claude Code, auto-update armed, hook written to settings.json.
    // MACHINE-WIDE: the skill lives in the agent's own config home, not in a checkout.
    let (ok, v) = run_in(
        &home,
        &claude,
        &["--json", "add", "-g", skill.to_str().unwrap()],
    );
    assert!(ok, "add should exit 0");
    assert_eq!(v["data"]["name"], "pr-describe");
    assert_eq!(v["data"]["currency"]["state"], "active");
    assert_eq!(v["data"]["currency"]["currency_kind"], "session_start");

    let settings = std::fs::read_to_string(claude.join("settings.json")).unwrap();
    assert!(
        settings.contains("topos install --quiet --hook claude-code"),
        "the hook command was installed, carrying the dialect marker"
    );
    assert!(
        settings.contains("# topos:currency"),
        "the idempotency sentinel is present"
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
    let out = Command::new(bin())
        .env("TOPOS_HOME", topos_home)
        .env("HOME", disc_home)
        .current_dir(cwd)
        .env("CLAUDE_CONFIG_DIR", claude)
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
    Command::new(bin())
        .env("TOPOS_HOME", topos_home)
        .env("HOME", disc_home)
        .current_dir(disc_home)
        .env("CLAUDE_CONFIG_DIR", claude)
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

/// **A newly installed agent needs no flags.** The sweep registers the auto-update trigger of an
/// agent it has found for the first time — over the REAL argv, the real detection roots, and the
/// real config writes, which is the seam the unit tests cannot reach. Three facts, one run each:
/// the trigger lands, the offer is never repeated (so a hand-removal stands), and the quiet
/// sweep's stdout is BYTE-IDENTICAL with a registration happening — a hook document that gained a
/// line, or a `reloadSkills` over bytes that never moved, is a lie a strict-schema agent pays for.
#[test]
fn the_quiet_sweep_registers_a_newly_detected_agent_once_and_says_nothing_about_it() {
    let home = scratch("register-home");
    let disc = scratch("register-disc");
    let claude = scratch("register-claude");

    // Settle the built-in FIRST, with no other agent around: the sweep under test then changes no
    // bytes at all, so the `reloadSkills` half of this test proves something.
    let warmup = sweep_raw(&home, &disc, &claude, &["update", "--quiet", "--ttl", "0"]);
    assert!(warmup.status.success(), "warm-up sweep exits 0");
    assert!(
        claude
            .join("skills")
            .join("topos")
            .join("SKILL.md")
            .exists(),
        "the warm-up placed the built-in, so the next sweep changes nothing"
    );
    // The ACTIVE agent was registered by that first sweep, exactly like any other detected one.
    assert!(
        claude.join("settings.json").exists(),
        "the sweep arms nobody on a verb's account — every detected agent is a candidate"
    );

    // Now an agent is installed. Its config dir is all detection needs.
    std::fs::create_dir_all(disc.join(".cursor")).unwrap();
    let hooks = disc.join(".cursor").join("hooks.json");

    let record = home.join("state").join("trigger_registration.json");
    let out = sweep_raw(
        &home,
        &disc,
        &claude,
        &["update", "--quiet", "--ttl", "0", "--hook", "claude-code"],
    );
    assert!(out.status.success(), "a session-start sweep always exits 0");
    assert!(hooks.exists(), "the newly detected agent was registered");
    assert!(record.exists(), "and the offer is written down");
    // The other half of the same sentence: the delivered bundle reached it too, with no flags.
    assert!(
        disc.join(".cursor")
            .join("skills")
            .join("topos")
            .join("SKILL.md")
            .exists(),
        "an agent found today gets the machine's bundles"
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !stdout.contains("additionalContext"),
        "a registration is never a line a person must read: {stdout:?}"
    );

    // REGISTRATION ALONE, with no byte anywhere left to move: the record is gone (a lost state
    // file re-opens the question) and so is the hook, while every placement above still stands.
    // The trigger goes back in and the hook document is byte-identical to a sweep that did
    // nothing — a `reloadSkills` here would ask for a re-scan over bytes that never moved.
    std::fs::remove_file(&record).unwrap();
    std::fs::remove_file(&hooks).unwrap();
    let out = sweep_raw(
        &home,
        &disc,
        &claude,
        &["update", "--quiet", "--ttl", "0", "--hook", "claude-code"],
    );
    assert!(out.status.success());
    assert!(hooks.exists(), "the offer was open again, and it landed");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).as_ref(),
        "",
        "a registration adds NOTHING to the hook's stdout — not a line, not a reload"
    );

    // The person deletes the hook. The sweep leaves it deleted: the question is asked once.
    std::fs::remove_file(&hooks).unwrap();
    let out = sweep_raw(&home, &disc, &claude, &["update", "--quiet", "--ttl", "0"]);
    assert!(out.status.success());
    assert!(
        !hooks.exists(),
        "a trigger somebody removed is never offered again"
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).as_ref(), "");

    let _ = std::fs::remove_dir_all(&home);
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
