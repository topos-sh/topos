//! Two presentations of one typed outcome: the `--json` envelope (the agent surface) and a thin TTY
//! renderer. Error messages are summarized so a raw git/io string never reaches a user surface.

use topos_types::results::{AddData, DiffData, ListData, LogData, PullData};
use topos_types::{
    ActionCode, Affected, JsonEnvelope, NextAction, SCHEMA_VERSION, TerminalOutcome, TriggerState,
    WireError,
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
            current_generation: None,
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
        _ => Vec::new(),
    }
}

/// A clean, leak-free summary for a user surface — variants whose `Display` could embed a raw serde / io
/// / git string or a host path get a fixed message; the inner detail stays in logs only.
pub(crate) fn safe_message(err: &ClientError) -> String {
    match err {
        ClientError::Io(_) => "a filesystem operation failed".to_owned(),
        ClientError::Gitstore(_) => "the embedded git store reported an error".to_owned(),
        ClientError::Verify(_) => "an integrity check failed".to_owned(),
        ClientError::Corrupt(_) => "a sidecar document is corrupt".to_owned(),
        ClientError::Scan(_) => "the skill directory was rejected".to_owned(),
        // The remaining Display strings are fixed text or a user-supplied name — safe to show verbatim.
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
    // Disclose the one write `add` makes outside ~/.topos/ — the session-start currency hook — honestly
    // (it is plumbing: it runs a no-op `pull` until the sync engine lands; never "it auto-updates").
    if let Some(report) = &data.currency {
        out.push_str(match report.state {
            TriggerState::Active => {
                "\nInstalled the session-start currency hook (runs `topos pull` at session start)."
            }
            TriggerState::AlreadyPresentUnmanaged => {
                "\nLeft your existing `topos pull` session-start hook untouched."
            }
            TriggerState::Degraded => {
                "\nCouldn't update settings.json for the currency hook — left it untouched."
            }
            TriggerState::Inactive => "",
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
        out.push_str(match report.state {
            TriggerState::Inactive => "Scrubbed the session-start currency hook.\n",
            TriggerState::AlreadyPresentUnmanaged => {
                "Left your own (unmanaged) `topos pull` hook in place.\n"
            }
            TriggerState::Degraded => {
                "Couldn't scrub the currency hook from settings.json — remove it by hand if present.\n"
            }
            TriggerState::Active => "",
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
