//! The composed SESSION + MANIFEST hero loop, over real HTTP against the real web app: login
//! (the browser-approval flow at `/verify`), the governance-transferring genesis publish, the
//! project-manifest reference landing INSIDE a checkout (self-ignoring, state in the checkout's own store),
//! fast-forward on `update`, the protection downgrade → review → approve loop, the `-g`
//! server-stored profile lane, and the OWNER-side session end (the CLI's next sweep prints the
//! one typed line and freezes — bytes stay).

mod common;

use common::{OWNER_EMAIL, start_stack};
use topos::test_support::SessionInstall;

/// Write the genesis skill source (a doc + an executable) under `dir`.
fn write_skill(dir: &std::path::Path, skill_md: &str) {
    std::fs::create_dir_all(dir).expect("create skill source dir");
    std::fs::write(dir.join("SKILL.md"), skill_md).expect("write SKILL.md");
    std::fs::write(dir.join("run.sh"), "#!/bin/sh\necho deploying\n").expect("write run.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .expect("mark run.sh executable");
    }
}

/// An install whose fake `$HOME` detects Claude Code (so PROJECT-scope placement resolves the
/// registry's `.claude/skills` root inside a checkout).
fn install(tag: &str) -> SessionInstall {
    let client = SessionInstall::new(tag);
    std::fs::create_dir_all(client.root().join("home").join(".claude"))
        .expect("arm claude-code detection");
    client
}

/// The one placement dir under `work/` holding a `SKILL.md` (person-scope placements are keyed by
/// the local skill id, opaque to the suite).
fn person_scope_dir(client: &SessionInstall) -> Option<std::path::PathBuf> {
    let work = client.root().join("work");
    let entries = std::fs::read_dir(&work).ok()?;
    entries
        .flatten()
        .map(|e| e.path())
        .find(|p| p.join("SKILL.md").is_file())
}

/// End one CLI session from the OWNER's workspace sessions page (the in-place-confirm arm's
/// underlying POST — `intent` + `session_id`, exactly what the form submits).
fn owner_session_arm(owner: &common::Session, intent: &str, session_id: &str) {
    let answer = owner.post_form(
        "/settings/sessions",
        &[("intent", intent), ("session_id", session_id)],
    );
    assert_eq!(
        answer.status, 200,
        "the owner {intent} lands: {}",
        answer.body
    );
}

