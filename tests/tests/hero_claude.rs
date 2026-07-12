//! HERO on the REAL Claude Code adapter — the distribute loop end to end, on real client verbs.
//!
//! An author (a plain confirmed workspace member) genesis-publishes a brand-new skill over loopback HTTP
//! (the plane stands up the author's first-skill roster row in the same transaction); a teammate on a
//! second "machine" (a fresh client home + a temp stand-in `$CLAUDE_CONFIG_DIR`) JOINS BY THE WORKSPACE
//! ADDRESS with the real `follow` (card → device flow → confirm → redeem → `--yes`) — which arms the REAL
//! `settings.json` SessionStart hook via the genuine Claude Code adapter — and the `--yes` reconcile lands
//! `everyone`'s genesis bundle byte-exact. Then the currency loop: the author ships
//! an update, the follower's next bare `pull` sweep (what the installed hook runs) fast-forwards the
//! placement byte-exact; the author `revert --to <good>` (the real client verb — a FORWARD pointer move),
//! and the next sweep restores the good bytes. A second, drafting follower (confirm-each, local edits) is
//! never clobbered by any of it — each sweep surfaces the change as a `diverged` row and leaves the draft
//! bytes untouched.
//!
//! The runner is table-driven over [`AdapterCase`] — the OpenClaw and Hermes cases below are exactly
//! that: one new case row + one `#[test]` each, not a copy. Only Claude Code guarantees the swap
//! completes before skills resolve; a sibling case asserts byte-landing, not that ordering.
//!
//! MUST-VERIFY (manual, not asserted here): that a live Claude Code session's SessionStart hook injects
//! `topos pull --quiet`'s stdout into the model context BEFORE skill resolution. This e2e proves the hook
//! command is installed byte-exact, the enrollment armed it through the real adapter, and the updated /
//! reverted bytes materialize into the config home's `skills/<id>/` — it does NOT and cannot assert that
//! a live Claude model saw the update.
//!
//! For OpenClaw the currency is honestly weaker: it surfaces on the first `topos` touch of a session,
//! never at bare session open (`session_start` is observer-only, and cron is never a currency path). The
//! OpenClaw case proves the `openclaw.json` bootstrap-inject registration + the topos-owned plugin file
//! are written through the REAL adapter's `follow` promote and that updates/reverts land byte-exact on
//! the follower's `pull` sweeps over a temp stand-in home. It does NOT and cannot assert that a live
//! OpenClaw gateway auto-watched the config and injected the refreshed surface at bootstrap — that, plus
//! the concrete config-byte shape (a readiness probe against the pilot's exact OpenClaw build), is an
//! external MUST-VERIFY, never a headless assertion.
//!
//! The Hermes case is the same honest shape one notch further: its per-turn `pre_llm_call` hook runs only
//! after Hermes's own one-time `(event, command)` approval, and no acceptance evidence exists in a fixture
//! home — so the case asserts the real `config.yaml` gained the exact registered entry (and never an
//! `on_session_start` one) AND that the disclosed report is honestly NOT active (the explicit-pull
//! degrade), never fabricating the live per-turn injection the pilot's real build must be the one to
//! prove.

mod common;

use common::{NOW, Plane, WS, expected, ws_address};
use plane_store::{Authority, ConfirmOutcome, Principal, WorkspaceId};
use topos::test_support::{ContributeHarness, Follow, FollowHarness, PublishResult, Scope};
use topos_types::results::PullAction;
use topos_types::{CurrencyKind, Generation, TriggerReport, TriggerState};

// ── shared constants ──────────────────────────────────────────────────────────────────────────────
/// The workspace owner — an OWNER with a fixed-key device (the workspace's governance root). Deliberately
/// NOT the author: the genesis standup must work for a plain member. Joining is now by ADDRESS, so the
/// owner mints no invite links — the followers hold pre-seeded invited seats, and the address is the door.
const ADMIN: &str = "p_admin";
const ADMIN_DKID: &str = "dk_admin";
/// The admin device's registered 32-byte public key (a fixed test value; nothing verifies against it).
const ADMIN_PUBKEY: [u8; 32] = [9u8; 32];
/// The admin owner's workspace Bearer credential — the acting credential the plane resolves to mint invites.
const ADMIN_CRED: &str = "wc_admin_secret";
/// The author — a plain confirmed MEMBER (not an owner); their device key is the client rig's own.
const AUTHOR: &str = "p_author";
/// The author's workspace Bearer credential — authenticates the author's genesis publish (which stands up
/// the skill roster row in the same transaction).
const AUTHOR_CRED: &str = "wc_author_secret";
/// The two followers, identified by email (the cloud confirms each at the verification step).
const FOLLOWER1: &str = "dev@acme.test";
const FOLLOWER2: &str = "eve@acme.test";

