//! Post-review hardening journeys over real HTTP against the real web app: a PINNED workspace
//! reference (`@ws/name@<digest>`) delivers exactly its pinned version while the unpinned line
//! tracks `current`, `publish --to` REFUSES a channel that does not exist (the pointed
//! workspace-slug near-miss included) without ever minting one, and the claim RETIRES the
//! collision-suffixed twin a real delivery placed beside a person's own same-named folder.

mod common;

use common::{OWNER_EMAIL, WS_NAME, start_stack};
use topos::test_support::SessionInstall;

/// Write a one-doc skill source under `dir` with `body` as its SKILL.md.
fn write_skill(dir: &std::path::Path, body: &str) {
    std::fs::create_dir_all(dir).expect("create skill source dir");
    std::fs::write(dir.join("SKILL.md"), body).expect("write SKILL.md");
}

/// An install whose fake `$HOME` detects Claude Code (project placement lands in `.claude/skills`).
fn install(tag: &str) -> SessionInstall {
    let client = SessionInstall::new(tag);
    std::fs::create_dir_all(client.root().join("home").join(".claude"))
        .expect("arm claude-code detection");
    client
}

#[test]
fn a_pinned_reference_delivers_its_version_and_publish_to_never_mints_a_channel() {
    let stack = start_stack("hardening");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // ── the author: login, adopt, publish v1 then v2 ────────────────────────────────────────────
    let author = install("hard-author");
    stack.login_begin_and_approve(&author, &owner);
    let granted = author.login(None).expect("login resume");
    assert_eq!(granted.session_status, "active");
    let cwd = author.root().canonicalize().expect("canonical root");
    let src = cwd.join("pinme");
    write_skill(&src, "# pinme v1\n");
    author.adopt_dir(&src, Some(&cwd)).expect("adopt");
    let v1 = match author
        .publish("pinme", false, None, Some("v1"), Some(&cwd))
        .expect("publish v1")
    {
        topos::test_support::PublishView::Published { version_id, .. } => version_id,
        other => panic!("v1 lands directly: {other:?}"),
    };
    // Advance current to v2 (the governed placement is the draft surface now).
    let placed = cwd.join(".claude").join("skills").join("pinme");
    let draft = if placed.join("SKILL.md").is_file() {
        placed
    } else {
        src.clone()
    };
    std::fs::write(draft.join("SKILL.md"), "# pinme v2\n").expect("edit the draft");
    let v2 = match author
        .publish("pinme", false, None, Some("v2"), Some(&cwd))
        .expect("publish v2")
    {
        topos::test_support::PublishView::Published { version_id, .. } => version_id,
        other => panic!("v2 lands directly: {other:?}"),
    };
    assert_ne!(v1, v2);

    // ── a second install pins v1 in its project manifest; update delivers EXACTLY v1 ────────────
    let dev = install("hard-dev");
    stack.login_begin_and_approve(&dev, &owner);
    dev.login(None).expect("login resume");
    let dev_cwd = dev.root().canonicalize().expect("canonical root");
    let proj = dev_cwd.join("proj");
    std::fs::create_dir_all(proj.join(".git")).expect("a git checkout");
    dev.pick_in_project(&proj);
    let pinned_ref = format!("@{WS_NAME}/pinme@{v1}");
    let added = dev
        .add_reference(&pinned_ref, false, Some(&proj))
        .expect("the pinned @ws/name@<digest> add resolves through the grammar, never a path arm");
    assert_eq!(added.name, "pinme");
    let manifest =
        std::fs::read_to_string(proj.join("topos.toml")).expect("the manifest was written");
    assert!(
        manifest.contains(&v1),
        "the pin is the entry value: {manifest}"
    );
    let placed = proj
        .join(".claude")
        .join("skills")
        .join("pinme")
        .join("SKILL.md");
    let bytes = std::fs::read_to_string(&placed).expect("the pinned version landed in-checkout");
    assert_eq!(bytes, "# pinme v1\n", "the PIN wins over current");

    // The person scope's own copy arrives on a MACHINE-scoped run — an `update` converges where it
    // stands, so the in-checkout add above was the project's business alone — and holding it is
    // what makes the split below real.
    dev.update(&[], None).expect("the machine-scoped sweep");

    // A fresh sweep keeps the pin (never silently fast-forwarded to v2) — and says NOTHING about
    // it. This installation genuinely holds `pinme` at two versions, the person scope's current
    // and this checkout's pin, and the applied report carries only one row per bundle; but the
    // difference is the pin doing its job. The receipt's cross-scope line is about STALENESS, and
    // no `topos update` anywhere would move a pinned row, so a line here could never be cleared.
    // This is the whole silence half of that rule, over the real engine.
    let (data, warnings) = dev.update(&[], Some(&proj)).expect("sweep");
    assert!(
        warnings.is_empty(),
        "a clean run earns no lines: {warnings:?}"
    );
    assert!(
        data.behind_elsewhere.is_empty(),
        "a pinned copy is deliberate, and the person copy is current: {:?}",
        data.behind_elsewhere
    );
    // The pin still holds on disk — the fact the silence is silent ABOUT.
    assert_eq!(
        std::fs::read_to_string(&placed).expect("still placed"),
        "# pinme v1\n"
    );
    // …while the machine-wide copy really did take v2, so the two scopes DO disagree here: the
    // silence above is a judgement about the difference, not an absence of one.
    // The copy is NAMED, never picked off a directory walk: `read_dir` promises no order, so
    // "the first SKILL.md under the skills root" is whichever entry the filesystem happens to
    // yield — an assertion that passes today because only one folder is there, and starts reading
    // some other skill's bytes the moment a second one is. The dir is the registry ROW's, under
    // the install's own home — the same resolution the planner runs.
    assert!(added.skill_id.is_some(), "the add recorded a bundle");
    let person_copy = dev.skills_dir("pinme").join("SKILL.md");
    assert_eq!(
        std::fs::read_to_string(&person_copy).expect("the feed landed the person-scope copy"),
        "# pinme v2\n",
        "the machine scope tracks current — this is a real cross-scope split"
    );

    // ── `--to` takes an EXISTING channel — refusals, and no channel is minted ───────────────────
    std::fs::write(draft.join("SKILL.md"), "# pinme v3\n").expect("edit for the next publish");
    let miss = author
        .publish("pinme", false, Some("backend"), Some("to-miss"), Some(&cwd))
        .expect_err("a nonexistent channel refuses");
    assert!(
        miss.contains("no channel 'backend'") && miss.contains("create 'backend' on the web"),
        "the refusal names the create path: {miss}"
    );
    // The value equal to the WORKSPACE slug gets the pointed near-miss.
    let near = author
        .publish("pinme", false, Some(WS_NAME), Some("to-slug"), Some(&cwd))
        .expect_err("the workspace slug is not a channel");
    assert!(
        near.contains("names the workspace, not a channel"),
        "the pointed near-miss: {near}"
    );
    // NOTHING was minted: the owner's channels page still lists ONLY the default channel.
    let page = owner.get("/channels");
    assert_eq!(page.status, 200);
    assert!(
        page.body.contains("everyone"),
        "the default channel renders"
    );
    assert!(
        !page.body.contains("backend"),
        "the refused --to never minted a channel"
    );
}

