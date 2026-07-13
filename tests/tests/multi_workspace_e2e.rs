//! MULTI-WORKSPACE e2e — one client, one plane, TWO workspaces, every verb targets the right one.
//!
//! The headline proof that a single `~/.topos/` install can follow skills from **several workspaces on the
//! same plane** and that every verb scopes itself to the correct one. On ONE loopback plane it seeds two
//! independent workspaces — **A** ("Acme", skill `s_alpha`) and **B** ("Beacon", skill `s_beacon`) — each
//! with the same invitee rostered (as an owner, so the joiner can itself invite). A single
//! [`FollowHarness`] then drives the GENUINE `topos follow <address>` twice into the same sidecar (same
//! `base_url`, one plane — no trust root to pin, the `current` pointer is unsigned) and asserts, end to end:
//!
//! 1. after B, `user.json` carries BOTH memberships (A never dropped) and `follows.json` tags each skill
//!    with its OWN workspace; both bundles land byte-exact.
//! 2. `list --json` carries each skill's own `workspace_id` — the two workspaces are distinguishable.
//! 3. a bare `pull` sweep is up-to-date for BOTH, each verified under its own workspace scope.
//! 4. an authoring `publish` of a `s_beacon` draft moves **B's** `current` and leaves **A's** untouched —
//!    skill-inference signs the skill's OWN workspace, not an ambient guess (the plane state is queried
//!    for both).
//! 5. `invite` with no `--workspace` fails LOCALLY (`WorkspaceSelection`, never reaching the plane), while
//!    `invite --workspace B` seats a new invited member scoped to B (the roster row lands in B, not A).

mod common;

use common::{NOW, Plane, Seeded, expected_placement};
use plane_store::{Authority, FileMode, OpId, Principal, SkillId, UploadedFile, WorkspaceId};
use sqlx::Row as _;
use topos::test_support::{FollowHarness, PublishResult, Scope};
use topos_types::{Generation, TerminalOutcome};

// ── the two workspaces on the ONE plane ─────────────────────────────────────────────────────────────
// The skill IDS are slug-clean (`s-alpha`, not `s_alpha`): a published genesis's CATALOG name is the slug
// of its display name (`_`→`-`), and the follower installs + resolves the skill by that catalog name. A
// slug-clean id makes id == name, so a single constant is both the custody id (the `current` row's
// `skill_id`, the placement dir) AND the resolvable name (`list`/`pull`/`publish`).
const WS_A: &str = "w_acme";
const SA: &str = "s-alpha";
const DISPLAY_A: &str = "Acme";
const WS_B: &str = "w_beacon";
const SB: &str = "s-beacon";
const DISPLAY_B: &str = "Beacon";

/// The ONE invitee the same client confirms in both workspaces — one device, one principal, two memberships.
const INVITEE: &str = "alice@acme.test";

const AUTHOR: &str = "d_seed";
const MSG: &str = "topos publish";
const AT: &str = "2026-07-07T00:00:00Z";

// Per-workspace bootstrap owner (mints the invite + publishes the genesis; distinct device public keys so
// the two workspaces never share an owner device). Distinct op ids everywhere — an op id is plane-unique.
const OWNER_PUBKEY_A: [u8; 32] = [21u8; 32];
const OWNER_PUBKEY_B: [u8; 32] = [22u8; 32];
const OWNER_DKID_A: &str = "dk_owner_a";
const OWNER_DKID_B: &str = "dk_owner_b";
const OWNER_A: &str = "p_owner_a";
const OWNER_B: &str = "p_owner_b";
/// Each bootstrap owner's workspace Bearer credential (authenticates its genesis publish + invite mint).
const OWNER_CRED_A: &str = "wc_owner_a_secret";
const OWNER_CRED_B: &str = "wc_owner_b_secret";
const GENESIS_OP_A: &str = "a0000000-0000-4000-8000-000000000001";
const GENESIS_OP_B: &str = "a0000000-0000-4000-8000-000000000002";

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
    owner_pubkey: &'a [u8; 32],
    owner_credential: &'a str,
    genesis_op: &'a str,
    files: Vec<UploadedFile>,
    /// The CATALOG display name the genesis publishes under (what the follower's local skill resolves by).
    /// `None` ⇒ the skill id doubles as the name. The same-name item gives a skill the SAME name in both
    /// workspaces — a collision across workspaces the disambiguation item drives.
    offered_name: Option<&'a str>,
}

