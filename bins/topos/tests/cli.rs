//! End-to-end coverage of the binary composition root the in-crate unit tests can't reach: real argv
//! parsing (clap), `TOPOS_HOME` resolution, the recover + first-use identity startup, and the `--json`
//! envelope on stdout. (`uninstall` is exercised via `ops::uninstall` with an injected fake binary in
//! the unit tests — running it here would unlink the test binary itself.)

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

    // add → recognized as Claude Code, currency armed, hook written to settings.json.
    let (ok, v) = run_in(&home, &claude, &["--json", "add", skill.to_str().unwrap()]);
    assert!(ok, "add should exit 0");
    assert_eq!(v["data"]["name"], "pr-describe");
    assert_eq!(v["data"]["harness"], "claude-code");
    assert_eq!(v["data"]["currency"]["state"], "active");
    assert_eq!(v["data"]["currency"]["currency_kind"], "session_start");

    let settings = std::fs::read_to_string(claude.join("settings.json")).unwrap();
    assert!(
        settings.contains("topos pull --quiet"),
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

    // The installed hook runs `topos pull --quiet` — it must exit 0 and emit NOTHING on stdout (a
    // SessionStart hook's stdout is injected into the session).
    let out = Command::new(bin())
        .env("TOPOS_HOME", &home)
        .env("CLAUDE_CONFIG_DIR", &claude)
        .args(["pull", "--quiet"])
        .output()
        .expect("spawn topos pull");
    assert!(out.status.success(), "pull exits 0");
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
