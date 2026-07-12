//! Two presentations of one typed outcome: the `--json` envelope (the agent surface) and a thin TTY
//! renderer. Error messages are summarized so a raw git/io string never reaches a user surface.

use topos_types::bootstrap::VerifiedDomainStatus;
use topos_types::persisted::ConflictPathKind;
use topos_types::requests::InvitationData;
use topos_types::results::{
    AddData, AddedNote, DiffData, FollowData, LogData, ProposeData, PublishData, PullData,
    PullSkill, RemoteFollowState, RemoteSkillEntry, RemoveData, RemoveItem, RemoveKind, RevertData,
    ReviewData, ReviewDecision, SkillEntry, UntrackedEntry,
};
use topos_types::{
    ActionCode, Affected, CurrencyKind, JsonEnvelope, NextAction, TerminalOutcome, TriggerState,
    WIRE_SCHEMA_VERSION, WireError,
};

use crate::error::ClientError;
use crate::ops::ListOutcome;

/// A success envelope wrapping a verb's typed `data`.
pub(crate) fn ok_envelope(command: &str, data: serde_json::Value) -> JsonEnvelope {
    JsonEnvelope {
        schema_version: WIRE_SCHEMA_VERSION,
        command: command.to_owned(),
        ok: true,
        data,
        warnings: Vec::new(),
        next_actions: Vec::new(),
        receipt: None,
        error: None,
    }
}

/// A failure envelope carrying the stable code, outcome, and machine-actionable next steps. An
/// [`ClientError::AmbiguousTarget`] additionally surfaces its paste-ready qualified paths as
/// `data.candidates` — the machine-readable half of the ambiguity refusal (the human list rides
/// the message).
pub(crate) fn err_envelope(command: &str, err: &ClientError) -> JsonEnvelope {
    let outcome = err.outcome();
    let next_actions = next_actions(err);
    let retryable = matches!(
        outcome,
        TerminalOutcome::RetryableFailure | TerminalOutcome::Unavailable
    );
    let data = match err {
        ClientError::AmbiguousTarget { candidates, .. } => {
            serde_json::json!({ "candidates": candidates })
        }
        _ => serde_json::json!({}),
    };
    JsonEnvelope {
        schema_version: WIRE_SCHEMA_VERSION,
        command: command.to_owned(),
        ok: false,
        data,
        warnings: Vec::new(),
        next_actions: next_actions.clone(),
        receipt: None,
        error: Some(WireError {
            code: err.code().to_owned(),
            outcome,
            retryable,
            affected: Affected::default(),
            expected_generation: None,
            // A CONFLICT carries the live generation (the rebase target); every other error has none.
            current_generation: err.current_generation(),
            context: serde_json::json!({ "message": safe_message(err) }),
            next_actions,
        }),
    }
}

fn next_actions(err: &ClientError) -> Vec<NextAction> {
    match err {
        // Every "look at the discovered inventory to resolve this" error points the agent at `list` — the
        // ambiguity shapes plus the not-found cases from `add <skill>` name resolution.
        ClientError::AmbiguousName { .. }
        | ClientError::AmbiguousHarness { .. }
        | ClientError::AmbiguousScope { .. }
        | ClientError::NoUntrackedSkill { .. }
        | ClientError::HarnessNotFound(_) => vec![NextAction {
            code: ActionCode::DisambiguateName,
            argv: vec!["topos".into(), "list".into(), "--json".into()],
        }],
        // A stale base — update to rebase, then re-show the diff and retry. Never a silent retry.
        ClientError::Conflict { skill, .. } => vec![NextAction {
            code: ActionCode::RebaseAndRetry,
            argv: vec![
                "topos".into(),
                "update".into(),
                skill.clone(),
                "--json".into(),
            ],
        }],
        // An unresolved author merge blocks publish — resolve it (the update surfaces/runs the resolution).
        ClientError::PublishBlocked { skill } => vec![NextAction {
            code: ActionCode::ResolveDivergedDraft,
            argv: vec![
                "topos".into(),
                "update".into(),
                skill.clone(),
                "--json".into(),
            ],
        }],
        // A denial is not self-service (ask an owner to invite/roster you, or contact an admin) — the
        // codes carry no executable argv.
        ClientError::Denied(_) => vec![
            NextAction {
                code: ActionCode::RequestAccess,
                argv: Vec::new(),
            },
            NextAction {
                code: ActionCode::ContactAdmin,
                argv: Vec::new(),
            },
        ],
        // A denied enrollment redeem (authenticated-but-uninvited): the ask-an-owner guidance rides the
        // message; the action code is the existing REQUEST_ACCESS (no argv — the fix is another human's).
        ClientError::RedeemDenied { .. } => vec![NextAction {
            code: ActionCode::RequestAccess,
            argv: Vec::new(),
        }],
        // A retryable plane outcome (e.g. a not-yet-committed lease) — re-run the same command. The agent
        // owns the argv (this surface doesn't carry the verb); a permanent one carries no Retry.
        ClientError::PlaneTerminal {
            retryable: true, ..
        } => vec![NextAction {
            code: ActionCode::Retry,
            argv: Vec::new(),
        }],
        // `topos upgrade` disambiguates to two concrete commands the agent can pick between.
        ClientError::UpgradeAmbiguous => vec![
            NextAction {
                code: ActionCode::from("UPDATE_SKILLS".to_owned()),
                argv: vec!["topos".into(), "update".into(), "--json".into()],
            },
            NextAction {
                code: ActionCode::from("UPDATE_CLI".to_owned()),
                argv: vec!["topos".into(), "self-update".into()],
            },
        ],
        _ => Vec::new(),
    }
}

/// The success-path next actions for `follow`: a pending enrollment ⇒ re-invoke `follow` (re-invoking IS
/// the resume — the pending WAL drives it); a completed enrollment that disclosed offers ⇒ `update` to
/// surface/place them.
pub(crate) fn follow_next_actions(data: &FollowData) -> Vec<NextAction> {
    if data.pending.is_some() {
        return vec![NextAction {
            // An OPEN action code (carries the executable argv); no schema change to the closed set.
            code: ActionCode::from("ENROLL_RESUME".to_owned()),
            argv: vec!["topos".into(), "follow".into(), "--json".into()],
        }];
    }
    if data.enrolled && !data.skills.is_empty() {
        return vec![NextAction {
            code: ActionCode::ApplyWaitingUpdate,
            argv: vec!["topos".into(), "update".into(), "--json".into()],
        }];
    }
    Vec::new()
}

/// A clean, leak-free summary for a user surface — variants whose `Display` could embed a raw serde / io
/// / git string or a host path get a fixed message. The inner detail is NOT lost: every top-level error
/// path appends the full `Display` chain ([`ClientError::detail`]) to the append-only diagnostics log
/// (`~/.topos/log.jsonl`) and prints it on stderr under `TOPOS_DEBUG=1`; the TTY error line points there
/// (`details: …`).
pub(crate) fn safe_message(err: &ClientError) -> String {
    match err {
        ClientError::Io(_) | ClientError::IoKind { .. } => {
            "a filesystem operation failed".to_owned()
        }
        ClientError::Gitstore(_) => "the embedded git store reported an error".to_owned(),
        ClientError::Verify(_) => "an integrity check failed".to_owned(),
        ClientError::Corrupt(_) => "a sidecar document is corrupt".to_owned(),
        ClientError::WireInvalid(_) => "the plane's response failed validation".to_owned(),
        ClientError::Scan(_) => "the skill directory was rejected".to_owned(),
        // The remaining Display strings are fixed text, a user-supplied name, or (InvalidArgument)
        // usage guidance written by this code — safe to show verbatim.
        other => other.to_string(),
    }
}

/// Serialize an envelope as one line of JSON (stdout; diagnostics go to stderr).
pub(crate) fn to_json(envelope: &JsonEnvelope) -> String {
    serde_json::to_string(envelope).unwrap_or_else(|_| "{\"ok\":false}".to_owned())
}

pub(crate) fn add_tty(data: &AddData) -> String {
    let mut out = format!(
        "Adopted '{}' ({}) @ {}",
        data.name,
        data.skill_id,
        short(&data.version_id)
    );
    // Provenance of a remote import (honest, never a trust claim) — where the bytes came from + license.
    if let Some(o) = &data.origin {
        out.push_str("\nImported from ");
        out.push_str(&o.source);
        if let Some(c) = &o.commit {
            out.push('@');
            out.push_str(c);
        }
        if let Some(sub) = &o.subdir {
            out.push_str(" (");
            out.push_str(sub);
            out.push(')');
        }
        match o.license.as_deref() {
            Some(lic) => {
                out.push_str(" · ");
                out.push_str(lic);
            }
            None => out.push_str(" · no license found"),
        }
    }
    // Disclose the one write `add` makes outside ~/.topos/ — the currency trigger — honestly (it is
    // plumbing: it runs a no-op `update` until something is followed; never "it auto-updates"). The copy
    // branches on the report's `currency_kind` so a harness's honest update moment is never overstated
    // (a session-start hook fires at session start; an inject surface only on the first `topos` touch).
    if let Some(report) = &data.currency {
        out.push_str(match (report.state, report.currency_kind) {
            (TriggerState::Active, CurrencyKind::SessionStart) => {
                "\nInstalled the session-start currency hook (runs `topos update` at session start)."
            }
            (TriggerState::Active, CurrencyKind::FirstToposTouch) => {
                "\nInstalled the currency trigger (updates surface on the first `topos` touch)."
            }
            (TriggerState::Active, CurrencyKind::FirstTurn) => {
                "\nInstalled the currency trigger (updates surface on the first turn)."
            }
            (TriggerState::Active, CurrencyKind::ExplicitPullOnly) => {
                "\nNo automatic currency trigger — run `topos update` to check for updates."
            }
            (TriggerState::AlreadyPresentUnmanaged, CurrencyKind::SessionStart) => {
                "\nLeft your existing session-start currency hook untouched."
            }
            (TriggerState::AlreadyPresentUnmanaged, _) => {
                "\nLeft your existing currency trigger untouched."
            }
            (TriggerState::Degraded, CurrencyKind::SessionStart) => {
                "\nCouldn't update settings.json for the currency hook — left it untouched."
            }
            (TriggerState::Degraded, _) => {
                "\nCouldn't update the harness config for the currency trigger — left it untouched; run `topos update` to check for updates."
            }
            (TriggerState::Inactive, _) => "",
        });
    }
    out
}

