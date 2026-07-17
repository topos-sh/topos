//! E2E — the DELIVERY-DRIVEN RECONCILE over the composed stack: channels decide what a person's
//! devices receive, and the bare enrolled sweep converges each device onto its entitlement —
//! ((default channels − self opt-outs) ∪ member channels ∪ direct follows) − unfollows − this
//! device's exclusions.
//!
//! Wire ops the CLI has no verb for (curation, channel membership) ride the REAL device lane
//! directly under a minted probe credential — the same `/api/v1` rows the client's own verbs
//! write. Every delivery consequence is then proven through the GENUINE client reconcile.

mod common;

use common::{OWNER_EMAIL, SKILL, Stack, expected, genesis_files};
use topos::test_support::{FollowHarness, PublishResult, Scope};
use topos_types::results::PullAction;

const MEMBER_EMAIL: &str = "carol@acme.test";
/// The second, channel-scoped skill.
const TOOLS: &str = "s-tools";

fn tools_files() -> Vec<(&'static str, bool, &'static [u8])> {
    vec![(
        "SKILL.md",
        false,
        b"# tools\nTeam tooling notes.\n" as &[u8],
    )]
}

/// Publish `skill` from `author` (adopt + direct publish).
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

/// The sweep's action for `name` (panics if the sweep does not mention it).
fn action_of(client: &FollowHarness, name: &str) -> PullAction {
    let (data, _) = client.reconcile(true);
    data.skills
        .iter()
        .find(|s| s.skill == name)
        .unwrap_or_else(|| panic!("{name} rides the sweep: {:?}", data.skills))
        .action
}

/// The shared arrangement: owner + genesis in `everyone`, one seated member with an enrolled CLI
/// holding the genesis bytes. Returns (stack, owner session, member session, member CLI).
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

// ── channel placement: place → deliver; unplace from the last channel → withdraw; re-place → return ─

#[test]
fn e2e_channel_placement_drives_delivery_and_withdrawal() {
    let (stack, owner, member, client) = arranged("channels");
    let owner_probe = stack.mint_device(&owner, "owner probe");
    let member_probe = stack.mint_device(&member, "member probe");

    // A second skill is born into `everyone` (a brand-new bundle always lands there) — the
    // owner's author CLI ships it, the member's sweep receives it.
    let author2 = FollowHarness::new("channels-author2");
    stack.enroll_begin_and_approve(&author2, &owner);
    author2.resume_apply().expect("the second author enrolls");
    publish_genesis(&author2, TOOLS, &tools_files());
    land(&client, TOOLS);
    assert_eq!(client.placement_files(TOOLS), expected(&tools_files()));

    // The owner UNPLACES it from `everyone` (curation over the wire): the member's next sweep
    // WITHDRAWS it — agent dirs cleaned, delivery ended, the subscription rows untouched.
    let unplaced = stack.device_delete(
        &owner_probe.credential,
        &format!(
            "/v1/workspaces/{}/channels/everyone/skills/{TOOLS}",
            stack.workspace_id
        ),
        None,
    );
    assert_eq!(unplaced.status, 200, "the unplace lands: {}", unplaced.body);
    assert_eq!(action_of(&client, TOOLS), PullAction::Withdrawn);
    assert!(
        client.placement_files(TOOLS).is_empty(),
        "the withdrawn skill's agent dir is cleaned"
    );

    // The owner PLACES it into a NEW channel (created member-level on first placement). The
    // member is not in #eng, so nothing returns yet.
    let placed = stack.device_put(
        &owner_probe.credential,
        &format!(
            "/v1/workspaces/{}/channels/eng/skills/{TOOLS}",
            stack.workspace_id
        ),
    );
    assert_eq!(placed.status, 200, "the place lands: {}", placed.body);
    assert!(
        placed.body.contains("created"),
        "first placement creates #eng: {}",
        placed.body
    );
    let (data, _) = client.reconcile(true);
    assert!(
        !data
            .skills
            .iter()
            .any(|s| s.skill == TOOLS && s.action == PullAction::FastForwarded),
        "not in #eng — nothing re-delivers: {:?}",
        data.skills
    );

    // The member JOINS #eng: the re-placed skill re-delivers on the next sweep (a withdrawal is
    // a delivery change, not a subscription change).
    let joined = stack.device_put(
        &member_probe.credential,
        &format!(
            "/v1/workspaces/{}/channels/eng/membership",
            stack.workspace_id
        ),
    );
    assert_eq!(joined.status, 200, "the join lands: {}", joined.body);
    land(&client, TOOLS);
    assert_eq!(
        client.placement_files(TOOLS),
        expected(&tools_files()),
        "the re-placed skill re-delivers byte-exact"
    );
}

