//! HERO on the REAL Claude Code adapter — the distribute loop end to end, on real client verbs.
//!
//! An author (a plain confirmed workspace member) genesis-publishes a brand-new skill over loopback HTTP
//! (the plane stands up the author's first-skill roster row in the same transaction); a teammate on a
//! second "machine" (a fresh client home + a temp stand-in `$CLAUDE_CONFIG_DIR`) redeems an invite with
//! the real two-call `follow` — which arms the REAL `settings.json` SessionStart hook via the genuine
//! Claude Code adapter — and places the first-received bundle. Then the currency loop: the author ships
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

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

mod common;
use std::sync::atomic::{AtomicU32, Ordering};

use ed25519_dalek::{Signer as _, SigningKey};
use plane_store::{
    Authority, ConfirmOutcome, CreateInviteOutcome, DeploymentMode, EnrollmentConfig, GovernanceOp,
    GovernanceSignedOp, Principal, Role, SkillId, WorkspaceId,
};
use topos::test_support::{ContributeHarness, FollowHarness, PublishResult, Scope};
use topos_core::sign::{GovernanceOpFields, GovernanceOpKind, governance_op_preimage};
use topos_plane::{PlaneState, router};
use topos_types::results::PullAction;
use topos_types::{CurrencyKind, Generation, TriggerReport, TriggerState};

// ── shared constants ──────────────────────────────────────────────────────────────────────────────
const WS: &str = "w_acme";
const SKILL: &str = "s_deploy";
/// The workspace admin — an OWNER with a fixed-seed device, who mints the follower invites. Deliberately
/// NOT the author: the genesis standup must work for a plain member.
const ADMIN: &str = "p_admin";
const ADMIN_DKID: &str = "dk_admin";
const ADMIN_SEED: [u8; 32] = [9u8; 32];
/// The author — a plain confirmed MEMBER (not an owner); their device key is the client rig's own.
const AUTHOR: &str = "p_author";
const AUTHOR_RT: &str = "rt_author_secret";
/// The two followers, identified by email (the cloud confirms each at the verification step).
const FOLLOWER1: &str = "dev@acme.test";
const FOLLOWER2: &str = "eve@acme.test";
const AT: &str = "2026-07-01T00:00:00Z";
const NOW: i64 = 1_000_000;
const INVITE_OP_1: &str = "b0000000-0000-4000-8000-000000000001";
const INVITE_OP_2: &str = "b0000000-0000-4000-8000-000000000002";

/// The exact SessionStart hook command the Claude Code adapter installs (duplicated here on purpose — the
/// e2e pins the contract; an adapter change must break this loudly).
const HOOK_COMMAND: &str =
    "command -v topos >/dev/null 2>&1 && topos pull --quiet  # topos:currency";

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

/// The placement a `(path, is_executable, bytes)` bundle must land as: `0o755`/`0o644`, sorted.
fn expected(files: &[(&str, bool, &[u8])]) -> Vec<(String, u32, Vec<u8>)> {
    let mut out: Vec<(String, u32, Vec<u8>)> = files
        .iter()
        .map(|(p, exec, b)| {
            (
                (*p).to_owned(),
                if *exec { 0o755 } else { 0o644 },
                b.to_vec(),
            )
        })
        .collect();
    out.sort();
    out
}

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
const HERMES_ENTRY_LINE: &str = "  - command: topos pull --quiet  # topos:currency";

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
                plugin.contains("topos pull"),
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

// ── the loopback plane ────────────────────────────────────────────────────────────────────────────

/// A self-cleaning temp dir (RAII).
struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("topos-hero-real-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create plane scratch dir");
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Plane {
    rt: tokio::runtime::Runtime,
    authority: Arc<Authority>,
    base_url: String,
    plane_key: [u8; 32],
    invite1: String,
    invite2: String,
    _dir: Scratch,
}

