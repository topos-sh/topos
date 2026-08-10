//! `xtask` — the one codegen + invariant-gate entrypoint.
//!
//! `cargo xtask gen-schema`          → (re)generate `contracts/schemas/*.schema.json` from topos-types.
//! `cargo xtask gen-schema --check`  → the CI drift gate (stale / missing / orphan schemas all fail).
//! `cargo xtask gen-fixtures`        → (re)generate the golden `--json` fixtures under `contracts/fixtures/`.
//! `cargo xtask gen-fixtures --check`→ the fixture drift gate.
//! `cargo xtask gen-cli-ref`         → (re)generate the CLI reference `docs/cli.md` from the real clap tree.
//! `cargo xtask gen-cli-ref --check` → the CLI-reference drift gate.
//! `cargo xtask check-arch`          → the architectural-layering + lint-opt-in + toolchain-pin gate.
//! `cargo xtask check-registry-drift`→ OPT-IN + advisory: fetch upstream `agents.ts` and diff the baked
//!                                     harness registry against it (network; NEVER in `ci`/CI).
//! `cargo xtask ci`                  → the full non-DB gate sequence, in CI's order (fmt, clippy, doc,
//!                                     the drift gates, check-arch) — the contributor's pre-push loop.
//! `cargo xtask conformance`         → the store matrices (not yet implemented).
//! `cargo xtask dist …`              → offline release packaging (deterministic tarball + SHA256SUMS) — see `dist.rs`.
//!
//! `gen-schema` also (re)generates + checks the plane OpenAPI (`contracts/openapi/openapi.json`, from
//! `topos_plane::openapi()`) under the same drift discipline. There is no formal-model subcommand — the
//! integration interleaving tests are the correctness net. (The `cargo xtask` alias lives in the committed
//! `.cargo/config.toml`; `cargo run -p xtask --` works identically.)

use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeSet,
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

mod dist;
mod registry_drift;

/// The committed JSON-Schema artifacts (the per-loop contract oracle). One entry per top-level wire type.
fn schemas() -> Vec<(&'static str, String)> {
    vec![
        (
            "json-envelope",
            emit(schemars::schema_for!(topos_types::JsonEnvelope)),
        ),
        ("receipt", emit(schemars::schema_for!(topos_types::Receipt))),
        (
            "wire-error",
            emit(schemars::schema_for!(topos_types::WireError)),
        ),
        (
            "wire-current-record",
            emit(schemars::schema_for!(topos_types::WireCurrentRecord)),
        ),
        (
            "next-action",
            emit(schemars::schema_for!(topos_types::NextAction)),
        ),
        (
            "trigger-report",
            emit(schemars::schema_for!(topos_types::TriggerReport)),
        ),
        // The per-device delivery-lane wire bodies (the delivery read + the applied-state report).
        (
            "wire-delivery",
            emit(schemars::schema_for!(topos_types::requests::WireDelivery)),
        ),
        (
            "wire-applied-report",
            emit(schemars::schema_for!(
                topos_types::requests::WireAppliedReport
            )),
        ),
        // The adopted member-lane wire bodies: the constant protocol card (the unmatched-path machine
        // face), the login-redeem answer, and the member-scoped describe reads + row-op request bodies the
        // two-phase verbs run over.
        (
            "wire-protocol-card",
            emit(schemars::schema_for!(
                topos_types::requests::WireProtocolCard
            )),
        ),
        (
            "wire-me",
            emit(schemars::schema_for!(topos_types::requests::WireMe)),
        ),
        (
            "wire-channel-index",
            emit(schemars::schema_for!(
                topos_types::requests::WireChannelIndex
            )),
        ),
        (
            "wire-proposal-index",
            emit(schemars::schema_for!(
                topos_types::requests::WireProposalIndex
            )),
        ),
        (
            "wire-skill-log",
            emit(schemars::schema_for!(topos_types::requests::WireSkillLog)),
        ),
        // The gh-style device-auth flow (the app serves it; the CLI speaks it).
        (
            "device-auth-start-request",
            emit(schemars::schema_for!(
                topos_types::requests::DeviceAuthStartRequest
            )),
        ),
        (
            "device-auth-start-response",
            emit(schemars::schema_for!(
                topos_types::requests::DeviceAuthStartResponse
            )),
        ),
        (
            "device-auth-poll-request",
            emit(schemars::schema_for!(
                topos_types::requests::DeviceAuthPollRequest
            )),
        ),
        (
            "device-auth-poll-response",
            emit(schemars::schema_for!(
                topos_types::requests::DeviceAuthPollResponse
            )),
        ),
        (
            "login-connect-request",
            emit(schemars::schema_for!(
                topos_types::requests::LoginConnectRequest
            )),
        ),
        (
            "login-connect-response",
            emit(schemars::schema_for!(
                topos_types::requests::LoginConnectResponse
            )),
        ),
        (
            "notice-ack-request",
            emit(schemars::schema_for!(
                topos_types::requests::NoticeAckRequest
            )),
        ),
        (
            "protection-set-request",
            emit(schemars::schema_for!(
                topos_types::requests::ProtectionSetRequest
            )),
        ),
        // Per-verb `--json` `data` payloads — one schema each.
        (
            "pull-data",
            emit(schemars::schema_for!(topos_types::results::PullData)),
        ),
        (
            "list-data",
            emit(schemars::schema_for!(topos_types::results::ListData)),
        ),
        (
            "diff-data",
            emit(schemars::schema_for!(topos_types::results::DiffData)),
        ),
        (
            "add-data",
            emit(schemars::schema_for!(topos_types::results::AddData)),
        ),
        (
            "add-describe-data",
            emit(schemars::schema_for!(topos_types::results::AddDescribeData)),
        ),
        (
            "init-data",
            emit(schemars::schema_for!(topos_types::results::InitData)),
        ),
        (
            "fmt-data",
            emit(schemars::schema_for!(topos_types::results::FmtData)),
        ),
        (
            "log-data",
            emit(schemars::schema_for!(topos_types::results::LogData)),
        ),
        (
            "publish-data",
            emit(schemars::schema_for!(topos_types::results::PublishData)),
        ),
        (
            "propose-data",
            emit(schemars::schema_for!(topos_types::results::ProposeData)),
        ),
        (
            "revert-data",
            emit(schemars::schema_for!(topos_types::results::RevertData)),
        ),
        (
            "revert-describe-data",
            emit(schemars::schema_for!(
                topos_types::results::RevertDescribeData
            )),
        ),
        (
            "review-data",
            emit(schemars::schema_for!(topos_types::results::ReviewData)),
        ),
        (
            "invitation-data",
            emit(schemars::schema_for!(topos_types::requests::InvitationData)),
        ),
        // The adopted verb describe/apply `data` payloads (the two-phase surface: a bare mutating verb
        // returns the describe, `--yes` returns it applied).
        (
            "remove-data",
            emit(schemars::schema_for!(topos_types::results::RemoveData)),
        ),
        (
            "protect-data",
            emit(schemars::schema_for!(topos_types::results::ProtectData)),
        ),
        (
            "review-index-data",
            emit(schemars::schema_for!(topos_types::results::ReviewIndexData)),
        ),
        (
            "review-describe-data",
            emit(schemars::schema_for!(
                topos_types::results::ReviewDescribeData
            )),
        ),
        (
            "invite-read-data",
            emit(schemars::schema_for!(topos_types::results::InviteReadData)),
        ),
        (
            "invite-describe-data",
            emit(schemars::schema_for!(
                topos_types::results::InviteDescribeData
            )),
        ),
        (
            "login-data",
            emit(schemars::schema_for!(topos_types::results::LoginData)),
        ),
        (
            "logout-data",
            emit(schemars::schema_for!(topos_types::results::LogoutData)),
        ),
        (
            "reset-data",
            emit(schemars::schema_for!(topos_types::results::ResetData)),
        ),
        (
            "publish-describe-data",
            emit(schemars::schema_for!(
                topos_types::results::PublishDescribeData
            )),
        ),
        (
            "keep-as-yours-data",
            emit(schemars::schema_for!(topos_types::results::KeepAsYoursData)),
        ),
        (
            "status-data",
            emit(schemars::schema_for!(topos_types::results::StatusData)),
        ),
        // On-disk persisted client documents.
        (
            "persisted-sync",
            emit(schemars::schema_for!(topos_types::persisted::SyncState)),
        ),
        (
            "persisted-lock",
            emit(schemars::schema_for!(topos_types::persisted::Lock)),
        ),
        (
            "persisted-map",
            emit(schemars::schema_for!(topos_types::persisted::PlacementMap)),
        ),
        (
            "persisted-op",
            emit(schemars::schema_for!(topos_types::persisted::OpRecord)),
        ),
        (
            "persisted-conflict",
            emit(schemars::schema_for!(topos_types::persisted::ConflictState)),
        ),
    ]
}

fn emit(schema: schemars::Schema) -> String {
    let mut s = serde_json::to_string_pretty(&schema).expect("a schema always serializes");
    s.push('\n');
    s
}

fn workspace_root() -> PathBuf {
    // xtask lives at <workspace-root>/xtask, so its manifest dir's parent is the root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent dir")
        .to_path_buf()
}

fn schemas_dir() -> PathBuf {
    workspace_root().join("contracts/schemas")
}

fn openapi_dir() -> PathBuf {
    workspace_root().join("contracts/openapi")
}

/// The committed OpenAPI artifact for the plane's HTTP surface, generated from `topos_plane::openapi()`
/// (the annotated routes + the `topos-types` wire DTOs) — pretty JSON + a trailing newline, like `emit`.
fn openapi_json() -> String {
    let mut s = serde_json::to_string_pretty(&topos_plane::openapi())
        .expect("the OpenAPI document always serializes");
    s.push('\n');
    s
}

/// Generate (or `--check`) `contracts/openapi/openapi.json`, mirroring the schema drift discipline
/// (stale / missing / orphan all fail). Folded into [`gen_schema`] so one gate covers both contracts.
fn gen_openapi(check: bool) -> Result<()> {
    let dir = openapi_dir();
    let path = dir.join("openapi.json");
    let content = openapi_json();
    if check {
        let mut drift = Vec::new();
        match fs::read_to_string(&path) {
            Ok(existing) if existing == content => {}
            Ok(_) => drift.push("openapi.json (stale)".to_owned()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                drift.push("openapi.json (missing)".to_owned())
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        }
        // An orphan `*.json` under contracts/openapi (produced by nothing) is drift too.
        if dir.is_dir() {
            for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
                let name = entry?.file_name().to_string_lossy().into_owned();
                if name.ends_with(".json") && name != "openapi.json" {
                    drift.push(format!("{name} (orphan — generated by nothing)"));
                }
            }
        }
        if drift.is_empty() {
            println!("openapi up to date");
        } else {
            bail!(
                "openapi drift: {} — run `cargo xtask gen-schema` and commit",
                drift.join(", ")
            );
        }
    } else {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        fs::write(&path, &content).with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

fn gen_schema(check: bool) -> Result<()> {
    let dir = schemas_dir();
    if !check {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let generated = schemas();
    let expected: BTreeSet<String> = generated
        .iter()
        .map(|(name, _)| format!("{name}.schema.json"))
        .collect();

    let mut drift = Vec::new();
    for (name, content) in &generated {
        let path = dir.join(format!("{name}.schema.json"));
        if check {
            // A read error is NOT silently treated as drift (the old `unwrap_or_default` masked
            // a permissions/IO fault as a stale schema). Missing vs stale are reported distinctly;
            // any other IO error aborts.
            match fs::read_to_string(&path) {
                Ok(existing) if existing == *content => {}
                Ok(_) => drift.push(format!("{name} (stale)")),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    drift.push(format!("{name} (missing)"))
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("reading {}", path.display()));
                }
            }
        } else {
            fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
            println!("wrote {}", path.display());
        }
    }

    if check {
        // An orphan schema (committed but produced by no current type) is also drift — otherwise a
        // deleted/renamed type leaves a stale public contract behind that the gate never notices.
        for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if name.ends_with(".schema.json") && !expected.contains(&name) {
                drift.push(format!("{name} (orphan — generated by no current type)"));
            }
        }
        if drift.is_empty() {
            println!("schemas up to date");
        } else {
            bail!(
                "schema drift: {} — run `cargo xtask gen-schema` and commit",
                drift.join(", ")
            );
        }
    }
    // The plane OpenAPI rides the same gate (one `gen-schema` regenerates + checks both contracts).
    gen_openapi(check)?;
    Ok(())
}