// ── the default channel: leaving is a per-person opt-out row; rejoining deletes it ──────────────────

#[test]
fn e2e_default_channel_optout_detaches_and_rejoin_resumes() {
    let (stack, _owner, member, client) = arranged("optout");
    let probe = stack.mint_device(&member, "member probe");
    let member_id = stack.user_id(MEMBER_EMAIL);

    // LEAVE `everyone`: membership is implicit, so leaving inserts the person's opt-out row.
    let left = stack.device_delete(
        &probe.credential,
        &format!(
            "/v1/workspaces/{}/channels/everyone/membership",
            stack.workspace_id
        ),
        None,
    );
    assert_eq!(left.status, 200, "the leave lands: {}", left.body);
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.channel_optout WHERE user_id = '{member_id}'"
        )),
        1,
        "the opt-out row is the leave"
    );

    // The person acted → the copy DETACHES: bytes frozen in place, delivery ended.
    assert_eq!(action_of(&client, SKILL), PullAction::Detached);
    assert_eq!(
        client.placement_files(SKILL),
        expected(&genesis_files()),
        "a detach freezes — never a clean"
    );

    // REJOIN: the opt-out row is deleted and delivery resumes.
    let rejoined = stack.device_put(
        &probe.credential,
        &format!(
            "/v1/workspaces/{}/channels/everyone/membership",
            stack.workspace_id
        ),
    );
    assert_eq!(rejoined.status, 200, "the rejoin lands: {}", rejoined.body);
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.channel_optout WHERE user_id = '{member_id}'"
        )),
        0,
        "the rejoin deletes the opt-out row"
    );
    land(&client, SKILL);
    let (data, _) = client.reconcile(true);
    let entry = data
        .skills
        .iter()
        .find(|s| s.skill == SKILL)
        .expect("delivered again");
    assert_eq!(entry.action, PullAction::UpToDate, "delivery resumed");
}

// ── per-device exclusion: `remove` stops THIS device; `follow` lifts it; the fleet report rows ──────

#[test]
fn e2e_device_exclusion_and_the_follow_that_lifts_it() {
    let (stack, _owner, _member, client) = arranged("exclude");
    let device_id = client.device_id().expect("enrolled");

    // The fleet report row landed with the first sweep.
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.device_bundle_state WHERE device_id = '{device_id}'"
        )),
        1,
        "the reconcile reports applied state"
    );

    // `remove <skill>` — the per-device exclusion: the row is fenced to the acting credential's
    // own device; the agent dir is cleaned, every sidecar byte kept.
    let removed = client.remove_apply(SKILL).expect("the remove applies");
    assert_eq!(removed.items.len(), 1, "one removal: {:?}", removed.items);
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.device_exclusion WHERE device_id = '{device_id}'"
        )),
        1,
        "the exclusion row names THIS device"
    );
    let (data, _) = client.reconcile(true);
    if let Some(entry) = data.skills.iter().find(|s| s.skill == SKILL) {
        assert_eq!(
            entry.action,
            PullAction::Excluded,
            "the sweep reports the exclusion"
        );
    }

    // `follow --skill <name>` lifts the exclusion; delivery resumes on this device.
    client
        .follow_apply_skills(&[SKILL])
        .expect("the follow lifts the exclusion");
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.device_exclusion WHERE device_id = '{device_id}'"
        )),
        0,
        "the exclusion row is gone"
    );
    land(&client, SKILL);
    let (data, _) = client.reconcile(true);
    let entry = data
        .skills
        .iter()
        .find(|s| s.skill == SKILL)
        .expect("delivered again");
    assert!(
        matches!(
            entry.action,
            PullAction::UpToDate | PullAction::FastForwarded
        ),
        "delivery resumed on this device: {:?}",
        entry.action
    );
}