/// The `keep-as-yours` DESCRIBE's TTY: what the retained copy is, why the team copy is gone, and that
/// `--yes` re-adopts it as a NEW local skill with no upstream (the draft rides along). Nothing has changed.
pub(crate) fn keep_as_yours_describe_tty(
    data: &topos_types::results::KeepAsYoursData,
    yes_argv: &[String],
) -> String {
    use topos_types::results::KeepReason;
    let why = match data.reason {
        KeepReason::WithdrawnUpstream => {
            "the team withdrew it (archived, or its last channel dropped it)"
        }
        KeepReason::Detached => "you unfollowed it",
        KeepReason::RemovedHere => "you removed it from this device",
    };
    let mut s = format!("'{}' — {}. Its bytes are kept locally.\n", data.name, why);
    s.push_str(
        "Keep it as yours: re-adopt the copy as a NEW local skill with no upstream (the team copy stays \
         gone; nothing syncs it any more)",
    );
    if data.has_draft {
        s.push_str(" — your local draft rides along");
    }
    s.push_str(".\n");
    s.push_str(&format!("  {}", argv_line(yes_argv)));
    s
}

/// The per-row next-actions a bare `update` surfaces: each WITHDRAWN skill points at the `keep-as-yours`
/// re-fork (`topos add <name> --yes`), so the agent has the paste-ready salvage command inline.
pub(crate) fn withdrawn_next_actions(data: &PullData) -> Vec<NextAction> {
    data.skills
        .iter()
        .filter(|s| matches!(s.action, topos_types::results::PullAction::Withdrawn))
        .map(|s| NextAction {
            code: ActionCode::from("KEEP_AS_YOURS".to_owned()),
            argv: vec![
                "topos".to_owned(),
                "add".to_owned(),
                s.skill.clone(),
                "--yes".to_owned(),
            ],
        })
        .collect()
}

pub(crate) fn list_tty(out: &ListOutcome) -> String {
    let data = &out.data;
    let mut s = String::new();
    // The enrollment header — the "am I enrolled, is the hook armed" disclosure. The workspace names move
    // to the per-group headers below (one install can follow skills across several workspaces). Rendered
    // only when enrolled; the unenrolled output is byte-identical to the accountless local list.
    if let Some(e) = &out.enrollment {
        s.push_str(&format!(
            "Enrolled at {} — currency hook: {}\n",
            e.base_url,
            if e.hook_active {
                "active"
            } else {
                "not installed"
            }
        ));
    }
    // The follow-state note `(mode, following)` for tracked row `i` (aligned by construction), present only
    // when enrolled+followed — extracted as plain fields so the row builder stays type-agnostic.
    let note_of = |i: usize| {
        out.enrollment
            .as_ref()
            .and_then(|en| en.notes.get(i))
            .and_then(Option::as_ref)
            .map(|n| (n.mode, n.following))
    };
    // Tracked skills. An empty inventory still falls through to the untracked discovery below — a fresh
    // user's whole value is "here's what you could adopt", so we never early-return on no-tracked.
    if data.tracked.is_empty() {
        s.push_str("No tracked skills.\n");
    } else {
        match &out.enrollment {
            // Enrolled: group the tracked rows by workspace (named by the membership display label), with
            // the purely-local skills under their own clearly-labelled group. `--json` stays a flat list —
            // grouping is TTY-only.
            Some(e) => {
                for (ws_id, label) in ordered_workspace_groups(&data.tracked, &e.workspace_labels) {
                    s.push_str(&format!("{label}:\n"));
                    for (i, entry) in data.tracked.iter().enumerate() {
                        if entry.workspace_id.as_deref() == Some(ws_id) {
                            s.push_str(&list_row(entry, note_of(i)));
                        }
                    }
                }
                if data.tracked.iter().any(|e| e.workspace_id.is_none()) {
                    s.push_str("local (not shared):\n");
                    for (i, entry) in data.tracked.iter().enumerate() {
                        if entry.workspace_id.is_none() {
                            s.push_str(&list_row(entry, note_of(i)));
                        }
                    }
                }
            }
            // Unenrolled: the flat accountless list (there are no workspaces to group by).
            None => {
                s.push_str("Tracked skills:\n");
                for (i, entry) in data.tracked.iter().enumerate() {
                    s.push_str(&list_row(entry, note_of(i)));
                }
            }
        }
    }
    // Untracked skills discovered in any known harness's skill dir — the `add`-able inventory.
    if !data.untracked.is_empty() {
        s.push_str("\nUntracked skills — run `topos add <skill>` to adopt:\n");
        for u in &data.untracked {
            s.push_str(&untracked_row(u));
        }
    }
    // The `--remote` catalog — what this install could follow next, grouped by workspace and annotated
    // with the local follow-state. HONEST: there is no self-serve `follow <skill>` for an ungranted catalog
    // skill yet, so an `Available` row names where it lives and does NOT promise `topos follow`.
    if !data.remote_available.is_empty() {
        s.push_str("\nRemote catalog:\n");
        let label_of = |ws_id: &str| -> String {
            out.enrollment
                .as_ref()
                .and_then(|e| e.workspace_labels.iter().find(|(id, _)| id == ws_id))
                .map(|(_, label)| label.clone())
                .unwrap_or_else(|| ws_id.to_owned())
        };
        // `remote_available` is sorted by (workspace_id, skill_id), so group by consecutive workspace.
        let mut last_ws: Option<&str> = None;
        for r in &data.remote_available {
            if last_ws != Some(r.workspace_id.as_str()) {
                s.push_str(&format!("  {}:\n", label_of(&r.workspace_id)));
                last_ws = Some(r.workspace_id.as_str());
            }
            s.push_str(&remote_row(r));
        }
    }
    // Isolated per-workspace catalog-read failures — the same stable lines the `--json` envelope carries.
    for w in &out.warnings {
        s.push_str(&format!("warning: {w}\n"));
    }
    if let Some(footprint) = &data.footprint {
        s.push_str(&format!(
            "Footprint: {} paths under the topos home\n",
            footprint.len()
        ));
    }
    s.trim_end().to_owned()
}

/// One `--remote` catalog row: `<name>  <name>@<short>  <state note>` (+ any open-proposal count). The
/// name falls back to the skill id when the plane discloses no display name. HONEST annotations — no
/// `topos follow <skill>` promise for an `Available` skill (that grant is not self-serve yet).
fn remote_row(r: &RemoteSkillEntry) -> String {
    let name = r.display_name.as_deref().unwrap_or(&r.skill_id);
    let note = match r.state {
        RemoteFollowState::Available => "(available)".to_owned(),
        RemoteFollowState::Following => "(following)".to_owned(),
        RemoteFollowState::FollowingBehind => {
            format!("(update available — run `topos update {name}`)")
        }
    };
    let proposals = if r.open_proposals > 0 {
        format!("  {} open proposal(s)", r.open_proposals)
    } else {
        String::new()
    };
    format!(
        "    {}  {}@{}  {}{}\n",
        name,
        name,
        short(&r.version_id),
        note,
        proposals
    )
}

/// One untracked-discovery row: `<name>  [<harness-name> · <slug>]  <path>`, plus an adopt-only note for a
/// harness topos has no full adapter for — it can still be `add`ed (the bytes track + share), but live
/// currency for that harness lands later. The **slug** is shown because it is the `<skill>@<harness>` token
/// `add` takes to disambiguate a name found in more than one harness.
fn untracked_row(u: &UntrackedEntry) -> String {
    let support = if u.adapter_supported {
        ""
    } else {
        "  (adopt-only — live currency lands later)"
    };
    format!(
        "  {}  [{} · {}]  {}{}\n",
        u.name, u.harness_name, u.harness, u.path, support
    )
}

/// One tracked row's text: the padded skill line (`<skill>  <skill>@<short>` + follow note + draft flag)
/// plus any open-proposal lines beneath it. `note` is the follow-state `(mode, following)` where the skill
/// is enrolled+followed, else `None` (a purely local skill).
fn list_row(entry: &SkillEntry, note: Option<(&str, bool)>) -> String {
    let follow_note = match note {
        Some((mode, true)) => format!("  (following, {mode})"),
        Some((_, false)) => format!("  (not following — `topos follow {}` resumes)", entry.skill),
        None => String::new(),
    };
    // The SOURCE / STATUS / CAUSE columns (present once `list` populated them): `[status]` + the source,
    // and the detach cause on a detached row.
    let columns = list_columns(entry);
    let mut s = format!(
        "  {}  {}@{}{}{}{}\n",
        entry.skill,
        entry.skill,
        short(&entry.version_id),
        follow_note,
        if entry.draft { "  (draft)" } else { "" },
        columns,
    );
    // Open proposals print IN FULL — this is the surface a reviewer copies the hash from.
    for p in &entry.pending_proposals {
        s.push_str(&format!(
            "    open proposal {p} — run `topos review {p} --approve` (or `--reject`)\n"
        ));
    }
    s
}

