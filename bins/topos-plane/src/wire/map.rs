//! The domain ⇄ wire mappers — the ONE place a [`SetCurrentReceipt`] becomes a canonical [`JsonEnvelope`], a
//! [`WireCandidate`] becomes a [`CandidateUpload`], and a [`VersionMeta`] becomes a [`WireVersionMeta`]. A
//! handler NEVER builds these by hand (no string-format drift, one home for the outcome→action policy).

use base64::Engine as _;
use plane_store::{CandidateUpload, CommitId, SetCurrentReceipt, UploadedFile, VersionMeta};
use topos_types::requests::{WireCandidate, WireVersionFile, WireVersionMeta};
use topos_types::{
    ActionCode, Affected, JsonEnvelope, NextAction, Receipt, SCHEMA_VERSION, SignedCurrentRecord,
    TerminalOutcome, WireError,
};

use super::error::PlaneHttpError;

/// Build the canonical [`JsonEnvelope`] for a returned pointer-move/contribute [`SetCurrentReceipt`].
///
/// HTTP status is ALWAYS 200 for a returned receipt — EVERY protocol outcome rides in the body. `ok` is true
/// for `OK` / `NEEDS_REVIEW`; on a failure outcome a flat [`WireError`] carries the code + retryability + the
/// right next-actions (mirrored onto the envelope). On `OK` the parsed `SignedCurrentRecord` lands in `data`
/// (so a future client can advance its anti-rollback floor from the response); otherwise `data` is `{}`.
pub(crate) fn write_envelope(receipt: &SetCurrentReceipt, ws: &str) -> JsonEnvelope {
    let outcome = receipt.outcome;
    let version_hex = receipt.version_id.map(|c| hex::encode(c.0));
    let command = wire_command(&receipt.command).to_owned();

    let wire_receipt = Receipt {
        schema_version: SCHEMA_VERSION,
        op_id: receipt.op_id.clone(),
        command: command.clone(),
        outcome,
        workspace_id: ws.to_owned(),
        skill_id: Some(receipt.skill_id.clone()),
        version_id: version_hex.clone(),
        bundle_digest: receipt.bundle_digest.map(hex::encode),
        expected_generation: Some(receipt.expected),
        current_generation: receipt.current,
        created_at: receipt.created_at.clone(),
        key_id: receipt.key_id.clone(),
        details: receipt.details.clone(),
    };

    let ok = matches!(outcome, TerminalOutcome::Ok | TerminalOutcome::NeedsReview);

    // On OK, surface the signed record in `data` (the client advances its floor from it); else `data = {}`.
    let data = if outcome == TerminalOutcome::Ok {
        receipt
            .signed_record
            .as_ref()
            .and_then(|bytes| serde_json::from_slice::<SignedCurrentRecord>(bytes).ok())
            .and_then(|record| serde_json::to_value(record).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // A failure outcome carries the flat WireError (mirrored onto the envelope's next_actions); OK /
    // NEEDS_REVIEW carry neither an error nor next-actions.
    let (error, next_actions) = if ok {
        (None, vec![])
    } else {
        let actions = next_actions_for(outcome);
        let error = WireError {
            code: error_code(receipt),
            outcome,
            retryable: retryable(outcome),
            affected: Affected {
                workspace: Some(ws.to_owned()),
                skill: Some(receipt.skill_id.clone()),
                version: version_hex,
                proposal: None,
            },
            expected_generation: Some(receipt.expected),
            current_generation: receipt.current,
            context: receipt
                .details
                .clone()
                .unwrap_or_else(|| serde_json::json!({})),
            next_actions: actions.clone(),
        };
        (Some(error), actions)
    };

    JsonEnvelope {
        schema_version: SCHEMA_VERSION,
        command,
        ok,
        data,
        warnings: vec![],
        next_actions,
        receipt: Some(wire_receipt),
        error,
    }
}

/// Map an inbound [`WireCandidate`] to the authority's [`CandidateUpload`]: base64-decode each file's bytes
/// (the server then rehashes them), hex-decode each parent into a [`CommitId`], translate the modes.
pub(crate) fn candidate_to_domain(c: WireCandidate) -> Result<CandidateUpload, PlaneHttpError> {
    let mut files = Vec::with_capacity(c.files.len());
    for f in c.files {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(f.content_base64.as_bytes())
            .map_err(|_| {
                PlaneHttpError::BadBody(format!("file {:?}: invalid base64 content", f.path))
            })?;
        files.push(UploadedFile {
            path: f.path,
            mode: super::domain_mode(f.mode),
            bytes,
        });
    }
    let mut parents = Vec::with_capacity(c.parents.len());
    for p in c.parents {
        let bytes = super::hex32(&p)
            .ok_or_else(|| PlaneHttpError::BadBody(format!("invalid parent commit id {p:?}")))?;
        parents.push(CommitId(bytes));
    }
    Ok(CandidateUpload {
        files,
        parents,
        author: c.author,
        message: c.message,
    })
}

/// Map a [`VersionMeta`] to its wire shape — hex-encode each 32-byte id, translate each file mode.
pub(crate) fn version_meta_to_wire(meta: VersionMeta) -> WireVersionMeta {
    WireVersionMeta {
        version_id: hex::encode(meta.version_id),
        parents: meta.parents.iter().map(hex::encode).collect(),
        author: meta.author,
        message: meta.message,
        bundle_digest: hex::encode(meta.bundle_digest),
        files: meta
            .files
            .into_iter()
            .map(|f| WireVersionFile {
                path: f.path,
                mode: super::wire_mode(f.mode),
                object_id: hex::encode(f.object_id),
            })
            .collect(),
    }
}

/// The CLI verb a domain command string maps to (the envelope's `command`).
fn wire_command(domain: &str) -> &str {
    match domain {
        "publish-direct" | "publish-propose" => "publish",
        "revert" => "revert",
        "review-approve" | "review-reject" => "review",
        other => other,
    }
}

/// The next-actions for a failure outcome (each with an empty `argv` — the client maps the `code` to its own
/// command; the plane does not know the client's local skill name or invocation).
fn next_actions_for(outcome: TerminalOutcome) -> Vec<NextAction> {
    let codes = match outcome {
        TerminalOutcome::Conflict => vec![ActionCode::RebaseAndRetry],
        TerminalOutcome::ApprovalRequired => vec![ActionCode::ProposePublish],
        TerminalOutcome::Denied => vec![ActionCode::RequestAccess, ActionCode::ContactAdmin],
        TerminalOutcome::Unavailable | TerminalOutcome::RetryableFailure => vec![ActionCode::Retry],
        _ => vec![],
    };
    codes
        .into_iter()
        .map(|code| NextAction { code, argv: vec![] })
        .collect()
}

/// Whether a failure outcome is worth a blind retry.
fn retryable(outcome: TerminalOutcome) -> bool {
    matches!(
        outcome,
        TerminalOutcome::Conflict
            | TerminalOutcome::Unavailable
            | TerminalOutcome::RetryableFailure
    )
}

/// The `WireError.code`: prefer a richer code the authority stamped into `details` (e.g.
/// `FIRST_PARENT_MISMATCH` on a `DENIED`), else the outcome's default code.
fn error_code(receipt: &SetCurrentReceipt) -> String {
    receipt
        .details
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(|c| c.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| default_code(receipt.outcome).to_owned())
}

fn default_code(outcome: TerminalOutcome) -> &'static str {
    match outcome {
        TerminalOutcome::Ok => "OK",
        TerminalOutcome::ApprovalRequired => "APPROVAL_REQUIRED",
        TerminalOutcome::NeedsReview => "NEEDS_REVIEW",
        TerminalOutcome::Conflict => "CONFLICT",
        TerminalOutcome::Diverged => "DIVERGED",
        TerminalOutcome::Denied => "DENIED",
        TerminalOutcome::Unavailable => "UNAVAILABLE",
        TerminalOutcome::AmbiguousName => "AMBIGUOUS_NAME",
        TerminalOutcome::KeyRepinRequired => "KEY_REPIN_REQUIRED",
        TerminalOutcome::RetryableFailure => "RETRYABLE_FAILURE",
        TerminalOutcome::PermanentFailure => "PERMANENT_FAILURE",
    }
}