/// Stand ONE workspace up on the shared authority: the workspace row, its bootstrap owner (a confirmed
/// owner holding `owner_credential` — that credential publishes the genesis), a published genesis at
/// `(1,1)` in the structural `everyone` (so every confirmed member is entitled to it), and the invitee
/// pre-seated as an OWNER, INVITED (so a redeem flips it to confirmed — able to author + invite within it).
/// No invite link: joining is by the workspace ADDRESS, and the invited seat is the lock.
async fn seed_one_workspace(authority: &Authority, s: WsSeed<'_>) {
    let ws = WorkspaceId::parse(s.ws).unwrap();
    let skill = SkillId::parse(s.skill).unwrap();
    let invitee = Principal::parse(INVITEE).unwrap();

    authority
        .seed_workspace(&ws, s.display, "verified", "cloud")
        .await
        .expect("seed workspace");
    // The bootstrap owner: a confirmed owner holding its credential (a genesis publish needs confirmed
    // membership now — per-skill roster grants nothing; the credential also drives item 5's invite).
    common::seed_member(
        authority,
        &ws,
        s.owner_dkid,
        s.owner_pubkey,
        s.owner,
        "owner",
        s.owner_credential,
    )
    .await;
    let receipt = authority
        .seed_published_genesis(
            &ws,
            &skill,
            s.owner_credential,
            &OpId::parse(s.genesis_op).unwrap(),
            s.files,
            AUTHOR,
            MSG,
            // A real publish carries the folder name — the CATALOG display name the follower's local skill
            // resolves by (the offered name, else the skill id). The same-name item gives both workspaces
            // the SAME name here, so the catalog label is what drives the `--workspace` disambiguation.
            s.offered_name.or(Some(s.skill)),
            AT,
            NOW,
        )
        .await
        .expect("seed genesis");
    assert_eq!(receipt.outcome, TerminalOutcome::Ok);
    assert_eq!(receipt.current, Some(Generation { epoch: 1, seq: 1 }));

    // Pre-seat the invitee (an OWNER — the cloud redeem gate needs a seated member, and an owner can itself
    // invite for item 5; INVITED so the redeem flips it to confirmed). The genesis rides `everyone`, so the
    // confirmed member is entitled to it the instant they join — no per-skill offer needed.
    authority
        .seed_workspace_member(&ws, &invitee, "owner", "invited")
        .await
        .expect("pre-roster the invitee");
}

/// Seed BOTH workspaces on the ONE authority (no invite links — the address is the join door).
async fn seed_two_workspaces(authority: &Authority) -> Seeded {
    seed_one_workspace(
        authority,
        WsSeed {
            ws: WS_A,
            skill: SA,
            display: DISPLAY_A,
            owner: OWNER_A,
            owner_dkid: OWNER_DKID_A,
            owner_pubkey: &OWNER_PUBKEY_A,
            owner_credential: OWNER_CRED_A,
            genesis_op: GENESIS_OP_A,
            files: files_a(),
            offered_name: None,
        },
    )
    .await;
    seed_one_workspace(
        authority,
        WsSeed {
            ws: WS_B,
            skill: SB,
            display: DISPLAY_B,
            owner: OWNER_B,
            owner_dkid: OWNER_DKID_B,
            owner_pubkey: &OWNER_PUBKEY_B,
            owner_credential: OWNER_CRED_B,
            genesis_op: GENESIS_OP_B,
            files: files_b(),
            offered_name: None,
        },
    )
    .await;
    Seeded {
        genesis: None,
        invites: Vec::new(),
    }
}

