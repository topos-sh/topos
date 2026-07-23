//! E2E — the reduced consent gate over the composed stack: `--yes` gates only acts with REACH
//! (touch other people), LOSS (destroy unrecoverable local work), or FIRST-TRUST (bytes land
//! somewhere new). Self-scoped acts reversible by their inverse apply IMMEDIATELY on a bare run
//! and answer an undo-led receipt; `--yes` stays an accepted no-op on those arms.
//!
//! Every assertion is a real effect: server rows through the composed stack's database, dirs on
//! disk, the typed receipts the genuine client engine answers. (The enrollment gate and the
//! second-workspace link gate keep their own suites — `follow_e2e` / `device_links_e2e` — which
//! run unchanged beside this one.)

mod common;

use common::{OWNER_EMAIL, SKILL, Stack, WS_NAME, expected, genesis_files};
use topos::test_support::{
    FollowHarness, FollowProbe, PublishResult, RemoveProbe, Scope, UnfollowProbe,
};
use topos_types::results::{PullAction, RemoveKind};

const MEMBER_EMAIL: &str = "dana@acme.test";
/// A channel-scoped second skill (born into #eng, never into `everyone`).
const TOOLS: &str = "s-tools";
/// A skill the member NEVER receives (born into #ops) — the first-trust gate's subject.
const FRESH: &str = "s-fresh";

fn tools_files() -> Vec<(&'static str, bool, &'static [u8])> {
    vec![(
        "SKILL.md",
        false,
        b"# tools\nTeam tooling notes.\n" as &[u8],
    )]
}

fn fresh_files() -> Vec<(&'static str, bool, &'static [u8])> {
    vec![(
        "SKILL.md",
        false,
        b"# fresh\nNever delivered here.\n" as &[u8],
    )]
}

/// Publish `skill` from `author` (adopt + direct genesis into `everyone`).
fn publish_genesis(author: &FollowHarness, skill: &str, files: &[(&str, bool, &[u8])]) {
    author.adopt(skill, files);
    let digest = author.draft_digest(skill);
    match author
        .publish_message("", &format!("{skill}@{digest}"), "genesis")
        .expect("the genesis lands")
    {
        PublishResult::Published(_) => {}
        other => panic!("expected a direct genesis, got {other:?}"),
    }
}

/// Publish `skill` from `author` into `channel` (the `--to` placement — replaces the everyone
/// default).
fn publish_to_channel(
    author: &FollowHarness,
    skill: &str,
    channel: &str,
    files: &[(&str, bool, &[u8])],
) {
    author.adopt(skill, files);
    let digest = author.draft_digest(skill);
    match author
        .publish_to(&format!("{skill}@{digest}"), channel, "channel genesis")
        .expect("the channel-targeted genesis lands")
    {
        PublishResult::Published(_) => {}
        other => panic!("expected a direct genesis, got {other:?}"),
    }
}

/// Sweep + (if offered) accept — land `name` on `client`.
fn land(client: &FollowHarness, name: &str) {
    let (data, warnings) = client.reconcile(true);
    assert!(warnings.is_empty(), "a clean sweep: {warnings:?}");
    if data
        .skills
        .iter()
        .any(|s| s.skill == name && s.action == PullAction::Offered)
    {
        let _ = client.pull(Scope::Accept {
            name: name.to_owned(),
        });
    }
}

/// The shared arrangement: owner + genesis in `everyone`, one seated member with an enrolled CLI
/// holding the genesis bytes.
fn arranged(tag: &str) -> (Stack, common::Session, common::Session, FollowHarness) {
    let stack = common::start_stack(tag);
    let owner = stack.claim_owner(OWNER_EMAIL);
    let author = FollowHarness::new(&format!("{tag}-author"));
    stack.enroll_begin_and_approve(&author, &owner);
    author.resume_apply().expect("the author enrolls");
    publish_genesis(&author, SKILL, &genesis_files());

    let member = stack.add_member(MEMBER_EMAIL, "member");
    let client = FollowHarness::new(&format!("{tag}-member"));
    stack.enroll_begin_and_approve(&client, &member);
    client.resume_apply().expect("the member enrolls");
    land(&client, SKILL);
    assert_eq!(client.placement_files(SKILL), expected(&genesis_files()));
    (stack, owner, member, client)
}