/// Mint an admin-signed `/i/` invite pre-offering the skill to `email` — the same governance frame the
/// plane re-derives + verifies (role byte 3 = Member, `expires_at = 0`).
async fn mint_invite(authority: &Authority, ws: &WorkspaceId, op_id: &str, email: &str) -> String {
    let hyphenless: String = op_id.chars().filter(|c| *c != '-').collect();
    let mut op_id_bytes = [0u8; 16];
    hex::decode_to_slice(&hyphenless, &mut op_id_bytes).expect("op_id is 16 hex bytes");

    let fields = GovernanceOpFields {
        workspace_id: WS,
        op_id: op_id_bytes,
        device_key_id: ADMIN_DKID,
        op: GovernanceOpKind::Invite {
            role: 3, // Member
            expires_at: 0,
            emails: &[email],
            skills: &[SKILL],
        },
    };
    let preimage = governance_op_preimage(&fields).expect("governance preimage");
    let signature = SigningKey::from_bytes(&ADMIN_SEED)
        .sign(&preimage)
        .to_bytes();
    let signed = GovernanceSignedOp {
        device_key_id: ADMIN_DKID.to_owned(),
        op: GovernanceOp::Invite {
            role: Role::Member,
            expires_at: None,
            emails: vec![Principal::parse(email).unwrap()],
            skills: vec![(SkillId::parse(SKILL).unwrap(), None)],
        },
        signature,
    };
    match authority
        .create_invite(ws, op_id, signed, AT)
        .await
        .expect("create_invite")
    {
        CreateInviteOutcome::Created(invite) => invite.link,
        CreateInviteOutcome::Denied(reason) => panic!("invite denied: {reason}"),
    }
}

