//! MULTI-WORKSPACE e2e — one client, one plane, TWO workspaces, every verb targets the right one.
//!
//! The headline proof that a single `~/.topos/` install can follow skills from **several workspaces on the
//! same plane** and that every verb scopes itself to the correct one. On ONE loopback plane it seeds two
//! independent workspaces — **A** ("Acme", skill `s_alpha`) and **B** ("Beacon", skill `s_beacon`) — each
//! with the same invitee rostered (as an owner, so the joiner can itself invite) and its own `/i/` link.
//! A single [`FollowHarness`] then drives the GENUINE `topos follow` twice into the same sidecar (same
//! `base_url`, same TOFU-pinned plane key) and asserts, end to end:
//!
//! 1. after B, `user.json` carries BOTH memberships (A never dropped) and `follows.json` tags each skill
//!    with its OWN workspace; both bundles land byte-exact.
//! 2. `list --json` carries each skill's own `workspace_id` — the two workspaces are distinguishable.
//! 3. a bare `pull` sweep is up-to-date for BOTH, each verified under its own workspace scope.
//! 4. an authoring `publish` of a `s_beacon` draft moves **B's** `current` and leaves **A's** untouched —
//!    skill-inference signs the skill's OWN workspace, not an ambient guess (the plane state is queried
//!    for both).
//! 5. `invite` with no `--workspace` fails LOCALLY (`WorkspaceSelection`, never reaching the plane), while
//!    `invite --workspace B` mints an invite scoped to B (the plane's invite row lands in B, not A).

mod common;

use common::{NOW, Plane, Seeded, expected_placement};
use ed25519_dalek::SigningKey;
use plane_store::{
    Authority, ConfirmOutcome, FileMode, OpId, Principal, Role, SkillId, UploadedFile, WorkspaceId,
};
use sqlx::Row as _;
use topos::test_support::{FollowHarness, PublishResult, Scope};
use topos_types::{Generation, TerminalOutcome};

// ── the two workspaces on the ONE plane ─────────────────────────────────────────────────────────────
const WS_A: &str = "w_acme";
const SA: &str = "s_alpha";
const DISPLAY_A: &str = "Acme";
const WS_B: &str = "w_beacon";
const SB: &str = "s_beacon";
const DISPLAY_B: &str = "Beacon";

/// The ONE invitee the same client confirms in both workspaces — one device, one principal, two memberships.
const INVITEE: &str = "alice@acme.test";

const AUTHOR: &str = "d_seed";
const MSG: &str = "topos publish";
const AT: &str = "2026-07-07T00:00:00Z";

// Per-workspace bootstrap owner (mints the invite + publishes the genesis; distinct device seeds so the
// two workspaces never share a signing key). Distinct op ids everywhere — an op id is plane-unique.
const OWNER_SEED_A: [u8; 32] = [21u8; 32];
const OWNER_SEED_B: [u8; 32] = [22u8; 32];
const OWNER_DKID_A: &str = "dk_owner_a";
const OWNER_DKID_B: &str = "dk_owner_b";
const OWNER_A: &str = "p_owner_a";
const OWNER_B: &str = "p_owner_b";
const GENESIS_OP_A: &str = "a0000000-0000-4000-8000-000000000001";
const GENESIS_OP_B: &str = "a0000000-0000-4000-8000-000000000002";
const INVITE_OP_A: &str = "b0000000-0000-4000-8000-000000000001";
const INVITE_OP_B: &str = "b0000000-0000-4000-8000-000000000002";

/// Workspace A's genesis bundle (its own distinct bytes — so a cross-workspace mix-up is visible).
fn files_a() -> Vec<UploadedFile> {
    vec![
        UploadedFile {
            path: "SKILL.md".to_owned(),
            mode: FileMode::Regular,
            bytes: b"# alpha\nAcme deploy.\n".to_vec(),
        },
        UploadedFile {
            path: "run.sh".to_owned(),
            mode: FileMode::Executable,
            bytes: b"#!/bin/sh\necho alpha\n".to_vec(),
        },
    ]
}