#[test]
fn the_session_manifest_hero_loop() {
    let stack = start_stack("hero");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // ── the author: login (pending → /verify approve → granted ACTIVE) ──────────────────────────
    let author = install("author");
    stack.login_begin_and_approve(&author, &owner);
    let granted = author.login(None).expect("login resume");
    assert_eq!(granted.session_status, "active");
    assert!(granted.pending.is_none(), "the resume settles the flow");
    assert!(!author.wal_exists(), "the WAL is consumed");
    assert_eq!(author.sessions().len(), 1);

    // ── the governance-transferring genesis publish ─────────────────────────────────────────────
    // Adopt a local dir (the manifest records the `./deploy` path line), publish — the landed
    // publish moves governance by default: catalog entry + the manifest line rewritten to the
    // canonical workspace reference.
    // Canonicalized so the adopt's dir-relative manifest spelling holds on macOS (`/var` is a
    // symlink to `/private/var` — the scan canonicalizes, the manifest dir must match).
    let cwd = author.root().canonicalize().expect("canonical root");
    let src = cwd.join("deploy");
    write_skill(&src, "# deploy\nDeploy the service.\n");
    let added = author.adopt_dir(&src, Some(&cwd)).expect("adopt the dir");
    assert_eq!(added.name, "deploy");
    assert_eq!(added.reference.as_deref(), Some("./deploy"));
    let manifest_path = cwd.join("topos.toml");
    assert!(manifest_path.exists(), "the adopt created the manifest");

    let published = author
        .publish("deploy", false, None, Some("genesis"), Some(&cwd))
        .expect("genesis publish");
    let topos::test_support::PublishView::Published {
        manifest,
        reference,
        converted_from,
        ..
    } = published
    else {
        panic!("the genesis lands directly: {published:?}");
    };
    assert_eq!(converted_from.as_deref(), Some("./deploy"));
    let canonical = reference.expect("the governed reference rides the receipt");
    assert!(
        canonical.ends_with("/acme/deploy"),
        "the canonical host-qualified reference: {canonical}"
    );
    assert!(
        manifest
            .expect("the rewritten manifest is named")
            .ends_with("topos.toml"),
        "the receipt names the manifest"
    );
    let manifest_bytes = std::fs::read_to_string(&manifest_path).expect("read the manifest");
    assert!(
        manifest_bytes.contains(&canonical),
        "the manifest line is the canonical reference now: {manifest_bytes}"
    );
    assert!(
        !manifest_bytes.contains("\"./deploy\""),
        "the path line is gone: {manifest_bytes}"
    );

    // ── the second person: a PROJECT-manifest reference lands INSIDE the checkout ───────────────
    let dev_browser = stack.add_member("dev@acme.test", "member");
    let dev = install("dev");
    stack.login_begin_and_approve(&dev, &dev_browser);
    let dev_granted = dev.login(None).expect("dev login resume");
    assert_eq!(dev_granted.session_status, "active");
    let dev_session_id = dev_granted.session_id.expect("the granted session id");

    let proj = dev.root().join("proj");
    std::fs::create_dir_all(proj.join(".git")).expect("a git checkout");
    let added = dev
        .add_reference("@acme/deploy", false, Some(&proj))
        .expect("add the workspace reference");
    assert!(
        added
            .manifest
            .as_deref()
            .is_some_and(|m| m.ends_with("topos.toml")),
        "the add names the manifest it edited: {added:?}"
    );

    // The bytes live INSIDE the checkout — and the delivery happened on the add itself. The
    // placed dir SELF-IGNORES: its root carries the exact sentinel ignore file beside the
    // byte-exact bundle (exec bit included).
    let placed = proj.join(".claude").join("skills").join("deploy");
    let files = SessionInstall::dir_files(&placed);
    assert_eq!(
        files
            .iter()
            .map(|(p, x, b)| (p.as_str(), *x != 0, b.as_slice()))
            .collect::<Vec<_>>(),
        vec![
            (
                ".gitignore",
                false,
                b"# topos: delivered bytes, not committed\n*\n" as &[u8]
            ),
            (
                "SKILL.md",
                false,
                b"# deploy\nDeploy the service.\n" as &[u8]
            ),
            ("run.sh", true, b"#!/bin/sh\necho deploying\n" as &[u8]),
        ],
        "the checkout placement is byte-exact plus the self-ignore sentinel"
    );
    // …and NOTHING was written under `.git/` (the placement keeps itself out of commits; the
    // engine state lives in the project's own self-ignoring `.topos/` store).
    assert!(
        std::fs::read_dir(proj.join(".git"))
            .expect("the checkout's .git dir")
            .next()
            .is_none(),
        "nothing under .git/ was written"
    );
    assert_eq!(
        std::fs::read(proj.join(".topos").join(".gitignore")).expect("the store ignore"),
        b"*\n",
        "the project store ignores itself whole"
    );

    // ── v2 fast-forwards silently (login was the acceptance — no offer step) ────────────────────
    write_skill(&src, "# deploy v2\nDeploy the service, faster.\n");
    author
        .publish("deploy", false, None, Some("v2"), Some(&cwd))
        .expect("publish v2");
    let (pulled, warnings) = dev.update(&[], Some(&proj)).expect("the sweep");
    assert!(
        warnings.is_empty(),
        "a clean sweep warns nothing: {warnings:?}"
    );
    let row = pulled
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .expect("the delivered row");
    assert_eq!(format!("{:?}", row.action), "FastForwarded");
    assert!(
        std::fs::read_to_string(placed.join("SKILL.md"))
            .expect("the placed doc")
            .contains("faster"),
        "v2 landed in the checkout"
    );

    // ── protection: the member's direct publish DOWNGRADES to a proposal; approve lands it ──────
    author.protect("deploy", None).expect("tighten to reviewed");
    let doc = placed.join("SKILL.md");
    let mut body = std::fs::read_to_string(&doc).expect("read the draft base");
    body.push_str("\nAlways run the smoke suite first.\n");
    std::fs::write(&doc, body).expect("write the member draft");
    let proposed = dev
        .publish("deploy", false, None, Some("smoke first"), Some(&proj))
        .expect("the member publish");
    let topos::test_support::PublishView::Proposed { proposal } = proposed else {
        panic!("the protection gate reroutes into a proposal: {proposed:?}");
    };
    author
        .review(&proposal, "approve", None)
        .expect("the owner approves");
    let (_pulled, warnings) = dev
        .update(&[], Some(&proj))
        .expect("the post-approve sweep");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(
        std::fs::read_to_string(placed.join("SKILL.md"))
            .expect("the placed doc")
            .contains("smoke suite"),
        "the approved proposal landed"
    );

    // ── the `-g` lane: the login-written FEED LINE delivers person-scope; the file takes control ──
    // No `-g` act so far: the login wrote acme's feed line into `~/.topos/topos.toml` on this
    // machine's first connection (login was the acceptance — delivery is automatic), so standing
    // OUTSIDE any checkout the bare update converges the machine and the feed lands the
    // person-scope copy by itself. The project sweeps above converged the checkout alone, as the
    // scope rule says.
    let (feed_pull, feed_warnings) = dev.update(&[], None).expect("the person-scope sweep");
    let person_dir = person_scope_dir(&dev).unwrap_or_else(|| {
        panic!(
            "the feed lands in person scope by itself; rows {:?} warnings {feed_warnings:?}",
            feed_pull.skills
        )
    });
    assert!(person_dir.join("SKILL.md").is_file());
    // A bare `add -g` of a feed-delivered skill is the disclosed REDUNDANCY no-op: no row is
    // written (a bare row would add nothing the feed does not already give).
    let redundant = dev
        .add_reference("@acme/deploy", true, None)
        .expect("the redundant add answers, not errors");
    assert!(
        redundant.manifest.is_none(),
        "no row is written for a feed-delivered bundle: {redundant:?}"
    );
    assert!(
        redundant.note.is_some(),
        "the no-op is disclosed: {redundant:?}"
    );
    // `remove -g` of the feed-delivered skill writes the machine-local OFF switch beside the
    // feed line the login recorded; the receipt names the web decline as the everywhere-path.
    let (kind, note) = dev.remove_global("deploy").expect("the off switch");
    assert_eq!(kind, "ManifestExcluded", "the off row is the one negative");
    assert!(
        note.as_deref()
            .is_some_and(|n| n.contains("decline") || n.contains("web")),
        "the everywhere-path is named: {note:?}"
    );
    let global_manifest = dev.root().join(".topos").join("topos.toml");
    let file = std::fs::read_to_string(&global_manifest).expect("the global manifest");
    assert!(file.contains("= \"off\""), "the off row is spelled: {file}");
    assert!(
        file.contains("/acme\" = \"*\""),
        "the login's first connection recorded the feed line: {file}"
    );
    // The next sweep cleans the person-scope placement; the checkout's copy is that scope's
    // business and stays.
    dev.update(&[], None).expect("the post-off sweep");
    assert!(
        !person_dir.exists(),
        "the off switch cleans the person-scope placement"
    );
    assert!(
        placed.join("SKILL.md").is_file(),
        "the project checkout's copy is untouched by the person-scope off"
    );
    // `add -g` with the off row standing DELETES the switch (never a redundant positive), and
    // the feed flows again on the next sweep.
    let lifted = dev
        .add_reference("@acme/deploy", true, None)
        .expect("the off lift");
    assert!(
        lifted.note.is_some(),
        "the lift is disclosed on the receipt: {lifted:?}"
    );
    let file = std::fs::read_to_string(&global_manifest).expect("the global manifest");
    assert!(!file.contains("\"off\""), "the off row is gone: {file}");
    dev.update(&[], None).expect("the post-lift sweep");
    let person_dir = person_scope_dir(&dev).expect("the feed delivers again after the lift");
    assert!(person_dir.join("SKILL.md").is_file());

    // ── the OWNER ends the dev session: one typed line, then a freeze — bytes stay ──────────────
    owner_session_arm(&owner, "remove-session", &dev_session_id);
    let (_pulled, warnings) = dev.update(&[], Some(&proj)).expect("the post-end sweep");
    assert!(
        warnings.iter().any(|w| w.starts_with("SESSION_ENDED")),
        "the end prints the one typed line: {warnings:?}"
    );
    assert!(
        placed.join("SKILL.md").is_file(),
        "the bytes stay in place (the freeze, never a clean)"
    );
    let ended = dev.sessions();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].2, "ended", "the local session row is marked ended");
    // The line prints ONCE — the next sweep is quiet about it.
    let (_pulled, warnings) = dev.update(&[], Some(&proj)).expect("the second sweep");
    assert!(
        !warnings.iter().any(|w| w.starts_with("SESSION_ENDED")),
        "the ended line does not repeat: {warnings:?}"
    );
}

