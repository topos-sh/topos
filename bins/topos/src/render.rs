//! Two presentations of one typed outcome: the `--json` envelope (the agent surface) and a thin TTY
//! renderer. Error messages are summarized so a raw git/io string never reaches a user surface.

use topos_types::bootstrap::VerifiedDomainStatus;
use topos_types::persisted::ConflictPathKind;
use topos_types::results::{
    AddData, DiffData, FollowData, InviteData, LogData, ProposeData, PublishData, PullData,
    PullSkill, RevertData, ReviewData, ReviewDecision, SkillEntry, UnfollowData, UntrackedEntry,
};
use topos_types::{
    ActionCode, Affected, CurrencyKind, JsonEnvelope, NextAction, TerminalOutcome, TriggerState,
    WIRE_SCHEMA_VERSION, WireError,
};

use crate::error::ClientError;
use crate::ops::{ListOutcome, UninstallOutcome};

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

/// A failure envelope carrying the stable code, outcome, and machine-actionable next steps.
pub(crate) fn err_envelope(command: &str, err: &ClientError) -> JsonEnvelope {
    let outcome = err.outcome();
    let next_actions = next_actions(err);
    let retryable = matches!(
        outcome,
        TerminalOutcome::RetryableFailure | TerminalOutcome::Unavailable
    );
    JsonEnvelope {
        schema_version: WIRE_SCHEMA_VERSION,
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
        // skill name); the `<skill>@<digest>` positional pin re-binds the same bytes; never an auto-flip.
        ClientError::ApprovalRequired { skill, digest } => vec![NextAction {
            code: ActionCode::ProposePublish,
            argv: vec![
                "topos".into(),
                "publish".into(),
                "--propose".into(),
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
        _ => Vec::new(),
    }
}

/// The success-path next actions for `follow`: a pending enrollment ⇒ re-invoke `follow` (re-invoking IS
/// the resume — the pending WAL drives it); a completed enrollment that disclosed offers ⇒ `pull` to
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
        s.push_str("\nUntracked skills — run `topos add <path>` to adopt:\n");
        for u in &data.untracked {
            s.push_str(&untracked_row(u));
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

/// One untracked-discovery row: `<name>  [<harness>]  <path>`, plus an adopt-only note for a harness topos
/// has no full adapter for — it can still be `add`ed (the bytes track + share), but live currency for that
/// harness lands later.
fn untracked_row(u: &UntrackedEntry) -> String {
    let support = if u.adapter_supported {
        ""
    } else {
        "  (adopt-only — live currency lands later)"
    };
    format!(
        "  {}  [{}]  {}{}\n",
        u.name, u.harness_name, u.path, support
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
    let mut s = format!(
        "  {}  {}@{}{}{}\n",
        entry.skill,
        entry.skill,
        short(&entry.version_id),
        follow_note,
        if entry.draft { "  (draft)" } else { "" }
    );
    // Open proposals print IN FULL — this is the surface a reviewer copies the hash from.
    for p in &entry.pending_proposals {
        s.push_str(&format!(
            "    open proposal {p} — run `topos review {p} --approve` (or `--reject`)\n"
        ));
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

pub(crate) fn follow_tty(out: &crate::ops::FollowOutcome) -> String {
    let data = &out.data;
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
        s.push_str(&format!(
            "\nOpen this URL to approve, then re-run `topos follow`:\n  {}\n  code: {}\n  \
             fingerprint: {} (confirm it matches the page before approving)",
            pending.verification_uri_complete,
            pending.user_code,
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
        s.push_str("\nApprove a skill with `topos follow <skill>` (or `topos pull <skill>`).");
        s
    };
    // The resume disclosure: a skill-path follow flipped a paused entry back on (TTY-only; the pinned
    // `FollowData` shape has no resume field).
    for name in &out.resumed {
        s.push_str(&format!(
            "\nResumed following {name} — auto-updates are back on; the next `topos pull` lands the \
             team's current."
        ));
    }
    s
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
    out.push_str(
        "\nTeammates just paste the link to their agent and ask it to follow — the link itself \
         walks the agent through install, sign-in, and landing the skills. Redeeming it never \
         enrolls anyone on its own.",
    );
    out
}

pub(crate) fn unfollow_tty(data: &UnfollowData) -> String {
    format!(
        "Stopped following {} — auto-updates stop; your local copy is kept, nothing was deleted. \
         `topos follow {}` resumes.",
        data.skill_id, data.skill_id,
    )
}

pub(crate) fn publish_tty(data: &PublishData) -> String {
    let mut out = String::new();
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
    // On a first (genesis) publish that minted a shareable door, surface the link.
    if let Some(link) = &data.invite_link {
        out.push_str(&format!(
            "\nShare this skill: {link}\nTeammates just paste that link to their agent and ask it \
             to follow — the link itself walks the agent through install, sign-in, and landing the \
             skill.",
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
    format!(
        "No workspace yet — publishing this first skill creates one.\nOpen this URL, sign in, and \
         approve (you become the workspace owner):\n  {}\n  code: {}\n  fingerprint: {} (confirm it \
         matches the page before approving)\nNothing is published yet; then re-run:\n  {}",
        pending.verification_uri_complete,
        pending.user_code,
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
    fn list_tty_groups_by_workspace_and_shows_follow_state() {
        let entry = |name: &str, draft: bool, ws: Option<&str>| SkillEntry {
            skill: name.to_owned(),
            workspace_id: ws.map(str::to_owned),
            version_id: "ab".repeat(32),
            bundle_digest: "cd".repeat(32),
            draft,
            pending_proposals: Vec::new(),
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
                footprint: None,
            },
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

        use crate::ops::FollowOutcome;

        let out = FollowOutcome {
            data: FollowData {
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
            },
            resumed: Vec::new(),
        };
        let text = follow_tty(&out);
        // The fingerprint prints GROUPED in fours for eyeball comparison against the verification page.
        assert!(text.contains("e4aa f52f 5c39 1ce9"), "{text}");
        assert!(text.contains("confirm it matches the page"), "{text}");
    }
}
