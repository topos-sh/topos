//! ONE workspace per command, end to end over the composed stack: the real `topos` binary against
//! the real web app, from a machine signed into TWO workspaces on one server.
//!
//! The state a by-hand journey hit: two logins, and a lookup about a bundle that exists in one of
//! them narrated `reading <host>/<a>` AND `reading <host>/<b>`, dialing a server that holds
//! nothing by that name. The rule these arms prove:
//!
//! - a bundle this machine already TRACKS carries its own workspace, so `log` reads that one and
//!   no other — one `reading …` line, naming it;
//! - `list --remote` is a lookup too: it shows the workspace the machine acts on (the default) and
//!   closes with ONE line naming the other login and the command that shows it;
//! - with several logins and no default at all, a lookup REFUSES and names both ways out rather
//!   than scanning every server.

mod common;

use std::path::{Path, PathBuf};

use common::{CliOut, OWNER_EMAIL, Stack, cli_binary, run_cli, start_stack};
use serde_json::Value;
use topos::test_support::SessionInstall;

/// The second workspace this machine also logs into.
const OTHER: &str = "beta";
/// The bundle published into the FIRST workspace — the tracked one every lookup is about.
const BUNDLE: &str = "deploy-guide";

/// One machine: the fixture rig (which owns the browser-approval ceremony) plus the real binary.
struct Machine {
    rig: SessionInstall,
    bin: PathBuf,
}

impl Machine {
    fn new(tag: &str) -> Self {
        Self {
            rig: SessionInstall::new(tag),
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
    /// Run the real binary from the install root — outside any checkout, so every verb acts on the
    /// machine scope.
    fn run(&self, args: &[&str]) -> CliOut {
        run_cli(
            &self.bin,
            &self.home(),
            &self.topos_home(),
            self.root(),
            args,
        )
    }
}

/// The machine's identity file, as the binary reads it.
fn sessions_file(machine: &Machine) -> PathBuf {
    machine.topos_home().join("identity").join("sessions.json")
}

/// Record a SECOND live login beside the real one — the row a `topos login <host>/<other>` leaves.
///
/// It is written rather than ceremonially minted because these arms are about what a lookup DOES
/// NOT dial: the second workspace's server is never supposed to be asked anything, so a row that
/// merely stands there is the whole fixture. A credential this suite never spends proves that
/// better than one it might.
fn join_second_workspace(machine: &Machine, stack: &Stack, workspace_id: &str, name: &str) {
    let path = sessions_file(machine);
    let mut doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let host = stack.origin.trim_start_matches("http://").to_owned();
    doc["sessions"]
        .as_array_mut()
        .expect("the sessions array")
        .push(serde_json::json!({
            "host": host,
            "base_url": stack.api_base,
            "workspace_id": workspace_id,
            "workspace_name": name,
            "display_name": name,
            "session_id": format!("sn_e2e_{name}"),
            "credential": "unspent-second-workspace-credential",
            "status": "active",
            "logged_in_at": 2,
        }));
    std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
}

/// Every `topos: reading …` phase line one run printed, in order.
fn reading_lines(out: &CliOut) -> Vec<String> {
    out.stderr
        .lines()
        .filter(|l| l.starts_with("topos: reading "))
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_lookup_acts_on_one_workspace() {
    let stack = start_stack("wsscope");
    let owner = stack.claim_owner(OWNER_EMAIL);
    let other_id = stack.add_workspace(OTHER, "Beta Corp");
    stack.seat_in(&other_id, OWNER_EMAIL, "owner");

    let machine = Machine::new("wsscope-cli");
    // The FIRST login stars itself as the machine default — the gcloud shape.
    stack.login_begin_and_approve(&machine.rig, &owner);
    let granted = machine.rig.login(None).expect("the login resume");
    assert_eq!(granted.session_status, "active");
    join_second_workspace(&machine, &stack, &other_id, OTHER);
    // A bundle published into the DEFAULT workspace, so the machine tracks it there.
    let src = machine.root().join(BUNDLE);
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# deploy guide\nship on green\n").unwrap();
    let added = machine.run(&["add", "-g", src.to_str().unwrap()]);
    assert_eq!(added.code, 0, "add: {} {}", added.stdout, added.stderr);
    let published = machine.run(&["publish", BUNDLE, "--yes"]);
    assert_eq!(
        published.code, 0,
        "publish: {} {}",
        published.stdout, published.stderr
    );

    // ---- 1. A TRACKED bundle: one workspace read, and it is the bundle's own.
    let logged = machine.run(&["log", BUNDLE]);
    assert_eq!(logged.code, 0, "log: {} {}", logged.stdout, logged.stderr);
    let read = reading_lines(&logged);
    assert_eq!(
        read.len(),
        1,
        "a tracked bundle's history reads ONE workspace: {read:?}"
    );
    assert!(
        read[0].ends_with(&format!("/{}", common::WS_NAME)),
        "and it is the workspace the bundle came from: {read:?}"
    );

    // ---- 2. `list --remote`: the resolved workspace, plus one closing line for the other login.
    let remote = machine.run(&["list", "--remote", "--json"]);
    assert_eq!(
        remote.code, 0,
        "list --remote: {} {}",
        remote.stdout, remote.stderr
    );
    let doc: Value = serde_json::from_str(&remote.stdout).expect("the list envelope");
    let shown: Vec<&str> = doc["data"]["remote"]
        .as_array()
        .expect("remote")
        .iter()
        .map(|w| w["workspace"]["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(
        shown,
        vec![common::WS_NAME],
        "the catalog view shows the workspace it acts on: {}",
        remote.stdout
    );
    let also: Vec<&str> = doc["data"]["also_signed_in"]
        .as_array()
        .expect("also_signed_in")
        .iter()
        .map(|w| w["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(also, vec![OTHER], "the other login is named, not read");

    let remote_tty = machine.run(&["list", "--remote"]);
    assert!(
        remote_tty.stdout.contains(&format!(
            "also logged into: {OTHER} — `topos list --remote --workspace {OTHER}`"
        )),
        "the closing line is the command that shows the other one: {}",
        remote_tty.stdout
    );

    // ---- 3. Several logins and NO default: refuse, naming both ways out.
    let sessions = sessions_file(&machine);
    let mut doc: Value = serde_json::from_slice(&std::fs::read(&sessions).unwrap()).unwrap();
    doc.as_object_mut().unwrap().remove("default");
    std::fs::write(&sessions, serde_json::to_vec(&doc).unwrap()).unwrap();

    let refused = machine.run(&["list", "--remote"]);
    assert_ne!(refused.code, 0, "the pick is not a guess: {refused:?}");
    let said = format!("{}{}", refused.stdout, refused.stderr);
    assert!(
        said.contains("you are logged into 2 workspaces")
            && said.contains(common::WS_NAME)
            && said.contains(OTHER),
        "the refusal names the workspaces: {said}"
    );
    assert!(
        said.contains("topos --workspace <name>") && said.contains("topos workspace use <name>"),
        "the refusal carries both runnable ways out: {said}"
    );
}
