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
    dir
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

fn run(home: &Path, args: &[&str]) -> (bool, serde_json::Value) {
    // Hermetic: point the Claude config home at an isolated (empty) dir so a test never reads or writes
    // the real `~/.claude`.
    run_in(home, &home.join(".claude-isolated"), args)
}

fn run_in(home: &Path, claude: &Path, args: &[&str]) -> (bool, serde_json::Value) {
    let out = Command::new(bin())
        .env("TOPOS_HOME", home)
        .env("CLAUDE_CONFIG_DIR", claude)
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
    let (ok, v) = run(&home, &["--json", "add", skill.to_str().unwrap()]);
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

    // list — finds the tracked skill via the same wiring.
    let (ok, v) = run(&home, &["--json", "list"]);
    assert!(ok);
    let tracked = v["data"]["tracked"].as_array().expect("tracked array");
    assert_eq!(tracked.len(), 1);
    assert_eq!(tracked[0]["skill"], "pr-describe");
    assert_eq!(tracked[0]["version_id"], version);

    // An unknown skill name fails closed with a coded error envelope (still exit-nonzero, valid JSON).
    let (ok, v) = run(&home, &["--json", "list", "nope"]);
    assert!(!ok, "an unknown name should exit nonzero");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "NO_SUCH_SKILL");

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
    let (ok, v) = run(&home, &["--json", "add", skill.to_str().unwrap()]);
    assert!(ok, "add should exit 0");
    // The add recorded a manifest line (an out-of-tree path ref lands in the personal manifest)
    // and disclosed its inverse — `remove <path>` — in the receipt.
    let undo: Vec<String> = v["data"]["undo"]
        .as_array()
        .expect("the add receipt carries its undo argv")
        .iter()
        .map(|t| t.as_str().expect("argv token").to_owned())
        .collect();
    assert_eq!(undo[..2], ["topos".to_owned(), "remove".to_owned()]);
    let path_token = undo[2].clone();

    // UNGATED (the manifest-line remove): dropping the line is immediate and reversible — the
    // bare run APPLIES with the receipt document (not a `describe` wrapper), `bytes_kept` (the
    // tracked bytes stay in the sidecar), and the undo-led next action (`add` the path back).
    let (ok, v) = run(&home, &["--json", "remove", &path_token]);
    assert!(ok, "the manifest-line remove applies: {v}");
    assert_eq!(v["command"], "remove");
    assert!(v["data"].get("describe").is_none(), "an apply receipt: {v}");
    assert_eq!(v["data"]["applied"], true);
    assert_eq!(v["data"]["items"][0]["kind"], "manifest-removed");
    assert_eq!(v["data"]["items"][0]["bytes_kept"], true);
    assert_eq!(v["next_actions"][0]["code"], "UNDO");
    assert_eq!(
        v["next_actions"][0]["argv"].as_array().map(Vec::len),
        Some(3),
        "the undo is the `add <path>` inverse: {v}"
    );

    // GATED (a local-only `remove` — the permanent delete of the only copy): the bare run answers
    // the DESCRIBE envelope, `applied: false`, the `--yes` apply next action; nothing is deleted.
    let (ok, v) = run(&home, &["--json", "remove", "pr-describe"]);
    assert!(ok, "the gated describe exits 0: {v}");
    assert_eq!(v["command"], "remove");
    assert_eq!(v["data"]["describe"]["applied"], false);
    assert_eq!(
        v["data"]["describe"]["items"][0]["kind"],
        "tracked-local-permanent"
    );
    assert_eq!(v["next_actions"][0]["code"], "APPLY_DESCRIBED");
    let argv = v["next_actions"][0]["argv"].as_array().expect("argv");
    assert_eq!(argv.last().and_then(|t| t.as_str()), Some("--yes"));
    // The describe deleted nothing: the skill still lists as tracked.
    let (ok, v) = run(&home, &["--json", "list", "--tracked"]);
    assert!(ok);
    assert_eq!(v["data"]["tracked"][0]["skill"], "pr-describe");

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
    let (ok, v) = run_in(&home, &claude, &["--json", "add", skill.to_str().unwrap()]);
    assert!(ok, "add should exit 0");
    assert_eq!(v["data"]["name"], "pr-describe");
    assert_eq!(v["data"]["harness"], "claude-code");
    assert_eq!(v["data"]["currency"]["state"], "active");
    assert_eq!(v["data"]["currency"]["currency_kind"], "session_start");

    let settings = std::fs::read_to_string(claude.join("settings.json")).unwrap();
    assert!(
        settings.contains("topos update --quiet"),
        "the hook command was installed"
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

    // list shows it tracked.
    let (ok, v) = run_in(&home, &claude, &["--json", "list"]);
    assert!(ok);
    assert_eq!(v["data"]["tracked"][0]["skill"], "pr-describe");

    // The installed hook runs `topos update --quiet`; the field's already-armed hooks run the retained
    // `topos pull --quiet` alias. BOTH must exit 0 and emit NOTHING on stdout (a SessionStart hook's
    // stdout is injected into the session). Exercise the alias here (it must keep working).
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
    let (ok, v) = run_in(&home, &claude, &["--json", "add", skill.to_str().unwrap()]);
    assert!(!ok, "re-adding the same dir exits nonzero");
    assert_eq!(v["error"]["code"], "ALREADY_TRACKED");

    let _ = std::fs::remove_dir_all(&claude);
    let _ = std::fs::remove_dir_all(&home);
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
    let out = Command::new(bin())
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
        .expect("spawn topos");
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("non-JSON stdout: {}", String::from_utf8_lossy(&out.stdout)))
}

