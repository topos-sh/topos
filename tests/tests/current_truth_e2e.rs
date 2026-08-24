//! CURRENT IS THE SERVER'S, end to end over the composed stack: the real `topos` binary against
//! the real web app in front of the in-process vault, driven from two machine homes.
//!
//! The scenario a by-hand journey reproduced twice: an author publishes a new version from inside
//! a project, undoes it with the `topos revert … --to` line the receipt printed, and then looks at
//! the copy still sitting in the project. That copy equals the version the revert just undid —
//! and every verb that decides "matches current" must decide against the workspace's LIVE
//! `current`, never the sidecar's cached notion of it:
//!
//! - `publish` previews a forward publish of the copy's content, names which version `current`
//!   is (the revert), and — applied — mints a new version whose content equals the undone one,
//!   with `log` showing every hop;
//! - `diff` shows the real difference against the live current;
//! - `update` on the copy fast-forwards (an unmodified copy is a clean tree) AND says which
//!   version it replaced, with the `revert --to` that brings it back;
//! - a publish from inside a project advances that project's `topos.lock` (a commit moves
//!   `HEAD`), so `list` shows the bundle current rather than `[behind]`.
//!
//! Like the store suite this drives the REAL CLI BINARY as a subprocess over fake `$HOME`s — the
//! placement dirs resolve against the environment, so only a real process proves what landed
//! where. The fixture rig owns the browser login each machine inherits.

mod common;

use std::path::{Path, PathBuf};

use common::{CliOut, OWNER_EMAIL, Session, Stack, cli_binary, run_cli, start_stack};
use serde_json::Value;
use topos::test_support::SessionInstall;

// ── the bundle under test ───────────────────────────────────────────────────────────────────────

const BUNDLE: &str = "truth-spine";

/// One file of the bundle as the suite states it: `(bundle-relative path, executable, bytes)`.
type File = (&'static str, bool, String);

fn v1_files(nonce: &str) -> Vec<File> {
    vec![
        (
            "SKILL.md",
            false,
            format!("# {BUNDLE}\n\nCurrent is the server's ({nonce}).\n"),
        ),
        (
            "docs/reference.md",
            false,
            format!("# reference ({nonce})\n\n- this is v1\n"),
        ),
        (
            "scripts/run.sh",
            true,
            format!("#!/bin/sh\necho \"truth v1 {nonce}\"\n"),
        ),
    ]
}

/// v2 rewrites the nested doc, adds a file, and adds an executable — the shape a person's real
/// edit takes, and the counts the preview names.
fn v2_files(nonce: &str) -> Vec<File> {
    let mut files = v1_files(nonce);
    for file in &mut files {
        if file.0 == "docs/reference.md" {
            file.2 = format!("# reference ({nonce})\n\n- this is v2\n");
        }
    }
    files.push((
        "docs/deeper/hop.md",
        false,
        format!("# hop ({nonce})\n\nadded in v2\n"),
    ));
    files.push((
        "scripts/check.sh",
        true,
        format!("#!/bin/sh\necho \"check {nonce}\"\n"),
    ));
    files
}

fn write_bundle(dir: &Path, files: &[File]) {
    for (rel, exec, body) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the bundle's nested dir");
        }
        std::fs::write(&path, body).expect("write a bundle file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if *exec { 0o755 } else { 0o644 };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .expect("set a bundle file's mode");
        }
    }
}

fn placed(dir: &Path) -> Vec<(String, bool, Vec<u8>)> {
    let mut out: Vec<(String, bool, Vec<u8>)> = SessionInstall::dir_files(dir)
        .into_iter()
        .filter(|(rel, _, _)| rel != ".gitignore")
        .map(|(rel, mode, bytes)| (rel, mode != 0, bytes))
        .collect();
    out.sort();
    out
}

fn expected(files: &[File]) -> Vec<(String, bool, Vec<u8>)> {
    let mut out: Vec<(String, bool, Vec<u8>)> = files
        .iter()
        .map(|(rel, exec, body)| ((*rel).to_owned(), *exec, body.clone().into_bytes()))
        .collect();
    out.sort();
    out
}

