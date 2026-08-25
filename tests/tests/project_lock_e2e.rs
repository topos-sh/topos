//! **THE LOCK RECORDS WHAT THE COPY HOLDS** — end to end over the composed stack: the real `topos`
//! binary against the real web app in front of the in-process vault.
//!
//! `topos.lock`'s own header calls it "the exact versions this project runs — commit it", and the
//! whole point of committing it is that `topos install --frozen` rebuilds the same folders on the
//! next machine. Two states put a version in it that no folder in the checkout held:
//!
//! - a LOCAL GO-BACK (`topos update <bundle>@<version>`) replaced the checkout's copy with an
//!   earlier version and left the lock naming the team's — so the committed file said the project
//!   runs a version nothing there holds, and `--frozen` would have rebuilt exactly the copy the
//!   go-back replaced;
//! - a STOPPED MERGE left every folder holding this person's own bytes while the lock advanced to
//!   the team's version, and publishing was blocked — the file claiming the project runs the very
//!   version the person had refused.
//!
//! Both arms read the same three surfaces against each other: what `topos.lock` records, what the
//! bytes on disk are, and what `list` says about the copy.
//!
//! Like the store suite this drives the REAL CLI BINARY as a subprocess over a fake `$HOME` — the
//! placement dirs resolve against the environment, so only a real process proves what landed where.
//! The fixture rig owns the browser login the machine inherits.

mod common;

use std::path::{Path, PathBuf};

use common::{CliOut, OWNER_EMAIL, Session, Stack, WS_NAME, cli_binary, run_cli, start_stack};
use serde_json::Value;
use topos::test_support::SessionInstall;

const BUNDLE: &str = "lock-truth";

/// The bundle's one file, with the step that every side of the scenario rewrites.
fn body(step_one: &str) -> String {
    format!("# {BUNDLE}\n\n1. {step_one}\n2. run the deploy\n")
}

fn write_bundle(dir: &Path, text: &str) {
    std::fs::create_dir_all(dir).expect("create the bundle dir");
    std::fs::write(dir.join("SKILL.md"), text).expect("write SKILL.md");
}

fn placed_text(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("SKILL.md")).unwrap_or_else(|e| {
        panic!("read the placed copy at {}: {e}", dir.display());
    })
}

// ── the machine ─────────────────────────────────────────────────────────────────────────────────

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

    /// The person's own pick FOR THAT CHECKOUT — personal and git-ignored, so a clone never
    /// carries one and a checkout without one places nothing.
    fn pick_in_project(&self, project_dir: &Path) {
        self.rig.pick_in_project(project_dir);
    }

    fn home(&self) -> PathBuf {
        self.rig.root().join("home")
    }

    fn topos_home(&self) -> PathBuf {
        self.rig.root().join(".topos")
    }

    fn run_in(&self, cwd: &Path, args: &[&str]) -> CliOut {
        run_cli(&self.bin, &self.home(), &self.topos_home(), cwd, args)
    }
}

fn login(stack: &Stack, machine: &Machine, approver: &Session) {
    stack.login_begin_and_approve(&machine.rig, approver);
    let granted = machine.rig.login(None).expect("the login resume");
    assert_eq!(granted.session_status, "active");
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

fn row<'a>(data: &'a Value, name: &str) -> &'a Value {
    data["skills"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["skill"] == name)
        .unwrap_or_else(|| panic!("no {name} row in the receipt: {data}"))
}

/// Every `code` the envelope's typed message channel carries.
fn message_codes(envelope: &Value) -> Vec<String> {
    envelope["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["code"].as_str())
        .map(str::to_owned)
        .collect()
}

/// The one message with `code`, as the text a person reads.
fn message_text<'a>(envelope: &'a Value, code: &str) -> &'a str {
    envelope["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["code"] == code)
        .and_then(|m| m["text"].as_str())
        .unwrap_or_else(|| panic!("no {code} message: {envelope}"))
}

fn version_of(receipt: &Value) -> String {
    receipt["version_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the publish landed a version id: {receipt}"))
        .to_owned()
}