/// This suite's skill id — SLUG-CLEAN (no underscore), shadowing the shared `common::SKILL`. The plane
/// mints the catalog NAME as the slug of the published folder name (`_` → `-`), and the REAL Claude Code
/// adapter places a delivered skill under that catalog name while the WorkHarness stub places by id — a
/// slug-clean id makes id == name, so `placement_files(SKILL)` reads the same directory every adapter
/// writes.
const SKILL: &str = "s-deploy";

/// The exact SessionStart hook command the Claude Code adapter installs (duplicated here on purpose — the
/// e2e pins the contract; an adapter change must break this loudly).
const HOOK_COMMAND: &str =
    "command -v topos >/dev/null 2>&1 && topos update --quiet || true  # topos:currency";

/// The OpenClaw adapter's config artifacts (duplicated here on purpose, like `HOOK_COMMAND` — the e2e
/// pins the contract). Provisional until the readiness probe against the pilot's exact OpenClaw build.
const OPENCLAW_EXTRA_FILES_KEY: &str = "bootstrap-extra-files";
const OPENCLAW_PLUGIN_FILE: &str = "topos-currency.mjs";

/// v1 — the genesis bundle the author first publishes (a doc + an EXECUTABLE script).
const V1: &[(&str, bool, &[u8])] = &[
    ("SKILL.md", false, b"# deploy\nDeploy the service.\n"),
    ("run.sh", true, b"#!/bin/sh\necho deploying\n"),
];
/// v2 — the author's update (the "bad" version the team later reverts).
const V2: &[(&str, bool, &[u8])] = &[
    ("SKILL.md", false, b"# deploy v2\nDeploy faster.\n"),
    ("run.sh", true, b"#!/bin/sh\necho deploying v2\n"),
];
/// The drafting follower's local edit — ahead of every published version, never to be clobbered.
const DRAFT: &[(&str, bool, &[u8])] = &[
    ("SKILL.md", false, b"# deploy (my local notes)\n"),
    ("run.sh", true, b"#!/bin/sh\necho deploying\n"),
];

// ── the adapter-parametrized scaffold ─────────────────────────────────────────────────────────────

/// One harness adapter's hero case. A sibling adapter increment adds a constructor on `FollowHarness`,
/// one `AdapterCase` row, and one `#[test]` — the runner does not change.
struct AdapterCase {
    tag: &'static str,
    /// Build a follower rig wired to this adapter over an isolated temp config home.
    follower: fn(&str) -> FollowHarness,
    /// Assert the adapter's own config gained the currency trigger byte-exact AND that the disclosed
    /// [`TriggerReport`] is honest for this adapter (a fixture home must never fabricate a live hook).
    assert_currency: fn(&FollowHarness, &TriggerReport),
}

fn claude_case() -> AdapterCase {
    AdapterCase {
        tag: "claude",
        follower: FollowHarness::new_claude,
        assert_currency: |h, report| {
            let raw = h
                .settings_json()
                .expect("promote wrote settings.json into the temp config home");
            let v: serde_json::Value =
                serde_json::from_str(&raw).expect("settings.json is valid JSON");
            let group = &v["hooks"]["SessionStart"][0];
            assert_eq!(group["matcher"].as_str(), Some("startup"));
            assert_eq!(
                group["hooks"][0]["command"].as_str(),
                Some(HOOK_COMMAND),
                "the installed hook command must be the exact literal the harness will exec"
            );
            assert!(
                group["hooks"][0]["timeout"].is_u64(),
                "the managed hook carries a timeout"
            );
            // Claude Code's hook needs no separate approval: a fresh install into the temp home is live.
            assert_eq!(report.state, TriggerState::Active);
            assert_eq!(report.currency_kind, CurrencyKind::SessionStart);
        },
    }
}

/// The exact `pre_llm_call` entry line the Hermes adapter registers (duplicated here on purpose — the e2e
/// pins the contract; an adapter change must break this loudly). The trailing `# topos:currency` is a YAML
/// comment outside the scalar: Hermes parses the command as exactly `topos pull --quiet`.
const HERMES_ENTRY_LINE: &str = "  - command: topos update --quiet  # topos:currency";

