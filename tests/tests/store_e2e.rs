//! The STORAGE SPINE, end to end over the composed stack: the real `topos` binary, driving the real
//! web app, in front of a vault whose bytes live on an `object_store` seam — a local directory in the
//! first test, an S3-compatible bucket (a local MinIO) in the second.
//!
//! **The first property: the whole byte loop is the same loop whatever holds the bytes.** One person
//! publishes a bundle of several files (one nested, one executable), a second machine installs it, a
//! v2 lands, the second machine's ordinary `topos update` converges on it BYTE FOR BYTE, and the three
//! read verbs the history rests on — `log`, `diff`, and the web app's own authenticated history
//! surface — all answer with the two versions and the one file that changed. Then a `revert` moves the
//! team pointer FORWARD onto the old bytes and the second machine converges on those.
//!
//! **The second property — the one this suite exists for: the vault is STATELESS.** With the bytes in
//! a bucket, the whole vault is torn down (its serve task aborted, every local dir it used deleted)
//! and rebuilt over a fresh authority with a brand-new EMPTY staging root, on the same loopback
//! address the running web app has been dialling all along. Nothing about the app or the two client
//! machines changes. Against that vault — which has never written a byte of its own to local disk —
//! `update`, `log` and `diff` still answer, and a THIRD, empty machine home installs the bundle from
//! scratch, fetching every object through it. Postgres and the bucket survived; nothing else did, and
//! nothing else needed to.
//!
//! The MinIO leg is GATED on reachability: no bucket on 127.0.0.1:9010 and the test says so loudly on
//! stderr and returns — a missing local rig is not a failing property.
//!
//! Run-uniqueness needs no invention here: the boot-minted workspace's row id is random per stack
//! (`w_<16 random bytes>`) and every object key is prefixed by it, so two runs against one bucket
//! cannot collide even before the keys' content addressing makes a re-put a no-op. The bundle bytes
//! carry this process's own nonce on top of that.
//!
//! Like the conflict and MCP suites this one drives the REAL CLI BINARY as a subprocess over fake
//! `$HOME`s — the placement dirs resolve against the environment, so only a real process proves what
//! landed where. The fixture rig owns the browser login each machine inherits.

mod common;

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{
    CliOut, OWNER_EMAIL, Session, Stack, cli_binary, run_cli, start_stack, start_stack_with_store,
};
use serde_json::Value;
use topos::test_support::SessionInstall;

// ── the session's MinIO (the S3 leg's rig) ──────────────────────────────────────────────────────

const MINIO_ENDPOINT: &str = "http://127.0.0.1:9010";
const MINIO_ADDR: &str = "127.0.0.1:9010";
const MINIO_BUCKET: &str = "topos-test";
const MINIO_KEY: &str = "topostest";
const MINIO_SECRET: &str = "topostest123";
const MINIO_REGION: &str = "us-east-1";

