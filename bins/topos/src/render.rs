//! Two presentations of one typed outcome: the `--json` envelope (the agent surface) and a thin TTY
//! renderer. Error messages are summarized so a raw git/io string never reaches a user surface.

use topos_types::bootstrap::VerifiedDomainStatus;
use topos_types::persisted::ConflictPathKind;
use topos_types::results::{
    AddData, DiffData, FollowData, InviteData, LogData, ProposeData, PublishData, PullData,
    PullSkill, RevertData, ReviewData, ReviewDecision, UnfollowData,
};
use topos_types::{
    ActionCode, Affected, CurrencyKind, JsonEnvelope, NextAction, SCHEMA_VERSION, TerminalOutcome,
    TriggerState, WireError,
};

use crate::error::ClientError;
use crate::ops::{ListOutcome, UninstallOutcome};

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

pub(crate) fn list_tty(out: &ListOutcome) -> String {
    let data = &out.data;
    let mut s = String::new();
    // The enrollment header — the "am I enrolled, is the hook armed" disclosure. Rendered only when
    // enrolled; the unenrolled output is byte-identical to the accountless local list.
    if let Some(e) = &out.enrollment {
        s.push_str(&format!(
            "Enrolled in {} at {} — currency hook: {}\n",
            e.workspace,
            e.base_url,
            if e.hook_active {
                "active"
            } else {
                "not installed"
            }
        ));
    }
    if data.tracked.is_empty() {
        s.push_str("No tracked skills.");
        return s;
    }
    s.push_str("Tracked skills:\n");
    for (i, e) in data.tracked.iter().enumerate() {
        // The row's follow state (aligned with `tracked` by construction): following + mode, or a
        // retained-but-paused entry that `topos follow` resumes. A purely local skill has no note.
        let note = out
            .enrollment
            .as_ref()
            .and_then(|en| en.notes.get(i))
            .and_then(Option::as_ref);
        let follow_note = match note {
            Some(n) if n.following => format!("  (following, {})", n.mode),
            Some(_) => "  (not following — `topos follow` resumes)".to_owned(),
            None => String::new(),
        };
        s.push_str(&format!(
            "  {}  {}@{}{}{}\n",
            e.skill,
            e.skill,
            short(&e.version_id),
            follow_note,
            if e.draft { "  (draft)" } else { "" }
        ));
        // Open proposals print IN FULL — this is the surface a reviewer copies the hash from.
        for p in &e.pending_proposals {
            s.push_str(&format!(
                "    open proposal {p} — run `topos review {p} --approve` (or `--reject`)\n"
            ));
        }
    }
    if let Some(footprint) = &data.footprint {
        s.push_str(&format!(
            "Footprint: {} paths under the topos home\n",
            footprint.len()
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
    if data.events.is_empty() {
        return "No history.".to_owned();
    }
    let mut out = String::new();
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
fn civil_from_days(z: i64) -> (i64, u32, u32) {
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

/// The human `pull` view — one line per skill that needs attention (gh-status style: name, what
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
                format!("update offered @{v} — run `topos pull {name}`"),
                Vec::new(),
            )
        }
        PullAction::Diverged => {
            let v = s
                .conflict
                .as_ref()
                .map(|c| short(&c.remote_version_id))
                .unwrap_or("?");
            (
                format!(
                    "diverged from the new current @{v} — your local draft is kept; run \
                     `topos pull {name}` to merge it (or `topos pull {name} --onto-current` to \
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
                     then run `topos pull {name} --onto-current` to commit your resolution \
                     (publish is blocked until then)"
                ),
                extra,
            )
        }
        PullAction::Held => (
            format!(
                "held — pinned at ({},{}) by a local go-back; run `topos pull {name}` to resume \
                 following current",
                s.applied.epoch, s.applied.seq
            ),
            Vec::new(),
        ),
        PullAction::Alarm => (
            String::from(
                "INTEGRITY ALARM — the plane's record for this skill failed verification or \
                 reuses a generation for different bytes; nothing was applied and your \
                 last-known-good copy is kept. Contact your workspace owner before pulling again.",
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

#[cfg(test)]
mod tests {
    use topos_types::Generation;
    use topos_types::persisted::ConflictPathKind;
    use topos_types::results::{
        Conflict, ConflictPathReport, ListData, LogData, MergeReport, Offer, PullAction, PullData,
        PullSkill, SkillEntry,
    };

    use crate::ops::{FollowNote, ListEnrollment, ListOutcome};

    use super::{list_tty, log_tty, pull_tty};

    fn g(epoch: u64, seq: u64) -> Generation {
        Generation { epoch, seq }
    }

    fn row(name: &str, action: PullAction) -> PullSkill {
        PullSkill {
            skill: name.to_owned(),
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
                row("audit", PullAction::Alarm),
            ],
            proposals_awaiting: 2,
        };
        let out = pull_tty(&data, &[]);

        // Offered: the short hash + the accept command.
        assert!(out.contains("docs"), "{out}");
        assert!(
            out.contains("update offered @ab12cd34ef56 — run `topos pull docs`"),
            "{out}"
        );
        // Fast-forwarded names the new generation.
        assert!(out.contains("fast-forwarded — now at (1,2)"), "{out}");
        // Diverged: both the merge command and the disclosed escape.
        assert!(out.contains("`topos pull deploy`"), "{out}");
        assert!(out.contains("`topos pull deploy --onto-current`"), "{out}");
        assert!(
            out.contains(&format!("@{}", &"77".repeat(32)[..12])),
            "{out}"
        );
        // Merged points at the review-then-publish next step.
        assert!(out.contains("`topos diff runbook`"), "{out}");
        // Conflicted: the resolving command + the conflicting path checklist.
        assert!(
            out.contains("`topos pull api-notes --onto-current`"),
            "{out}"
        );
        assert!(out.contains("SKILL.md (content"), "{out}");
        assert!(out.contains("publish is blocked"), "{out}");
        // Held says what is pinned and how to resume.
        assert!(out.contains("held — pinned at (1,2)"), "{out}");
        assert!(out.contains("`topos pull pinned`"), "{out}");
        // The alarm line is LOUD and names the integrity alarm.
        assert!(out.contains("INTEGRITY ALARM"), "{out}");
        assert!(out.contains("last-known-good"), "{out}");
        // Up-to-date rows stay compact: counted in the summary, no `style` action row.
        assert!(!out.contains("style  up to date"), "{out}");
        assert!(
            out.contains("Checked 8 followed skill(s): 1 up to date."),
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
        };
        assert_eq!(
            pull_tty(&clean, &[]),
            "Checked 2 followed skill(s) — all up to date."
        );
        // Nothing followed at all.
        let empty = PullData {
            skills: Vec::new(),
            proposals_awaiting: 0,
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
    fn list_tty_shows_enrollment_header_and_follow_state() {
        let entry = |name: &str, draft: bool| SkillEntry {
            skill: name.to_owned(),
            version_id: "ab".repeat(32),
            bundle_digest: "cd".repeat(32),
            draft,
            pending_proposals: Vec::new(),
        };
        let mut narrowed = entry("docs", false);
        narrowed.pending_proposals = vec![format!("docs@{}", "ef".repeat(32))];
        let out = ListOutcome {
            data: ListData {
                followed: vec![narrowed.clone()],
                published_by_you: Vec::new(),
                tracked: vec![narrowed, entry("paused", false), entry("local", true)],
                untracked: Vec::new(),
                footprint: None,
            },
            enrollment: Some(ListEnrollment {
                workspace: "Acme".to_owned(),
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
        assert!(
            text.starts_with("Enrolled in Acme at https://topos.example — currency hook: active"),
            "{text}"
        );
        assert!(text.contains("docs@ababababab"), "{text}");
        assert!(text.contains("(following, auto)"), "{text}");
        assert!(
            text.contains("paused@") && text.contains("(not following — `topos follow` resumes)"),
            "{text}"
        );
        // A purely local skill carries no follow note; its draft flag still shows.
        assert!(
            text.contains("local@") && text.contains("(draft)"),
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
        };
        assert_eq!(list_tty(&unenrolled), "No tracked skills.");
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
}