/// Workspace B's genesis bundle — DIFFERENT bytes from A's.
fn files_b() -> Vec<UploadedFile> {
    vec![
        UploadedFile {
            path: "SKILL.md".to_owned(),
            mode: FileMode::Regular,
            bytes: b"# beacon\nBeacon deploy.\n".to_vec(),
        },
        UploadedFile {
            path: "run.sh".to_owned(),
            mode: FileMode::Executable,
            bytes: b"#!/bin/sh\necho beacon\n".to_vec(),
        },
    ]
}

/// The `s_beacon` draft the item-4 publish ships — a forward child of B's genesis.
const B_DRAFT: &[(&str, bool, &[u8])] = &[
    ("SKILL.md", false, b"# beacon\nBeacon deploy v2.\n"),
    ("run.sh", true, b"#!/bin/sh\necho beacon v2\n"),
];

// ── seeding: two full workspaces on the one authority ───────────────────────────────────────────────

/// Everything one workspace needs so the invitee can follow it, then author + invite within it.
struct WsSeed<'a> {
    ws: &'a str,
    skill: &'a str,
    display: &'a str,
    owner: &'a str,
    owner_dkid: &'a str,
    owner_seed: &'a [u8; 32],
    genesis_op: &'a str,
    invite_op: &'a str,
    files: Vec<UploadedFile>,
    /// The offered skill NAME the follower adopts locally (advisory — not signed). `None` ⇒ the follower
    /// names the skill by its id. The same-name item gives a skill the SAME name in both workspaces here.
    offered_name: Option<&'a str>,
}

/// Stand ONE workspace up on the shared authority: the workspace row, its bootstrap owner (rostered on the
/// skill so it can publish the genesis, and the signer of the invite), a published genesis at `(1,1)`, the
/// invitee pre-rostered as an OWNER (so a redeem confirms them owner — able to author + invite within it),
/// and an owner-role `/i/` invite offering the skill. Returns the minted invite link.
async fn seed_workspace_with_invite(authority: &Authority, s: WsSeed<'_>) -> String {
    let ws = WorkspaceId::parse(s.ws).unwrap();
    let skill = SkillId::parse(s.skill).unwrap();
    let owner = Principal::parse(s.owner).unwrap();
    let invitee = Principal::parse(INVITEE).unwrap();
    let owner_pk = SigningKey::from_bytes(s.owner_seed)
        .verifying_key()
        .to_bytes();

    authority
        .seed_workspace(&ws, s.display, "verified", "cloud")
        .await
        .expect("seed workspace");
    authority
        .seed_workspace_member(&ws, &owner, "owner", "confirmed")
        .await
        .expect("seed bootstrap owner");
    authority
        .seed_device(&ws, s.owner_dkid, &owner_pk, &owner, false)
        .await
        .expect("seed owner device");
    authority
        .seed_roster(&ws, &skill, &owner)
        .await
        .expect("roster the owner so it can publish the genesis");
    let receipt = authority
        .seed_published_genesis(
            &ws,
            &skill,
            s.owner_dkid,
            s.owner_seed,
            &OpId::parse(s.genesis_op).unwrap(),
            s.files,
            AUTHOR,
            MSG,
            AT,
            NOW,
        )
        .await
        .expect("seed genesis");
    assert_eq!(receipt.outcome, TerminalOutcome::Ok);
    assert_eq!(receipt.current, Some(Generation { epoch: 1, seq: 1 }));

    // Pre-roster the invitee (as an OWNER — the cloud redeem gate needs a rostered member, and an owner can
    // itself mint invites for item 5). The redeem then rosters them on the OFFERED skill (item 4's write).
    authority
        .seed_workspace_member(&ws, &invitee, "owner", "invited")
        .await
        .expect("pre-roster the invitee");

    common::mint_invite_with_role(
        authority,
        &ws,
        (s.owner_dkid, s.owner_seed),
        s.invite_op,
        INVITEE,
        s.skill,
        s.offered_name,
        Role::Owner,
        AT,
    )
    .await
}