/// Whether this session's MinIO answers a TCP connect. A missing local rig SKIPS the S3 leg loudly
/// rather than failing it — the property is about the vault, not about who remembered to start a
/// container.
fn minio_is_up() -> bool {
    let addr: SocketAddr = MINIO_ADDR.parse().expect("the MinIO address parses");
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

// ── the bundle under test ───────────────────────────────────────────────────────────────────────

/// The bundle every arc publishes. Several files, one of them under a nested path and one of them
/// executable — a single-file bundle would prove nothing about a tree of objects.
const BUNDLE: &str = "store-spine";

/// One file of the bundle as the suite states it: `(bundle-relative path, executable, bytes)`.
type File = (&'static str, bool, String);

/// The bundle at v1. `nonce` makes each RUN's bytes its own, so a shared bucket's leftovers are
/// never what an assertion reads (the content-addressed keys make a re-put harmless either way).
fn v1_files(nonce: &str) -> Vec<File> {
    vec![
        (
            "SKILL.md",
            false,
            format!(
                "# {BUNDLE}\n\nProve the byte spine end to end ({nonce}).\n\nrun: scripts/run.sh\n"
            ),
        ),
        (
            "docs/reference.md",
            false,
            format!("# reference ({nonce})\n\n- every object is content-addressed\n- this is v1\n"),
        ),
        (
            "scripts/run.sh",
            true,
            format!("#!/bin/sh\necho \"spine v1 {nonce}\"\n"),
        ),
    ]
}

/// The bundle at v2 — ONE file changes, the nested one. The other two are byte-identical to v1, so
/// the version-to-version diff has exactly one file to name and the `update` that converges on v2
/// must leave the executable's bit exactly as it was.
fn v2_files(nonce: &str) -> Vec<File> {
    let mut files = v1_files(nonce);
    for file in &mut files {
        if file.0 == CHANGED {
            file.2 = format!(
                "# reference ({nonce})\n\n- every object is content-addressed\n- this is v2\n- \
                 and the store is a seam\n"
            );
        }
    }
    files
}

/// The one file v2 rewrites — what `topos diff` must name, and what it must be alone in naming.
const CHANGED: &str = "docs/reference.md";
/// A file v2 leaves alone — the negative half of the diff assertion.
const UNCHANGED: &str = "scripts/run.sh";

/// Write a version of the bundle into `dir`, exec bits and nested dirs included.
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

/// What a placement dir HOLDS, in the shape the assertions compare: `(rel path, executable, bytes)`,
/// sorted, with the delivery's own self-ignore sentinel dropped — topos writes that file, the
/// publisher never did.
fn placed(dir: &Path) -> Vec<(String, bool, Vec<u8>)> {
    let mut out: Vec<(String, bool, Vec<u8>)> = SessionInstall::dir_files(dir)
        .into_iter()
        .filter(|(rel, _, _)| rel != ".gitignore")
        .map(|(rel, mode, bytes)| (rel, mode != 0, bytes))
        .collect();
    out.sort();
    out
}

/// The published files in that same shape — what every placement must equal, byte for byte.
fn expected(files: &[File]) -> Vec<(String, bool, Vec<u8>)> {
    let mut out: Vec<(String, bool, Vec<u8>)> = files
        .iter()
        .map(|(rel, exec, body)| ((*rel).to_owned(), *exec, body.clone().into_bytes()))
        .collect();
    out.sort();
    out
}

// ── the machines ────────────────────────────────────────────────────────────────────────────────

/// One machine: a fake `$HOME` where TWO harnesses with their own private skills dirs are detected
/// (so a delivered bundle lands in more than one agent-readable folder, and "the bytes converged" is
/// a claim about all of them) over the fixture rig's `~/.topos`, where the login it performed lives.
struct Machine {
    rig: SessionInstall,
    bin: PathBuf,
}

impl Machine {
    fn new(tag: &str) -> Self {
        let rig = SessionInstall::new(tag);
        let home = rig.root().join("home");
        // Neither is covered by the shared `~/.agents/skills` dir, so each gets its own placement.
        for dir in [".claude", ".codex"] {
            std::fs::create_dir_all(home.join(dir)).expect("seed a harness detect dir");
        }
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

    /// This machine's store (`~/.topos`) — where the login the fixture rig performed lives, and
    /// where the machine-scope records are.
    fn topos_home(&self) -> PathBuf {
        self.rig.root().join(".topos")
    }

    /// Run the real binary from the install root — OUTSIDE any checkout, so the scope it acts in is
    /// the machine's own.
    fn run(&self, args: &[&str]) -> CliOut {
        self.run_in(self.root(), args)
    }

    /// Run the real binary standing in `cwd` (the publisher stands in its project).
    fn run_in(&self, cwd: &Path, args: &[&str]) -> CliOut {
        run_cli(&self.bin, &self.home(), &self.topos_home(), cwd, args)
    }
}

/// Log a machine in through the REAL browser-approval ceremony (the fixture rig owns the flow; the
/// binary inherits the session it lands).
fn login(stack: &Stack, machine: &Machine, approver: &Session) {
    stack.login_begin_and_approve(&machine.rig, approver);
    let granted = machine.rig.login(None).expect("the login resume");
    assert_eq!(granted.session_status, "active");
}

// ── reading what a machine holds ────────────────────────────────────────────────────────────────

/// Every folder the machine store records as a placement of `name` — read from the map the engine
/// itself writes, never guessed from the disk.
fn placements(machine: &Machine, name: &str) -> Vec<PathBuf> {
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

/// Assert EVERY folder this machine placed `BUNDLE` in holds exactly `files`, byte for byte, exec
/// bits included — the whole point of a delivery, read off the real disk.
fn assert_converged(machine: &Machine, files: &[File], what: &str) {
    let dirs = placements(machine, BUNDLE);
    assert!(
        dirs.len() >= 2,
        "{what}: the bundle landed in both detected agents' folders: {dirs:?}"
    );
    for dir in &dirs {
        assert_eq!(
            placed(dir),
            expected(files),
            "{what}: {} holds the published bytes",
            dir.display()
        );
    }
}

/// One bundle's row in an `update` receipt.
fn row<'a>(data: &'a Value, name: &str) -> &'a Value {
    data["skills"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["skill"] == name)
        .unwrap_or_else(|| panic!("no {name} row in the receipt: {data}"))
}

/// Publish the bundle standing in the author's project — the whole receipt, so a caller can take
/// the version it landed and (on the genesis publish) the bundle id the workspace filed it under.
fn publish(author: &Machine, project: &Path, message: &str) -> Value {
    author
        .run_in(
            project,
            &["publish", BUNDLE, "--yes", "-m", message, "--json"],
        )
        .data(&format!("publish {message}"))
}

/// One publish's version id.
fn version_of(receipt: &Value) -> String {
    receipt["version_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the publish landed a version id: {receipt}"))
        .to_owned()
}

/// Stand the AUTHOR up: a machine, a logged-in session, a project holding the bundle source, the
/// adopt that records it, and the genesis publish. Returns `(project dir, the genesis receipt)`.
fn author_publishes_v1(
    stack: &Stack,
    author: &Machine,
    owner: &Session,
    nonce: &str,
) -> (PathBuf, Value) {
    login(stack, author, owner);
    // Canonicalized so the adopt's dir-relative manifest spelling holds on macOS (`/var` is a
    // symlink to `/private/var` — the scan canonicalizes, and the manifest dir must match).
    let project = author.root().canonicalize().expect("canonical author root");
    let src = project.join(BUNDLE);
    write_bundle(&src, &v1_files(nonce));
    // Exactly what a person does: mint the project's manifest, then record the folder in it.
    author
        .run_in(&project, &["init", "--json"])
        .data("the author's topos init");
    author
        .run_in(
            &project,
            &["add", src.to_str().expect("utf-8 path"), "--json"],
        )
        .data("the author adopts the bundle folder");
    let genesis = publish(author, &project, "v1: the first spine");
    (project, genesis)
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// 1. The whole loop over the LOCAL byte store.
// ═══════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn full_loop_on_the_local_store() {
    let nonce = format!("local-{}", std::process::id());
    let stack = start_stack("store-local");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // ── 1. the author publishes v1 ──────────────────────────────────────────────────────────────
    let author = Machine::new("store-local-author");
    let (project, genesis) = author_publishes_v1(&stack, &author, &owner, &nonce);
    let v1 = version_of(&genesis);
    // The bundle id the workspace filed it under — the key the app's history lane is addressed by.
    let skill_id = genesis["skill_id"]
        .as_str()
        .expect("the genesis receipt names the bundle id")
        .to_owned();

    // ── 2. a SECOND machine — its own home, its own login — installs it ─────────────────────────
    let dev_browser = stack.add_member("dev@acme.test", "member");
    let dev = Machine::new("store-local-dev");
    login(&stack, &dev, &dev_browser);
    let installed = dev
        .run(&["update", "--json"])
        .data("the second machine's first sweep");
    assert_eq!(
        row(&installed, BUNDLE)["action"],
        "installed",
        "the bundle installs on the bare sweep: {installed}"
    );
    assert_converged(&dev, &v1_files(&nonce), "the first install");

    // ── 3. v2 from the first home; the second machine's ordinary sweep converges on it ──────────
    write_bundle(&project.join(BUNDLE), &v2_files(&nonce));
    let v2 = version_of(&publish(
        &author,
        &project,
        "v2: the reference gains a line",
    ));
    assert_ne!(v1, v2, "v2 is its own version");
    let swept = dev
        .run(&["update", "--json"])
        .data("the second machine's v2 sweep");
    assert_eq!(
        row(&swept, BUNDLE)["action"],
        "fast_forwarded",
        "the sweep moves to v2: {swept}"
    );
    assert_converged(&dev, &v2_files(&nonce), "the v2 sweep");

    // ── 4. `log` — both versions, each with the message its publish carried ─────────────────────
    let log = dev
        .run(&["log", BUNDLE, "--limit", "0", "--json"])
        .data("the history");
    let versions: Vec<(String, String)> = log["events"]
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
        .collect();
    assert!(
        versions.contains(&(v1.clone(), "v1: the first spine".to_owned())),
        "the log carries v1 with its message: {log}"
    );
    assert!(
        versions.contains(&(v2.clone(), "v2: the reference gains a line".to_owned())),
        "the log carries v2 with its message: {log}"
    );

    // ── 5. `diff` between the two versions names the ONE file that changed ──────────────────────
    let diff = dev
        .run(&["diff", BUNDLE, &format!("{v1}..{v2}"), "--json"])
        .data("the version-to-version diff");
    let body = diff["diff"].as_str().expect("the diff body");
    assert!(
        body.contains(CHANGED),
        "the diff names the changed file: {body}"
    );
    assert!(!body.contains(UNCHANGED), "and names nothing else: {body}");
    assert!(
        body.contains("this is v2"),
        "the diff carries the new line: {body}"
    );

    // ── 6. `revert` moves the team pointer FORWARD onto v1's bytes ──────────────────────────────
    // A revert is a REVIEWER's act, so it comes from the owner's machine. It reads the machine's
    // own record of the bundle, which the login's feed line delivers on an ordinary `-g` sweep —
    // the author has only ever held this bundle in their project until now.
    let mine = author
        .run(&["update", "-g", "--json"])
        .data("the owner's own machine-wide sweep");
    assert_eq!(
        row(&mine, BUNDLE)["action"],
        "installed",
        "the author's own machine takes the feed's copy: {mine}"
    );
    let reverted = author
        .run(&["revert", BUNDLE, "--to", &v1, "--yes", "--json"])
        .data("the revert");
    assert_eq!(reverted["reverted_to"], v1.as_str(), "{reverted}");
    let new_version = reverted["new_version_id"]
        .as_str()
        .expect("the forward-revert commit")
        .to_owned();
    assert_ne!(
        new_version, v1,
        "a revert moves forward, it does not rewind"
    );
    assert_ne!(new_version, v2, "and it is its own version");
    // The second machine's ordinary sweep takes it, and the old bytes are back on disk.
    let back = dev.run(&["update", "--json"]).data("the post-revert sweep");
    assert_eq!(
        row(&back, BUNDLE)["action"],
        "fast_forwarded",
        "the revert reaches the machine as an ordinary move: {back}"
    );
    assert_converged(&dev, &v1_files(&nonce), "the revert");

    // ── 7. the WEB APP's own history surface, read under authentication ─────────────────────────
    // Two doors, both the app's: the session lane's JSON (a bearer the real login ceremony minted,
    // exactly what a machine holds) and the signed-in browser page (the owner's cookie session).
    let probe = stack.mint_session(&owner, "store-history-probe");
    let lane = stack.device_get(
        &probe.credential,
        &format!(
            "/v1/workspaces/{}/skills/{skill_id}/log",
            stack.workspace_id
        ),
    );
    assert_eq!(lane.status, 200, "the history lane answers: {}", lane.body);
    let lane: Value = serde_json::from_str(&lane.body).expect("the history lane's JSON");
    let listed: Vec<(String, String)> = lane["versions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| {
            (
                v["version_id"].as_str().unwrap_or_default().to_owned(),
                v["message"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    assert!(
        listed
            .iter()
            .any(|(id, msg)| *id == v1 && msg == "v1: the first spine"),
        "the app's history lists v1: {lane}"
    );
    assert!(
        listed
            .iter()
            .any(|(id, msg)| *id == v2 && msg == "v2: the reference gains a line"),
        "the app's history lists v2: {lane}"
    );
    assert!(
        listed.iter().any(|(id, _)| *id == new_version),
        "…and the revert that followed them: {lane}"
    );
    assert_eq!(
        lane["versions"][0]["current"], true,
        "the head of the walk is the current version: {lane}"
    );

    // The signed-in PAGE the same facts render on (the owner's cookie session, no bounce).
    let page = owner.get(&format!("/skills/{BUNDLE}/history"));
    assert_eq!(
        page.status, 200,
        "the history page renders for a member: {}",
        page.body
    );
    assert!(
        page.body.contains("v1: the first spine")
            && page.body.contains("v2: the reference gains a line"),
        "the history page lists the versions' messages: {}",
        page.body
    );
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// 2. The same loop over an S3 bucket — and then the vault is thrown away and rebuilt empty.
// ═══════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn stateless_vault_on_minio() {
    if !minio_is_up() {
        eprintln!(
            "\n\n!!! store e2e SKIPPED its MinIO leg — start MinIO on 9010 (bucket {MINIO_BUCKET}, \
             key {MINIO_KEY}) and re-run; the stateless-vault property was NOT proved !!!\n\n"
        );
        return;
    }

    let nonce = format!("minio-{}", std::process::id());
    let stack = start_stack_with_store(
        "store-minio",
        Some(plane_store::StoreConfig::S3 {
            endpoint: MINIO_ENDPOINT.to_owned(),
            bucket: MINIO_BUCKET.to_owned(),
            access_key_id: MINIO_KEY.to_owned(),
            secret_access_key: MINIO_SECRET.to_owned(),
            region: MINIO_REGION.to_owned(),
        }),
    );
    let owner = stack.claim_owner(OWNER_EMAIL);

    // ── 1. publish → install on a second machine → v2 → update, all through the bucket ──────────
    let author = Machine::new("store-minio-author");
    let (project, genesis) = author_publishes_v1(&stack, &author, &owner, &nonce);
    let v1 = version_of(&genesis);

    let dev_browser = stack.add_member("dev@acme.test", "member");
    let dev = Machine::new("store-minio-dev");
    login(&stack, &dev, &dev_browser);
    let installed = dev
        .run(&["update", "--json"])
        .data("the second machine's first sweep");
    assert_eq!(
        row(&installed, BUNDLE)["action"],
        "installed",
        "the bundle installs from the bucket: {installed}"
    );
    assert_converged(&dev, &v1_files(&nonce), "the install over S3");

    write_bundle(&project.join(BUNDLE), &v2_files(&nonce));
    let v2 = version_of(&publish(
        &author,
        &project,
        "v2: the reference gains a line",
    ));
    let swept = dev
        .run(&["update", "--json"])
        .data("the second machine's v2 sweep");
    assert_eq!(
        row(&swept, BUNDLE)["action"],
        "fast_forwarded",
        "the sweep moves to v2 over S3: {swept}"
    );
    assert_converged(&dev, &v2_files(&nonce), "the v2 sweep over S3");

    // ── 2. THE STATELESS PROOF: throw the vault away and build a new one on the same address ────
    // Its serve task is aborted, every local dir it used is deleted, and the replacement opens the
    // SAME bucket over the SAME database with a brand-new empty staging root. The web app is not
    // restarted and not reconfigured — it keeps calling the address it has always called.
    let staging = stack.restart_vault();
    assert!(
        std::fs::read_dir(&staging)
            .expect("the rebuilt vault's staging root")
            .next()
            .is_none(),
        "the rebuilt vault starts with an EMPTY staging root ({})",
        staging.display()
    );

    // ── 3. everything still answers, out of Postgres + the bucket alone ─────────────────────────
    let quiet = dev
        .run(&["update", "--json"])
        .data("a sweep against the rebuilt vault");
    assert_eq!(
        row(&quiet, BUNDLE)["action"],
        "up_to_date",
        "the converged machine has nothing to do: {quiet}"
    );
    assert_converged(&dev, &v2_files(&nonce), "after the vault was rebuilt");

    let log = dev
        .run(&["log", BUNDLE, "--limit", "0", "--json"])
        .data("the history after the rebuild");
    let ids: Vec<&str> = log["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["action"] == "version")
        .filter_map(|e| e["version_id"].as_str())
        .collect();
    assert!(
        ids.contains(&v1.as_str()) && ids.contains(&v2.as_str()),
        "the whole history survives the vault: {log}"
    );

    let diff = dev
        .run(&["diff", BUNDLE, &format!("{v1}..{v2}"), "--json"])
        .data("the diff after the rebuild");
    let body = diff["diff"].as_str().expect("the diff body");
    assert!(
        body.contains(CHANGED) && !body.contains(UNCHANGED),
        "both versions' bytes came back out of the bucket: {body}"
    );

    // ── 4. …and a THIRD, EMPTY machine installs from scratch through that vault ─────────────────
    // Nothing on this machine has ever seen the bundle, and the vault it fetches through has never
    // written a byte to local disk. Every object crosses the bucket, the vault and the app to get
    // here — which is what makes the byte-for-byte equality below the stateless proof.
    let third = Machine::new("store-minio-third");
    login(&stack, &third, &dev_browser);
    let fresh = third
        .run(&["update", "--json"])
        .data("a first install through the rebuilt vault");
    assert_eq!(
        row(&fresh, BUNDLE)["action"],
        "installed",
        "the third machine installs from nothing: {fresh}"
    );
    assert_converged(
        &third,
        &v2_files(&nonce),
        "the third machine's first install",
    );
}