// ── the machines ────────────────────────────────────────────────────────────────────────────────

struct Machine {
    rig: SessionInstall,
    bin: PathBuf,
}

impl Machine {
    fn new(tag: &str) -> Self {
        let rig = SessionInstall::new(tag);
        let home = rig.root().join("home");
        std::fs::create_dir_all(home.join(".claude")).expect("seed a harness detect dir");
        Self {
            rig,
            bin: cli_binary(),
        }
    }

    fn root(&self) -> &Path {
        self.rig.root()
    }

    fn home(&self) -> PathBuf {
        self.rig.root().join("home")
    }

    fn topos_home(&self) -> PathBuf {
        self.rig.root().join(".topos")
    }

    /// Run the real binary from the install root — OUTSIDE any checkout.
    fn run(&self, args: &[&str]) -> CliOut {
        self.run_in(self.root(), args)
    }

    /// Run the real binary standing in `cwd` (the author stands in the project).
    fn run_in(&self, cwd: &Path, args: &[&str]) -> CliOut {
        run_cli(&self.bin, &self.home(), &self.topos_home(), cwd, args)
    }
}

fn login(stack: &Stack, machine: &Machine, approver: &Session) {
    stack.login_begin_and_approve(&machine.rig, approver);
    let granted = machine.rig.login(None).expect("the login resume");
    assert_eq!(granted.session_status, "active");
}

/// Every folder the MACHINE store records as a placement of `name`.
fn machine_placements(machine: &Machine, name: &str) -> Vec<PathBuf> {
    let skills = machine.topos_home().join("skills");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&skills) else {
        panic!("no machine store at {}", skills.display());
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let (Ok(lock), Ok(map)) = (
            std::fs::read(dir.join("lock.json")),
            std::fs::read(dir.join("map.json")),
        ) else {
            continue;
        };
        let lock: Value = serde_json::from_slice(&lock).expect("lock.json is JSON");
        let map: Value = serde_json::from_slice(&map).expect("map.json is JSON");
        if lock["name"].as_str() != Some(name) {
            continue;
        }
        out.extend(
            map["placements"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(PathBuf::from),
        );
    }
    out.sort();
    out
}

fn assert_converged(machine: &Machine, files: &[File], what: &str) {
    let dirs = machine_placements(machine, BUNDLE);
    assert!(!dirs.is_empty(), "{what}: the bundle landed somewhere");
    for dir in &dirs {
        assert_eq!(
            placed(dir),
            expected(files),
            "{what}: {} holds the published bytes",
            dir.display()
        );
    }
}

fn row<'a>(data: &'a Value, name: &str) -> &'a Value {
    data["skills"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["skill"] == name)
        .unwrap_or_else(|| panic!("no {name} row in the receipt: {data}"))
}

fn publish(author: &Machine, project: &Path, message: &str) -> Value {
    author
        .run_in(
            project,
            &["publish", BUNDLE, "--yes", "-m", message, "--json"],
        )
        .data(&format!("publish {message}"))
}

fn version_of(receipt: &Value) -> String {
    receipt["version_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the publish landed a version id: {receipt}"))
        .to_owned()
}