/// Seed BOTH workspaces on the ONE authority; returns the two `/i/` links in `[A, B]` order.
async fn seed_two_workspaces(authority: &Authority) -> Seeded {
    let link_a = seed_workspace_with_invite(
        authority,
        WsSeed {
            ws: WS_A,
            skill: SA,
            display: DISPLAY_A,
            owner: OWNER_A,
            owner_dkid: OWNER_DKID_A,
            owner_seed: &OWNER_SEED_A,
            genesis_op: GENESIS_OP_A,
            invite_op: INVITE_OP_A,
            files: files_a(),
            offered_name: None,
        },
    )
    .await;
    let link_b = seed_workspace_with_invite(
        authority,
        WsSeed {
            ws: WS_B,
            skill: SB,
            display: DISPLAY_B,
            owner: OWNER_B,
            owner_dkid: OWNER_DKID_B,
            owner_seed: &OWNER_SEED_B,
            genesis_op: GENESIS_OP_B,
            invite_op: INVITE_OP_B,
            files: files_b(),
            offered_name: None,
        },
    )
    .await;
    Seeded {
        genesis: None,
        invites: vec![link_a, link_b],
    }
}

fn start_plane(tag: &str) -> Plane {
    common::start_plane("topos-mws-e2e", tag, true, seed_two_workspaces)
}

// ── plane-state witnesses (row-level, via the per-test pool) ────────────────────────────────────────

/// The plane's authoritative `current` generation for a skill, or `None` if it has no `current` — read
/// straight off the `current` row so item 4 proves which workspace's pointer actually moved.
fn current_gen(plane: &Plane, ws: &str, skill: &str) -> Option<(i64, i64)> {
    plane.rt.block_on(async {
        sqlx::query("SELECT epoch, seq FROM current WHERE workspace_id = $1 AND skill_id = $2")
            .bind(ws)
            .bind(skill)
            .fetch_optional(&plane.pool)
            .await
            .expect("query current")
            .map(|row| (row.get::<i64, _>("epoch"), row.get::<i64, _>("seq")))
    })
}

/// How many invites a workspace holds — item 5 proves an ambient invite mints into B and never touches A.
fn invite_count(plane: &Plane, ws: &str) -> i64 {
    plane.rt.block_on(async {
        sqlx::query("SELECT count(*) AS n FROM invites WHERE workspace_id = $1")
            .bind(ws)
            .fetch_one(&plane.pool)
            .await
            .expect("count invites")
            .get::<i64, _>("n")
    })
}

/// The real two-call `follow`: begin (bootstrap → TOFU-pin → device-authorize), confirm the invitee's
/// identity headless, resume (redeem + promote). Leaves the offered skill as a never-received baseline.
fn follow_workspace(plane: &Plane, client: &FollowHarness, invite_idx: usize) {
    let pending = client
        .follow(plane.invite(invite_idx), plane.plane_key)
        .expect("follow call 1");
    assert!(!pending.enrolled, "call 1 only begins enrollment");
    let user_code = pending
        .pending
        .as_ref()
        .expect("the pending arm carries the verification handle")
        .user_code
        .clone();
    let confirm = plane
        .rt
        .block_on(
            plane
                .authority
                .confirm_external_identity(&user_code, INVITEE, NOW),
        )
        .expect("confirm the session identity");
    assert!(matches!(confirm, ConfirmOutcome::Confirmed));
    let done = client.resume(plane.plane_key).expect("follow --resume");
    assert!(done.enrolled, "enrolled after the resume redeem");
}

/// [`follow_workspace`] then `follow --approve <skill_name>` — placing the offered skill's genesis bytes.
fn follow_and_approve(plane: &Plane, client: &FollowHarness, invite_idx: usize, skill_name: &str) {
    follow_workspace(plane, client, invite_idx);
    client
        .approve(&plane.base_url, plane.plane_key, &[skill_name.to_owned()])
        .expect("follow --approve places the first-received bytes");
}