/// Stand a machine up with the bundle published machine-wide at `first`, and a project whose
/// `topos.toml` asks for it by name and whose first `update` places the copy and writes the lock.
/// Returns `(the machine source dir, the project dir, the placed copy, the genesis version)`.
fn published_into_a_project(
    stack: &Stack,
    machine: &Machine,
    owner: &Session,
    first: &str,
) -> (PathBuf, PathBuf, PathBuf, String) {
    login(stack, machine, owner);
    let root = machine.root().canonicalize().expect("canonical root");
    let src = root.join("src").join(BUNDLE);
    write_bundle(&src, &body(first));
    machine
        .run_in(
            &root,
            &["add", "-g", src.to_str().expect("utf-8 path"), "--json"],
        )
        .data("the bundle is adopted machine-wide");
    let genesis = machine
        .run_in(&root, &["publish", BUNDLE, "--yes", "-m", "v1", "--json"])
        .data("the genesis publish");
    let v1 = version_of(&genesis);

    let project = root.join("proj");
    std::fs::create_dir_all(&project).expect("create the project dir");
    machine
        .run_in(&project, &["init", "--json"])
        .data("the project's topos init");
    machine
        .run_in(&project, &["add", BUNDLE, "--json"])
        .data("the project asks for the bundle by name");
    let placed_row = machine
        .run_in(&project, &["update", "--json"])
        .data("the project's first update");
    let copy = row(&placed_row, BUNDLE)["destinations"][0]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| project.join(".claude").join("skills").join(BUNDLE));
    let copy = if copy.is_absolute() {
        copy
    } else {
        project.join(copy)
    };
    assert_eq!(placed_text(&copy), body(first), "{placed_row}");
    assert_eq!(locked_version(&project).as_deref(), Some(v1.as_str()));
    (src, project, copy, v1)
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// ① A LOCAL GO-BACK: the lock records the version the go-back put back, so `--frozen` reproduces
//    the folder that stands there.
// ═══════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn a_go_back_records_the_version_the_copy_now_holds() {
    let stack = start_stack("lock-truth-goback");
    let owner = stack.claim_owner(OWNER_EMAIL);
    let machine = Machine::new("lock-truth-goback");
    let (src, project, copy, v1) =
        published_into_a_project(&stack, &machine, &owner, "check the region");

    // The team moves on, and the project takes it: the lock records v2 and the copy holds it.
    write_bundle(&src, &body("check the region and the account"));
    let v2 = version_of(
        &machine
            .run_in(
                machine.root(),
                &["publish", BUNDLE, "--yes", "-m", "v2", "--json"],
            )
            .data("v2"),
    );
    assert_ne!(v1, v2);
    machine
        .run_in(&project, &["update", "--json"])
        .data("the project takes v2");
    assert_eq!(locked_version(&project).as_deref(), Some(v2.as_str()));
    assert_eq!(placed_text(&copy), body("check the region and the account"));

    // A `-g` go-back run from INSIDE the checkout takes the MACHINE's copy back, and this
    // checkout's own folder is untouched — so its lock must be too. The rule is not "a go-back
    // moves the nearest lock", it is "the lock records what THIS copy holds".
    let short = &v1[..12];
    machine
        .run_in(
            &project,
            &["update", "-g", &format!("{BUNDLE}@{short}"), "--json"],
        )
        .envelope("the machine-scope go-back, run from inside the checkout");
    assert_eq!(
        locked_version(&project).as_deref(),
        Some(v2.as_str()),
        "a machine-scope go-back leaves the checkout's lock alone"
    );
    assert_eq!(placed_text(&copy), body("check the region and the account"));

    // ── the go-back: the documented machine-only rollback, run inside the checkout ──────────────
    let went_back = machine
        .run_in(
            &project,
            &["update", &format!("{BUNDLE}@{short}"), "--json"],
        )
        .envelope("the go-back");
    assert_eq!(
        row(&went_back["data"], BUNDLE)["action"],
        "held",
        "{went_back}"
    );
    // The BYTES moved back…
    assert_eq!(placed_text(&copy), body("check the region"));
    // …and so did the file the project commits, which is the whole point of committing it.
    assert_eq!(
        locked_version(&project).as_deref(),
        Some(v1.as_str()),
        "the lock records the version the copy holds, not the one it left"
    );
    assert!(
        message_codes(&went_back).contains(&"LOCK_MOVED".to_owned()),
        "the receipt names the lock it moved: {went_back}"
    );
    assert_eq!(
        message_text(&went_back, "LOCK_MOVED"),
        format!(
            "topos.lock now records {BUNDLE}@{} — the version this copy holds",
            &v1[..12]
        ),
        "{went_back}"
    );

    // The proof that the record is worth anything: a FROZEN install reproduces exactly this copy
    // rather than rebuilding the version the go-back replaced.
    machine
        .run_in(&project, &["install", "--frozen", "--json"])
        .data("a frozen install over the go-back's lock");
    assert_eq!(
        placed_text(&copy),
        body("check the region"),
        "--frozen reproduces what the lock records"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// ② A STOPPED MERGE: every folder keeps this person's bytes, so the lock keeps the version it had.
// ═══════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn a_stopped_merge_leaves_the_lock_at_the_version_the_copy_holds() {
    let stack = start_stack("lock-truth-conflict");
    let owner = stack.claim_owner(OWNER_EMAIL);
    let machine = Machine::new("lock-truth-conflict");
    let (src, project, copy, v1) =
        published_into_a_project(&stack, &machine, &owner, "check the region");

    // This person edits the project copy; the team changes the SAME line and publishes.
    let mine = body("check the region twice");
    std::fs::write(copy.join("SKILL.md"), &mine).expect("edit the project copy");
    write_bundle(&src, &body("check the region and the account"));
    let v2 = version_of(
        &machine
            .run_in(
                machine.root(),
                &["publish", BUNDLE, "--yes", "-m", "v2", "--json"],
            )
            .data("the team's v2"),
    );
    assert_ne!(v1, v2);

    // The ordinary update merges — and STOPS.
    let swept = machine
        .run_in(&project, &["update", "--json"])
        .envelope("the merging update");
    assert_eq!(
        row(&swept["data"], BUNDLE)["action"],
        "conflicted",
        "{swept}"
    );
    // The copy an agent reads still holds this person's own bytes…
    assert_eq!(placed_text(&copy), mine);
    // …so the committed file still records the version this project runs, not the one it refused.
    assert_eq!(
        locked_version(&project).as_deref(),
        Some(v1.as_str()),
        "the lock never records a version no folder here holds: {swept}"
    );
    assert!(
        message_codes(&swept).contains(&"LOCK_STANDS".to_owned()),
        "the receipt says the lock stood, and why: {swept}"
    );
    assert_eq!(
        message_text(&swept, "LOCK_STANDS"),
        format!(
            "{BUNDLE}: the merge here has stopped — topos.lock keeps the version this copy holds"
        ),
        "{swept}"
    );

    // The third surface agrees with the other two: the copy is not at the team's version.
    let listed = machine
        .run_in(&project, &["list", BUNDLE, "--json"])
        .data("the listing over the blocked bundle");
    assert!(
        !serde_json::to_string(&listed)
            .unwrap_or_default()
            .contains(&v2),
        "nothing here claims the copy runs the team's version: {listed}"
    );

    // Resolving it the team's way moves BOTH together — the rule is not "never move", it is
    // "move with the copy".
    machine
        .run_in(&project, &["update", BUNDLE, "--reset", "--yes", "--json"])
        .data("take the team's version");
    assert_eq!(placed_text(&copy), body("check the region and the account"));
    machine
        .run_in(&project, &["update", "--json"])
        .data("the update after the reset");
    assert_eq!(
        locked_version(&project).as_deref(),
        Some(v2.as_str()),
        "once the copy holds the team's version, so does the lock"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// ③ A FRESH CLONE: the two committed files are ALL a checkout has — `--frozen` builds the rest.
// ═══════════════════════════════════════════════════════════════════════════════════════════════

/// **THE `npm ci` PROPERTY, ON A REAL CLONE.** A project's own store is gitignored whole
/// (`.topos/.gitignore` holds `*`), so what a teammate's `git clone` actually leaves on disk is
/// exactly two topos files: `topos.toml` and `topos.lock`. A frozen install there has to create
/// whatever local store it needs and place exactly the versions the lock names — the whole point
/// of committing the file. It refuses only when the lock and the manifest disagree, or a locked
/// version cannot be fetched.
///
/// The team publishes a newer version the clone's lock does not name, so "it installed something"
/// is not enough to pass: the bytes that land must be the LOCKED ones.
#[test]
fn a_frozen_install_on_a_fresh_clone_places_the_locked_versions() {
    let stack = start_stack("lock-truth-clone");
    let owner = stack.claim_owner(OWNER_EMAIL);
    let machine = Machine::new("lock-truth-clone");
    let (src, project, copy, v1) =
        published_into_a_project(&stack, &machine, &owner, "check the region");

    // The team moves on WITHOUT this project — so the clone has something to be right about.
    write_bundle(&src, &body("check the region and the account"));
    let v2 = version_of(
        &machine
            .run_in(
                machine.root(),
                &["publish", BUNDLE, "--yes", "-m", "v2", "--json"],
            )
            .data("the team's v2"),
    );
    assert_ne!(v1, v2);
    assert_eq!(
        locked_version(&project).as_deref(),
        Some(v1.as_str()),
        "the project never took v2, so its committed lock still names v1"
    );

    // ── the clone: the two committed files, and nothing else ────────────────────────────────────
    let manifest = std::fs::read_to_string(project.join("topos.toml")).expect("read topos.toml");
    let lock = std::fs::read_to_string(project.join("topos.lock")).expect("read topos.lock");
    let clone = machine
        .root()
        .canonicalize()
        .expect("canonical root")
        .join("clone");
    std::fs::create_dir_all(clone.join(".git")).expect("a git-shaped checkout");
    std::fs::write(clone.join("topos.toml"), &manifest).expect("the committed manifest");
    std::fs::write(clone.join("topos.lock"), &lock).expect("the committed lock");
    assert!(
        !clone.join(".topos").exists(),
        "a real clone carries no project store — that is the state under test"
    );
    // The person picks the agents FOR THIS CHECKOUT — `topos init -a` in the real motion. The pick
    // is personal and git-ignored, so it is never what the clone carried; it is the first thing
    // its owner does with it, and nothing is placed here before they do.
    machine.pick_in_project(&clone);

    let frozen = machine.run_in(&clone, &["install", "--frozen", "--json"]);
    assert_eq!(
        frozen.code, 0,
        "a fresh clone is the state --frozen exists for: {} {}",
        frozen.stdout, frozen.stderr
    );
    let placed_row = frozen.data("the frozen install on the fresh clone");

    // The bytes: exactly what the lock records, byte-identical to the copy the author's checkout
    // holds — not the version the workspace serves today.
    let landed = row(&placed_row, BUNDLE)["destinations"][0]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| clone.join(".claude").join("skills").join(BUNDLE));
    let landed = if landed.is_absolute() {
        landed
    } else {
        clone.join(landed)
    };
    assert_eq!(
        placed_text(&landed),
        body("check the region"),
        "--frozen places the version topos.lock names: {placed_row}"
    );
    assert_eq!(
        placed_text(&landed),
        placed_text(&copy),
        "the clone reproduces the author's checkout byte for byte"
    );

    // The committed file is untouched: an install never moves a lock entry.
    assert_eq!(
        std::fs::read_to_string(clone.join("topos.lock")).expect("read the clone's lock"),
        lock,
        "--frozen never rewrites the file it was given"
    );
    assert_eq!(locked_version(&clone).as_deref(), Some(v1.as_str()));

    // ── THE REFUSAL A LOCK NOBODY CAN SERVE GETS ────────────────────────────────────────────────
    // A version this workspace will never hand over is a SETTLED fact: the same command meets the
    // same answer forever, and the way out is a login that reaches the workspace carrying it, or
    // a lock that names something else. It answered `IO_ERROR` / `RETRYABLE_FAILURE` with the
    // prose "this is often transient — running it again is safe", which is an instruction to loop.
    let unserved = "deadbeef".repeat(8);
    assert_ne!(unserved, v1);
    let broken = machine
        .root()
        .canonicalize()
        .expect("canonical root")
        .join("clone-broken");
    std::fs::create_dir_all(broken.join(".git")).expect("a git-shaped checkout");
    machine.pick_in_project(&broken);
    std::fs::write(broken.join("topos.toml"), &manifest).expect("the committed manifest");
    std::fs::write(broken.join("topos.lock"), lock.replace(&v1, &unserved))
        .expect("a lock naming a version nobody serves");

    let refused = machine
        .run_in(&broken, &["install", "--frozen", "--json"])
        .refusal("a lock naming an unserved version");
    assert_eq!(
        refused["error"]["code"], "NOT_FOUND",
        "the uniform not-found, met one level down at a version id: {refused}"
    );
    assert_eq!(
        refused["error"]["outcome"], "PERMANENT_FAILURE",
        "a re-run meets the identical answer — never the retry class: {refused}"
    );
    assert_eq!(refused["error"]["retryable"], false, "{refused}");
    let text = refused["error"]["context"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.starts_with(&format!("the lock names {BUNDLE}@{}", &unserved[..12])),
        "the refusal leads with the entry it could not honour: {refused}"
    );
    assert!(
        text.contains("does not serve that version to this login"),
        "…says what the server answered: {refused}"
    );
    assert!(
        text.contains(&format!("`topos update {BUNDLE}`")) && text.contains("`topos login "),
        "…and names both cures: {refused}"
    );
    assert!(
        !text.contains("safe to run again"),
        "nothing here heals on a re-run: {refused}"
    );
    let argvs: Vec<Value> = refused["next_actions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|a| a["argv"].clone())
        .collect();
    assert!(
        argvs.contains(&serde_json::json!(["topos", "update", BUNDLE, "--json"])),
        "the lock-changing cure rides as argv: {refused}"
    );
    assert!(
        argvs.iter().any(|a| {
            let tokens: Vec<&str> = a
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            tokens.starts_with(&["topos", "login"])
                && tokens.get(2).is_some_and(|t| t.contains(WS_NAME))
        }),
        "…and so does the login, with the workspace address as ONE token: {refused}"
    );
    assert!(
        !argvs.contains(&serde_json::json!([
            "topos", "install", "--frozen", "--json"
        ])),
        "a permanent refusal never offers its own re-run: {refused}"
    );
    assert!(
        !broken.join(".claude").join("skills").join(BUNDLE).exists(),
        "the refusal placed nothing"
    );
}

/// **A SERVER THAT IS NOT ANSWERING IS THE ONE FROZEN REFUSAL WORTH RETRYING.** The sibling above
/// pins the permanent half; this is the other, and the split is the whole point — an agent that
/// reads one outcome for both either loops on a settled lock or gives up on a blip.
///
/// The server is killed under a machine whose session, store, and committed files are all exactly
/// as they were, on a fresh clone with no store of its own to fall back on — the shape a CI runner
/// meets when the network goes.
#[test]
fn a_frozen_install_calls_an_unreachable_server_retryable() {
    let mut stack = start_stack("lock-truth-fault");
    let owner = stack.claim_owner(OWNER_EMAIL);
    let machine = Machine::new("lock-truth-fault");
    let (_src, project, _copy, v1) =
        published_into_a_project(&stack, &machine, &owner, "check the region");

    // The clone: the two committed files, no project store — the state `--frozen` exists for.
    let manifest = std::fs::read_to_string(project.join("topos.toml")).expect("read topos.toml");
    let lock = std::fs::read_to_string(project.join("topos.lock")).expect("read topos.lock");
    let clone = machine
        .root()
        .canonicalize()
        .expect("canonical root")
        .join("clone-dark");
    std::fs::create_dir_all(clone.join(".git")).expect("a git-shaped checkout");
    machine.pick_in_project(&clone);
    std::fs::write(clone.join("topos.toml"), &manifest).expect("the committed manifest");
    std::fs::write(clone.join("topos.lock"), &lock).expect("the committed lock");
    assert_eq!(locked_version(&clone).as_deref(), Some(v1.as_str()));

    // ── the fault ───────────────────────────────────────────────────────────────────────────────
    stack.stop_app();

    let refused = machine
        .run_in(&clone, &["install", "--frozen", "--json"])
        .refusal("a frozen install against a server that stopped answering");
    assert_eq!(
        refused["error"]["code"], "IO_ERROR",
        "a fault is the filesystem family's word, as it always was: {refused}"
    );
    assert_eq!(
        refused["error"]["outcome"], "RETRYABLE_FAILURE",
        "the one frozen refusal a re-run can get past: {refused}"
    );
    assert_eq!(refused["error"]["retryable"], true, "{refused}");
    let text = refused["error"]["context"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains(&format!("could not stage bundle {BUNDLE}")),
        "the refusal says bundle, and names it: {refused}"
    );
    assert!(
        text.contains("could not read a connected workspace's list of bundles"),
        "a server that did not answer is named as one: {refused}"
    );
    assert!(
        !text.contains("no connected catalog serves it"),
        "…and never as a workspace that has settled the question and does not carry it: {refused}"
    );
    assert!(
        text.contains("nothing was placed, so `topos install --frozen` is safe to run again"),
        "the state it left, and the command to run: {refused}"
    );
    assert!(
        refused["next_actions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|a| a["argv"] == serde_json::json!(["topos", "install", "--frozen", "--json"])),
        "the same command rides as argv, not only as prose: {refused}"
    );
    assert!(
        !clone.join(".claude").join("skills").join(BUNDLE).exists(),
        "the refusal placed nothing"
    );
    assert_eq!(
        std::fs::read_to_string(clone.join("topos.lock")).expect("read the clone's lock"),
        lock,
        "…and wrote nothing"
    );
}