/// This device's exclusion-row count (the per-device `remove` stance).
fn exclusion_rows(stack: &Stack, device_id: &str) -> i64 {
    stack.count(&format!(
        "SELECT count(*) FROM web.device_exclusion WHERE device_id = '{device_id}'"
    ))
}

/// The person's `unfollowed`-stance row count for one skill.
fn unfollowed_rows(stack: &Stack, user_id: &str, skill: &str) -> i64 {
    stack.count(&format!(
        "SELECT count(*) FROM web.bundle_subscription bs \
         JOIN web.bundle b ON b.id = bs.bundle_id \
         WHERE bs.user_id = '{user_id}' AND b.name = '{skill}' AND bs.state = 'unfollowed'"
    ))
}

// ── acceptance 1 + 4 + 5: the five ungated arms apply on a BARE run, receipts undo-led ─────────────

#[test]
fn e2e_bare_runs_apply_for_the_ungated_arms_with_undo_led_receipts() {
    let (stack, _owner, _member, client) = arranged("yss-apply");
    let device_id = client.device_id().expect("enrolled");
    let member_id = stack.user_id(MEMBER_EMAIL);

    // Arm: `remove <skill>` on a followed CLEAN skill — the bare run writes the exclusion row.
    let removed = client
        .remove_probe(&[SKILL], &[], false)
        .expect("the bare remove applies");
    let RemoveProbe::Applied(data) = removed else {
        panic!("a followed clean skill applies immediately: {removed:?}");
    };
    assert!(data.applied);
    assert!(matches!(data.items[0].kind, RemoveKind::FollowedExclusion));
    assert!(data.items[0].bytes_kept);
    let qualified = format!("{WS_NAME}/skills/{SKILL}");
    assert_eq!(data.undo, vec!["topos", "follow", qualified.as_str()]);
    assert_eq!(exclusion_rows(&stack, &device_id), 1, "the row is real");
    assert!(
        !client.placement_path(SKILL).exists(),
        "the agent dir is cleaned"
    );

    // Arm: `follow <skill>` re-attach (excluded-here) — the bare run lifts the row + reinstalls.
    let followed = client
        .follow_probe(SKILL, &[], false)
        .expect("the bare follow re-attaches");
    let FollowProbe::Reattached(r) = followed else {
        panic!("an excluded skill re-attaches immediately: {followed:?}");
    };
    assert_eq!(r["cause"], "excluded-here");
    assert_eq!(r["installed"], true);
    assert_eq!(
        r["undo"],
        serde_json::json!(["topos", "remove", qualified]),
        "the receipt leads with the literal inverse (qualified — never an ambiguity refusal)"
    );
    assert_eq!(exclusion_rows(&stack, &device_id), 0, "the row is gone");
    assert_eq!(
        client.placement_files(SKILL),
        expected(&genesis_files()),
        "the current bytes are back on disk"
    );

    // Arm: `unfollow <skill>` — the bare run writes the person's unfollowed stance.
    let unfollowed = client
        .unfollow_probe(&[SKILL], &[], false)
        .expect("the bare unfollow applies");
    let UnfollowProbe::Applied { undo, bytes_kept } = unfollowed else {
        panic!("a skill unfollow applies immediately: {unfollowed:?}");
    };
    assert!(bytes_kept);
    assert_eq!(undo, vec!["topos", "follow", qualified.as_str()]);
    assert_eq!(
        unfollowed_rows(&stack, &member_id, SKILL),
        1,
        "the unfollowed stance row is real"
    );
    assert!(
        client.placement_path(SKILL).exists(),
        "the local copy stays frozen in place"
    );

    // Arm: `follow <skill>` re-attach (previously-unfollowed) — the bare run clears the stance
    // SERVER-side and resumes delivery.
    let refollowed = client
        .follow_probe(SKILL, &[], false)
        .expect("the bare re-follow applies");
    let FollowProbe::Reattached(r) = refollowed else {
        panic!("a previously-unfollowed skill re-attaches immediately: {refollowed:?}");
    };
    assert_eq!(r["cause"], "unfollowed");
    assert_eq!(
        r["undo"],
        serde_json::json!(["topos", "unfollow", qualified])
    );
    assert_eq!(
        unfollowed_rows(&stack, &member_id, SKILL),
        0,
        "the unfollowed stance cleared server-side"
    );
    let (data, _) = client.reconcile(true);
    assert!(
        data.skills.iter().any(|s| s.skill == SKILL
            && matches!(s.action, PullAction::UpToDate | PullAction::FastForwarded)),
        "delivery resumed: {:?}",
        data.skills
    );

    // Arms: the `--agent` scope pair — device-local placement policy, immediate, receipt carries
    // the cleaned/added/kept disclosure and the literal undo; the subscription never moves.
    let scoped = client
        .follow_probe(SKILL, &["cursor"], false)
        .expect("the bare scope set applies");
    let FollowProbe::Scope(s) = scoped else {
        panic!("--agent on a followed skill is the scope receipt: {scoped:?}");
    };
    assert_eq!(s["action"], "scope");
    assert_eq!(s["applied"], true);
    assert_eq!(
        s["undo"],
        serde_json::json!(["topos", "follow", SKILL, "--agent", "*"])
    );
    assert!(
        s["items"][0].get("cleaned").is_some()
            || s["items"][0].get("added").is_some()
            || s["items"][0].get("kept").is_some(),
        "the receipt carries the placement disclosure: {s}"
    );

    let excluded = client
        .remove_probe(&[SKILL], &["cursor"], false)
        .expect("the bare per-agent exclusion applies");
    let RemoveProbe::AgentScope { data: s, described } = excluded else {
        panic!("-a on a followed skill is the shared exclusion arm: {excluded:?}");
    };
    assert!(!described, "the per-agent exclusion applies immediately");
    assert_eq!(s["action"], "exclude");
    assert_eq!(s["applied"], true);
    assert_eq!(
        s["undo"],
        serde_json::json!(["topos", "follow", SKILL, "--agent", "cursor"])
    );
    // Placement policy only: no server stance moved.
    assert_eq!(exclusion_rows(&stack, &device_id), 0);
    assert_eq!(unfollowed_rows(&stack, &member_id, SKILL), 0);

    // Acceptance 5: `--yes` stays an ACCEPTED NO-OP on an ungated arm (same receipt shape).
    let yes_scope = client
        .follow_probe(SKILL, &["*"], true)
        .expect("--yes is accepted on the ungated arm");
    assert!(
        matches!(yes_scope, FollowProbe::Scope(ref v) if v["applied"] == true),
        "--yes answers the same applied receipt: {yes_scope:?}"
    );
}