// ── the keystone: one client, two workspaces, every verb scoped right ────────────────────────────────

#[test]
fn one_client_follows_two_workspaces_and_every_verb_targets_the_right_one() {
    let plane = start_plane("mws");
    let client = FollowHarness::new("mws");

    // ── Item 1 — follow A, then follow B into the SAME sidecar (same plane, same pinned key) ──
    follow_and_approve(&plane, &client, 0, SA);
    assert_eq!(
        client.memberships().len(),
        1,
        "after A: exactly one membership"
    );

    follow_and_approve(&plane, &client, 1, SB);

    // BOTH memberships present — correct ids + display names, A never dropped by B's promote.
    let mut members = client.memberships();
    members.sort();
    assert_eq!(
        members,
        vec![
            (WS_A.to_owned(), Some(DISPLAY_A.to_owned())),
            (WS_B.to_owned(), Some(DISPLAY_B.to_owned())),
        ],
        "both memberships (ids + display names); A retained after B"
    );

    // follows.json — both skills, each tagged with its OWN workspace, both following, A not dropped.
    let mut follows = client.follows();
    follows.sort();
    assert_eq!(
        follows,
        vec![
            (SA.to_owned(), WS_A.to_owned(), true),
            (SB.to_owned(), WS_B.to_owned(), true),
        ],
        "each follow carries its own workspace_id; A's entry survived B's merge"
    );

    // Both bundles placed byte-exact (path/mode/content), and the two bundles genuinely differ.
    let placed_a = client.placement_files(SA);
    let placed_b = client.placement_files(SB);
    assert_eq!(placed_a, expected_placement(&files_a()), "A byte-exact");
    assert_eq!(placed_b, expected_placement(&files_b()), "B byte-exact");
    assert_ne!(placed_a, placed_b, "no cross-workspace byte contamination");

    // ── Item 2 — `list --json`: each skill carries its OWN workspace_id ──
    let list = client.list();
    let entry = |name: &str| {
        list.tracked
            .iter()
            .find(|e| e.skill == name)
            .unwrap_or_else(|| panic!("{name} present in list.tracked"))
    };
    assert_eq!(
        entry(SA).workspace_id.as_deref(),
        Some(WS_A),
        "list scopes sA to workspace A"
    );
    assert_eq!(
        entry(SB).workspace_id.as_deref(),
        Some(WS_B),
        "list scopes sB to workspace B"
    );
    assert_ne!(
        entry(SA).workspace_id,
        entry(SB).workspace_id,
        "the two workspaces are distinguishable in list"
    );
    // Both are also in the followed bucket, each with its own provenance.
    assert_eq!(list.followed.len(), 2, "both skills are followed");

    // ── Item 3 — a bare `pull` sweep: up-to-date for BOTH, each under its own workspace scope ──
    let pull = client.pull(Scope::AllFollowed);
    let pulled = |name: &str| {
        pull.skills
            .iter()
            .find(|s| s.skill == name)
            .unwrap_or_else(|| panic!("{name} present in the pull sweep"))
    };
    assert_eq!(
        pulled(SA).workspace_id.as_deref(),
        Some(WS_A),
        "the sweep stamps sA with workspace A"
    );
    assert_eq!(
        pulled(SB).workspace_id.as_deref(),
        Some(WS_B),
        "the sweep stamps sB with workspace B"
    );
    // Each pointer verified under its own workspace read scope — both sit at their genesis current (1,1).
    assert_eq!(pulled(SA).applied, Generation { epoch: 1, seq: 1 });
    assert_eq!(pulled(SB).applied, Generation { epoch: 1, seq: 1 });

    // ── Item 4 — authoring `s_beacon` signs B's OWN workspace (skill-inference), never A ──
    assert_eq!(
        current_gen(&plane, WS_A, SA),
        Some((1, 1)),
        "A's current starts at genesis"
    );
    assert_eq!(
        current_gen(&plane, WS_B, SB),
        Some((1, 1)),
        "B's current starts at genesis"
    );

    client.edit_placement(SB, B_DRAFT);
    let digest = client.draft_digest(SB);
    let published = client
        .publish(&plane.base_url, plane.plane_key, &format!("{SB}@{digest}"))
        .expect("publish the sB draft");
    match published {
        PublishResult::Published(d) => assert_eq!(
            d.current_generation,
            Some(Generation { epoch: 1, seq: 2 }),
            "B's current moved +1"
        ),
        other => panic!("expected a direct publish, got {other:?}"),
    }

    // The signing-scope proof: B's current advanced, A's did NOT — the op was scoped to sB's OWN workspace.
    assert_eq!(
        current_gen(&plane, WS_B, SB),
        Some((1, 2)),
        "B's current moved to (1,2) — the op signed in B"
    );
    assert_eq!(
        current_gen(&plane, WS_A, SA),
        Some((1, 1)),
        "A's current is untouched — the publish did NOT bleed into A"
    );

    // ── Item 5 — ambient verb selection: `invite` without / with `--workspace` ──
    let a_before = invite_count(&plane, WS_A);
    let b_before = invite_count(&plane, WS_B);

    // No `--workspace` while two workspaces are joined ⇒ a LOCAL WorkspaceSelection error; never a plane hit.
    let err = client
        .invite("bob@acme.test", &[SB])
        .expect_err("an ambiguous invite must fail locally, not mint into an arbitrary workspace");
    assert!(
        err.contains("--workspace") && err.contains(WS_A) && err.contains(WS_B),
        "the WorkspaceSelection error names --workspace + both joined ids: {err}"
    );
    assert_eq!(
        invite_count(&plane, WS_A),
        a_before,
        "the ambiguous invite minted nothing in A"
    );
    assert_eq!(
        invite_count(&plane, WS_B),
        b_before,
        "the ambiguous invite minted nothing in B (never reached the plane)"
    );

    // `--workspace B` ⇒ the governance op the plane verifies is scoped to B (the invite lands in B, not A).
    let link = client
        .invite_in_workspace("carol@acme.test", &[SB], WS_B)
        .expect("invite --workspace B mints an invite");
    assert!(link.contains("/i/"), "a real /i/ link: {link}");
    assert_eq!(
        invite_count(&plane, WS_B),
        b_before + 1,
        "the invite was minted in B"
    );
    assert_eq!(
        invite_count(&plane, WS_A),
        a_before,
        "workspace A is untouched by a B-scoped invite"
    );
}