#[test]
fn deny_sweeps_the_flow_and_logout_ends_the_session_server_side() {
    let stack = start_stack("deny-logout");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // ── the DENY arm: one typed refusal, zero state ─────────────────────────────────────────────
    let denied = install("denied");
    let pending = denied
        .login(Some(&stack.address()))
        .expect("login call 1 pends");
    let user_code = pending.pending.expect("the pending handle").user_code;
    stack.deny_device(&owner, &user_code);
    let err = denied.login(None).expect_err("the denied resume refuses");
    assert!(err.contains("denied"), "the refusal names the deny: {err}");
    assert!(!denied.wal_exists(), "the denied WAL is swept");
    assert!(denied.sessions().is_empty(), "nothing was minted");

    // ── logout: the server-side self-end + the local row delete ─────────────────────────────────
    let author = install("logout-author");
    stack.login_begin_and_approve(&author, &owner);
    author.login(None).expect("login resume");
    let live_before = stack.count("SELECT count(*) FROM web.cli_session");
    let ended = author.logout_all().expect("logout");
    assert_eq!(ended, vec!["acme".to_owned()]);
    assert!(author.sessions().is_empty(), "the local rows are gone");
    let live_after = stack.count("SELECT count(*) FROM web.cli_session");
    assert!(
        live_after < live_before,
        "the server-side session ended with the logout ({live_before} -> {live_after})"
    );
}
