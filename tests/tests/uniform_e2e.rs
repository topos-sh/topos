//! The NON-ORACLE discipline over real HTTP: every miss on the session lane — a workspace the
//! caller holds no session in, a workspace that never existed, a wrong path, a garbage
//! credential — answers ONE byte-identical uniform 404. The exactly-two pending-tolerant routes
//! (`/me`, `/delivery`) answer typed `session_status: "pending"` where a live row proves standing;
//! every other route stays uniform. The owner's approve arm opens the lane.

mod common;

use common::{OWNER_EMAIL, start_stack};

#[test]
fn every_miss_is_the_one_uniform_404() {
    let stack = start_stack("uniform");
    let owner = stack.claim_owner(OWNER_EMAIL);
    let grant = stack.mint_session(&owner, "probe CLI");
    assert_eq!(grant.session_status.as_deref(), Some("active"));

    // A REAL second workspace the caller holds no session in, and a workspace that never existed.
    let beta_id = stack.add_workspace("beta", "Beta Corp");
    let ghost_id = "w_nope000000000000000000000000000";

    let misses = [
        // The real-but-foreign workspace: reads, row-op writes, and the me describe.
        stack.device_get(
            &grant.credential,
            &format!("/v1/workspaces/{beta_id}/delivery"),
        ),
        stack.device_get(&grant.credential, &format!("/v1/workspaces/{beta_id}/me")),
        stack.device_get(
            &grant.credential,
            &format!("/v1/workspaces/{beta_id}/skills"),
        ),
        stack.device_put(
            &grant.credential,
            &format!("/v1/workspaces/{beta_id}/profile/skills/s_x0000000000000000000000000000"),
        ),
        // The never-existed workspace, same routes.
        stack.device_get(
            &grant.credential,
            &format!("/v1/workspaces/{ghost_id}/delivery"),
        ),
        stack.device_get(&grant.credential, &format!("/v1/workspaces/{ghost_id}/me")),
        // A wrong PATH inside a real workspace.
        stack.device_get(
            &grant.credential,
            &format!("/v1/workspaces/{}/no-such-route", stack.workspace_id),
        ),
        // A garbage credential against the caller's OWN workspace.
        stack.device_get(
            "not-a-credential",
            &format!("/v1/workspaces/{}/delivery", stack.workspace_id),
        ),
    ];
    let reference = &misses[0];
    assert_eq!(reference.status, 404);
    for (i, miss) in misses.iter().enumerate() {
        assert_eq!(miss.status, 404, "miss #{i} is a 404: {}", miss.body);
        assert_eq!(
            miss.body, reference.body,
            "miss #{i} is byte-identical to every other miss"
        );
    }

    // The caller's OWN lane is untouched by all that probing.
    let own = stack.device_get(
        &grant.credential,
        &format!("/v1/workspaces/{}/delivery", stack.workspace_id),
    );
    assert_eq!(own.status, 200, "the live lane still answers: {}", own.body);
}

#[test]
fn the_connect_lane_mints_browser_free_and_misses_uniformly() {
    let stack = start_stack("connect");
    let owner = stack.claim_owner(OWNER_EMAIL);
    let grant = stack.mint_session(&owner, "first CLI");

    // The lane-side second connect: an already-credentialed machine mints a further session with
    // no browser round-trip — seat standing is the trust basis, and the answer carries a FRESH
    // secret (the acting credential never widens its own scope).
    let ok = stack.device_post_json(
        Some(&grant.credential),
        "/v1/login/connect",
        &serde_json::json!({ "workspace": common::WS_NAME, "requested_name": "second CLI" }),
    );
    assert_eq!(ok.status, 200, "the connect mints: {}", ok.body);
    let ok: serde_json::Value = serde_json::from_str(&ok.body).expect("connect JSON");
    assert_eq!(ok["session_status"], "active");
    assert_eq!(ok["workspace"]["name"], common::WS_NAME);
    let fresh = ok["credential"].as_str().expect("credential").to_owned();
    assert_ne!(
        fresh, grant.credential,
        "a fresh secret, never the acting one"
    );
    let ws = ok["workspace"]["workspace_id"]
        .as_str()
        .expect("workspace_id");
    let live = stack.device_get(&fresh, &format!("/v1/workspaces/{ws}/delivery"));
    assert_eq!(
        live.status, 200,
        "the fresh session's lane answers: {}",
        live.body
    );

    // The unauthenticated start stays a non-oracle: a preselect naming NOTHING still mints a
    // flow (the browser approval is where reality is disclosed, behind a signed-in session).
    let start = stack.device_post_json(
        None,
        "/v1/login/authorize",
        &serde_json::json!({ "requested_name": "probe", "preselect": "never-existed" }),
    );
    assert_eq!(start.status, 200, "the start never oracles: {}", start.body);

    // Connect misses — a garbage bearer, a missing bearer, a slug this install does not address —
    // all the ONE byte-identical uniform 404.
    let misses = [
        stack.device_post_json(
            Some("not-a-credential"),
            "/v1/login/connect",
            &serde_json::json!({ "workspace": common::WS_NAME, "requested_name": "x" }),
        ),
        stack.device_post_json(
            None,
            "/v1/login/connect",
            &serde_json::json!({ "workspace": common::WS_NAME, "requested_name": "x" }),
        ),
        stack.device_post_json(
            Some(&grant.credential),
            "/v1/login/connect",
            &serde_json::json!({ "workspace": "never-existed", "requested_name": "x" }),
        ),
    ];
    let reference = &misses[0];
    assert_eq!(reference.status, 404);
    for (i, miss) in misses.iter().enumerate() {
        assert_eq!(
            miss.status, 404,
            "connect miss #{i} is a 404: {}",
            miss.body
        );
        assert_eq!(
            miss.body, reference.body,
            "connect miss #{i} is byte-identical to every other miss"
        );
    }
}