// ── acceptance 2 + 3 + 5: the gates hold (describe, no mutation); `--yes` still applies ────────────

#[test]
fn e2e_gates_hold_loss_guard_fails_closed_and_yes_still_applies() {
    let (stack, owner, _member, client) = arranged("yss-gates");
    let device_id = client.device_id().expect("enrolled");

    // LOSS-GUARD: a followed skill WITH a draft ahead — the bare run DESCRIBES (loss-led), the
    // row does not move, the draft stays on disk; `--yes` then applies.
    client.edit_placement(SKILL, &[("SKILL.md", false, b"# edited ahead\n")]);
    let guarded = client
        .remove_probe(&[SKILL], &[], false)
        .expect("the loss-guard describes");
    let RemoveProbe::Described { data, yes_argv } = guarded else {
        panic!("a draft holds the gate: {guarded:?}");
    };
    assert!(!data.applied);
    assert!(
        data.items[0]
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("local edits ahead"),
        "the describe leads with the loss: {:?}",
        data.items[0].note
    );
    assert!(yes_argv.contains(&"--yes".to_owned()));
    assert_eq!(exclusion_rows(&stack, &device_id), 0, "nothing mutated");
    assert!(client.placement_path(SKILL).exists(), "the draft stays");

    // Acceptance 5 (gated side): `--yes` still applies the described removal — with NO undo on
    // the receipt (a re-follow would land the canonical bytes, not the draft this apply cleaned).
    let applied = client
        .remove_probe(&[SKILL], &[], true)
        .expect("--yes applies the guarded remove");
    assert!(
        matches!(applied, RemoveProbe::Applied(ref d) if d.applied && d.undo.is_empty()),
        "a consented draft removal applies with no undo: {applied:?}"
    );
    assert_eq!(exclusion_rows(&stack, &device_id), 1, "the row landed");
    assert!(
        !client.placement_path(SKILL).exists(),
        "the dirs are cleaned on apply (draft snapshotted first)"
    );
    // Restore for the arms below.
    let restored = client.follow_probe(SKILL, &[], false).expect("re-attach");
    assert!(matches!(restored, FollowProbe::Reattached(_)));

    // LOSS-GUARD FAIL-CLOSED: an unreadable placement cannot rule a draft out — the bare run
    // must DESCRIBE (never apply on an indeterminate scan).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = client.placement_path(SKILL);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
        let out = client.remove_probe(&[SKILL], &[], false);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
        let RemoveProbe::Described { data, .. } = out.expect("the indeterminate scan describes")
        else {
            panic!("an unscannable placement fails TOWARD the gate");
        };
        assert!(
            data.items[0]
                .note
                .as_deref()
                .unwrap_or_default()
                .contains("cannot be scanned"),
            "the describe names the indeterminacy: {:?}",
            data.items[0].note
        );
        assert_eq!(exclusion_rows(&stack, &device_id), 0, "nothing mutated");
    }

    // LOCAL-ONLY REMOVE stays gated: a permanent delete of the only copy describes first.
    client.adopt("local-notes", &[("SKILL.md", false, b"# mine\n")]);
    let local = client
        .remove_probe(&["local-notes"], &[], false)
        .expect("the local-only remove describes");
    let RemoveProbe::Described { data, .. } = local else {
        panic!("a permanent delete holds the gate: {local:?}");
    };
    assert!(matches!(
        data.items[0].kind,
        RemoveKind::TrackedLocalPermanent
    ));
    assert!(!data.items[0].bytes_kept);
    let gone = client
        .remove_probe(&["local-notes"], &[], true)
        .expect("--yes applies the permanent delete");
    match gone {
        RemoveProbe::Applied(d) => assert!(d.undo.is_empty(), "no inverse for a permanent delete"),
        other => panic!("--yes applies: {other:?}"),
    }

    // CHANNEL UNFOLLOW stays gated (computed breadth): joining #eng delivers TOOLS; the bare
    // channel unfollow DESCRIBES which skills stop, the membership row stays; `--yes` leaves.
    let author2 = FollowHarness::new("yss-gates-author2");
    stack.enroll_begin_and_approve(&author2, &owner);
    author2.resume_apply().expect("the second author enrolls");
    publish_to_channel(&author2, TOOLS, "eng", &tools_files());
    client
        .follow_apply("acme/channels/eng")
        .expect("the member joins #eng");
    land(&client, TOOLS);
    let member_id = stack.user_id(MEMBER_EMAIL);
    let member_rows = || {
        stack.count(&format!(
            "SELECT count(*) FROM web.channel_member cm \
             JOIN web.channel c ON c.id = cm.channel_id \
             WHERE cm.user_id = '{member_id}' AND c.name = 'eng'"
        ))
    };
    assert_eq!(member_rows(), 1, "the join row landed");
    let gated = client
        .unfollow_probe(&[], &["eng"], false)
        .expect("the channel unfollow describes");
    let UnfollowProbe::Described { yes_argv, stops } = gated else {
        panic!("a channel unfollow holds the gate: {gated:?}");
    };
    assert!(
        stops.iter().any(|s| s == TOOLS),
        "the computed breadth names what stops: {stops:?}"
    );
    assert!(yes_argv.contains(&"--yes".to_owned()));
    assert_eq!(member_rows(), 1, "the describe left the membership row");
    let left = client
        .unfollow_probe(&[], &["eng"], true)
        .expect("--yes leaves the channel");
    assert!(matches!(left, UnfollowProbe::Applied { .. }));
    assert_eq!(member_rows(), 0, "the leave row op landed");

    // FIRST FOLLOW stays gated (first-trust): a catalog skill never delivered to this member
    // (born into #ops, which the member never joined) describes — nothing lands.
    let author3 = FollowHarness::new("yss-gates-author3");
    stack.enroll_begin_and_approve(&author3, &owner);
    author3.resume_apply().expect("the third author enrolls");
    publish_to_channel(&author3, FRESH, "ops", &fresh_files());
    let fresh = client
        .follow_probe(FRESH, &[], false)
        .expect("the first follow describes");
    assert!(
        matches!(fresh, FollowProbe::Described { .. }),
        "first-trust holds the two-phase gate: {fresh:?}"
    );
    assert!(
        !client.placement_path(FRESH).exists(),
        "the describe lands nothing"
    );
}