fn hermes_case() -> AdapterCase {
    AdapterCase {
        tag: "hermes",
        follower: FollowHarness::new_hermes,
        assert_currency: |h, report| {
            let raw = h
                .hermes_config()
                .expect("promote wrote config.yaml into the temp hermes home");
            assert!(
                raw.lines().any(|l| l == HERMES_ENTRY_LINE),
                "the registered entry must be the exact line the adapter contracts: {raw:?}"
            );
            assert!(
                raw.contains("pre_llm_call"),
                "the registered event is the injecting per-turn hook"
            );
            assert!(
                !raw.contains("session_start"),
                "on_session_start is observer-only and never the currency mechanism"
            );
            // Honest, fixture-home-based form: no acceptance evidence exists here, so the report must
            // NOT claim the hook is live — the per-turn injection on the pilot's real build stays a
            // MUST-VERIFY, and until then currency degrades plainly to explicit pull.
            assert_eq!(report.state, TriggerState::Inactive);
            assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
        },
    }
}

fn openclaw_case() -> AdapterCase {
    AdapterCase {
        tag: "openclaw",
        follower: FollowHarness::new_openclaw,
        assert_currency: |h, report| {
            let home = h.openclaw_home().expect("the rig is in openclaw mode");
            let raw = h
                .openclaw_config_json()
                .expect("promote wrote openclaw.json into the temp stand-in home");
            let v: serde_json::Value =
                serde_json::from_str(&raw).expect("openclaw.json is valid JSON");
            let entries = v[OPENCLAW_EXTRA_FILES_KEY]
                .as_array()
                .expect("the bootstrap-inject registration array exists");
            let expected = home.join(OPENCLAW_PLUGIN_FILE);
            assert_eq!(
                entries
                    .iter()
                    .filter_map(|e| e.as_str())
                    .collect::<Vec<_>>(),
                vec![expected.to_str().unwrap()],
                "the registration is exactly the topos-owned plugin path"
            );
            let plugin = h
                .openclaw_plugin()
                .expect("promote wrote the topos-owned inject plugin file");
            assert!(
                plugin.contains("topos update"),
                "the inject surface names the currency verb"
            );
            assert!(
                plugin.contains("first `topos` touch"),
                "honest label: the update moment is the first `topos` touch"
            );
            assert!(
                !plugin.to_lowercase().contains("session start"),
                "the inject surface never claims session-start currency"
            );
            // A fresh temp home has no disabling flag and no blown budget, so the registration itself is
            // live — honestly labeled first-`topos`-touch (the live gateway auto-watch stays the external
            // MUST-VERIFY above, never asserted here).
            assert_eq!(report.state, TriggerState::Active);
            assert_eq!(report.currency_kind, CurrencyKind::FirstToposTouch);
        },
    }
}

// ── the loopback plane (the shared harness + this suite's scenario seeding) ─────────────────────────

/// Stand the plane up via the shared harness (bind-first, enrollment-configured) with a workspace, the
/// owner, and two invited followers (seated, no invite link — the address is the door) — deliberately NO
/// skill roster and NO published genesis: the author's first client publish creates both.
fn start_plane(tag: &str, author_device: (&str, [u8; 32])) -> Plane {
    let (author_dkid, author_pubkey) = author_device;
    let author_dkid = author_dkid.to_owned();
    common::start_plane(
        "topos-hero-real",
        tag,
        true,
        async |authority: &Authority| {
            let ws = WorkspaceId::parse(WS).unwrap();
            let admin = Principal::parse(ADMIN).unwrap();
            let author = Principal::parse(AUTHOR).unwrap();

            authority
                .seed_workspace(&ws, "Acme", "verified", "cloud")
                .await
                .expect("seed workspace");
            // The admin owner (governance authority — mints the invites) holding ADMIN_CRED.
            authority
                .seed_workspace_member(&ws, &admin, "owner", "confirmed")
                .await
                .expect("seed admin");
            authority
                .seed_device(&ws, ADMIN_DKID, &ADMIN_PUBKEY, &admin, false, ADMIN_CRED)
                .await
                .expect("seed admin device");

            // The author: a plain confirmed MEMBER holding AUTHOR_CRED — and deliberately NO per-skill roster
            // row and NO published genesis. Their first publish (authenticated by AUTHOR_CRED → confirmed
            // member) must stand both up. Their device key is the client rig's own.
            authority
                .seed_workspace_member(&ws, &author, "member", "confirmed")
                .await
                .expect("seed author member");
            authority
                .seed_device(
                    &ws,
                    &author_dkid,
                    &author_pubkey,
                    &author,
                    false,
                    AUTHOR_CRED,
                )
                .await
                .expect("seed author device");

            // The followers are invited members — an invited seat is all a join-by-address needs (the
            // roster is the lock; the redeem flips invited → confirmed). No invite link is minted.
            for email in [FOLLOWER1, FOLLOWER2] {
                authority
                    .seed_workspace_member(
                        &ws,
                        &Principal::parse(email).unwrap(),
                        "member",
                        "invited",
                    )
                    .await
                    .expect("pre-roster follower");
            }
            common::Seeded {
                genesis: None,
                invites: Vec::new(),
            }
        },
    )
}