/// THE TWIN RETIREMENT, over the real engine. A person already keeps their own copy of a bundle in
/// an agent's skills dir; the workspace then delivers that same bundle, the placement ladder finds
/// the name taken and lands the engine's copy beside it as `<name>-<workspace>`. Claiming the
/// person's folder makes the duplicate pure duplication — so it retires, and from then on THEIR
/// folder is what the sweep keeps current.
///
/// Only a composed run reaches this: the collision is produced by a real delivery through the real
/// placement ladder, and the workspace suffix comes from the enrolled session's own address.
#[test]
fn a_claim_retires_the_twin_a_real_delivery_placed_beside_it() {
    let stack = start_stack("twin");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // ── the author publishes v1 ──────────────────────────────────────────────────────────────
    let author = install("twin-author");
    stack.login_begin_and_approve(&author, &owner);
    author.login(None).expect("login resume");
    let cwd = author.root().canonicalize().expect("canonical root");
    let src = cwd.join("twinny");
    write_skill(&src, "# twinny\n\nStep: one\n");
    author.adopt_dir(&src, Some(&cwd)).expect("adopt");
    match author
        .publish("twinny", false, None, Some("v1"), Some(&cwd))
        .expect("publish v1")
    {
        topos::test_support::PublishView::Published { .. } => {}
        other => panic!("v1 lands directly: {other:?}"),
    }

    // ── the person's OWN copy, by hand, under the name the workspace publishes ────────────────
    let dev = install("twin-dev");
    stack.login_begin_and_approve(&dev, &owner);
    dev.login(None).expect("login resume");
    let dev_cwd = dev.root().canonicalize().expect("canonical root");
    let proj = dev_cwd.join("proj");
    std::fs::create_dir_all(proj.join(".git")).expect("a git checkout");
    dev.pick_in_project(&proj);
    let mine = proj.join(".claude").join("skills").join("twinny");
    // Their own copy of the same bundle, with a note of their own beside it — so it is NOT
    // byte-identical to what the workspace publishes, and the delivery cannot simply adopt it.
    write_skill(&mine, "# twinny\n\nStep: one\n");
    std::fs::write(mine.join("notes.md"), "mine\n").expect("their own file");

    // The delivery cannot have that dir, so the ladder suffixes with the workspace address.
    let added = dev
        .add_reference(&format!("@{WS_NAME}/twinny"), false, Some(&proj))
        .expect("the workspace reference delivers");
    assert_eq!(added.name, "twinny");
    let twin = proj
        .join(".claude")
        .join("skills")
        .join(format!("twinny-{WS_NAME}"));
    assert!(
        twin.join("SKILL.md").is_file(),
        "the engine's copy landed beside the person's own folder: {:?}",
        std::fs::read_dir(proj.join(".claude").join("skills")).map(|d| d
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>())
    );
    assert_eq!(
        std::fs::read_to_string(mine.join("SKILL.md")).expect("the person's copy is untouched"),
        "# twinny\n\nStep: one\n"
    );

    // ── the claim: their folder becomes the bundle's place, and the duplicate retires ─────────
    let claimed = dev
        .claim(&mine, "twinny", Some(&proj))
        .expect("the claim lands");
    let receipt = topos::test_support::SessionInstall::add_receipt_tty(&claimed);
    assert!(
        receipt.contains(&format!(
            "removed the duplicate {} (unedited copy of the same skill)",
            twin.display()
        )),
        "the receipt says exactly what it removed: {receipt}"
    );
    assert!(!twin.exists(), "the twin directory is gone");
    assert!(
        mine.join("SKILL.md").is_file(),
        "the claimed folder is untouched — nothing was recreated or moved"
    );

    // ── and THEIR folder is what the next sweep keeps current ────────────────────────────────
    let placed = cwd.join(".claude").join("skills").join("twinny");
    let draft = if placed.join("SKILL.md").is_file() {
        placed
    } else {
        src.clone()
    };
    std::fs::write(draft.join("SKILL.md"), "# twinny\n\nStep: two\n").expect("edit the draft");
    author
        .publish("twinny", false, None, Some("v2"), Some(&cwd))
        .expect("publish v2");

    dev.update(&[], Some(&proj)).expect("the project sweep");
    assert_eq!(
        std::fs::read_to_string(mine.join("SKILL.md")).expect("still there"),
        "# twinny\n\nStep: two\n",
        "the claimed folder converged — the claim's whole promise"
    );
    assert_eq!(
        std::fs::read_to_string(mine.join("notes.md")).expect("their own file survives"),
        "mine\n",
        "…without destroying the local edit that made it a draft"
    );
    assert!(!twin.exists(), "and the duplicate never came back");
}