/// Golden `--json` fixtures — representative envelopes built FROM the typed shapes (so they cannot
/// drift from the contract) and committed as the agent-facing examples + the positive L1 oracle.
fn fixtures() -> Vec<(&'static str, String)> {
    use topos_types::persisted::{ConflictPathKind, ConflictReason};
    use topos_types::requests::{WireDelivery, WireDeliverySkill, WireNotice, WireVia};
    use topos_types::results::{
        AddData, ConflictHolds, ConflictPathReport, ConflictPlacement, DiffData, DiffPatchInfo,
        DiffSource, EnrollmentPending, InviteReadData, ListData, LogData, LoginData, LogoutData,
        MergeReport, ProtectData, PublishData, PublishDescribeData, PublishGate, PublishedMatch,
        PullAction, PullData, PullSkill, RemoveData, RemoveItem, RemoveKind, ReviewIndexData,
        ReviewIndexEntry, SkillEntry, SkillStatus, StatusData, StatusScope, StatusScopeSummary,
        StatusTrigger, WorkspaceSyncReport,
    };
    use topos_types::results::{AttentionCount, ListScope, McpServerSummary};
    use topos_types::{ActionCode, Affected, JsonEnvelope, Receipt, TerminalOutcome, WireError};

    let argv = |parts: &[&str]| parts.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

    // The deterministic identity of the committed `tests/fixtures/pr-describe` skill (device id
    // `d_test`, the fixed adopt message) — the local verbs reproduce these byte-for-byte.
    let fx_version = "d77b648d8149d63189864c6b6d06da4f7919935c4242cc197e708b1dafe941d5";
    let fx_digest = "c35004153b0f72e2e8363b557f36594319d5382eb9e4c7add5ff0feb3b15c369";
    // The digest of the one-line DRAFT edit below (`clear` → `GREAT`) — what a bare `diff` reports so
    // `publish --approve <skill>@<digest>` consents to the bytes being shipped, not the base version's.
    let fx_draft_digest = "0c6cdac7150f974c8acb9d608516adfb5181655ce0316464300fa82bbb5c19fe";

    // `add` of the fixture skill (offline; no plane op, so no receipt).
    let add_ok = JsonEnvelope {
        schema_version: 1,
        command: "add".to_owned(),
        ok: true,
        data: serde_json::to_value(AddData {
            skill_id: Some("topos_t00".to_owned()),
            name: "pr-describe".to_owned(),
            version_id: Some(fx_version.to_owned()),
            bundle_digest: Some(fx_digest.to_owned()),
            tracked: true,
            // The fixture skill is adopted from a plain dir (not under ~/.claude), so it is recognized as
            // no harness and arms no auto-update trigger — all three omit from the envelope.
            harness: None,
            harness_slug: None,
            currency: None,
            triggers: Vec::new(),
            // Adopted from a local dir, not a remote source — no upstream origin.
            origin: None,
            // The fixture adopt writes no manifest line (no machine roots in the fixture rig).
            manifest: None,
            reference: None,
            undo: Vec::new(),
            governed_copy: None,
            // The fixture rig has no session, so no workspace can publish the same name.
            published_match: None,
            note: None,
            mcp: None,
            dest: Vec::new(),
            display: None,
            dest_resolved: Vec::new(),
        })
        .expect("AddData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // The bare-NAME arms, where the connected workspaces are part of the namespace.
    //
    // (a) Nothing local carries the name, ONE workspace publishes it: the row is that workspace's
    //     canonical reference, and the note teaches the spelling it resolved to.
    let add_subscribed = JsonEnvelope {
        schema_version: 1,
        command: "add".to_owned(),
        ok: true,
        data: serde_json::to_value(AddData {
            skill_id: Some("s_codereview".to_owned()),
            name: "code-review".to_owned(),
            version_id: Some(fx_version.to_owned()),
            bundle_digest: Some(fx_digest.to_owned()),
            tracked: true,
            harness: None,
            harness_slug: None,
            currency: None,
            triggers: Vec::new(),
            origin: None,
            manifest: Some("/work/acme-api/topos.toml".to_owned()),
            reference: Some("topos.sh/acme/code-review".to_owned()),
            undo: argv(&["topos", "remove", "topos.sh/acme/code-review"]),
            governed_copy: None,
            // The subscribe IS the workspace copy — there is no local one to compare it against.
            published_match: None,
            note: Some(
                "resolved 'code-review' to topos.sh/acme/code-review — no untracked skill of that \
                 name is on this machine, and acme publishes it"
                    .to_owned(),
            ),
            mcp: None,
            dest: Vec::new(),
            display: None,
            dest_resolved: Vec::new(),
        })
        .expect("AddData serializes"),
        warnings: vec![],
        next_actions: vec![topos::actions::next_action(
            ActionCode::from("UNDO".to_owned()),
            argv(&["topos", "remove", "topos.sh/acme/code-review"]),
        )],
        receipt: None,
        error: None,
    };

    // (b) A local directory carried the name and adopted in place — and a connected workspace
    //     publishes the same name, so the team-managed spelling rides the receipt beside it.
    let add_published_match = JsonEnvelope {
        schema_version: 1,
        command: "add".to_owned(),
        ok: true,
        data: serde_json::to_value(AddData {
            skill_id: Some("topos_t00".to_owned()),
            name: "pr-describe".to_owned(),
            version_id: Some(fx_version.to_owned()),
            bundle_digest: Some(fx_digest.to_owned()),
            tracked: true,
            harness: None,
            harness_slug: None,
            currency: None,
            triggers: Vec::new(),
            origin: None,
            manifest: Some("/work/acme-api/topos.toml".to_owned()),
            reference: Some("./tools/pr-describe".to_owned()),
            undo: argv(&["topos", "remove", "./tools/pr-describe"]),
            governed_copy: None,
            published_match: Some(PublishedMatch {
                workspace: "acme".to_owned(),
                name: "pr-describe".to_owned(),
                reference: "topos.sh/acme/pr-describe".to_owned(),
                // The adopted bytes hash to exactly what the workspace serves today.
                identical: true,
            }),
            note: None,
            mcp: None,
            dest: Vec::new(),
            display: None,
            dest_resolved: Vec::new(),
        })
        .expect("AddData serializes"),
        warnings: vec![],
        next_actions: vec![topos::actions::next_action(
            ActionCode::from("UNDO".to_owned()),
            argv(&["topos", "remove", "./tools/pr-describe"]),
        )],
        receipt: None,
        error: None,
    };

    // (c) Nothing local carries the name and SEVERAL workspaces publish it — the refusal names
    //     every spelling, machine-readably in `data.references` and as one runnable action each.
    let ws_refs = ["topos.sh/acme/code-review", "topos.sh/beta/code-review"];
    let ambiguous_workspace_actions: Vec<_> = ws_refs
        .iter()
        .map(|r| {
            topos::actions::next_action(
                ActionCode::from("RUN_COMMAND".to_owned()),
                argv(&["topos", "add", r, "--json"]),
            )
        })
        .collect();
    let add_ambiguous_workspace = JsonEnvelope {
        schema_version: 1,
        command: "add".to_owned(),
        ok: false,
        data: serde_json::json!({ "references": ws_refs }),
        warnings: vec![],
        next_actions: ambiguous_workspace_actions.clone(),
        receipt: None,
        error: Some(WireError {
            code: "AMBIGUOUS_WORKSPACE".to_owned(),
            outcome: TerminalOutcome::AmbiguousName,
            retryable: false,
            affected: Affected::default(),
            expected_generation: None,
            current_generation: None,
            context: serde_json::json!({
                "message": "'code-review' is published in 2 of the workspaces this machine is \
                            connected to (topos.sh/acme/code-review, topos.sh/beta/code-review) — \
                            name the one you mean (`topos add <reference>`)"
            }),
            next_actions: ambiguous_workspace_actions,
        }),
    };

    // (d) The LOCAL ambiguity, enriched: two directories carry the name in one harness, and a
    //     workspace publishes it too — so the refusal offers the inventory read AND the subscribe.
    let ambiguous_scope_actions = vec![
        topos::actions::next_action(
            ActionCode::DisambiguateName,
            argv(&["topos", "list", "--json"]),
        ),
        topos::actions::next_action(
            ActionCode::from("RUN_COMMAND".to_owned()),
            argv(&["topos", "add", "topos.sh/acme/code-review", "--json"]),
        ),
    ];
    // The LOCAL MCP DOOR — `topos add --kind mcp ./team-weather -g`. This is now the only door
    // that adopts a server from bytes on this machine (a workspace reference is the other way in,
    // and needs no flag at all). The folder IS the bundle: adopted in place, so the row points at
    // it and the undo is the row drop. No version history is minted, so `skill_id` and
    // `version_id` are ABSENT — but the bundle digest is real, the kernel digest of the document
    // bytes, recomputable by anyone holding that folder.
    //
    // The typed `mcp` block carries the server facts a JSON consumer reads, and `agents` names
    // the agents the converge actually reached — an empty list there is what makes the receipt
    // say the row was recorded rather than installed.
    let add_mcp_adopted = JsonEnvelope {
        schema_version: 1,
        command: "add".to_owned(),
        ok: true,
        data: serde_json::to_value(AddData {
            skill_id: None,
            name: "team-weather".to_owned(),
            version_id: None,
            // A stand-in digest of the right SHAPE: the real one is the kernel digest of the
            // one-file bundle in the adopted folder.
            bundle_digest: Some("f".repeat(64)),
            tracked: true,
            harness: None,
            harness_slug: None,
            currency: None,
            triggers: Vec::new(),
            origin: None,
            manifest: Some("/home/ada/.topos/topos.toml".to_owned()),
            reference: Some("/home/ada/work/team-weather".to_owned()),
            undo: argv(&["topos", "remove", "-g", "/home/ada/work/team-weather"]),
            governed_copy: None,
            published_match: None,
            note: Some(
                "MCP server io.github.acme/weather v1.4.0 — https://weather.acme.example/mcp \
                 over streamable-http, auth oauth · /home/ada/.cursor/mcp.json: server entry — \
                 restart Cursor"
                    .to_owned(),
            ),
            dest: Vec::new(),
            display: None,
            mcp: Some(McpServerSummary {
                server: "io.github.acme/weather".to_owned(),
                description: "Current conditions and forecasts for a named place.".to_owned(),
                version: "1.4.0".to_owned(),
                url: "https://weather.acme.example/mcp".to_owned(),
                transport: "streamable-http".to_owned(),
                auth: Some("oauth".to_owned()),
                // Only LITERAL headers reach a receipt — a credential-shaped one refuses the
                // whole document instead.
                headers: vec!["X-Region".to_owned()],
                bundle: Some("/home/ada/work/team-weather".to_owned()),
                agents: vec!["Claude Code".to_owned(), "Cursor".to_owned()],
            }),
            dest_resolved: Vec::new(),
        })
        .expect("AddData serializes"),
        warnings: vec![],
        // The undo takes a SERVER off this machine, so its caution says so rather than borrowing
        // a skill's sentence about folders — read off the receipt, never hardcoded.
        next_actions: vec![topos::actions::next_action_about(
            ActionCode::from("UNDO".to_owned()),
            argv(&["topos", "remove", "-g", "/home/ada/work/team-weather"]),
            topos::actions::Subject::McpServer,
        )],
        receipt: None,
        error: None,
    };

    let add_ambiguous_scope = JsonEnvelope {
        schema_version: 1,
        command: "add".to_owned(),
        ok: false,
        data: serde_json::json!({}),
        warnings: vec![],
        next_actions: ambiguous_scope_actions.clone(),
        receipt: None,
        error: Some(WireError {
            code: "AMBIGUOUS_SCOPE".to_owned(),
            outcome: TerminalOutcome::AmbiguousName,
            retryable: false,
            affected: Affected::default(),
            expected_generation: None,
            current_generation: None,
            context: serde_json::json!({
                "message": "the skill 'code-review' in harness 'claude-code' matches 2 directories \
                            (/home/ada/.claude/skills/code-review, \
                            /work/acme-api/.claude/skills/code-review) — adopt one by path (`topos \
                            add <dir>`); every local copy above is byte-identical to what \
                            `topos.sh/acme/code-review` serves today — `topos add \
                            topos.sh/acme/code-review` subscribes to the team's copy, while \
                            adopting a path forks it into a new, unmanaged skill"
            }),
            next_actions: ambiguous_scope_actions,
        }),
    };

    // A clean `pull` that found one followed skill already current.
    let pull_ok = JsonEnvelope {
        schema_version: 1,
        command: "pull".to_owned(),
        ok: true,
        data: serde_json::to_value(PullData {
            scope: None,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            skills: vec![PullSkill {
                skill: "pr-describe".to_owned(),
                workspace_id: Some("w_acme".to_owned()),
                observed: 42,
                applied: 42,
                action: PullAction::UpToDate,
                merge: None,
                synced_placements: None,
                destinations: Vec::new(),
                kept: Vec::new(),
                display: None,
                note: None,
                scope: Some("person".to_owned()),
                harnesses: Vec::new(),
                kind: None,
                draft: false,
            }],
            proposals_awaiting: 0,
        })
        .expect("PullData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // A `pull` that auto-resolved a diverged draft cleanly → a draft-on-current (publishable).
    let fx_merged = "1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f";
    let pull_merged = JsonEnvelope {
        schema_version: 1,
        command: "pull".to_owned(),
        ok: true,
        data: serde_json::to_value(PullData {
            scope: None,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            skills: vec![PullSkill {
                skill: "pr-describe".to_owned(),
                workspace_id: Some("w_acme".to_owned()),
                observed: 7,
                applied: 7,
                action: PullAction::Merged,
                merge: Some(MergeReport {
                    base_version_id: fx_version.to_owned(),
                    theirs_version_id: fx_digest.to_owned(),
                    result_version_id: fx_merged.to_owned(),
                    result_digest: fx_digest.to_owned(),
                    clean: true,
                    conflicts: vec![],
                    drop_diff: None,
                    reason: None,
                    // A merge that ran to completion on its own was never stopped, so no exit
                    // finished it.
                    resolved: None,
                    took: vec![],
                    placements: vec![],
                    copy_dir: None,
                }),
                synced_placements: None,
                destinations: Vec::new(),
                kept: Vec::new(),
                display: None,
                note: None,
                scope: Some("person".to_owned()),
                harnesses: Vec::new(),
                kind: None,
                draft: false,
            }],
            proposals_awaiting: 0,
        })
        .expect("PullData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // A `pull` whose merge conflicted → every agent folder still holding the author's own version,
    // the complete conflict tree in the scope's own workbench, publish blocked until resolved.
    let pull_conflicted = JsonEnvelope {
        schema_version: 1,
        command: "pull".to_owned(),
        ok: true,
        data: serde_json::to_value(PullData {
            scope: None,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            skills: vec![PullSkill {
                skill: "pr-describe".to_owned(),
                workspace_id: Some("w_acme".to_owned()),
                observed: 7,
                applied: 7,
                action: PullAction::Conflicted,
                merge: Some(MergeReport {
                    base_version_id: fx_version.to_owned(),
                    theirs_version_id: fx_digest.to_owned(),
                    result_version_id: fx_merged.to_owned(),
                    result_digest: fx_merged.to_owned(),
                    clean: false,
                    conflicts: vec![ConflictPathReport {
                        path: "SKILL.md".to_owned(),
                        kind: ConflictPathKind::Content,
                    }],
                    drop_diff: None,
                    // WHY it stopped — which decides what the workbench below holds.
                    reason: Some(ConflictReason::ThreeWay),
                    // A block is finished by neither exit, so it takes nothing.
                    resolved: None,
                    took: vec![],
                    // Each agent folder and WHAT IS IN IT, read from the folder itself: a stop
                    // writes to none of them, so a freshly stopped merge reads `yours` across the
                    // set. Beside it, the workbench the marked-up copy of both versions went to.
                    placements: vec![ConflictPlacement {
                        dir: "~/.claude/skills/pr-describe".to_owned(),
                        holds: ConflictHolds::Yours,
                    }],
                    copy_dir: Some("~/.topos/conflicts/pr-describe".to_owned()),
                }),
                synced_placements: None,
                destinations: Vec::new(),
                kept: Vec::new(),
                display: None,
                note: None,
                scope: Some("person".to_owned()),
                harnesses: Vec::new(),
                kind: None,
                draft: false,
            }],
            proposals_awaiting: 0,
        })
        .expect("PullData serializes"),
        warnings: vec![],
        // The two ways out, per stopped row — the same pair the receipt prints under it, spelled
        // for the row's own scope (`person` → `-g`). The one state that asks for a decision must
        // offer that decision on the machine surface too.
        next_actions: vec![
            topos::actions::next_action(
                ActionCode::ResolveDivergedDraft,
                argv(&[
                    "topos",
                    "update",
                    "-g",
                    "pr-describe",
                    "--keep-mine",
                    "--json",
                ]),
            ),
            topos::actions::next_action(
                ActionCode::ResolveDivergedDraft,
                argv(&["topos", "update", "-g", "pr-describe", "--reset", "--json"]),
            ),
        ],
        receipt: None,
        error: None,
    };

    // `list` over one workspace session with one assigned-and-applied skill: the machine scope is
    // the here-scope (no covering project file), governed by the machine's own
    // `~/.topos/topos.toml` and its feed line. Byte-equal to the real op's output in the
    // `list_golden_matches...` test — path-free by construction, so the bytes are stable across
    // machines.
    let list_ok = JsonEnvelope {
        schema_version: 1,
        command: "list".to_owned(),
        ok: true,
        data: serde_json::to_value(ListData {
            scopes: vec![ListScope {
                scope: "machine".to_owned(),
                manifest: Some("~/.topos/topos.toml".to_owned()),
                rows: vec![SkillEntry {
                    skill: "pr-describe".to_owned(),
                    workspace_id: Some("w_acme".to_owned()),
                    version_id: "d".repeat(64),
                    bundle_digest: "e".repeat(64),
                    draft: false,
                    pending_proposals: vec![],
                    source: Some("topos.sh/acme".to_owned()),
                    status: Some(SkillStatus::Current),
                    kind: None,
                    source_health: None,
                    // A clean row names no draft folder — both fields omit from the envelope.
                    draft_dir: None,
                    draft_diverged: None,
                }],
                orphans: Vec::new(),
            }],
            signed_in: true,
            ..Default::default()
        })
        .expect("ListData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // `diff` of a one-line draft edit against current — the vendored unified diff body.
    let diff_ok = JsonEnvelope {
        schema_version: 1,
        command: "diff".to_owned(),
        ok: true,
        data: serde_json::to_value(DiffData {
            source: DiffSource::Local,
            version_id: fx_version.to_owned(),
            bundle_digest: fx_draft_digest.to_owned(),
            diff: "--- a/SKILL.md\n+++ b/SKILL.md\n@@ -4,4 +4,4 @@\n \n # PR describe\n \n-Write a clear PR description.\n+Write a GREAT PR description.\n".to_owned(),
            truncated: false,
            files: Vec::new(),
            // One placement — nothing to disambiguate, so the copy is not named.
            dest: None,
            skill: None,
        })
        .expect("DiffData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // `log` after adopting the fixture skill — the local add action + the genesis version.
    let log_ok = JsonEnvelope {
        schema_version: 1,
        command: "log".to_owned(),
        ok: true,
        data: serde_json::to_value(LogData {
            events: vec![
                serde_json::json!({
                    "action": "add",
                    "skill_id": "topos_t00",
                    "name": "pr-describe",
                    "version_id": fx_version,
                    "at": 1_700_000_000_000u64,
                }),
                serde_json::json!({
                    "action": "version",
                    "version_id": fx_version,
                    "author": "d_test",
                    "message": "topos: add",
                    "parents": [],
                }),
            ],
            team: None,
            // A local skill resolved by its own name — no freed-base-name archived-successor hint (omits).
            archived_successor: None,
            truncated: false,
            total: None,
            // The delivering workspace's last exchange landed (a local-only skill has none at all).
            sync_fault: None,
        })
        .expect("LogData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // A direct `publish` a protected bundle DOWNGRADED to a proposal (never a rejection): the
    // version is safely staged as NEEDS_REVIEW and the receipt's `details.downgraded` says why the
    // pointer did not move.
    let publish_downgraded = JsonEnvelope {
        schema_version: 1,
        command: "publish".to_owned(),
        ok: true,
        data: serde_json::json!({}),
        warnings: vec![],
        next_actions: vec![],
        receipt: Some(Receipt {
            schema_version: 1,
            op_id: "f47ac10b-58cc-4372-a567-0e02b2c3d479".to_owned(),
            command: "publish".to_owned(),
            outcome: TerminalOutcome::NeedsReview,
            workspace_id: "w_demo".to_owned(),
            skill_id: Some("s_prdescribe".to_owned()),
            version_id: Some(
                "3f786850e387550fdab836ed7e6dc881de23001b3f786850e387550fdab836ed".to_owned(),
            ),
            bundle_digest: Some(
                "89e6c98d92887913cadf06b2adb97f26cde4849b89e6c98d92887913cadf06b2".to_owned(),
            ),
            expected_generation: Some(42),
            current_generation: None,
            created_at: "2026-06-25T00:00:00Z".to_owned(),
            details: Some(serde_json::json!({ "downgraded": true })),
        }),
        error: None,
    };

    // A publish that lost the race — the team moved current; rebase and retry.
    let publish_conflict = JsonEnvelope {
        schema_version: 1,
        command: "publish".to_owned(),
        ok: false,
        data: serde_json::json!({}),
        warnings: vec![],
        next_actions: vec![topos::actions::next_action(
            ActionCode::RebaseAndRetry,
            argv(&["topos", "publish", "pr-describe"]),
        )],
        receipt: Some(Receipt {
            schema_version: 1,
            op_id: "9f1b8c2e-7a6d-4e3f-9b0a-1c2d3e4f5a6b".to_owned(),
            command: "publish".to_owned(),
            outcome: TerminalOutcome::Conflict,
            workspace_id: "w_demo".to_owned(),
            skill_id: Some("s_prdescribe".to_owned()),
            version_id: None,
            bundle_digest: None,
            expected_generation: Some(42),
            current_generation: Some(43),
            created_at: "2026-06-25T00:00:00Z".to_owned(),
            details: None,
        }),
        error: Some(WireError {
            code: "STALE_BASE".to_owned(),
            outcome: TerminalOutcome::Conflict,
            // A blind retry cannot resolve a CAS conflict — the caller must pull/rebase first (the
            // client and the plane both compute false).
            retryable: false,
            affected: Affected {
                skill: Some("pr-describe".to_owned()),
                ..Default::default()
            },
            expected_generation: Some(42),
            current_generation: Some(43),
            context: serde_json::json!({}),
            next_actions: vec![topos::actions::next_action(
                ActionCode::RebaseAndRetry,
                argv(&["topos", "publish", "pr-describe"]),
            )],
        }),
    };

    // The per-session delivery answer (`GET /v1/workspaces/{ws}/delivery`): two entitled skills —
    // one via `everyone` only (indirect), one via `ops` + `everyone` WITH a direct assignment and
    // `reviewed` protection — one declined bundle (the caller's own web stance), one `verdict`
    // notice carrying its reason, and one open proposal awaiting review.
    let delivery_ok = WireDelivery {
        schema_version: 1,
        workspace_id: "w_demo".to_owned(),
        skills: vec![
            WireDeliverySkill {
                skill_id: "s_prdescribe".to_owned(),
                name: "pr-describe".to_owned(),
                kind: "skill".to_owned(),
                display_name: Some("PR describe".to_owned()),
                protection: "open".to_owned(),
                version_id: "a".repeat(64),
                bundle_digest: "b".repeat(64),
                generation: 42,
                updated_at: 1_700_000_000_000,
                via: WireVia {
                    channels: vec!["everyone".to_owned()],
                    direct: false,
                    assigned_by: None,
                    picked: None,
                },
            },
            WireDeliverySkill {
                skill_id: "s_deploy".to_owned(),
                name: "deploy".to_owned(),
                kind: "skill".to_owned(),
                display_name: None,
                protection: "reviewed".to_owned(),
                version_id: "c".repeat(64),
                bundle_digest: "d".repeat(64),
                generation: 23,
                updated_at: 1_700_000_100_000,
                via: WireVia {
                    channels: vec!["ops".to_owned(), "everyone".to_owned()],
                    direct: true,
                    assigned_by: Some("Alice Reviewer".to_owned()),
                    picked: None,
                },
            },
        ],
        declined: vec![topos_types::requests::WireDeclined {
            skill_id: "s_legacy".to_owned(),
            name: "legacy-notes".to_owned(),
        }],
        notices: vec![WireNotice {
            id: "ntc_01".to_owned(),
            kind: "verdict".to_owned(),
            skill_id: Some("s_deploy".to_owned()),
            skill_name: Some("deploy".to_owned()),
            version_id: Some("c".repeat(64)),
            actor: Some("reviewer@demo.test".to_owned()),
            outcome: Some("approve".to_owned()),
            reason: Some("Ship it — the rollback note is clear.".to_owned()),
            message: None,
            created_at: "2026-06-25T00:00:00Z".to_owned(),
        }],
        staleness_window_ms: 604_800_000,
        proposals_awaiting: 1,
        session_status: Some("active".to_owned()),
    };

    // The PENDING-session delivery: no data flows over a pending session — the server answers
    // empty sets and a zero gauge with `session_status: "pending"`; the client's sweep skips the
    // workspace quietly (a `status`-visible fact, not an error).
    let delivery_pending = WireDelivery {
        schema_version: 1,
        workspace_id: "w_demo".to_owned(),
        skills: vec![],
        declined: vec![],
        notices: vec![],
        staleness_window_ms: 604_800_000,
        proposals_awaiting: 0,
        session_status: Some("pending".to_owned()),
    };

    // =============================================================================================
    // The ADOPTED verb surface — the two-phase describe/apply envelopes (a bare mutating verb returns
    // the describe with `applied: false`; `--yes` returns it applied) + the reshaped reads.
    // =============================================================================================

    // `remove <skill>` (bare, LOSS-GUARDED) — the DESCRIBE a permanent delete holds: the skill is
    // a tracked, never-published local, so no other copy exists and the delete waits for `--yes`
    // (a manifest-line remove applies immediately instead — see `remove.ok`). `applied: false` —
    // nothing has changed.
    let remove_describe = JsonEnvelope {
        schema_version: 1,
        command: "remove".to_owned(),
        ok: true,
        data: serde_json::to_value(RemoveData {
            items: vec![RemoveItem {
                name: "deploy".to_owned(),
                kind: RemoveKind::TrackedLocalPermanent,
                manifest: None,
                workspace_id: None,
                agent_dirs: vec!["~/.claude/skills/deploy".to_owned()],
                kept_dirs: Vec::new(),
                bytes_kept: false,
                note: Some(
                    "'deploy' was never published — no other copy exists, so removing deletes it \
                     permanently (the topos entry is dropped too). Share it first with \
                     `topos publish deploy`, or apply with --yes"
                        .to_owned(),
                ),
            }],
            applied: false,
            undo: Vec::new(),
            uninstalled: Vec::new(),
        })
        .expect("RemoveData serializes"),
        warnings: vec![],
        next_actions: vec![topos::actions::next_action(
            ActionCode::from("APPLY_DESCRIBED".to_owned()),
            argv(&["topos", "remove", "deploy", "--yes"]),
        )],
        receipt: None,
        error: None,
    };

    // `remove <ref>` naming a manifest line — the IMMEDIATE apply (`applied: true`; the include
    // line leaves the nearest manifest, every sidecar byte kept) with the undo-led receipt
    // (`add` is the inverse).
    let remove_ok = JsonEnvelope {
        schema_version: 1,
        command: "remove".to_owned(),
        ok: true,
        data: serde_json::to_value(RemoveData {
            items: vec![RemoveItem {
                name: "deploy".to_owned(),
                kind: RemoveKind::ManifestRemoved,
                manifest: Some("./topos.toml".to_owned()),
                workspace_id: None,
                agent_dirs: Vec::new(),
                kept_dirs: Vec::new(),
                bytes_kept: true,
                note: None,
            }],
            applied: true,
            undo: argv(&["topos", "add", "@acme/deploy"]),
            uninstalled: Vec::new(),
        })
        .expect("RemoveData serializes"),
        warnings: vec![],
        next_actions: vec![topos::actions::next_action(
            ActionCode::from("UNDO".to_owned()),
            argv(&["topos", "add", "@acme/deploy"]),
        )],
        receipt: None,
        error: None,
    };

    // `protect <skill>` (bare) — the DESCRIBE of TIGHTENING a skill to `reviewed`.
    // `applied: false`, `loosening: false` (tightening takes reviewer+).
    let protect_describe = JsonEnvelope {
        schema_version: 1,
        command: "protect".to_owned(),
        ok: true,
        data: serde_json::to_value(ProtectData {
            target: "deploy".to_owned(),
            kind: "skill".to_owned(),
            workspace_id: "w_acme".to_owned(),
            level: "reviewed".to_owned(),
            loosening: false,
            note: None,
            applied: false,
        })
        .expect("ProtectData serializes"),
        warnings: vec![],
        next_actions: vec![topos::actions::next_action(
            ActionCode::from("APPLY_DESCRIBED".to_owned()),
            argv(&["topos", "protect", "deploy", "--yes"]),
        )],
        receipt: None,
        error: None,
    };

    // `review` (bare) — the review INBOX/OUTBOX across enrolled workspaces, author-message first.
    let review_inbox = JsonEnvelope {
        schema_version: 1,
        command: "review".to_owned(),
        ok: true,
        data: serde_json::to_value(ReviewIndexData {
            inbox: vec![ReviewIndexEntry {
                workspace_id: "w_acme".to_owned(),
                workspace_name: "acme".to_owned(),
                skill: "deploy".to_owned(),
                proposal: format!("deploy@{}", "c".repeat(64)),
                proposer: "alice@acme.com".to_owned(),
                message: "Tighten the rollback note.".to_owned(),
                base_version_id: "a".repeat(64),
                created_at: "2026-06-25T00:00:00Z".to_owned(),
                stale: false,
            }],
            outbox: vec![],
        })
        .expect("ReviewIndexData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // `invite` (bare, no emails) — the no-mutation read of the workspace address
    // (`changed: false` — nothing was sent).
    let invite_read = JsonEnvelope {
        schema_version: 1,
        command: "invite".to_owned(),
        ok: true,
        data: serde_json::to_value(InviteReadData {
            address: "https://topos.sh/acme".to_owned(),
            changed: false,
        })
        .expect("InviteReadData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // `publish <skill>` (bare, enrolled) — the DESCRIBE: where it lands, the gate outcome (an `open`
    // bundle lands directly), the share line, and the undo path. Nothing shipped yet.
    // The TTY prints only the destination + the gate; the rest of these fields are the envelope's
    // (an agent reads them, a person reads them on the receipt).
    let publish_describe = JsonEnvelope {
        schema_version: 1,
        command: "publish".to_owned(),
        ok: true,
        data: serde_json::to_value(PublishDescribeData {
            skill: "deploy".to_owned(),
            skill_id: "s_deploy".to_owned(),
            workspace_id: "w_acme".to_owned(),
            workspace_display_name: Some("Acme".to_owned()),
            workspace_address: Some("topos.sh/acme".to_owned()),
            bundle_digest: "b".repeat(64),
            placements: vec!["everyone".to_owned()],
            // ONE edited copy — nothing was chosen between, so no folder is named.
            from_placement: None,
            other_edited: Vec::new(),
            gate: PublishGate::Lands,
            is_revert: false,
            share_line: Some("https://topos.sh/acme/skills/deploy".to_owned()),
            invite_line: Some(
                "Ask your agent: \"Set up Topos for us: fetch https://topos.sh/agent and follow \
                 it. Our workspace: https://topos.sh/acme\""
                    .to_owned(),
            ),
            undo: Some(format!("topos revert deploy --to {}", "a".repeat(64))),
            origin_note: None,
            placement_note: None,
            // An up-to-date copy predicts nothing — the additive preview omits (absent = unknown).
            merge_preview: None,
            manifest: None,
            reference: None,
            converted_from: None,
            // The ordinary skill publish: the additive kind tag omits (absent = a skill).
            kind: None,
        })
        .expect("PublishDescribeData serializes"),
        warnings: vec![],
        next_actions: vec![topos::actions::next_action(
            ActionCode::from("APPLY_DESCRIBED".to_owned()),
            argv(&["topos", "publish", "deploy", "--yes"]),
        )],
        receipt: None,
        error: None,
    };

    // `publish <skill> --yes` LANDED — the public SUCCESS envelope: the moved pointer's facts plus
    // the address-derived lines (`workspace_address` / `share_line` / `invite_line`) the landed
    // receipt carries via the best-effort post-publish `me` read (each omits when that read fails
    // or the address does not validate — the publish itself is unaffected), and the `undo` that
    // puts the team back on the version `current` held before this one.
    let publish_ok = JsonEnvelope {
        schema_version: 1,
        command: "publish".to_owned(),
        ok: true,
        data: serde_json::to_value(PublishData {
            manifest: None,
            reference: None,
            converted_from: None,
            skill_id: "s_deploy".to_owned(),
            name: "deploy".to_owned(),
            version_id: "d".repeat(64),
            bundle_digest: "b".repeat(64),
            current_generation: 43,
            added: None,
            placement_withheld: None,
            placement_missing: None,
            invite_line: Some(
                "Ask your agent: \"Set up Topos for us: fetch https://topos.sh/agent and follow \
                 it. Our workspace: https://topos.sh/acme\""
                    .to_owned(),
            ),
            origin_note: None,
            rewrite_pending: None,
            rewrite_skipped: None,
            // The ordinary skill publish: the additive kind tag omits (absent = a skill).
            kind: None,
            workspace_address: Some("topos.sh/acme".to_owned()),
            share_line: Some("https://topos.sh/acme/skills/deploy".to_owned()),
            undo: Some("topos revert deploy --to aaaaaaaaaaaa".to_owned()),
            // ONE edited copy — the receipt names no folder and leaves none behind.
            from_placement: None,
            other_edited: Vec::new(),
        })
        .expect("PublishData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // `publish <skill>` when the draft equals `current` — the NEGATIVE `NO_CHANGES` refusal (a permanent
    // failure: there is nothing to ship, so no retry helps).
    let publish_no_changes = JsonEnvelope {
        schema_version: 1,
        command: "publish".to_owned(),
        ok: false,
        data: serde_json::json!({}),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: Some(WireError {
            code: "NO_CHANGES".to_owned(),
            outcome: TerminalOutcome::PermanentFailure,
            retryable: false,
            affected: Affected {
                skill: Some("deploy".to_owned()),
                ..Default::default()
            },
            expected_generation: None,
            current_generation: None,
            context: serde_json::json!({}),
            next_actions: vec![],
        }),
    };

    // A bare `update` sweep whose workspace has gone STALE — the additive `sync` freshness rows + the
    // person-scoped notices feed the hook's staleness warning + narration read. `command: "update"`
    // (the reshaped verb; `pull` is its hidden alias).
    let update_stale = JsonEnvelope {
        schema_version: 1,
        command: "update".to_owned(),
        ok: true,
        data: serde_json::to_value(PullData {
            scope: None,
            skills: vec![PullSkill {
                skill: "deploy".to_owned(),
                workspace_id: Some("w_acme".to_owned()),
                observed: 12,
                applied: 12,
                action: PullAction::UpToDate,
                merge: None,
                synced_placements: None,
                destinations: Vec::new(),
                kept: Vec::new(),
                display: None,
                note: None,
                scope: Some("person".to_owned()),
                harnesses: Vec::new(),
                kind: None,
                draft: false,
            }],
            proposals_awaiting: 1,
            notices: vec![WireNotice {
                id: "ntc_09".to_owned(),
                kind: "verdict".to_owned(),
                skill_id: Some("s_deploy".to_owned()),
                skill_name: Some("deploy".to_owned()),
                version_id: Some("c".repeat(64)),
                actor: Some("reviewer@acme.com".to_owned()),
                outcome: Some("approve".to_owned()),
                reason: Some("Ship it.".to_owned()),
                message: None,
                created_at: "2026-06-25T00:00:00Z".to_owned(),
            }],
            sync: vec![WorkspaceSyncReport {
                workspace_id: "w_acme".to_owned(),
                last_delivery_at: Some(1_699_000_000_000),
                last_report_at: Some(1_699_000_000_000),
                staleness_window_ms: 604_800_000,
            }],
            behind_elsewhere: vec![],
        })
        .expect("PullData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // A BYTE-CAPPED `diff` (`--max-bytes`, or the `--json` default): the body keeps only the leading
    // whole-file sections that fit, `files` lists every changed file with `patch_omitted` marks, and
    // the FETCH_FULL_DIFF next action re-runs the same diff uncapped.
    let diff_truncated = JsonEnvelope {
        schema_version: 1,
        command: "diff".to_owned(),
        ok: true,
        data: serde_json::to_value(DiffData {
            source: DiffSource::Local,
            version_id: fx_version.to_owned(),
            bundle_digest: fx_draft_digest.to_owned(),
            diff: "--- a/SKILL.md\n+++ b/SKILL.md\n@@ -4,4 +4,4 @@\n \n # PR describe\n \n-Write a clear PR description.\n+Write a GREAT PR description.\n".to_owned(),
            truncated: true,
            files: vec![
                DiffPatchInfo {
                    path: "SKILL.md".to_owned(),
                    patch_omitted: false,
                    patch_bytes: 120,
                },
                DiffPatchInfo {
                    path: "reference.md".to_owned(),
                    patch_omitted: true,
                    patch_bytes: 98_304,
                },
            ],
            dest: None,
            skill: None,
        })
        .expect("DiffData serializes"),
        warnings: vec![],
        next_actions: vec![topos::actions::next_action(
            ActionCode::FetchFullDiff,
            argv(&["topos", "diff", "pr-describe", "--max-bytes", "0", "--json"]),
        )],
        receipt: None,
        error: None,
    };

    // A ROW-PAGED `log` (`--limit`/`--offset`, or the `--json` default page): the additive
    // `truncated`/`total` markers + the NEXT_PAGE next action carrying the COMPLETE argv.
    let log_paged = JsonEnvelope {
        schema_version: 1,
        command: "log".to_owned(),
        ok: true,
        data: serde_json::to_value(LogData {
            events: vec![
                serde_json::json!({
                    "action": "add",
                    "skill_id": "topos_t00",
                    "name": "pr-describe",
                    "version_id": fx_version,
                    "at": 1_700_000_000_000u64,
                }),
                serde_json::json!({
                    "action": "version",
                    "version_id": fx_version,
                    "author": "d_test",
                    "message": "topos: add",
                    "parents": [],
                }),
            ],
            team: None,
            archived_successor: None,
            truncated: true,
            total: Some(3),
            sync_fault: None,
        })
        .expect("LogData serializes"),
        warnings: vec![],
        next_actions: vec![topos::actions::next_action(
            ActionCode::NextPage,
            argv(&[
                "topos",
                "log",
                "pr-describe",
                "--limit",
                "2",
                "--offset",
                "2",
                "--json",
            ]),
        )],
        receipt: None,
        error: None,
    };

    // `status` — the offline health panel: an enrolled, signed-in install inside a project (the
    // here-scope body carries its attention counts; the machine scope rides the one summary line
    // with ITS counts) and the read-only trigger rows (OpenClaw's presence needs a live scheduler
    // query, so its row is an honest unknown).
    let status_ok = JsonEnvelope {
        schema_version: 1,
        command: "status".to_owned(),
        ok: true,
        data: serde_json::to_value(StatusData {
            version: "0.1.0".to_owned(),
            server: Some("https://topos.sh/api".to_owned()),
            signed_in: true,
            sessions: vec![topos_types::results::StatusSession {
                workspace_id: "w_demo".to_owned(),
                name: "demo".to_owned(),
                display_name: "Demo".to_owned(),
                host: "topos.sh".to_owned(),
                // An active session omits its status (the shape stays lean); a pending one
                // would read "pending" — awaiting owner approval.
                session_status: None,
            }],
            triggers: vec![
                StatusTrigger {
                    agent: "claude-code".to_owned(),
                    armed: Some(true),
                    note: None,
                },
                StatusTrigger {
                    agent: "openclaw".to_owned(),
                    armed: None,
                    note: Some("presence needs a live scheduler query".to_owned()),
                },
            ],
            scopes: vec![StatusScope {
                scope: "project".to_owned(),
                manifest: Some("/repo/topos.toml".to_owned()),
                regimes: Vec::new(),
                notes: Vec::new(),
                attention: vec![AttentionCount {
                    kind: "updates-pending".to_owned(),
                    count: 2,
                    command: "topos update".to_owned(),
                }],
            }],
            machine_summary: Some(StatusScopeSummary {
                attention: vec![AttentionCount {
                    kind: "assignments-not-applied".to_owned(),
                    count: 1,
                    command: "topos update -g".to_owned(),
                }],
                command: "topos status -g".to_owned(),
            }),
            // An external source's last auto-update check — recorded whatever its outcome, so
            // "is this still being kept current?" has a local answer.
            forge: vec![topos_types::results::ForgeSource {
                source: "github.com/vercel-labs/skills".to_owned(),
                checked_at: 1_750_000_000_000,
                answered_at: Some(1_750_000_000_000),
                commit: Some("1164afa5f0e21ebd01e6fc11249759353f494ad1".to_owned()),
                error: None,
                gone: false,
            }],
        })
        .expect("StatusData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // A PENDING `login <workspace-address>` — the RFC-8628-shaped flow awaits the browser
    // approval: still `ok = true` (nothing failed, a human approval is simply required); the
    // `ENROLL_RESUME` next-action re-invokes `login` (re-invoking IS the resume, at the disclosed
    // interval). The code rides its own field — it never rides a URL.
    let login_pending = JsonEnvelope {
        schema_version: 1,
        command: "login".to_owned(),
        ok: true,
        data: serde_json::to_value(LoginData {
            workspace_id: String::new(),
            host: "topos.sh".to_owned(),
            name: "acme".to_owned(),
            display_name: None,
            server: Some("https://topos.sh/api".to_owned()),
            session_id: None,
            session_status: "awaiting-approval".to_owned(),
            delivered: None,
            delivered_names: Vec::new(),
            manifest_note: None,
            user: None,
            feed_row_added: false,
            undo: Vec::new(),
            pending: Some(EnrollmentPending {
                verification_uri: "https://topos.sh/verify".to_owned(),
                user_code: "WXYZ-1234".to_owned(),
                expires_at: Some("2026-06-25T00:15:00Z".to_owned()),
                interval_secs: Some(5),
            }),
            currency: None,
            triggers: Vec::new(),
        })
        .expect("LoginData serializes"),
        warnings: vec![],
        next_actions: vec![topos::actions::next_action(
            ActionCode::from("ENROLL_RESUME".to_owned()),
            argv(&["topos", "login", "--json"]),
        )],
        receipt: None,
        error: None,
    };

    // The GRANTED login — the session row persisted (workspace-scoped credential), the acceptance
    // disclosure (`delivered` = what the profile delivers here right now; from here delivery is
    // silent).
    let login_ok = JsonEnvelope {
        schema_version: 1,
        command: "login".to_owned(),
        ok: true,
        data: serde_json::to_value(LoginData {
            workspace_id: "w_acme".to_owned(),
            host: "topos.sh".to_owned(),
            name: "acme".to_owned(),
            display_name: Some("Acme".to_owned()),
            server: Some("https://topos.sh/api".to_owned()),
            session_id: Some("sn_01hzy3".to_owned()),
            session_status: "active".to_owned(),
            delivered: Some(3),
            delivered_names: vec![
                "deploy".to_owned(),
                "code-review".to_owned(),
                "release-notes".to_owned(),
            ],
            manifest_note: None,
            user: Some("robert".to_owned()),
            feed_row_added: true,
            undo: vec!["remove".to_owned(), "-g".to_owned(), "@acme".to_owned()],
            pending: None,
            currency: None,
            triggers: Vec::new(),
        })
        .expect("LoginData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // The version floor's CLIENT half: the protocol card declared a server older than the oldest
    // wire this build speaks, so the login refuses before anything is minted. Both remedies are
    // real, but only one is this machine's to run — so only that one rides as an argv, pinned to
    // the server's own release.
    let server_too_old_actions = vec![topos::actions::next_action(
        ActionCode::SelfUpdate,
        argv(&["topos", "self-update", "--version", "v0.1.9"]),
    )];
    let login_server_too_old = JsonEnvelope {
        schema_version: 1,
        command: "login".to_owned(),
        ok: false,
        data: serde_json::json!({}),
        warnings: vec![],
        next_actions: server_too_old_actions.clone(),
        receipt: None,
        error: Some(WireError {
            code: "SERVER_TOO_OLD".to_owned(),
            outcome: TerminalOutcome::PermanentFailure,
            retryable: false,
            affected: Affected::default(),
            expected_generation: None,
            current_generation: None,
            context: serde_json::json!({
                "message": "that server is too old for this topos — it runs 0.1.9, and this \
                            build speaks to 0.1.15 and later; ask whoever runs the server to \
                            update it, or pin this machine back to the server's release"
            }),
            next_actions: server_too_old_actions,
        }),
    };

    // `logout` — the session ended: the server-side revoke landed, the local row deleted. Skills,
    // drafts, and manifests stay; `topos login <address>` starts a fresh session.
    let logout_ok = JsonEnvelope {
        schema_version: 1,
        command: "logout".to_owned(),
        ok: true,
        data: serde_json::to_value(LogoutData {
            ended: vec!["acme".to_owned()],
            server_revoked: true,
        })
        .expect("LogoutData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // The UNENROLLED bare `update` — an honest empty sweep that is really a dead-end: nothing is
    // followed because nothing CAN be. The join fix rides `next_actions` as an argv TEMPLATE whose
    // `needs` names the placeholder the caller must fill (`<workspace-address>`).
    let update_not_enrolled = JsonEnvelope {
        schema_version: 1,
        command: "update".to_owned(),
        ok: true,
        data: serde_json::to_value(PullData {
            scope: None,
            skills: vec![],
            proposals_awaiting: 0,
            notices: vec![],
            sync: vec![],
            behind_elsewhere: vec![],
        })
        .expect("PullData serializes"),
        warnings: vec![],
        next_actions: vec![topos::actions::next_action(
            ActionCode::from("LOGIN_WORKSPACE".to_owned()),
            argv(&["topos", "login", "<workspace-address>", "--json"]),
        )],
        receipt: None,
        error: None,
    };

    vec![
        ("json/status.ok", emit_json(&status_ok)),
        ("json/update.not-enrolled", emit_json(&update_not_enrolled)),
        ("json/pull.ok", emit_json(&pull_ok)),
        ("json/pull.merged", emit_json(&pull_merged)),
        ("json/pull.conflicted", emit_json(&pull_conflicted)),
        ("json/add.ok", emit_json(&add_ok)),
        ("json/add.subscribed", emit_json(&add_subscribed)),
        ("json/add.published-match", emit_json(&add_published_match)),
        (
            "json/add.ambiguous-workspace",
            emit_json(&add_ambiguous_workspace),
        ),
        ("json/add.ambiguous-scope", emit_json(&add_ambiguous_scope)),
        ("json/add.mcp-adopted", emit_json(&add_mcp_adopted)),
        ("json/list.ok", emit_json(&list_ok)),
        ("json/diff.ok", emit_json(&diff_ok)),
        ("json/log.ok", emit_json(&log_ok)),
        ("json/publish.downgraded", emit_json(&publish_downgraded)),
        ("json/publish.conflict", emit_json(&publish_conflict)),
        ("json/login.pending", emit_json(&login_pending)),
        ("json/login.ok", emit_json(&login_ok)),
        (
            "json/login.server-too-old",
            emit_json(&login_server_too_old),
        ),
        ("json/logout.ok", emit_json(&logout_ok)),
        ("json/delivery.ok", emit_json(&delivery_ok)),
        ("json/delivery.pending", emit_json(&delivery_pending)),
        ("json/remove.describe", emit_json(&remove_describe)),
        ("json/remove.ok", emit_json(&remove_ok)),
        ("json/protect.describe", emit_json(&protect_describe)),
        ("json/review.inbox", emit_json(&review_inbox)),
        ("json/invite.read", emit_json(&invite_read)),
        ("json/publish.describe", emit_json(&publish_describe)),
        ("json/publish.ok", emit_json(&publish_ok)),
        ("json/publish.no-changes", emit_json(&publish_no_changes)),
        ("json/update.stale", emit_json(&update_stale)),
        ("json/diff.truncated", emit_json(&diff_truncated)),
        ("json/log.paged", emit_json(&log_paged)),
    ]
}

fn emit_json<T: serde::Serialize>(value: &T) -> String {
    let mut s = serde_json::to_string_pretty(value).expect("a fixture always serializes");
    s.push('\n');
    s
}

fn fixtures_dir() -> PathBuf {
    workspace_root().join("contracts/fixtures")
}

/// Generate (or `--check`) the golden `--json` fixtures, mirroring the schema drift gate.
fn gen_fixtures(check: bool) -> Result<()> {
    let dir = fixtures_dir();
    let mut drift = Vec::new();
    for (name, content) in fixtures() {
        let path = dir.join(format!("{name}.json"));
        if check {
            match fs::read_to_string(&path) {
                Ok(existing) if existing == content => {}
                Ok(_) => drift.push(format!("{name} (stale)")),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    drift.push(format!("{name} (missing)"))
                }
                Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
            }
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            fs::write(&path, &content).with_context(|| format!("writing {}", path.display()))?;
            println!("wrote {}", path.display());
        }
    }
    if check {
        // Reject orphan fixtures (a committed `*.json` produced by no current generator), so a
        // renamed/removed fixture can't leave a stale example behind — same discipline as the schemas.
        let expected: BTreeSet<PathBuf> = fixtures()
            .iter()
            .map(|(name, _)| dir.join(format!("{name}.json")))
            .collect();
        for path in json_files_under(&dir)? {
            if !expected.contains(&path) {
                let rel = path.strip_prefix(&dir).unwrap_or(&path);
                drift.push(format!("{} (orphan — generated by nothing)", rel.display()));
            }
        }
        if drift.is_empty() {
            println!("fixtures up to date");
        } else {
            bail!(
                "fixture drift: {} — run `cargo xtask gen-fixtures` and commit",
                drift.join(", ")
            );
        }
    }
    Ok(())
}

/// Every `*.json` file under `dir`, recursively (the fixtures live in sub-dirs like `json/`).
fn json_files_under(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if dir.is_dir() {
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            if path.is_dir() {
                out.extend(json_files_under(&path)?);
            } else if path.extension().is_some_and(|e| e == "json") {
                out.push(path);
            }
        }
    }
    Ok(out)
}

// =================================================================================================
// gen-cli-ref — the CLI reference, rendered from the REAL `clap` tree by the client lib's own
// renderer (`topos::cli_ref_md()`). TWO committed copies of the same bytes: `docs/cli.md` (the
// repo's reference doc) and `skills/topos/reference.md` (the downloadable public skill's copy —
// the built-in bundle renders the same fn at placement, and skill installers fetch the committed
// file straight from the repo, so it must never go stale). Same byte-compare `--check` drift
// discipline as the schema/fixture gates.
// =================================================================================================

/// The two committed copies the one renderer keeps in step.
fn cli_ref_paths() -> [PathBuf; 2] {
    [
        workspace_root().join("docs").join("cli.md"),
        workspace_root()
            .join("skills")
            .join("topos")
            .join("reference.md"),
    ]
}

/// Generate (or `--check`) `docs/cli.md` AND the public skill's `skills/topos/reference.md`,
/// mirroring the schema/fixture drift discipline (a stale or missing file fails). Rendered from
/// the real clap tree, so both references track the binary exactly.
fn gen_cli_ref(check: bool) -> Result<()> {
    let content = topos::cli_ref_md();
    for path in cli_ref_paths() {
        let shown = path
            .strip_prefix(workspace_root())
            .unwrap_or(&path)
            .display()
            .to_string();
        if check {
            match fs::read_to_string(&path) {
                Ok(existing) if existing == content => {
                    println!("cli reference up to date: {shown}");
                }
                Ok(_) => bail!(
                    "cli-reference drift: {shown} is stale — run `cargo xtask gen-cli-ref` and commit"
                ),
                Err(e) if e.kind() == io::ErrorKind::NotFound => bail!(
                    "cli-reference drift: {shown} is missing — run `cargo xtask gen-cli-ref` and commit"
                ),
                Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
            }
        } else {
            let dir = path.parent().expect("cli-ref paths have a parent");
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            fs::write(&path, &content).with_context(|| format!("writing {}", path.display()))?;
            println!("wrote {shown}");
        }
    }
    Ok(())
}

/// The resolved set of crate names in a package's NORMAL (non-dev, non-build) dependency tree.
fn normal_tree(pkg: &str) -> Result<BTreeSet<String>> {
    // `--all-features` so a future *feature-gated* edge (e.g. an optional `topos -> plane-store`)
    // can't slip past the layering check by being off-by-default.
    let out = Command::new(env!("CARGO"))
        .current_dir(workspace_root())
        .args([
            "tree",
            "-p",
            pkg,
            "-e",
            "normal",
            "--all-features",
            "--prefix",
            "none",
        ])
        .output()
        .with_context(|| format!("running `cargo tree -p {pkg}`"))?;
    if !out.status.success() {
        bail!(
            "`cargo tree -p {pkg}` failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect())
}

/// Fail if any banned crate is reachable in `pkg`'s normal dependency tree.
fn assert_excludes(pkg: &str, banned: &[&str]) -> Result<()> {
    let tree = normal_tree(pkg)?;
    let hits: Vec<&str> = banned
        .iter()
        .copied()
        .filter(|b| tree.contains(*b))
        .collect();
    if hits.is_empty() {
        println!("ok: `{pkg}` carries none of {banned:?}");
        Ok(())
    } else {
        bail!("architectural layering violated: `{pkg}` must not depend on {hits:?}");
    }
}

/// The PRODUCTION normal-dependency graph of `pkg` (NO `--all-features` — the artifact a real `cargo build`
/// resolves), one line per node as `{p} {f}` (package + its active features), prefix-free.
fn production_dep_lines(pkg: &str) -> Result<Vec<String>> {
    let out = Command::new(env!("CARGO"))
        .current_dir(workspace_root())
        .args([
            "tree", "-p", pkg, "-e", "normal", "-f", "{p} {f}", "--prefix", "none",
        ])
        .output()
        .with_context(|| format!("running `cargo tree -p {pkg}` (production features)"))?;
    if !out.status.success() {
        bail!(
            "`cargo tree -p {pkg}` (production features) failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// Assert the test-only `test-fixtures` feature stays OFF in `pkg`'s PRODUCTION graph — i.e. the node for
/// `on_crate` carries no `test-fixtures` feature. This is the cross-repo trust guard: a production
/// `topos-plane` must never enable `plane-store/test-fixtures` (the seed/move/tamper shims), and a production
/// `topos` must never enable its own `test-fixtures` (the `test_support` facade). `test-fixtures` cannot
/// appear in a version or a path, so its presence on the node's line means the feature is on.
fn assert_test_fixtures_off(pkg: &str, on_crate: &str) -> Result<()> {
    for line in production_dep_lines(pkg)? {
        let name = line.split_whitespace().next().unwrap_or("");
        if name == on_crate && line.contains("test-fixtures") {
            bail!(
                "production graph of `{pkg}` enables `{on_crate}/test-fixtures` (`{}`) — that feature is \
                 test-only and must stay OFF in any production build",
                line.trim()
            );
        }
    }
    println!("ok: production `{pkg}` does not enable `{on_crate}/test-fixtures`");
    Ok(())
}

/// Every workspace member must opt into the shared lints (`[lints]\nworkspace = true`) — Cargo does
/// NOT inherit them automatically, so a member that forgets the stanza silently escapes
/// `unsafe_code = forbid` and the clippy gate.
fn check_member_lints() -> Result<()> {
    let root = workspace_root();
    let mut manifests = Vec::new();
    for sub in ["crates", "bins"] {
        let dir = root.join(sub);
        if dir.is_dir() {
            for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
                let manifest = entry?.path().join("Cargo.toml");
                if manifest.is_file() {
                    manifests.push(manifest);
                }
            }
        }
    }
    manifests.push(root.join("xtask/Cargo.toml"));
    // The integration-test member (`tests/`) is a workspace member too — it must opt into the shared lints
    // (incl. `unsafe_code = forbid`), exactly like crates/ + bins/ + xtask.
    manifests.push(root.join("tests/Cargo.toml"));

    let mut offenders = Vec::new();
    for manifest in manifests {
        let text = fs::read_to_string(&manifest)
            .with_context(|| format!("reading {}", manifest.display()))?;
        if !lints_opt_in(&text) {
            let rel = manifest.strip_prefix(&root).unwrap_or(&manifest);
            offenders.push(rel.display().to_string());
        }
    }
    if offenders.is_empty() {
        println!("ok: every workspace member opts into [workspace.lints]");
        Ok(())
    } else {
        bail!(
            "these members don't opt into the shared lints (`[lints]` + `workspace = true`): {}",
            offenders.join(", ")
        );
    }
}

/// True iff the manifest opts into the workspace lints — either the section form
/// (`[lints]` then `workspace = true`) or the dotted form (`lints.workspace = true`), both of which
/// Cargo accepts.
fn lints_opt_in(toml: &str) -> bool {
    let mut in_lints = false;
    for line in toml.lines() {
        let t = line.trim();
        let code = t.split('#').next().unwrap_or("").replace(' ', "");
        if t.starts_with('[') {
            in_lints = t == "[lints]";
            continue;
        }
        if code == "lints.workspace=true" || (in_lints && code == "workspace=true") {
            return true;
        }
    }
    false
}

/// The Dockerfile's builder image and `rust-toolchain.toml` must pin the SAME toolchain — otherwise a
/// toolchain bump silently leaves the self-host image building on the old compiler. The Dockerfile tag may
/// be the minor-series alias of the toolchain's full version (`rust:1.96-…` for channel `1.96.0`).
fn check_toolchain_pins() -> Result<()> {
    let root = workspace_root();
    let toolchain_path = root.join("rust-toolchain.toml");
    let toolchain = fs::read_to_string(&toolchain_path)
        .with_context(|| format!("reading {}", toolchain_path.display()))?;
    let channel = toolchain
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("channel")?
                .trim()
                .strip_prefix('=')?
                .trim()
                .strip_prefix('"')?
                .strip_suffix('"')
                .map(str::to_owned)
        })
        .context("rust-toolchain.toml has no `channel = \"…\"` line")?;
    let dockerfile_path = root.join("Dockerfile");
    let dockerfile = fs::read_to_string(&dockerfile_path)
        .with_context(|| format!("reading {}", dockerfile_path.display()))?;
    let tag = dockerfile
        .lines()
        .find_map(|l| {
            let version = l.trim().strip_prefix("FROM rust:")?;
            // `FROM rust:1.96-bookworm AS builder` → the version component before the distro suffix.
            version.split('-').next().map(str::to_owned)
        })
        .context("Dockerfile has no `FROM rust:<version>-…` builder line")?;
    if channel == tag || channel.starts_with(&format!("{tag}.")) {
        println!(
            "ok: Dockerfile builder `rust:{tag}` matches rust-toolchain.toml channel `{channel}`"
        );
    } else {
        bail!(
            "toolchain-pin drift: Dockerfile builds on `rust:{tag}` but rust-toolchain.toml pins \
             `{channel}` — bump them together"
        );
    }
    // The cargo-deny action runs in its own container, which cannot honor rust-toolchain.toml unless the
    // workflow passes the pinned version explicitly (`rust-version:` in ci.yml) — the third leg of the
    // same pin pair. A missing line is drift too: the action would fail on the toolchain override.
    let ci_path = root.join(".github/workflows/ci.yml");
    let ci =
        fs::read_to_string(&ci_path).with_context(|| format!("reading {}", ci_path.display()))?;
    let ci_pin = ci
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("rust-version:")?
                .trim()
                .trim_matches('"')
                .split_whitespace()
                .next()
                .map(str::to_owned)
        })
        .context("ci.yml has no `rust-version: \"…\"` line for the cargo-deny action")?;
    if ci_pin == channel {
        println!("ok: ci.yml cargo-deny `rust-version: {ci_pin}` matches rust-toolchain.toml");
        Ok(())
    } else {
        bail!(
            "toolchain-pin drift: ci.yml pins the cargo-deny toolchain at `{ci_pin}` but \
             rust-toolchain.toml pins `{channel}` — bump them together"
        );
    }
}

/// The architectural invariants the dependency graph must hold — the central trust claims, as a gate.
fn check_arch() -> Result<()> {
    // The client is never an authority and stays a thin SYNC tool: no edge to the server store, SQL, or a
    // SQLite C lib, and no async runtime / async HTTP stack (`ureq` is blocking + self-contained). `tokio`
    // is the load-bearing one — a future `reqwest`/async-`ureq` transport would pull it in without touching
    // `plane-store`/`sqlx`, so the gate must name it explicitly to hold the documented tokio-free line.
    // The openssl/native-tls three hold the prebuilt-binary claim (static on musl, OS-libs-only on
    // macOS, no system cert store): a transitive native-TLS/openssl edge would ship silently — a
    // vendored static openssl links fine — so the gate names them, not just the storage/async bans.
    // The contract-GENERATION machinery is banned too: `topos-types` gates its schemars/utoipa derives
    // behind the default-off `contract-derives` feature (only xtask + topos-plane turn it on), so the
    // client's DTOs stay pure serde — "wire DTOs only, no logic" must hold in the dependency graph.
    assert_excludes(
        "topos",
        &[
            "plane-store",
            "sqlx",
            "libsqlite3-sys",
            "tokio",
            "reqwest",
            "hyper",
            "openssl-sys",
            "native-tls",
            "rustls-native-certs",
            "utoipa",
            "utoipa-gen",
            "schemars",
            "schemars_derive",
        ],
    )?;
    // The kernel stays pure: no wire DTOs, no async/IO/storage/HTTP crates, no diff/merge engines — only
    // crypto primitives. (`diffy`/`imara-diff` are byte execution; they live in `topos-gitstore`.)
    assert_excludes(
        "topos-core",
        &[
            "topos-types",
            "tokio",
            "sqlx",
            "axum",
            "gix",
            "diffy",
            "imara-diff",
            "reqwest",
            "ureq",
            "hyper",
        ],
    )?;
    // The test-only `test-fixtures` feature must never be enabled in a production build: a downstream cloud
    // plane composes the PRODUCTION `topos-plane`, which must not carry `plane-store`'s seed/move/tamper
    // shims NOR its own mailer-injection shim; and the production client must not carry its own
    // `test_support` facade. (The `tests/` member enables them, but it is excluded from the production
    // artifact — `cargo build -p topos-plane`.)
    assert_test_fixtures_off("topos-plane", "plane-store")?;
    assert_test_fixtures_off("topos-plane", "topos-plane")?;
    assert_test_fixtures_off("topos", "topos")?;
    // The leaf crates stay lean: no async runtime, no HTTP stack, no SQL — and the two pure-port leaves
    // carry no git mechanics either (`topos-gitstore` IS the git mechanics crate, so `gix` is its point).
    // These are `--all-features` checks: no feature of a leaf may smuggle a heavy edge in.
    assert_excludes(
        "topos-types",
        &["tokio", "axum", "sqlx", "ureq", "hyper", "gix"],
    )?;
    assert_excludes(
        "topos-harness",
        &["tokio", "axum", "sqlx", "ureq", "hyper", "gix"],
    )?;
    assert_excludes(
        "topos-gitstore",
        &["tokio", "axum", "sqlx", "ureq", "hyper"],
    )?;
    // The vault is pure byte custody: the identity-era stacks cannot even be NAMED by its graph —
    // no OIDC/OAuth client, no HTTP client, no mailer (all `--all-features` checks; the features
    // that once gated them are deleted outright). (`hmac`/`zeroize` stay resolvable only as sqlx's
    // own SCRAM/TLS internals — the vault code names neither.)
    assert_excludes(
        "topos-plane",
        &["oauth2", "openidconnect", "reqwest", "lettre"],
    )?;
    // plane-store shed the credential-mint machinery with the directory: no op-id UUIDs, no
    // signer, no mailer, no OAuth/OIDC client. (`base64`/`hmac` stay resolvable only as sqlx's own
    // wire internals — the vault code names neither.)
    assert_excludes(
        "plane-store",
        &["uuid", "ed25519-dalek", "lettre", "oauth2", "openidconnect"],
    )?;
    // No member silently escapes the shared lint floor (incl. unsafe_code = forbid).
    check_member_lints()?;
    // The self-host image and the workspace must build on the same pinned compiler.
    check_toolchain_pins()?;
    // The vault names no app-schema table in any SQL string.
    check_seam()?;
    // Custody speaks bundles: no `skill` vocabulary anywhere in the vault.
    check_custody_vocabulary()?;
    // The vault is identity-free: no identity vocabulary anywhere in it.
    check_identity_vocabulary()?;
    Ok(())
}

/// The directories the two VOCABULARY gates scan — the whole vault: plane-store (source AND
/// migrations) + the gitstore byte layer. EVERY file, not just `.rs` (a stray non-Rust file cannot
/// slip vocabulary past the gate).
fn vault_vocabulary_dirs() -> Vec<PathBuf> {
    let root = workspace_root();
    vec![
        root.join("crates/plane-store/src"),
        root.join("crates/plane-store/migrations"),
        root.join("crates/topos-gitstore/src"),
    ]
}

/// The words-change boundary gate: the vault speaks BUNDLES — the word `skill` (any case, any
/// position: identifiers, strings, comments) must not appear anywhere in it. The old storage-
/// spelling exemption (`db/custody`'s frozen `skill_*` table names) died with the old schema: the
/// custody tables speak bundles too, so the gate now covers plane-store WHOLE, migrations included.
fn check_custody_vocabulary() -> Result<()> {
    let violations = scan_custody_vocabulary(&vault_vocabulary_dirs())?;
    if violations.is_empty() {
        Ok(())
    } else {
        bail!(
            "custody speaks bundles — `skill` vocabulary in the vault:\n  {}",
            violations.join("\n  ")
        );
    }
}

/// The scan half of [`check_custody_vocabulary`], parameterized over its roots so the red test can
/// point it at a violating temp tree.
fn scan_custody_vocabulary(dirs: &[PathBuf]) -> Result<Vec<String>> {
    let root = workspace_root();
    let mut violations = Vec::new();
    for dir in dirs {
        for file in files_under(dir, "vocabulary gate")? {
            let text =
                fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let shown = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .display()
                .to_string();
            for (n, line) in text.lines().enumerate() {
                if line.to_ascii_lowercase().contains("skill") {
                    violations.push(format!("{shown}:{}: {}", n + 1, line.trim()));
                }
            }
        }
    }
    Ok(violations)
}

/// The IDENTITY vocabulary gate: the vault is identity-free BY VOCABULARY, not just by schema —
/// none of the identity stems may appear anywhere in it (identifiers, strings, comments; matched
/// case-insensitively as word-ish parts, so `EnrollmentGrant`, `enroll_secret`, and a prose
/// "enrollment" all trip it, while `reclaimed` — which merely CONTAINS `claim` — does not).
///
/// The allowlist is deliberately SHORT and explicit — genuine non-identity uses only (git plumbing,
/// a Postgres GUC). Prefer renaming code to allowlisting it: the GC's old `claim` vocabulary was
/// renamed to `acquire` rather than allowlisted.
const IDENTITY_STEMS: [&str; 10] = [
    "email",
    "principal",
    "invit", // invite / invites / invited / invitation(s)
    "claim",
    "enroll",
    "passcode",
    "session",
    "roster",
    "seat",
    "user",
];

/// `(file-path suffix, full lowercase token)` pairs the identity gate allows. Each entry is a
/// genuine non-identity use that cannot be renamed away:
/// - the gitstore writes git COMMITTER signatures, and git's signature format requires an email
///   field (constant plumbing bytes, never a person);
/// - the pool applies the Postgres `idle_in_transaction_session_timeout` GUC — a server-defined
///   identifier ("session" here is a database connection, not a login).
const IDENTITY_ALLOWLIST: [(&str, &str); 5] = [
    ("crates/topos-gitstore/src/store.rs", "email"),
    (
        "crates/topos-gitstore/src/store.rs",
        "topos_committer_email",
    ),
    ("crates/topos-gitstore/src/tests.rs", "email"),
    (
        "crates/plane-store/src/db/mod.rs",
        "idle_in_transaction_session_timeout",
    ),
    (
        "crates/plane-store/src/authority.rs",
        "idle_in_transaction_session_timeout",
    ),
];

fn check_identity_vocabulary() -> Result<()> {
    let violations = scan_identity_vocabulary(&vault_vocabulary_dirs())?;
    if violations.is_empty() {
        Ok(())
    } else {
        bail!(
            "the vault is identity-free — identity vocabulary found:\n  {}",
            violations.join("\n  ")
        );
    }
}

/// The scan half of [`check_identity_vocabulary`], parameterized over its roots so the red test can
/// point it at a violating temp tree. Tokenizes each line into `[A-Za-z0-9_]+` runs, splits each
/// token on `_`, lowercases, and flags any part that STARTS WITH an identity stem — unless the
/// whole token is allowlisted for that file.
fn scan_identity_vocabulary(dirs: &[PathBuf]) -> Result<Vec<String>> {
    let root = workspace_root();
    let mut violations = Vec::new();
    for dir in dirs {
        for file in files_under(dir, "identity vocabulary gate")? {
            let text =
                fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let shown = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .display()
                .to_string();
            // The allowlist keys on a path SUFFIX so the scan works from any checkout root (and on
            // the red test's temp tree, which allowlists nothing).
            let path_str = file.to_string_lossy().replace('\\', "/");
            for (n, line) in text.lines().enumerate() {
                for token in line
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .filter(|t| !t.is_empty())
                {
                    let token_lower = token.to_ascii_lowercase();
                    let allowlisted = IDENTITY_ALLOWLIST
                        .iter()
                        .any(|(suffix, tok)| path_str.ends_with(suffix) && token_lower == *tok);
                    if allowlisted {
                        continue;
                    }
                    let hit = token_lower
                        .split('_')
                        .any(|part| IDENTITY_STEMS.iter().any(|stem| part.starts_with(stem)));
                    if hit {
                        violations.push(format!("{shown}:{}: `{token}`", n + 1));
                    }
                }
            }
        }
    }
    Ok(violations)
}

/// The schema-boundary gate: the vault's SQL names NO app-schema table. The directory left this
/// crate for the app's own schema, so the old witness-seam scan is repurposed: every table of the
/// app schema (and the retired plane spellings) is banned after a FROM / JOIN / INTO / UPDATE token
/// in ANY file under `crates/plane-store/src` — only the vault's own tables (`version`,
/// `current_pointer`, `upload`, `version_object`, `version_digest`, `object_presence`,
/// `promotion_lease`, `promotion_lease_object`, `tombstones`) may follow one.
fn check_seam() -> Result<()> {
    let root = workspace_root();
    let violations = scan_seam(&[root.join("crates/plane-store/src")])?;
    if violations.is_empty() {
        Ok(())
    } else {
        bail!(
            "the vault names an app-schema table in SQL:\n  {}",
            violations.join("\n  ")
        );
    }
}

/// The app-schema tables (plus retired plane-era spellings) the vault must never touch. `version`
/// and `upload` are deliberately ABSENT — they are the vault's own tables.
const APP_SCHEMA_TABLES: [&str; 26] = [
    "user",
    "session",
    "account",
    "verification",
    "device",
    "device_auth_session",
    "workspace",
    "seat",
    "invitation",
    "bundle",
    "bundle_name_hint",
    "channel",
    "channel_member",
    "channel_optout",
    "channel_bundle",
    "bundle_subscription",
    "bundle_detachment",
    "device_exclusion",
    "device_bundle_state",
    "notice",
    "proposal",
    "approval",
    "proposal_comment",
    "audit_event",
    "op_receipt",
    "op_receipts",
];

/// The scan half of [`check_seam`], parameterized over its roots so the red test can point it at a
/// violating temp tree. Tokenizes the WHOLE file as one whitespace-normalized stream, so an
/// introducer and its table split across lines (`FROM\n  workspace`) can never evade the gate.
fn scan_seam(dirs: &[PathBuf]) -> Result<Vec<String>> {
    const SQL_INTRODUCERS: [&str; 4] = ["from", "join", "into", "update"];
    let root = workspace_root();
    let mut violations = Vec::new();
    for dir in dirs {
        for file in files_under(dir, "seam check")? {
            if file.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text =
                fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let shown = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .display()
                .to_string();
            let tokens: Vec<&str> = text
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .filter(|t| !t.is_empty())
                .collect();
            for pair in tokens.windows(2) {
                let introducer = pair[0].to_ascii_lowercase();
                let following = pair[1].to_ascii_lowercase();
                if SQL_INTRODUCERS.contains(&introducer.as_str())
                    && APP_SCHEMA_TABLES.contains(&following.as_str())
                {
                    violations.push(format!(
                        "{shown}: SQL `{} {}` names an app-schema table",
                        pair[0], pair[1]
                    ));
                }
            }
        }
    }
    Ok(violations)
}

/// Every regular file under `dir`, recursively (deterministic order). An absent dir is a gate
/// failure attributed to `gate` — a scan must never silently pass because the tree moved out from
/// under it.
fn files_under(dir: &Path, gate: &str) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    if !dir.is_dir() {
        bail!("{gate}: expected gated dir {} to exist", dir.display());
    }
    while let Some(d) = stack.pop() {
        let mut entries: Vec<PathBuf> = fs::read_dir(&d)
            .with_context(|| format!("reading {}", d.display()))?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<_>>()?;
        entries.sort();
        for path in entries {
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    Ok(out)
}

/// Run a cargo subcommand from the workspace root as a gate step, with optional extra env.
fn cargo_gate(args: &[&str], envs: &[(&str, &str)]) -> Result<()> {
    let mut cmd = Command::new(env!("CARGO"));
    cmd.current_dir(workspace_root()).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let status = cmd
        .status()
        .with_context(|| format!("running `cargo {}`", args.join(" ")))?;
    if status.success() {
        Ok(())
    } else {
        bail!("`cargo {}` failed", args.join(" "));
    }
}

/// One named `ci` gate: its banner label + the closure that runs it.
type Gate = (&'static str, Box<dyn FnOnce() -> Result<()>>);

/// `cargo xtask ci` — the full NON-DB gate sequence, in the same order CI runs it, failing fast at the
/// first red gate. One local command == the CI `gate` job, so a contributor's pre-push loop matches CI
/// exactly. The DB-backed gates still run separately: `cargo test --workspace` (needs a Postgres via
/// `DATABASE_URL`), `cargo deny check` (needs cargo-deny), and the sqlx offline-metadata drift job.
fn ci() -> Result<()> {
    let gates: Vec<Gate> = vec![
        (
            "format (cargo fmt --all --check)",
            Box::new(|| cargo_gate(&["fmt", "--all", "--check"], &[])),
        ),
        (
            "clippy (warnings are errors)",
            Box::new(|| {
                cargo_gate(
                    &[
                        "clippy",
                        "--workspace",
                        "--all-targets",
                        "--locked",
                        "--",
                        "-D",
                        "warnings",
                    ],
                    &[],
                )
            }),
        ),
        (
            "docs (rustdoc warnings are errors)",
            Box::new(|| {
                cargo_gate(
                    &["doc", "--workspace", "--no-deps", "--locked"],
                    &[("RUSTDOCFLAGS", "-D warnings")],
                )
            }),
        ),
        (
            "contract drift gate (schemas + openapi)",
            Box::new(|| gen_schema(true)),
        ),
        (
            "contract drift gate (fixtures)",
            Box::new(|| gen_fixtures(true)),
        ),
        (
            "contract drift gate (cli reference)",
            Box::new(|| gen_cli_ref(true)),
        ),
        ("architectural layering", Box::new(check_arch)),
    ];
    let total = gates.len();
    for (i, (name, run)) in gates.into_iter().enumerate() {
        println!("\n=== ci gate {}/{total}: {name} ===", i + 1);
        run().with_context(|| format!("ci gate {}/{total} FAILED: {name}", i + 1))?;
    }
    println!("\n=== ci: all {total} gates green ===");
    println!(
        "(not covered here: `cargo test --workspace` [needs DATABASE_URL], `cargo deny check`, the sqlx offline-metadata drift job)"
    );
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let check = args.iter().any(|a| a == "--check");
    match cmd {
        "gen-schema" => gen_schema(check)?,
        "gen-fixtures" => gen_fixtures(check)?,
        "gen-cli-ref" => gen_cli_ref(check)?,
        "check-arch" => check_arch()?,
        "check-registry-drift" => registry_drift::run()?,
        "ci" => ci()?,
        "conformance" => println!("conformance: not yet implemented"),
        "dist" => dist::run(&args[1..])?,
        _ => {
            eprintln!(
                "usage: cargo xtask <gen-schema [--check] | gen-fixtures [--check] | gen-cli-ref [--check] | check-arch | check-registry-drift | ci | conformance | dist …>"
            );
            std::process::exit(2);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a throwaway tree of files for a gate scan (RAII cleanup).
    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str, files: &[(&str, &str)]) -> Self {
            let root = std::env::temp_dir().join(format!(
                "topos-xtask-{tag}-{}-{:x}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            let _ = fs::remove_dir_all(&root);
            for (path, content) in files {
                let full = root.join(path);
                fs::create_dir_all(full.parent().expect("a parent")).expect("mkdir");
                fs::write(&full, content).expect("write");
            }
            Self { root }
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// RED test: the custody vocabulary gate FIRES on a `skill` spelling (any case, any file kind)
    /// — and stays quiet on a clean tree. The real-tree run is `check_custody_vocabulary` in
    /// check-arch; this proves the scan itself has teeth.
    #[test]
    fn custody_vocabulary_gate_fires_on_skill_vocabulary() {
        let dirty = TempTree::new(
            "custody-dirty",
            &[
                ("src/a.rs", "// a Skill is a bundle\nfn skill_id() {}\n"),
                ("migrations/0001.sql", "CREATE TABLE bundles (id text);\n"),
            ],
        );
        let hits =
            scan_custody_vocabulary(&[dirty.root.join("src"), dirty.root.join("migrations")])
                .expect("scan runs");
        assert_eq!(hits.len(), 2, "both skill lines fire: {hits:?}");

        let clean = TempTree::new(
            "custody-clean",
            &[("src/a.rs", "// a bundle is a bundle\nfn bundle_id() {}\n")],
        );
        let hits = scan_custody_vocabulary(&[clean.root.join("src")]).expect("scan runs");
        assert!(hits.is_empty(), "a clean tree passes: {hits:?}");
    }

    /// RED test: the identity vocabulary gate FIRES on identity stems (word-ish: identifier parts
    /// and prose alike, any case, derived forms included) — while `reclaimed`, which merely
    /// CONTAINS `claim`, stays clean, and an allowlisted token passes only in its allowlisted file.
    #[test]
    fn identity_vocabulary_gate_fires_wordish_and_honors_the_allowlist() {
        let dirty = TempTree::new(
            "identity-dirty",
            &[(
                "src/a.rs",
                "struct EnrollmentGrant;\nfn seat_user(email: &str) {}\n// the invitation session\n",
            )],
        );
        let hits = scan_identity_vocabulary(&[dirty.root.join("src")]).expect("scan runs");
        // EnrollmentGrant (enroll…) + seat_user (seat, user) + email + invitation + session.
        assert!(hits.len() >= 5, "the identity stems fire: {hits:?}");

        let clean = TempTree::new(
            "identity-clean",
            &[(
                "src/a.rs",
                "// the reclaimed bytes are acquired by the sweep\nfn acquire_for_delete() {}\n",
            )],
        );
        let hits = scan_identity_vocabulary(&[clean.root.join("src")]).expect("scan runs");
        assert!(
            hits.is_empty(),
            "`reclaimed`/`acquire` never trip the claim stem: {hits:?}"
        );

        // The allowlist keys on (file suffix, token): the same token OUTSIDE its allowlisted file fires.
        let misplaced = TempTree::new(
            "identity-misplaced",
            &[(
                "src/other.rs",
                "const T: &str = \"idle_in_transaction_session_timeout\";\n",
            )],
        );
        let hits = scan_identity_vocabulary(&[misplaced.root.join("src")]).expect("scan runs");
        assert_eq!(hits.len(), 1, "an un-allowlisted file fires: {hits:?}");
    }

    /// RED test: the seam gate FIRES when SQL names an app-schema table after FROM/JOIN/INTO/UPDATE
    /// (across lines too) — while the vault's own tables stay clean.
    #[test]
    fn seam_gate_fires_on_app_schema_tables() {
        let dirty = TempTree::new(
            "seam-dirty",
            &[(
                "src/a.rs",
                "const Q: &str = \"SELECT 1 FROM\n  workspace WHERE id = $1\";\n",
            )],
        );
        let hits = scan_seam(&[dirty.root.join("src")]).expect("scan runs");
        assert_eq!(hits.len(), 1, "the split-line app table fires: {hits:?}");

        let clean = TempTree::new(
            "seam-clean",
            &[(
                "src/a.rs",
                "const Q: &str = \"SELECT 1 FROM version JOIN version_object USING (version_id)\";\n",
            )],
        );
        let hits = scan_seam(&[clean.root.join("src")]).expect("scan runs");
        assert!(hits.is_empty(), "the vault's own tables pass: {hits:?}");
    }

    /// The REAL tree stays clean under all three gates (the same calls check-arch runs).
    #[test]
    fn the_real_tree_passes_the_gates() {
        check_custody_vocabulary().expect("custody vocabulary clean");
        check_identity_vocabulary().expect("identity vocabulary clean");
        check_seam().expect("seam clean");
    }
}