/// The SOURCE / STATUS / CAUSE suffix for a tracked row (`  [behind]  from acme  (excluded here)`) —
/// rendered only for the fields `list` populated (an older/local producer leaves them `None`).
fn list_columns(entry: &SkillEntry) -> String {
    use topos_types::results::{DetachCause, SkillStatus};
    let mut s = String::new();
    // `draft` already shows via its own flag; skip it here to avoid a doubled note.
    if let Some(status) = entry.status
        && !matches!(status, SkillStatus::Draft)
    {
        let label = match status {
            SkillStatus::Current => "current",
            SkillStatus::Behind => "behind",
            SkillStatus::Detached => "detached",
            SkillStatus::Draft => "draft",
        };
        s.push_str(&format!("  [{label}]"));
    }
    if let Some(source) = &entry.source
        && source != "local"
    {
        s.push_str(&format!("  from {source}"));
    }
    if let Some(cause) = entry.cause {
        let label = match cause {
            DetachCause::Unfollowed => "unfollowed",
            DetachCause::ExcludedHere => "excluded here",
            DetachCause::RemovedUpstream => "removed upstream",
            DetachCause::SignedOut => "signed out",
        };
        s.push_str(&format!("  ({label})"));
    }
    s
}

/// The workspace groups present among `tracked`, ordered `(workspace_id, display_label)`: membership order
/// first (from `workspace_labels`), then any workspace that appears on a row but has no membership label
/// (defensive — named by its raw id). The purely-local (no-workspace) group is rendered by the caller.
fn ordered_workspace_groups<'a>(
    tracked: &'a [SkillEntry],
    workspace_labels: &'a [(String, String)],
) -> Vec<(&'a str, &'a str)> {
    let mut present: Vec<&str> = tracked
        .iter()
        .filter_map(|e| e.workspace_id.as_deref())
        .collect();
    present.sort_unstable();
    present.dedup();

    let mut ordered: Vec<(&'a str, &'a str)> = Vec::new();
    for (id, label) in workspace_labels {
        if present.contains(&id.as_str()) {
            ordered.push((id.as_str(), label.as_str()));
        }
    }
    for ws in present {
        if !ordered.iter().any(|(id, _)| *id == ws) {
            ordered.push((ws, ws));
        }
    }
    ordered
}

/// The `update --reset` DESCRIBE's TTY — LOSS-led: it shows exactly the draft delta being discarded.
pub(crate) fn reset_describe_tty(
    items: &[topos_types::results::ResetData],
    yes_argv: &[String],
) -> String {
    let mut s = String::new();
    for item in items {
        s.push_str(&format!(
            "Reset '{}' — this DISCARDS your local edits back to {}:\n",
            item.skill,
            short(&item.to_version)
        ));
        if item.drop_diff.trim().is_empty() {
            s.push_str("  (no local edits — nothing to discard)\n");
        } else {
            for line in item.drop_diff.trim_end_matches('\n').lines() {
                s.push_str(&format!("  {line}\n"));
            }
        }
    }
    s.push_str(&format!(
        "Nothing has changed yet — apply with:\n  {}",
        argv_line(yes_argv)
    ));
    s
}

/// The `update --reset` APPLY's TTY.
pub(crate) fn reset_applied_tty(items: &[topos_types::results::ResetData]) -> String {
    let mut s = String::new();
    for item in items {
        s.push_str(&format!(
            "Reset '{}' to {} — local edits discarded (a snapshot was kept in the sidecar store).\n",
            item.skill,
            short(&item.to_version)
        ));
    }
    s.trim_end().to_owned()
}

pub(crate) fn diff_tty(data: &DiffData) -> String {
    if data.diff.is_empty() {
        "No changes — the draft matches current.".to_owned()
    } else {
        data.diff.trim_end_matches('\n').to_owned()
    }
}

pub(crate) fn log_tty(data: &LogData) -> String {
    let mut out = String::new();
    // Lead with the archived-successor hint when the skill was resolved by its freed base name.
    if let Some(hint) = &data.archived_successor {
        out.push_str(&format!("note: {hint}\n"));
    }
    if data.events.is_empty() {
        return if out.is_empty() {
            "No history.".to_owned()
        } else {
            out.trim_end().to_owned()
        };
    }
    for e in &data.events {
        out.push_str(&format!("  {}\n", log_line(e)));
    }
    out.trim_end().to_owned()
}

/// One log event as readable columns: when (UTC, from `at`), action, who/what, `@`short-id — plus the
/// git commit message where one exists. The event schema is deliberately open, so an event with no
/// `action` string falls back to its raw JSON line (nothing is ever dropped); an `error` event renders
/// its verb, code, and the first line of the recorded detail.
fn log_line(e: &serde_json::Value) -> String {
    let get = |k: &str| e.get(k).and_then(serde_json::Value::as_str);
    let Some(action) = get("action") else {
        return e.to_string();
    };
    // The synthesized git-history events carry no `at`; keep the columns aligned with a blank stamp.
    let when = e
        .get("at")
        .and_then(serde_json::Value::as_u64)
        .map(fmt_utc_millis)
        .unwrap_or_else(|| " ".repeat(16));
    if action == "error" {
        let detail = get("detail").unwrap_or("").lines().next().unwrap_or("");
        return format!(
            "{when}  error  {} [{}] {detail}",
            get("verb").unwrap_or("?"),
            get("code").unwrap_or("?"),
        )
        .trim_end()
        .to_owned();
    }
    // A PURGED plane version renders its tombstone — the bytes are gone, so lead with who purged it.
    if action == "version"
        && let Some(purged_by) = get("purged_by")
    {
        let vid = get("version_id").map(short).unwrap_or("?");
        let purged_when = e
            .get("purged_at")
            .and_then(serde_json::Value::as_u64)
            .map(fmt_utc_millis)
            .unwrap_or_default();
        return format!("version {vid} — purged by {purged_by} {purged_when} — bytes gone")
            .trim_end()
            .to_owned();
    }
    // A plane proposal event: who proposed, its status, and any resolution.
    if action == "proposal" {
        let vid = get("version_id").map(short).unwrap_or("?");
        let status = get("status").unwrap_or("open");
        let mut s = format!(
            "proposal {vid}  {status}  by {}",
            get("proposer").unwrap_or("?")
        );
        if let Some(by) = get("resolved_by") {
            s.push_str(&format!(" — {status} by {by}"));
        }
        if let Some(reason) = get("resolved_reason") {
            s.push_str(&format!(": {reason}"));
        }
        return s;
    }
    let mut parts = vec![when, action.to_owned()];
    // Who/what: the human name where recorded, else the skill id; git version events carry the author.
    if let Some(name) = get("name")
        .or_else(|| get("skill_id"))
        .or_else(|| get("author"))
    {
        parts.push(name.to_owned());
    }
    if let Some(v) = get("version_id") {
        parts.push(format!("@{}", short(v)));
    }
    if let Some(m) = get("message") {
        parts.push(m.to_owned());
    }
    parts.join("  ")
}