#[test]
fn end_to_end_add_by_name_resolves_a_discovered_skill() {
    let home = scratch("byname-home");
    let disc = scratch("byname-disc"); // an EMPTY discovery HOME — only the Claude config below is present
    let claude = scratch("byname-claude");
    let skill = claude.join("skills").join("deploy");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "# deploy\n\nShip it.\n").unwrap();

    // `topos list` discovers it as untracked, tagged with the registry slug (the `@harness` token).
    let v = run_disc(&home, &disc, &claude, &["--json", "list"]);
    let untracked = v["data"]["untracked"].as_array().expect("untracked array");
    assert_eq!(untracked.len(), 1, "{untracked:?}");
    assert_eq!(untracked[0]["name"], "deploy");
    assert_eq!(untracked[0]["harness"], "claude-code");

    // `topos add deploy` — resolves the NAME against that inventory and adopts it (Claude Code recognized,
    // auto-update armed), with no path typed.
    let v = run_disc(&home, &disc, &claude, &["--json", "add", "deploy"]);
    assert_eq!(v["command"], "add");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["data"]["name"], "deploy");
    assert_eq!(v["data"]["harness"], "claude-code");
    assert_eq!(v["data"]["tracked"], true);

    // Now tracked → discovery no longer lists it as untracked, and re-adopting by name reports it tracked.
    let v = run_disc(&home, &disc, &claude, &["--json", "list"]);
    assert_eq!(v["data"]["tracked"][0]["skill"], "deploy");
    assert!(v["data"]["untracked"].as_array().unwrap().is_empty());
    let v = run_disc(&home, &disc, &claude, &["--json", "add", "deploy"]);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "ALREADY_TRACKED");
    // The DISAMBIGUATED re-add reports the SAME code (an agent branches identically whether or not it
    // typed `@harness`) — not HARNESS_NOT_FOUND just because discovery excludes the tracked placement.
    let v = run_disc(
        &home,
        &disc,
        &claude,
        &["--json", "add", "deploy@claude-code"],
    );
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "ALREADY_TRACKED");

    // A name nowhere in the inventory fails closed with the discovery-specific code (not NO_SUCH_SKILL,
    // which is about tracked skills).
    let v = run_disc(&home, &disc, &claude, &["--json", "add", "ghost"]);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "NO_UNTRACKED_SKILL");

    // A path-shaped positional is treated as a PATH (adopt in place), NEVER resolved as the same-named
    // discovered skill: a `./`-prefixed token pointing at nothing here fails as a path adopt (an fs
    // error), so it can't sneak in as the discovered "deploy".
    let v = run_disc(&home, &disc, &claude, &["--json", "add", "./deploy"]);
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
    assert_eq!(v["error"]["code"], "ENROLLMENT_FAILED");

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
        "a sidecar document is corrupt"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn an_io_error_is_redacted_on_the_surface_and_detailed_in_the_log() {
    let home = scratch("iodiag");
    let missing = format!("/definitely-missing-topos-{}", std::process::id());

    // --json: the fixed message on stdout; the full context (path + cause) in the diagnostics log.
    let out = run_raw(&home, &["add", &missing, "--json"], false);
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON stdout");
    assert_eq!(v["error"]["code"], "IO_ERROR");
    assert_eq!(
        v["error"]["context"]["message"],
        "a filesystem operation failed"
    );
    let log = std::fs::read_to_string(home.join("log.jsonl")).expect("the diagnostics log exists");
    let event: serde_json::Value = log
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .find(|e: &serde_json::Value| e["action"] == "error")
        .expect("an error event landed");
    assert_eq!(event["verb"], "add");
    assert_eq!(event["code"], "IO_ERROR");
    let detail = event["detail"].as_str().expect("detail");
    assert!(detail.contains(&missing), "{detail}");

    // TTY: redacted line + the pointer at the log; the path never reaches stderr un-asked.
    let out = run_raw(&home, &["add", &missing], false);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error: a filesystem operation failed"),
        "{stderr}"
    );
    assert!(stderr.contains("details: "), "{stderr}");
    assert!(stderr.contains("log.jsonl"), "{stderr}");
    assert!(!stderr.contains(&missing), "stays redacted: {stderr}");

    // TOPOS_DEBUG=1: the full chain ALSO reaches stderr, while stdout stays the clean envelope.
    let out = run_raw(&home, &["add", &missing, "--json"], true);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON stdout");
    assert_eq!(
        v["error"]["context"]["message"],
        "a filesystem operation failed"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&missing), "{stderr}");

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn update_onto_current_flag_shapes_are_usage_errors() {
    let home = scratch("ontousage");

    // Missing <skill> target → the runtime usage error (INVALID_ARGUMENT, exit 1). `--onto-current` is a
    // hidden flag on `update` (reached here through the `pull` alias the field's armed hooks still run).
    let out = run_raw(&home, &["pull", "--onto-current", "--json"], false);
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON stdout");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");

    // Combined with @<hash> → the runtime usage error rides the envelope as INVALID_ARGUMENT.
    let target = format!("docs@{}", "ab".repeat(32));
    let out = run_raw(
        &home,
        &["update", &target, "--onto-current", "--json"],
        false,
    );
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON stdout");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");

    let _ = std::fs::remove_dir_all(&home);
}