// ── Item 6 — same-name disambiguation: `publish <name> --workspace <id>` ─────────────────────────────
//
// A DEDICATED plane whose two workspaces each offer a skill under the SAME display name "common" (distinct
// ids). One install follows both; the shared name is then ambiguous to a bare `publish`, and only
// `--workspace` resolves it — proving the `resolve_skill_in_workspace` filter end to end.

const SHARED_A: &str = "s_shared_a";
const SHARED_B: &str = "s_shared_b";
const SHARED_NAME: &str = "common";

/// The `s_shared_a` draft the `--workspace A` publish ships (a forward child of A's genesis).
const SHARED_DRAFT: &[(&str, bool, &[u8])] = &[
    ("SKILL.md", false, b"# common\nShared skill, edited in Acme.\n"),
    ("run.sh", true, b"#!/bin/sh\necho common v2\n"),
];

/// Two workspaces on ONE plane, EACH offering its own skill under the identical name `"common"`.
async fn seed_two_shared_name_workspaces(authority: &Authority) -> Seeded {
    let link_a = seed_workspace_with_invite(
        authority,
        WsSeed {
            ws: WS_A,
            skill: SHARED_A,
            display: DISPLAY_A,
            owner: OWNER_A,
            owner_dkid: OWNER_DKID_A,
            owner_seed: &OWNER_SEED_A,
            genesis_op: GENESIS_OP_A,
            invite_op: INVITE_OP_A,
            files: files_a(),
            offered_name: Some(SHARED_NAME),
        },
    )
    .await;
    let link_b = seed_workspace_with_invite(
        authority,
        WsSeed {
            ws: WS_B,
            skill: SHARED_B,
            display: DISPLAY_B,
            owner: OWNER_B,
            owner_dkid: OWNER_DKID_B,
            owner_seed: &OWNER_SEED_B,
            genesis_op: GENESIS_OP_B,
            invite_op: INVITE_OP_B,
            files: files_b(),
            offered_name: Some(SHARED_NAME),
        },
    )
    .await;
    Seeded {
        genesis: None,
        invites: vec![link_a, link_b],
    }
}