/// Enroll a follower through the real `follow <address>` (headless: the identity confirm is driven through
/// the authority), landing `everyone`'s genesis via `--yes`, then assert the adapter armed currency.
fn enroll_follower(
    plane: &Plane,
    case: &AdapterCase,
    tag: &str,
    address: &str,
    email: &str,
    manual: bool,
) -> FollowHarness {
    let follower = (case.follower)(tag);
    // Call 1 — `topos follow <address>` in the case's mode (confirm-each for the drafter): fetch the card,
    // re-root, mint the device key, device-authorize toward the address name, write the pending WAL.
    let pending = follower
        .follow_with(address, manual)
        .expect("follow call 1 (address)");
    assert!(!pending.enrolled, "call 1 only begins enrollment");
    let user_code = pending
        .pending
        .as_ref()
        .expect("pending verification handle")
        .user_code
        .clone();
    // The human identity leg, in-process (the authority's external-confirm op — the flow is headless).
    let confirm = plane
        .rt
        .block_on(
            plane
                .authority
                .confirm_external_identity(&user_code, email, NOW),
        )
        .expect("confirm identity");
    assert!(matches!(confirm, ConfirmOutcome::Confirmed));
    // Resume WITH `--yes`: poll → redeem → promote (arming the REAL adapter's currency trigger) then apply —
    // the reconcile batch-accepts `everyone`'s genesis first-receive in this same invocation.
    let applied = follower
        .resume_apply()
        .expect("the resume enrolls + applies");
    assert!(applied.enrolled_now, "THIS invocation enrolled the device");
    assert_eq!(
        applied.installed.len(),
        1,
        "the everyone genesis landed: {:?}",
        applied.installed
    );
    assert_eq!(applied.installed[0].skill_id, SKILL);
    // CONVERSION-NOTE (the confirm-each drafter): call 1 recorded the `--manual` intent in the WAL, but
    // the reconcile's first-receive install currently records `Auto` in `follows.json` (threading the WAL
    // mode into the install is the client's later work). Re-record the human's declared intent through the
    // doc-level bridge, so every subsequent sweep runs the engine's GENUINE confirm-each contract (the
    // never-clobber assertions below stay real engine behavior, not a fixture).
    if manual {
        follower.set_follow_mode(SKILL, Follow::ConfirmEach);
    }
    // The promote armed the adapter's currency trigger; re-probe it (the address flow's apply does not carry
    // the classic `FollowData.currency`) and assert both the config bytes and the report's honesty per adapter.
    let report = follower.currency_report();
    (case.assert_currency)(&follower, &report);
    follower
}