/// The versions `log` lists, newest first, as `(id, message)`.
fn log_versions(machine: &Machine, cwd: &Path) -> Vec<(String, String)> {
    let log = machine
        .run_in(cwd, &["log", BUNDLE, "--limit", "0", "--json"])
        .data("the history");
    log["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["action"] == "version")
        .map(|e| {
            (
                e["version_id"].as_str().unwrap_or_default().to_owned(),
                e["message"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// The project's `topos.lock` entry for the bundle.
fn locked_version(project: &Path) -> Option<String> {
    let text = std::fs::read_to_string(project.join("topos.lock")).ok()?;
    let mut in_block = false;
    for line in text.lines() {
        if line.trim() == format!("[skills.{BUNDLE}]") {
            in_block = true;
            continue;
        }
        if line.starts_with('[') {
            in_block = false;
        }
        if in_block && let Some(rest) = line.trim().strip_prefix("version = ") {
            return Some(rest.trim_matches('"').to_owned());
        }
    }
    None
}

/// Stand the AUTHOR up: a machine, a logged-in session, a project holding the bundle source, the
/// adopt that records it, the genesis publish, and the machine-wide copy the login's feed
/// delivers (the copy `revert` reads). Returns `(project dir, the genesis version)`.
fn author_publishes_v1(
    stack: &Stack,
    author: &Machine,
    owner: &Session,
    nonce: &str,
) -> (PathBuf, String) {
    login(stack, author, owner);
    let project = author.root().canonicalize().expect("canonical author root");
    let src = project.join(BUNDLE);
    write_bundle(&src, &v1_files(nonce));
    author
        .run_in(&project, &["init", "--json"])
        .data("the author's topos init");
    author
        .run_in(
            &project,
            &["add", src.to_str().expect("utf-8 path"), "--json"],
        )
        .data("the author adopts the bundle folder");
    let genesis = publish(author, &project, "v1: the first truth");
    let v1 = version_of(&genesis);
    // The author's own machine takes the feed's copy — the copy `revert` reads.
    let mine = author
        .run(&["update", "-g", "--json"])
        .data("the owner's own machine-wide sweep");
    assert_eq!(row(&mine, BUNDLE)["action"], "installed", "{mine}");
    // The project's lock records the genesis version the way an `update` there would.
    author
        .run_in(&project, &["update", "--json"])
        .data("the project's first update");
    assert_eq!(locked_version(&project).as_deref(), Some(v1.as_str()));
    (project, v1)
}

/// Run the exact `undo:` line a receipt printed (`topos revert <name> --to <version>`), applied.
fn run_undo(machine: &Machine, cwd: &Path, undo: &str) -> Value {
    let mut argv: Vec<&str> = undo.split_whitespace().collect();
    assert_eq!(
        argv.first(),
        Some(&"topos"),
        "the undo line is a topos command: {undo}"
    );
    argv.remove(0);
    argv.push("--yes");
    argv.push("--json");
    machine
        .run_in(cwd, &argv)
        .data(&format!("the undo: {undo}"))
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// publish → revert (via the printed undo) → publish again / diff / update, from the same copy.
// ═══════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn after_a_revert_every_verb_decides_against_the_live_current() {
    let nonce = format!("truth-{}", std::process::id());
    let stack = start_stack("current-truth");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // ── 1. v1, then v2 from inside the project ──────────────────────────────────────────────────
    let author = Machine::new("truth-author");
    let (project, v1) = author_publishes_v1(&stack, &author, &owner, &nonce);
    let src = project.join(BUNDLE);
    write_bundle(&src, &v2_files(&nonce));
    let v2_receipt = publish(&author, &project, "v2: the reference gains a line");
    let v2 = version_of(&v2_receipt);
    assert_ne!(v1, v2);
    let undo = v2_receipt["undo"]
        .as_str()
        .expect("the receipt hands back the undo line")
        .to_owned();
    assert!(
        undo.starts_with(&format!("topos revert {BUNDLE} --to ")) && undo.contains(&v1[..12]),
        "the undo restores v1: {undo}"
    );

    // ── 2. the undo, exactly as printed ─────────────────────────────────────────────────────────
    let reverted = run_undo(&author, &project, &undo);
    let restored = reverted["new_version_id"]
        .as_str()
        .expect("the forward-revert commit")
        .to_owned();
    assert_ne!(restored, v1);
    assert_ne!(restored, v2);

    // The project copy still holds v2's bytes — the version the revert just undid. Nothing here
    // touched it, and the sidecar's cache still calls v2 current.
    assert_eq!(placed(&src), expected(&v2_files(&nonce)));

    // ── 3. `diff` shows the REAL difference against the live current ────────────────────────────
    let diffed = author
        .run_in(&project, &["diff", BUNDLE, "--json"])
        .data("the bare diff after the revert");
    assert_eq!(
        diffed["version_id"], restored,
        "the left side is the live current, not the cached base: {diffed}"
    );
    let body = diffed["diff"].as_str().expect("the diff body");
    assert!(
        body.contains("+- this is v2") && body.contains("docs/deeper/hop.md"),
        "the copy's v2 content shows as the difference: {body}"
    );

    // ── 4. `publish` previews a FORWARD publish, naming which version current is ────────────────
    let preview = author
        .run_in(&project, &["publish", BUNDLE, "-m", "v2 again", "--json"])
        .data("the bare publish after the revert");
    assert!(
        preview.get("result").is_none(),
        "never 'already published' over a copy that differs from current: {preview}"
    );
    let described = &preview["describe"];
    let republish = &described["republish"];
    assert_eq!(republish["current_version_id"], restored, "{described}");
    assert_eq!(republish["current_message"], "topos: revert", "{described}");
    assert_eq!(republish["copy_version_id"], v2, "{described}");
    let predicted = republish["new_version_id"]
        .as_str()
        .expect("the preview names the version the apply mints")
        .to_owned();
    // The counts are against the LIVE current (v1's bytes): one changed, two added, one of them
    // executable — and the undo names the live current.
    assert_eq!(described["changes"]["files"], 3, "{described}");
    assert_eq!(described["changes"]["added"], 2, "{described}");
    assert_eq!(described["changes"]["executable"], 1, "{described}");
    assert_eq!(
        described["undo"],
        format!("topos revert {BUNDLE} --to {restored}"),
        "{described}"
    );
    let tty = author
        .run_in(&project, &["publish", BUNDLE, "-m", "v2 again"])
        .stdout;
    assert!(
        tty.contains(&format!(
            "current is {} (\"topos: revert\"); your copy equals {} — publishing {}'s content \
             forward as {}",
            &restored[..12],
            &v2[..12],
            &v2[..12],
            &predicted[..12]
        )),
        "{tty}"
    );
    assert!(
        tty.contains(
            "Preview only — nothing published. 3 files changed (2 added, 1 executable). Lands \
             in everyone, no review required. Apply: topos publish truth-spine -m 'v2 again' \
             --yes"
        ),
        "{tty}"
    );
    assert!(!tty.contains("Nothing has changed yet"), "{tty}");

    // ── 5. applied: a NEW version whose content equals v2, parented on the revert ───────────────
    let again = publish(&author, &project, "v2 again");
    let v3 = version_of(&again);
    assert_eq!(
        v3, predicted,
        "the apply mints the version the preview named"
    );
    assert_ne!(v3, v2, "a new version, not the old id resurrected");
    assert_eq!(
        again["bundle_digest"], v2_receipt["bundle_digest"],
        "…whose content equals v2's: {again}"
    );
    assert_eq!(
        again["republish"]["current_version_id"], restored,
        "{again}"
    );
    assert_eq!(again["republish"]["copy_version_id"], v2, "{again}");
    assert_eq!(
        again["undo"],
        format!("topos revert {BUNDLE} --to {}", &restored[..12]),
        "{again}"
    );
    // And now the copy IS current — a republish says so, truthfully this time.
    let settled = author
        .run_in(&project, &["publish", BUNDLE, "--json"])
        .data("a publish over a copy that equals the live current");
    assert_eq!(settled["result"], "no_changes", "{settled}");

    // `log` shows every hop: v1, v2, the revert, and the forward publish.
    let versions = log_versions(&author, &project);
    for (id, msg) in [
        (&v1, "v1: the first truth"),
        (&v2, "v2: the reference gains a line"),
        (&restored, "topos: revert"),
        (&v3, "v2 again"),
    ] {
        assert!(
            versions.iter().any(|(i, m)| i == id && m == msg),
            "the log carries {msg}: {versions:?}"
        );
    }

    // ── 6. a second machine converges on the carried-forward content ────────────────────────────
    let dev_browser = stack.add_member("dev@acme.test", "member");
    let dev = Machine::new("truth-dev");
    login(&stack, &dev, &dev_browser);
    let installed = dev
        .run(&["update", "--json"])
        .data("the second machine's first sweep");
    assert_eq!(
        row(&installed, BUNDLE)["action"],
        "installed",
        "{installed}"
    );
    assert_converged(
        &dev,
        &v2_files(&nonce),
        "the forward publish reaches a fresh machine",
    );
}