#[test]
fn publish_disambiguates_a_shared_skill_name_by_workspace() {
    let plane = common::start_plane(
        "topos-mws-e2e",
        "mws-shared",
        true,
        seed_two_shared_name_workspaces,
    );
    let client = FollowHarness::new("mws-shared");

    // Follow A and PLACE its "common" skill (unambiguous while B isn't joined yet — so a subsequent publish
    // has real bytes to draft). Then follow B WITHOUT approving: its "common" skill stays a never-received
    // baseline, still TRACKED — so now two skills answer to the name "common", one per workspace.
    follow_and_approve(&plane, &client, 0, SHARED_NAME);
    follow_workspace(&plane, &client, 1);

    assert_eq!(client.memberships().len(), 2, "both workspaces joined");
    let list = client.list();
    let commons: Vec<&_> = list
        .tracked
        .iter()
        .filter(|e| e.skill == SHARED_NAME)
        .collect();
    assert_eq!(commons.len(), 2, "two tracked skills share the name 'common'");
    let mut scopes: Vec<String> = commons
        .iter()
        .filter_map(|e| e.workspace_id.clone())
        .collect();
    scopes.sort();
    assert_eq!(
        scopes,
        vec![WS_A.to_owned(), WS_B.to_owned()],
        "one 'common' per workspace"
    );

    // Stage a draft on A's (placed) copy so a resolved publish has something to ship.
    client.edit_placement(SHARED_A, SHARED_DRAFT);
    let token = format!("{SHARED_NAME}@{}", client.draft_digest(SHARED_A));

    // `publish <name>` with NO `--workspace` ⇒ AMBIGUOUS across the two workspaces — refused before any send.
    let err = client
        .publish(&plane.base_url, plane.plane_key, &token)
        .expect_err("a shared name with no --workspace must be ambiguous");
    assert!(
        err.to_lowercase().contains("ambiguous"),
        "ambiguous-name guidance: {err}"
    );
    assert_eq!(
        current_gen(&plane, WS_A, SHARED_A),
        Some((1, 1)),
        "A's current unmoved by the refused publish"
    );
    assert_eq!(
        current_gen(&plane, WS_B, SHARED_B),
        Some((1, 1)),
        "B's current unmoved by the refused publish"
    );

    // `publish <name> --workspace A` ⇒ resolves to A's copy and ships it; B's identically-named copy is untouched.
    let published = client
        .publish_in_workspace(&plane.base_url, plane.plane_key, &token, WS_A)
        .expect("--workspace A resolves the shared name to A");
    match published {
        PublishResult::Published(d) => assert_eq!(
            d.current_generation,
            Some(Generation { epoch: 1, seq: 2 }),
            "A's 'common' moved +1"
        ),
        other => panic!("expected a direct publish, got {other:?}"),
    }
    assert_eq!(
        current_gen(&plane, WS_A, SHARED_A),
        Some((1, 2)),
        "A's 'common' current moved — the name resolved to A"
    );
    assert_eq!(
        current_gen(&plane, WS_B, SHARED_B),
        Some((1, 1)),
        "B's identically-named 'common' is untouched"
    );
}
