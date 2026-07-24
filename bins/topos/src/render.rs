//! Two presentations of one typed outcome: the `--json` envelope (the agent surface) and a thin TTY
//! renderer. Error messages are summarized so a raw git/io string never reaches a user surface.

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
        | ClientError::HarnessNotFound(_) => vec![crate::actions::next_action(
            ActionCode::DisambiguateName,
            vec!["topos".into(), "list".into(), "--json".into()],
        )],
        // A review verdict on a no-longer-open proposal — point the agent at the open inbox.
        ClientError::ReviewNotOpen(_) => vec![crate::actions::next_action(
            ActionCode::from("REVIEW_INBOX".to_owned()),
            vec!["topos".into(), "review".into(), "--json".into()],
        )],
        // A stale base — update to rebase, then re-show the diff and retry. Never a silent retry.
        ClientError::Conflict { skill, .. } => vec![crate::actions::next_action(
            ActionCode::RebaseAndRetry,
            vec![
                "topos".into(),
                "update".into(),
                skill.clone(),
                "--json".into(),
            ],
        )],
        // An unresolved author merge blocks publish. The block only ever exists with a RECORDED
        // conflict, and a plain `update <skill>` merely RE-DISCLOSES a recorded conflict (it never
        // re-merges) — naming it here sent agents into a publish→update→publish refusal loop. Name
        // the two acts that actually RESOLVE the block: keep yours (`--onto-current` commits your
        // edited resolution — or your original draft — onto current and clears the record) or let
        // the team win (`--reset`, the loss-led two-phase discard).
        ClientError::PublishBlocked { skill } => vec![
            crate::actions::next_action(
                ActionCode::ResolveDivergedDraft,
                vec![
                    "topos".into(),
                    "update".into(),
                    skill.clone(),
                    "--onto-current".into(),
                    "--json".into(),
                ],
            ),
            crate::actions::next_action(
                ActionCode::ResolveDivergedDraft,
                vec![
                    "topos".into(),
                    "update".into(),
                    skill.clone(),
                    "--reset".into(),
                    "--json".into(),
                ],
            ),
        ],
        // A denial is not self-service (ask an owner to invite/roster you, or contact an admin) — the
        // codes carry no executable argv.
        ClientError::Denied(_) => vec![
            crate::actions::next_action(ActionCode::RequestAccess, Vec::new()),
            crate::actions::next_action(ActionCode::ContactAdmin, Vec::new()),
        ],
        // A denied enrollment redeem (authenticated-but-uninvited): the ask-an-owner guidance rides the
        // message; the action code is the existing REQUEST_ACCESS (no argv — the fix is another human's).
        ClientError::EnrollDenied => vec![crate::actions::next_action(
            ActionCode::RequestAccess,
            Vec::new(),
        )],
        // A retryable plane outcome (e.g. a not-yet-committed lease) — re-run the same command. The agent
        // owns the argv (this surface doesn't carry the verb); a permanent one carries no Retry.
        ClientError::PlaneTerminal {
            retryable: true, ..
        } => vec![crate::actions::next_action(ActionCode::Retry, Vec::new())],
        // `topos upgrade` disambiguates to two concrete commands the agent can pick between.
        ClientError::UpgradeAmbiguous => vec![
            crate::actions::next_action(
                ActionCode::from("UPDATE_SKILLS".to_owned()),
                vec!["topos".into(), "update".into(), "--json".into()],
            ),
            crate::actions::next_action(
                ActionCode::from("UPDATE_CLI".to_owned()),
                vec!["topos".into(), "self-update".into()],
            ),
        ],
        // The shared not-enrolled refusal: the join command as a TEMPLATE — its argv carries the
        // `<workspace-address>` placeholder and `needs` names it, so an agent substitutes and runs
        // instead of parsing prose.
        ClientError::NotEnrolled => vec![crate::actions::next_action(
            ActionCode::from("FOLLOW_WORKSPACE".to_owned()),
            vec![
                "topos".into(),
                "follow".into(),
                "<workspace-address>".into(),
                "--json".into(),
            ],
        )],
        // "upgrade topos" in prose is `topos self-update` structurally.
        ClientError::UnknownSchemaVersion { .. } => vec![crate::actions::next_action(
            ActionCode::from("UPDATE_CLI".to_owned()),
            vec!["topos".into(), "self-update".into()],
        )],
        // The bareword-enroll guard (headless refusal and TTY decline alike): the two DELIBERATE
        // spellings, both fully concrete — the explicit address form, and the consented bareword.
        ClientError::BarewordEnrollUnconfirmed { name, server }
        | ClientError::BarewordEnrollDeclined { name, server } => vec![
            crate::actions::next_action(
                ActionCode::from("FOLLOW_WORKSPACE".to_owned()),
                vec![
                    "topos".into(),
                    "follow".into(),
                    format!("{server}/{name}"),
                    "--json".into(),
                ],
            ),
            crate::actions::next_action(
                ActionCode::from("FOLLOW_WORKSPACE".to_owned()),
                vec![
                    "topos".into(),
                    "follow".into(),
                    name.clone(),
                    "--yes".into(),
                    "--json".into(),
                ],
            ),
        ],
        // Divergent per-placement edits: the prose names the loss-led discard; mirror it (the
        // bare `--reset` DESCRIBES — nothing is dropped without its own `--yes`).
        ClientError::PlacementsDiverged { skill, .. } => vec![crate::actions::next_action(
            ActionCode::ResolveDivergedDraft,
            vec![
                "topos".into(),
                "update".into(),
                skill.clone(),
                "--reset".into(),
                "--json".into(),
            ],
        )],
        // The value-carrying fixes, TYPED: the runtime value rides as exactly ONE argv element,
        // straight from the error variant's own field — never re-tokenized out of prose, so a
        // hostile name containing whitespace or a `--flag` fragment stays one inert token.
        ClientError::PathNotName { arg } => vec![crate::actions::next_action(
            ActionCode::from("RUN_COMMAND".to_owned()),
            vec![
                "topos".into(),
                "add".into(),
                format!("./{arg}"),
                "--json".into(),
            ],
        )],
        ClientError::AlreadyTrackedName { name } => vec![crate::actions::next_action(
            ActionCode::from("RUN_COMMAND".to_owned()),
            vec!["topos".into(), "diff".into(), name.clone(), "--json".into()],
        )],
        ClientError::HarnessMismatch { name, .. } => vec![crate::actions::next_action(
            ActionCode::from("RUN_COMMAND".to_owned()),
            vec![
                "topos".into(),
                "publish".into(),
                name.clone(),
                "--json".into(),
            ],
        )],
        ClientError::PlacementOccupied { path } => vec![crate::actions::next_action(
            ActionCode::from("RUN_COMMAND".to_owned()),
            vec!["topos".into(), "add".into(), path.clone(), "--json".into()],
        )],
        // Everything else: a refusal whose PROSE names one of the STATIC command spellings
        // mirrors it structurally. Prose text is never tokenized into argv — see the allowlist.
        other => mirror_prose_commands(&safe_message(other)),
    }
}

/// The STATIC command spellings a refusal's prose may name, each with its READY argv — the whole
/// mirror vocabulary. The fall-through lifts a backticked span ONLY on a byte-exact match against
/// this table, so the argv always comes from HERE (compile-time constants), never from tokenizing
/// message text: prose that interpolates a runtime value (a path, a skill name typed by a user or
/// found on disk) can never smuggle tokens — let alone a `--yes` — into an executable next
/// action. An error whose fix needs a value gets a TYPED arm in [`next_actions`] instead, which
/// pushes the value as one argv element.
const STATIC_PROSE_COMMANDS: &[(&str, &str, &[&str])] = &[
    (
        "topos follow <workspace-address>",
        "FOLLOW_WORKSPACE",
        &["topos", "follow", "<workspace-address>", "--json"],
    ),
    (
        "topos follow <server>/<workspace>",
        "RUN_COMMAND",
        &["topos", "follow", "<server>/<workspace>", "--json"],
    ),
    (
        "topos auth login",
        "SIGN_IN",
        &["topos", "auth", "login", "--json"],
    ),
    (
        "topos follow",
        "RUN_COMMAND",
        &["topos", "follow", "--json"],
    ),
    (
        "topos update",
        "UPDATE_SKILLS",
        &["topos", "update", "--json"],
    ),
    ("topos self-update", "UPDATE_CLI", &["topos", "self-update"]),
];

/// Mirror the STATIC `topos …` commands an error's shown PROSE names into structural next
/// actions — the fall-through rule for the error envelope: whenever a refusal tells a human "run
/// `topos X`", the agent gets the same fix as an executable argv. Backtick-quoted spans are
/// looked up in [`STATIC_PROSE_COMMANDS`] byte-for-byte (a non-matching span mirrors nothing);
/// duplicates collapse. Template placeholders like `<workspace-address>` ride the const argv and
/// surface in `needs`.
fn mirror_prose_commands(message: &str) -> Vec<NextAction> {
    let mut out: Vec<NextAction> = Vec::new();
    let mut rest = message;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else { break };
        let span = &after[..end];
        rest = &after[end + 1..];
        let Some((_, code, argv)) = STATIC_PROSE_COMMANDS.iter().find(|(s, _, _)| *s == span)
        else {
            continue;
        };
        let argv: Vec<String> = argv.iter().map(|t| (*t).to_owned()).collect();
        if out.iter().any(|a| a.argv == argv) {
            continue;
        }
        out.push(crate::actions::next_action(
            ActionCode::from((*code).to_owned()),
            argv,
        ));
    }
    out
}