// ── a genesis `--to <channel>` placement REPLACES the everyone default ──────────────────────────────

#[test]
fn e2e_publish_to_channel_replaces_the_everyone_default() {
    let (stack, owner, member, client) = arranged("placement");
    let member_probe = stack.mint_device(&member, "member probe");

    // The owner's authoring CLI ships a NEW skill `--to eng`: the `--to` placement is the
    // targeting mechanism, so the reference lands in #eng ALONE — never additionally in
    // `everyone` (which would deliver it to the whole workspace and make `--to` meaningless).
    let author = FollowHarness::new("placement-author");
    stack.enroll_begin_and_approve(&author, &owner);
    author.resume_apply().expect("the author enrolls");
    author.adopt(TOOLS, &tools_files());
    let digest = author.draft_digest(TOOLS);
    match author
        .publish_to(&format!("{TOOLS}@{digest}"), "eng", "tools for eng")
        .expect("the channel-targeted genesis lands")
    {
        PublishResult::Published(_) => {}
        other => panic!("a genesis lands directly, got {other:?}"),
    }

    let placements = |ch: &str| {
        stack.count(&format!(
            "SELECT count(*) FROM web.channel_bundle cb \
             JOIN web.channel c ON c.id = cb.channel_id \
             JOIN web.bundle b ON b.id = cb.bundle_id \
             WHERE c.name = '{ch}' AND b.name = '{TOOLS}'"
        ))
    };
    assert_eq!(placements("eng"), 1, "the --to placement landed in #eng");
    assert_eq!(
        placements("everyone"),
        0,
        "a --to genesis does NOT also land in everyone"
    );

    // The member (in `everyone`, not #eng) receives NOTHING on the next sweep.
    let (data, warnings) = client.reconcile(true);
    assert!(warnings.is_empty(), "a clean sweep: {warnings:?}");
    assert!(
        !data.skills.iter().any(|s| s.skill == TOOLS),
        "not in #eng — the channel-scoped skill is not delivered: {:?}",
        data.skills
    );
    assert!(client.placement_files(TOOLS).is_empty());

    // Joining #eng delivers it byte-exact — placement was the only thing withholding it.
    let joined = stack.device_put(
        &member_probe.credential,
        &format!(
            "/v1/workspaces/{}/channels/eng/membership",
            stack.workspace_id
        ),
    );
    assert_eq!(joined.status, 200, "the join lands: {}", joined.body);
    land(&client, TOOLS);
    assert_eq!(client.placement_files(TOOLS), expected(&tools_files()));
}

// ── a curated `everyone` gates the genesis default placement: catalog-only until a curator places ──

/// Enroll an owner CLI and TIGHTEN the default channel: a bare channel `protect` sets `curated`
/// (a member-level placement into it takes reviewer+), mode row witnessed. Returns the owner rig.
fn curator_rig(stack: &Stack, owner: &common::Session, tag: &str) -> FollowHarness {
    let rig = FollowHarness::new(tag);
    stack.enroll_begin_and_approve(&rig, owner);
    rig.resume_apply().expect("the owner enrolls");
    let tightened = rig
        .protect("everyone", None, true)
        .expect("the owner may tighten the channel");
    assert_eq!(tightened.level, "curated", "the tighten's applied level");
    assert_eq!(
        stack.text_witness("SELECT mode FROM web.channel WHERE name = 'everyone'"),
        Some("curated".to_owned()),
        "the mode row landed"
    );
    rig
}

