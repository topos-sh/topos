//! The composed MERGE-CONFLICT loop, over real HTTP against the real web app and the real `topos`
//! binary: an author publishes, a second person edits their own copy, the author publishes a change
//! to the SAME lines, and that person's ordinary `topos update` runs the three-way merge — which
//! conflicts.
//!
//! Only a composed run reaches this state at all. A conflict is the `Diverged` apply class, and the
//! shipped configuration reaches it exclusively through a workspace delivery: a local path row never
//! moves upstream, a forge row refuses outright when its placement is modified, and the built-in is
//! force-synced. So the merge is driven here the one way a person ever drives it — a live session, a
//! real publish, a bare sweep — and the assertions are then made against the REAL agent-readable
//! folders a live `$HOME` resolves, not a fixture's stand-in dir.
//!
//! **The property under test: a folder an agent reads always holds one coherent, complete bundle.**
//! The conflict writes NOTHING into any placement — every one keeps the person's own bytes, byte for
//! byte — and the marked-up copy (diff3 markers carrying both sides) goes to the scope's own
//! workbench, `~/.topos/conflicts/<name>/`, which no harness reads and `publish` cannot ship. Then
//! each of the three exits is driven end to end: `--onto-current` over an UNTOUCHED workbench (keep
//! my version), `--onto-current` over an EDITED one (commit the hand merge), and `--reset` (take the
//! team's). Each clears the block and deletes the workbench.
//!
//! Like the MCP suite, this one drives the REAL CLI BINARY as a subprocess over a fake `$HOME` — the
//! placement dirs, the workbench path and the two exits all resolve against the environment, so only
//! a real process proves them. The fixture rig owns the browser login and the author's publishes;
//! the binary owns every verb the conflicted person runs.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::{CliOut, OWNER_EMAIL, Session, Stack, cli_binary, run_cli, start_stack};
use serde_json::Value;
use topos::test_support::SessionInstall;

// ── the three bundles, one per exit ─────────────────────────────────────────────────────────────

/// Resolved by leaving the workbench ALONE — `--onto-current` keeps this person's version.
const KEEP: &str = "deploy-guide";
/// Resolved by EDITING the workbench — `--onto-current` commits that hand merge.
const HAND: &str = "release-runbook";
/// Resolved by `--reset` — the team's version lands and this person's edits are dropped.
const TAKE: &str = "oncall-rota";
/// All three, in the order the scenario publishes them.
const BUNDLES: [&str; 3] = [KEEP, HAND, TAKE];

/// The version the author publishes first, and the bytes both later sides diverge from.
fn v1(name: &str) -> String {
    format!("# {name}\n\nsteps:\n1. check the region\n2. run the deploy\n")
}

/// What the RECEIVER edits their own copy to — line 4, the same line the author changes.
fn mine(name: &str) -> String {
    format!("# {name}\n\nsteps:\n1. check the region twice\n2. run the deploy\n")
}

/// What the AUTHOR publishes as v2 — the same line, differently.
fn theirs(name: &str) -> String {
    format!("# {name}\n\nsteps:\n1. check the region and the account\n2. run the deploy\n")
}

/// The by-hand merge a person types INTO the workbench (both sides reconciled).
fn hand_merge(name: &str) -> String {
    format!("# {name}\n\nsteps:\n1. check the region and the account, twice\n2. run the deploy\n")
}

/// The diff3 marker a folder an agent reads must never contain.
const MARKER: &str = "<<<<<<<";

// ── the installations ───────────────────────────────────────────────────────────────────────────

/// The receiver's installation: a fake `$HOME` where TWO harnesses with their own private skills
/// dirs are detected — so every bundle lands in SEVERAL agent-readable folders and "no folder an
/// agent reads" is a claim about more than one of them — over the fixture rig's `~/.topos` (where
/// the login it performed lives, and where the workbench belongs).
struct Install {
    rig: SessionInstall,
    bin: PathBuf,
}

impl Install {
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

    /// This installation's machine store (`~/.topos`), where the workbench lives.
    fn topos_home(&self) -> PathBuf {
        self.rig.root().join(".topos")
    }