/// The success-path next actions for `follow`: a pending enrollment ⇒ re-invoke `follow` (re-invoking IS
/// the resume — the pending WAL drives it); a completed enrollment that disclosed offers ⇒ `update` to
/// surface/place them.
pub(crate) fn follow_next_actions(data: &FollowData) -> Vec<NextAction> {
    if data.pending.is_some() {
        // An OPEN action code (carries the executable argv); no schema change to the closed set.
        return vec![crate::actions::next_action(
            ActionCode::from("ENROLL_RESUME".to_owned()),
            vec!["topos".into(), "follow".into(), "--json".into()],
        )];
    }
    if data.enrolled && !data.skills.is_empty() {
        return vec![crate::actions::next_action(
            ActionCode::ApplyWaitingUpdate,
            vec!["topos".into(), "update".into(), "--json".into()],
        )];
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

/// The `login` receipt — pending (the approval instructions) or the completed session, led by the
/// acceptance disclosure (what connecting delivers; silent from here).
pub(crate) fn session_login_tty(data: &topos_types::results::LoginData) -> String {
    if let Some(p) = &data.pending {
        let mut s = format!(
            "Approve this login in your browser:\n  Open: {}\n  Code: {} (the page shows the \
             same code — confirm it matches)\nThen re-run `topos login` to finish.",
            p.verification_uri, p.user_code
        );
        if !data.name.is_empty() {
            s = format!("Logging into '{}'.\n{s}", data.name);
        }
        return s;
    }
    let label = data
        .display_name
        .clone()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| data.name.clone());
    let mut s = format!("Logged into {label}");
    if !data.name.is_empty() && data.display_name.as_deref().is_some_and(|d| d != data.name) {
        s.push_str(&format!(" ({})", data.name));
    }
    match data.session_status.as_str() {
        "pending" => s.push_str(
            " — the session awaits an owner's approval; delivery starts automatically once \
             approved (nothing lands until then).",
        ),
        _ => match data.delivered {
            Some(0) => s.push_str(" — nothing in your profile delivers here yet."),
            Some(n) => s.push_str(&format!(
                " — your profile delivers {n} skill{} here; updates arrive silently (`topos \
                 update` any time).",
                if n == 1 { "" } else { "s" }
            )),
            None => s.push('.'),
        },
    }
    s.push_str(&format!(
        "\nUndo: topos logout{}",
        if data.name.is_empty() {
            String::new()
        } else {
            format!(" {}", data.name)
        }
    ));
    s
}

/// The `logout` receipt — the ended sessions + the honest server-side outcome.
pub(crate) fn session_logout_tty(data: &topos_types::results::LogoutData) -> String {
    let mut s = format!("Logged out of {}.", data.ended.join(", "));
    if !data.server_revoked {
        s.push_str(
            "\nNote: at least one session could not be revoked server-side (already ended, or \
             unreachable) — the local sign-out completed regardless; the web sessions page shows \
             what the server still holds.",
        );
    }
    s.push_str("\nSkills, drafts, and manifests stay; `topos login <address>` signs back in.");
    s
}

/// The `init` receipt — the manifest's path, whether it was created, and the honest travel note.
pub(crate) fn init_tty(data: &topos_types::results::InitData) -> String {
    let mut out = if data.created {
        format!(
            "Created {} — record skills with `topos add`; `topos update` reconciles agents in \
             this folder against it.",
            data.manifest
        )
    } else {
        format!("{} already exists — nothing changed.", data.manifest)
    };
    if let Some(note) = &data.note {
        out.push_str(&format!("\nNote: {note}."));
    }
    out
}

pub(crate) fn add_tty(data: &AddData) -> String {
    let mut out = String::new();
    // The MANIFEST edited comes FIRST (the trust rail's first half: which manifest line asked for
    // it), with the paste-ready inverse.
    if let (Some(manifest), Some(reference)) = (&data.manifest, &data.reference) {
        out.push_str(&format!(
            "{manifest}: added \"{reference}\" (undo: {})\n",
            if data.undo.is_empty() {
                format!("topos remove {reference}")
            } else {
                data.undo.join(" ")
            }
        ));
    }
    out.push_str(&format!(
        "Adopted '{}' ({}) @ {}",
        data.name,
        data.skill_id,
        short(&data.version_id)
    ));
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
    // Disclose the one write `add` makes outside ~/.topos/ — the auto-update trigger — honestly (it is
    // plumbing: it runs a no-op `update` until something is followed; never "it auto-updates"). The copy
    // branches on the report's `currency_kind` so a harness's honest update moment is never overstated
    // (a session-start hook fires at session boundaries; a scheduled job only while its scheduler runs).
    if let Some(report) = &data.currency {
        out.push_str(match (report.state, report.currency_kind) {
            (TriggerState::Active, CurrencyKind::SessionStart) => {
                "\nInstalled the session-start auto-update hook (runs `topos update` at session start)."
            }
            (TriggerState::Active, CurrencyKind::Scheduled) => {
                "\nRegistered the auto-update job (updates land within about a minute while the harness's scheduler runs)."
            }
            (TriggerState::Active, CurrencyKind::ExplicitPullOnly) => {
                "\nNo automatic auto-update trigger — run `topos update` to check for updates."
            }
            (TriggerState::AlreadyPresentUnmanaged, CurrencyKind::SessionStart) => {
                "\nLeft your existing session-start auto-update hook untouched."
            }
            (TriggerState::AlreadyPresentUnmanaged, _) => {
                "\nLeft your existing auto-update trigger untouched."
            }
            (TriggerState::Degraded, CurrencyKind::SessionStart) => {
                "\nCouldn't update settings.json for the auto-update hook — left it untouched."
            }
            (TriggerState::Degraded, _) => {
                "\nCouldn't update the harness config for the auto-update trigger — left it untouched; run `topos update` to check for updates."
            }
            (TriggerState::Inactive, _) => "",
        });
    }
    out.push_str(&breadth_trigger_lines(&data.triggers));
    out
}

/// The breadth arming sweep's receipt lines — one per OTHER detected agent, honest per row (an
/// `Active` row names its live moment; a registered-but-ungated row names the consent still owed;
/// a degraded row names the explicit-pull floor). Empty input renders nothing.
pub(crate) fn breadth_trigger_lines(
    triggers: &[topos_types::results::BreadthTriggerReport],
) -> String {
    if triggers.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nOther detected agents:");
    for t in triggers {
        let phrase = match (t.state, t.currency_kind) {
            (TriggerState::Active, CurrencyKind::SessionStart) => "armed (session start)",
            (TriggerState::Active, CurrencyKind::Scheduled) => "armed (scheduled)",
            (TriggerState::Active, _) => "armed",
            (TriggerState::Inactive, _) => "registered",
            (TriggerState::Degraded, _) => "couldn't arm — `topos update` still works",
            (TriggerState::AlreadyPresentUnmanaged, _) => "left your existing trigger untouched",
        };
        out.push_str(&format!("\n  {}: {}", t.agent, phrase));
        if let Some(note) = &t.note {
            out.push_str(&format!(" — {note}"));
        }
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
        .map(|s| {
            crate::actions::next_action(
                ActionCode::from("KEEP_AS_YOURS".to_owned()),
                vec![
                    "topos".to_owned(),
                    "add".to_owned(),
                    s.skill.clone(),
                    "--yes".to_owned(),
                ],
            )
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
            "Enrolled at {} — auto-update hook: {}\n",
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
    // An explicit `--limit`/`--offset` page on the TTY: one line per capped bucket.
    for t in &data.truncated {
        s.push_str(&format!(
            "… {}: {} of {} rows shown — a higher `--offset` pages on\n",
            t.bucket, t.shown, t.total
        ));
    }
    // Isolated per-workspace catalog-read failures — the same stable lines the `--json` envelope carries.
    for w in &out.warnings {
        s.push_str(&format!("warning: {w}\n"));
    }
    if let Some(footprint) = &data.footprint {
        // The count is the header; then each path, so a `--footprint` read reports WHAT topos owns (not
        // just how many) — the set `uninstall` deletes.
        s.push_str(&format!(
            "Footprint: {} paths under the topos home\n",
            footprint.len()
        ));
        for p in footprint {
            s.push_str(&format!("  {p}\n"));
        }
    }
    s.trim_end().to_owned()
}

/// One `--remote` catalog row: `<name>  <name>@<short>  <kind>  <state note>` (+ any open-proposal
/// count). The name falls back to the skill id when the plane discloses no display name; the kind is
/// the catalog's bundle kind, displayed verbatim (never branched on). HONEST annotations — no
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
        "    {}  {}@{}  {}  {}{}\n",
        name,
        name,
        short(&r.version_id),
        r.kind,
        note,
        proposals
    )
}

/// One untracked-discovery row: `<name>  [<harness-name> · <slug>]  <path>`, plus an adopt-only note for a
/// harness topos has no full adapter for — it can still be `add`ed (the bytes track + share), but live
/// auto-updates for that harness land later. The **slug** is shown because it is the `<skill>@<harness>` token
/// `add` takes to disambiguate a name found in more than one harness.
fn untracked_row(u: &UntrackedEntry) -> String {
    let support = if u.adapter_supported {
        ""
    } else {
        "  (adopt-only — live auto-updates land later)"
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
    if data.diff.is_empty() && !data.truncated {
        return "No changes — the draft matches current.".to_owned();
    }
    let mut s = data.diff.trim_end_matches('\n').to_owned();
    // An explicit `--max-bytes` cap on the TTY: say what fell off and how to get it (the same cap
    // the `--json` envelope discloses structurally).
    if data.truncated {
        let omitted = data.files.iter().filter(|f| f.patch_omitted).count();
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(&format!(
            "… diff truncated — {omitted} of {} file patch(es) omitted by the byte cap; re-run \
             with `--max-bytes 0` for the full diff",
            data.files.len()
        ));
    }
    s
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
    // An explicit `--limit`/`--offset` page on the TTY: name what lies past it.
    if data.truncated
        && let Some(total) = data.total
    {
        out.push_str(&format!(
            "… {} of {total} events shown — a higher `--offset` pages on\n",
            data.events.len()
        ));
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
    if let Some(n) = &o.note {
        s.push_str(&format!("\nnote: {n}"));
    }
    s
}

pub(crate) fn follow_tty(data: &FollowData, resumed: &[String]) -> String {
    // A pending enrollment: surface the approval URL with the workspace + server it points at (the
    // human checks the address before approving).
    if let Some(pending) = &data.pending {
        let workspace = data
            .workspace_display_name
            .clone()
            .unwrap_or_else(|| data.workspace_id.clone());
        let mut s = format!("Enrolling with {workspace}");
        if let Some(plane) = &data.plane_base_url {
            s.push_str(&format!("\nserver: {plane}"));
        }
        // The URL and the code print on SEPARATE lines — the code never rides a URL. The human
        // opens the page (an invitation enrollment's page weaves account → accept → approval)
        // and checks the code shown there against this one. This render surfaces only when the
        // invocation ENDED still pending (a headless run, or a `--wait` cap that passed): the
        // interactive path auto-polls to completion and never shows it, so the re-invoke line
        // here is honest resume guidance, never a required post-approval step.
        s.push_str(&format!(
            "\nOpen: {}\nCode: {} (the page shows the same code — confirm it matches before \
             approving)",
            pending.verification_uri, pending.user_code,
        ));
        if let Some(exp) = &pending.expires_at {
            s.push_str(&format!("\nThe code is valid until {exp}."));
        }
        s.push_str(
            "\nStill waiting for the approval — re-run `topos follow` to keep waiting (nothing \
             is lost; the same enrollment resumes).",
        );
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
    s.push_str(&breadth_trigger_lines(&data.triggers));
    s
}

/// The generic next-actions for a two-phase DESCRIBE: each argv is the ready-to-exec apply command
/// (`… --yes`). An empty argv list yields an empty next-actions list (a standing follow's
/// nothing-to-apply describe).
pub(crate) fn describe_next_actions(argvs: Vec<Vec<String>>) -> Vec<NextAction> {
    argvs
        .into_iter()
        .map(|argv| {
            crate::actions::next_action(ActionCode::from("APPLY_DESCRIBED".to_owned()), argv)
        })
        .collect()
}

/// The next-actions for an UNDO-led apply receipt: the literal inverse command, when one exists
/// (an immediate self-scoped apply always discloses its way back).
pub(crate) fn undo_next_actions(undo: &[String]) -> Vec<NextAction> {
    if undo.is_empty() {
        return Vec::new();
    }
    vec![crate::actions::next_action(
        ActionCode::from("UNDO".to_owned()),
        undo.to_vec(),
    )]
}

/// The TTY spelling of an undo-led receipt's tail — the paste-ready inverse command. Empty when
/// there is nothing to undo.
fn undo_line(undo: &[String]) -> String {
    if undo.is_empty() {
        return String::new();
    }
    format!("\nUndo: {}", argv_line(undo))
}

/// One argv as a paste-ready shell line (the TTY's spelling of a next action) — each token
/// [`shell_quote`]d so a value carrying whitespace or a shell metacharacter (e.g. a multi-word `-m
/// <message>`) copy-pastes back as ONE argument instead of mis-parsing. The `--json` envelope carries the
/// argv ARRAY untouched (already unambiguous); only this human line needs the quoting.
fn argv_line(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quote ONE argv token for a paste-ready POSIX shell line: a token that is safe bare (non-empty and all
/// `[A-Za-z0-9_@%+=:,./-]`) is returned as-is; anything else — whitespace, glob/redirection
/// metacharacters, quotes, or an empty string — is wrapped in single quotes with any embedded single quote
/// escaped as `'\''`.
fn shell_quote(arg: &str) -> String {
    let safe = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-')
    };
    if !arg.is_empty() && arg.chars().all(safe) {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// The follow DESCRIBE's TTY: the workspace story, the install list with digests + via, the
/// dirname outcomes (adoptions; auto-namespaced collisions), the standing disclosures, and the
/// `--yes` argv. Nothing has changed yet — except an enrollment, which is disclosed as a fact
/// (the credential/link persisted and the trigger armed), never papered over with "nothing has
/// changed".
pub(crate) fn follow_describe_tty(
    d: &crate::ops::FollowDescribe,
    next_argvs: &[Vec<String>],
) -> String {
    let mut s = format!(
        "{} ({}) — {}",
        d.workspace.display_name, d.workspace.name, d.workspace.address
    );
    if d.enrolled_now {
        s.push_str(&format!(
            "\nEnrolled this device as {} — role: {}",
            d.principal, d.role
        ));
        if let Some(by) = &d.invited_by {
            s.push_str(&format!(" — invited by {by}"));
        }
        s.push('.');
    } else {
        s.push_str(&format!("\nYour role: {}", d.role));
        if let Some(by) = &d.invited_by {
            s.push_str(&format!(" — invited by {by}"));
        }
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
    if let Some(note) = &d.standing_note {
        // The standing no-op: one honest line carrying the whole fact (the all-devices constant
        // is folded into the note), no install lines, no apply block.
        s.push_str(&format!(
            "\nFollowing: {} — {note}.",
            target_list.join(", ")
        ));
        return s;
    }
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
    if !d.adoptions.is_empty() {
        s.push_str("\nAdopts in place:");
        for a in &d.adoptions {
            s.push_str(&format!(
                "\n  {} — adopts your existing identical copy at {}",
                a.name, a.path
            ));
        }
    }
    if !d.collisions.is_empty() {
        s.push_str("\nName collisions:");
        for c in &d.collisions {
            s.push_str(&format!(
                "\n  {} — a different skill already lives at {}; installs as {}",
                c.name, c.existing, c.installs_as
            ));
        }
    }
    s.push_str(&format!("\n{}", d.all_devices_note));
    if !next_argvs.is_empty() {
        if d.enrolled_now {
            // The enrollment DID persist (credential/link + the armed trigger) — never claim
            // nothing has changed.
            s.push_str("\nApply the follow with:");
        } else {
            s.push_str("\nNothing has changed yet — apply with:");
        }
        for argv in next_argvs {
            s.push_str(&format!("\n  {}", argv_line(argv)));
        }
    }
    s
}

/// The follow APPLY's TTY: what was subscribed and what landed.
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
    for w in &a.warnings {
        s.push_str(&format!("\nwarning: {w}"));
    }
    s.push_str(&undo_line(&a.undo));
    s
}

/// The re-attach APPLY's TTY — undo-led: the stance is cleared, the bytes are back, the way back
/// is the literal inverse. Worded by the cause (`excluded-here` vs `unfollowed`).
pub(crate) fn reattach_applied_tty(r: &crate::ops::Reattach) -> String {
    let mut s = if r.cause == "unfollowed" {
        format!(
            "Following {} again in {} ({}) — the unfollow is cleared; delivery resumes on every \
             device of yours.",
            r.name, r.workspace_name, r.workspace_id
        )
    } else {
        format!(
            "Re-attached {} on this device in {} ({}) — the exclusion is lifted; the person keeps \
             following it.",
            r.name, r.workspace_name, r.workspace_id
        )
    };
    if r.installed {
        let digest = r.bundle_digest.as_deref().map(short).unwrap_or("?");
        s.push_str(&format!("\nReinstalled: {}  @{digest}", r.name));
    } else {
        s.push_str("\nThe current bytes will land on the next `topos update`.");
    }
    for w in &r.warnings {
        s.push_str(&format!("\nwarning: {w}"));
    }
    s.push_str(&undo_line(&r.undo));
    s
}

/// The browser-free link DESCRIBE's TTY: lead with "link this device to <workspace>", the standing
/// disclosures, whether the link is born active or pending, and the paste-ready `--yes`.
pub(crate) fn link_describe_tty(d: &crate::ops::LinkDescribe, yes_argv: &[String]) -> String {
    let mut s = format!(
        "Link this device to {} ({}) — {}",
        d.workspace.display_name, d.workspace.name, d.workspace.address
    );
    s.push_str(
        "\nThis device is already enrolled with this server; joining the workspace is one \
                link — no browser step.",
    );
    s.push_str(&format!("\nYour role: {}", d.role));
    match d.link_status.as_str() {
        "active" => s.push_str("\nAlready linked (active) — `--yes` re-affirms it."),
        "pending" => s.push_str(
            "\nA link request is already waiting for an owner's approval — `--yes` re-affirms it.",
        ),
        _ => match d.born.as_str() {
            "pending" => s.push_str(
                "\nThe link is born PENDING — this workspace approves new devices: an owner \
                 confirms it in the web app, then delivery starts automatically.",
            ),
            _ => s.push_str("\nThe link is born active — delivery starts on `--yes`."),
        },
    }
    s.push_str(&format!("\n{}", d.all_devices_note));
    s.push_str(&format!(
        "\nNothing has changed yet — apply with:\n  {}",
        argv_line(yes_argv)
    ));
    s
}

/// The PENDING-link receipt's TTY — the link awaits an owner's approval; nothing subscribed, no
/// bytes; delivery starts automatically after approval.
pub(crate) fn link_pending_tty(p: &topos_types::results::LinkPendingData) -> String {
    let label = p
        .workspace_display_name
        .as_deref()
        .unwrap_or(&p.workspace_name);
    let mut s = String::new();
    if p.enrolled_now {
        s.push_str("Enrolled this device (identity only — nothing is installed yet).\n");
    }
    s.push_str(&format!(
        "Linked to {label} ({}) — awaiting owner approval.\nNo skills land until an owner \
         approves this device in the web app; delivery then starts automatically.\n`topos status` \
         shows the waiting link.",
        p.workspace_name
    ));
    s
}

/// The pending-link receipt's next actions: check the wait with `topos status` (delivery resumes
/// by itself — there is nothing to re-run).
pub(crate) fn link_pending_next_actions() -> Vec<NextAction> {
    vec![crate::actions::next_action(
        ActionCode::from("CHECK_STATUS".to_owned()),
        vec!["topos".into(), "status".into(), "--json".into()],
    )]
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

/// The `--agent` scope verbs' TTY (describe when `yes_argv` is `Some`, apply otherwise) — the
/// placement plan per skill: what lands, what is cleaned (snapshot-first), what stays, and the
/// standing "subscription untouched" constant.
pub(crate) fn agent_scope_tty(
    d: &crate::ops::AgentScopeData,
    yes_argv: Option<&[String]>,
) -> String {
    let mut s = String::new();
    let heading = match (d.action.as_str(), d.agents.is_empty()) {
        ("exclude", _) => format!("Excluding agents on this device: {}", d.agents.join(", ")),
        ("scope", true) => "Clearing the agent scope (back to every detected agent)".to_owned(),
        ("restore", _) => "Placing the built-in `topos` skill on this machine".to_owned(),
        _ => format!("Scoping placement to agents: {}", d.agents.join(", ")),
    };
    s.push_str(&heading);
    for item in &d.items {
        s.push_str(&format!("\n{}:", item.skill));
        for dir in &item.added {
            s.push_str(&format!("\n  + lands in {dir}"));
        }
        for dir in &item.cleaned {
            s.push_str(&format!(
                "\n  - removed from {dir} (any edit is snapshotted first)"
            ));
        }
        for dir in &item.kept {
            s.push_str(&format!("\n  = stays in {dir}"));
        }
        if item.added.is_empty() && item.cleaned.is_empty() && item.kept.is_empty() {
            s.push_str("\n  no placement changes on this machine");
        }
        for note in &item.notes {
            s.push_str(&format!("\n  note: {note}"));
        }
    }
    s.push_str(&format!("\n{}", d.subscription_note));
    if let Some(argv) = yes_argv {
        s.push_str(&format!(
            "\nNothing has changed yet — apply with:\n  {}",
            argv_line(argv)
        ));
    } else {
        s.push_str(&undo_line(&d.undo));
    }
    s
}

/// The unfollow APPLY's TTY — undo-led: what stopped, what never changes, the way back.
pub(crate) fn unfollow_applied_tty(a: &crate::ops::UnfollowApplied) -> String {
    let mut s = String::new();
    for item in &a.items {
        s.push_str(&format!(
            "Stopped following {} {} — delivery ends on every device of yours; the local copy \
             stays as a frozen copy.\n",
            item.kind, item.name
        ));
    }
    s.push_str("`topos follow` re-attaches.");
    s.push_str(&undo_line(&a.undo));
    s
}

/// The pending login's TTY (the device-flow wait — the same shape as the follow wait). Surfaces
/// only when the invocation ended still pending (the interactive path auto-polls to completion),
/// so the re-invoke line is resume guidance, never a required post-approval step.
pub(crate) fn login_pending_tty(p: &crate::ops::AuthLoginPending) -> String {
    let mut s = format!(
        "Signing in to {}\nOpen: {}\nCode: {} (the page shows the same code — confirm it \
         matches before approving)",
        p.server, p.verification_uri, p.user_code,
    );
    if let Some(exp) = &p.expires_at {
        s.push_str(&format!("\nThe code is valid until {exp}."));
    }
    s.push_str(
        "\nStill waiting for the approval — re-run `topos auth login` to keep waiting (nothing \
         is lost; the same sign-in resumes).",
    );
    s
}

/// The completed login's TTY: the ONE re-minted device credential + the workspace it ran through.
pub(crate) fn login_done_tty(d: &crate::ops::AuthLoginData) -> String {
    format!(
        "Signed in to {} — this device's credential was re-minted (device {}).\nApproved through \
         {} ({}); the one credential covers every workspace your seats reach.",
        d.server, d.device_id, d.workspace_display_name, d.workspace_name,
    )
}

/// The pending login's next action — re-invoke `auth login` (re-invoking IS the resume).
pub(crate) fn login_pending_next_actions() -> Vec<NextAction> {
    vec![crate::actions::next_action(
        ActionCode::from("ENROLL_RESUME".to_owned()),
        vec![
            "topos".into(),
            "auth".into(),
            "login".into(),
            "--json".into(),
        ],
    )]
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
        s.push_str("\nNo stored credential — already signed out.");
    } else {
        s.push_str(&format!(
            "\nWould sign this device out of the server everywhere — one revoke covers every \
             linked workspace ({}) — and delete the stored credential.",
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

/// The `uninstall` DESCRIBE's TTY — everything `--yes` would remove (nothing has changed yet).
pub(crate) fn uninstall_describe_tty(
    d: &crate::ops::UninstallDescribe,
    yes_argv: &[String],
) -> String {
    let mut s = String::from("Uninstalling topos would:");
    if d.hook_paths.is_empty() {
        s.push_str("\n  · scrub the session-start auto-update hook: none is armed");
    } else {
        s.push_str(&format!(
            "\n  · scrub the session-start auto-update hook from: {}",
            d.hook_paths.join(", ")
        ));
    }
    if d.sidecar_present {
        s.push_str(&format!(
            "\n  · delete the sidecar tree {} (the signed-in credential lives there and goes with it)",
            d.sidecar_path
        ));
    } else {
        s.push_str(&format!(
            "\n  · nothing to delete at {} (no sidecar tree here)",
            d.sidecar_path
        ));
    }
    if !d.builtin_dirs.is_empty() {
        s.push_str(&format!(
            "\n  · remove the built-in `topos` skill's copies (topos-authored): {}",
            d.builtin_dirs.join(", ")
        ));
    }
    s.push_str("\n  · leave every SKILL FILE in your agent dirs untouched — uninstall deletes no skill bytes");
    if let Some(bin) = &d.binary_path {
        s.push_str(&format!(
            "\nThe `topos` binary at {bin} is NOT removed — delete it with the installer you used (or `rm {bin}`)."
        ));
    }
    s.push_str(&format!(
        "\nNothing has changed yet — apply with:\n  {}",
        argv_line(yes_argv)
    ));
    s
}

/// The applied `uninstall`'s TTY — what was removed, the hook scrub surfaced honestly.
pub(crate) fn uninstall_applied_tty(d: &crate::ops::UninstallApplied) -> String {
    let mut s = String::from("Uninstalled topos.");
    if !d.builtin_dirs.is_empty() {
        s.push_str(&format!(
            "\n  · removed the built-in `topos` skill's copies: {}",
            d.builtin_dirs.join(", ")
        ));
    }
    let hook_line = match (d.hook.state, d.hook.touched_path.as_deref()) {
        (TriggerState::Degraded, _) => {
            "\n  · couldn't edit the harness config — remove the topos auto-update hook manually"
                .to_owned()
        }
        (TriggerState::AlreadyPresentUnmanaged, _) => {
            "\n  · left your hand-rolled auto-update hook untouched".to_owned()
        }
        (_, Some(path)) => format!("\n  · scrubbed the auto-update hook from {path}"),
        (_, None) => "\n  · no auto-update hook was installed — nothing to scrub".to_owned(),
    };
    s.push_str(&hook_line);
    // The breadth scrub's rows — only agents with something to say (a real scrub, or a survival
    // disclosure); clean no-ops never reach this receipt.
    for t in &d.triggers {
        let phrase = match t.state {
            TriggerState::Degraded => {
                "couldn't remove the trigger — it may still be registered there".to_owned()
            }
            TriggerState::AlreadyPresentUnmanaged => "left your own trigger untouched".to_owned(),
            _ => match &t.touched_path {
                Some(path) => format!("scrubbed the trigger from {path}"),
                None => "trigger removed".to_owned(),
            },
        };
        s.push_str(&format!("\n  · {}: {}", t.agent, phrase));
    }
    if d.sidecar_removed {
        s.push_str("\n  · deleted the ~/.topos sidecar tree (credential included)");
    } else {
        s.push_str("\n  · no sidecar tree to delete");
    }
    if let Some(bin) = &d.binary_path {
        s.push_str(&format!(
            "\nThe `topos` binary at {bin} was left in place — remove it with your installer (or `rm {bin}`)."
        ));
    }
    s
}

/// The applied logout's TTY.
pub(crate) fn logout_applied_tty(d: &crate::ops::AuthLogoutData) -> String {
    let mut s = if d.credentials_deleted {
        "Signed out — the stored credential is deleted.".to_owned()
    } else {
        "Already signed out — no credential was stored.".to_owned()
    };
    if d.revoked {
        s.push_str("\nRevoked this device on the server — every linked workspace at once.");
    } else if d.credentials_deleted {
        s.push_str(
            "\nCould not reach the server to revoke the device (the credential is deleted \
             locally either way; an owner can unlink the device on the web).",
        );
    }
    s.push_str(&format!("\n{}", d.keeps_note));
    s
}

/// `auth status`'s TTY — whoami, per-workspace health, hook health, reporting posture.
/// The `status` snapshot's TTY — the one orientation read: version, enrollment/sign-in, follows +
/// pending offers, and the per-agent trigger rows. Entirely from the offline snapshot.
pub(crate) fn status_tty(d: &topos_types::results::StatusData) -> String {
    let mut s = format!("topos {}", d.version);
    match &d.server {
        Some(server) => {
            s.push_str(&format!(
                "\nserver: {server} — {}",
                if d.signed_in {
                    "signed in"
                } else {
                    "signed out (`topos auth login` signs back in)"
                }
            ));
            for ws in &d.workspaces {
                s.push_str(&format!("\n  {} ({})", ws.display_name, ws.name));
                match ws.link_status.as_deref() {
                    Some("pending") => s.push_str(" — awaiting owner approval"),
                    Some("ended") => s.push_str(&format!(
                        " — no access; relink with `topos follow {}`",
                        ws.name
                    )),
                    _ => {}
                }
            }
        }
        None => s.push_str(
            "\nnot enrolled — join your team with `topos follow <workspace-address>` (ask a \
             teammate for the address), or create a workspace at https://topos.sh",
        ),
    }
    s.push_str(&format!(
        "\nfollowing: {} {}",
        d.followed_skills,
        if d.followed_skills == 1 {
            "skill"
        } else {
            "skills"
        }
    ));
    if let Some(p) = d.pending_offers
        && p > 0
    {
        s.push_str(&format!(
            " — {p} first-receive {} awaiting consent (`topos update` discloses {})",
            if p == 1 { "offer" } else { "offers" },
            if p == 1 { "it" } else { "them" },
        ));
    }
    if !d.triggers.is_empty() {
        s.push_str("\nauto-update triggers:");
        for t in &d.triggers {
            let state = match t.armed {
                Some(true) => "armed",
                Some(false) => "not armed",
                None => "unknown",
            };
            s.push_str(&format!("\n  {}: {state}", t.agent));
            if let Some(n) = &t.note {
                s.push_str(&format!(" — {n}"));
            }
        }
    }
    s
}

/// The bare `topos` welcome on a fresh (unenrolled) machine — what topos is, in two lines, plus
/// the two ways in. The enrolled bare invocation renders [`status_tty`] instead.
pub(crate) fn welcome_tty(d: &topos_types::results::StatusData) -> String {
    format!(
        "topos {} — shared skills for AI agents: your team's behaviors, delivered to every \
         agent and kept current.\nJoin your team:      topos follow <workspace-address>   (ask a \
         teammate for the address)\nCreate a workspace:  https://topos.sh",
        d.version
    )
}

/// The signed-out `auth status` next actions — the machine half of the prose fix below: a
/// never-enrolled install gets the join TEMPLATE (its `needs` names the workspace address); an
/// enrolled-but-signed-out one gets the concrete `auth login`.
pub(crate) fn auth_status_next_actions(d: &crate::ops::AuthStatusData) -> Vec<NextAction> {
    if d.signed_in {
        return Vec::new();
    }
    if d.server.is_none() && d.workspaces.is_empty() {
        vec![crate::actions::next_action(
            ActionCode::from("FOLLOW_WORKSPACE".to_owned()),
            vec![
                "topos".into(),
                "follow".into(),
                "<workspace-address>".into(),
                "--json".into(),
            ],
        )]
    } else {
        vec![crate::actions::next_action(
            ActionCode::from("SIGN_IN".to_owned()),
            vec![
                "topos".into(),
                "auth".into(),
                "login".into(),
                "--json".into(),
            ],
        )]
    }
}

pub(crate) fn auth_status_tty(d: &crate::ops::AuthStatusData) -> String {
    let mut s = match (&d.principal, d.signed_in) {
        (Some(p), true) => format!("Signed in as {p}"),
        (Some(p), false) => format!("Signed out (last account: {p})"),
        (None, _) => "Not signed in.".to_owned(),
    };
    if let Some(server) = &d.server {
        s.push_str(&format!("\nserver: {server}"));
    }
    // Signed out: state the fix in prose (mirrored structurally by the envelope's next actions).
    if !d.signed_in {
        if d.server.is_none() && d.workspaces.is_empty() {
            s.push_str(
                "\nnot enrolled — join with `topos follow <workspace-address>` (ask a teammate \
                 for the address)",
            );
        } else {
            s.push_str("\nsign back in with `topos auth login`");
        }
    }
    for ws in &d.workspaces {
        let label = ws.display_name.as_deref().unwrap_or(&ws.workspace_id);
        s.push_str(&format!("\n  {label}: {}", ws.health));
        if let Some(role) = &ws.role {
            s.push_str(&format!(" ({role})"));
        }
    }
    s.push_str(&format!(
        "\nauto-update hook: {}",
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
    // A removal-specific disclosure overrides the kind's stock line (the built-in skill's durable
    // opt-out and its way back).
    if let Some(note) = &item.note {
        let verb = if applied { "Removed" } else { "Would remove" };
        let dirs = if item.agent_dirs.is_empty() {
            String::new()
        } else {
            format!(" from {}", item.agent_dirs.join(", "))
        };
        return format!("{verb} '{}'{dirs} — {note}.", item.name);
    }
    match item.kind {
        RemoveKind::ManifestRemoved => {
            let manifest = item.manifest.as_deref().unwrap_or("topos.toml");
            format!(
                "{manifest}: removed '{}' — delivery to this scope ends at the next `topos \
                 update`; bytes already placed stay until then (undo: topos add {}).",
                item.name, item.name
            )
        }
        RemoveKind::ManifestExcluded => {
            let manifest = item.manifest.as_deref().unwrap_or("topos.toml");
            format!(
                "{manifest}: excluded '{}' — a broader manifest still provides it, so an exclude \
                 line (the one negative state) now shadows it here (undo: topos add {}).",
                item.name, item.name
            )
        }
        RemoveKind::FollowedExclusion => {
            let verb = if applied { "Removed" } else { "Would remove" };
            format!(
                "{verb} '{}' from THIS device only — your other devices and the team are \
                 unaffected. The copy leaves this device's agent dirs; topos keeps the canonical bytes \
                 (so re-attaching needs no re-download), and nothing returns at the next sync. `topos \
                 follow {}` re-attaches it here (stopping it everywhere is `topos unfollow`).",
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

/// The `remove` APPLY's TTY — undo-led on the reversible shape (the followed exclusion).
pub(crate) fn remove_applied_tty(data: &RemoveData) -> String {
    let mut s = String::new();
    for item in &data.items {
        s.push_str(&remove_item_line(item, true));
        s.push('\n');
    }
    let mut s = s.trim_end().to_owned();
    s.push_str(&undo_line(&data.undo));
    s
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

/// The bare `invite` read's TTY — the workspace address and the explicit no-op note.
pub(crate) fn invite_read_tty(data: &topos_types::results::InviteReadData) -> String {
    format!(
        "Workspace address: {}\nInviting is owner-only.\nNothing was sent or changed — pass emails \
         to invite (`topos invite <email>...`).",
        data.address
    )
}

/// The `invite <email>...` DESCRIBE's TTY — who gets invited, the hint, the mailed-link note.
pub(crate) fn invite_describe_tty(
    data: &topos_types::results::InviteDescribeData,
    yes_argv: &[String],
) -> String {
    let mut s = format!("Would invite into {}:", data.address);
    for e in &data.seat {
        s.push_str(&format!("\n  {e}"));
    }
    if let Some(skill) = &data.skill {
        s.push_str(&format!(
            "\nLeads with the {skill} skill — accepting follows it for them."
        ));
    }
    if let Some(channel) = &data.channel {
        s.push_str(&format!(
            "\nLeads with #{channel} — accepting joins them to it."
        ));
    }
    s.push_str(
        "\nEach address gets a mailed single-use invite link (browser, agent paste-block, or \
         `topos follow <invite-url>`).",
    );
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
/// states only the adoption; the line that FOLLOWS conveys what the publish then did.
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
        if let Some(note) = &data.placement_note {
            s.push_str(&format!(" — {note}"));
        }
    }
    if let Some(reach) = data.reach {
        s.push_str(&format!("\n  reaches {reach} people"));
    }
    // The behind-copy conflict prediction: this publish would be refused (rebase first), and the
    // in-memory dry run says how that rebase's merge would go.
    if let Some(preview) = &data.merge_preview {
        s.push_str(&format!(
            "\n  note: your copy is behind the team's current — this publish will be refused \
             (update to rebase); {}",
            merge_preview_line(preview)
        ));
    }
    if let Some(note) = &data.origin_note {
        s.push_str(&format!("\n  note: {note}"));
    }
    if let Some(line) = &data.share_line {
        s.push_str(&format!("\n  share: {line}"));
    }
    if let Some(line) = &data.invite_line {
        s.push_str(&format!("\n  bring a teammate: {line}"));
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
    // Lead with the NAME — the handle the person publishes by; the opaque skill_id stays a
    // `--json` key, never the human line.
    out.push_str(&format!(
        "Published {}@{} (digest {}) — current is now generation {}.",
        data.name,
        short(&data.version_id),
        short(&data.bundle_digest),
        data.current_generation,
    ));
    // A withheld placement is disclosed next to the success it qualifies: the publish landed,
    // the curated channel's reference did not — placement takes reviewer or owner.
    if let Some(ch) = &data.placement_withheld {
        out.push_str(&format!(
            "\n#{ch} is curated — the reference was NOT placed (placement takes reviewer or \
             owner). The skill is in the catalog; a curator places it: `topos channel add {ch} {}`.",
            data.name,
        ));
    }
    // The teammate handoff, after the confirmation: the one line to hand someone not yet in the
    // workspace (the skill page URL answers only for members, so it never recruits anyone).
    if let Some(line) = &data.invite_line {
        out.push_str(&format!("\nbring a teammate: {line}"));
    }
    out
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
        "Reverted {} to {} as forward commit {} — current is now generation {}. Nothing was \
         deleted; move current forward again to redo.",
        data.name,
        short(&data.reverted_to),
        short(&data.new_version_id),
        data.current_generation,
    )
}

/// The bare `revert` DESCRIBE's TTY — what the forward move would do (nothing has changed yet).
pub(crate) fn revert_describe_tty(
    data: &topos_types::results::RevertDescribeData,
    yes_argv: &[String],
) -> String {
    format!(
        "Revert {} — move current @{} forward to restore @{} (from generation {}).\n  a forward \
         move restoring older bytes; nothing deleted\nNothing has changed yet — apply with:\n  {}",
        data.skill,
        short(&data.current_version_id),
        short(&data.reverted_to),
        data.current_generation,
        argv_line(yes_argv),
    )
}

/// The byte-level no-op's TTY — good's bytes already ARE current, so reverting changes nothing.
pub(crate) fn revert_noop_tty(data: &topos_types::results::RevertDescribeData) -> String {
    format!(
        "'{}' is already at these bytes — @{} matches current (@{}). Reverting would change nothing.",
        data.skill,
        short(&data.reverted_to),
        short(&data.current_version_id),
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
    let by = if data.yours {
        format!("{} (your proposal)", data.proposer)
    } else {
        data.proposer.clone()
    };
    let mut s = format!(
        "{}\n  proposal {}\n  by {}  on base {}{}",
        data.message,
        data.proposal,
        by,
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
    // A four-eyes author only ever withdraws their own version; a reviewer decides.
    s.push_str(if data.yours {
        "\nWithdraw with:"
    } else {
        "\nDecide with:"
    });
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
                .map(|g| format!("generation {g}"))
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

    // The delivered notices (verdicts first) — an interactive `update` MARKED these read server-side,
    // so it MUST show them here or the verdict + its reason are lost forever (the quiet hook fetches
    // without acking, so it never reaches this renderer).
    let (verdicts, others): (Vec<_>, Vec<_>) =
        data.notices.iter().partition(|n| n.kind == "verdict");
    for n in verdicts.iter().chain(others.iter()) {
        out.push_str(&format!("{}\n", notice_line(n)));
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

/// A one-line human rendering of a delivered notice — the verdict (with its reason), a proposal
/// closure, or any other kind. Falls back to the server's rendered `message` for a kind this client
/// does not specially format.
fn notice_line(n: &topos_types::requests::WireNotice) -> String {
    let skill = n.skill_name.as_deref().unwrap_or("a skill");
    match n.kind.as_str() {
        "verdict" => {
            let verb = match n.outcome.as_deref() {
                Some("approve") => "was approved",
                Some("reject") => "was rejected",
                _ => "was reviewed",
            };
            let who = n
                .actor
                .as_deref()
                .map_or_else(String::new, |a| format!(" by {a}"));
            let why = n
                .reason
                .as_deref()
                .filter(|r| !r.is_empty())
                .map_or_else(String::new, |r| format!(" — {r}"));
            format!("verdict: your proposal to {skill} {verb}{who}{why}")
        }
        "proposal_closed" => {
            let why = n
                .reason
                .as_deref()
                .filter(|r| !r.is_empty())
                .map_or_else(String::new, |r| format!(" ({r})"));
            format!("proposal closed: {skill}{why}")
        }
        _ => n
            .message
            .clone()
            .unwrap_or_else(|| format!("{}: {skill}", n.kind)),
    }
}

/// One non-up-to-date skill's line (after the padded name) + any indented detail lines.
fn pull_row(s: &PullSkill) -> (String, Vec<String>) {
    use topos_types::results::PullAction;
    let name = &s.skill;
    match s.action {
        // Handled by the caller's compact summary.
        PullAction::UpToDate => (String::from("up to date"), Vec::new()),
        PullAction::FastForwarded => (
            format!("fast-forwarded — now at generation {}", s.applied),
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
                // The in-memory merge PREVIEW (already-local bytes only): what the merge WOULD do,
                // so the person picks merge-vs-escape informed. Absent = unknown, nothing printed.
                s.merge_preview
                    .as_ref()
                    .map(merge_preview_line)
                    .into_iter()
                    .collect(),
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

/// One human line for a merge PREVIEW (the in-memory dry run — a prediction, never a promise).
fn merge_preview_line(p: &topos_types::results::MergePreview) -> String {
    use topos_types::results::MergePreviewVerdict;
    match p.verdict {
        MergePreviewVerdict::Clean => {
            "merge preview: clean — the three-way merge would apply without conflicts".to_owned()
        }
        MergePreviewVerdict::Conflicted if p.conflicts.is_empty() => {
            "merge preview: conflicted — the merge would need manual resolution".to_owned()
        }
        MergePreviewVerdict::Conflicted => {
            format!("merge preview: conflicts in {}", p.conflicts.join(", "))
        }
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
    use topos_types::persisted::ConflictPathKind;
    use topos_types::results::{
        Conflict, ConflictPathReport, ListData, LogData, MergeReport, Offer, PublishData,
        PullAction, PullData, PullSkill, SkillEntry,
    };

    use crate::ops::{FollowNote, ListEnrollment, ListOutcome};

    use super::{
        auth_status_next_actions, auth_status_tty, follow_tty, list_tty, log_tty, publish_tty,
        pull_tty, safe_message, status_tty, welcome_tty,
    };

    fn row(name: &str, action: PullAction) -> PullSkill {
        PullSkill {
            skill: name.to_owned(),
            workspace_id: None,
            observed: 2,
            applied: 2,
            action,
            offer: None,
            conflict: None,
            merge: None,
            merge_preview: None,
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
    fn publish_tty_leads_with_the_skill_name_never_the_opaque_id() {
        let line = publish_tty(&PublishData {
            skill_id: "topos_a1b2c3".to_owned(),
            name: "smoke-notes".to_owned(),
            version_id: "a".repeat(64),
            bundle_digest: "c".repeat(64),
            current_generation: 3,
            added: None,
            placement_withheld: None,
            invite_line: None,
        });
        assert!(line.starts_with("Published smoke-notes@"), "{line}");
        assert!(
            !line.contains("topos_a1b2c3"),
            "the internal bundle id must never surface on the TTY line: {line}"
        );
    }

    #[test]
    fn publish_tty_discloses_a_withheld_curated_placement_next_to_the_success() {
        let line = publish_tty(&PublishData {
            skill_id: "topos_a1b2c3".to_owned(),
            name: "smoke-notes".to_owned(),
            version_id: "a".repeat(64),
            bundle_digest: "c".repeat(64),
            current_generation: 1,
            added: None,
            placement_withheld: Some("everyone".to_owned()),
            invite_line: None,
        });
        assert!(line.starts_with("Published smoke-notes@"), "{line}");
        assert!(
            line.contains("#everyone is curated — the reference was NOT placed"),
            "the withheld placement is disclosed: {line}"
        );
        assert!(
            line.contains("`topos channel add everyone smoke-notes`"),
            "the curator's way in is named: {line}"
        );
    }

    #[test]
    fn publish_tty_carries_the_teammate_handoff_after_the_confirmation() {
        // The apply receipt hands the author the ONE line that brings a teammate in — after the
        // published confirmation, never before it (the success stays the lead).
        let invite = "Ask your agent: \"Set up Topos for us: fetch https://topos.sh/agent and \
                      follow it. Our workspace: https://topos.sh/acme\"";
        let line = publish_tty(&PublishData {
            skill_id: "topos_a1b2c3".to_owned(),
            name: "smoke-notes".to_owned(),
            version_id: "a".repeat(64),
            bundle_digest: "c".repeat(64),
            current_generation: 3,
            added: None,
            placement_withheld: None,
            invite_line: Some(invite.to_owned()),
        });
        assert!(line.starts_with("Published smoke-notes@"), "{line}");
        let handoff = format!("\nbring a teammate: {invite}");
        assert!(
            line.ends_with(&handoff),
            "the handoff line follows the confirmation: {line}"
        );
    }

    #[test]
    fn publish_describe_tty_labels_the_teammate_handoff_next_to_the_share_line() {
        use topos_types::results::{PublishDescribeData, PublishGate};
        let invite = "Ask your agent: \"Set up Topos for us: fetch https://topos.sh/agent and \
                      follow it. Our workspace: https://topos.sh/acme\"";
        let s = super::publish_describe_tty(
            &PublishDescribeData {
                skill: "deploy".to_owned(),
                skill_id: "s_deploy".to_owned(),
                workspace_id: "w_acme".to_owned(),
                workspace_display_name: Some("Acme".to_owned()),
                bundle_digest: "b".repeat(64),
                placements: vec!["everyone".to_owned()],
                gate: PublishGate::Lands,
                is_revert: false,
                reach: Some(12),
                share_line: Some("https://topos.sh/acme/skills/deploy".to_owned()),
                invite_line: Some(invite.to_owned()),
                undo: None,
                origin_note: None,
                placement_note: None,
                merge_preview: None,
            },
            &["topos".to_owned(), "publish".to_owned(), "--yes".to_owned()],
        );
        let share_at = s.find("share: ").expect("the share line renders");
        let invite_at = s
            .find("bring a teammate: ")
            .expect("the handoff line renders");
        assert!(
            invite_at > share_at,
            "the handoff sits next to (after) the share line: {s}"
        );
        assert!(s.contains(invite), "the exact join line renders: {s}");
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
        assert!(
            out.contains("fast-forwarded — now at generation 2"),
            "{out}"
        );
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
    fn pull_tty_renders_delivered_notices_so_an_interactive_update_never_acks_them_unseen() {
        // An interactive `update` marks the delivered notices read server-side; the TTY MUST show them
        // or the verdict + reason are lost forever. Verdicts render first, with the reviewer's reason.
        let notice = |kind: &str, outcome: Option<&str>, reason: Option<&str>| {
            topos_types::requests::WireNotice {
                id: "n1".into(),
                kind: kind.into(),
                skill_id: Some("s_deploy".into()),
                skill_name: Some("deploy".into()),
                version_id: None,
                actor: Some("rob@acme.test".into()),
                outcome: outcome.map(str::to_owned),
                reason: reason.map(str::to_owned),
                message: None,
                created_at: "2026-07-12T00:00:00Z".into(),
            }
        };
        let data = PullData {
            skills: vec![row("deploy", PullAction::UpToDate)],
            proposals_awaiting: 0,
            notices: vec![
                notice("proposal_closed", Some("closed"), Some("superseded")),
                notice("verdict", Some("reject"), Some("needs a test")),
            ],
            sync: Vec::new(),
        };
        let out = pull_tty(&data, &[]);
        // The verdict shows its outcome + reason, and sorts before the closure.
        let v = out.find("was rejected").expect("the verdict is shown");
        assert!(
            out.contains("needs a test"),
            "the reason rides along: {out}"
        );
        let c = out.find("proposal closed").expect("the closure is shown");
        assert!(v < c, "verdicts render before other notices: {out}");
        assert!(out.contains("deploy"), "the skill name is named: {out}");
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
                truncated: Vec::new(),
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
            text.starts_with("Enrolled at https://topos.example — auto-update hook: active"),
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
            kind: "skill".to_owned(),
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
        assert!(
            text.contains("deploy@abababababab  skill  (available)"),
            "{text}"
        );
        assert!(!text.contains("topos follow deploy"), "{text}");
        assert!(
            text.contains("runbook@abababababab  skill  (following)"),
            "{text}"
        );
        // Behind points at `topos update` (the real advance path).
        assert!(
            text.contains(
                "audit@abababababab  skill  (update available — run `topos update audit`)"
            ),
            "{text}"
        );
        // The per-workspace degradation warning surfaces.
        assert!(
            text.contains("warning: could not read the catalog for workspace Beta"),
            "{text}"
        );
    }

    #[test]
    fn list_footprint_prints_each_path_not_just_a_count() {
        // `--footprint` reports WHAT topos owns (the set `uninstall` deletes), not merely how many.
        let out = ListOutcome {
            data: ListData {
                footprint: Some(vec![
                    "/home/x/.topos/identity".to_owned(),
                    "/home/x/.topos/skills/topos_s00".to_owned(),
                ]),
                ..ListData::default()
            },
            enrollment: None,
            warnings: Vec::new(),
        };
        let text = list_tty(&out);
        assert!(
            text.contains("Footprint: 2 paths under the topos home"),
            "{text}"
        );
        assert!(
            text.contains("/home/x/.topos/identity"),
            "each path is listed: {text}"
        );
        assert!(
            text.contains("/home/x/.topos/skills/topos_s00"),
            "each path is listed: {text}"
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
            truncated: false,
            total: None,
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
    fn argv_line_shell_quotes_only_tokens_that_would_mis_parse() {
        use super::{argv_line, shell_quote};
        // Safe tokens ride bare (the common apply line stays clean).
        for safe in [
            "topos",
            "publish",
            "release-notes",
            "--yes",
            "-m",
            "acme/skills/deploy",
            "release-notes@abc123",
        ] {
            assert_eq!(shell_quote(safe), safe, "{safe:?} should not be quoted");
        }
        // A multi-word -m message (the bug) pastes back as ONE argument.
        assert_eq!(
            shell_quote("First cut of the release-notes skill"),
            "'First cut of the release-notes skill'"
        );
        // Shell metacharacters + an embedded single quote are quoted/escaped; empty is quoted.
        assert_eq!(shell_quote("a > b"), "'a > b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        // The whole line quotes each token independently.
        let argv = [
            "topos".to_owned(),
            "publish".to_owned(),
            "release-notes".to_owned(),
            "-m".to_owned(),
            "First cut of the release-notes skill".to_owned(),
            "--yes".to_owned(),
        ];
        assert_eq!(
            argv_line(&argv),
            "topos publish release-notes -m 'First cut of the release-notes skill' --yes"
        );
    }

    #[test]
    fn follow_tty_pending_discloses_the_url_and_the_cross_check_code() {
        use topos_types::results::{EnrollmentPending, FollowData};

        let data = FollowData {
            workspace_id: "w_acme".to_owned(),
            enrolled: false,
            skills: Vec::new(),
            workspace_display_name: Some("Acme Inc".to_owned()),
            plane_base_url: Some("https://api.topos.sh".to_owned()),
            pending: Some(EnrollmentPending {
                verification_uri: "https://topos.sh/verify".to_owned(),
                user_code: "WXYZ-1234".to_owned(),
                expires_at: None,
                interval_secs: Some(5),
            }),
            currency: None,
            triggers: Vec::new(),
        };
        let text = follow_tty(&data, &[]);
        // The bare URL and the SHORT code surface on SEPARATE lines — the code never rides a URL.
        assert!(text.contains("Open: https://topos.sh/verify"), "{text}");
        assert!(text.contains("Code: WXYZ-1234"), "{text}");
        assert!(!text.contains("verify?code="), "{text}");
        assert!(text.contains("confirm it matches"), "{text}");
    }

    #[test]
    fn status_tty_renders_both_enrollment_faces() {
        use topos_types::results::{StatusData, StatusTrigger, StatusWorkspace};
        let enrolled = StatusData {
            version: "0.1.0".to_owned(),
            enrolled: true,
            server: Some("https://topos.sh/api".to_owned()),
            signed_in: true,
            workspaces: vec![StatusWorkspace {
                workspace_id: "w_demo".to_owned(),
                name: "demo".to_owned(),
                display_name: "Demo".to_owned(),
                link_status: None,
            }],
            followed_skills: 2,
            pending_offers: Some(1),
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
        };
        let text = status_tty(&enrolled);
        assert!(text.contains("topos 0.1.0"), "{text}");
        assert!(
            text.contains("server: https://topos.sh/api — signed in"),
            "{text}"
        );
        assert!(text.contains("Demo (demo)"), "{text}");
        assert!(text.contains("following: 2 skills"), "{text}");
        assert!(
            text.contains("1 first-receive offer awaiting consent"),
            "{text}"
        );
        assert!(text.contains("claude-code: armed"), "{text}");
        assert!(
            text.contains("openclaw: unknown — presence needs"),
            "{text}"
        );

        // The unenrolled face states the fix in prose (join by address, or create a workspace).
        let fresh = StatusData {
            enrolled: false,
            server: None,
            signed_in: false,
            workspaces: Vec::new(),
            followed_skills: 0,
            pending_offers: Some(0),
            triggers: Vec::new(),
            ..enrolled
        };
        let text = status_tty(&fresh);
        assert!(text.contains("not enrolled"), "{text}");
        assert!(
            text.contains("`topos follow <workspace-address>`"),
            "{text}"
        );
        assert!(text.contains("https://topos.sh"), "{text}");
        assert!(text.contains("following: 0 skills"), "{text}");

        // The bare-`topos` welcome stays three lines: what topos is + the two ways in.
        let welcome = welcome_tty(&fresh);
        assert_eq!(welcome.lines().count(), 3, "{welcome}");
        assert!(
            welcome.contains("topos follow <workspace-address>"),
            "{welcome}"
        );
        assert!(welcome.contains("https://topos.sh"), "{welcome}");
    }

    #[test]
    fn not_enrolled_carries_the_join_template_with_its_needs() {
        let actions = super::next_actions(&crate::error::ClientError::NotEnrolled);
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "FOLLOW_WORKSPACE");
        assert_eq!(
            actions[0].argv,
            vec!["topos", "follow", "<workspace-address>", "--json"]
        );
        assert_eq!(actions[0].needs, vec!["workspace-address"]);
        // The prose states the same fix (never a structural mirror without the human half).
        let msg = safe_message(&crate::error::ClientError::NotEnrolled);
        assert!(msg.contains("`topos follow <workspace-address>`"), "{msg}");
    }

    #[test]
    fn prose_named_commands_mirror_structurally_on_the_fall_through() {
        // PATH_NOT_NAME's fix is TYPED — the envelope carries the concrete `topos add ./<arg>`.
        let actions = super::next_actions(&crate::error::ClientError::PathNotName {
            arg: "deploy".to_owned(),
        });
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "RUN_COMMAND");
        assert_eq!(actions[0].argv, vec!["topos", "add", "./deploy", "--json"]);
        assert!(actions[0].needs.is_empty());

        // A sign-in-in-progress refusal mirrors `topos auth login` under its own code.
        let actions = super::next_actions(&crate::error::ClientError::Enrollment(
            "a sign-in is in progress; re-run `topos auth login` to finish it first".to_owned(),
        ));
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "SIGN_IN");
        assert_eq!(actions[0].argv, vec!["topos", "auth", "login", "--json"]);

        // An expired flow mirrors the join template, needs and all.
        let actions = super::next_actions(&crate::error::ClientError::Enrollment(
            "the enrollment flow expired; start over with `topos follow <workspace-address>`"
                .to_owned(),
        ));
        assert_eq!(actions[0].code.as_str(), "FOLLOW_WORKSPACE");
        assert_eq!(actions[0].needs, vec!["workspace-address"]);

        // "upgrade topos" is `topos self-update`, structurally.
        let actions = super::next_actions(&crate::error::ClientError::UnknownSchemaVersion {
            found: 9,
            max: 1,
        });
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "UPDATE_CLI");

        // Divergent placements name the loss-led reset (a describe — nothing dropped yet).
        let actions = super::next_actions(&crate::error::ClientError::PlacementsDiverged {
            skill: "deploy".to_owned(),
            paths: vec!["/a".into(), "/b".into()],
        });
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "RESOLVE_DIVERGED_DRAFT");
        assert!(actions[0].argv.iter().any(|t| t == "--reset"));

        // A message without a backticked `topos …` command mirrors nothing.
        assert!(super::next_actions(&crate::error::ClientError::EmptyBundle).is_empty());
    }

    #[test]
    fn the_bareword_guard_names_both_deliberate_spellings() {
        for err in [
            crate::error::ClientError::BarewordEnrollUnconfirmed {
                name: "acme".to_owned(),
                server: "https://topos.sh".to_owned(),
            },
            crate::error::ClientError::BarewordEnrollDeclined {
                name: "acme".to_owned(),
                server: "https://topos.sh".to_owned(),
            },
        ] {
            let actions = super::next_actions(&err);
            assert_eq!(actions.len(), 2, "{actions:?}");
            assert!(
                actions
                    .iter()
                    .all(|a| a.code.as_str() == "FOLLOW_WORKSPACE")
            );
            // Both spellings are CONCRETE (no template holes): the explicit address form and
            // the consented bareword.
            assert_eq!(
                actions[0].argv,
                vec!["topos", "follow", "https://topos.sh/acme", "--json"]
            );
            assert_eq!(
                actions[1].argv,
                vec!["topos", "follow", "acme", "--yes", "--json"]
            );
            assert!(actions.iter().all(|a| a.needs.is_empty()));
        }
    }

    #[test]
    fn a_hostile_value_can_never_become_extra_argv_tokens() {
        // The classic injection: a directory literally named "skill --yes". The typed arm pushes
        // the value as ONE argv element — never re-tokenized — so no `--yes` token exists and the
        // consent flag cannot be smuggled into an executable next action.
        let actions = super::next_actions(&crate::error::ClientError::PathNotName {
            arg: "skill --yes".to_owned(),
        });
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(
            actions[0].argv,
            vec!["topos", "add", "./skill --yes", "--json"],
            "the hostile name stays one inert token"
        );
        assert!(!actions[0].argv.iter().any(|t| t == "--yes"), "{actions:?}");
        // Same discipline on the other typed value arms.
        let actions = super::next_actions(&crate::error::ClientError::HarnessMismatch {
            name: "pwn --yes".to_owned(),
            requested: "cursor".to_owned(),
            tracked: "claude-code".to_owned(),
        });
        assert_eq!(
            actions[0].argv,
            vec!["topos", "publish", "pwn --yes", "--json"]
        );
        let actions = super::next_actions(&crate::error::ClientError::PlacementOccupied {
            path: "/tmp/x --yes".to_owned(),
        });
        assert_eq!(
            actions[0].argv,
            vec!["topos", "add", "/tmp/x --yes", "--json"]
        );

        // And the prose mirror itself: a backticked command CONTAINING a runtime value matches
        // nothing in the static allowlist, so it mirrors NOTHING — message text is never
        // tokenized into argv.
        let actions = super::mirror_prose_commands(
            "adopt it in place (`topos add ./skill --yes`), or run `topos publish evil --yes`",
        );
        assert!(actions.is_empty(), "{actions:?}");
        // The allowlist still lifts its exact static spellings.
        let actions = super::mirror_prose_commands("run `topos follow <workspace-address>` first");
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0].argv,
            vec!["topos", "follow", "<workspace-address>", "--json"]
        );
    }

    #[test]
    fn auth_status_signed_out_carries_the_matching_fix() {
        use crate::ops::AuthStatusData;
        let fresh = AuthStatusData {
            server: None,
            principal: None,
            device_id: None,
            signed_in: false,
            workspaces: Vec::new(),
            hook_armed: false,
            reporting: Vec::new(),
        };
        let actions = auth_status_next_actions(&fresh);
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "FOLLOW_WORKSPACE");
        assert_eq!(actions[0].needs, vec!["workspace-address"]);
        let text = auth_status_tty(&fresh);
        assert!(
            text.contains("`topos follow <workspace-address>`"),
            "{text}"
        );

        // Enrolled but signed out: the fix is the concrete `auth login`.
        let signed_out = AuthStatusData {
            server: Some("https://topos.sh/api".to_owned()),
            ..fresh
        };
        let actions = auth_status_next_actions(&signed_out);
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "SIGN_IN");
        assert!(actions[0].needs.is_empty());
        let text = auth_status_tty(&signed_out);
        assert!(text.contains("`topos auth login`"), "{text}");

        // Signed in: nothing to fix.
        let signed_in = AuthStatusData {
            signed_in: true,
            server: Some("https://topos.sh/api".to_owned()),
            principal: None,
            device_id: None,
            workspaces: Vec::new(),
            hook_armed: true,
            reporting: Vec::new(),
        };
        assert!(auth_status_next_actions(&signed_in).is_empty());
    }

    #[test]
    fn publish_blocked_names_the_real_resolutions_never_the_bare_update() {
        // PUBLISH_BLOCKED only ever fires with a RECORDED conflict, and a plain `update <skill>`
        // merely RE-DISCLOSES a recorded conflict (it never re-merges) — an agent following that
        // pointer looped publish→update→publish forever. The envelope names the two acts that
        // actually clear the block: the `--onto-current` escape and the `--reset` discard.
        let actions = super::next_actions(&crate::error::ClientError::PublishBlocked {
            skill: "deploy-checklist".to_owned(),
        });
        assert_eq!(actions.len(), 2, "{actions:?}");
        assert!(
            actions[0].argv.iter().any(|t| t == "--onto-current"),
            "{actions:?}"
        );
        assert!(
            actions[1].argv.iter().any(|t| t == "--reset"),
            "{actions:?}"
        );
        for a in &actions {
            assert_eq!(a.code.as_str(), "RESOLVE_DIVERGED_DRAFT");
            assert!(
                a.argv.contains(&"deploy-checklist".to_owned()),
                "the argv must name the blocked skill: {a:?}"
            );
        }
    }
}