// ── the widened re-attach on SNAPSHOT evidence alone (no local entry) ──────────────────────────────

#[test]
fn e2e_detached_refollow_with_no_local_entry_lands_on_a_second_device() {
    // The person unfollows a skill on device A, then brings up a FRESH device B (enrolled after
    // the unfollow): B holds no `follows.json` entry and no bytes — the delivery snapshot's
    // `detached` list is the ONLY evidence the skill was ever on the person's trust surface. The
    // bare `follow <skill>` on B is still a re-follow, not first-trust: it must apply immediately
    // AND actually converge — the server stance cleared, the bytes landed on disk, the local
    // entry created — never a receipt over nothing.
    let (stack, _owner, member, client_a) = arranged("yss-detached");
    let member_id = stack.user_id(MEMBER_EMAIL);

    // Device A: the person's standing unfollow (applies immediately, server-recorded).
    let unfollowed = client_a
        .unfollow_probe(&[SKILL], &[], false)
        .expect("the bare unfollow applies");
    assert!(matches!(unfollowed, UnfollowProbe::Applied { .. }));
    assert_eq!(unfollowed_rows(&stack, &member_id, SKILL), 1);

    // Device B enrolls AFTER the unfollow — the delivery carries nothing for the skill.
    let client_b = FollowHarness::new("yss-detached-b");
    stack.enroll_begin_and_approve(&client_b, &member);
    client_b.resume_apply().expect("the second device enrolls");
    let (data, _) = client_b.reconcile(true);
    assert!(
        data.skills.iter().all(|s| s.skill != SKILL),
        "an unfollowed skill is not delivered to a fresh device: {:?}",
        data.skills
    );
    assert!(!client_b.placement_path(SKILL).exists(), "no bytes yet");

    // The bare re-follow on B: previously trusted (the snapshot's `detached` evidence) — applies.
    let refollowed = client_b
        .follow_probe(SKILL, &[], false)
        .expect("the bare re-follow applies");
    let FollowProbe::Applied { installed, undo } = refollowed else {
        panic!("a detached skill re-follows immediately: {refollowed:?}");
    };
    assert!(
        installed.iter().any(|n| n == SKILL),
        "the bytes actually land THIS invocation: {installed:?}"
    );
    // NO undo on the snapshot-only re-follow: the apply minted this device's local entry and
    // landed bytes an `unfollow` would not take back (it pauses and keeps them), so the receipt
    // offers no inverse rather than half of one. The undo-led shape belongs to the local-pause
    // flip, proven above on device A's arms.
    assert!(
        undo.is_empty(),
        "a snapshot-only re-follow offers no undo: {undo:?}"
    );
    assert_eq!(
        unfollowed_rows(&stack, &member_id, SKILL),
        0,
        "the stance cleared server-side"
    );
    assert_eq!(
        client_b.placement_files(SKILL),
        expected(&genesis_files()),
        "the current bytes are on B's disk"
    );
    // The local entry converged: the next sweep syncs it as a known skill — never a re-offer.
    let (data, warnings) = client_b.reconcile(true);
    assert!(warnings.is_empty(), "a clean sweep: {warnings:?}");
    assert!(
        data.skills
            .iter()
            .any(|s| s.skill == SKILL && matches!(s.action, PullAction::UpToDate)),
        "no first-receive re-offer after the re-follow: {:?}",
        data.skills
    );
}