    /// Run the real binary from the install root — OUTSIDE any checkout, so the scope it acts in is
    /// the machine's own, exactly as the receipts' `-g` spelling says.
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

/// Log an installation in through the REAL browser-approval ceremony (the fixture rig owns the
/// flow; the binary inherits the session it lands).
fn login(stack: &Stack, install: &Install, approver: &Session) {
    stack.login_begin_and_approve(&install.rig, approver);
    let granted = install.rig.login(None).expect("the login resume");
    assert_eq!(granted.session_status, "active");
}

// ── reading what the installation holds ─────────────────────────────────────────────────────────

/// One bundle's machine-store record: the store dir (where `conflict.json` lives) and EVERY
/// placement dir the record names — which is what "every folder this install put bytes in" means,
/// read from the map the engine itself writes rather than guessed from the disk.
struct Record {
    store_dir: PathBuf,
    placements: Vec<PathBuf>,
}

impl Record {
    fn conflict_json(&self) -> PathBuf {
        self.store_dir.join("conflict.json")
    }
}

/// Every recorded bundle in the machine store, keyed by NAME.
fn records(install: &Install) -> BTreeMap<String, Record> {
    let mut out = BTreeMap::new();
    let skills = install.topos_home().join("skills");
    let Ok(entries) = std::fs::read_dir(&skills) else {
        return out;
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
        let Some(name) = lock["name"].as_str() else {
            continue;
        };
        out.insert(
            name.to_owned(),
            Record {
                store_dir: dir,
                placements: map["placements"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(PathBuf::from)
                    .collect(),
            },
        );
    }
    out
}

/// One bundle's record, or a panic naming what the store does hold.
fn record<'a>(all: &'a BTreeMap<String, Record>, name: &str) -> &'a Record {
    all.get(name).unwrap_or_else(|| {
        panic!(
            "no store record for {name}; the store holds {:?}",
            all.keys().collect::<Vec<_>>()
        )
    })
}

/// Every file under `dir` whose bytes carry a conflict marker, as paths.
fn marked_up_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if std::fs::read(&path)
                .map(|b| String::from_utf8_lossy(&b).contains(MARKER))
                .unwrap_or(false)
            {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// One placement dir's whole content, as `(rel path, exec bits, bytes)`.
type Snapshot = Vec<(String, u32, Vec<u8>)>;

fn snapshot(dir: &Path) -> Snapshot {
    SessionInstall::dir_files(dir)
}

/// What ONE bundle's folders held before the merging sweep — the baseline every placement must
/// still be byte-identical to once the conflict is recorded.
type Untouched = Vec<(PathBuf, Snapshot)>;

/// The single `SKILL.md` a placement holds (these bundles are one file each).
fn placed_text(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("SKILL.md"))
        .unwrap_or_else(|e| panic!("read {} ({e})", dir.join("SKILL.md").display()))
}

