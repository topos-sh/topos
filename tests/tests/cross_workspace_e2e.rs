//! E2E — the cross-workspace refusal probe. The OSS install is single-tenant, but the device lane
//! still scopes every route by the workspace id in its path — so a credential seated in workspace
//! A must get the UNIFORM wire 404 on every workspace-B route, byte-indistinguishable from a route
//! that names a workspace that has never existed (no oracle in either direction).
//!
//! Workspace B is a bare directly-inserted row (the product deliberately has no second-workspace
//! surface here) — the probe's subject is the lane's seat gate, not B's lifecycle.

mod common;

use common::{OWNER_EMAIL, SKILL, genesis_files};
use topos::test_support::{FollowHarness, PublishResult};

#[test]
fn e2e_a_seated_credential_gets_the_uniform_404_on_every_foreign_workspace_route() {
    let stack = common::start_stack("xws");
    let owner = stack.claim_owner(OWNER_EMAIL);

    // A real enrolled device with real entitlements in workspace A.
    let author = FollowHarness::new("xws-author");
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
    let probe = stack.mint_device(&owner, "xws probe");

    // Workspace B: a second row inserted directly (claimed, so the row is CHECK-valid).
    stack
        .rt
        .block_on(
            sqlx::query(
                "INSERT INTO web.workspace (id, name, display_name, claimed_at)
                 VALUES ('w_other0000000000000000000000000000', 'othr', 'Other', now())",
            )
            .execute(&stack.pool),
        )
        .expect("insert the second workspace row");
    let b = "w_other0000000000000000000000000000";
    let ghost = "w_ghost0000000000000000000000000000";

    // The credential WORKS in A…
    let a_me = stack.device_get(
        &probe.credential,
        &format!("/v1/workspaces/{}/me", stack.workspace_id),
    );
    assert_eq!(a_me.status, 200, "the A lane is live: {}", a_me.body);

    // …and EVERY B route answers the uniform 404, byte-identical to a NEVER-EXISTED workspace
    // and to a wrong path (the no-oracle discipline).
    let reads = [
        format!("/v1/workspaces/{b}/me"),
        format!("/v1/workspaces/{b}/channels"),
        format!("/v1/workspaces/{b}/delivery"),
        format!("/v1/workspaces/{b}/skills"),
        format!("/v1/workspaces/{b}/proposals"),
        format!("/v1/workspaces/{b}/skills/{SKILL}/current"),
        format!("/v1/workspaces/{b}/skills/{SKILL}/log"),
    ];
    let reference = stack.device_get(&probe.credential, &format!("/v1/workspaces/{ghost}/me"));
    assert_eq!(reference.status, 404);
    for path in &reads {
        let answer = stack.device_get(&probe.credential, path);
        assert_eq!(
            answer.status, 404,
            "GET {path} is the uniform miss: {}",
            answer.body
        );
        assert_eq!(
            answer.body, reference.body,
            "GET {path} is byte-identical to the never-existed miss"
        );
    }

    // The row-op writes refuse identically — nothing lands in B.
    let put_follow = stack.device_put(
        &probe.credential,
        &format!("/v1/workspaces/{b}/follows/{SKILL}"),
    );
    assert_eq!(put_follow.status, 404);
    assert_eq!(put_follow.body, reference.body);
    let put_exclusion = stack.device_put(
        &probe.credential,
        &format!("/v1/workspaces/{b}/exclusions/{SKILL}"),
    );
    assert_eq!(put_exclusion.status, 404);
    assert_eq!(put_exclusion.body, reference.body);
    // The RETIRED per-workspace device-revoke route (the global `DELETE /v1/device` replaced it):
    // the catch-all keeps the byte discipline — the dead path answers the same envelope as a miss.
    let del_device = stack.device_delete(
        &probe.credential,
        &format!("/v1/workspaces/{b}/devices"),
        Some(&serde_json::json!({
            "op_id": "a0000000-0000-4000-8000-0000000000bb",
            "target_device_key_id": probe.device_id,
        })),
    );
    assert_eq!(del_device.status, 404);
    assert_eq!(del_device.body, reference.body);

    // A wrong PATH under B answers the same envelope (the splat's miss == the seat gate's miss).
    let wrong_path = stack.device_get(&probe.credential, &format!("/v1/workspaces/{b}/nope"));
    assert_eq!(wrong_path.status, 404);
    assert_eq!(wrong_path.body, reference.body);

    // And the A lane is UNTOUCHED by the probing: still live, still entitled.
    let a_again = stack.device_get(
        &probe.credential,
        &format!("/v1/workspaces/{}/delivery", stack.workspace_id),
    );
    assert_eq!(a_again.status, 200, "A is unaffected: {}", a_again.body);

    // A SEAT without a LINK is still the uniform 404 — the device↔workspace link is a second,
    // independent gate on the lane, byte-indistinguishable from no seat at all.
    stack.seat_in(b, OWNER_EMAIL, "owner");
    let seated_unlinked = stack.device_get(&probe.credential, &format!("/v1/workspaces/{b}/me"));
    assert_eq!(
        seated_unlinked.status, 404,
        "seated but unlinked: {}",
        seated_unlinked.body
    );
    assert_eq!(
        seated_unlinked.body, reference.body,
        "the seat-without-link miss is byte-identical to the never-existed miss"
    );

    // Creating the link over the lane's own op flips exactly that gate: the same read now answers.
    let linked = stack.device_post_json(
        Some(&probe.credential),
        "/v1/device/link",
        &serde_json::json!({ "workspace": "othr" }),
    );
    assert_eq!(linked.status, 200, "the link applies: {}", linked.body);
    let b_me = stack.device_get(&probe.credential, &format!("/v1/workspaces/{b}/me"));
    assert_eq!(
        b_me.status, 200,
        "seat + link opens the lane: {}",
        b_me.body
    );
}