/// Stand the plane up with a workspace, the admin owner, two invited followers, and their invites —
/// deliberately NO skill roster and NO published genesis: the author's first client publish creates both.
fn start_plane(tag: &str, author_device: (&str, [u8; 32])) -> Plane {
    let dir = Scratch::new(tag);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let listener = rt
        .block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await })
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("local addr");
    let base_url = format!("http://{addr}");

    let (author_dkid, author_pubkey) = author_device;
    let author_dkid = author_dkid.to_owned();
    let (authority, invite1, invite2, plane_key) = rt.block_on(async {
        let authority = Authority::from_pool(
            common::provision_pg().await,
            &dir.0.join("git"),
            &dir.0.join("large"),
        )
        .expect("open authority")
        .with_plane_key(&dir.0.join("plane.key"))
        .expect("load plane key")
        .with_enrollment_config(EnrollmentConfig {
            secret_path: dir.0.join("enroll.key"),
            base_url: base_url.clone(),
            deployment_mode: DeploymentMode::Cloud,
            enrollment_method: "device_code".to_owned(),
        })
        .expect("load enrollment secret");

        let ws = WorkspaceId::parse(WS).unwrap();
        let skill = SkillId::parse(SKILL).unwrap();
        let admin = Principal::parse(ADMIN).unwrap();
        let author = Principal::parse(AUTHOR).unwrap();

        authority
            .seed_workspace(&ws, "Acme", "verified", "cloud")
            .await
            .expect("seed workspace");
        // The admin owner (governance authority — mints the invites).
        authority
            .seed_workspace_member(&ws, &admin, "owner", "confirmed")
            .await
            .expect("seed admin");
        let admin_pk = SigningKey::from_bytes(&ADMIN_SEED)
            .verifying_key()
            .to_bytes();
        authority
            .seed_device(&ws, ADMIN_DKID, &admin_pk, &admin, false)
            .await
            .expect("seed admin device");

        // The author: a plain confirmed MEMBER with a registered device — and deliberately NO per-skill
        // roster row and NO published genesis. Their first publish must stand both up.
        authority
            .seed_workspace_member(&ws, &author, "member", "confirmed")
            .await
            .expect("seed author member");
        authority
            .seed_device(&ws, &author_dkid, &author_pubkey, &author, false)
            .await
            .expect("seed author device");
        // The author's read credential (usable once the skill exists — reads are rostered ∧ reachable).
        authority
            .mint_read_token(&ws, &skill, &author, AUTHOR_RT)
            .await
            .expect("mint author read token");

        // The followers are invited members; each confirms their email at the verification step.
        for email in [FOLLOWER1, FOLLOWER2] {
            authority
                .seed_workspace_member(&ws, &Principal::parse(email).unwrap(), "member", "invited")
                .await
                .expect("pre-roster follower");
        }
        let invite1 = mint_invite(&authority, &ws, INVITE_OP_1, FOLLOWER1).await;
        let invite2 = mint_invite(&authority, &ws, INVITE_OP_2, FOLLOWER2).await;
        let plane_key = authority.plane_public_key().expect("plane public key");
        (authority, invite1, invite2, plane_key)
    });

    let authority = Arc::new(authority);
    let state = PlaneState::new(authority.clone());
    rt.spawn(async move {
        let _ = axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    Plane {
        rt,
        authority,
        base_url,
        plane_key,
        invite1,
        invite2,
        _dir: dir,
    }
}

/// Enroll a follower through the real two-call `follow` (headless: the identity confirm is driven through
/// the authority), then assert the adapter armed currency and place the offered version.
fn enroll_follower(
    plane: &Plane,
    case: &AdapterCase,
    tag: &str,
    invite: &str,
    email: &str,
    manual: bool,
) -> FollowHarness {
    let follower = (case.follower)(tag);
    let pending = follower
        .follow_with(invite, plane.plane_key, manual)
        .expect("follow call 1");
    let user_code = pending
        .pending
        .as_ref()
        .expect("pending verification handle")
        .user_code
        .clone();
    let confirm = plane
        .rt
        .block_on(
            plane
                .authority
                .confirm_external_identity(&user_code, email, NOW),
        )
        .expect("confirm identity");
    assert!(matches!(confirm, ConfirmOutcome::Confirmed));
    let done = follower.resume(plane.plane_key).expect("follow --resume");
    assert!(done.enrolled);
    // The promote armed the REAL adapter's currency trigger and disclosed it — assert both the config
    // bytes and the report's honesty per adapter.
    let currency = done
        .currency
        .as_ref()
        .expect("the enrollment must disclose the currency-arm outcome");
    (case.assert_currency)(&follower, currency);
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
    author.enroll(
        &plane.base_url,
        plane.plane_key,
        WS,
        SKILL,
        AUTHOR_RT,
        false,
        V1,
    );
    let published = author
        .publish(
            plane.plane_key,
            false,
            &format!("{SKILL}@{}", author.draft_digest()),
        )
        .expect("the genesis publish must succeed for a confirmed member (the roster standup)");
    let PublishResult::Published(genesis) = published else {
        panic!("a direct publish moves current, never opens a proposal");
    };
    assert_eq!(genesis.current_generation, Generation { epoch: 1, seq: 1 });
    assert!(
        genesis.invite_link.is_none(),
        "the genesis invite fold-in is owner-gated; a plain member publishes without one"
    );
    let genesis_id = genesis.version_id.clone();

    // ── 2 · Machine B: a pure follower enrolls via the real `follow`; the hook arms; genesis lands. ──
    let follower = enroll_follower(
        &plane,
        case,
        &format!("hero-{}-f1", case.tag),
        &plane.invite1,
        FOLLOWER1,
        false,
    );
    follower
        .approve(
            &plane.base_url,
            plane.plane_key,
            &[format!("{SKILL}@{genesis_id}")],
        )
        .expect("first-receive approve");
    assert_eq!(
        follower.placement_files(SKILL),
        expected(V1),
        "the genesis bundle lands byte-exact (incl. the exec bit) in the adapter's skills dir"
    );

    // ── 3 · The drafting follower (confirm-each) receives genesis, then edits a local draft. ──
    let drafter = enroll_follower(
        &plane,
        case,
        &format!("hero-{}-f2", case.tag),
        &plane.invite2,
        FOLLOWER2,
        true,
    );
    drafter
        .approve(
            &plane.base_url,
            plane.plane_key,
            &[format!("{SKILL}@{genesis_id}")],
        )
        .expect("drafter first-receive approve");
    drafter.edit_placement(SKILL, DRAFT);

    // ── 4 · The author ships an update; the follower's next bare sweep self-updates byte-exact. ──
    author.edit_placement(V2);
    let updated = author
        .publish(
            plane.plane_key,
            false,
            &format!("{SKILL}@{}", author.draft_digest()),
        )
        .expect("the v2 publish");
    let PublishResult::Published(v2) = updated else {
        panic!("a direct publish moves current");
    };
    assert_eq!(v2.current_generation, Generation { epoch: 1, seq: 2 });

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
        .revert(
            plane.plane_key,
            &genesis_id,
            &format!("{SKILL}@{genesis_id}"),
            false,
        )
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
        "the drafting follower's floor still advances on the signed revert record"
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