// ── the hero runner ───────────────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
fn run_distribute_hero(case: &AdapterCase) {
    // The author client mints its device key first, so the plane can register exactly that key.
    let author = ContributeHarness::new(&format!("hero-{}-author", case.tag));
    let author_dkid = author.device_key_id();
    let plane = start_plane(case.tag, (&author_dkid, author.device_pubkey()));

    // ── 1 · The author's FIRST publish of a brand-new skill: the genesis standup over the wire. ──
    let mut author = author;
    author.enroll(&plane.base_url, WS, SKILL, AUTHOR_CRED, false, V1);
    let published = author
        .publish(false, &format!("{SKILL}@{}", author.draft_digest()))
        .expect("the genesis publish must succeed for a confirmed member (the roster standup)");
    let PublishResult::Published(genesis) = published else {
        panic!("a direct publish moves current, never opens a proposal");
    };
    assert_eq!(
        genesis.current_generation,
        Some(Generation { epoch: 1, seq: 1 })
    );
    assert!(
        genesis.share_line.is_none(),
        "an ordinary enrolled publish carries no share line (only a workspace-creating standup does); the \
         address is the share surface — a plain member publishes fine"
    );
    let genesis_id = genesis
        .version_id
        .clone()
        .expect("a published receipt names its version");

    // The workspace ADDRESS both followers join by (w_acme slugifies to w-acme).
    let address = ws_address(&plane.base_url);

    // ── 2 · Machine B: a pure follower JOINS BY ADDRESS via the real `follow`; the hook arms; `--yes`
    //        lands `everyone`'s genesis in one invocation (no separate approve). ──
    let follower = enroll_follower(
        &plane,
        case,
        &format!("hero-{}-f1", case.tag),
        &address,
        FOLLOWER1,
        false,
    );
    assert_eq!(
        follower.placement_files(SKILL),
        expected(V1),
        "the genesis bundle lands byte-exact (incl. the exec bit) in the adapter's skills dir"
    );

    // ── 3 · The drafting follower (confirm-each) joins + receives genesis via `--yes`, then edits a draft. ──
    let drafter = enroll_follower(
        &plane,
        case,
        &format!("hero-{}-f2", case.tag),
        &address,
        FOLLOWER2,
        true,
    );
    drafter.edit_placement(SKILL, DRAFT);

    // ── 4 · The author ships an update; the follower's next bare sweep self-updates byte-exact. ──
    author.edit_placement(V2);
    let updated = author
        .publish(false, &format!("{SKILL}@{}", author.draft_digest()))
        .expect("the v2 publish");
    let PublishResult::Published(v2) = updated else {
        panic!("a direct publish moves current");
    };
    assert_eq!(v2.current_generation, Some(Generation { epoch: 1, seq: 2 }));

    let sweep = follower.pull(Scope::AllFollowed);
    assert_eq!(sweep.skills.len(), 1);
    assert_eq!(
        sweep.skills[0].action,
        PullAction::FastForwarded,
        "an auto follower's session-start sweep applies the update with no prompt"
    );
    assert_eq!(
        follower.placement_files(SKILL),
        expected(V2),
        "the update lands byte-exact on the second machine"
    );
    assert_eq!(
        follower.sync_state(SKILL).observed,
        Generation { epoch: 1, seq: 2 }
    );

    // The drafting follower is surfaced, never clobbered.
    let drafter_sweep = drafter.pull(Scope::AllFollowed);
    assert_eq!(
        drafter_sweep.skills[0].action,
        PullAction::Diverged,
        "a drafting follower sees the change as a diverged row on the normal poll"
    );
    assert_eq!(
        drafter.placement_files(SKILL),
        expected(DRAFT),
        "a local draft is never clobbered by a sweep"
    );

    // ── 5 · The team revert: the REAL client verb — a FORWARD move restoring the good bytes. ──
    let reverted = author
        .revert(&genesis_id, &format!("{SKILL}@{genesis_id}"), false)
        .expect("revert --to <good>");
    assert_eq!(
        reverted.current_generation,
        Generation { epoch: 1, seq: 3 },
        "a revert moves the pointer FORWARD (a new higher seq), never backward"
    );

    let sweep = follower.pull(Scope::AllFollowed);
    assert_eq!(
        sweep.skills[0].action,
        PullAction::FastForwarded,
        "the rollback rides the next normal poll"
    );
    assert_eq!(
        follower.placement_files(SKILL),
        expected(V1),
        "the good bytes are restored byte-exact on the following machine"
    );
    assert_eq!(
        follower.sync_state(SKILL).observed,
        Generation { epoch: 1, seq: 3 }
    );

    // The drafting follower still gets the rollback line — a local draft can't suppress it — and keeps
    // its bytes.
    let drafter_sweep = drafter.pull(Scope::AllFollowed);
    assert_eq!(drafter_sweep.skills[0].action, PullAction::Diverged);
    assert_eq!(
        drafter.sync_state(SKILL).observed,
        Generation { epoch: 1, seq: 3 },
        "the drafting follower's floor still advances on the revert record"
    );
    assert_eq!(drafter.placement_files(SKILL), expected(DRAFT));
}

#[test]
fn distribute_hero_on_the_real_claude_code_adapter() {
    run_distribute_hero(&claude_case());
}

#[test]
fn distribute_hero_on_the_real_openclaw_adapter() {
    run_distribute_hero(&openclaw_case());
}

#[test]
fn distribute_hero_on_the_real_hermes_adapter() {
    run_distribute_hero(&hermes_case());
}