fn start_plane(tag: &str) -> Plane {
    common::start_stack("topos-mws-e2e", tag, true, seed_two_workspaces)
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

/// How many INVITED (pending) roster seats a workspace holds — item 5 proves an ambient invite seats into
/// B and never touches A. The reshaped `invite` is a member-lane ROSTER write (a `workspace_member` row at
/// status `invited`), not an invite-link row — the `/i/` mechanism is gone, so the witness reads the roster.
fn invite_count(plane: &Plane, ws: &str) -> i64 {
    plane.rt.block_on(async {
        sqlx::query(
            "SELECT count(*) AS n FROM workspace_member WHERE workspace_id = $1 AND status = 'invited'",
        )
        .bind(ws)
        .fetch_one(&plane.pool)
        .await
        .expect("count invited seats")
        .get::<i64, _>("n")
    })
}

/// The full workspace ADDRESS a `follow` targets: `<base_url>/<slug>` where the slug is the workspace id
/// with `_`→`-` (`w_acme` → `w-acme`, `w_beacon` → `w-beacon`).
fn address(plane: &Plane, ws_id: &str) -> String {
    format!("{}/{}", plane.link_base_url, ws_id.replace('_', "-"))
}

/// The real `follow <address> --yes`: enroll toward the workspace (card → device flow → confirm identity
/// headless → redeem → promote), then APPLY — the `--yes` reconcile lands the workspace's `everyone`
/// genesis this invocation (JOIN + PLACE). Both workspaces ride the SAME plane, so the second enroll
/// re-authorizes against one base and simply ADDS its membership.
fn follow_and_place(plane: &Plane, client: &FollowHarness, ws_id: &str) {
    let addr = address(plane, ws_id);
    common::begin_address_enroll(plane, client, &addr, INVITEE);
    let applied = client.resume_apply().expect("the resume enrolls + applies");
    assert!(
        applied.enrolled_now,
        "the address enroll seated the device this invocation"
    );
}

// ── the keystone: one client, two workspaces, every verb scoped right ────────────────────────────────

#[test]
fn one_client_follows_two_workspaces_and_every_verb_targets_the_right_one() {
    let plane = start_plane("mws");
    let client = FollowHarness::new("mws");

    // ── Item 1 — follow A by ADDRESS, then follow B into the SAME sidecar (same plane, one base) ──
    follow_and_place(&plane, &client, WS_A);
    assert_eq!(
        client.memberships().len(),
        1,
        "after A: exactly one membership"
    );

    follow_and_place(&plane, &client, WS_B);

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
        .publish(&plane.base_url, &format!("{SB}@{digest}"))
        .expect("publish the sB draft");
    match published {
        PublishResult::Published(d) => assert_eq!(
            d.current_generation,
            Some(Generation { epoch: 1, seq: 2 }),
            "B's current moved +1"
        ),
        other => panic!("expected a direct publish, got {other:?}"),
    }

    // The op-scope proof: B's current advanced, A's did NOT — the op was scoped to sB's OWN workspace.
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

    // `--workspace B` ⇒ the governance op the plane verifies is scoped to B (the seat lands in B, not A).
    let addr = client
        .invite_in_workspace("carol@acme.test", &[SB], WS_B)
        .expect("invite --workspace B seats an invited member");
    assert!(
        addr.contains("w-beacon"),
        "the reshaped invite returns B's workspace ADDRESS (no /i/ link): {addr}"
    );
    assert_eq!(
        invite_count(&plane, WS_B),
        b_before + 1,
        "the invited seat was written in B"
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
    (
        "SKILL.md",
        false,
        b"# common\nShared skill, edited in Acme.\n",
    ),
    ("run.sh", true, b"#!/bin/sh\necho common v2\n"),
];

/// Two workspaces on ONE plane, EACH publishing its own skill under the identical catalog name `"common"`
/// (distinct ids) — the same-name collision across workspaces the disambiguation item resolves.
async fn seed_two_shared_name_workspaces(authority: &Authority) -> Seeded {
    seed_one_workspace(
        authority,
        WsSeed {
            ws: WS_A,
            skill: SHARED_A,
            display: DISPLAY_A,
            owner: OWNER_A,
            owner_dkid: OWNER_DKID_A,
            owner_pubkey: &OWNER_PUBKEY_A,
            owner_credential: OWNER_CRED_A,
            genesis_op: GENESIS_OP_A,
            files: files_a(),
            offered_name: Some(SHARED_NAME),
        },
    )
    .await;
    seed_one_workspace(
        authority,
        WsSeed {
            ws: WS_B,
            skill: SHARED_B,
            display: DISPLAY_B,
            owner: OWNER_B,
            owner_dkid: OWNER_DKID_B,
            owner_pubkey: &OWNER_PUBKEY_B,
            owner_credential: OWNER_CRED_B,
            genesis_op: GENESIS_OP_B,
            files: files_b(),
            offered_name: Some(SHARED_NAME),
        },
    )
    .await;
    Seeded {
        genesis: None,
        invites: Vec::new(),
    }
}

#[test]
fn publish_disambiguates_a_shared_skill_name_by_workspace() {
    let plane = common::start_stack(
        "topos-mws-e2e",
        "mws-shared",
        true,
        seed_two_shared_name_workspaces,
    );
    let client = FollowHarness::new("mws-shared");

    // Follow A and PLACE its "common" skill (unambiguous while B isn't joined yet — so a subsequent publish
    // has real bytes to draft). Then follow B: its identically-named "common" is DECLINED at follow by the
    // reconcile's dirname-collision gate, so only A's "common" ends up locally tracked.
    //
    // CONVERSION-NOTE (address-flow reshape): the classic scenario tracked BOTH workspaces' "common" skills
    // locally (B's a never-received baseline) so a BARE `publish common` was AMBIGUOUS across them. The new
    // model's delivery reconcile never lets two PLAIN same-named skills sit locally — a second same-named
    // cross-workspace delivery is DECLINED (or, with `--prefix-dirname`, renamed to `<ws>.<name>`), so that
    // both-tracked state is no longer reachable through the address follow. The disambiguation's reachable
    // CORE is proven instead: `--workspace <id>` scopes resolution to EXACTLY the named workspace's copy and
    // never bleeds into the other's identically-named skill.
    follow_and_place(&plane, &client, WS_A);
    follow_and_place(&plane, &client, WS_B);

    assert_eq!(client.memberships().len(), 2, "both workspaces joined");
    // Exactly ONE local "common" (A's) — B's identically-named delivery was declined by the collision gate.
    let list = client.list();
    let commons: Vec<&_> = list
        .tracked
        .iter()
        .filter(|e| e.skill == SHARED_NAME)
        .collect();
    assert_eq!(
        commons.len(),
        1,
        "only A's 'common' is locally tracked (B's same-named delivery was declined at follow)"
    );
    assert_eq!(
        commons[0].workspace_id.as_deref(),
        Some(WS_A),
        "the tracked 'common' is A's"
    );

    // Stage a draft on A's (placed) copy so a resolved publish has something to ship.
    client.edit_placement(SHARED_A, SHARED_DRAFT);
    let token = format!("{SHARED_NAME}@{}", client.draft_digest(SHARED_A));

    // `publish <name> --workspace B` ⇒ scoped to B, which holds no local "common" — it must NOT resolve A's
    // identically-named copy (no cross-workspace bleed) and must move NOTHING.
    client
        .publish_in_workspace(&plane.base_url, &token, WS_B)
        .expect_err("--workspace B must not resolve A's 'common' into B");
    assert_eq!(
        current_gen(&plane, WS_A, SHARED_A),
        Some((1, 1)),
        "A's current unmoved by the B-scoped miss"
    );
    assert_eq!(
        current_gen(&plane, WS_B, SHARED_B),
        Some((1, 1)),
        "B's current unmoved by the B-scoped miss"
    );

    // `publish <name> --workspace A` ⇒ resolves to A's copy and ships it; B's identically-named copy is untouched.
    let published = client
        .publish_in_workspace(&plane.base_url, &token, WS_A)
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
