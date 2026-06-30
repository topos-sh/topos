//! `xtask` — the one codegen + invariant-gate entrypoint.
//!
//! `cargo xtask gen-schema`          → (re)generate `contracts/schemas/*.schema.json` from topos-types.
//! `cargo xtask gen-schema --check`  → the CI drift gate (stale / missing / orphan schemas all fail).
//! `cargo xtask gen-fixtures`        → (re)generate the golden `--json` fixtures under `contracts/fixtures/`.
//! `cargo xtask gen-fixtures --check`→ the fixture drift gate.
//! `cargo xtask check-arch`          → the architectural-layering + lint-opt-in gate.
//! `cargo xtask conformance`         → the store matrices (not yet implemented).
//!
//! `gen-schema` also (re)generates + checks the plane OpenAPI (`contracts/openapi/openapi.json`, from
//! `topos_plane::openapi()`) under the same drift discipline. There is no formal-model subcommand — the
//! integration interleaving tests are the correctness net.

use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeSet,
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

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
            "signed-current-record",
            emit(schemars::schema_for!(topos_types::SignedCurrentRecord)),
        ),
        (
            "next-action",
            emit(schemars::schema_for!(topos_types::NextAction)),
        ),
        (
            "trigger-report",
            emit(schemars::schema_for!(topos_types::TriggerReport)),
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
            "follow-data",
            emit(schemars::schema_for!(topos_types::results::FollowData)),
        ),
        (
            "unfollow-data",
            emit(schemars::schema_for!(topos_types::results::UnfollowData)),
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
            "review-data",
            emit(schemars::schema_for!(topos_types::results::ReviewData)),
        ),
        (
            "invite-data",
            emit(schemars::schema_for!(topos_types::results::InviteData)),
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
    use topos_types::persisted::ConflictPathKind;
    use topos_types::results::{
        AddData, ConflictPathReport, DiffData, DiffSource, ListData, LogData, MergeReport,
        PullAction, PullData, PullSkill, SkillEntry,
    };
    use topos_types::{
        ActionCode, Affected, Generation, JsonEnvelope, NextAction, Receipt, TerminalOutcome,
        WireError,
    };

    let argv = |parts: &[&str]| parts.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

    // The deterministic identity of the committed `tests/fixtures/pr-describe` skill (device id
    // `d_test`, the fixed adopt message) — the local verbs reproduce these byte-for-byte.
    let fx_version = "d77b648d8149d63189864c6b6d06da4f7919935c4242cc197e708b1dafe941d5";
    let fx_digest = "c35004153b0f72e2e8363b557f36594319d5382eb9e4c7add5ff0feb3b15c369";

    // `add` of the fixture skill (offline; no plane op, so no receipt).
    let add_ok = JsonEnvelope {
        schema_version: 1,
        command: "add".to_owned(),
        ok: true,
        data: serde_json::to_value(AddData {
            skill_id: "topos_t00".to_owned(),
            name: "pr-describe".to_owned(),
            version_id: fx_version.to_owned(),
            bundle_digest: fx_digest.to_owned(),
            tracked: true,
            // The fixture skill is adopted from a plain dir (not under ~/.claude), so it is recognized as
            // no harness and arms no currency — both omit from the envelope.
            harness: None,
            currency: None,
        })
        .expect("AddData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // A clean `pull` that found one followed skill already current.
    let pull_ok = JsonEnvelope {
        schema_version: 1,
        command: "pull".to_owned(),
        ok: true,
        data: serde_json::to_value(PullData {
            skills: vec![PullSkill {
                skill: "pr-describe".to_owned(),
                observed: Generation { epoch: 1, seq: 42 },
                applied: Generation { epoch: 1, seq: 42 },
                action: PullAction::UpToDate,
                offer: None,
                conflict: None,
                merge: None,
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
            skills: vec![PullSkill {
                skill: "pr-describe".to_owned(),
                observed: Generation { epoch: 1, seq: 7 },
                applied: Generation { epoch: 1, seq: 7 },
                action: PullAction::Merged,
                offer: None,
                conflict: None,
                merge: Some(MergeReport {
                    base_version_id: fx_version.to_owned(),
                    theirs_version_id: fx_digest.to_owned(),
                    result_version_id: fx_merged.to_owned(),
                    result_digest: fx_digest.to_owned(),
                    clean: true,
                    conflicts: vec![],
                    drop_diff: None,
                }),
            }],
            proposals_awaiting: 0,
        })
        .expect("PullData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // A `pull` whose merge conflicted → a complete conflict tree on disk + publish blocked until resolved.
    let pull_conflicted = JsonEnvelope {
        schema_version: 1,
        command: "pull".to_owned(),
        ok: true,
        data: serde_json::to_value(PullData {
            skills: vec![PullSkill {
                skill: "pr-describe".to_owned(),
                observed: Generation { epoch: 1, seq: 7 },
                applied: Generation { epoch: 1, seq: 7 },
                action: PullAction::Conflicted,
                offer: None,
                conflict: None,
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
                }),
            }],
            proposals_awaiting: 0,
        })
        .expect("PullData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // `list` after adopting the fixture skill — one tracked skill, no draft.
    let list_ok = JsonEnvelope {
        schema_version: 1,
        command: "list".to_owned(),
        ok: true,
        data: serde_json::to_value(ListData {
            tracked: vec![SkillEntry {
                skill: "pr-describe".to_owned(),
                version_id: fx_version.to_owned(),
                bundle_digest: fx_digest.to_owned(),
                draft: false,
                pending_proposals: vec![],
            }],
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
            bundle_digest: fx_digest.to_owned(),
            diff: "--- a/SKILL.md\n+++ b/SKILL.md\n@@ -4,4 +4,4 @@\n \n # PR describe\n \n-Write a clear PR description.\n+Write a GREAT PR description.\n".to_owned(),
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
        })
        .expect("LogData serializes"),
        warnings: vec![],
        next_actions: vec![],
        receipt: None,
        error: None,
    };

    // A direct `publish` refused under review-required: uploads/opens nothing, carries the propose argv.
    let propose_action = NextAction {
        code: ActionCode::ProposePublish,
        argv: argv(&["topos", "publish", "pr-describe", "--propose"]),
    };
    let publish_approval_required = JsonEnvelope {
        schema_version: 1,
        command: "publish".to_owned(),
        ok: false,
        data: serde_json::json!({}),
        warnings: vec![],
        next_actions: vec![propose_action.clone()],
        receipt: Some(Receipt {
            schema_version: 1,
            op_id: "f47ac10b-58cc-4372-a567-0e02b2c3d479".to_owned(),
            command: "publish".to_owned(),
            outcome: TerminalOutcome::ApprovalRequired,
            workspace_id: "w_demo".to_owned(),
            skill_id: Some("s_prdescribe".to_owned()),
            version_id: None,
            bundle_digest: None,
            expected_generation: None,
            current_generation: Some(Generation { epoch: 1, seq: 42 }),
            created_at: "2026-06-25T00:00:00Z".to_owned(),
            key_id: Some("pk_demo".to_owned()),
            details: None,
        }),
        error: Some(WireError {
            code: "REVIEW_REQUIRED".to_owned(),
            outcome: TerminalOutcome::ApprovalRequired,
            retryable: false,
            affected: Affected {
                skill: Some("pr-describe".to_owned()),
                ..Default::default()
            },
            expected_generation: None,
            current_generation: None,
            context: serde_json::json!({}),
            next_actions: vec![propose_action],
        }),
    };

    // A publish that lost the race — the team moved current; rebase and retry.
    let publish_conflict = JsonEnvelope {
        schema_version: 1,
        command: "publish".to_owned(),
        ok: false,
        data: serde_json::json!({}),
        warnings: vec![],
        next_actions: vec![NextAction {
            code: ActionCode::RebaseAndRetry,
            argv: argv(&["topos", "publish", "pr-describe"]),
        }],
        receipt: Some(Receipt {
            schema_version: 1,
            op_id: "9f1b8c2e-7a6d-4e3f-9b0a-1c2d3e4f5a6b".to_owned(),
            command: "publish".to_owned(),
            outcome: TerminalOutcome::Conflict,
            workspace_id: "w_demo".to_owned(),
            skill_id: Some("s_prdescribe".to_owned()),
            version_id: None,
            bundle_digest: None,
            expected_generation: Some(Generation { epoch: 1, seq: 42 }),
            current_generation: Some(Generation { epoch: 1, seq: 43 }),
            created_at: "2026-06-25T00:00:00Z".to_owned(),
            key_id: Some("pk_demo".to_owned()),
            details: None,
        }),
        error: Some(WireError {
            code: "STALE_BASE".to_owned(),
            outcome: TerminalOutcome::Conflict,
            retryable: true,
            affected: Affected {
                skill: Some("pr-describe".to_owned()),
                ..Default::default()
            },
            expected_generation: Some(Generation { epoch: 1, seq: 42 }),
            current_generation: Some(Generation { epoch: 1, seq: 43 }),
            context: serde_json::json!({}),
            next_actions: vec![NextAction {
                code: ActionCode::RebaseAndRetry,
                argv: argv(&["topos", "publish", "pr-describe"]),
            }],
        }),
    };

    vec![
        ("json/pull.ok", emit_json(&pull_ok)),
        ("json/pull.merged", emit_json(&pull_merged)),
        ("json/pull.conflicted", emit_json(&pull_conflicted)),
        ("json/add.ok", emit_json(&add_ok)),
        ("json/list.ok", emit_json(&list_ok)),
        ("json/diff.ok", emit_json(&diff_ok)),
        ("json/log.ok", emit_json(&log_ok)),
        (
            "json/publish.approval-required",
            emit_json(&publish_approval_required),
        ),
        ("json/publish.conflict", emit_json(&publish_conflict)),
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

/// The architectural invariants the dependency graph must hold — the central trust claims, as a gate.
fn check_arch() -> Result<()> {
    // The client is never an authority: no edge to the server store, SQL, or a SQLite C lib.
    assert_excludes("topos", &["plane-store", "sqlx", "libsqlite3-sys"])?;
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
    // No member silently escapes the shared lint floor (incl. unsafe_code = forbid).
    check_member_lints()?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let check = args.iter().any(|a| a == "--check");
    match cmd {
        "gen-schema" => gen_schema(check)?,
        "gen-fixtures" => gen_fixtures(check)?,
        "check-arch" => check_arch()?,
        "conformance" => println!("conformance: not yet implemented"),
        _ => {
            eprintln!(
                "usage: cargo xtask <gen-schema [--check] | gen-fixtures [--check] | check-arch | conformance>"
            );
            std::process::exit(2);
        }
    }
    Ok(())
}