/// Epoch-millis → `YYYY-MM-DD HH:MM` (UTC) — a plain civil-date conversion (no timezone dependency;
/// the log stamps are UTC epoch millis and minute precision is plenty for a history view).
fn fmt_utc_millis(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Days-since-epoch → (year, month, day), proleptic Gregorian (the standard era-based conversion).
/// `pub(crate)` — the publish pending receipt's RFC-3339 expiry formatter reuses it.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

pub(crate) fn self_update_tty(o: &crate::ops::SelfUpdateOutcome) -> String {
    use crate::ops::SelfUpdateAction::*;
    let mut s = match o.action {
        Checked if o.update_available => format!(
            "A newer topos is available: {} -> {}.\nRun `topos self-update` to install it.",
            o.current_version,
            o.latest_version.as_deref().unwrap_or("?")
        ),
        Checked | AlreadyCurrent => format!("topos is up to date ({}).", o.current_version),
        Upgraded => format!(
            "Updated topos {} -> {}.",
            o.current_version,
            o.latest_version.as_deref().unwrap_or("?")
        ),
    };
    if let Some(w) = &o.warning {
        s.push_str(&format!("\nwarning: {w}"));
    }
    s
}

pub(crate) fn follow_tty(data: &FollowData, resumed: &[String]) -> String {
    // A pending enrollment: surface the verification URL WITH the workspace + verified-domain provenance
    // (the relay-phishing guard — the human checks the domain before approving).
    if let Some(pending) = &data.pending {
        let workspace = data
            .workspace_display_name
            .clone()
            .unwrap_or_else(|| data.workspace_id.clone());
        let mut s = format!("Enrolling with {workspace}");
        if let Some(domain) = &data.verified_domain {
            let status = match data.verified_domain_status {
                Some(VerifiedDomainStatus::Verified) => "verified",
                Some(VerifiedDomainStatus::Pending) => "pending verification",
                _ => "unverified",
            };
            s.push_str(&format!(" ({domain}, {status})"));
        }
        if let Some(plane) = &data.plane_base_url {
            s.push_str(&format!("\nplane: {plane}"));
        }
        // The code rides inside the URL (RFC-8628 `verification_uri_complete`) — the human clicks it, never
        // types it — so only the URL + the anti-phishing fingerprint are surfaced (no separate code line).
        s.push_str(&format!(
            "\nOpen this URL to approve, then re-run `topos follow`:\n  {}\n  \
             fingerprint: {} (confirm it matches the page before approving)",
            pending.verification_uri_complete,
            group_fingerprint(&pending.device_fingerprint),
        ));
        return s;
    }
    // A completed enrollment.
    let mut s = if !data.enrolled {
        format!("Enrolled with workspace {}.", data.workspace_id)
    } else if data.skills.is_empty() {
        format!(
            "Enrolled with workspace {} (no skills to follow).",
            data.workspace_id
        )
    } else {
        let mut s = format!(
            "Enrolled with workspace {}. Offered skills:",
            data.workspace_id
        );
        for sk in &data.skills {
            s.push_str(&format!(
                "\n  {}  {}@{}",
                sk.name,
                sk.name,
                short(&sk.offer.version_id)
            ));
        }
        s.push_str("\nApprove a skill with `topos follow <skill>` (or `topos update <skill>`).");
        s
    };
    // The resume disclosure: a skill-path follow flipped a paused entry back on (TTY-only; the pinned
    // `FollowData` shape has no resume field).
    for name in resumed {
        s.push_str(&format!(
            "\nResumed following {name} — auto-updates are back on; the next `topos update` lands the \
             team's current."
        ));
    }
    s
}

/// The generic next-actions for a two-phase DESCRIBE: each argv is the ready-to-exec apply command
/// (`… --yes`, plus alternatives like the `--prefix-dirname` variant).
pub(crate) fn describe_next_actions(argvs: Vec<Vec<String>>) -> Vec<NextAction> {
    argvs
        .into_iter()
        .map(|argv| NextAction {
            code: ActionCode::from("APPLY_DESCRIBED".to_owned()),
            argv,
        })
        .collect()
}

/// One argv as a paste-ready shell line (the TTY's spelling of a next action).
fn argv_line(argv: &[String]) -> String {
    argv.join(" ")
}

/// The follow DESCRIBE's TTY: the workspace story, the install list with digests + via, the
/// collision choice, the standing disclosures, and the `--yes` argvs. Nothing has changed yet.
pub(crate) fn follow_describe_tty(
    d: &crate::ops::FollowDescribe,
    next_argvs: &[Vec<String>],
) -> String {
    let mut s = format!(
        "{} ({}) — {}",
        d.workspace.display_name, d.workspace.name, d.workspace.address
    );
    if d.enrolled_now {
        s.push_str("\nEnrolled this device (identity only — nothing is installed yet).");
    }
    s.push_str(&format!("\nYour role: {}", d.role));
    if let Some(by) = &d.invited_by {
        s.push_str(&format!(" — invited by {by}"));
    }
    if !d.preplaced_channels.is_empty() {
        s.push_str(&format!(
            "\nPre-placed channels: #{}",
            d.preplaced_channels.join(", #")
        ));
    }
    let target_list: Vec<String> = d
        .targets
        .iter()
        .map(|t| format!("{} {}", t.kind, t.name))
        .collect();
    s.push_str(&format!("\nFollowing: {}", target_list.join(", ")));
    if d.installs.is_empty() {
        s.push_str("\nNothing new would install on this device.");
    } else {
        s.push_str("\nWould install:");
        for i in &d.installs {
            let via = if i.via_channels.is_empty() {
                if i.via_direct {
                    "direct".to_owned()
                } else {
                    String::new()
                }
            } else {
                let mut v = format!("#{}", i.via_channels.join(", #"));
                if i.via_direct {
                    v.push_str(", direct");
                }
                v
            };
            let digest = i.bundle_digest.as_deref().map(short).unwrap_or("?");
            s.push_str(&format!("\n  {}  @{}  via {}", i.name, digest, via));
        }
    }
    if let Some(note) = &d.direct_follow_note {
        s.push_str(&format!("\nnote: {note}"));
    }
    for note in &d.freed_name_notes {
        s.push_str(&format!("\nnote: {note}"));
    }
    if !d.collisions.is_empty() {
        s.push_str("\nName collisions (declined by default):");
        for c in &d.collisions {
            s.push_str(&format!(
                "\n  {} — a different skill already lives at {}; `--prefix-dirname` installs it \
                 as {}",
                c.name, c.existing, c.prefixed_dirname
            ));
        }
    }
    s.push_str(&format!("\n{}", d.all_devices_note));
    s.push_str(&format!("\n{}", d.reporting_note));
    s.push_str("\nNothing has changed yet — apply with:");
    for argv in next_argvs {
        s.push_str(&format!("\n  {}", argv_line(argv)));
    }
    s
}

/// The follow APPLY's TTY: what was subscribed, what landed, what was declined.
pub(crate) fn follow_applied_tty(a: &crate::ops::FollowApplied) -> String {
    let mut s = format!("Following in {} ({}).", a.workspace_name, a.workspace_id);
    if a.enrolled_now {
        s = format!(
            "Enrolled into {} ({}).\n{s}",
            a.workspace_name, a.workspace_id
        );
    }
    for t in &a.subscribed {
        s.push_str(&format!("\nSubscribed: {} {}", t.kind, t.name));
    }
    if a.installed.is_empty() {
        s.push_str("\nNothing new installed on this device.");
    } else {
        s.push_str("\nInstalled:");
        for i in &a.installed {
            let digest = i.bundle_digest.as_deref().map(short).unwrap_or("?");
            s.push_str(&format!("\n  {}  @{digest}", i.name));
        }
    }
    for c in &a.declined {
        s.push_str(&format!(
            "\nDeclined {} (name collision with {}); re-run with `--prefix-dirname` to install it \
             as {}",
            c.name, c.existing, c.prefixed_dirname
        ));
    }
    for w in &a.warnings {
        s.push_str(&format!("\nwarning: {w}"));
    }
    s
}

/// The unfollow DESCRIBE's TTY: what stops where, what never changes, and the `--yes` argv.
pub(crate) fn unfollow_describe_tty(
    d: &crate::ops::UnfollowDescribe,
    yes_argv: &[String],
) -> String {
    let mut s = String::new();
    for item in &d.items {
        s.push_str(&format!("Unfollowing {} {}:", item.kind, item.name));
        if item.stops.is_empty() {
            s.push_str("\n  nothing currently delivered stops");
        } else {
            s.push_str(&format!("\n  stops: {}", item.stops.join(", ")));
        }
        if !item.keeps.is_empty() {
            s.push_str(&format!(
                "\n  keeps arriving (other channels / direct): {}",
                item.keeps.join(", ")
            ));
        }
        s.push('\n');
    }
    s.push_str(&d.all_devices_note);
    s.push_str(&format!("\n{}", d.bytes_note));
    s.push_str(&format!("\n{}", d.record_note));
    s.push_str(&format!(
        "\nNothing has changed yet — apply with:\n  {}",
        argv_line(yes_argv)
    ));
    s
}

/// The unfollow APPLY's TTY.
pub(crate) fn unfollow_applied_tty(a: &crate::ops::UnfollowApplied) -> String {
    let mut s = String::new();
    for item in &a.items {
        s.push_str(&format!(
            "Stopped following {} {} — delivery ends on every device; the local copy stays as a \
             frozen copy.\n",
            item.kind, item.name
        ));
    }
    s.push_str("`topos follow` re-attaches.");
    s
}

/// The pending login's TTY (the device-flow wait — same shape as the follow/publish waits).
pub(crate) fn login_pending_tty(p: &crate::ops::AuthLoginPending) -> String {
    format!(
        "Signing in to {}\nOpen this URL to approve, then re-run `topos auth login`:\n  {}\n  \
         fingerprint: {} (confirm it matches the page before approving)",
        p.server,
        p.verification_uri_complete,
        group_fingerprint(&p.device_fingerprint),
    )
}

/// The completed login's TTY: the per-workspace minted/blocked report.
pub(crate) fn login_done_tty(d: &crate::ops::AuthLoginData) -> String {
    let mut s = format!("Signed in to {} as {}.", d.server, d.principal);
    if let Some(old) = &d.replaced_principal {
        s.push_str(&format!("\nReplaced the previous account {old}."));
    }
    if d.memberships.is_empty() {
        s.push_str("\nNo workspace seats yet — follow a workspace address to join one.");
    }
    for m in &d.memberships {
        if m.minted {
            s.push_str(&format!(
                "\n  {} ({}) — credential minted ({})",
                m.display_name, m.name, m.role
            ));
        } else {
            s.push_str(&format!(
                "\n  {} ({}) — blocked: {}",
                m.display_name,
                m.name,
                m.blocked.as_deref().unwrap_or("no credential minted")
            ));
        }
    }
    s
}

/// The pending login's next action — re-invoke `auth login` (re-invoking IS the resume).
pub(crate) fn login_pending_next_actions() -> Vec<NextAction> {
    vec![NextAction {
        code: ActionCode::from("ENROLL_RESUME".to_owned()),
        argv: vec![
            "topos".into(),
            "auth".into(),
            "login".into(),
            "--json".into(),
        ],
    }]
}

/// The logout DESCRIBE's TTY.
pub(crate) fn logout_describe_tty(
    d: &crate::ops::AuthLogoutDescribe,
    yes_argv: &[String],
) -> String {
    let mut s = match &d.principal {
        Some(p) => format!("Signing out {p}."),
        None => "Signing out.".to_owned(),
    };
    if d.workspaces.is_empty() {
        s.push_str("\nNo stored workspace credentials — already signed out.");
    } else {
        s.push_str(&format!(
            "\nWould revoke this device (best-effort) and delete its credential in: {}",
            d.workspaces.join(", ")
        ));
    }
    s.push_str(&format!("\n{}", d.keeps_note));
    s.push_str(&format!(
        "\nNothing has changed yet — apply with:\n  {}",
        argv_line(yes_argv)
    ));
    s
}

/// The applied logout's TTY.
pub(crate) fn logout_applied_tty(d: &crate::ops::AuthLogoutData) -> String {
    let mut s = if d.credentials_deleted {
        "Signed out — the stored credentials are deleted.".to_owned()
    } else {
        "Already signed out — no credentials were stored.".to_owned()
    };
    if !d.revoked.is_empty() {
        s.push_str(&format!(
            "\nRevoked this device in: {}",
            d.revoked.join(", ")
        ));
    }
    if !d.revoke_failed.is_empty() {
        s.push_str(&format!(
            "\nCould not reach the server to revoke in: {} (the credential is deleted locally \
             either way; an owner can revoke the device on the web)",
            d.revoke_failed.join(", ")
        ));
    }
    s.push_str(&format!("\n{}", d.keeps_note));
    s
}

/// `auth status`'s TTY — whoami, per-workspace health, hook health, reporting posture.
pub(crate) fn auth_status_tty(d: &crate::ops::AuthStatusData) -> String {
    let mut s = match (&d.principal, d.signed_in) {
        (Some(p), true) => format!("Signed in as {p}"),
        (Some(p), false) => format!("Signed out (last account: {p})"),
        (None, _) => "Not signed in.".to_owned(),
    };
    if let Some(server) = &d.server {
        s.push_str(&format!("\nserver: {server}"));
    }
    for ws in &d.workspaces {
        let label = ws.display_name.as_deref().unwrap_or(&ws.workspace_id);
        s.push_str(&format!("\n  {label}: {}", ws.health));
        if let Some(role) = &ws.role {
            s.push_str(&format!(" ({role})"));
        }
    }
    s.push_str(&format!(
        "\ncurrency hook: {}",
        if d.hook_armed {
            "armed"
        } else {
            "not installed"
        }
    ));
    for r in &d.reporting {
        let last = r
            .last_report_at
            .map(|t| format!("last report at {t}"))
            .unwrap_or_else(|| "never reported".to_owned());
        s.push_str(&format!(
            "\n  reporting {}: {last}{}",
            r.workspace_id,
            if r.stale { " — STALE" } else { "" }
        ));
    }
    s
}

/// One `remove` item line for the describe/apply — the boundary a followed removal keeps vs the
/// permanence of a local delete.
fn remove_item_line(item: &RemoveItem, applied: bool) -> String {
    match item.kind {
        RemoveKind::FollowedExclusion => {
            let verb = if applied { "Removed" } else { "Would remove" };
            format!(
                "{verb} '{}' from THIS device only — your other devices and the team are \
                 unaffected. The local copy is kept as a frozen copy; nothing returns at the next \
                 sync. `topos follow {}` re-attaches (stopping it everywhere is `topos unfollow`).",
                item.name, item.name
            )
        }
        RemoveKind::TrackedLocalPermanent => {
            let verb = if applied { "Deleted" } else { "Would delete" };
            format!(
                "{verb} '{}' PERMANENTLY — it was never published, so no other copy exists (the \
                 topos entry is dropped too).",
                item.name
            )
        }
        RemoveKind::UntrackedLocal => {
            let verb = if applied { "Deleted" } else { "Would delete" };
            format!(
                "{verb} '{}' PERMANENTLY — an untracked local copy; no other copy exists.",
                item.name
            )
        }
    }
}

/// The `remove` DESCRIBE's TTY — the per-skill boundary + the `--yes` argv (nothing changed yet).
pub(crate) fn remove_describe_tty(data: &RemoveData, yes_argv: &[String]) -> String {
    let mut s = String::new();
    for item in &data.items {
        s.push_str(&remove_item_line(item, false));
        s.push('\n');
    }
    s.push_str(&format!(
        "Nothing has changed yet — apply with:\n  {}",
        argv_line(yes_argv)
    ));
    s
}

/// The `remove` APPLY's TTY.
pub(crate) fn remove_applied_tty(data: &RemoveData) -> String {
    let mut s = String::new();
    for item in &data.items {
        s.push_str(&remove_item_line(item, true));
        s.push('\n');
    }
    s.trim_end().to_owned()
}

/// The `channel add|remove` DESCRIBE's TTY — the placements/removals, the mode gate, the create note.
pub(crate) fn channel_describe_tty(
    data: &topos_types::results::ChannelData,
    yes_argv: &[String],
) -> String {
    use topos_types::results::ChannelAction;
    let verb = match data.action {
        ChannelAction::Add => "Place into",
        ChannelAction::Remove => "Remove from",
    };
    let mut s = format!("{verb} #{}", data.channel);
    if data.creates {
        s.push_str(&format!(
            " (creates #{} — it does not exist yet)",
            data.channel
        ));
    } else {
        s.push_str(&format!(" (mode: {})", data.mode));
    }
    s.push(':');
    for item in &data.items {
        s.push_str(&format!("\n  {}", item.skill));
    }
    if data.mode == "curated" {
        s.push_str("\nThis channel is curated — placement takes reviewer or owner.");
    }
    s.push_str(&format!(
        "\nNothing has changed yet — apply with:\n  {}",
        argv_line(yes_argv)
    ));
    s
}

/// The `channel add|remove` APPLY's TTY — per-skill outcomes, honest about a partial failure.
pub(crate) fn channel_applied_tty(data: &topos_types::results::ChannelData) -> String {
    use topos_types::results::{ChannelAction, ChannelItemOutcome};
    let mut s = match data.action {
        ChannelAction::Add if data.creates => format!("Created #{} and placed:", data.channel),
        ChannelAction::Add => format!("Placed into #{}:", data.channel),
        ChannelAction::Remove => format!("Removed from #{}:", data.channel),
    };
    for item in &data.items {
        let outcome = match item.outcome {
            ChannelItemOutcome::Placed => "placed".to_owned(),
            ChannelItemOutcome::Removed => "removed".to_owned(),
            ChannelItemOutcome::Pending => "pending".to_owned(),
            ChannelItemOutcome::Failed => {
                format!("FAILED — {}", item.detail.as_deref().unwrap_or("refused"))
            }
        };
        s.push_str(&format!("\n  {}  {}", item.skill, outcome));
    }
    s
}

/// The `protect` DESCRIBE's TTY — the level being set, the audience it governs, and the standing note.
pub(crate) fn protect_describe_tty(
    data: &topos_types::results::ProtectData,
    yes_argv: &[String],
) -> String {
    let direction = if data.loosening { "Loosen" } else { "Tighten" };
    let mut s = format!(
        "{direction} {} '{}' to `{}`",
        data.kind, data.target, data.level
    );
    if let Some(n) = data.audience {
        let noun = if data.kind == "channel" {
            "members"
        } else {
            "people"
        };
        s.push_str(&format!(" — reaches {n} {noun}"));
    }
    if data.loosening {
        s.push_str(" (an owner act)");
    } else {
        s.push_str(" (reviewer or owner)");
    }
    if let Some(note) = &data.note {
        s.push_str(&format!("\nnote: {note}"));
    }
    s.push_str(&format!(
        "\nNothing has changed yet — apply with:\n  {}",
        argv_line(yes_argv)
    ));
    s
}

/// The `protect` APPLY's TTY.
pub(crate) fn protect_applied_tty(data: &topos_types::results::ProtectData) -> String {
    format!("Set {} '{}' to `{}`.", data.kind, data.target, data.level)
}

/// The bare `invite` read's TTY — the workspace address + invite policy, and the explicit no-op note.
pub(crate) fn invite_read_tty(data: &topos_types::results::InviteReadData) -> String {
    let who = if data.invite_policy == "owners" {
        "owners"
    } else {
        "any member"
    };
    format!(
        "Workspace address: {}\nInvite policy: {} ({who} may invite).\nNothing was sent or changed \
         — pass emails to invite (`topos invite <email>...`).",
        data.address, data.invite_policy
    )
}

/// The `invite <email>...` DESCRIBE's TTY — who gets seated, the pre-placements, the mail-or-paste note.
pub(crate) fn invite_describe_tty(
    data: &topos_types::results::InviteDescribeData,
    yes_argv: &[String],
) -> String {
    let mut s = format!("Would seat as invited members of {}:", data.address);
    for e in &data.seat {
        s.push_str(&format!("\n  {e}"));
    }
    if !data.channels.is_empty() {
        s.push_str(&format!(
            "\nPre-placed into channels: #{}",
            data.channels.join(", #")
        ));
    }
    s.push_str(&format!(
        "\nThey join at {} — the server mails them if it can, otherwise paste the address to them.",
        data.address
    ));
    s.push_str(&format!(
        "\nNothing has changed yet — apply with:\n  {}",
        argv_line(yes_argv)
    ));
    s
}

pub(crate) fn invite_tty(data: &InvitationData) -> String {
    let mut out = if data.invited.is_empty() {
        "No new invitations.".to_owned()
    } else {
        format!("Invited: {}", data.invited.join(", "))
    };
    if data.mailed {
        out.push_str("\nInvitation email sent.");
    }
    out.push_str(&format!(
        "\nThey join at {} — ask them to run `topos follow {}` and sign in with their invited email.",
        data.address, data.address,
    ));
    out
}

/// The one-line disclosure that a `publish` ADDED the skill to topos first (the auto-add convenience) —
/// honest plumbing ("it also adopted this skill"), naming the harness it was attributed to when known. It
/// states only the adoption; the line that FOLLOWS conveys whether the publish then shipped or is pending.
fn added_line(added: &AddedNote) -> String {
    match &added.harness_slug {
        Some(slug) => format!("Added '{}' from {slug} to topos.", added.name),
        None => format!("Added '{}' to topos.", added.name),
    }
}

/// The bare enrolled `publish` DESCRIBE — where it lands, the gate, the audience, the share line, and
/// the undo. Nothing has landed on the plane yet.
pub(crate) fn publish_describe_tty(
    data: &topos_types::results::PublishDescribeData,
    yes_argv: &[String],
) -> String {
    use topos_types::results::PublishGate;
    let ws = data
        .workspace_display_name
        .as_deref()
        .unwrap_or(&data.workspace_id);
    let mut s = format!("Publish '{}' to {ws}:", data.skill);
    s.push_str(&format!("\n  digest {}", short(&data.bundle_digest)));
    let gate = match data.gate {
        PublishGate::Lands => "open — this moves current directly",
        PublishGate::Proposal => {
            "reviewed — this opens a proposal (current does not move until approved)"
        }
    };
    s.push_str(&format!("\n  gate: {gate}"));
    if data.is_revert {
        s.push_str("\n  this restores earlier bytes (a revert), shipped through the same gate");
    }
    if !data.placements.is_empty() {
        s.push_str(&format!(
            "\n  places into: #{}",
            data.placements.join(", #")
        ));
    }
    if let Some(reach) = data.reach {
        s.push_str(&format!("\n  reaches {reach} people"));
    }
    if let Some(note) = &data.origin_note {
        s.push_str(&format!("\n  note: {note}"));
    }
    if let Some(line) = &data.share_line {
        s.push_str(&format!("\n  share: {line}"));
    }
    if let Some(undo) = &data.undo {
        s.push_str(&format!("\n  undo: {undo}"));
    }
    s.push_str(&format!(
        "\nNothing has landed yet — apply with:\n  {}",
        argv_line(yes_argv)
    ));
    s
}

pub(crate) fn publish_tty(data: &PublishData) -> String {
    let mut out = String::new();
    // If this invocation ADDED the skill first (the auto-add convenience), say so before the publish line.
    if let Some(added) = &data.added {
        out.push_str(&added_line(added));
        out.push('\n');
    }
    // A workspace-creating publish discloses what it stood up and who owns it FIRST (hijack visibility:
    // an owner you don't recognize means someone else approved the sign-in).
    if let Some(standup) = &data.standup {
        out.push_str(&format!(
            "Stood up workspace {} — owner {}.\n",
            standup.workspace_display_name,
            standup.owner_principal.as_deref().unwrap_or("(unknown)"),
        ));
    }
    let version = data.version_id.as_deref().map(short).unwrap_or("?");
    let gen_text = data
        .current_generation
        .map(|g| format!("({},{})", g.epoch, g.seq))
        .unwrap_or_else(|| "(?)".to_owned());
    out.push_str(&format!(
        "Published {}@{} (digest {}) — current is now {}.",
        data.skill_id,
        version,
        short(&data.bundle_digest),
        gen_text,
    ));
    // When the plane disclosed the workspace address (a genesis standup), surface the paste-able share line.
    if let Some(line) = &data.share_line {
        out.push_str(&format!(
            "\nShare this skill: {line}\nTeammates just paste that address to their agent and ask it \
             to follow — the address walks the agent through install, sign-in, and landing the skill.",
        ));
    }
    out
}

/// The PENDING standup publish: the human opens the sign-in URL and approves; the agent then re-runs the
/// SAME publish command (nothing was published yet — honest about that).
pub(crate) fn publish_pending_tty(data: &PublishData, resume_argv: &[String]) -> String {
    let Some(pending) = &data.pending else {
        // Unreachable by construction (the Pending outcome always carries the block) — stay honest anyway.
        return "Publish is pending a workspace sign-in.".to_owned();
    };
    // If this invocation ADDED the skill first (an un-enrolled `publish <untracked>`), disclose it before
    // the sign-in prompt — the skill is now tracked locally even while the workspace standup is pending.
    let prefix = match &data.added {
        Some(added) => format!("{}\n", added_line(added)),
        None => String::new(),
    };
    // The code rides inside the URL (RFC-8628 `verification_uri_complete`) — clicked, never typed — so only
    // the URL + the anti-phishing fingerprint are surfaced (no separate code line).
    format!(
        "{prefix}No workspace yet — publishing this first skill creates one.\nOpen this URL, sign in, and \
         approve (you become the workspace owner):\n  {}\n  fingerprint: {} (confirm it \
         matches the page before approving)\nNothing is published yet; then re-run:\n  {}",
        pending.verification_uri_complete,
        group_fingerprint(&pending.device_fingerprint),
        resume_argv.join(" "),
    )
}

/// The pending publish's one next action: re-invoke THE SAME publish command (`ENROLL_RESUME` — the
/// resume IS the original command; the optional `@<digest>` pin re-derives from it on every invocation).
pub(crate) fn publish_pending_next_actions(resume_argv: Vec<String>) -> Vec<NextAction> {
    vec![NextAction {
        code: ActionCode::from("ENROLL_RESUME".to_owned()),
        argv: resume_argv,
    }]
}

pub(crate) fn propose_tty(data: &ProposeData) -> String {
    // If this invocation ADDED the skill first (the auto-add convenience), disclose it before the proposal.
    let prefix = match &data.added {
        Some(added) => format!("{}\n", added_line(added)),
        None => String::new(),
    };
    // Honest: this is NEEDS_REVIEW — a proposal opened, `current` did NOT move.
    format!(
        "{prefix}Opened proposal {} on base {}. Awaiting review — a reviewer runs `topos review {} --approve`.",
        data.proposal,
        short(&data.base_version_id),
        data.proposal,
    )
}

pub(crate) fn revert_tty(data: &RevertData) -> String {
    format!(
        "Reverted {} to {} as forward commit {} — current is now ({},{}). Nothing was deleted; move \
         current forward again to redo.",
        data.skill_id,
        short(&data.reverted_to),
        short(&data.new_version_id),
        data.current_generation.epoch,
        data.current_generation.seq,
    )
}

/// The bare `review` inbox/outbox TTY — author-message FIRST, grouped by workspace, inbox before outbox.
pub(crate) fn review_inbox_tty(data: &topos_types::results::ReviewIndexData) -> String {
    use topos_types::results::ReviewIndexEntry;
    let entry_line = |e: &ReviewIndexEntry| -> String {
        let stale = if e.stale {
            "  [STALE — needs re-propose]"
        } else {
            ""
        };
        format!(
            "  {}  ({})\n    {}  by {}{}\n    review with `topos review {} --approve` (or `--reject -m <reason>`)",
            e.message, e.workspace_name, e.proposal, e.proposer, stale, e.proposal
        )
    };
    if data.inbox.is_empty() && data.outbox.is_empty() {
        return "No open proposals.".to_owned();
    }
    let mut s = String::new();
    if !data.inbox.is_empty() {
        s.push_str("To review (others' proposals):\n");
        for e in &data.inbox {
            s.push_str(&entry_line(e));
            s.push('\n');
        }
    }
    if !data.outbox.is_empty() {
        s.push_str("Your open proposals:\n");
        for e in &data.outbox {
            s.push_str(&format!(
                "  {}  ({})\n    {}  awaiting review{}\n",
                e.message,
                e.workspace_name,
                e.proposal,
                if e.stale {
                    "  [STALE — re-propose]"
                } else {
                    ""
                }
            ));
        }
    }
    s.trim_end().to_owned()
}

/// A bare review TARGET's describe TTY — author, message, base, staleness, and the diff. Nothing mutates.
pub(crate) fn review_describe_tty(
    data: &topos_types::results::ReviewDescribeData,
    next_argvs: &[Vec<String>],
) -> String {
    let mut s = format!(
        "{}\n  proposal {}\n  by {}  on base {}{}",
        data.message,
        data.proposal,
        data.proposer,
        short(&data.base_version_id),
        if data.stale {
            "  [STALE — current moved; the author should re-propose]"
        } else {
            ""
        },
    );
    if data.diff.trim().is_empty() {
        s.push_str("\n(no changes against current)");
    } else {
        s.push_str("\n\n");
        s.push_str(data.diff.trim_end_matches('\n'));
    }
    s.push_str("\nDecide with:");
    for argv in next_argvs {
        s.push_str(&format!("\n  {}", argv_line(argv)));
    }
    s
}

pub(crate) fn review_tty(data: &ReviewData) -> String {
    match data.decision {
        ReviewDecision::Approve => {
            let moved_to = data
                .current_generation
                .map(|g| format!("({},{})", g.epoch, g.seq))
                .unwrap_or_else(|| "the new version".to_owned());
            format!(
                "Approved {} — current moved to {moved_to}. Every follower picks it up on their next update.",
                data.proposal,
            )
        }
        ReviewDecision::Reject => format!(
            "Rejected {}. It will no longer be applied; `current` is unchanged.",
            data.proposal,
        ),
        ReviewDecision::Withdraw => format!(
            "Withdrew {}. Your proposal is closed; `current` is unchanged.",
            data.proposal,
        ),
    }
}

/// The human `update` view — one line per skill that needs attention (gh-status style: name, what
/// happened, and the concrete next command where one exists), up-to-date rows summarized compactly,
/// isolated per-skill failures (`warnings` — the same stable lines the `--json` envelope carries)
/// rendered visibly, and the awaiting-review trailer. The `--quiet` hook path never reaches this
/// renderer (it stays byte-silent).
pub(crate) fn pull_tty(data: &PullData, warnings: &[String]) -> String {
    if data.skills.is_empty() && warnings.is_empty() {
        return append_proposals_trailer("No followed skills.".to_owned(), data.proposals_awaiting);
    }
    let mut up_to_date = 0usize;
    let rows: Vec<(&str, String, Vec<String>)> = data
        .skills
        .iter()
        .filter_map(|s| {
            if matches!(s.action, topos_types::results::PullAction::UpToDate) {
                up_to_date += 1;
                return None;
            }
            let (line, extra) = pull_row(s);
            Some((s.skill.as_str(), line, extra))
        })
        .collect();

    let mut out = String::new();
    let pad = rows.iter().map(|(n, ..)| n.len()).max().unwrap_or(0);
    for (name, line, extra) in &rows {
        out.push_str(&format!("{name:<pad$}  {line}\n"));
        for x in extra {
            out.push_str(&format!("    {x}\n"));
        }
    }
    for w in warnings {
        out.push_str(&format!("warning: {w}\n"));
    }

    // The summary counts every skill the sweep attempted — including the failed ones above.
    let total = data.skills.len() + warnings.len();
    if rows.is_empty() && warnings.is_empty() {
        out.push_str(&format!(
            "Checked {total} followed skill(s) — all up to date."
        ));
    } else {
        let mut parts = Vec::new();
        if up_to_date > 0 {
            parts.push(format!("{up_to_date} up to date"));
        }
        if !warnings.is_empty() {
            parts.push(format!("{} failed", warnings.len()));
        }
        out.push_str(&format!("Checked {total} followed skill(s)"));
        if !parts.is_empty() {
            out.push_str(&format!(": {}", parts.join(", ")));
        }
        out.push('.');
    }
    append_proposals_trailer(out, data.proposals_awaiting)
}

/// One non-up-to-date skill's line (after the padded name) + any indented detail lines.
fn pull_row(s: &PullSkill) -> (String, Vec<String>) {
    use topos_types::results::PullAction;
    let name = &s.skill;
    match s.action {
        // Handled by the caller's compact summary.
        PullAction::UpToDate => (String::from("up to date"), Vec::new()),
        PullAction::FastForwarded => (
            format!(
                "fast-forwarded — now at ({},{})",
                s.applied.epoch, s.applied.seq
            ),
            Vec::new(),
        ),
        PullAction::Offered => {
            let v = s
                .offer
                .as_ref()
                .map(|o| short(&o.version_id))
                .unwrap_or("?");
            (
                format!("update offered @{v} — run `topos update {name}`"),
                Vec::new(),
            )
        }
        PullAction::Withdrawn => (
            format!(
                "withdrawn upstream — agent dirs cleaned; your copy + drafts are kept locally (run \
                 `topos add {name} --yes` to keep it as yours)"
            ),
            Vec::new(),
        ),
        PullAction::Detached => (
            String::from("detached (you unfollowed) — frozen in place; `topos follow` re-attaches"),
            Vec::new(),
        ),
        PullAction::Excluded => (
            String::from(
                "not on this device (you removed it here) — your other devices still receive it",
            ),
            Vec::new(),
        ),
        PullAction::Diverged => {
            let v = s
                .conflict
                .as_ref()
                .map(|c| short(&c.remote_version_id))
                .unwrap_or("?");
            (
                format!(
                    "diverged from the new current @{v} — your local draft is kept; run \
                     `topos update {name}` to merge it (or `topos update {name} --onto-current` to \
                     keep your bytes and drop the update)"
                ),
                Vec::new(),
            )
        }
        PullAction::Merged => {
            let v = s
                .merge
                .as_ref()
                .map(|m| short(&m.result_version_id))
                .unwrap_or("?");
            (
                format!(
                    "merged — your draft was rebased onto the new current as @{v}; review with \
                     `topos diff {name}`, then publish"
                ),
                Vec::new(),
            )
        }
        PullAction::Conflicted => {
            let v = s
                .merge
                .as_ref()
                .map(|m| short(&m.theirs_version_id))
                .unwrap_or("?");
            let extra = s
                .merge
                .iter()
                .flat_map(|m| &m.conflicts)
                .map(|c| format!("{} ({})", c.path, conflict_kind_label(c.kind)))
                .collect();
            (
                format!(
                    "merge conflict with the new current @{v} — markers written; edit the files, \
                     then run `topos update {name} --onto-current` to commit your resolution \
                     (publish is blocked until then)"
                ),
                extra,
            )
        }
        PullAction::Held => (
            format!(
                "held — pinned at a local go-back; run `topos update {name}` to resume following current"
            ),
            Vec::new(),
        ),
    }
}

/// What a conflicted path's `kind` means on disk — where "yours" ended up, so the checklist is actionable.
fn conflict_kind_label(kind: ConflictPathKind) -> &'static str {
    match kind {
        ConflictPathKind::Content => "content — diff3 markers at the path",
        ConflictPathKind::BinaryContent => "binary content — yours kept in the .topos-mine sidecar",
        ConflictPathKind::ModifyDelete => "you modified, current deleted — yours kept",
        ConflictPathKind::DeleteModify => "you deleted, current modified — theirs kept",
        ConflictPathKind::AddAdd => "both added — yours kept in the .topos-mine sidecar",
        ConflictPathKind::ModeMode => "mode disagreement — theirs kept",
        ConflictPathKind::Oversize => "too large to merge — yours kept in the .topos-mine sidecar",
    }
}