/// The `everyone` reference-row count for `TOOLS` — the placement witness.
fn everyone_placements(stack: &Stack) -> i64 {
    stack.count(&format!(
        "SELECT count(*) FROM web.channel_bundle cb \
         JOIN web.channel c ON c.id = cb.channel_id \
         JOIN web.bundle b ON b.id = cb.bundle_id \
         WHERE c.name = 'everyone' AND b.name = '{TOOLS}'"
    ))
}

#[test]
fn e2e_member_genesis_under_curated_everyone_stays_catalog_only() {
    let (stack, owner, _member, client) = arranged("curated");
    let owner_rig = curator_rig(&stack, &owner, "curated-owner");

    // The MEMBER's device ships a BRAND-NEW skill with no `--to`. Custody is never
    // curation-blocked — the genesis SUCCEEDS (catalog row, moved pointer, nothing on the review
    // lane) — but REACH is: the default placement into the CURATED `everyone` is WITHHELD, the
    // receipt disclosing it (`placement_withheld`), and the skill stays CATALOG-ONLY.
    client.adopt(TOOLS, &tools_files());
    let digest = client.draft_digest(TOOLS);
    let published = match client
        .publish_message("", &format!("{TOOLS}@{digest}"), "a member's genesis")
        .expect("the genesis lands")
    {
        PublishResult::Published(d) => d,
        other => panic!("expected a direct genesis, got {other:?}"),
    };
    assert_eq!(published.current_generation, 1, "a fresh pointer");
    assert_eq!(
        published.placement_withheld.as_deref(),
        Some("everyone"),
        "the receipt discloses the withheld default placement"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.bundle WHERE name = '{TOOLS}'"
        )),
        1,
        "the catalog row landed"
    );
    assert_eq!(
        stack.count("SELECT count(*) FROM web.proposal"),
        0,
        "nothing rode the review lane"
    );
    assert_eq!(
        everyone_placements(&stack),
        0,
        "the curated everyone withheld the member's default placement — catalog-only"
    );

    // The CURATOR places it — the real `channel add` verb (an owner passes the curated gate) —
    // and a second device receives the placed skill byte-exact on its next sweep.
    let placed = owner_rig
        .channel_apply("add", "everyone", &[TOOLS])
        .expect("the curator's placement lands");
    assert!(placed.applied, "the placement applied");
    assert_eq!(everyone_placements(&stack), 1, "the reference row landed");
    land(&owner_rig, TOOLS);
    assert_eq!(
        owner_rig.placement_files(TOOLS),
        expected(&tools_files()),
        "the curator-placed skill delivers byte-exact"
    );
}

// ── `--to everyone` rides the same curated gate as any named channel (no string-match bypass) ──────

#[test]
fn e2e_member_to_everyone_is_gated_like_a_named_curated_channel() {
    let (stack, owner, _member, client) = arranged("curated-to");
    let _owner_rig = curator_rig(&stack, &owner, "curated-to-owner");

    // The member ships a brand-new skill EXPLICITLY `--to everyone`: the target routes through
    // the SAME gated path a named channel rides, so it answers exactly what `--to <curated-ch>`
    // answers a member — the publish lands, the placement is withheld (`curated_role_required`
    // on the receipt), and no reference row exists anywhere.
    client.adopt(TOOLS, &tools_files());
    let digest = client.draft_digest(TOOLS);
    let published = match client
        .publish_to(
            &format!("{TOOLS}@{digest}"),
            "everyone",
            "a member's genesis",
        )
        .expect("the genesis lands")
    {
        PublishResult::Published(d) => d,
        other => panic!("expected a direct genesis, got {other:?}"),
    };
    assert_eq!(published.current_generation, 1, "a fresh pointer");
    assert_eq!(
        published.placement_withheld.as_deref(),
        Some("everyone"),
        "the receipt discloses the withheld `--to everyone` placement"
    );
    assert_eq!(
        stack.count(&format!(
            "SELECT count(*) FROM web.channel_bundle cb \
             JOIN web.bundle b ON b.id = cb.bundle_id WHERE b.name = '{TOOLS}'"
        )),
        0,
        "no reference row in ANY channel — the gate answered like a named curated channel"
    );
}
