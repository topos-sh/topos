//! Two presentations of one typed outcome: the `--json` envelope (the agent surface) and a thin TTY
//! renderer. Error messages are summarized so a raw git/io string never reaches a user surface.

use topos_types::bootstrap::VerifiedDomainStatus;
use topos_types::results::{
    AddData, DiffData, FollowData, InviteData, ListData, LogData, ProposeData, PublishData,
    PullData, RevertData, ReviewData, ReviewDecision, UnfollowData,
};
use topos_types::{
    ActionCode, Affected, CurrencyKind, JsonEnvelope, NextAction, SCHEMA_VERSION, TerminalOutcome,
    TriggerState, WireError,
};

use crate::error::ClientError;
use crate::ops::UninstallOutcome;

/// A success envelope wrapping a verb's typed `data`.
pub(crate) fn ok_envelope(command: &str, data: serde_json::Value) -> JsonEnvelope {
    JsonEnvelope {
        schema_version: SCHEMA_VERSION,
        command: command.to_owned(),
        ok: true,
        data,
        warnings: Vec::new(),
        next_actions: Vec::new(),
        receipt: None,
        error: None,
    }
}

/// A failure envelope carrying the stable code, outcome, and machine-actionable next steps.
pub(crate) fn err_envelope(command: &str, err: &ClientError) -> JsonEnvelope {
    let outcome = err.outcome();
    let next_actions = next_actions(err);
    let retryable = matches!(
        outcome,
        TerminalOutcome::RetryableFailure | TerminalOutcome::Unavailable
    );
    JsonEnvelope {
        schema_version: SCHEMA_VERSION,
        command: command.to_owned(),
        ok: false,
        data: serde_json::json!({}),
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
        ClientError::AmbiguousName { .. } => vec![NextAction {
            code: ActionCode::DisambiguateName,
            argv: vec!["topos".into(), "list".into(), "--json".into()],
        }],
        // A pinned-key change is not self-service in v0 — surface the repin action code.
        ClientError::KeyRepinRequired => vec![NextAction {
            code: ActionCode::RepinPlaneKey,
            argv: vec!["topos".into(), "list".into(), "--json".into()],
        }],
        // The plane refused a direct publish under review-required — the agent re-runs it as a proposal.
        // The CLIENT fills the executable argv (the plane sends an empty one — it doesn't know the local
        // skill name); never an auto-flip.
        ClientError::ApprovalRequired { skill, digest } => vec![NextAction {
            code: ActionCode::ProposePublish,
            argv: vec![
                "topos".into(),
                "publish".into(),
                skill.clone(),
                "--propose".into(),
                "--approve".into(),
                format!("{skill}@{digest}"),
                "--json".into(),
            ],
        }],
        // A stale base — pull to rebase, then re-show the diff and retry. Never a silent retry.
        ClientError::Conflict { skill, .. } => vec![NextAction {
            code: ActionCode::RebaseAndRetry,
            argv: vec![
                "topos".into(),
                "pull".into(),
                skill.clone(),
                "--json".into(),
            ],
        }],
        // An unresolved author merge blocks publish — resolve it (the pull surfaces/runs the resolution).
        ClientError::PublishBlocked { skill } => vec![NextAction {
            code: ActionCode::ResolveDivergedDraft,
            argv: vec![
                "topos".into(),
                "pull".into(),
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
        // A retryable plane outcome (e.g. a not-yet-committed lease) — re-run the same command. The agent
        // owns the argv (this surface doesn't carry the verb); a permanent one carries no Retry.
        ClientError::PlaneTerminal {
            retryable: true, ..
        } => vec![NextAction {
            code: ActionCode::Retry,
            argv: Vec::new(),
        }],
        _ => Vec::new(),
    }
}

/// The success-path next actions for `follow`: a pending enrollment ⇒ run `follow --resume`; a completed
/// enrollment that disclosed offers ⇒ `pull` to surface/place them.
pub(crate) fn follow_next_actions(data: &FollowData) -> Vec<NextAction> {
    if data.pending.is_some() {
        return vec![NextAction {
            // An OPEN action code (carries the executable argv); no schema change to the closed set.
            code: ActionCode::from("ENROLL_RESUME".to_owned()),
            argv: vec![
                "topos".into(),
                "follow".into(),
                "--resume".into(),
                "--json".into(),
            ],
        }];
    }
    if data.enrolled && !data.skills.is_empty() {
        return vec![NextAction {
            code: ActionCode::ApplyWaitingUpdate,
            argv: vec!["topos".into(), "pull".into(), "--json".into()],
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
    // Disclose the one write `add` makes outside ~/.topos/ — the currency trigger — honestly (it is
    // plumbing: it runs a no-op `pull` until something is followed; never "it auto-updates"). The copy
    // branches on the report's `currency_kind` so a harness's honest update moment is never overstated
    // (a session-start hook fires at session start; an inject surface only on the first `topos` touch).
    if let Some(report) = &data.currency {
        out.push_str(match (report.state, report.currency_kind) {
            (TriggerState::Active, CurrencyKind::SessionStart) => {
                "\nInstalled the session-start currency hook (runs `topos pull` at session start)."
            }
            (TriggerState::Active, CurrencyKind::FirstToposTouch) => {
                "\nInstalled the currency trigger (updates surface on the first `topos` touch)."
            }
            (TriggerState::Active, CurrencyKind::FirstTurn) => {
                "\nInstalled the currency trigger (updates surface on the first turn)."
            }
            (TriggerState::Active, CurrencyKind::ExplicitPullOnly) => {
                "\nNo automatic currency trigger — run `topos pull` to check for updates."
            }
            (TriggerState::AlreadyPresentUnmanaged, CurrencyKind::SessionStart) => {
                "\nLeft your existing `topos pull` session-start hook untouched."
            }
            (TriggerState::AlreadyPresentUnmanaged, _) => {
                "\nLeft your existing `topos pull` currency trigger untouched."
            }
            (TriggerState::Degraded, CurrencyKind::SessionStart) => {
                "\nCouldn't update settings.json for the currency hook — left it untouched."
            }
            (TriggerState::Degraded, _) => {
                "\nCouldn't update the harness config for the currency trigger — left it untouched; run `topos pull` to check for updates."
            }
            (TriggerState::Inactive, _) => "",
        });
    }
    out
}

pub(crate) fn list_tty(data: &ListData) -> String {
    if data.tracked.is_empty() {
        return "No tracked skills.".to_owned();
    }
    let mut out = String::from("Tracked skills:\n");
    for e in &data.tracked {
        out.push_str(&format!(
            "  {}  {}@{}{}\n",
            e.skill,
            e.skill,
            short(&e.version_id),
            if e.draft { "  (draft)" } else { "" }
        ));
    }
    if let Some(footprint) = &data.footprint {
        out.push_str(&format!(
            "Footprint: {} paths under the topos home\n",
            footprint.len()
        ));
    }
    out.trim_end().to_owned()
}

pub(crate) fn diff_tty(data: &DiffData) -> String {
    if data.diff.is_empty() {
        "No changes — the draft matches current.".to_owned()
    } else {
        data.diff.trim_end_matches('\n').to_owned()
    }
}

pub(crate) fn log_tty(data: &LogData) -> String {
    if data.events.is_empty() {
        return "No history.".to_owned();
    }
    let mut out = String::new();
    for e in &data.events {
        let action = e.get("action").and_then(|v| v.as_str()).unwrap_or("event");
        out.push_str(&format!("  {action}  {e}\n"));
    }
    out.trim_end().to_owned()
}

pub(crate) fn uninstall_tty(data: &UninstallOutcome) -> String {
    let mut out = String::new();
    if let Some(footprint) = &data.footprint {
        out.push_str(&format!(
            "Removing {} topos-owned paths:\n",
            footprint.len()
        ));
        for p in footprint {
            out.push_str(&format!("  {p}\n"));
        }
    }
    if let Some(report) = &data.currency {
        out.push_str(match (report.state, report.currency_kind) {
            (TriggerState::Inactive, CurrencyKind::SessionStart) => {
                "Scrubbed the session-start currency hook.\n"
            }
            (TriggerState::Inactive, _) => {
                "Scrubbed the currency trigger from the harness config.\n"
            }
            (TriggerState::AlreadyPresentUnmanaged, _) => {
                "Left your own (unmanaged) `topos pull` hook in place.\n"
            }
            (TriggerState::Degraded, CurrencyKind::SessionStart) => {
                "Couldn't scrub the currency hook from settings.json — remove it by hand if present.\n"
            }
            (TriggerState::Degraded, _) => {
                "Couldn't scrub the currency trigger from the harness config — remove it by hand if present.\n"
            }
            (TriggerState::Active, _) => "",
        });
    }
    out.push_str(if data.home_removed {
        "Removed ~/.topos."
    } else {
        "Nothing to remove (~/.topos absent)."
    });
    if let Some(bin) = &data.binary_removed {
        out.push_str(&format!("\nRemoved the binary at {bin}."));
    }
    out.push_str("\nNo skill bytes were touched.");
    out
}

pub(crate) fn follow_tty(data: &FollowData) -> String {
    // A pending enrollment: surface the verification URL WITH the workspace + verified-domain provenance
    // (the relay-phishing guard — the human checks the domain before approving).
    if let Some(pending) = &data.pending {
        let workspace = data
            .workspace_display_name
            .clone()
            .unwrap_or_else(|| data.workspace_id.clone());
        let mut out = format!("Enrolling with {workspace}");
        if let Some(domain) = &data.verified_domain {
            let status = match data.verified_domain_status {
                Some(VerifiedDomainStatus::Verified) => "verified",
                Some(VerifiedDomainStatus::Pending) => "pending verification",
                _ => "unverified",
            };
            out.push_str(&format!(" ({domain}, {status})"));
        }
        out.push_str(&format!(
            "\nOpen this URL to approve, then run `topos follow --resume`:\n  {}\n  code: {}",
            pending.verification_uri_complete, pending.user_code
        ));
        return out;
    }
    // A completed enrollment.
    if !data.enrolled {
        return format!("Enrolled with workspace {}.", data.workspace_id);
    }
    if data.skills.is_empty() {
        return format!(
            "Enrolled with workspace {} (no skills to follow).",
            data.workspace_id
        );
    }
    let mut out = format!(
        "Enrolled with workspace {}. Offered skills:",
        data.workspace_id
    );
    for s in &data.skills {
        out.push_str(&format!(
            "\n  {}  {}@{}",
            s.name,
            s.name,
            short(&s.offer.version_id)
        ));
    }
    out.push_str(
        "\nApprove a skill with `topos follow --approve <skill>` (or `topos pull <skill>`).",
    );
    out
}

pub(crate) fn invite_tty(data: &InviteData) -> String {
    let mut out = format!("Invite link: {}", data.invite_link);
    if !data.roster_added.is_empty() {
        out.push_str(&format!(
            "\nSeeded onto the roster: {}",
            data.roster_added.join(", ")
        ));
    }
    if data.skills.is_empty() {
        out.push_str("\nA membership-only door (no skills pre-offered).");
    } else {
        out.push_str(&format!("\nPre-offers: {}", data.skills.join(", ")));
    }
    out.push_str("\nShare the link; redeeming it never enrolls on its own.");
    out
}

pub(crate) fn unfollow_tty(data: &UnfollowData) -> String {
    format!(
        "Stopped following {} — auto-updates stop; your local copy is kept, nothing was deleted. \
         `follow` resumes.",
        data.skill_id,
    )
}

pub(crate) fn publish_tty(data: &PublishData) -> String {
    let mut out = format!(
        "Published {}@{} (digest {}) — current is now ({},{}).",
        data.skill_id,
        short(&data.version_id),
        short(&data.bundle_digest),
        data.current_generation.epoch,
        data.current_generation.seq,
    );
    // On a first (genesis) publish that minted a shareable door, surface the link.
    if let Some(link) = &data.invite_link {
        out.push_str(&format!("\nShare this skill: {link}"));
    }
    out
}

pub(crate) fn propose_tty(data: &ProposeData) -> String {
    // Honest: this is NEEDS_REVIEW — a proposal opened, `current` did NOT move.
    format!(
        "Opened proposal {} on base {}. Awaiting review — a reviewer runs `topos review {} --approve`.",
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

pub(crate) fn review_tty(data: &ReviewData) -> String {
    match data.decision {
        ReviewDecision::Approve => {
            let moved_to = data
                .current_generation
                .map(|g| format!("({},{})", g.epoch, g.seq))
                .unwrap_or_else(|| "the new version".to_owned());
            format!(
                "Approved {} — current moved to {moved_to}. Every follower picks it up on their next pull.",
                data.proposal,
            )
        }
        ReviewDecision::Reject => format!(
            "Rejected {}. It will no longer be applied; `current` is unchanged.",
            data.proposal,
        ),
    }
}

pub(crate) fn pull_tty(data: &PullData) -> String {
    if data.skills.is_empty() {
        "No followed skills.".to_owned()
    } else {
        format!("Checked {} followed skill(s).", data.skills.len())
    }
}

pub(crate) fn err_tty(err: &ClientError) -> String {
    format!("error: {}", safe_message(err))
}

fn short(hex: &str) -> &str {
    hex.get(..12).unwrap_or(hex)
}