#[test]
fn a_pending_session_gets_exactly_two_typed_answers_until_the_owner_approves() {
    let stack = start_stack("pending");
    let owner = stack.claim_owner(OWNER_EMAIL);
    stack.set_session_approval(true, "pending-lane suite");

    // A MEMBER's own approval births the session PENDING under the knob (an owner's would be
    // active); the flow still grants — the credential exists, the lane is gated.
    let member = stack.add_member("member@acme.test", "member");
    let grant = stack.mint_session_in(&member, "member CLI", common::WS_NAME);
    assert_eq!(
        grant.session_status.as_deref(),
        Some("pending"),
        "the member session is born pending under the knob"
    );

    // Exactly TWO pending-tolerant routes: delivery (shape-complete EMPTY + the typed status)…
    let delivery = stack.device_get(
        &grant.credential,
        &format!("/v1/workspaces/{}/delivery", stack.workspace_id),
    );
    assert_eq!(delivery.status, 200, "{}", delivery.body);
    let delivery: serde_json::Value = serde_json::from_str(&delivery.body).expect("delivery JSON");
    assert_eq!(delivery["session_status"], "pending");
    assert_eq!(delivery["skills"].as_array().map(Vec::len), Some(0));
    // …and me (a live pending row proves standing — the person IS seated).
    let me = stack.device_get(
        &grant.credential,
        &format!("/v1/workspaces/{}/me", stack.workspace_id),
    );
    assert_eq!(me.status, 200, "{}", me.body);
    let me: serde_json::Value = serde_json::from_str(&me.body).expect("me JSON");
    assert_eq!(me["session_status"], "pending");

    // Every OTHER route answers the uniform 404 — byte-identical to a garbage credential's miss.
    // Reads AND row-op writes alike: the profile PUT is gated exactly like the catalog read.
    let gated = stack.device_get(
        &grant.credential,
        &format!("/v1/workspaces/{}/skills", stack.workspace_id),
    );
    let garbage = stack.device_get(
        "not-a-credential",
        &format!("/v1/workspaces/{}/skills", stack.workspace_id),
    );
    assert_eq!(gated.status, 404);
    assert_eq!(gated.body, garbage.body, "the pending gate has no oracle");
    let gated_write = stack.device_put(
        &grant.credential,
        &format!(
            "/v1/workspaces/{}/profile/skills/s_x0000000000000000000000000000",
            stack.workspace_id
        ),
    );
    assert_eq!(gated_write.status, 404);
    assert_eq!(
        gated_write.body, garbage.body,
        "a pending session's row-op write is the same uniform miss"
    );

    // The OWNER approves on the sessions page — the same lane now delivers.
    let approve = owner.post_form(
        "/settings/sessions",
        &[
            ("intent", "approve-session"),
            ("session_id", &grant.session_id),
        ],
    );
    assert_eq!(approve.status, 200, "the approve lands: {}", approve.body);
    let delivery = stack.device_get(
        &grant.credential,
        &format!("/v1/workspaces/{}/delivery", stack.workspace_id),
    );
    assert_eq!(delivery.status, 200);
    let delivery: serde_json::Value = serde_json::from_str(&delivery.body).expect("delivery JSON");
    assert_eq!(delivery["session_status"], "active", "{delivery}");
}
