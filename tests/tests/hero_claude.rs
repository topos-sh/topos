//! The HERO on the REAL harness adapters, over the composed stack: a follower whose rig is wired
//! to the genuine Claude Code / OpenClaw / Hermes adapter enrolls through the device flow, and the
//! promote arms the REAL auto-update surface (the `settings.json` SessionStart hook / OpenClaw's
//! silent auto-update cron through the rig's fake CLI / the Hermes `config.yaml`
//! session-boundary entries) while the `everyone` genesis lands byte-exact in the adapter's OWN skill
//! directory. An update then lands on a subsequent bare sweep through the same adapter.
//!
//! Table-driven: one case row per adapter, the SAME flow. The honest ceiling stands:
//! hook-installed plus bytes-materialized is asserted; that a live session's hook output reaches
//! model context is a documented manual MUST-VERIFY per harness.

mod common;

use common::{OWNER_EMAIL, SKILL, expected, genesis_files};
use topos::test_support::{FollowHarness, PublishResult, Scope};
use topos_types::results::PullAction;

/// One adapter case: how the rig is built + how its armed auto-update surface is witnessed.
struct Case {
    tag: &'static str,
    rig: fn(&str) -> FollowHarness,
    /// Read the adapter's config surface after enrollment; `None` = not armed / missing.
    config: fn(&FollowHarness) -> Option<String>,
    /// A substring the armed config MUST carry (the topos auto-update entry).
    marker: &'static str,
}

fn v2_files() -> Vec<(&'static str, bool, &'static [u8])> {
    vec![
        (
            "SKILL.md",
            false,
            b"# deploy\nDeploy the service. v2.\n" as &[u8],
        ),
        ("run.sh", true, b"#!/bin/sh\necho deploying\n" as &[u8]),
    ]
}

#[test]
fn e2e_real_adapters_arm_their_currency_surface_and_land_the_bytes() {
    let cases = [
        Case {
            tag: "claude",
            rig: FollowHarness::new_claude,
            config: FollowHarness::settings_json,
            marker: "SessionStart",
        },
        Case {
            tag: "openclaw",
            rig: FollowHarness::new_openclaw,
            config: FollowHarness::openclaw_cron_state,
            marker: "topos:openclaw:currency:2",
        },
        Case {
            tag: "hermes",
            rig: FollowHarness::new_hermes,
            config: FollowHarness::hermes_config,
            marker: "on_session_start",
        },
    ];

    let stack = common::start_stack("adapters");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // One author ships the genesis; every adapter rig then receives it.
    let author = FollowHarness::new("adapters-author");
    stack.enroll_begin_and_approve(&author, &owner);
    author.resume_apply().expect("the author enrolls");
    author.adopt(SKILL, &genesis_files());
    let digest = author.draft_digest(SKILL);
    match author
        .publish_message("", &format!("{SKILL}@{digest}"), "genesis")
        .expect("the genesis lands")
    {
        PublishResult::Published(_) => {}
        other => panic!("expected a direct genesis, got {other:?}"),
    }

    let mut rigs = Vec::new();
    for case in &cases {
        let client = (case.rig)(case.tag);
        stack.enroll_begin_and_approve(&client, &owner);
        let applied = client.resume_apply().expect("the adapter rig enrolls");
        assert!(applied.enrolled_now, "[{}] enrolled", case.tag);

        // The genesis lands byte-exact in the ADAPTER's own skill directory (offer-accepted when
        // the sweep chose the first-receive consent shape).
        let (data, _) = client.reconcile(true);
        if data
            .skills
            .iter()
            .any(|s| s.skill == SKILL && s.action == PullAction::Offered)
        {
            let _ = client.pull(Scope::Accept {
                name: SKILL.to_owned(),
            });
        }
        assert_eq!(
            client.placement_files(SKILL),
            expected(&genesis_files()),
            "[{}] the genesis is placed byte-exact through the real adapter",
            case.tag
        );

        // The enrollment promote ARMED the adapter's real auto-update surface.
        let config = (case.config)(&client)
            .unwrap_or_else(|| panic!("[{}] the auto-update config file exists", case.tag));
        assert!(
            config.contains(case.marker),
            "[{}] the auto-update entry is registered: {config}",
            case.tag
        );
        rigs.push(client);
    }

    // OpenClaw's promote writes NO file of its own any more (the inject surface is retired) —
    // the registered cron above is the whole trigger footprint.

    // v2 ships; every adapter's bare sweep lands it through the same placement path.
    author.edit_placement(SKILL, &v2_files());
    let digest = author.draft_digest(SKILL);
    author
        .publish_message("", &format!("{SKILL}@{digest}"), "v2")
        .expect("v2 lands");
    for (case, client) in cases.iter().zip(&rigs) {
        let (data, warnings) = client.reconcile(true);
        assert!(
            warnings.is_empty(),
            "[{}] a clean v2 sweep: {warnings:?}",
            case.tag
        );
        let entry = data
            .skills
            .iter()
            .find(|s| s.skill == SKILL)
            .unwrap_or_else(|| panic!("[{}] still followed", case.tag));
        assert_eq!(entry.action, PullAction::FastForwarded, "[{}]", case.tag);
        assert_eq!(
            client.placement_files(SKILL),
            expected(&v2_files()),
            "[{}] v2 lands byte-exact",
            case.tag
        );
    }
}