/// The reviewer-queue trailer, appended when open proposals await review.
fn append_proposals_trailer(mut out: String, awaiting: u32) -> String {
    if awaiting > 0 {
        out.push_str(&format!(
            "\n{awaiting} proposal(s) awaiting review — run `topos review <skill>@<hash> \
             --approve` (or `--reject`); `topos list <skill>` prints each hash."
        ));
    }
    out
}

pub(crate) fn err_tty(err: &ClientError) -> String {
    format!("error: {}", safe_message(err))
}

fn short(hex: &str) -> &str {
    hex.get(..12).unwrap_or(hex)
}

/// Group a device fingerprint into space-separated 4-char chunks for eyeball comparison against the
/// verification page (e.g. `e4aaf52f5c391ce9` → `e4aa f52f 5c39 1ce9`). `pub(crate)` — the bin's
/// interactive blocking `follow` prints the same grouped form to stderr while it polls.
pub(crate) fn group_fingerprint(fp: &str) -> String {
    fp.chars()
        .collect::<Vec<_>>()
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use topos_types::Generation;
    use topos_types::persisted::ConflictPathKind;
    use topos_types::results::{
        Conflict, ConflictPathReport, ListData, LogData, MergeReport, Offer, PullAction, PullData,
        PullSkill, SkillEntry,
    };

    use crate::ops::{FollowNote, ListEnrollment, ListOutcome};

    use super::{follow_tty, group_fingerprint, list_tty, log_tty, pull_tty};

    fn g(epoch: u64, seq: u64) -> Generation {
        Generation { epoch, seq }
    }

    fn row(name: &str, action: PullAction) -> PullSkill {
        PullSkill {
            skill: name.to_owned(),
            workspace_id: None,
            observed: g(1, 2),
            applied: g(1, 2),
            action,
            offer: None,
            conflict: None,
            merge: None,
        }
    }

    fn merge_report(clean: bool, conflicts: Vec<ConflictPathReport>) -> MergeReport {
        MergeReport {
            base_version_id: "0a".repeat(32),
            theirs_version_id: "1b".repeat(32),
            result_version_id: "2c".repeat(32),
            result_digest: "3d".repeat(32),
            clean,
            conflicts,
            drop_diff: None,
        }
    }

    #[test]
    fn pull_tty_renders_each_action_with_its_next_command() {
        let offered = PullSkill {
            offer: Some(Offer {
                version_id: "ab12cd34ef56".to_owned() + &"0".repeat(52),
                bundle_digest: "9f".repeat(32),
            }),
            ..row("docs", PullAction::Offered)
        };
        let diverged = PullSkill {
            conflict: Some(Conflict {
                remote_version_id: "77".repeat(32),
                local_version_id: None,
            }),
            ..row("deploy", PullAction::Diverged)
        };
        let merged = PullSkill {
            merge: Some(merge_report(true, Vec::new())),
            ..row("runbook", PullAction::Merged)
        };
        let conflicted = PullSkill {
            merge: Some(merge_report(
                false,
                vec![ConflictPathReport {
                    path: "SKILL.md".to_owned(),
                    kind: ConflictPathKind::Content,
                }],
            )),
            ..row("api-notes", PullAction::Conflicted)
        };
        let data = PullData {
            skills: vec![
                row("style", PullAction::UpToDate),
                row("ffwd", PullAction::FastForwarded),
                offered,
                diverged,
                merged,
                conflicted,
                row("pinned", PullAction::Held),
            ],
            proposals_awaiting: 2,
            notices: Vec::new(),
            sync: Vec::new(),
        };
        let out = pull_tty(&data, &[]);

        // Offered: the short hash + the accept command.
        assert!(out.contains("docs"), "{out}");
        assert!(
            out.contains("update offered @ab12cd34ef56 — run `topos update docs`"),
            "{out}"
        );
        // Fast-forwarded names the new generation.
        assert!(out.contains("fast-forwarded — now at (1,2)"), "{out}");
        // Diverged: both the merge command and the disclosed escape.
        assert!(out.contains("`topos update deploy`"), "{out}");
        assert!(
            out.contains("`topos update deploy --onto-current`"),
            "{out}"
        );
        assert!(
            out.contains(&format!("@{}", &"77".repeat(32)[..12])),
            "{out}"
        );
        // Merged points at the review-then-publish next step.
        assert!(out.contains("`topos diff runbook`"), "{out}");
        // Conflicted: the resolving command + the conflicting path checklist.
        assert!(
            out.contains("`topos update api-notes --onto-current`"),
            "{out}"
        );
        assert!(out.contains("SKILL.md (content"), "{out}");
        assert!(out.contains("publish is blocked"), "{out}");
        // Held says it is pinned by a local go-back and how to resume.
        assert!(out.contains("held — pinned at a local go-back"), "{out}");
        assert!(out.contains("`topos update pinned`"), "{out}");
        // Up-to-date rows stay compact: counted in the summary, no `style` action row.
        assert!(!out.contains("style  up to date"), "{out}");
        assert!(
            out.contains("Checked 7 followed skill(s): 1 up to date."),
            "{out}"
        );
        // The reviewer-queue trailer.
        assert!(out.contains("2 proposal(s) awaiting review"), "{out}");
        assert!(
            out.contains("`topos review <skill>@<hash> --approve`"),
            "{out}"
        );
    }

    #[test]
    fn pull_tty_compact_when_everything_is_current_and_loud_on_warnings() {
        // All current → one summary line, no per-skill rows.
        let clean = PullData {
            skills: vec![
                row("a", PullAction::UpToDate),
                row("b", PullAction::UpToDate),
            ],
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
        };
        assert_eq!(
            pull_tty(&clean, &[]),
            "Checked 2 followed skill(s) — all up to date."
        );
        // Nothing followed at all.
        let empty = PullData {
            skills: Vec::new(),
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
        };
        assert_eq!(pull_tty(&empty, &[]), "No followed skills.");
        // A failed skill renders visibly and is counted (even when every synced row was current).
        let warnings = vec!["IO_ERROR s_docs: a filesystem operation failed".to_owned()];
        let out = pull_tty(&clean, &warnings);
        assert!(
            out.contains("warning: IO_ERROR s_docs: a filesystem operation failed"),
            "{out}"
        );
        assert!(
            out.contains("Checked 3 followed skill(s): 2 up to date, 1 failed."),
            "{out}"
        );
    }

    #[test]
    fn list_tty_groups_by_workspace_and_shows_follow_state() {
        let entry = |name: &str, draft: bool, ws: Option<&str>| SkillEntry {
            skill: name.to_owned(),
            workspace_id: ws.map(str::to_owned),
            version_id: "ab".repeat(32),
            bundle_digest: "cd".repeat(32),
            draft,
            pending_proposals: Vec::new(),
            source: None,
            status: None,
            cause: None,
        };
        let mut docs = entry("docs", false, Some("w_acme"));
        docs.pending_proposals = vec![format!("docs@{}", "ef".repeat(32))];
        let out = ListOutcome {
            data: ListData {
                followed: vec![docs.clone()],
                published_by_you: Vec::new(),
                // Two workspace skills (one paused) + one purely-local skill.
                tracked: vec![
                    docs,
                    entry("paused", false, Some("w_acme")),
                    entry("local", true, None),
                ],
                untracked: Vec::new(),
                remote_available: Vec::new(),
                footprint: None,
            },
            warnings: Vec::new(),
            enrollment: Some(ListEnrollment {
                workspace_labels: vec![("w_acme".to_owned(), "Acme".to_owned())],
                base_url: "https://topos.example".to_owned(),
                hook_active: true,
                notes: vec![
                    Some(FollowNote {
                        mode: "auto",
                        following: true,
                    }),
                    Some(FollowNote {
                        mode: "confirm-each",
                        following: false,
                    }),
                    None,
                ],
            }),
        };
        let text = list_tty(&out);
        // The header names the plane + hook; the workspace names move to the group headers.
        assert!(
            text.starts_with("Enrolled at https://topos.example — currency hook: active"),
            "{text}"
        );
        // The workspace group is named by its membership display label; the local skills group separately.
        assert!(text.contains("\nAcme:\n"), "{text}");
        assert!(text.contains("\nlocal (not shared):\n"), "{text}");
        // The Acme group holds the followed + the paused rows (before the local group's line).
        let acme_at = text.find("Acme:").unwrap();
        let local_at = text.find("local (not shared):").unwrap();
        assert!(
            acme_at < local_at,
            "workspace group precedes local:\n{text}"
        );
        assert!(text.contains("docs@ababababab"), "{text}");
        assert!(text.contains("(following, auto)"), "{text}");
        assert!(
            text.contains("paused@")
                && text.contains("(not following — `topos follow paused` resumes)"),
            "{text}"
        );
        // A purely local skill sits under the local group with no follow note; its draft flag still shows.
        assert!(
            text[local_at..].contains("local@") && text.contains("(draft)"),
            "{text}"
        );
        // The open proposal prints IN FULL — the copy-paste surface for `review`.
        assert!(
            text.contains(&format!("docs@{}", "ef".repeat(32))),
            "{text}"
        );
        assert!(text.contains("`topos review docs@"), "{text}");

        // Unenrolled: the header disappears and the output matches the accountless view.
        let unenrolled = ListOutcome {
            data: ListData::default(),
            enrollment: None,
            warnings: Vec::new(),
        };
        assert_eq!(list_tty(&unenrolled), "No tracked skills.");
    }

    #[test]
    fn list_tty_renders_the_remote_catalog_grouped_and_honest() {
        use topos_types::results::{RemoteFollowState, RemoteSkillEntry};

        let remote = |skill: &str, ws: &str, state| RemoteSkillEntry {
            skill_id: skill.to_owned(),
            workspace_id: ws.to_owned(),
            display_name: Some(skill.to_owned()),
            version_id: "ab".repeat(32),
            bundle_digest: "cd".repeat(32),
            open_proposals: 0,
            state,
        };
        let out = ListOutcome {
            data: ListData {
                remote_available: vec![
                    remote("deploy", "w_acme", RemoteFollowState::Available),
                    remote("runbook", "w_acme", RemoteFollowState::Following),
                    remote("audit", "w_acme", RemoteFollowState::FollowingBehind),
                ],
                ..ListData::default()
            },
            enrollment: Some(ListEnrollment {
                workspace_labels: vec![("w_acme".to_owned(), "Acme".to_owned())],
                base_url: "https://topos.example".to_owned(),
                hook_active: true,
                notes: Vec::new(),
            }),
            warnings: vec![
                "could not read the catalog for workspace Beta (plane unreachable) — skipped"
                    .to_owned(),
            ],
        };
        let text = list_tty(&out);
        assert!(text.contains("Remote catalog:"), "{text}");
        // Grouped under the workspace's membership label.
        assert!(text.contains("  Acme:\n"), "{text}");
        // Available is honest — it does NOT print `topos follow`.
        assert!(text.contains("deploy@abababababab  (available)"), "{text}");
        assert!(!text.contains("topos follow deploy"), "{text}");
        assert!(text.contains("runbook@abababababab  (following)"), "{text}");
        // Behind points at `topos update` (the real advance path).
        assert!(
            text.contains("audit@abababababab  (update available — run `topos update audit`)"),
            "{text}"
        );
        // The per-workspace degradation warning surfaces.
        assert!(
            text.contains("warning: could not read the catalog for workspace Beta"),
            "{text}"
        );
    }

    #[test]
    fn log_tty_renders_columns_and_falls_back_to_raw_json() {
        let data = LogData {
            events: vec![
                serde_json::json!({
                    "action": "add",
                    "skill_id": "topos_t00",
                    "name": "pr-describe",
                    "version_id": "ab".repeat(32),
                    "at": 1_700_000_000_000u64,
                }),
                serde_json::json!({
                    "action": "version",
                    "version_id": "cd".repeat(32),
                    "author": "d_test",
                    "message": "topos: publish",
                    "parents": [],
                }),
                serde_json::json!({
                    "action": "error",
                    "verb": "pull",
                    "code": "IO_ERROR",
                    "detail": "open /x/y: denied\nsecond line never shows",
                    "at": 1_700_000_000_000u64,
                }),
                // The event schema is deliberately open — no `action` string means raw-JSON fallback.
                serde_json::json!({ "unknown": true }),
            ],
            team: None,
            archived_successor: None,
        };
        let out = log_tty(&data);
        // Columns: human timestamp, action, name, short id.
        assert!(
            out.contains("2023-11-14 22:13  add  pr-describe  @abababababab"),
            "{out}"
        );
        // A git version event (no `at`) keeps columns with a blank stamp + the author and message.
        assert!(
            out.contains("version  d_test  @cdcdcdcdcdcd  topos: publish"),
            "{out}"
        );
        // The error event is readable: verb, code, FIRST line of detail only.
        assert!(
            out.contains("error  pull [IO_ERROR] open /x/y: denied"),
            "{out}"
        );
        assert!(!out.contains("second line"), "{out}");
        // Unknown shapes fall back to their raw JSON line — never dropped.
        assert!(out.contains("{\"unknown\":true}"), "{out}");
    }

    #[test]
    fn group_fingerprint_chunks_hex_into_fours() {
        assert_eq!(group_fingerprint("e4aaf52f5c391ce9"), "e4aa f52f 5c39 1ce9");
        assert_eq!(group_fingerprint(""), "");
        // A non-multiple-of-four length keeps the trailing short chunk (never panics).
        assert_eq!(group_fingerprint("abcdef"), "abcd ef");
    }

    #[test]
    fn follow_tty_pending_discloses_the_grouped_fingerprint() {
        use topos_types::results::{EnrollmentPending, FollowData};

        let data = FollowData {
            workspace_id: "w_acme".to_owned(),
            enrolled: false,
            skills: Vec::new(),
            deployment_mode: None,
            workspace_display_name: Some("Acme Inc".to_owned()),
            verified_domain: None,
            verified_domain_status: None,
            plane_base_url: Some("https://api.topos.sh".to_owned()),
            pending: Some(EnrollmentPending {
                verification_uri_complete: "https://topos.sh/verify/WXYZ-1234".to_owned(),
                user_code: "WXYZ-1234".to_owned(),
                device_fingerprint: "e4aaf52f5c391ce9".to_owned(),
                expires_at: None,
            }),
            currency: None,
        };
        let text = follow_tty(&data, &[]);
        // The fingerprint prints GROUPED in fours for eyeball comparison against the verification page.
        assert!(text.contains("e4aa f52f 5c39 1ce9"), "{text}");
        assert!(text.contains("confirm it matches the page"), "{text}");
        // The clickable URL is surfaced; the bare user_code is NOT shown on its own line (it rides inside
        // the URL — clicked, never typed).
        assert!(text.contains("https://topos.sh/verify/WXYZ-1234"), "{text}");
        assert!(
            !text.contains("\n  code:"),
            "code line should be gone: {text}"
        );
    }
}
