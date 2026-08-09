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
//! each of the three exits is driven end to end: `--keep-mine` over an UNTOUCHED workbench (keep
//! my version), `--keep-mine` over an EDITED one (commit the hand merge), and `--reset` (take the
//! team's). Each clears the block and deletes the workbench.
//!
//! **And the second property: `--keep-mine` is `git merge -X ours`, not `-s ours`.** The author's v2
//! changes the line this person also changed AND a line nobody contested AND adds a whole new file.
//! Keeping your version keeps YOUR wording on the contested line and takes the other two — through
//! the exit, through an ordinary `--yes` publish, and back out to the AUTHOR, whose next ordinary
//! update fast-forwards their own install onto the resolution. That last hop is the one that
//! matters: it is the difference between "my folder looks right" and "the team did not lose
//! anything". It is the author's install and not a fresh teammate's first sweep — their folder
//! already held the file they added — so what the hop proves is that the PUBLISHED tree still
//! carries both, since a fast-forward rewrites the whole folder from it.
//!
//! A last arm covers the OTHER refusal this touches: a copy deliberately put behind (a go-back)
//! cannot publish directly — the plane's lineage fence would refuse the candidate anyway — but its
//! `--propose` lands, because a proposal moves no pointer and asking the team to look is the safest
//! thing anyone in that state can do.
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

/// Resolved by leaving the workbench ALONE — `--keep-mine` keeps this person's version.
const KEEP: &str = "deploy-guide";
/// Resolved by EDITING the workbench — `--keep-mine` commits that hand merge.
const HAND: &str = "release-runbook";
/// Resolved by `--reset` — the team's version lands and this person's edits are dropped.
const TAKE: &str = "oncall-rota";
/// All three, in the order the scenario publishes them.
const BUNDLES: [&str; 3] = [KEEP, HAND, TAKE];

/// The version the author publishes first, and the bytes both later sides diverge from. The two
/// numbered steps sit far enough apart to be separate hunks — adjacent lines are ONE conflict to
/// diff3 and to git, and this suite is about what happens to the hunk nobody contested.
fn v1(name: &str) -> String {
    body(name, "1. check the region", "2. run the deploy", "")
}

/// What the RECEIVER edits their own copy to — step 1, the same line the author changes. Step 2 is
/// left exactly as it stands, which is what makes it uncontested.
fn mine(name: &str) -> String {
    body(name, "1. check the region twice", "2. run the deploy", "")
}

/// What the AUTHOR publishes as v2: the contested step 1, AND an uncontested step 2 the receiver
/// never touched. (The author also adds a whole new file — see `NEW_FILE`.)
fn theirs(name: &str) -> String {
    body(
        name,
        "1. check the region and the account",
        "2. run the deploy and tail the logs",
        "",
    )
}

/// What `--keep-mine` must produce over an untouched workbench: THIS person's step 1, the AUTHOR's
/// step 2. The shipped code produced `mine(name)` — reverting a line nobody argued about.
fn kept(name: &str) -> String {
    body(
        name,
        "1. check the region twice",
        "2. run the deploy and tail the logs",
        "",
    )
}

/// The by-hand merge a person types INTO the workbench: the team's contested line taken as it
/// stands, plus a step of this person's own.
fn hand_merge(name: &str) -> String {
    format!(
        "{}3. tail the logs\n",
        body(
            name,
            "1. check the region and the account",
            "2. run the deploy",
            ""
        )
    )
}

/// A go-back copy edited in the NOTES block — far enough from both lines the author's v2 rewrites
/// that the merge which follows is clean. The point of that arm is the publish refusal, not another
/// collision.
fn goback_edit(name: &str) -> String {
    body(
        name,
        "1. check the region",
        "2. run the deploy",
        "- page the on-call\n",
    )
}

/// The one shape every version of these bundles takes: a contested first step, a notes block nobody
/// argues about, and a last step the author alone rewrites. The blocks are separated by enough
/// unchanged lines to be distinct diff3 hunks — adjacent edits are ONE conflict to diff3 and to git,
/// and this suite is about the hunk nobody contested.
fn body(name: &str, first: &str, last: &str, extra_note: &str) -> String {
    format!(
        "# {name}\n\nsteps:\n{first}\n\nnotes:\n- keep the runbook short\n- ask before a \
         rollback\n{extra_note}- confirm the change window\n- announce in the channel\n\n\
         afterwards:\n{last}\n"
    )
}