/// A path as the CLIENT reports it — canonicalized (on macOS `/var` is a symlink to `/private/var`).
fn canon(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// One path a receipt printed, back as a real path: receipts abbreviate the machine home to `~`
/// (anything outside it stays absolute), so the `~/` prefix is expanded against this
/// installation's own home before the comparison.
fn from_receipt(home: &Path, printed: &str) -> String {
    match printed.strip_prefix("~/") {
        Some(rest) => canon(&home.join(rest)),
        None => canon(Path::new(printed)),
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

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// The arc — one composed stack, three genuine conflicts, three exits.
// ═══════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn a_merge_conflict_never_reaches_a_folder_an_agent_reads() {
    let stack = start_stack("conflict");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // ── 1. the author publishes v1 of all three bundles ─────────────────────────────────────────
    let author = SessionInstall::new("conflict-author");
    stack.login_begin_and_approve(&author, &owner);
    author.login(None).expect("the author's login resume");
    // Canonicalized so the adopt's dir-relative manifest spelling holds on macOS (the scan
    // canonicalizes, and the manifest dir must match).
    let authored = author.root().canonicalize().expect("canonical author root");
    for name in BUNDLES {
        let src = authored.join(name);
        std::fs::create_dir_all(&src).expect("create the bundle source dir");
        std::fs::write(src.join("SKILL.md"), v1(name)).expect("write SKILL.md");
        author
            .adopt_dir(&src, Some(&authored))
            .unwrap_or_else(|e| panic!("adopt {name}: {e}"));
        author
            .publish(name, false, None, Some("v1"), Some(&authored))
            .unwrap_or_else(|e| panic!("publish {name} v1: {e}"));
    }

    // ── 2. the receiver's first sweep installs all three ────────────────────────────────────────
    let dev_browser = stack.add_member("dev@acme.test", "member");
    let dev = Install::new("conflict-dev");
    login(&stack, &dev, &dev_browser);

    let installed = dev.run(&["update", "--json"]).data("the first sweep");
    for name in BUNDLES {
        assert_eq!(
            row(&installed, name)["action"],
            "installed",
            "{name} installs on the bare sweep: {installed}"
        );
    }
    let placed = records(&dev);
    for name in BUNDLES {
        let rec = record(&placed, name);
        assert!(
            rec.placements.len() >= 2,
            "{name} landed in both detected agents' folders: {:?}",
            rec.placements
        );
        for dir in &rec.placements {
            assert_eq!(placed_text(dir), v1(name), "{name} in {}", dir.display());
        }
    }

    // ── 3. the receiver edits their copies; the author changes the SAME lines and publishes v2 ──
    // Every copy of one bundle takes the SAME edit, so the bundle has one draft (copies that
    // disagreed would be the placement freeze, a different state with its own exits).
    let mut before: BTreeMap<&str, Untouched> = BTreeMap::new();
    for name in BUNDLES {
        let mut per_bundle = Vec::new();
        for dir in &record(&placed, name).placements {
            std::fs::write(dir.join("SKILL.md"), mine(name)).expect("edit the placed copy");
            per_bundle.push((dir.clone(), snapshot(dir)));
        }
        before.insert(name, per_bundle);
    }
    for name in BUNDLES {
        std::fs::write(authored.join(name).join("SKILL.md"), theirs(name))
            .expect("the author's own edit");
        author
            .publish(name, false, None, Some("v2"), Some(&authored))
            .unwrap_or_else(|e| panic!("publish {name} v2: {e}"));
    }

    // ── 4. the ordinary sweep merges — and all three conflict ───────────────────────────────────
    let swept = dev.run(&["update", "--json"]).data("the merging sweep");
    for name in BUNDLES {
        let r = row(&swept, name);
        assert_eq!(r["action"], "conflicted", "{name}: {swept}");
        assert_eq!(r["merge"]["clean"], false, "{name}: {swept}");
        assert!(
            r["merge"]["conflicts"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|c| c["path"] == "SKILL.md"),
            "{name} names the conflicting path: {swept}"
        );
    }

    // ── ASSERTION 1: no folder an agent reads holds a marker, anywhere under the fake $HOME ─────
    // The whole home, not just the placements: every skills dir any harness resolves lives under it,
    // so a marker escaping into ANY of them fails here.
    assert_eq!(
        marked_up_files(&dev.home()),
        Vec::<PathBuf>::new(),
        "a conflict must not write markers into anything an agent can read"
    );
    // And explicitly, per recorded placement — the folders the engine itself says it manages.
    for name in BUNDLES {
        for dir in &record(&placed, name).placements {
            assert_eq!(
                marked_up_files(dir),
                Vec::<PathBuf>::new(),
                "{name}: {} holds no markers",
                dir.display()
            );
        }
    }

    // ── ASSERTION 2: every placement still holds this person's OWN pre-update bytes ─────────────
    for name in BUNDLES {
        for (dir, was) in &before[name] {
            assert_eq!(
                &snapshot(dir),
                was,
                "{name}: {} is byte-identical to what stood there before the update",
                dir.display()
            );
            assert_eq!(placed_text(dir), mine(name), "{name} in {}", dir.display());
        }
    }

    // ── ASSERTION 3: the workbench is at the documented path and DOES carry both sides ──────────
    for name in BUNDLES {
        let workbench = dev.topos_home().join("conflicts").join(name);
        assert!(
            workbench.is_dir(),
            "the machine scope's workbench is ~/.topos/conflicts/<name>/ ({})",
            workbench.display()
        );
        let marked =
            std::fs::read_to_string(workbench.join("SKILL.md")).expect("the marked-up copy");
        assert!(
            marked.contains(MARKER) && marked.contains("=======") && marked.contains(">>>>>>>"),
            "{name}: the workbench copy is diff3-marked: {marked}"
        );
        assert!(
            marked.contains("check the region twice")
                && marked.contains("check the region and the account"),
            "{name}: both sides survive inside the markers: {marked}"
        );
        // The receipt names that exact folder, and names the untouched placements beside it — so a
        // person is sent to the folder every exit below actually reads.
        let r = row(&swept, name);
        assert_eq!(
            r["merge"]["copy_dir"]
                .as_str()
                .map(|p| from_receipt(&dev.home(), p)),
            Some(canon(&workbench)),
            "{name}: the receipt names the workbench: {swept}"
        );
        let named: Vec<String> = r["merge"]["placements"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|p| from_receipt(&dev.home(), p))
            .collect();
        assert_eq!(
            named,
            record(&placed, name)
                .placements
                .iter()
                .map(|p| canon(p))
                .collect::<Vec<_>>(),
            "{name}: the receipt names the folders that still hold this person's version: {swept}"
        );
        assert!(
            record(&placed, name).conflict_json().is_file(),
            "{name}: the block is on record"
        );
    }

    // ── ASSERTION 4: publish is refused while the block stands ──────────────────────────────────
    for name in BUNDLES {
        let refused = dev
            .run(&["publish", name, "--yes", "--json"])
            .refusal(&format!("publish {name} while blocked"));
        assert_eq!(
            refused["error"]["code"], "PUBLISH_BLOCKED",
            "{name}: {refused}"
        );
    }

    // ── ASSERTION 5: `--onto-current` over an UNTOUCHED workbench keeps this person's version ───
    let kept = dev
        .run(&["update", "-g", KEEP, "--onto-current", "--json"])
        .data("--onto-current with the workbench untouched");
    assert_eq!(row(&kept, KEEP)["action"], "merged", "{kept}");
    for dir in &record(&placed, KEEP).placements {
        assert_eq!(
            placed_text(dir),
            mine(KEEP),
            "the original draft is what landed in {}",
            dir.display()
        );
    }
    assert!(
        !dev.topos_home().join("conflicts").join(KEEP).exists(),
        "the exit deletes the workbench"
    );
    assert!(
        !record(&placed, KEEP).conflict_json().exists(),
        "the exit clears the block"
    );
    // The block really is gone: the publish that was refused a moment ago now lands.
    dev.run(&["publish", KEEP, "--yes", "-m", "mine", "--json"])
        .data("publish after the escape");

    // ── ASSERTION 6: an EDITED workbench is committed as the hand resolution ────────────────────
    let workbench = dev.topos_home().join("conflicts").join(HAND);
    std::fs::write(workbench.join("SKILL.md"), hand_merge(HAND)).expect("resolve by hand");
    let resolved = dev
        .run(&["update", "-g", HAND, "--onto-current", "--json"])
        .data("--onto-current over an edited workbench");
    assert_eq!(row(&resolved, HAND)["action"], "merged", "{resolved}");
    for dir in &record(&placed, HAND).placements {
        assert_eq!(
            placed_text(dir),
            hand_merge(HAND),
            "the hand resolution is what landed in {}",
            dir.display()
        );
    }
    assert!(!workbench.exists(), "the exit deletes the workbench");
    assert!(
        !record(&placed, HAND).conflict_json().exists(),
        "the exit clears the block"
    );

    // ── ASSERTION 7: `--reset` lands the team's bytes ───────────────────────────────────────────
    dev.run(&["update", "-g", TAKE, "--reset", "--yes", "--json"])
        .data("--reset on a blocked bundle");
    for dir in &record(&placed, TAKE).placements {
        assert_eq!(
            placed_text(dir),
            theirs(TAKE),
            "the team's version is what landed in {}",
            dir.display()
        );
    }
    assert!(
        !dev.topos_home().join("conflicts").join(TAKE).exists(),
        "the exit deletes the workbench"
    );
    assert!(
        !record(&placed, TAKE).conflict_json().exists(),
        "the exit clears the block"
    );

    // ── the aftermath: nothing re-blocks, nothing re-renders a workbench, no marker anywhere ────
    let after = dev.run(&["update", "--json"]).data("the settling sweep");
    for name in BUNDLES {
        assert_ne!(
            row(&after, name)["action"],
            "conflicted",
            "{name} is resolved and stays resolved: {after}"
        );
    }
    assert!(
        !dev.topos_home().join("conflicts").exists()
            || std::fs::read_dir(dev.topos_home().join("conflicts"))
                .expect("the conflicts dir")
                .flatten()
                .next()
                .is_none(),
        "every workbench is gone"
    );
    assert_eq!(
        marked_up_files(&dev.home()),
        Vec::<PathBuf>::new(),
        "no marker ever reached a folder an agent reads"
    );
}