/// A file the author's v2 ADDS — nowhere near the contested line, and nowhere in this person's copy.
/// The probe that started all this deleted exactly this.
const NEW_FILE: &str = "ROLLBACK.md";
const NEW_FILE_BODY: &str = "# rollback\n\n1. stop the deploy\n2. restore the last version\n";

/// The diff3 marker a folder an agent reads must never contain.
const MARKER: &str = "<<<<<<<";

/// Whether these bytes hold a real conflict marker — a LINE that begins with `<<<<<<<`, which is the
/// only shape diff3 ever writes. Prose that merely mentions the characters (the built-in `topos`
/// skill's own reference documents `--keep-mine` by naming them) is not a corrupted instruction file,
/// and a substring search that called it one would fail this suite on a documentation edit.
fn holds_a_marker(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(bytes)
        .lines()
        .any(|l| l.starts_with(MARKER))
}

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
                .map(|b| holds_a_marker(&b))
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
    // Each v1's version id, kept for the go-back that puts a copy behind on purpose (assertion 8).
    let mut v1_ids: BTreeMap<&str, String> = BTreeMap::new();
    for name in BUNDLES {
        let src = authored.join(name);
        std::fs::create_dir_all(&src).expect("create the bundle source dir");
        std::fs::write(src.join("SKILL.md"), v1(name)).expect("write SKILL.md");
        author
            .adopt_dir(&src, Some(&authored))
            .unwrap_or_else(|e| panic!("adopt {name}: {e}"));
        let published = author
            .publish(name, false, None, Some("v1"), Some(&authored))
            .unwrap_or_else(|e| panic!("publish {name} v1: {e}"));
        let topos::test_support::PublishView::Published { version_id, .. } = published else {
            panic!("{name} v1 lands directly");
        };
        v1_ids.insert(name, version_id);
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
        // …and a brand-new file, nowhere near the contested line.
        std::fs::write(authored.join(name).join(NEW_FILE), NEW_FILE_BODY)
            .expect("the author's new file");
        author
            .publish(name, false, None, Some("v2"), Some(&authored))
            .unwrap_or_else(|e| panic!("publish {name} v2: {e}"));
    }

    // ── 4. the ordinary sweep merges — and all three conflict ───────────────────────────────────
    let swept_envelope = dev.run(&["update", "--json"]).envelope("the merging sweep");
    let swept = swept_envelope["data"].clone();
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
        // Each folder is named WITH what is in it, read from the folder itself. Nothing has moved
        // any of them since the stop, so every one reads `yours` — the state the aggregate
        // "your agents are unaffected" sentence is the true summary of.
        let named: Vec<(String, String)> = r["merge"]["placements"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| {
                (
                    from_receipt(&dev.home(), p["dir"].as_str().expect("a dir")),
                    p["holds"].as_str().expect("a holds").to_owned(),
                )
            })
            .collect();
        assert_eq!(
            named,
            record(&placed, name)
                .placements
                .iter()
                .map(|p| (canon(p), "yours".to_owned()))
                .collect::<Vec<_>>(),
            "{name}: the receipt names the folders and what each holds: {swept}"
        );
        assert!(
            record(&placed, name).conflict_json().is_file(),
            "{name}: the block is on record"
        );
    }

    // ── ASSERTION 3b: the `--json` half offers the SAME two exits, as runnable argv ─────────────
    // The one state that asks for a decision is the one an agent most needs a next action for, and
    // the envelope used to carry an empty list — the two ways out were terminal prose only.
    let offered: Vec<Vec<String>> = swept_envelope["next_actions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["code"] == "RESOLVE_DIVERGED_DRAFT")
        .map(|a| {
            a["argv"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .collect();
    for name in BUNDLES {
        for exit in ["--keep-mine", "--reset"] {
            let want: Vec<String> = ["topos", "update", "-g", name, exit, "--json"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect();
            assert!(
                offered.contains(&want),
                "{name} {exit}: the envelope offers the exits the receipt prints: \
                 {swept_envelope}"
            );
        }
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

    // ── ASSERTION 5: `--keep-mine` is `git merge -X ours` ──────────────────────────────────────
    // THE ASSERTION THIS WHOLE CHANGE EXISTS FOR. The author's v2 did three things at once: it
    // rewrote the line this person also rewrote, it rewrote a line they never touched, and it added
    // a file they have never seen. Keeping your version keeps YOUR wording on the first and takes
    // BOTH of the others. The shipped code committed this person's whole folder instead, so the
    // second line reverted and the new file was deleted — silently.
    let kept_receipt = dev
        .run(&["update", "-g", KEEP, "--keep-mine", "--json"])
        .data("--keep-mine with the workbench untouched");
    assert_eq!(
        row(&kept_receipt, KEEP)["action"],
        "merged",
        "{kept_receipt}"
    );
    for dir in &record(&placed, KEEP).placements {
        assert_eq!(
            placed_text(dir),
            kept(KEEP),
            "my wording on the contested line, the team's on the one I never touched, in {}",
            dir.display()
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(NEW_FILE)).ok().as_deref(),
            Some(NEW_FILE_BODY),
            "the file the team added, nowhere near the collision, in {}",
            dir.display()
        );
    }
    // The receipt names WHICH exit finished it — the two say different things about whose wording
    // landed, and a reader (or an agent) must not have to infer that from a diff both of them carry.
    assert_eq!(
        row(&kept_receipt, KEEP)["merge"]["resolved"],
        "keep_mine",
        "{kept_receipt}"
    );
    // The receipt NAMES what came along, so the fact is readable rather than inferred.
    let took: Vec<&str> = row(&kept_receipt, KEEP)["merge"]["took"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(took, vec![NEW_FILE, "SKILL.md"], "{kept_receipt}");
    assert!(
        !dev.topos_home().join("conflicts").join(KEEP).exists(),
        "the exit deletes the workbench"
    );
    assert!(
        !record(&placed, KEEP).conflict_json().exists(),
        "the exit clears the block"
    );

    // ── ASSERTION 5b: publishing it is an ORDINARY publish, and the team gets everything ────────
    // No extra flag, no extra confirmation — git has none either. The resolution's commit parents
    // on the team's version, so the plane's lineage fence is satisfied and a plain `--yes` ships it.
    let landed = dev
        .run(&["publish", KEEP, "--yes", "-m", "kept mine", "--json"])
        .data("a plain --yes ships the resolution");
    assert!(
        landed["version_id"].as_str().is_some(),
        "the publish landed: {landed}"
    );
    // And the read surfaces call it what it is: a draft, until it ships. Every folder holds it
    // (topos wrote it), so no single folder is named as THE draft.
    let after_publish = dev
        .run(&["update", "-g", KEEP, "--json"])
        .data("the sweep after publishing");
    assert_eq!(
        row(&after_publish, KEEP)["action"],
        "up_to_date",
        "{after_publish}"
    );
    // Republishing the landed bytes is an ordinary no-op refusal.
    let noop = dev
        .run(&["publish", KEEP, "--yes", "--json"])
        .refusal("republishing the landed bytes");
    assert_eq!(noop["error"]["code"], "NO_CHANGES", "{noop}");

    // THE ROUND TRIP: what the TEAM receives. The author's own copy is now behind, and their next
    // ordinary update fast-forwards onto this person's resolution — carrying the author's OWN
    // uncontested line and their OWN new file back to them, which is precisely what `-s ours` threw
    // away. A folder looking right locally proves nothing; this does. (This is the AUTHOR's install,
    // not a fresh teammate's first one: their folder already held the new file. The fast-forward
    // rewrites that folder wholesale from the published tree, so what it reads afterwards is what
    // the published tree carries.)
    let (authors_sweep, _) = author
        .update(&[KEEP], Some(&authored))
        .unwrap_or_else(|e| panic!("the author's sweep after the receiver published: {e}"));
    let authors_copy: PathBuf = authors_sweep
        .skills
        .iter()
        .find(|r| r.skill == KEEP)
        .and_then(|r| r.destinations.first())
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("the author's sweep landed nowhere: {authors_sweep:?}"));
    assert_eq!(
        std::fs::read_to_string(authors_copy.join("SKILL.md")).expect("the author's copy"),
        kept(KEEP),
        "the team receives this person's contested line — and their own uncontested one back"
    );
    assert_eq!(
        std::fs::read_to_string(authors_copy.join(NEW_FILE))
            .ok()
            .as_deref(),
        Some(NEW_FILE_BODY),
        "and the file they added is still in the published tree the fast-forward rewrote from"
    );

    // ── ASSERTION 6: an EDITED workbench is committed as the hand resolution ────────────────────
    let workbench = dev.topos_home().join("conflicts").join(HAND);
    std::fs::write(workbench.join("SKILL.md"), hand_merge(HAND)).expect("resolve by hand");
    let resolved = dev
        .run(&["update", "-g", HAND, "--keep-mine", "--json"])
        .data("--keep-mine over an edited workbench");
    assert_eq!(row(&resolved, HAND)["action"], "merged", "{resolved}");
    // This person took the TEAM's contested line and added a step of their own — the tree a
    // "kept your wording" headline would misdescribe. The row says which exit ran, and claims no
    // file as taken from the team: topos never examined a byte of this.
    assert_eq!(
        row(&resolved, HAND)["merge"]["resolved"],
        "by_hand",
        "{resolved}"
    );
    assert!(
        row(&resolved, HAND)["merge"]["took"].is_null(),
        "{resolved}"
    );
    for dir in &record(&placed, HAND).placements {
        assert_eq!(
            placed_text(dir),
            hand_merge(HAND),
            "the hand resolution is what landed in {}",
            dir.display()
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(NEW_FILE)).ok().as_deref(),
            Some(NEW_FILE_BODY),
            "the workbench IS the merged tree, so the team's new file was already in it: {}",
            dir.display()
        );
    }
    assert!(!workbench.exists(), "the exit deletes the workbench");
    assert!(
        !record(&placed, HAND).conflict_json().exists(),
        "the exit clears the block"
    );

    // ── ASSERTION 6b: a hand resolution publishes like any other draft ─────────────────────────
    // Nothing extra is asked of it, exactly as nothing extra was asked of the kept-mine one above.
    let landed = dev
        .run(&["publish", HAND, "--yes", "-m", "merged", "--json"])
        .data("a plain --yes ships the hand merge");
    assert!(landed["version_id"].as_str().is_some(), "{landed}");
    for dir in &record(&placed, HAND).placements {
        assert_eq!(
            placed_text(dir),
            hand_merge(HAND),
            "the published bytes are the person's reconciliation"
        );
    }

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

    // ── ASSERTION 8: a BEHIND copy cannot publish, but can always ASK ───────────────────────────
    // A go-back puts this copy deliberately behind the team's `current`. A direct publish from
    // there would build a candidate whose first parent is not the pointed version — which the
    // plane's lineage fence refuses — so the client refuses it first, offline and for free.
    let goback = format!("{TAKE}@{}", v1_ids[TAKE]);
    dev.run(&["update", "-g", &goback, "--json"])
        .data("a go-back to the earlier version");
    for dir in &record(&placed, TAKE).placements {
        std::fs::write(dir.join("SKILL.md"), goback_edit(TAKE)).expect("edit the restored copy");
    }
    let behind = dev
        .run(&["publish", TAKE, "--yes", "--json"])
        .refusal("a direct publish from behind");
    assert_eq!(behind["error"]["code"], "CONFLICT", "{behind}");
    assert_eq!(
        behind["error"]["context"]["message"],
        format!("{TAKE}: your version is behind."),
        "{behind}"
    );
    // A PROPOSAL moves no pointer and meets no fence, so it is the one act a behind copy can
    // always safely perform — and the real server accepts it. Refusing it would block asking the
    // team to look, which is the safest thing anyone in this state can do.
    let proposed = dev
        .run(&["publish", TAKE, "--propose", "--yes", "--json"])
        .data("a proposal from behind");
    assert!(
        proposed["proposal"]
            .as_str()
            .is_some_and(|p| p.starts_with(TAKE)),
        "asking the team to look is never refused: {proposed}"
    );

    // ── the aftermath: a settled bundle stays settled, and no marker ever reached an agent ──────
    // ALL THREE, including KEEP: a decision that stands is a decision that stops being asked. The
    // shipped behaviour re-raised KEEP's conflict on every sweep forever, because its exit never
    // became durable — this loop is what the arc above closes.
    let after = dev.run(&["update", "--json"]).data("the settling sweep");
    for name in BUNDLES {
        assert_ne!(
            row(&after, name)["action"],
            "conflicted",
            "{name} is resolved and stays resolved: {after}"
        );
        assert!(
            !dev.topos_home().join("conflicts").join(name).exists(),
            "{name}: its workbench is gone"
        );
    }
    assert_eq!(
        marked_up_files(&dev.home()),
        Vec::<PathBuf>::new(),
        "no marker ever reached a folder an agent reads"
    );
}
