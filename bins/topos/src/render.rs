//! Two presentations of one typed outcome: the `--json` envelope (the agent surface) and a thin TTY
//! renderer. Error messages are summarized so a raw git/io string never reaches a user surface.

use topos_types::requests::InvitationData;
use topos_types::results::{
    AddData, AddedNote, AgentView, ConflictHolds, DiffData, LogData, ProposeData, PublishData,
    PullData, PullSkill, RemoteSkill, RemoveData, RemoveItem, RemoveKind, RevertData, ReviewData,
    ReviewDecision, SkillEntry, UntrackedEntry,
};
use topos_types::{
    ActionCode, Affected, CurrencyKind, JsonEnvelope, NextAction, TerminalOutcome, TriggerState,
    WIRE_SCHEMA_VERSION, WireError,
};

use crate::bundle_kind::BundleKind;
use crate::error::{ClientError, TargetCandidate};
use crate::ops::ListOutcome;

/// A success envelope wrapping a verb's typed `data`.
pub(crate) fn ok_envelope(command: &str, data: serde_json::Value) -> JsonEnvelope {
    JsonEnvelope {
        schema_version: WIRE_SCHEMA_VERSION,
        command: command.to_owned(),
        ok: true,
        data,
        warnings: Vec::new(),
        messages: Vec::new(),
        next_actions: Vec::new(),
        receipt: None,
        error: None,
    }
}

/// A failure envelope carrying the stable code, outcome, and machine-actionable next steps. The
/// two ambiguity families — [`ClientError::AmbiguousTarget`] and the add-side
/// [`ClientError::AmbiguousSource`] — additionally surface their paste-ready spellings as
/// `data.candidates`, the machine-readable half of the refusal.
pub(crate) fn err_envelope(command: &str, argv: &[String], err: &ClientError) -> JsonEnvelope {
    let outcome = err.outcome();
    let next_actions = next_actions(command, argv, err);
    let retryable = matches!(
        outcome,
        TerminalOutcome::RetryableFailure | TerminalOutcome::Unavailable
    );
    let data = match err {
        // The RENDERED spelling of each candidate — the string this field has always carried;
        // the structure behind it is the client's, and the wire shape does not move for it.
        ClientError::AmbiguousTarget { candidates, .. }
        | ClientError::AmbiguousSource { candidates, .. } => {
            let candidates: Vec<String> = candidates.iter().map(|c| c.spelling()).collect();
            serde_json::json!({ "candidates": candidates })
        }
        // The already-added answer's OPTIONS are the same machine-readable half — when there are
        // any. Nothing provable to offer leaves the envelope exactly as it was.
        ClientError::AlreadyAdded { candidates, .. } if !candidates.is_empty() => {
            let candidates: Vec<String> = candidates.iter().map(|c| c.spelling()).collect();
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
        messages: Vec::new(),
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

/// The machine-actionable ways out of one failure. Two inputs no error variant holds ride
/// alongside it: `command`, the canonical VERB that refused (the same name the envelope carries),
/// and `argv`, the user's own invocation past the binary name. An ambiguity needs both — the verb
/// to rebuild with, and the whole invocation to judge whether rebuilding can be FAITHFUL at all.
pub(crate) fn next_actions(command: &str, argv: &[String], err: &ClientError) -> Vec<NextAction> {
    match err {
        // THE chooser: one runnable command per candidate, always — the whole point of the answer
        // is that every way out is spelled, and the reader (human or agent) picks a line.
        ClientError::AmbiguousSource { candidates, .. } => candidates
            .iter()
            .map(|candidate| {
                crate::actions::next_action(
                    ActionCode::from("RUN_COMMAND".to_owned()),
                    candidate_command(argv, err, &candidate.argv_tokens()),
                )
            })
            .collect(),
        // The already-added answer's OPTIONS are not a rebuild of the refused invocation — the
        // question they answer is a different one ("this copy too?"), so each is spelled whole:
        // the verb, the scope the answer is about, and the candidate's own tokens.
        //
        // Only the copies whose bytes a version of the bundle EXPLAINS are offered as commands
        // (see `TargetCandidate::offerable`). The answer LISTS every folder the name is in — that
        // is what a person reads to choose — but an argv handed to an agent is run, and running a
        // claim over a folder holding something else merges two histories that never met.
        ClientError::AlreadyAdded {
            scope, candidates, ..
        } => {
            let offerable: Vec<TargetCandidate> = candidates
                .iter()
                .filter(|c| c.offerable())
                .cloned()
                .collect();
            candidate_actions(
                "add",
                *scope == topos_types::results::ReceiptScope::Machine,
                &offerable,
            )
        }
        // Every "look at the discovered inventory to resolve this" error points the agent at `list` — the
        // count-only ambiguity plus the not-found cases from `add <skill>` name resolution.
        ClientError::AmbiguousName { .. }
        | ClientError::NoUntrackedSkill { .. }
        | ClientError::HarnessNotFound(_) => vec![disambiguate_with_list()],
        // A review verdict on a no-longer-open proposal — point the agent at the open inbox.
        ClientError::ReviewNotOpen(_) => vec![crate::actions::next_action(
            ActionCode::from("REVIEW_INBOX".to_owned()),
            vec!["topos".into(), "review".into(), "--json".into()],
        )],
        // A stale base — update to rebase, then re-show the diff and retry. Never a silent retry.
        ClientError::Conflict { skill, .. } => vec![crate::actions::next_action(
            ActionCode::RebaseAndRetry,
            update_rebuild(command, argv, skill, &[]),
        )],
        // An unresolved author merge blocks publish. The block only ever exists with a RECORDED
        // conflict, and a plain `update <skill>` merely RE-DISCLOSES a recorded conflict (it never
        // re-merges) — naming it here sent agents into a publish→update→publish refusal loop. Name
        // the two acts that actually RESOLVE the block: keep yours (`--keep-mine` commits your
        // edited resolution — or your original draft — onto current and clears the record) or let
        // the team win (`--reset`, the loss-led two-phase discard).
        //
        // Spelled for the scope the BLOCK is in, not the one the invocation stood in: `publish`
        // carries no scope flag and resolves home-first, so a rebuild from its argv can offer an
        // exit that drives the other copy and refuses all over again.
        ClientError::PublishBlocked { skill, global } => vec![
            crate::actions::next_action(
                ActionCode::ResolveDivergedDraft,
                update_at_scope(*global, skill, &["--keep-mine"]),
            ),
            crate::actions::next_action(
                ActionCode::ResolveDivergedDraft,
                update_at_scope(*global, skill, &["--reset"]),
            ),
        ],
        // A copy behind the served `current`. ONE way out, and it is the same one the sentence names:
        // update, which runs the merge that makes a publish safe to ship — in the scope that copy
        // lives in, for the reason the block above carries its own.
        ClientError::PublishBehind { skill, global } => vec![crate::actions::next_action(
            ActionCode::RebaseAndRetry,
            update_at_scope(*global, skill, &[]),
        )],
        // No merge stopped, so the fix is the merge itself — the one that lands the team's
        // non-conflicting changes and stops only where the two sides really collide. Spelled for
        // the scope the REFUSAL names (the copy `--keep-mine` was aimed at), the same way the
        // sentence spells it: a merge waiting machine-wide, offered without `-g`, drives the
        // project's copy from inside a checkout and refuses all over again.
        ClientError::NoStoppedMerge { skill, global } => vec![crate::actions::next_action(
            ActionCode::ApplyWaitingUpdate,
            update_at_scope(*global, skill, &[]),
        )],
        // A denial is not self-service (ask an owner to invite/roster you, or contact an admin) — the
        // codes carry no executable argv.
        ClientError::Denied(_) | ClientError::MailNotConfigured => vec![
            crate::actions::next_action(ActionCode::RequestAccess, Vec::new()),
            crate::actions::next_action(ActionCode::ContactAdmin, Vec::new()),
        ],
        // A denied enrollment redeem (authenticated-but-uninvited): the ask-an-owner guidance rides the
        // message; the action code is the existing REQUEST_ACCESS (no argv — the fix is another human's).
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
            ActionCode::from("LOGIN_WORKSPACE".to_owned()),
            vec![
                "topos".into(),
                "login".into(),
                "<workspace-address>".into(),
                "--json".into(),
            ],
        )],
        // The session-required refusal with the address IN HAND: the login rides as an
        // EXECUTABLE argv — the address as ONE token straight from the variant's field, never
        // re-tokenized out of prose (a placeholder address stays a template via `needs`).
        ClientError::SessionRequired { address, .. } => vec![crate::actions::next_action(
            ActionCode::from("LOGIN_WORKSPACE".to_owned()),
            vec![
                "topos".into(),
                "login".into(),
                address.clone(),
                "--json".into(),
            ],
        )],
        // No `topos.toml` covers this folder: the two ways out are the two SCOPES, and neither is
        // guessed at. Creating the file here is `init`'s job (the cargo/pnpm/git precedent); the
        // other is this very invocation aimed at the machine-wide file — rebuilt from the argv, so
        // nothing about the request is lost but the scope it could not resolve.
        ClientError::NoManifest => vec![
            crate::actions::next_action(
                ActionCode::from("INIT_PROJECT_MANIFEST".to_owned()),
                vec!["topos".into(), "init".into()],
            ),
            crate::actions::next_action(
                ActionCode::from("RETRY_MACHINE_WIDE".to_owned()),
                retry_machine_wide(command, argv),
            ),
        ],
        // "upgrade topos" in prose is `topos self-update` structurally.
        ClientError::UnknownSchemaVersion { .. } => vec![crate::actions::next_action(
            ActionCode::from("UPDATE_CLI".to_owned()),
            vec!["topos".into(), "self-update".into()],
        )],
        // The server refused this build: the only way forward is a newer one. The floor it named
        // rides the message, not the argv — `self-update` lands the latest, which clears any floor.
        ClientError::UpdateRequired { .. } => vec![crate::actions::next_action(
            ActionCode::SelfUpdate,
            vec!["topos".into(), "self-update".into()],
        )],
        // The mirror image: THIS build is ahead of the server. Only one of the two remedies is
        // this machine's to run, so only that one rides as an argv — pinned to the server's own
        // release, which is the version it is provably able to speak to.
        ClientError::ServerTooOld { server_version } => vec![crate::actions::next_action(
            ActionCode::SelfUpdate,
            vec![
                "topos".into(),
                "self-update".into(),
                "--version".into(),
                format!("v{server_version}"),
            ],
        )],
        // Divergent per-placement edits: the prose names the loss-led discard; mirror it (the
        // bare `--reset` DESCRIBES — nothing is dropped without its own `--yes`).
        ClientError::PlacementsDiverged { skill, .. } => vec![crate::actions::next_action(
            ActionCode::ResolveDivergedDraft,
            update_rebuild(command, argv, skill, &["--reset"]),
        )],
        // The value-carrying fixes, TYPED: the runtime value rides as exactly ONE argv element,
        // straight from the error variant's own field — never re-tokenized out of prose, so a
        // hostile name containing whitespace or a `--flag` fragment stays one inert token.
        // The fix re-runs the VERB that refused, spelled as a path — the same command the message
        // prints. Hardcoding `add` here sent a person who typed `publish` (or `remove`) off to a
        // different verb than the one they were using.
        ClientError::PathNotName { arg, verb } => vec![crate::actions::next_action(
            ActionCode::from("RUN_COMMAND".to_owned()),
            vec![
                "topos".into(),
                verb.clone(),
                crate::error::path_token(arg),
                "--json".into(),
            ],
        )],
        // The shared-copy narrowing refusal: BOTH ways out, structurally — the whole-row remove
        // and the per-agent re-add — each argv straight from the error's own fields (the TTY
        // prints the same two lines, annotated).
        ClientError::SharedCopyOnly {
            remove_argv,
            readd_argv,
            ..
        } => vec![
            crate::actions::next_action(
                ActionCode::from("RUN_COMMAND".to_owned()),
                remove_argv.clone(),
            ),
            crate::actions::next_action(
                ActionCode::from("RUN_COMMAND".to_owned()),
                readd_argv.clone(),
            ),
        ],
        // A folder whose kind the door could not read: the fix is the `--kind` word, runnable as
        // is. A folder carrying BOTH markers gets BOTH words — an agent must not have to pick
        // between a real choice and a guess, and offering only one would be this refusal making
        // the very decision it just declined to make.
        ClientError::KindRequired { dir, ambiguous, .. } => {
            let run = |kind: &str| {
                crate::actions::next_action(
                    ActionCode::from("RUN_COMMAND".to_owned()),
                    vec![
                        "topos".into(),
                        "add".into(),
                        "--kind".into(),
                        kind.into(),
                        dir.clone(),
                        "--json".into(),
                    ],
                )
            };
            if *ambiguous {
                vec![run("skill"), run("mcp")]
            } else {
                vec![run("mcp")]
            }
        }
        ClientError::AlreadyTrackedName { name } => vec![crate::actions::next_action(
            ActionCode::from("RUN_COMMAND".to_owned()),
            vec!["topos".into(), "diff".into(), name.clone(), "--json".into()],
        )],
        // A folder a row already installs: both exits ride structurally — the read that shows
        // what the tracked copy holds, and the drop of the claiming row. The `-g` goes on ONLY
        // where the claim PROVED the machine file; inventing it would aim the remove at the
        // scope nobody asked about, and omitting it where it was proved would do the same.
        ClientError::AlreadyTracked { name, claim, .. } => {
            let mut remove = vec!["topos".to_owned(), "remove".to_owned()];
            if claim.as_ref().is_some_and(|c| c.global) {
                remove.push("-g".to_owned());
            }
            remove.push(name.clone());
            remove.push("--json".to_owned());
            vec![
                crate::actions::next_action(
                    ActionCode::from("RUN_COMMAND".to_owned()),
                    vec!["topos".into(), "diff".into(), name.clone(), "--json".into()],
                ),
                crate::actions::next_action(ActionCode::from("RUN_COMMAND".to_owned()), remove),
            ]
        }
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
        // An ambiguous name: ONE runnable way out per candidate, each the FAILING invocation
        // re-spelled — same verb, this candidate's tokens, and the refused invocation's `-g` (the
        // same preservation `subscribe_action` makes: the machine-wide file and this folder's are
        // two different documents, so a rebuilt command that dropped the flag would edit the one
        // nobody asked about).
        //
        // THE FAITHFULNESS RULE, in two halves that must BOTH pass — a rebuilt command is offered
        // only where it is the whole of what was asked, with one name swapped for one candidate.
        //
        // The VERB half (a closed match): `remove` and `add` take a target and act on it.
        // `protect` does not qualify — its optional LEVEL positional decides which act it is, so
        // rebuilding `topos protect <name> open` as `topos protect <candidate>` offers the
        // opposite (a tighten where a loosen was asked). A verb added later lands in `_` and stays
        // silent until someone proves its argv reconstructible.
        //
        // The INVOCATION half: even `remove` is only reconstructible when the ambiguous token was
        // the whole request. `topos remove foo bar --yes` rebuilt as `topos remove <candidate>`
        // succeeds while silently abandoning `bar` — the caller would believe their batch ran.
        //
        // Either half failing means NO offer: the short refusal stands, and the agent still gets
        // every way out in `data.candidates`. A wrong offered command is worse than none.
        ClientError::AmbiguousTarget {
            candidates, global, ..
        } => match (command, is_lone_target_invocation(argv)) {
            ("remove" | "add", true) => candidate_actions(command, *global, candidates),
            _ => Vec::new(),
        },
        // Everything else: a refusal whose PROSE names one of the STATIC command spellings
        // mirrors it structurally. Prose text is never tokenized into argv — see the allowlist.
        other => mirror_prose_commands(&safe_message(other)),
    }
}

/// One runnable command per ambiguity candidate: the verb that refused, the invocation's own `-g`,
/// then the candidate's OWN argv tokens — the reference always exactly one of them, so a path
/// holding whitespace (or a `--yes`) can never become argv of its own.
fn candidate_actions(
    verb: &str,
    global: bool,
    candidates: &[crate::error::TargetCandidate],
) -> Vec<NextAction> {
    candidates
        .iter()
        .map(|candidate| {
            let mut rebuilt = vec!["topos".to_owned(), verb.to_owned()];
            if global {
                rebuilt.push("-g".to_owned());
            }
            rebuilt.extend(candidate.argv_tokens());
            rebuilt.push("--json".to_owned());
            crate::actions::next_action(ActionCode::from("RUN_COMMAND".to_owned()), rebuilt)
        })
        .collect()
}

/// The command ONE chooser candidate stands for: THE REFUSED INVOCATION, with the ambiguous
/// positional swapped for this candidate's tokens and nothing else touched.
///
/// Carrying the flags is the whole point. A rebuild that kept only the verb and the candidate
/// turned `publish foo --propose` into a direct publish and `add foo -a cursor` into an install
/// for every agent — offering an act nobody asked for, which is worse than offering nothing. The
/// swap needs no flag table and no guess at which token is a flag's VALUE: every token but the one
/// the person typed as the target rides through exactly as typed.
///
/// Two tokens are dropped. `--json` is the agent half of the same action and is re-appended here.
/// `--yes` is dropped deliberately — consent was given to a target the caller has now been told
/// was ambiguous, and a candidate is a source they have not seen described; they can re-run with
/// it themselves. (This is the discipline [`is_lone_target_invocation`] states for the other
/// ambiguity family, kept here without having to withhold the offer at all.)
///
/// An invocation whose target token is not in `argv` falls back to the plain `verb + candidate`
/// spelling the error carries for exactly that case.
fn candidate_command(argv: &[String], err: &ClientError, tokens: &[String]) -> Vec<String> {
    let ClientError::AmbiguousSource {
        token,
        verb,
        global,
        ..
    } = err
    else {
        return Vec::new();
    };
    if !argv.iter().any(|a| a == token) {
        let mut fallback = vec!["topos".to_owned(), verb.clone()];
        if *global {
            fallback.push("-g".to_owned());
        }
        fallback.extend(tokens.iter().cloned());
        fallback.push("--json".to_owned());
        return fallback;
    }
    let mut rebuilt = vec!["topos".to_owned()];
    let mut swapped = false;
    for arg in argv {
        if arg == "--json" || arg == "--yes" {
            continue;
        }
        if !swapped && arg == token {
            rebuilt.extend(tokens.iter().cloned());
            swapped = true;
            continue;
        }
        rebuilt.push(arg.clone());
    }
    rebuilt.push("--json".to_owned());
    rebuilt
}

/// Whether the refused invocation was EXACTLY its verb plus the ONE ambiguous token — the only
/// shape a per-candidate rebuild can honor, since the rebuild says nothing but "this verb, this
/// candidate". Read from the user's own argv (past the binary name), because only the argv knows:
/// the typed error carries the name that failed, never the request it was part of.
///
/// Three tokens are looked past. The VERB — the first non-flag token, however the user spelled it
/// (`pull` for `update`) — is what the rebuild re-states. `--json` is the agent half of the same
/// action, and the rebuild appends its own. `-g`/`--global` is preserved structurally by the
/// error's own `global` field, so seeing it here changes nothing.
///
/// EVERY other flag disqualifies, `--yes` most of all — and deliberately, even though it could be
/// carried over. Re-offering it would apply an act against a target the caller has never seen
/// described: they consented to losing `foo`, and the rebuild is about a candidate they are only
/// now choosing between. They can re-run with `--yes` themselves, having read the describe.
///
/// A second positional disqualifies too: a rebuilt single-target command would drop the rest of
/// the batch and still exit 0. And an EMPTY argv (no invocation in hand) fails toward silence.
fn is_lone_target_invocation(argv: &[String]) -> bool {
    let mut verb_seen = false;
    let mut positionals = 0_usize;
    for token in argv {
        match token.as_str() {
            // Carried by the rebuild itself, or by the error's own scope field.
            "--json" | "-g" | "--global" => {}
            // Anything else flag-shaped is part of the request the rebuild cannot restate —
            // including a value-taking selector, whose VALUE then reads as a positional and
            // disqualifies the shape a second time. Both directions fail closed.
            t if t.starts_with('-') => return false,
            _ if !verb_seen => verb_seen = true,
            _ => positionals += 1,
        }
    }
    verb_seen && positionals == 1
}

/// A rebuilt `topos update <skill> …` way-out that PRESERVES the refused invocation's scope: an
/// `update -g` that refused must be offered a `-g` resolution, or the offered command would act on
/// the OTHER scope's copy (the project's, from inside a checkout). The flag carries over only when
/// the refused verb was `update` itself and spelled it — reading a scope out of some OTHER verb's
/// argv would be inventing one. A refusal whose own TYPE knows the scope says so directly through
/// [`update_at_scope`] instead; that is a fact, not an inference from what the user typed.
fn update_rebuild(command: &str, argv: &[String], skill: &str, extras: &[&str]) -> Vec<String> {
    update_at_scope(
        command == "update" && argv.iter().any(|t| t == "-g" || t == "--global"),
        skill,
        extras,
    )
}

/// The same offered `update`, spelled for a scope the REFUSAL knows and the invocation does not.
/// A bare `publish` resolves the DRAFTED copy across scopes, so an exit rebuilt from its argv would
/// drive whichever copy that resolution happens to pick — not necessarily the one that refused. The
/// typed error carries the answer; this is where it is spelled.
fn update_at_scope(global: bool, skill: &str, extras: &[&str]) -> Vec<String> {
    let mut out = vec!["topos".to_owned(), "update".to_owned()];
    if global {
        out.push("-g".to_owned());
    }
    out.push(skill.to_owned());
    out.extend(extras.iter().map(|s| (*s).to_owned()));
    out.push("--json".to_owned());
    out
}

/// The refused invocation re-spelled at the MACHINE scope — the user's OWN argv with `-g` inserted
/// right after the verb, so the retry is the whole of what was asked with only the scope changed.
/// The verb is the first non-flag token, however it was spelled; `--json` is filtered out (the
/// agent surface appends its own, and the TTY hint drops it anyway). An argv with no verb in it
/// (or none at all) falls back to the bare `topos <command> -g`.
fn retry_machine_wide(command: &str, argv: &[String]) -> Vec<String> {
    let mut rest: Vec<String> = argv
        .iter()
        .filter(|t| t.as_str() != "--json")
        .cloned()
        .collect();
    let Some(verb_at) = rest.iter().position(|t| !t.starts_with('-')) else {
        return vec!["topos".to_owned(), command.to_owned(), "-g".to_owned()];
    };
    rest.insert(verb_at + 1, "-g".to_owned());
    let mut out = vec!["topos".to_owned()];
    out.extend(rest);
    out
}

/// The inventory read every name-resolution refusal offers: what is adoptable here, machine-readable.
fn disambiguate_with_list() -> NextAction {
    crate::actions::next_action(
        ActionCode::DisambiguateName,
        vec!["topos".into(), "list".into(), "--json".into()],
    )
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
        "topos login <workspace-address>",
        "LOGIN_WORKSPACE",
        &["topos", "login", "<workspace-address>", "--json"],
    ),
    // The bare resume (a pending flow's way forward — `login` with no address re-polls).
    ("topos login", "RESUME_LOGIN", &["topos", "login", "--json"]),
    (
        "topos login <server>/<workspace>",
        "RUN_COMMAND",
        &["topos", "login", "<server>/<workspace>", "--json"],
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
        ClientError::Gitstore(_) => "the local skill store reported an error".to_owned(),
        ClientError::Verify(_) => "an integrity check failed".to_owned(),
        ClientError::Corrupt(_) => "topos's own state on this machine is unreadable".to_owned(),
        ClientError::WireInvalid(_) => "the server sent a response topos could not read".to_owned(),
        // The remaining Display strings are fixed text, a user-supplied name, or (InvalidArgument)
        // usage guidance written by this code — safe to show verbatim. `Scan` is among them: every
        // reason is a bundle-relative name inside the folder the person themselves named, or the
        // kernel's own fixed words — and a rejected folder is useless news without the file that
        // caused it.
        other => other.to_string(),
    }
}

/// Serialize an envelope as one line of JSON (stdout; diagnostics go to stderr).
pub(crate) fn to_json(envelope: &JsonEnvelope) -> String {
    serde_json::to_string(envelope).unwrap_or_else(|_| "{\"ok\":false}".to_owned())
}

/// The `login` receipt — pending (the approval instructions) or the signed-in session. ONE
/// renderer for all three connected arms: the browser grant, the browser-free lane connect, and a
/// workspace this machine already reaches. The signed-in receipt leads with who and where; on
/// this machine's FIRST connection to the workspace it states what the feed line it just wrote
/// means, undo-led — a re-login prints the first line alone (the line's absence from the file is
/// deliberate, and login never re-adds it).
pub(crate) fn session_login_tty(data: &topos_types::results::LoginData) -> String {
    let server = if data.host.is_empty() {
        data.server.clone().unwrap_or_default()
    } else {
        data.host.clone()
    };
    if let Some(p) = &data.pending {
        // A bare `topos login` has no workspace to name yet — the human picks (or creates) one at
        // the approval, so the lead names the SERVER this login is toward.
        let lead = if data.name.is_empty() {
            format!("Logging in to {server} — choose your workspace in the browser.")
        } else {
            format!("Logging in to {}.", address_of(data, &server))
        };
        return format!(
            "{lead}\nApprove this login in your browser:\n  Open: {}\n  Code: {} (the page shows \
             the same code — confirm it matches)\nThen re-run `topos login` to finish.",
            p.verification_uri, p.user_code
        );
    }
    if data.session_status == "pending" {
        // The owner-approval hold: nothing flows yet, so the receipt promises nothing.
        let label = data
            .display_name
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| data.name.clone());
        let mut s = format!(
            "Connected to {} — the session awaits an owner's approval; nothing arrives until \
             then.",
            if data.name.is_empty() {
                &label
            } else {
                &data.name
            }
        );
        if let Some(note) = &data.manifest_note {
            s.push_str(&format!("\nmanifest: {note}"));
        }
        s.push_str(&format!(
            "\nUndo: topos logout{}",
            if data.name.is_empty() {
                String::new()
            } else {
                format!(" {}", data.name)
            }
        ));
        return s;
    }
    let mut s = format!("signed in to {}", address_of(data, &server));
    if let Some(user) = data.user.as_deref().filter(|u| !u.is_empty()) {
        s.push_str(&format!(" as {user}"));
    }
    if data.feed_row_added {
        // The feed line this login wrote (only ever on the machine's first connection): what it
        // means, then the paste-ready inverse.
        s.push_str(&format!(
            "\nwhat {} delivers to you installs on this machine",
            data.name
        ));
        if !data.undo.is_empty() {
            s.push_str(&format!("\n(undo: topos {})", data.undo.join(" ")));
        }
    }
    // The honest disclosure when the feed line could NOT be recorded (an unreadable or
    // unwritable file) — never a failed login.
    if let Some(note) = &data.manifest_note {
        s.push_str(&format!("\nmanifest: {note}"));
    }
    s
}

/// The workspace's full address for a receipt — `<host>/<name>` when the host is known (it always
/// is on a live session), else the bare name.
fn address_of(data: &topos_types::results::LoginData, server: &str) -> String {
    if server.is_empty() {
        data.name.clone()
    } else {
        format!("{server}/{}", data.name)
    }
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
    s.push_str(
        "\nSkills, drafts, and manifests stay — `topos login <workspace-address>` signs back in.",
    );
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

/// The FIRST line of every add receipt: what was added, and which of the two files now asks for
/// it — named by its one portable spelling, never by the absolute path a reader's own cwd makes
/// noise of. The paste-ready inverse rides it, as it always has.
fn added_lead(data: &AddData) -> String {
    let mut lead = match data.scope {
        Some(scope) => format!("added {} {}", data.name, crate::error::scope_file(scope)),
        // No row was recorded (an internal adopt) — there is no file to name.
        None => format!("added {}", data.name),
    };
    if !data.undo.is_empty() {
        lead.push_str(&format!(" (undo: {})", argv_line(&data.undo)));
    }
    lead
}

/// The SECOND line of every add receipt: where this bundle comes from, in the one spelling that
/// stays runnable — a workspace or forge reference, or the folder on this machine. Empty (no
/// line at all) only where the receipt genuinely has no source to name.
fn source_line(data: &AddData) -> String {
    match &data.source {
        Some(source) => format!("source: {}\n", tty_path(source)),
        None => String::new(),
    }
}

/// The line a machine-wide add prints when it has just created a SECOND checkout: the project you
/// are standing in already delivers this bundle, and this add gives you a copy of your own beside
/// it. Two copies, two drafts, two version histories — stated at the moment the second one is
/// born, because no other line on the receipt says a first one existed.
///
/// Empty (no line at all) wherever the producer found no such folder — see
/// [`crate::ops::reference`]'s own gate: nothing here decides WHETHER there is a second copy, only
/// how it reads.
fn machine_copy_line(data: &AddData) -> String {
    match &data.machine_copy {
        Some(folder) => format!(
            "this project already delivers '{}' — this adds your machine copy ({})\n",
            data.name,
            tty_path(folder)
        ),
        None => String::new(),
    }
}

/// A path as the TERMINAL spells it: `~/`-abbreviated under this machine's home, verbatim
/// otherwise (and a reference, which holds no leading path, passes straight through).
///
/// ONE derivation for every printed path, because the surfaces used to disagree about the same
/// folder — a plain add's `source:` printed it in full while the already-added answer and the
/// claim receipt printed `~/…`, so two receipts about one directory did not look like one
/// directory. The WIRE keeps the absolute form: a machine reading `source` resolves it without a
/// shell, and `~/` is a shell's to expand.
fn tty_path(raw: &str) -> String {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return raw.to_owned();
    };
    // The ONE home read on this side, resolved the same way the roots are: a `$HOME` carrying a
    // symlink would otherwise fail to prefix the resolved paths every producer hands over, and the
    // abbreviation would silently stop happening.
    let home = home.canonicalize().unwrap_or(home);
    match std::path::Path::new(raw).strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => raw.to_owned(),
    }
}

/// The IDENTITY CLAIM's receipt (`add <path> --as <bundle>`): the folder is now one of this
/// bundle's places, and the tail says what that means for the bytes already in it — nothing
/// changed at claim time, and what the next update will do about it depends only on what they are.
///
/// The `source:` line is the same one every other add answer carries; the twin line, when one is
/// there, is a second fact about a second folder and gets a line of its own.
fn add_claim_tty(data: &AddData, claim: &topos_types::results::ClaimReceipt) -> String {
    use topos_types::results::ClaimState;
    let name = &data.name;
    let tail = match claim.state {
        ClaimState::Current => "nothing in it changed; updates land here from now on".to_owned(),
        ClaimState::Older => {
            "it holds an older version; the next update brings it current".to_owned()
        }
        // THE SPREAD IS REAL and must be disclosed exactly here: a settled draft is copied onto
        // this bundle's other folders, and `publish` offers it to the team.
        ClaimState::Edited => format!(
            "its edits become your draft of this skill: they will reach its other folders and \
             `topos publish` will offer them (`topos diff {name}` shows them)"
        ),
        // No spread to promise: the copies disagree, so nothing syncs until one is chosen.
        ClaimState::Frozen => format!(
            "its edits differ from another folder's, so nothing syncs until one is chosen (`topos \
             update {name} --reset` discards every copy's edits)"
        ),
    };
    let mut out = format!("added {} as a copy of {name} — {tail}\n", claim.folder);
    out.push_str(&source_line(data));
    if let Some(twin) = &claim.twin {
        out.push_str(&if twin.removed {
            format!(
                "removed the duplicate {} (unedited copy of the same skill)\n",
                twin.folder
            )
        } else {
            format!(
                "kept the duplicate {} — it is edited, so both copies stay\n",
                twin.folder
            )
        });
    }
    if let Some(note) = &data.note {
        out.push_str(&format!("note: {note}\n"));
    }
    out.trim_end().to_owned()
}

pub(crate) fn add_tty(data: &AddData) -> String {
    // NOTHING CHANGED IS THE WHOLE ANSWER, so it is the whole lead. An `added …` / `+ … installed`
    // headline above a note retracting it announces an act that did not happen, and the reader has
    // to get to the second line to learn the first one was not true. The `source:` line still
    // rides: which bundle this was about is exactly what a reader checks next.
    if data.unchanged {
        let mut out = match &data.note {
            Some(note) => format!("{note}\n"),
            None => String::new(),
        };
        out.push_str(&source_line(data));
        return out.trim_end().to_owned();
    }
    // The identity claim has its own two lines: no row was written for it, so the file-naming
    // lead every other add prints would name a file this act never touched.
    if let Some(claim) = &data.claim {
        return add_claim_tty(data, claim);
    }
    // A `-a`/`--dest` add prints the DESTINATION receipt whatever the SOURCE was: the person
    // named where the copy should land, so where it landed is the receipt's first claim — the
    // `+` row with the destination, then the undo-led inverse. A workspace bundle names itself
    // by its qualified `display`; a local folder or a forge import has none, and the plain name
    // is what the person typed anyway. The gate is the SELECTION, never the source kind: a
    // receipt that never names the folder after `-a codex` answers a question nobody asked.
    if !data.dest.is_empty() {
        return add_dest_receipt(data);
    }
    let mut out = format!("{}\n", added_lead(data));
    out.push_str(&source_line(data));
    out.push_str(&machine_copy_line(data));
    // The disclosure a plain row write did not carry: a file born by this act, a row NOT written
    // (the feed already delivers it), an `off` switch deleted instead, a standing web decline.
    if let Some(note) = &data.note {
        // ONE CLAUSE PER LINE — the same rule the removal receipt follows: a note that needs a
        // list (the per-config outcomes of an MCP converge) leads with its first line and indents
        // the rest, instead of running every clause together behind separators.
        let mut lines = note.lines();
        if let Some(first) = lines.next() {
            out.push_str(&format!("note: {first}\n"));
            for line in lines {
                out.push_str(&format!("      {line}\n"));
            }
        }
    }
    // A SET reference (a channel, a feed, a whole repo) has no bytes of its own, so the receipt
    // closes by stating what the row expands to instead.
    if data.skill_id.is_none() {
        let sentence = match data
            .reference
            .as_deref()
            .and_then(|r| crate::manifest::keys::classify_key(r).ok())
        {
            Some(crate::manifest::keys::KeyShape::Feed { workspace, .. }) => format!(
                "This machine now takes whatever {workspace} gives you — `topos update` delivers \
                 it."
            ),
            Some(crate::manifest::keys::KeyShape::RepoSet { .. }) => format!(
                "Added the '{}' repository — its skills deliver with `topos update`.",
                data.name
            ),
            _ => format!(
                "Added the '{}' channel — its skills deliver with `topos update` (and just did, \
                 where reachable).",
                data.name
            ),
        };
        out.push_str(&sentence);
        return out.trim_end().to_owned();
    }
    // Everything below appends `\n`-led clauses to the receipt's last line.
    out.truncate(out.trim_end().len());
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
            // Every non-`Active` state carries the explicit-pull floor (the one report constructor
            // applies it), so the copy below names no update moment it cannot back.
            (TriggerState::AlreadyPresentUnmanaged, _) => {
                "\nLeft your existing auto-update trigger untouched."
            }
            (TriggerState::Degraded, _) => {
                "\nCouldn't update the harness config for the auto-update trigger — left it untouched; run `topos update` to check for updates."
            }
            (TriggerState::Inactive, _) => "",
        });
    }
    out.push_str(&breadth_trigger_lines(&data.triggers));
    // The same courtesy for a NAME: the local dir was adopted as asked, and a connected workspace
    // publishes that name too — so the team-managed spelling is named beside it. Proven-identical
    // bytes say so, because then the local copy has nothing the team's copy lacks.
    if let Some(p) = &data.published_match {
        out.push_str(&format!(
            "\nAlso published: workspace '{}' has '{}' as {} — {} `topos add {}` subscribes to the \
             team's copy (delivered and kept current), instead of this local one.",
            p.workspace,
            p.name,
            p.reference,
            if p.identical {
                "the bytes you just adopted are identical to what it serves today;"
            } else {
                "its version may differ from yours;"
            },
            p.reference
        ));
    }
    // The DEDUP courtesy on a remote import: a connected workspace already governs this source —
    // name the reference (visible, never blocking; the import above landed as asked).
    if let Some(g) = &data.governed_copy {
        out.push_str(&format!(
            "\nAlready governed: workspace '{}' has this {} as {} — `topos add {}` delivers the \
             team's copy (updates, review, one shared history) instead of a separate import.",
            g.workspace,
            if g.same_path { "source" } else { "repository" },
            g.reference,
            g.reference
        ));
    }
    out
}

/// The destination receipt EVERY `-a`/`--dest` add prints — the same column convention the
/// update receipt's `+` rows use, speaking in the row's recorded destinations: one entry prints
/// its spelling, several print a count in the bundle's own noun. The bundle is named by its
/// workspace-qualified `display` where it has one (a workspace reference) and by its plain name
/// where it does not (a local folder, a forge import).
fn add_dest_receipt(data: &AddData) -> String {
    let name = data.display.as_deref().unwrap_or(&data.name);
    // The subject comes from WHAT THE BUNDLE IS, asked once. This site used to guess it by
    // sniffing every `dest` entry for a known MCP config spelling — a fourth, different answer to
    // a question the kind already settles.
    let subject = if data.mcp.is_some() {
        crate::actions::Subject::McpServer
    } else {
        crate::actions::Subject::Skill
    };
    // WHAT ACTUALLY HAPPENED. A row can be written on a machine that has nowhere to put its
    // bytes — no agent of that kind is set up here — and the receipt then said `installed` above
    // a note saying no agent was set up yet: a headline contradicting its own next line. The row
    // IS the durable act, so it is what the headline reports.
    let landed_nowhere = data.mcp.as_ref().is_some_and(|m| m.agents.is_empty());
    // WHERE THE BYTES WENT, not how the row spells it. `dest` is the manifest's own portable
    // spelling and stays in the file; but an env override (`$CODEX_HOME`, `$XDG_CONFIG_HOME`,
    // `$CLAUDE_CONFIG_DIR`) moves where that spelling lands, and this headline used to name
    // `~/.codex/config.toml` on a machine whose entry went to `$CODEX_HOME/config.toml` — a path
    // the reader could go and look at and find nothing. The note beneath already said the true
    // one; the two now agree.
    let shown_dest = if data.dest_resolved.is_empty() {
        &data.dest
    } else {
        &data.dest_resolved
    };
    let column = match (landed_nowhere, shown_dest.as_slice()) {
        (true, _) => "recorded — no agent is set up here yet".to_owned(),
        (false, [one]) => format!("installed ({one})"),
        (false, many) => format!("installed ({})", subject.targets(many.len())),
    };
    // A row that ALREADY stood gained destinations; it was not installed here for the first time,
    // and a `+ … installed (…)` headline over that reads as a new arrival. The change itself is
    // the answer.
    let lead = match &data.dest_change {
        Some(change) => dest_change_lead(name, change),
        None => format!("+ {name}   {column}"),
    };
    let mut s = format!("{lead}\n");
    // The same second line every other add answer carries — the destination receipt says where
    // the copy went, and this says what it is a copy OF.
    s.push_str(&source_line(data));
    s.push_str(&machine_copy_line(data));
    s.truncate(s.trim_end().len());
    if !data.undo.is_empty() {
        s.push_str(&format!("\n(undo: {})", argv_line(&data.undo)));
    }
    if let Some(note) = &data.note {
        s.push_str(&format!("\nnote: {note}"));
    }
    s
}

/// The lead line of an add that changed a STANDING row's destinations.
///
/// A first freeze has to say what it froze: the row reached every agent a moment ago, so the whole
/// set that replaced "everywhere" is listed, one per line — otherwise nobody could tell what
/// quietly stopped being implied. A row that already named destinations only gained some, and
/// naming those is the whole story.
fn dest_change_lead(name: &str, change: &topos_types::results::DestChange) -> String {
    if change.frozen.is_empty() {
        return format!("added {} to {name}'s destinations", change.added.join(", "));
    }
    let plural = if change.added.len() == 1 {
        "one"
    } else {
        "ones"
    };
    let mut lead = format!(
        "{name} reached every agent — recorded its current destinations plus the new {plural}:"
    );
    for entry in &change.frozen {
        lead.push_str(&format!("\n  {entry}"));
    }
    lead
}

/// The describe an `add` of a git source always returns: what the source holds, what would be
/// written where, and the one command that applies it. Nothing has landed.
pub(crate) fn add_describe_tty(
    data: &topos_types::results::AddDescribeData,
    yes_argv: &[String],
) -> String {
    // The opener NAMES the source and claims nothing about the machine. It used to say nothing had
    // been downloaded yet, which was true only of a source being seen for the first time — and a
    // describe now happens on every add, including one whose skills are already installed here.
    // The closing line is where "nothing has changed yet" belongs, and it is true of both.
    let mut s = format!("{}\n", data.source);
    if data.members.is_empty() {
        s.push_str("It holds no skills topos can see.\n");
    } else {
        s.push_str(&format!(
            "It holds {} skill{}: {}\n",
            data.members.len(),
            if data.members.len() == 1 { "" } else { "s" },
            data.members.join(", ")
        ));
    }
    s.push_str(&format!(
        "Would write to {}:\n  \"{}\" = \"{}\"\n",
        data.manifest, data.reference, data.value
    ));
    if let Some(note) = &data.note {
        s.push_str(&format!("note: {note}\n"));
    }
    s.push_str(&format!(
        "Nothing has changed yet — apply with:\n  {}",
        argv_line(yes_argv)
    ));
    s
}

/// The `fmt` receipt — one line: what moved, or that nothing had to.
pub(crate) fn fmt_tty(data: &topos_types::results::FmtData) -> String {
    if data.changed {
        format!("Formatted {}", data.manifest)
    } else {
        format!("{} already formatted", data.manifest)
    }
}

/// The breadth arming sweep's receipt lines — one per OTHER detected agent, honest per row (an
/// `Active` row names its live moment; a registered-but-ungated row names the consent still owed;
/// a degraded row names the explicit-pull floor). Empty input renders nothing.
pub(crate) fn breadth_trigger_lines(triggers: &[topos_types::TriggerReport]) -> String {
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
    s.push_str(&format!(
        "Nothing has changed yet — apply with:\n  {}",
        argv_line(yes_argv)
    ));
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

/// The per-row next-action a FINISHED merge surfaces. A `--keep-mine` resolution is one ordinary
/// commit on top of the team's version, so the only thing left to do with it is the ordinary
/// publish — and that is exactly the fact worth carrying, because the state it replaced (a blocked
/// bundle) was one an agent could not ship at all. The argv mirrors the command the receipt prints,
/// so a human and an agent are offered the same one.
pub(crate) fn resolution_next_actions(data: &PullData) -> Vec<NextAction> {
    data.skills
        .iter()
        .filter(|s| s.merge.as_ref().is_some_and(|m| m.resolved.is_some()))
        .map(|s| {
            crate::actions::next_action(
                ActionCode::from("RUN_COMMAND".to_owned()),
                vec!["topos".to_owned(), "publish".to_owned(), s.skill.clone()],
            )
        })
        .collect()
}

/// The per-row next-action a DRAFT surfaces: the ordinary publish. The TTY row already says it
/// ("your edits are not shared yet (topos publish <name>)") and the `--json` lane said nothing at
/// all — so an agent that had just been told, in the payload, that a bundle carries unshared edits
/// had no offered act to make on that fact. `publish` resolves the name across scopes itself, so
/// the argv takes no scope flag, exactly like the printed command.
pub(crate) fn draft_next_actions(data: &PullData) -> Vec<NextAction> {
    data.skills
        .iter()
        // A row whose merge just RESOLVED is already offered the same publish by
        // `resolution_next_actions`; offering it twice would put one act on the list twice.
        .filter(|s| s.draft && s.merge.as_ref().is_none_or(|m| m.resolved.is_none()))
        .map(|s| {
            crate::actions::next_action(
                ActionCode::from("RUN_COMMAND".to_owned()),
                vec!["topos".to_owned(), "publish".to_owned(), s.skill.clone()],
            )
        })
        .collect()
}

/// The two ways out of a STOPPED merge, per conflicted row — the same pair the TTY prints under
/// the row, as runnable argv. Without them the `--json` half of the one state that asks for a
/// decision carried no next action at all: an agent read a conflicted row, found nothing to do,
/// and left the bundle stuck. Spelled for the ROW's own scope, so the offered command drives the
/// copy that actually stopped rather than the other scope's.
pub(crate) fn conflict_next_actions(data: &PullData) -> Vec<NextAction> {
    let scope = PullReceiptScope::read(data.scope.as_deref());
    data.skills
        .iter()
        .filter(|s| s.action == topos_types::results::PullAction::Conflicted)
        .flat_map(|s| stopped_merge_actions(row_takes_g(s, &scope), &s.skill))
        .collect()
}

/// The two acts that END a stopped merge, as runnable argv — keep your version, or take the
/// team's. Written ONCE because every surface that meets a stopped merge owes the same pair: the
/// conflicted row above, and a `--reset` that took one copy and left the merge standing
/// ([`reset_next_actions`]). `global` is the SCOPE the stopped copy lives in, never the one the
/// invocation stood in.
fn stopped_merge_actions(global: bool, skill: &str) -> [NextAction; 2] {
    [
        crate::actions::next_action(
            ActionCode::ResolveDivergedDraft,
            update_at_scope(global, skill, &["--keep-mine"]),
        ),
        crate::actions::next_action(
            ActionCode::ResolveDivergedDraft,
            update_at_scope(global, skill, &["--reset"]),
        ),
    ]
}

/// What a `--reset` leaves an agent to do. A narrowed reset settles the ONE copy it took, so a
/// merge over the other copies is still stopped when it returns — the TTY says so in a sentence,
/// and without this the `--json` half of that state carried no action at all: an agent that just
/// reset a copy read `still_stopped` and had nothing to run. It is the same pair a conflicted row
/// offers, spelled for the scope the reset ran in. A merge this reset CONCLUDED (and a reset that
/// met no merge) leaves nothing to decide, and offers nothing.
pub(crate) fn reset_next_actions(items: &[topos_types::results::ResetData]) -> Vec<NextAction> {
    items
        .iter()
        .filter(|i| i.merge == Some(topos_types::results::ResetMergeOutcome::StillStopped))
        .flat_map(|i| stopped_merge_actions(i.global, &i.skill))
        .collect()
}

/// One typed message per bundle waiting on a decision: the bundle, then what is waiting. NO code —
/// a code names a fault to look up, and there is no fault here; what the reader (or the agent)
/// needs is the two commands, which ride [`decision_next_actions`] beside it.
pub(crate) fn decision_message(d: &crate::ops::PendingDecision) -> topos_types::Message {
    crate::message::decision(format!("{}: {}", d.name, d.line))
}

/// Every pending decision's ways out as structured next actions — the same argv the receipt prints
/// under the row, so the two surfaces can never drift into offering different commands.
pub(crate) fn decision_next_actions(decisions: &[crate::ops::PendingDecision]) -> Vec<NextAction> {
    decisions
        .iter()
        .flat_map(|d| d.ways_out.iter())
        .map(|argv| {
            crate::actions::next_action(ActionCode::from("RUN_COMMAND".to_owned()), argv.clone())
        })
        .collect()
}

/// A count with its correctly-pluralized noun (`1 skill`, `2 skills`).
fn counted(n: u64, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// The scope section header — it leads with the governing FILE, so "why is this here" starts at
/// the line that asked for it. The machine scope with no file says so honestly: no file, nothing
/// demanded machine-wide.
fn scope_header(scope: &str, manifest: Option<&str>) -> String {
    match (scope, manifest) {
        ("project", Some(f)) => format!("This folder — {f}"),
        ("project", None) => "This folder".to_owned(),
        (_, Some(f)) => format!("Machine-wide — {f}"),
        (_, None) => "Machine-wide — no global manifest; nothing demanded machine-wide".to_owned(),
    }
}

/// The INVENTORY's own section header — the listing names what the section holds and which file
/// governs it, once, so no row below has to repeat the file. `status` keeps its own heading
/// ([`scope_header`]): a health panel answers where you stand, not what is installed.
fn list_scope_header(scope: &str, manifest: Option<&str>) -> String {
    let what = if scope == "project" {
        "Project bundles"
    } else {
        "Machine-wide bundles"
    };
    match manifest {
        Some(f) => format!("{what} ({f}):"),
        None if scope == "project" => format!("{what}:"),
        None => format!("{what} (no ~/.topos/topos.toml):"),
    }
}

/// The `list` TTY — the inventory: the scope sections in full (or, under `--untracked`, one
/// summary line each), then whatever the optional views carry, then the quiet-rule summary lines
/// (each EXACTLY one line ending in the command that expands it), the paging markers, the
/// isolated warnings, and the footprint.
///
/// ONE list of bundles: every managed line rides a scope section, and each one names its own
/// origin in the `from` column — and, when that origin has stopped answering, says so itself
/// (`[not responding] … last answered: <when>`). An external repository is where a bundle comes
/// from, not a second kind of thing to enumerate, so nothing about it gets a trailing block here
/// in any state. How the sources are FARING across the machine is `status`'s panel.
pub(crate) fn list_tty(out: &ListOutcome) -> String {
    let data = &out.data;
    let mut s = String::new();

    // The one-skill deep dive is the whole answer — plus the tail every list shares (a focused
    // view still honors `--footprint`, and still says when a page dropped rows).
    if let Some(detail) = &data.detail {
        let mut s = list_detail_tty(detail);
        s.push('\n');
        push_list_tail(&mut s, out);
        return s.trim_end().to_owned();
    }
    // So is the agent-eye view.
    if let Some(view) = &data.agent_view {
        let mut s = agent_view_tty(view);
        s.push('\n');
        push_list_tail(&mut s, out);
        return s.trim_end().to_owned();
    }

    if out.untracked_view {
        // `--untracked`: each tracked scope is ONE summary line (never invisible, never dumped).
        let machine_alone = data.scopes.len() == 1;
        for scope in &data.scopes {
            let skills = scope.rows.len() as u64;
            let line = if scope.scope == "project" {
                format!(
                    "{} tracked in this folder — `topos list`",
                    counted(skills, "skill")
                )
            } else if machine_alone {
                format!("{} machine-wide — `topos list`", counted(skills, "skill"))
            } else {
                format!(
                    "{} machine-wide — `topos list -g`",
                    counted(skills, "skill")
                )
            };
            s.push_str(&line);
            s.push('\n');
        }
        if data.untracked.is_empty() {
            s.push_str("No untracked skills found in your agents' folders.\n");
        }
    } else {
        for scope in &data.scopes {
            s.push_str(&list_scope_header(&scope.scope, scope.manifest.as_deref()));
            s.push('\n');
            if scope.rows.is_empty() && scope.orphans.is_empty() {
                s.push_str("  (nothing installed in this scope)\n");
            }
            for entry in &scope.rows {
                s.push_str(&list_row(entry, &scope.scope));
            }
            for o in &scope.orphans {
                s.push_str(&orphan_row(o));
            }
        }
    }

    // The untracked LISTING (under `--untracked`), grouped by folder.
    if !data.untracked.is_empty() {
        s.push_str("Untracked skills — `topos add <name>` manages one:\n");
        let mut last_folder: Option<&str> = None;
        for u in &data.untracked {
            if last_folder != Some(u.folder.as_str()) {
                s.push_str(&untracked_folder_line(u));
                last_folder = Some(&u.folder);
            }
            s.push_str(&untracked_row(u));
        }
    }

    // The `--remote` catalog — per workspace: the channels (with the adopting file, when one
    // adopts), then the skills with this machine's adoption markers.
    for ws in &data.remote {
        s.push_str(&format!("{}/{}:\n", ws.host, ws.workspace));
        if !ws.channels.is_empty() {
            s.push_str("  channels:\n");
            for c in &ws.channels {
                let adopted = match &c.adopted_in {
                    Some(f) => format!("  (adopted in {f})"),
                    None => String::new(),
                };
                s.push_str(&format!(
                    "    {} — {}{adopted}\n",
                    c.name,
                    counted(c.skills, "skill")
                ));
            }
        }
        if !ws.skills.is_empty() {
            s.push_str("  skills:\n");
            for r in &ws.skills {
                s.push_str(&remote_row(r));
            }
        }
    }

    // The quiet-rule summaries: what this view did not show, ONE line each.
    if let Some(m) = &data.machine_summary {
        let updates = if m.updates_pending > 0 {
            format!(", {} pending", counted(m.updates_pending, "update"))
        } else {
            String::new()
        };
        s.push_str(&format!(
            "{} machine-wide{updates} — `{}`\n",
            counted(m.skills, "skill"),
            m.command
        ));
    }
    if let Some(u) = &data.untracked_summary {
        s.push_str(&format!(
            "{} in {} — `{}` shows them; `topos add <name>` manages one\n",
            counted(u.skills, "untracked skill"),
            counted(u.folders, "folder"),
            u.command
        ));
    }
    // The static pointer at what the workspaces offer — no network, no cached counts.
    if data.signed_in && data.remote.is_empty() && !out.untracked_view {
        s.push_str("See what your workspaces offer: `topos list --remote`\n");
    }

    push_list_tail(&mut s, out);
    s.trim_end().to_owned()
}

/// The tail EVERY `list` shape ends with, focused views included: the paging markers, the isolated
/// warnings, and the footprint. It is shared because a flag's answer must not depend on which view
/// it was asked with — `list <name> --footprint` and `list -a <slug> --footprint` carry the
/// footprint in `--json`, so they print it here too.
fn push_list_tail(s: &mut String, out: &ListOutcome) {
    let data = &out.data;
    // The external sources' block belongs to the ONE-SKILL dive alone: there it is narrowed to the
    // repository that delivers the bundle on screen, so its last check reads as one more fact
    // about that bundle. The LISTING has no block at all — a healthy source is named in each row's
    // `from` column, and a source that has stopped answering is said ON the rows it froze, which
    // is where a reader is already looking.
    if data.detail.is_some() {
        let block = forge_block(&data.forge);
        if !block.is_empty() {
            s.push_str(block.trim_start_matches('\n'));
            s.push('\n');
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
        s.push_str(&format!("warning: {}\n", w.text));
    }
    if let Some(footprint) = &data.footprint {
        // The count is the header; then each path, so a `--footprint` read reports WHAT topos owns
        // (not just how many) — the set `uninstall` deletes. The header states the COUNT and
        // nothing else: the rows are not all under the topos home (a trigger artifact lives in its
        // harness's own config dir), and the header used to say they were.
        s.push_str(&format!("Footprint: {} paths\n", footprint.len()));
        for p in footprint {
            s.push_str(&format!("  {p}\n"));
        }
    }
}

/// One `--remote` catalog row: `<name>  <name>@<short>  <kind>  <adoption note>` (+ any
/// open-proposal count). The kind is displayed verbatim (never branched on).
/// The ONE line a record nothing demands earns, when its entries or folders still stand.
///
/// It exists because the two surfaces that should have covered it both, correctly, do not: the
/// inventory is built from manifest rows (a record may describe a row, never create one) and the
/// sweep's orphan resolution passes over a record whose config entries still stand. Between them
/// an MCP server could sit live in an agent's config, named by nothing and fixable by nothing.
///
/// One line, never a section, and it says the two things a person needs: WHERE the thing still is,
/// and the command that ends it. `remove` here is the record delete — it takes no scope flag,
/// because there is no row in any file to scope to; that is the whole condition being reported.
fn orphan_row(o: &topos_types::results::OrphanRecord) -> String {
    let what = match o.standing.as_slice() {
        [one] => one.clone(),
        many => many.join(", "),
    };
    format!(
        "  {}  no longer in this file — still in {what} (`topos remove {}` ends it)\n",
        o.name, o.name
    )
}

fn remote_row(r: &RemoteSkill) -> String {
    use topos_types::results::RemoteAdoption;
    let note = match r.state {
        RemoteAdoption::AdoptedHere => "(adopted here)".to_owned(),
        RemoteAdoption::AdoptedOnMachine => "(adopted machine-wide)".to_owned(),
        RemoteAdoption::NotAdopted => format!("(not adopted — `topos add {}`)", r.name),
        RemoteAdoption::UpdateAvailable => {
            format!("(update available — `topos update {}`)", r.name)
        }
    };
    let proposals = if r.open_proposals > 0 {
        format!("  {} open proposal(s)", r.open_proposals)
    } else {
        String::new()
    };
    format!(
        "    {}  {}@{}  {}  {}{}\n",
        r.name,
        r.name,
        short(&r.version_id),
        r.kind,
        note,
        proposals
    )
}

/// The header of one untracked-discovery group: the folder, then the slugs of every installed agent
/// that reads it. The slugs sit on the FOLDER because that is what they describe — every skill in
/// the folder is read by the same agents — and they are machine tokens, not display names: they are
/// what `<name>@<slug>` and `-a <slug>` take. A folder with no installed reader carries no bracket.
fn untracked_folder_line(u: &UntrackedEntry) -> String {
    if u.readers.is_empty() {
        format!("  {}:\n", u.folder)
    } else {
        format!("  {}  [{}]:\n", u.folder, u.readers.join(", "))
    }
}

/// One untracked-discovery row: the skill's name — and, for a LINK SHELL, the folder its bytes
/// really live in, because that is the folder an `add` of this row takes. Where it lives and who
/// reads it are the group's folder line, so a row says the one thing that varies between rows.
fn untracked_row(u: &UntrackedEntry) -> String {
    match &u.original {
        Some(original) => format!("    {}  → {}\n", u.name, tty_path(original)),
        None => format!("    {}\n", u.name),
    }
}

/// One inventory row: `<skill>  <skill>@<short>` + the draft flag + the STATUS / SOURCE / CAUSE
/// columns. A row never applied here carries the all-zero identity — rendered as the honest
/// not-applied note instead of a meaningless `@000000000000`.
///
/// `scope` is the section the row rides, and every command the row suggests is SCOPE-EXACT: a
/// machine row spells `topos update -g`, because a bare `update` read from inside a project drives
/// the PROJECT and would never touch the row the reader is looking at. The `-g` spelling is
/// correct outside a project too (it is what the bare form resolves to there), so the row needs no
/// knowledge of where the reader stands — only of which section it is in.
fn list_row(entry: &SkillEntry, scope: &str) -> String {
    let flag = if scope == "machine" { " -g" } else { "" };
    let columns = list_columns(entry);
    if entry.version_id.bytes().all(|b| b == b'0') {
        let mut note = String::new();
        // A row whose FOLDER IS GONE is never going to apply, so it is never offered the update.
        // The one command that changes anything here is the one that drops the line — spelled for
        // the file that holds the row, and complete, so running it finishes the job.
        if entry.source_missing {
            note.push_str(&format!(
                "  (its folder is gone — run 'topos remove{flag} {} --yes' to drop the line)",
                entry.source.as_deref().unwrap_or(&entry.skill)
            ));
        } else if entry.status.is_none() {
            note.push_str(&format!(
                "  (not applied here yet — `topos update{flag}` applies it)"
            ));
        }
        note.push_str(&columns);
        return format!("  {}{note}\n", entry.skill);
    }
    format!(
        "  {}  {}@{}{}{}\n{}",
        entry.skill,
        entry.skill,
        short(&entry.version_id),
        if entry.draft { "  (draft)" } else { "" },
        columns,
        draft_sub_lines(entry, flag),
    )
}

/// What a DRAFT row says beneath itself. `(draft)` alone states that edits exist and leaves every
/// question a person actually has — where are they, and what can I do about them — unanswered, so
/// the row names the folder and then the three exits: share it, read it, drop it.
///
/// Copies that DISAGREE collapse to one line instead: no single folder is the draft, and a row that
/// guessed one would send the reader to publish bytes they did not mean. The deep dive lists them.
///
/// A BLOCKED row takes over the whole block: its edits are not shareable (`publish` is refused
/// while the merge stands), so offering `publish` would print a command that cannot run, and the
/// only two things that move this row are the merge's own exits. Terse on purpose — this is an
/// inventory row, and the update receipt is where the folders and the marked-up copy are named.
///
/// Empty for every row that cannot name a folder — a clean row, and the rare edited one whose scan
/// classified nothing. The commands are scope-exact through `flag`, exactly as the row's own are:
/// `update` drives a scope, so a machine row spells `-g`. `publish` and `diff` take the bare name
/// here because a DRAFTED copy is what each of them resolves to on its own, and this row's whole
/// subject is that draft. The flag is written where a person writes it — right after the verb,
/// ahead of the bundle name — which is the one order every offered command uses (both orders
/// parse; only one is printed).
fn draft_sub_lines(entry: &SkillEntry, flag: &str) -> String {
    let name = &entry.skill;
    if entry.status == Some(topos_types::results::SkillStatus::Blocked) {
        return blocked_sub_lines(name, flag);
    }
    if let Some(dir) = &entry.draft_dir {
        return format!(
            "      draft in {dir} (not shared)\n      to share:     topos publish {name}\n      \
             to view diff: topos diff {name}\n      to drop:      topos update{flag} {name} \
             --reset\n"
        );
    }
    match entry.draft_diverged {
        // The SAME words every other surface uses for this state (the deep dive, the freeze menu,
        // the escape's receipt): copies whose edits disagree are "different edits in N folders".
        // A row that said it a second way would read as a second state.
        Some(n) => format!(
            "      different edits in {} (see: topos list{flag} {name})\n",
            counted(u64::from(n), "folder")
        ),
        None => String::new(),
    }
}

/// The two exits a BLOCKED row prints beneath itself — the SAME pair the update receipt named,
/// spelled for the section the row rides so each is runnable exactly as printed. Shared with the
/// deep dive ([`list_detail_tty`]), so one bundle never reads two ways.
fn blocked_sub_lines(name: &str, flag: &str) -> String {
    format!(
        "      the team's version needs merging — you cannot publish until you pick one\n      to \
         keep yours:  topos update{flag} {name} --keep-mine\n      to take theirs: topos \
         update{flag} {name} --reset\n"
    )
}

/// The STATUS / SOURCE suffix for an inventory row (`  [behind]  from topos.sh/acme`) — rendered
/// only for the fields the resolution populated. The source is the row's ORIGIN (the workspace
/// address, the repository, the local folder), never the manifest file: the section header names
/// that file once, and a column that repeats it per row says nothing the reader did not have.
fn list_columns(entry: &SkillEntry) -> String {
    use topos_types::results::SkillStatus;
    let mut s = String::new();
    // The kind badge: a config-placed bundle reads differently from a skill (its "installed"
    // lives in agent config files, not skill dirs).
    if let Some(kind) = &entry.kind {
        s.push_str(&format!("  [{kind}]"));
    }
    // A source that has stopped answering REPLACES the update status: `current` would be measured
    // against an answer nobody got. The copy on disk is fine — nothing is keeping it current.
    if entry.source_health.is_some() {
        s.push_str("  [not responding]");
    } else if let Some(status) = entry.status
        && !matches!(status, SkillStatus::Draft)
    {
        // `draft` already shows via its own flag; skip it here to avoid a doubled note. `blocked`
        // has no flag of its own and must not read as an ordinary row at a glance, so it takes the
        // status column in the receipt's own word for it.
        let label = match status {
            SkillStatus::Current => "current",
            SkillStatus::Behind => "behind",
            SkillStatus::Off => "off",
            SkillStatus::Draft => "draft",
            SkillStatus::Blocked => "waiting on you",
        };
        s.push_str(&format!("  [{label}]"));
    }
    if let Some(source) = &entry.source {
        s.push_str(&format!("  from {source}"));
    }
    // How stale the row is, next to the badge that says it stopped moving.
    if let Some(h) = &entry.source_health {
        match h.answered_at {
            Some(at) => s.push_str(&format!(
                "  last answered: {}",
                fmt_utc_millis(u64::try_from(at).unwrap_or(0))
            )),
            None => s.push_str("  never answered"),
        }
    }
    s
}

/// The agent-eye view (`list -a <slug>`): each skills dir the harness reads from this folder,
/// every entry marked managed (`topos: <file>:<row-key>` / `feed <host>/<ws>` / `topos:
/// built-in`) or untracked.
fn agent_view_tty(view: &AgentView) -> String {
    let mut s = format!("{} — what it reads from this folder:", view.agent_name);
    for dir in &view.dirs {
        s.push_str(&format!("\n  {} ({}):", dir.path, dir.scope));
        if dir.entries.is_empty() {
            s.push_str("\n    (empty)");
        }
        for e in &dir.entries {
            match &e.managed {
                Some(m) if m.starts_with("feed ") => {
                    s.push_str(&format!("\n    {}  {m}", e.name));
                }
                Some(m) => s.push_str(&format!("\n    {}  topos: {m}", e.name)),
                None => s.push_str(&format!(
                    "\n    {}  untracked (`topos add {}` manages it)",
                    e.name, e.name
                )),
            }
        }
    }
    s
}

/// The `update --reset` DESCRIBE's TTY — LOSS-led: it shows exactly the draft delta being
/// discarded, and says exactly WHOSE edits those are. A `--dest`-narrowed reset takes ONE copy;
/// the sentence around the diff names that folder and states that the other copies keep their
/// edits, because a consent surface that overstates the loss is asking for the wrong yes.
///
/// The sentence states the loss plainly and stops — the delta printed under it is the argument,
/// and a shouted word above a diff a person is already reading only makes the surface sound
/// anxious. The receipt below reads in the same voice.
pub(crate) fn reset_describe_tty(
    items: &[topos_types::results::ResetData],
    yes_argv: &[String],
) -> String {
    let mut s = String::new();
    for item in items {
        match &item.dest {
            Some(dest) => s.push_str(&format!(
                "Reset '{}' in {dest} — discard local edits to {}:\n",
                item.skill,
                short(&item.to_version)
            )),
            None => s.push_str(&format!(
                "Reset '{}' — discard local edits to {}:\n",
                item.skill,
                short(&item.to_version)
            )),
        }
        if item.drop_diff.trim().is_empty() {
            s.push_str("  (no local edits — nothing to discard)\n");
        } else {
            for line in item.drop_diff.trim_end_matches('\n').lines() {
                s.push_str(&format!("  {line}\n"));
            }
        }
        if let Some(line) = others_kept_line(&item.others_kept) {
            s.push_str(&format!("  {line}\n"));
        }
    }
    s.push_str(&format!(
        "Nothing has changed yet — apply with:\n  {}",
        argv_line(yes_argv)
    ));
    s
}

/// The `update --reset` APPLY's TTY — the same naming discipline as the describe: a per-copy reset
/// reports the copy it took, never the bundle.
pub(crate) fn reset_applied_tty(items: &[topos_types::results::ResetData]) -> String {
    let mut s = String::new();
    for item in items {
        match &item.dest {
            Some(dest) => s.push_str(&format!(
                "Reset '{}' in {dest} to {} — that copy's local edits discarded.\n",
                item.skill,
                short(&item.to_version)
            )),
            None => s.push_str(&format!(
                "Reset '{}' to {} — local edits discarded.\n",
                item.skill,
                short(&item.to_version)
            )),
        }
        if let Some(line) = others_kept_line(&item.others_kept) {
            s.push_str(&format!("{line}\n"));
        }
        // A by-hand merge this reset did not read — and therefore did not delete. It lives nowhere
        // else, so the folder is named.
        if let Some(folder) = &item.hand_merge {
            s.push_str(&format!("your hand merge is still in {folder}\n"));
        }
        // WHERE that leaves the merge, when one had stopped here. A narrowed reset settles the
        // copy it took and nothing else, so the block usually stands — and the reader who just
        // watched one folder go back to the team's version is exactly the one who would assume
        // otherwise. The pointer is scope-exact, like every other command this CLI offers.
        let g = if item.global { " -g" } else { "" };
        match item.merge {
            Some(topos_types::results::ResetMergeOutcome::StillStopped) => s.push_str(&format!(
                "  the merge on '{}' is still stopped (see: topos list{g} {})\n",
                item.skill, item.skill
            )),
            Some(topos_types::results::ResetMergeOutcome::Concluded) => s.push_str(
                "  that was the last copy holding the merge — the team's version stands \
                 everywhere\n",
            ),
            None => {}
        }
    }
    s.trim_end().to_owned()
}

/// What a per-copy reset LEAVES: the other edited copies, named, still holding their edits. `None`
/// when there are none — a whole-bundle reset takes every copy, and there is nothing to keep.
fn others_kept_line(others: &[String]) -> Option<String> {
    match others {
        [] => None,
        [one] => Some(format!("your other copy in {one} keeps its edits")),
        many => Some(format!(
            "your other copies in {} keep their edits",
            many.join(", ")
        )),
    }
}

pub(crate) fn diff_tty(data: &DiffData) -> String {
    // WHICH copy was read, when the bundle sits in more than one folder — the producer populates
    // the pair on exactly that condition, so a single-copy diff is byte-identical to what it always
    // printed. The left side is named too: it is the applied base whether or not `--dest` narrowed
    // the right, which is what makes two `--dest` runs comparable.
    //
    // Built BEFORE the no-changes answer, and printed with it: "nothing differs" is a claim about
    // ONE copy, and a bundle sitting in several folders that answered it without saying which copy
    // it read would leave the reader no way to tell which of them is clean.
    let header = match (&data.skill, &data.dest) {
        (Some(skill), Some(dest)) => {
            format!("{skill} — {dest} vs applied {}\n", short(&data.version_id))
        }
        _ => String::new(),
    };
    if data.diff.is_empty() && !data.truncated {
        return format!("{header}No changes — the draft matches current.");
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
    format!("{header}{s}")
}

pub(crate) fn log_tty(data: &LogData) -> String {
    let mut out = String::new();
    // Lead with the archived-successor hint when the skill was resolved by its freed base name.
    if let Some(hint) = &data.archived_successor {
        out.push_str(&format!("note: {hint}\n"));
    }
    // The workspace this copy comes from did not land its last exchange with this machine: say so
    // ONCE, with the true cause (the shared clause — a failed exchange must never read as a dead
    // network), so history is not taken for freshly synced.
    if let Some(f) = &data.sync_fault {
        out.push_str(&format!(
            "note: {}'s last exchange with this machine did not succeed — {}; retry with `topos \
             update`\n",
            f.workspace,
            crate::ops::StaleReason::from(f.kind).clause()
        ));
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
            "A newer topos is available: {} → {}.\nRun `topos self-update` to install it.",
            o.current_version,
            o.latest_version.as_deref().unwrap_or("?")
        ),
        Checked | AlreadyCurrent => format!("topos is up to date ({}).", o.current_version),
        Upgraded => format!(
            "Updated topos {} → {}.",
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
/// (an immediate self-scoped apply always discloses its way back). `about` is the one thing the
/// inverse argv cannot say for itself — what leaves this machine when it runs.
pub(crate) fn undo_next_actions(
    undo: &[String],
    about: crate::actions::Subject,
) -> Vec<NextAction> {
    if undo.is_empty() {
        return Vec::new();
    }
    vec![crate::actions::next_action_about(
        ActionCode::from("UNDO".to_owned()),
        undo.to_vec(),
        about,
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
    // `~` rides bare: a `~/`-spelled folder is the one portable way to write a path under this
    // machine's home, and quoting it costs the reader the expansion while buying nothing — the
    // shell hands topos the absolute path, and topos reads the literal `~/` form too.
    let safe = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-' | '~'
            )
    };
    if !arg.is_empty() && arg.chars().all(safe) {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// The `uninstall` DESCRIBE's TTY — everything `--yes` would remove (nothing has changed yet).
pub(crate) fn uninstall_describe_tty(
    d: &crate::ops::UninstallDescribe,
    yes_argv: &[String],
) -> String {
    let mut s = String::from("Uninstalling topos would:");
    if d.trigger_artifacts.is_empty() {
        s.push_str("\n  · scrub the session-start auto-update hook: none is armed");
    } else {
        s.push_str(&format!(
            "\n  · scrub the session-start auto-update hook from: {}",
            d.trigger_artifacts.join(", ")
        ));
    }
    if d.sidecar_present {
        s.push_str(&format!(
            "\n  · delete topos's own files at {} (the signed-in credential lives there and goes with it)",
            d.sidecar_path
        ));
    } else {
        s.push_str(&format!(
            "\n  · nothing to delete at {} (topos keeps no files here)",
            d.sidecar_path
        ));
    }
    if !d.builtin_dirs.is_empty() {
        s.push_str(&format!(
            "\n  · remove the built-in `topos` skill's copies (topos-authored): {}",
            d.builtin_dirs.join(", ")
        ));
    }
    // The MCP config edits, one row per FILE. These are live wiring — an entry topos placed points
    // an agent at a server — and the ledger that proves which entries are topos's dies with the
    // sidecar, so the preview says exactly which files it opens and which entries it leaves.
    for path in &d.mcp_files {
        s.push_str(&format!(
            "\n  · remove topos-placed MCP server entries from {path}"
        ));
    }
    for path in &d.mcp_drifted {
        s.push_str(&format!(
            "\n  · leave the hand-edited entries in {path} in place"
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
///
/// `complete` is the ONE thing the headline reads: a run that left an agent's auto-update trigger
/// armed — or that died on a filesystem error partway — did not uninstall topos, whatever else it
/// managed, and the first line must not say it did. `failures` are the things it could not remove,
/// each stating its own way out under the rows. The ROWS are identical on both paths: they report
/// the work that landed, which is true however the run ended.
pub(crate) fn uninstall_applied_tty(
    d: &crate::ops::UninstallApplied,
    failures: &[topos_types::Message],
    complete: bool,
) -> String {
    let mut s = String::from(if complete {
        "Uninstalled topos."
    } else {
        "Uninstall incomplete — topos could not remove everything it placed."
    });
    if !d.builtin_dirs.is_empty() {
        s.push_str(&format!(
            "\n  · removed the built-in `topos` skill's copies: {}",
            d.builtin_dirs.join(", ")
        ));
    }
    // The MCP config edits, keyed by the FILE the entries lived in — receipts speak in
    // destinations. A hand-edited entry SURVIVING is the fact a person must not miss, so it is
    // stated in its own row rather than folded into the removal's.
    for path in &d.mcp_files {
        s.push_str(&format!(
            "\n  · removed topos-placed MCP server entries from {path}"
        ));
    }
    for path in &d.mcp_drifted {
        s.push_str(&format!(
            "\n  · left the hand-edited entries in {path} in place"
        ));
    }
    // Absent = the teardown died before the scrubs ran, and the trigger is still armed. Saying
    // nothing is the honest answer; the error above it is what happened.
    if let Some(hook) = &d.hook {
        let hook_line = match (hook.state, hook.touched_path.as_deref()) {
            (TriggerState::Degraded, _) => {
                "\n  · couldn't edit the harness config — remove the topos auto-update hook \
                 manually"
                    .to_owned()
            }
            (TriggerState::AlreadyPresentUnmanaged, _) => {
                "\n  · left your hand-rolled auto-update hook untouched".to_owned()
            }
            (_, Some(path)) => format!("\n  · scrubbed the auto-update hook from {path}"),
            (_, None) => "\n  · no auto-update hook was installed — nothing to scrub".to_owned(),
        };
        s.push_str(&hook_line);
    }
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
        s.push_str("\n  · deleted topos's own files at ~/.topos (credential included)");
    } else {
        s.push_str("\n  · topos kept no files here to delete");
    }
    // Each failure's own sentence carries the way out — the file to fix, or the harness command to
    // run. The code stays on the typed channel.
    for f in failures {
        s.push_str(&format!("\n{}", f.text));
    }
    if let Some(bin) = &d.binary_path {
        s.push_str(&format!(
            "\nThe `topos` binary at {bin} was left in place — remove it with your installer (or `rm {bin}`)."
        ));
    }
    s
}

/// The `status` panel's TTY — HEALTH, no inventory: version, sign-in, the sessions, then per
/// shown SCOPE its governing file, the regimes, the notes verbatim, and the attention counts —
/// each ONE line ending in the exact command that resolves it — then the machine summary (when
/// the machine body is not shown) and the per-agent trigger rows. Entirely from the offline
/// snapshot.
pub(crate) fn status_tty(d: &topos_types::results::StatusData) -> String {
    let mut s = format!("topos {}", d.version);
    match &d.server {
        Some(server) => {
            s.push_str(&format!(
                "\nserver: {server} — {}",
                if d.signed_in {
                    "signed in"
                } else {
                    "signed out (`topos login <workspace-address>` signs back in)"
                }
            ));
        }
        None => s.push_str(
            "\nnot connected — join your team with `topos login <workspace-address>` (ask a \
             teammate for the address), or create a workspace at https://topos.sh",
        ),
    }
    // The SESSIONS this installation holds (the session model — one per workspace).
    if !d.sessions.is_empty() {
        s.push_str("\nsessions:");
        for sess in &d.sessions {
            s.push_str(&format!(
                "\n  {}/{} ({})",
                sess.host, sess.name, sess.display_name
            ));
            match sess.session_status.as_deref() {
                Some("pending") => s.push_str(" — awaiting owner approval"),
                Some("ended") => s.push_str(&format!(
                    " — ended; `topos login {}/{}` starts a fresh one",
                    sess.host, sess.name
                )),
                _ => {}
            }
        }
    }
    // The scope bodies: the governing file leads, then the regimes (machine scope), the notes
    // verbatim, and the attention counts — nothing pending is said out loud.
    for scope in &d.scopes {
        s.push_str(&format!(
            "\n{}",
            scope_header(&scope.scope, scope.manifest.as_deref())
        ));
        for r in &scope.regimes {
            s.push_str(&format!("\n  {}/{} — {}", r.host, r.workspace, r.regime));
        }
        for note in &scope.notes {
            s.push_str(&format!("\n  {note}"));
        }
        if scope.attention.is_empty() {
            s.push_str("\n  nothing pending");
        }
        for a in &scope.attention {
            s.push_str(&format!("\n  {} — `{}`", attention_phrase(a), a.command));
        }
    }
    // The machine scope not shown in full: ONE summary line with ITS counts.
    if let Some(m) = &d.machine_summary {
        let counts = if m.attention.is_empty() {
            "nothing pending".to_owned()
        } else {
            m.attention
                .iter()
                .map(attention_phrase)
                .collect::<Vec<_>>()
                .join(", ")
        };
        s.push_str(&format!("\nmachine-wide: {counts} — `{}`", m.command));
    }
    s.push_str(&forge_block(&d.forge));
    if !d.triggers.is_empty() {
        s.push_str("\nauto-update triggers:");
        for t in &d.triggers {
            // The word the EVIDENCE supports. `status` never installs anything, so it holds no
            // `Active` report to call a trigger armed: all it has is the artifact's footprint,
            // which proves the trigger is REGISTERED — the same word the install receipt uses for
            // exactly that evidence level (see [`breadth_trigger_lines`]). `armed` is reserved
            // there, for a report that stated a live update moment.
            let state = match t.armed {
                Some(true) => "registered",
                Some(false) => "not registered",
                None => "not checked",
            };
            s.push_str(&format!("\n  {}: {state}", t.agent));
            if let Some(n) = &t.note {
                s.push_str(&format!(" — {n}"));
            }
        }
    }
    s
}

/// The external sources' last auto-update check — the plain answer to "is this even working?".
/// Empty when nothing external is tracked here. `status` shows it over every source the shown
/// scopes name; `list` shows it in full only on the one-skill deep dive (narrowed to that skill's
/// own repository), and elsewhere only for the sources in trouble — the listing names each row's
/// origin in its `from` column, so a healthy source needs no line of its own there.
///
/// A failure is stated where a person can act on it, never counted as an emergency: the copies on
/// disk keep working through every one of these, which is why the silent sweep stays quiet about
/// them until they have been failing for a long time.
fn forge_block(sources: &[topos_types::results::ForgeSource]) -> String {
    if sources.is_empty() {
        return String::new();
    }
    let mut s = String::from("\nexternal sources:");
    for f in sources {
        s.push_str(&format!("\n  {}", f.source));
        match (&f.error, &f.commit) {
            (Some(e), _) if f.gone => {
                s.push_str(&format!(" — {e}; not retried until the row changes"));
            }
            (Some(e), _) => s.push_str(&format!(" — last check failed: {e}")),
            (None, Some(c)) => s.push_str(&format!(" — {}", short_hex_ref(c))),
            (None, None) => {}
        }
        s.push_str(&format!(
            " (checked {})",
            fmt_utc_millis(u64::try_from(f.checked_at).unwrap_or(0))
        ));
    }
    s
}

/// A commit as a receipt spells it — short enough to read, long enough to recognize.
fn short_hex_ref(c: &str) -> &str {
    &c[..c.len().min(12)]
}

/// One attention count, phrased (`2 updates pending` / `1 assignment not applied` / `1 draft
/// ahead` / `1 merge waiting on you`) — the command is appended by the caller (body line vs
/// summary).
fn attention_phrase(a: &topos_types::results::AttentionCount) -> String {
    match a.kind.as_str() {
        "updates-pending" => format!("{} pending", counted(a.count, "update")),
        "assignments-not-applied" => format!("{} not applied", counted(a.count, "assignment")),
        "drafts-ahead" => format!("{} ahead", counted(a.count, "draft")),
        "waiting-on-you" => format!("{} waiting on you", counted(a.count, "merge")),
        kind => format!("{} {kind}", a.count),
    }
}

/// `topos list <name>` — where this one skill comes from, spelled out: the row (file + key) or
/// the feed (+ who aimed it), the version and any pin, where its bytes are, and its state. Every
/// suggested command is SCOPE-EXACT (the same rule as [`list_row`]): an answer from the machine
/// scope spells `topos update -g`, a project answer the bare form. A name NOTHING manages is the
/// headline plus any unmanaged folders found on disk — and nothing else: no store mentions, no
/// workspace hints.
fn list_detail_tty(detail: &topos_types::results::ListDetail) -> String {
    use topos_types::results::StatusItemState;
    if !detail.managed {
        let mut s = format!("{} — not managed on this machine", detail.name);
        // EVERY folder holding a copy, never a sample: the question this answer exists for is
        // "where is it?", and a capped list would leave a copy the person never learns about.
        for folder in &detail.folders {
            s.push_str(&format!("\n  {folder}"));
        }
        // Copies exist, so there is something to bring under management — the one command that
        // does it, with the destination left as the choice only the person can make. Nothing
        // found means nothing to adopt: the bare headline stays bare.
        if !detail.folders.is_empty() {
            s.push_str(&format!(
                "\nto manage: topos add -g {} --dest <dest>",
                detail.name
            ));
        }
        return s;
    }
    let flag = if detail.scope.as_deref() == Some("machine") {
        " -g"
    } else {
        ""
    };
    // THE CHECKOUT, on the name line: each place you work holds its own copy of a bundle, and
    // which one this whole answer is about is the first thing a reader needs — every folder,
    // version, and command below belongs to that one copy. The scope that ANSWERED says it, so a
    // bare run inside a checkout and the same run with `-g` never read alike.
    let mut s = match detail.scope.as_deref() {
        Some("project") => format!("{} — in this project", detail.name),
        Some("machine") => format!("{} — on this machine", detail.name),
        _ => detail.name.clone(),
    };
    match (&detail.source_file, &detail.source_key, &detail.feed) {
        (Some(file), Some(key), _) => s.push_str(&format!("\n  from {file}, line key {key}")),
        (Some(file), None, _) => s.push_str(&format!("\n  from {file}")),
        (None, _, Some(feed)) => s.push_str(&format!("\n  from the {feed} feed")),
        (None, _, None) => s.push_str("\n  from this machine"),
    }
    if let Some(a) = &detail.attribution {
        s.push_str(&format!(" — {a}"));
    }
    match (&detail.version, &detail.pin) {
        (Some(v), Some(p)) => {
            s.push_str(&format!("\n  version {} (pinned {})", short(v), short(p)))
        }
        (Some(v), None) => s.push_str(&format!("\n  version {}", short(v))),
        (None, Some(p)) => s.push_str(&format!("\n  pinned {} (not applied here)", short(p))),
        (None, None) => {}
    }
    // The DRAFT beside the version it is ahead of — the other half of a checkout, and the one
    // fact a version line alone cannot carry. `not shared yet` is what a draft IS here; the read
    // that shows it is spelled for this answer's own scope, like every other command on the page.
    if detail.drafted {
        s.push_str(&format!(
            "\n  your draft: edited, not shared yet (topos diff{flag} {})",
            detail.name
        ));
    }
    // ONE folder per line. A comma-joined run of absolute paths wraps into a wall the eye cannot
    // find a path's start in, and the count of copies — the thing this line exists to report — is
    // unreadable at a glance. Same shape whatever the count, so a second copy never changes how
    // the answer is read.
    if !detail.placements.is_empty() {
        s.push_str("\n  placed in:");
        for p in &detail.placements {
            s.push_str(&format!("\n    {p}"));
        }
    }
    // An mcp bundle's "where": per-agent config entries (placed file + state), instead of
    // placement dirs.
    if BundleKind::of_tag(detail.kind.as_deref()) == Some(BundleKind::Mcp) {
        if let Some(why) = &detail.mcp_unreachable {
            // "not recorded yet" would be a lie of omission here: nothing is coming. The row's own
            // dest is what withholds it, so the line names the cause and the files that would work.
            s.push_str(&format!(
                "\n  an MCP server bundle that reaches no agent — {why}"
            ));
        } else if detail.harnesses.is_empty() {
            s.push_str("\n  an MCP server bundle — no agent config entries recorded yet");
        } else {
            // "as of the last converge": these entries are what the ledger / delivery cache
            // recorded when topos last wrote — a hand edit since then surfaces only when the
            // next converge looks, so the heading claims exactly that much.
            s.push_str("\n  an MCP server bundle, configured in (as of the last converge):");
            for h in &detail.harnesses {
                let file = h.file.as_deref().unwrap_or("(no file recorded)");
                match (h.state, h.note.as_deref()) {
                    // Created/refreshed is what the LAST converge DID, not a standing property of
                    // the entry — this heading speaks about where entries live, so every healthy
                    // outcome reads the same way here.
                    (s2, _) if s2.wrote() || s2 == topos_types::results::TargetOutcome::Current => {
                        s.push_str(&format!("\n    {}: {file}", h.agent));
                    }
                    (state, Some(note)) => s.push_str(&format!(
                        "\n    {}: {file} — {} ({note})",
                        h.agent,
                        state.word()
                    )),
                    (state, None) => {
                        s.push_str(&format!("\n    {}: {file} — {}", h.agent, state.word()));
                    }
                }
            }
        }
    }
    let state = match detail.state {
        StatusItemState::Applied => "applied".to_owned(),
        StatusItemState::Behind => {
            format!("behind — `topos update{flag}` lands the newer version")
        }
        // Copies that DISAGREE take over the state line the way a block does. `local edits ahead
        // of the applied version` is true of each of them and answers neither question the row
        // sent this reader here with — which folders disagree, and what can be done about them —
        // so the copies are named, each with the three acts that apply to ONE of them, in the
        // same vocabulary the placement freeze's own refusal prints.
        StatusItemState::LocalEdits if !detail.diverged.is_empty() => {
            diverged_copies_block(&detail.name, flag, &detail.diverged)
        }
        StatusItemState::LocalEdits => "local edits ahead of the applied version".to_owned(),
        // The SAME two lines the inventory row prints, indented into the dive's own body: one
        // bundle, one answer, whichever command asked — plus the folder the merge stopped in,
        // which only this surface can still name once the update's receipt has scrolled away.
        StatusItemState::Blocked => blocked_detail_block(detail, flag),
        StatusItemState::Excluded => "excluded here".to_owned(),
        StatusItemState::Off => "off — withheld here by your global manifest".to_owned(),
        StatusItemState::NotAvailable => "not available with your current access".to_owned(),
        StatusItemState::PendingSession => "awaiting session approval".to_owned(),
        StatusItemState::NoDeliveryYet => {
            format!("no delivery yet (`topos update{flag}` performs the first exchange)")
        }
        StatusItemState::Unknown => {
            format!("not applied here yet (`topos update{flag}` applies it)")
        }
    };
    s.push_str(&format!("\n  {state}"));
    // The OTHER scope's checkout, LAST — everything above is about the copy that answered, and
    // this is the one line that says there is a second one. A separate copy, with its own draft
    // state and its own history; the command that opens it is the same `list` in the other scope.
    if let Some(twin) = &detail.twin {
        let (where_, twin_flag) = if twin.machine {
            ("also on this machine", " -g")
        } else {
            ("also in this project", "")
        };
        let edits = if twin.drafted { "edited" } else { "no edits" };
        s.push_str(&format!(
            "\n  {where_}: a separate copy, {edits} (topos list{twin_flag} {})",
            detail.name
        ));
    }
    s
}

/// What the deep dive says about a BLOCKED bundle: the block, the folder the stopped merge is
/// waiting in when the record names one, then the merge's two exits.
///
/// The folder lines are the update receipt's own sentences, word for word (the conflict row in
/// [`pull_action_row`]) — one state, one vocabulary, whichever surface a person meets it on — and
/// they are discriminated on the RECORDED reason exactly as the receipt discriminates, so a block
/// re-read here says what its first disclosure said. This is also the ONLY surface that can still
/// answer "which folder?" after that receipt has scrolled off, or when a silent sweep raised the
/// block and printed no receipt at all.
///
/// A detail that names no folder prints no folder line: the record is the one thing that knows
/// which folder is this bundle's, and an answer never claims a place it cannot prove.
fn blocked_detail_block(detail: &topos_types::results::ListDetail, flag: &str) -> String {
    let name = &detail.name;
    let mut out =
        "the team's version needs merging — you cannot publish until you pick one".to_owned();
    if let Some(dir) = &detail.conflict_copy {
        if detail.conflict_reason == Some(topos_types::persisted::ConflictReason::NoBase) {
            out.push_str(
                "\n  to merge by hand, your files are here with the team's beside them \
                 (.topos-theirs):",
            );
        } else {
            out.push_str("\n  to merge by hand, both versions are marked up here:");
        }
        out.push_str(&format!("\n    {dir}/"));
    }
    out.push_str(&format!(
        "\n  to keep yours:  topos update{flag} {name} --keep-mine\n  to take theirs: topos \
         update{flag} {name} --reset"
    ));
    out
}

/// What the deep dive says about copies whose edits DISAGREE: the count, then one block per copy —
/// share it, read it, drop it — then the whole-bundle discard, last.
///
/// The state's own words are the placement freeze's ("different edits in N folders"), so one
/// bundle never reads as two states; the dive spells the per-copy acts out because it is an
/// inventory a person is browsing rather than a refusal blocking a command they just ran. The
/// lead-in differs too — the dive has already printed the bundle's name as its headline, so the
/// sentence does not repeat it.
fn diverged_copies_block(
    name: &str,
    flag: &str,
    copies: &[topos_types::results::DivergedCopy],
) -> String {
    let mut out = format!(
        "different edits in {} — name the one to work with:",
        counted(copies.len() as u64, "folder")
    );
    for c in copies {
        let dest = &c.dest;
        out.push_str(&format!(
            "\n    {}\n      to share:     topos publish {name} --dest {dest}\n      to view \
             diff: topos diff {name} --dest {dest}\n      to drop:      topos update{flag} {name} \
             --dest {dest} --reset",
            c.display
        ));
    }
    out.push_str(&format!(
        "\n  to drop every copy's edits: topos update{flag} {name} --reset"
    ));
    out
}

/// The bare `topos` welcome on a fresh (unenrolled) machine — what topos is, in two lines, plus
/// the two ways in. The enrolled bare invocation renders [`status_tty`] instead.
pub(crate) fn welcome_tty(d: &topos_types::results::StatusData) -> String {
    format!(
        "topos {} — shared skills for AI agents: your team's behaviors, delivered to every \
         agent and kept current.\nJoin your team:      topos login <workspace-address>   (ask a \
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
    // Signed out — whether never connected or with every session ended, the way (back) in is
    // the same login flow; `topos auth login` does not exist.
    let _ = &d.workspaces;
    vec![crate::actions::next_action(
        ActionCode::from("LOGIN_WORKSPACE".to_owned()),
        vec![
            "topos".into(),
            "login".into(),
            "<workspace-address>".into(),
            "--json".into(),
        ],
    )]
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
                "\nnot connected — join with `topos login <workspace-address>` (ask a teammate \
                 for the address)",
            );
        } else {
            s.push_str("\nsign back in with `topos login <workspace-address>`");
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

/// Which of a bundle's several rows speaks for it in the SUMMARY. Higher wins.
///
/// The ordering is "what would a person say happened to this bundle?", and only one pair of it
/// fires in practice today: a dest move produces an updated row and a removed-surface row for one
/// bundle, and the bundle was UPDATED — it moved, and the surface it vacated is a detail of the
/// move, visible on its own row underneath. The rest of the ladder is written down so a future
/// multi-row shape lands somewhere deliberate instead of on whichever row was pushed first.
fn primary_rank(s: &PullSkill) -> u8 {
    use topos_types::results::PullAction as A;
    match s.action {
        // A decision owed outranks everything: it is the only outcome that needs a person.
        A::Conflicted => 6,
        // Bytes moved.
        A::Installed | A::FastForwarded | A::Refreshed | A::Merged | A::DraftSynced => 5,
        A::Removed => 4,
        A::Withdrawn | A::Released => 3,
        A::Held => 2,
        A::UpToDate => 1,
    }
}

/// One `remove` item line for the describe/apply — the boundary a followed removal keeps vs the
/// permanence of a local delete.
fn remove_item_line(item: &RemoveItem, applied: bool) -> String {
    // A removal-specific disclosure overrides the kind's stock line (the built-in skill's durable
    // opt-out and its way back).
    //
    // A note that carries SEVERAL facts becomes several lines. A config-placed bundle leaving six
    // agents folded six per-file clauses, plus any warning, into one `·`-joined sentence — an
    // eleven-clause run-on whose load-bearing clause (your hand edit survived) sat at whatever
    // position the harness table happened to order it into. The clauses are separated at their
    // one producer now; here the FIRST is the headline and the rest are indented under it, which
    // is what every other multi-fact receipt in this CLI already does.
    if let Some(note) = &item.note {
        // THE ONE THING A NOTE MAY NOT SWALLOW: what happens to the bytes. Replacing the stock
        // line is harmless while that line only re-words the kind — but the three LOCAL shapes
        // state the byte outcome itself (`PERMANENTLY` for the two that destroy, "the folder
        // stays" for the one that does not), and a config-placed bundle's note is ALWAYS populated
        // by the per-file convergence lines. So the gate for an irreversible delete was asking
        // consent in a sentence about config files. For those three the stock line is the
        // headline and every note clause indents under it; for the rest the note still leads.
        let (mut out, clauses): (String, Vec<&str>) = match item.kind {
            RemoveKind::TrackedLocalPermanent
            | RemoveKind::UntrackedLocal
            | RemoveKind::TrackedLocalRetired => {
                (remove_kind_line(item, applied), note.lines().collect())
            }
            _ => {
                let verb = if applied { "Removed" } else { "Would remove" };
                let mut lines = note.lines();
                let head = lines.next().unwrap_or_default();
                let mut lead = format!("{verb} '{}'{} — {head}", item.name, from_dirs(item));
                let rest: Vec<&str> = lines.collect();
                if rest.is_empty() {
                    lead.push('.');
                }
                (lead, rest)
            }
        };
        for line in clauses {
            out.push_str(&format!("\n    {line}"));
        }
        return out;
    }
    remove_kind_line(item, applied)
}

/// The stock sentence one `remove` kind speaks in — the byte outcome in the verb's own words,
/// before any per-file note the apply folded on.
fn remove_kind_line(item: &RemoveItem, applied: bool) -> String {
    match item.kind {
        RemoveKind::ManifestRemoved => {
            let manifest = item.manifest.as_deref().unwrap_or("topos.toml");
            // The copies-leave claim is made only where copies verifiably left: the describe
            // states the plan, and an APPLIED row's movements ride the uninstall block (or this
            // item's own cleaned dirs). An applied row whose uninstall moved nothing states the
            // row removal ALONE — the feed-still-delivers shape carries its own note, which wins
            // above, and everything else must not claim a departure that never happened.
            if applied && item.agent_dirs.is_empty() {
                format!("{manifest}: removed '{}'.", item.name)
            } else {
                format!(
                    "{manifest}: removed '{}' — the copies it placed leave this machine now (an \
                     edited copy stays in place).",
                    item.name
                )
            }
        }
        RemoveKind::ManifestNarrowed => {
            let manifest = item.manifest.as_deref().unwrap_or("topos.toml");
            format!(
                "{manifest}: narrowed '{}' — the named destination's copy leaves this machine \
                 now (an edited copy stays in place); the row keeps the rest.",
                item.name
            )
        }
        // A CLAIMED folder is the person's own — topos never created it, and detaching it is the
        // exact inverse of the claim: the record forgets the folder and not one byte moves.
        RemoveKind::ClaimDetached => format!(
            "removed {} from {} — the folder and its files stay; it just stops updating",
            item.kept_dirs.join(", "),
            item.name
        ),
        RemoveKind::FeedRemoved => {
            let manifest = item.manifest.as_deref().unwrap_or("topos.toml");
            let verb = if applied { "removed" } else { "would remove" };
            format!(
                "{manifest}: {verb} the {} feed line — the copies it delivered leave this \
                 machine now (an edited copy stays in place).",
                item.name
            )
        }
        RemoveKind::ManifestExcluded => {
            let manifest = item.manifest.as_deref().unwrap_or("topos.toml");
            format!(
                "{manifest}: excluded '{}' — a broader layer still provides it, so an exclude \
                 line (the one negative state) now withholds it here.",
                item.name
            )
        }
        RemoveKind::TrackedLocalPermanent => {
            let verb = if applied { "Deleted" } else { "Would delete" };
            format!(
                "{verb} '{}'{} PERMANENTLY — it was never published, so no other copy exists (the \
                 topos entry is dropped too).",
                item.name,
                from_dirs(item)
            )
        }
        RemoveKind::UntrackedLocal => {
            let verb = if applied { "Deleted" } else { "Would delete" };
            format!(
                "{verb} '{}'{} PERMANENTLY — an untracked local copy; no other copy exists.",
                item.name,
                from_dirs(item)
            )
        }
        // The folder is the person's own — they named it to `add`, and topos recorded it without
        // ever creating it. So the sentence leads with what actually ends (the record, and with it
        // whatever it placed elsewhere) and states plainly that the folder is not part of that.
        // Saying WHY it stays is the difference between a receipt someone trusts and one they
        // re-check against the filesystem.
        RemoveKind::TrackedLocalRetired => {
            let (verb, manages) = if applied {
                ("Removed", "topos no longer manages it")
            } else {
                ("Would remove", "topos would stop managing it")
            };
            let (stays, they, them) = if item.kept_dirs.len() == 1 {
                ("stays where it is", "that folder", "it")
            } else {
                ("stay where they are", "those folders", "them")
            };
            // A record can hold BOTH shapes — a copy topos materialized somewhere and the source
            // folder it adopted. The permanent half keeps the word; the kept half comes last,
            // because "what survives" is the clause a person scans a removal receipt for.
            let deleted = if item.agent_dirs.is_empty() {
                String::new()
            } else {
                format!(
                    "; the copies in {} are deleted PERMANENTLY",
                    item.agent_dirs.join(", ")
                )
            };
            format!(
                "{verb} '{}' — {manages}{deleted}; {} {stays} (you added {they} in place, so \
                 topos never created {them}).",
                item.name,
                item.kept_dirs.join(", "),
            )
        }
        RemoveKind::BuiltinOptOut => {
            let verb = if applied { "Removed" } else { "Would remove" };
            format!(
                "{verb} '{}'{} — the built-in skill's opt-out is durable (no sweep re-places it); \
                 `topos add topos` brings it back.",
                item.name,
                from_dirs(item)
            )
        }
    }
}

/// The ` from <dir>[, <dir>…]` clause a removal line carries when it names folders — ONE spelling,
/// so a describe names the very places its apply will empty, in the same words. A permanent delete
/// that named no path made the `--yes` gate ask about bytes it would not show.
fn from_dirs(item: &RemoveItem) -> String {
    if item.agent_dirs.is_empty() {
        String::new()
    } else {
        format!(" from {}", item.agent_dirs.join(", "))
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

/// The `remove` APPLY's TTY — undo-led on the reversible shape (the followed exclusion). A
/// feed-line drop's receipt speaks in what MOVED: one aligned `- <name>   removed (<dest>)` line
/// per bundle that lost copies, a kept line per edited copy left in place, then the paste-ready
/// `(undo: …)` — the same lines the update receipt prints for the hand-edited equivalent.
pub(crate) fn remove_applied_tty(data: &RemoveData) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for item in &data.items {
        // The uninstall block below says what a feed drop / row drop / narrow did, concretely —
        // the item's own summary line would only restate it vaguely. A carried note survives as
        // its own line (the off-switch's standing disclosure, the materialize note).
        let covered = match item.kind {
            RemoveKind::FeedRemoved => !data.uninstalled.is_empty(),
            RemoveKind::ManifestRemoved
            | RemoveKind::ManifestExcluded
            | RemoveKind::ManifestNarrowed => data
                .uninstalled
                .iter()
                .any(|u| u.name == item.name || u.name.rsplit('/').next() == Some(&item.name)),
            _ => false,
        };
        if covered {
            if let Some(note) = &item.note {
                notes.push(format!("note: {note}"));
            }
            continue;
        }
        lines.push(remove_item_line(item, true));
    }
    let rows: Vec<(String, String)> = data
        .uninstalled
        .iter()
        .filter(|u| !u.destinations.is_empty())
        .map(|u| (format!("- {}", u.name), uninstalled_column(u)))
        .collect();
    let pad = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, column) in &rows {
        lines.push(format!("{name:<pad$}   {column}"));
    }
    for u in &data.uninstalled {
        if !u.kept.is_empty() {
            let plain = u.name.rsplit('/').next().unwrap_or(&u.name);
            lines.push(kept_line(plain, &u.kept));
        }
    }
    lines.extend(notes);
    let mut s = lines.join("\n");
    if !data.undo.is_empty() {
        if data.uninstalled.is_empty() {
            s.push_str(&undo_line(&data.undo));
        } else {
            s.push_str(&format!("\n(undo: {})", argv_line(&data.undo)));
        }
    }
    s
}

/// The destination column of one uninstalled bundle — the same count/path rule the update
/// receipt's removed rows use. A NARROWED removal appends what the row still names
/// (`— 2 folders remain` / `— 1 config file remains`).
fn uninstalled_column(u: &topos_types::results::UninstalledBundle) -> String {
    let subject = crate::actions::Subject::of_kind(u.kind.as_deref());
    let mut out = match u.destinations.as_slice() {
        [] => "removed".to_owned(),
        [one] => format!("removed ({one})"),
        many => format!("removed ({})", subject.targets(many.len())),
    };
    if let Some(n) = u.remaining {
        let verb = if n == 1 { "remains" } else { "remain" };
        out.push_str(&format!(
            " — {} {verb}",
            subject.targets(usize::try_from(n).unwrap_or(usize::MAX))
        ));
    }
    out
}

/// The `protect` DESCRIBE's TTY — the level being set, the role it takes, and the standing note.
pub(crate) fn protect_describe_tty(
    data: &topos_types::results::ProtectData,
    yes_argv: &[String],
) -> String {
    let direction = if data.loosening { "Loosen" } else { "Tighten" };
    let mut s = format!(
        "{direction} {} '{}' to `{}`",
        data.kind, data.target, data.level
    );
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
            "\nLeads with the {skill} skill — accepting assigns it to them."
        ));
    }
    if let Some(channel) = &data.channel {
        s.push_str(&format!(
            "\nLeads with the {channel} channel — accepting assigns it to them."
        ));
    }
    s.push_str(
        "\nEach address gets a mailed single-use invite link (browser, agent paste-block, or \
         `topos login <invite-url>`). Sending needs the server's outgoing mail — without SMTP \
         the send refuses and nothing is recorded.",
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
        "\nThey join at {} — ask them to run `topos login {}` and sign in with their invited email.",
        data.address, data.address,
    ));
    out
}

/// The one-line disclosure that a `publish` ADDED the skill to topos first (the auto-add convenience) —
/// honest plumbing ("it also added this skill"), naming the FOLDER it came from. A folder is the fact
/// the line can state; which agents read it is a different question. It states only the add; the line
/// that FOLLOWS conveys what the publish then did.
fn added_line(added: &AddedNote) -> String {
    match &added.folder {
        Some(folder) => format!("Added '{}' from {folder} to topos.", added.name),
        None => format!("Added '{}' to topos.", added.name),
    }
}

/// The bare enrolled `publish` DESCRIBE — where it lands and what the gate does with it. Nothing has
/// landed on the plane yet.
///
/// The TTY is deliberately NARROWER than the `--json` payload: the digest, the share/handoff lines,
/// and the undo describe a publish that has not happened, so they say nothing a reader can act on
/// BEFORE `--yes`. They ride the envelope untouched — every one of them prints on the receipt,
/// where they are facts rather than predictions.
pub(crate) fn publish_describe_tty(
    data: &topos_types::results::PublishDescribeData,
    yes_argv: &[String],
) -> String {
    use topos_types::results::PublishGate;
    // A workspace is named by its ADDRESS — the handle a person recognizes and types. The display
    // name is the fallback for an address that could not be read, never the first choice.
    let ws = data
        .workspace_address
        .as_deref()
        .or(data.workspace_display_name.as_deref())
        .unwrap_or(&data.workspace_id);
    let mut s = format!("Publish '{}' to {ws}:", data.skill);
    // WHICH copy, whenever the folder was a CHOICE: one of several edited copies, or a copy in a
    // scope other than the one this command stands in. With a single edited copy here there was
    // nothing to choose between, and naming the folder would answer a question nobody asked.
    if let Some(from) = &data.from_placement {
        if !data.other_edited.is_empty() {
            s.push_str(&format!(
                "\n  from {from} (1 of {} edited copies)",
                data.other_edited.len() + 1
            ));
        } else if data.from_machine && data.other_scope_draft.is_none() {
            // The reason the OTHER scope's copy is the one shipping — the whole surprise a
            // cross-scope publish can carry, stated where the folder is named. Withheld when the
            // standing copy has edits too: the disclosure below says what happens to them, and
            // "this project's copy has none" would then be false.
            s.push_str(&format!(
                "\n  from {from} — the machine copy carries the edits; this project's copy has \
                 none."
            ));
        } else {
            s.push_str(&format!("\n  from {from}"));
        }
    }
    // The KIND, when it is not the ordinary skill: it decides what the workspace records and how
    // every receiving machine places the bytes, so it is named before anything else about them.
    if let Some(kind) = &data.kind {
        s.push_str(&format!("\n  kind: {kind}"));
    }
    // The gate, as the sentence it means for the person running the command — not the protection
    // level's name.
    let gate = match data.gate {
        PublishGate::Lands => {
            "No review required — it becomes immediately published in the workspace."
        }
        PublishGate::Proposal => {
            "Review required — will require approval by a reviewer after publishing."
        }
    };
    s.push_str(&format!("\n  {gate}"));
    // What happens to the copies this publish does NOT ship: nothing. Each keeps its bytes and
    // becomes an ordinary draft ahead of the version about to land — the same shape a teammate's
    // publish leaves behind — so the reader knows the other folder is not being chosen against.
    if !data.other_edited.is_empty() {
        s.push_str(&format!("\n  {}", other_copies_clause(&data.other_edited)));
    }
    // The same fact across SCOPES: the other scope's copy is not being shipped and is not being
    // touched, and one command shares it when the reader wants that too.
    if let Some(other) = &data.other_scope_draft {
        s.push_str(&format!("\n  {}", other_scope_clause(other, &data.skill)));
    }
    if data.is_revert {
        s.push_str("\n  this restores earlier bytes (a revert), shipped through the same gate");
    }
    if !data.placements.is_empty() {
        s.push_str(&format!(
            "\n  places into channel: {}",
            data.placements.join(", ")
        ));
        if let Some(note) = &data.placement_note {
            s.push_str(&format!(" — {note}"));
        }
    }
    // A behind copy reaches the describe only on the `--propose` arm (the behind guard refuses the
    // direct one first), and a proposal from behind is fine — a reviewer decides. What they are
    // owed is the fact that this candidate is not built on the team's newest version, plus the
    // in-memory dry run of the merge that would follow.
    if let Some(preview) = &data.merge_preview {
        s.push_str(&format!(
            "\n  note: the team has a newer version this copy does not have, so this proposal is \
             not built on it; {}",
            merge_preview_line(preview)
        ));
    }
    if let Some(note) = &data.origin_note {
        s.push_str(&format!("\n  note: {note}"));
    }
    // The predicted governance transfer — the manifest edit the apply would perform.
    if let (Some(manifest), Some(reference), Some(from)) =
        (&data.manifest, &data.reference, &data.converted_from)
    {
        s.push_str(&format!(
            "\n  governance transfer: {manifest} — the \"{from}\" line becomes \"{reference}\" \
             (the local copy becomes a managed placement of the governed bundle)"
        ));
    }
    // The read that settles it, spelled for the copy that resolved: a preview says WHAT would
    // ship, and this is the one command that shows the bytes themselves.
    if let Some(review) = &data.review {
        s.push_str(&format!("\nreview: {review}"));
    }
    s.push_str(&format!(
        "\nNothing has changed yet — apply with:\n  {}",
        argv_line(yes_argv)
    ));
    s
}

/// The OTHER scope's copy, in the words the same-scope sentence uses ([`other_copies_clause`]) plus
/// the two things only a scope crossing adds: WHOSE copy it is, and the command that shares it —
/// which differs from the one just run by exactly the `-g` that names the machine.
fn other_scope_clause(other: &topos_types::results::ScopeDraft, skill: &str) -> String {
    let (whose, flag) = if other.machine {
        ("your machine copy", " -g")
    } else {
        ("your project copy", "")
    };
    format!(
        "{whose} in {} keeps its edits — it becomes a draft ahead of this version (topos \
         publish{flag} {skill} shares it).",
        other.folder
    )
}

/// The already-published answer: the converged state, stated as the success it is. No `error:`, no
/// apply scaffold — there is nothing to apply, and a refusal here sent agents into a retry loop
/// over a state that was exactly what they asked for.
///
/// The second line fires only where the edits are somewhere this run did not look: the other
/// scope's copy. "Nothing to publish" is a true answer that would still be a misleading one while a
/// draft sits one flag away.
pub(crate) fn publish_no_changes_tty(data: &topos_types::results::PublishNoChangesData) -> String {
    let mut out = format!(
        "'{}' is already published — your copy matches current",
        data.skill
    );
    if let Some(other) = &data.other_scope_draft {
        let (whose, flag) = if other.machine {
            ("your machine copy", " -g")
        } else {
            ("this project's copy", "")
        };
        out.push_str(&format!(
            "\nyour edits are in {whose} — share them: topos publish{flag} {}",
            data.skill
        ));
    }
    out
}

/// The one command the already-published answer offers an agent: the publish of the OTHER scope's
/// copy, byte-for-byte the command its second line prints. Empty when there is no such copy —
/// better no action than one that re-runs what just succeeded.
pub(crate) fn publish_no_changes_actions(
    data: &topos_types::results::PublishNoChangesData,
) -> Vec<NextAction> {
    let Some(other) = &data.other_scope_draft else {
        return Vec::new();
    };
    let mut argv = vec!["topos".to_owned(), "publish".to_owned()];
    if other.machine {
        argv.push("-g".to_owned());
    }
    argv.push(data.skill.clone());
    argv.push("--json".to_owned());
    vec![crate::actions::next_action(
        ActionCode::from("RUN_COMMAND".to_owned()),
        argv,
    )]
}

/// The WHOLE "these folders are left alone" sentence, shared by the publish describe and its
/// receipt. One outcome deserves one sentence: the two surfaces used to say the same fact three
/// different ways (a singular describe, a singular receipt, a plural receipt), which reads as three
/// different facts to anyone who sees more than one of them. Singular is the case the wording was
/// written for — one copy shipped, one left behind; three-plus edited copies pluralize the same
/// sentence rather than gaining a shape of their own.
fn other_copies_clause(others: &[String]) -> String {
    if let [one] = others {
        return format!(
            "your other copy in {one} keeps its edits — it becomes a draft ahead of this version."
        );
    }
    format!(
        "your other copies in {} keep their edits — they become drafts ahead of this version.",
        others.join(", ")
    )
}

pub(crate) fn publish_tty(data: &PublishData) -> String {
    let mut out = String::new();
    // If this invocation ADDED the skill first (the auto-add convenience), say so before the publish line.
    if let Some(added) = &data.added {
        out.push_str(&added_line(added));
        out.push('\n');
    }
    // Lead with the NAME and WHERE it landed — the two facts the author checks; the opaque
    // skill_id stays a `--json` key, never the human line. The address falls off (rather than
    // being faked from a display name) when the post-write `me` read did not answer.
    out.push_str(&format!(
        "Published {}@{}",
        data.name,
        short(&data.version_id),
    ));
    if let Some(address) = &data.workspace_address {
        out.push_str(&format!(" to {address}"));
    }
    // WHICH copy shipped, and what became of the ones that did not — printed only where the skill
    // was edited in more than one folder, which is the only time either was a choice. The fact is
    // the SAME one the describe predicted, said in the same words (see [`other_copies_clause`]):
    // the copy is untouched and is now an ordinary draft ahead of what just landed.
    if let Some(from) = &data.from_placement {
        out.push_str(&format!(" (from {from})"));
    }
    if !data.other_edited.is_empty() {
        out.push_str(&format!("\n{}", other_copies_clause(&data.other_edited)));
    }
    // The same fact across SCOPES, in the same words: the other scope's copy kept its edits, and
    // the one command that shares them is named beside the one that just ran.
    if let Some(other) = &data.other_scope_draft {
        out.push_str(&format!("\n{}", other_scope_clause(other, &data.name)));
    }
    // The KIND the catalog now records — stated because it is what decides how every receiving
    // machine places these bytes (an mcp bundle lands in each agent's MCP config, not a folder).
    if let Some(kind) = &data.kind {
        out.push_str(&format!(
            "\nthe workspace records it as a `{kind}` bundle — teammates' agents get it as a \
             server entry in their own config, not a skill folder."
        ));
    }
    // A withheld placement is disclosed next to the success it qualifies: the publish landed,
    // the curated channel's reference did not — placement takes reviewer or owner.
    if let Some(ch) = &data.placement_withheld {
        out.push_str(&format!(
            "\nchannel '{ch}' is curated — the reference was NOT placed (placement takes \
             reviewer or owner). The '{}' skill is in the catalog; a curator places it on the \
             web (the channel's page).",
            data.name,
        ));
    }
    // The deletion race's honest line: the verified channel vanished before the write — the
    // publish landed catalog-only; nothing was minted.
    if let Some(ch) = &data.placement_missing {
        out.push_str(&format!(
            "\nchannel '{ch}' no longer exists — it was deleted before this publish landed, so \
             the reference was NOT placed (nothing was created); the skill is in the catalog."
        ));
    }
    // The governance transfer, stated on the receipt with its inverse (a manifest edit — there
    // is no one-command undo, so the inverse is named as the edit it is).
    if let (Some(manifest), Some(reference), Some(from)) =
        (&data.manifest, &data.reference, &data.converted_from)
    {
        out.push_str(&format!(
            "\ngovernance transfer: {manifest} — \"{from}\" is now \"{reference}\" (the local \
             copy is a managed placement of the governed bundle; to undo, edit that manifest and \
             restore the \"{from}\" line)"
        ));
    }
    // The landed-but-rewrite-pending truth: the publish holds; the manifest edit does not, yet.
    if let Some(note) = &data.rewrite_pending {
        out.push_str(&format!("\nnote: {note}"));
    }
    // The concurrent-removal truth: the row was removed while the publish ran; nothing re-added it.
    if let Some(note) = &data.rewrite_skipped {
        out.push_str(&format!("\nnote: {note}"));
    }
    if let Some(note) = &data.origin_note {
        out.push_str(&format!("\nnote: {note}"));
    }
    // The undo LEADS the trailer — the receipt's first offer is the way back out. It is present
    // only when it verifiably restores the whole prior state (the producer withholds it otherwise).
    if let Some(undo) = &data.undo {
        out.push_str(&format!("\nundo: {undo}"));
    }
    if let Some(line) = &data.share_line {
        out.push_str(&format!("\nshare: {line}"));
    }
    // The teammate handoff, after the confirmation: the one line to hand someone not yet in the
    // workspace (the skill page URL answers only for members, so it never recruits anyone).
    if let Some(line) = &data.invite_line {
        out.push_str(&format!("\nbring a teammate: {line}"));
    }
    out
}

/// The review-gated publish's receipt: the bytes ARE uploaded and named, `current` did NOT move, and
/// BOTH ways it can settle are on the page. The two verdict lines are padded so their commands line
/// up as the one block they are — and both are named because the four-eyes rule means the author
/// cannot approve their own proposal: someone else's `--approve`, or the author's own `--withdraw`.
pub(crate) fn propose_tty(data: &ProposeData) -> String {
    // If this invocation ADDED the skill first (the auto-add convenience), disclose it before the proposal.
    let prefix = match &data.added {
        Some(added) => format!("{}\n", added_line(added)),
        None => String::new(),
    };
    // The handle a reviewer pastes — SHORT, because the verdict path resolves a unique prefix of
    // 8+ chars exactly as `revert --to` does. The skill NAME rides out of the same split: the
    // scope disclosure below spells a command with it.
    let (skill, handle) = match data.proposal.split_once('@') {
        Some((skill, version)) => (skill, format!("{skill}@{}", short(version))),
        None => (data.title.as_str(), data.proposal.clone()),
    };
    let destination = match &data.workspace_address {
        Some(address) => format!(" to {address}"),
        None => String::new(),
    };
    // WHICH copy these bytes came from — the same parenthetical the landed receipt prints, in the
    // same place relative to where they went. A proposal ships bytes; the choice is the same one.
    let from = match &data.from_placement {
        Some(folder) => format!(" (from {folder})"),
        None => String::new(),
    };
    let mut out = format!(
        "{prefix}Published {handle}{destination}{from} for review.\n\
         Review required — a reviewer approves with: topos review {handle} --approve\n\
         withdraw it yourself:                       topos review {handle} --withdraw"
    );
    // What becomes of the copies this proposal did not ship — one outcome, one sentence, wherever
    // the reader meets it (the describe predicted these exact words).
    if !data.other_edited.is_empty() {
        out.push_str(&format!("\n{}", other_copies_clause(&data.other_edited)));
    }
    if let Some(other) = &data.other_scope_draft {
        out.push_str(&format!("\n{}", other_scope_clause(other, skill)));
    }
    if let Some(ch) = &data.placement_withheld {
        out.push_str(&format!(
            "\nchannel '{ch}' is curated — the reference was NOT placed (placement takes \
             reviewer or owner); a curator places it on the web (the channel's page)."
        ));
    }
    if let Some(ch) = &data.placement_missing {
        out.push_str(&format!(
            "\nchannel '{ch}' no longer exists — it was deleted before this proposal opened, so \
             the reference was NOT placed (nothing was created)."
        ));
    }
    if let (Some(manifest), Some(reference), Some(from)) =
        (&data.manifest, &data.reference, &data.converted_from)
    {
        out.push_str(&format!(
            "\ngovernance transfer: {manifest} — \"{from}\" is now \"{reference}\" (delivery \
             follows once the proposal is approved; to undo, edit that manifest and restore the \
             \"{from}\" line)"
        ));
    }
    if let Some(note) = &data.rewrite_pending {
        out.push_str(&format!("\nnote: {note}"));
    }
    if let Some(note) = &data.rewrite_skipped {
        out.push_str(&format!("\nnote: {note}"));
    }
    if let Some(line) = &data.share_line {
        out.push_str(&format!("\nshare: {line}"));
    }
    out
}

pub(crate) fn revert_tty(data: &RevertData) -> String {
    format!(
        "Reverted {} to {} as forward commit {} — nothing was deleted; move current forward \
         again to redo.",
        data.name,
        short(&data.reverted_to),
        short(&data.new_version_id),
    )
}

/// The bare `revert` DESCRIBE's TTY — what the forward move would do (nothing has changed yet).
pub(crate) fn revert_describe_tty(
    data: &topos_types::results::RevertDescribeData,
    yes_argv: &[String],
) -> String {
    format!(
        "Revert {} — move current @{} forward to restore @{}.\n  a forward \
         move restoring older bytes; nothing deleted\nNothing has changed yet — apply with:\n  {}",
        data.skill,
        short(&data.current_version_id),
        short(&data.reverted_to),
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
        ReviewDecision::Approve => format!(
            // `proposal` is `<skill>@<version_id>` — the hash IS the destination, git-style; the
            // pointer's internal edition count stays a `--json` field, never the human line.
            "Approved {} — current moved to it. Every follower picks it up on their next update.",
            data.proposal,
        ),
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

/// Which set an `update` receipt is reporting on, read back from [`PullData::scope`]. `update` acts
/// where you stand, so a receipt that does not name its scope leaves the OTHER set's silence
/// ambiguous. The background sweep reconciles `"both"` and prints nothing anyway, so it reads as
/// unstated — as does a producer predating the scoped update, and the single-skill go-back, which
/// acts on local bytes rather than on a scope.
enum PullReceiptScope {
    Project(String),
    Machine,
    Unstated,
}

impl PullReceiptScope {
    fn read(scope: Option<&str>) -> Self {
        match scope {
            Some("machine") => PullReceiptScope::Machine,
            Some(s) => match s.strip_prefix("project ") {
                Some(dir) => PullReceiptScope::Project(dir.to_owned()),
                None => PullReceiptScope::Unstated,
            },
            None => PullReceiptScope::Unstated,
        }
    }

    /// The receipt's opening line, when there is one to say. The project form NAMES the folder
    /// once, and by doing so DEFINES the `project` token every path below it is written against
    /// (see [`PullReceiptScope::relative`]).
    ///
    /// The VERB is the run's own answer to "did anything move here": a sweep whose only row is a
    /// stopped merge's re-disclosure, or an mcp bundle with drift worth eyes, changed nothing —
    /// and a header claiming it updated the set is a claim its own rows contradict. `checked` is
    /// what such a run did. See [`PullReceiptScope::moved_anything`] for the rule.
    fn lead(&self, changed: bool) -> Option<String> {
        let verb = if changed { "updated" } else { "checked" };
        match self {
            PullReceiptScope::Project(dir) => Some(format!("{verb} project ({dir})")),
            PullReceiptScope::Machine => Some(format!("{verb} machine-wide")),
            PullReceiptScope::Unstated => None,
        }
    }

    /// Whether a run's rows moved anything on this machine. A row COUNTS when the run wrote
    /// something durable for it: bytes landed, were rewritten, were fanned out, or left (the
    /// four `updated` outcomes plus `installed`, `removed` and upstream's `withdrawn`), or a
    /// record retired for good (`released` — nothing on disk moved, but the sweep will never
    /// report that record again). A row does NOT count when the run only looked and reported:
    /// already current, a merge still waiting on a person, a pinned hold. Failures, decisions,
    /// advisories and disclosures are all "looked and reported" too, so none of them makes a
    /// run a change either.
    ///
    /// The rule is about DISK, not about versions — which is why an MCP bundle whose store was
    /// already current but whose config entry this run put back reaches here as `updated`
    /// rather than `up to date`: the converge answers for what it wrote
    /// ([`crate::mcp_engine::BundleStates::wrote`]), and the sweep re-reads the row as the
    /// ordinary catch-up it is. A converge that only rewrote topos's own ledger wrote nothing
    /// a person's agent reads, and moves no row.
    fn moved_anything(skills: &[PullSkill]) -> bool {
        use topos_types::results::PullAction as A;
        skills.iter().any(|s| match s.action {
            A::UpToDate | A::Conflicted | A::Held => false,
            A::FastForwarded
            | A::Installed
            | A::Refreshed
            | A::Removed
            | A::Merged
            | A::DraftSynced
            | A::Withdrawn
            | A::Released => true,
        })
    }

    /// One body line with every in-project path rewritten against the folder the lead line named:
    /// `~/Forward/labs/api/.claude/skills/deploy` → `project/.claude/skills/deploy`. The absolute
    /// path is said ONCE, at the top, and the rows below read as what they are — places inside the
    /// thing you are standing in. Machine-scope paths keep their `~/` spelling untouched.
    fn relative(&self, line: &str) -> String {
        match self {
            PullReceiptScope::Project(dir) => line.replace(&format!("{dir}/"), "project/"),
            PullReceiptScope::Machine | PullReceiptScope::Unstated => line.to_owned(),
        }
    }
}

/// The human `update` view — one line per skill that needs attention (gh-status style: name, what
/// happened, and the concrete next command where one exists), up-to-date rows summarized compactly,
/// isolated per-skill failures (`warnings` — the same stable lines the `--json` envelope carries)
/// rendered visibly, and the awaiting-review trailer. The `--quiet` hook path never reaches this
/// renderer (it stays byte-silent).
///
/// `decisions` are the third kind of row: a bundle waiting on the PERSON, not on the network or on
/// a repair. They join the table beside the skill rows — same padding, same indented detail — and
/// count under `waiting on you`, because a person told their update "failed" goes looking for the
/// fault instead of making the choice, and a run that only ever asked a question has not failed.
///
/// `advisories` print as `warning:` beside the failures but are counted by NEITHER half of the
/// summary: each annotates a bundle that DELIVERED and has its own row (an unknown MCP dest
/// entry dropped from its narrowing), so counting the line too would invent a second, failed
/// bundle. `disclosures` are the OTHER half of that stable line channel: facts about what WORKED
/// (a settled-draft fan-out, a cross-scope version split). They print as `note:` — the same word
/// every other receipt in this CLI uses for a disclosure — and they are counted separately,
/// because the summary below says "failed" and a successful run must never claim one.
pub(crate) fn pull_tty(
    data: &PullData,
    decisions: &[crate::ops::PendingDecision],
    warnings: &[topos_types::Message],
    advisories: &[topos_types::Message],
    disclosures: &[topos_types::Message],
    // `failed`: how many BUNDLES this run could not carry forward — the sweep's own count,
    // never `warnings.len()`. See the arithmetic at the summary below.
    failed: usize,
) -> String {
    let scope = PullReceiptScope::read(data.scope.as_deref());
    if data.skills.is_empty()
        && decisions.is_empty()
        && warnings.is_empty()
        && advisories.is_empty()
        && disclosures.is_empty()
    {
        // A run that moved nothing says exactly that. What a person standing in a project could
        // once MISREAD here — an untouched machine-wide set taken for an emptied one — is answered
        // by the trailer below, which is why this path calls it: it names the scope this run left
        // alone, counts what stands behind there, and carries the command that fixes it. Silence
        // about the other set only ever read as "nothing to do" because nothing counted it.
        let mut line = match &scope {
            PullReceiptScope::Project(_) => "Up to date.".to_owned(),
            PullReceiptScope::Machine => {
                "Nothing to update machine-wide — no manifest or workspace feed demands anything."
                    .to_owned()
            }
            PullReceiptScope::Unstated => {
                "Nothing to update here — no manifest or profile demands anything in this directory."
                    .to_owned()
            }
        };
        for behind in behind_trailer(&data.behind_elsewhere) {
            line.push_str(&format!("\n{behind}"));
        }
        return append_proposals_trailer(line, data.proposals_awaiting);
    }
    use topos_types::results::PullAction;
    let mut kept_lines: Vec<String> = Vec::new();
    let mut tally = PullTally::default();
    // ONE BUNDLE, ONE COUNT — the rows STAY, the tally does not double.
    //
    // A single bundle can earn several rows in one run and every one of them is true: a dest move
    // both updates the bundle and removes the surface it left, so the receipt shows an updated row
    // AND a removed-surface row, which is what a person needs to see. The SUMMARY is a different
    // question — how many bundles did this run touch — and counting rows answered it wrong
    // ("Checked 2 bundles" over a machine holding one). Each distinct bundle is counted once now,
    // under its PRIMARY outcome ([`primary_rank`]), so the clauses still sum to the total.
    let primary: std::collections::HashSet<usize> = {
        let mut best: std::collections::BTreeMap<(&str, &str), (usize, u8)> =
            std::collections::BTreeMap::new();
        for (i, s) in data.skills.iter().enumerate() {
            let key = (s.scope.as_deref().unwrap_or(""), s.skill.as_str());
            let rank = primary_rank(s);
            match best.get(&key) {
                Some(&(_, held)) if held >= rank => {}
                _ => {
                    best.insert(key, (i, rank));
                }
            }
        }
        best.values().map(|&(i, _)| i).collect()
    };
    let mut rows: Vec<(String, String, Vec<String>)> = data
        .skills
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            if primary.contains(&i) {
                tally.count(s);
            }
            // The name a receipt row leads with: the workspace-qualified display where one is
            // recorded, `+`/`-`-led for the rows that moved bytes in or out.
            let shown = s.display.as_deref().unwrap_or(&s.skill);
            if !s.kept.is_empty() {
                kept_lines.push(kept_line(&s.skill, &s.kept));
            }
            // An up-to-date row stays out of the table — UNLESS it is an mcp bundle with a
            // per-agent state that needs eyes (drift, a conflict, an unprovable surface): a
            // compact receipt must not swallow those. A file this run wrote (`placed`) is not
            // one of them: a row that moved bytes has already earned its own verb. A row that
            // carries a NOTE has something to say by construction (an mcp bundle whose dest
            // reaches no agent has no per-agent state to speak for it), so it is never swallowed
            // either — a healthy row carries none.
            // A DRAFTED row is noteworthy by construction: swallowing it is exactly how the
            // summary came to say "all up to date" about a bundle `list` was calling a draft.
            let noteworthy = s.draft
                || s.note.is_some()
                || s.harnesses.iter().any(|h| {
                    !(h.state.wrote() || h.state == topos_types::results::TargetOutcome::Current)
                });
            if matches!(s.action, PullAction::UpToDate) && !noteworthy {
                return None;
            }
            // A removal that uninstalled nothing (every copy edited, all kept) says so through
            // its kept line alone — a `- … removed` row naming no destination would be false.
            if s.action == PullAction::Removed && s.destinations.is_empty() && !s.kept.is_empty() {
                return None;
            }
            let lead = match s.action {
                PullAction::Installed => format!("+ {shown}"),
                PullAction::Removed => format!("- {shown}"),
                // `-` is this receipt's mark for bytes that LEFT this machine. A released record
                // deletes nothing — its own line says the files stay — so it leads with the name
                // alone, still workspace-qualified: which workspace stopped sharing is the point.
                PullAction::Released => shown.to_owned(),
                _ => s.skill.clone(),
            };
            let (line, mut extra) = pull_row(s, &scope);
            // A row that WROTE a config file this run reads its untouched files as `unchanged`
            // beside the ones it placed — two words that answer the same question. A row that
            // wrote nothing has nothing to contrast with, so its files stay `current`.
            let wrote = s.harnesses.iter().any(|h| h.state.wrote());
            extra.extend(s.harnesses.iter().map(|h| mcp_agent_line(h, wrote)));
            Some((lead, line, extra))
        })
        .collect();
    // The decisions take their place in the SAME table: a bundle waiting on an answer is a row of
    // this run like any other, and padding it with the rest is what makes the receipt readable as
    // one list rather than a receipt plus an appendix of complaints.
    rows.extend(
        decisions
            .iter()
            .map(|d| (d.name.clone(), d.line.clone(), d.detail.clone())),
    );

    let mut out = String::new();
    if let Some(lead) = scope.lead(PullReceiptScope::moved_anything(&data.skills)) {
        out.push_str(&format!("{lead}\n"));
    }
    let pad = rows.iter().map(|(n, ..)| n.len()).max().unwrap_or(0);
    for (name, line, extra) in &rows {
        out.push_str(&scope.relative(&format!("{name:<pad$}   {line}\n")));
        for x in extra {
            out.push_str(&scope.relative(&format!("    {x}\n")));
        }
    }
    for k in &kept_lines {
        out.push_str(&scope.relative(&format!("{k}\n")));
    }
    // The TTY prints the TEXT, never the code: a person reading a receipt should not have to
    // skip a machine word to reach the sentence, and the code rides `messages[].code` for the
    // consumer that wants it.
    for w in warnings.iter().chain(advisories) {
        out.push_str(&format!("warning: {}\n", w.text));
    }
    for d in disclosures {
        out.push_str(&format!("note: {}\n", d.text));
    }

    // The delivered notices (verdicts first) — an interactive `update` MARKED these read server-side,
    // so it MUST show them here or the verdict + its reason are lost forever (the quiet hook fetches
    // without acking, so it never reaches this renderer).
    let (verdicts, others): (Vec<_>, Vec<_>) =
        data.notices.iter().partition(|n| n.kind == "verdict");
    for n in verdicts.iter().chain(others.iter()) {
        out.push_str(&format!("{}\n", notice_line(n)));
    }

    // The summary counts every BUNDLE the sweep attempted — the ones it could not carry forward,
    // and the ones waiting on an answer — and names them by what they ARE: a sweep that
    // reconciled an MCP server counts bundles, because calling that row a skill is simply false.
    // All-skills stays the ordinary word.
    //
    // `failed` is the count of BUNDLES that failed, never of warning LINES. Those are not the
    // same number and never were: a scope-level fault (an unavailable lock, an unreadable custody
    // document) is one line about no bundle at all, and two lines can be about one bundle. Counting
    // lines invented bundles that do not exist and then reported them failed — `Checked 3 skills:
    // 2 already up to date, 1 failed` over a machine holding two.
    let total = primary.len() + failed + decisions.len();
    let noun = managed_noun(&data.skills, total);
    tally.failed = failed;
    tally.waiting += decisions.len();
    if rows.is_empty() && warnings.is_empty() && kept_lines.is_empty() {
        out.push_str(&format!("Checked {total} {noun}: all up to date."));
    } else {
        out.push_str(&format!("Checked {total} {noun}"));
        let parts = tally.parts();
        if !parts.is_empty() {
            out.push_str(&format!(": {}", parts.join(", ")));
        }
        out.push('.');
    }
    for line in behind_trailer(&data.behind_elsewhere) {
        out.push_str(&format!("\n{line}"));
    }
    append_proposals_trailer(out, data.proposals_awaiting)
}

/// What an `update` receipt's summary counts, by name. The clause exists so the reader never has
/// to subtract, which only holds if the arithmetic does: EVERY row lands in exactly one bucket, and
/// the parts always sum to the total the line opens with. So a state that is a decision rather than
/// a landing gets a bucket too — a conflicted row is `waiting on you`, never rolled into a number
/// with no word on it. The words are the ones a person uses about their own skills, not the
/// engine's state names: every way bytes caught up reads `updated`, and a withdrawal and a retired
/// record both read `no longer shared`, because the difference between them is the row's own line
/// to make. Only the buckets that actually hold something are rendered.
#[derive(Default)]
struct PullTally {
    installed: usize,
    updated: usize,
    removed: usize,
    up_to_date: usize,
    waiting: usize,
    no_longer_shared: usize,
    held: usize,
    failed: usize,
    /// Bundles carrying local edits that are not shared — the same fact `list` prints as
    /// `(draft)` and `status` counts as `drafts ahead`.
    drafts_ahead: usize,
}

impl PullTally {
    fn count(&mut self, s: &PullSkill) {
        use topos_types::results::PullAction as A;
        // A DRAFTED row is not "already up to date" — delivery owed it nothing, and something of
        // the person's is still unshared. It takes its own bucket INSTEAD of that one (never as
        // well, or the parts would stop summing to the total), in the words `status` already
        // uses for the same fact.
        if s.draft && matches!(s.action, A::UpToDate | A::Refreshed) {
            self.drafts_ahead += 1;
            return;
        }
        match s.action {
            A::Installed => self.installed += 1,
            // Four ways bytes caught up: a served version landed, a copy behind the version this
            // machine holds was rewritten, a draft was rebased onto the new current, a settled
            // draft was fanned out to this scope's other folders.
            A::FastForwarded | A::Refreshed | A::Merged | A::DraftSynced => self.updated += 1,
            A::Removed => self.removed += 1,
            A::UpToDate => self.up_to_date += 1,
            A::Conflicted => self.waiting += 1,
            // Nothing on disk was deleted in either case — delivery ended, and each row says what
            // that left behind. Still rows this run checked, so still counted.
            A::Withdrawn | A::Released => self.no_longer_shared += 1,
            A::Held => self.held += 1,
        }
    }

    /// The counted clauses, in the order a person reads them: what arrived, what moved, what left,
    /// what needed nothing, what is waiting on a decision, what stopped being shared, what broke.
    fn parts(&self) -> Vec<String> {
        [
            (self.installed, "installed"),
            (self.updated, "updated"),
            (self.removed, "removed"),
            (self.up_to_date, "already up to date"),
            (self.drafts_ahead, "draft ahead"),
            (self.waiting, "waiting on you"),
            (self.no_longer_shared, "no longer shared"),
            (self.held, "held"),
            (self.failed, "failed"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        // `draft ahead` is the one clause whose noun pluralizes inside it, not at the end.
        .map(|(n, what)| match what.strip_suffix(" ahead") {
            Some(noun) => format!("{} ahead", counted(n as u64, noun)),
            None => format!("{n} {what}"),
        })
        .collect()
    }

    /// Every counted row, however it was counted — the sum the summary's total must equal.
    #[cfg(test)]
    fn total(&self) -> usize {
        self.installed
            + self.updated
            + self.removed
            + self.up_to_date
            + self.waiting
            + self.no_longer_shared
            + self.held
            + self.failed
            + self.drafts_ahead
    }
}

/// The noun a receipt counts its rows in. `skill` while every row IS one — the word a person
/// reads most and the one the rest of the CLI uses — and the generic `bundle` the moment a row of
/// another kind rode along, because one summary cannot call a server a skill and stay true.
fn managed_noun(skills: &[PullSkill], total: usize) -> &'static str {
    match (skills.iter().any(|s| s.kind.is_some()), total == 1) {
        (true, true) => "bundle",
        (true, false) => "bundles",
        (false, true) => "skill",
        (false, false) => "skills",
    }
}

/// The receipt's closing arithmetic about the scope this run left alone: how many bundles stand
/// behind there, and the ONE command that fixes it. Counted, never named — the list belongs to the
/// scope that would print it, and a person reading this one wants the number and the way out.
fn behind_trailer(entries: &[topos_types::results::BehindElsewhere]) -> Vec<String> {
    let mut dirs: Vec<Option<&str>> = entries.iter().map(|e| e.project_dir.as_deref()).collect();
    dirs.sort_unstable();
    dirs.dedup();
    dirs.into_iter()
        .map(|dir| {
            let n = entries
                .iter()
                .filter(|e| e.project_dir.as_deref() == dir)
                .count();
            let noun = if n == 1 { "bundle" } else { "bundles" };
            match dir {
                // The machine's own copies: one command from here brings them current.
                None => {
                    let it = if n == 1 { "it" } else { "them" };
                    format!("{n} {noun} behind machine-wide — `topos update -g` updates {it}.")
                }
                // A checkout's copies: `update` acts where you stand, so the fix is run THERE.
                Some(dir) => format!("{n} {noun} behind in {dir} — run `topos update` there."),
            }
        })
        .collect()
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

/// One MCP config outcome as a receipt sub-line, KEYED BY THE CONFIG FILE the entry lives in
/// (receipts speak in destinations, never agents) — the agent slug keys only an outcome that
/// landed in no file at all (not placed, unreadable). The word for every outcome comes from
/// [`TargetOutcome::word`], the ONE vocabulary the dir side reads too, so a folder and a config
/// entry can never call the same outcome two different things.
///
/// `row_wrote` is whether THIS row wrote any config file this run: beside a created/refreshed
/// line, a file left alone reads `unchanged` — the answer to the question the written line just
/// raised — where on a row that wrote nothing the same outcome reads `current`, a standing fact
/// about the file rather than a comparison with a sibling.
pub(crate) fn mcp_agent_line(h: &topos_types::results::McpAgentState, row_wrote: bool) -> String {
    use topos_types::results::TargetOutcome;
    let key = h.file.as_deref().unwrap_or(&h.agent);
    match (h.state, h.note.as_deref()) {
        (TargetOutcome::Current, _) if row_wrote => format!("{key}: unchanged"),
        (TargetOutcome::Current, None) => format!("{key}: current"),
        // Drift's own sentence IS the word plus what it means for the file; a note would repeat it.
        (TargetOutcome::Drifted, _) => format!("{key}: hand-edited — left in place"),
        (state, Some(note)) => format!("{key}: {} — {note}", state.word()),
        (state, None) => format!("{key}: {}", state.word()),
    }
}

/// One non-up-to-date skill's line (after the padded name) + any indented detail lines — the
/// action's own line, plus the row's SECOND fact when it carries one (`also installed …` for a
/// folder healed beside a draft fan-out, `also updated …` for a stale copy caught up beside a
/// fresh install). A `released` row states its whole fact in `note` and never doubles it.
///
/// A note that spans lines becomes one detail line each, so a fact that needs a list (the escape's
/// collapsed copies) is indented like every other detail rather than trailing off the first line.
fn pull_row(s: &PullSkill, scope: &PullReceiptScope) -> (String, Vec<String>) {
    let (line, mut extra) = pull_action_row(s, scope);
    if s.action != topos_types::results::PullAction::Released
        && let Some(note) = &s.note
    {
        extra.extend(note.lines().map(str::to_owned));
    }
    (line, extra)
}

/// The action's own line + its detail lines (see [`pull_row`], which adds the row's second fact).
/// `scope` spells the next commands for the set this row belongs to (see [`row_takes_g`]).
fn pull_action_row(s: &PullSkill, scope: &PullReceiptScope) -> (String, Vec<String>) {
    use topos_types::results::{MergeResolution, PullAction};
    let name = &s.skill;
    match s.action {
        // Handled by the caller's compact summary — unless the person has unshared edits in it,
        // which is the one thing an "up to date" row must not leave unsaid: nothing was OWED,
        // and something of theirs is still only here. `list` and `status` both say it, and a
        // receipt that did not was the third surface disagreeing about one machine. `publish`
        // resolves the name across scopes itself, so this command takes no `-g`.
        PullAction::UpToDate if s.draft => (
            format!("up to date — your edits are not shared yet (topos publish {name})"),
            Vec::new(),
        ),
        PullAction::UpToDate => (String::from("up to date"), Vec::new()),
        PullAction::FastForwarded => (String::from("fast-forwarded"), Vec::new()),
        // The destination column: exactly one destination prints its path; several print a
        // count in the bundle's own noun (folders for a skill, config files for an MCP server),
        // and then spell them out beneath — a bare `(2 folders)` is a number a person cannot act
        // on, and that is as true of what arrived as of what was caught up. Never an agent name,
        // never an agent count.
        PullAction::Installed => (destination_column("installed", s), sub_destinations(s)),
        // A copy that stood behind the version this machine holds was caught up — the folder was
        // already there, so the word is never `installed`. Its destinations are every placement
        // that NOW holds the applied version, not only the ones this run rewrote, so the count is
        // an answer to "where is it?" rather than to "what did the disk do?"; two or more get a
        // line each, because a bare `(2 folders)` is a number a person cannot act on.
        PullAction::Refreshed => (destination_column("updated", s), sub_destinations(s)),
        // A removal names the folders it emptied for the same reason: "2 folders" is not a place
        // anybody can go and look.
        PullAction::Removed => (destination_column("removed", s), sub_destinations(s)),
        PullAction::Withdrawn => (
            format!(
                "withdrawn upstream — agent dirs cleaned; your copy + drafts are kept locally (run \
                 `topos add {name} --yes` to keep it as yours)"
            ),
            Vec::new(),
        ),
        // The one-time orphan resolution's closing statement — the whole row IS the carried fact
        // (why the record resolved, and then where its files stand), composed by the sweep. Its
        // second sentence is its own line, indented under the first like any other detail.
        PullAction::Released => {
            let fact = s.note.clone().unwrap_or_default();
            let mut lines = fact.lines();
            let head = lines.next().unwrap_or_default().to_owned();
            (head, lines.map(str::to_owned).collect())
        }
        // THREE outcomes share this word and say different things about whose wording landed, so
        // none of them may share a sentence. Both exits from a STOPPED merge are named by the row's
        // own `resolved` — never inferred from a `drop_diff`, which both of them carry.
        //
        // The `-X ours` exit settled the lines the two sides both changed on this person's side and
        // took everything else the team wrote, so the row says both halves and names the files the
        // rest came in on. That list is the diffstat against this person's own side, so a file they
        // collided in appears in it too — the second line therefore reads "everything else the team
        // changed, IN <n> files", never "their other changes TO <n> files", which a reader takes as
        // "<n> other files" and then cannot square with the line above it.
        PullAction::Merged
            if s.merge.as_ref().and_then(|m| m.resolved) == Some(MergeResolution::KeepMine) =>
        {
            let took = s.merge.as_ref().map_or(&[][..], |m| m.took.as_slice());
            // The destination convention, over files: one is named inline, several are counted and
            // then spelled out — a bare "3 files" is not something a person can go and read.
            let mut extra = match took {
                [] => Vec::new(),
                [one] => vec![format!("took everything else the team changed, in {one}")],
                many => {
                    let mut lines = vec![format!(
                        "took everything else the team changed, in {} files:",
                        many.len()
                    )];
                    lines.extend(many.iter().map(|p| format!("  {p}")));
                    lines
                }
            };
            extra.push(ordinary_draft_line(name));
            (
                "kept your wording where you both changed the same lines".to_owned(),
                extra,
            )
        }
        // The hand-resolution exit. topos chose NOTHING here — it committed the tree the person
        // left in the folder, unexamined — so the row must not claim their wording won the
        // contested lines (they may well have taken the team's), and it names no files as taken
        // from the team.
        PullAction::Merged
            if s.merge.as_ref().and_then(|m| m.resolved) == Some(MergeResolution::ByHand) =>
        {
            (
                "committed the merge you made by hand, exactly as you left it".to_owned(),
                vec![ordinary_draft_line(name)],
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
        // The ONE row that asks for a decision, so it carries the whole situation in the order a
        // person needs it: what landed, that their agents read the same bytes they read before it,
        // where both versions are marked up for a by-hand merge, the two ways out — each named
        // right under the sentence that describes what it does — and the one consequence of
        // deciding neither. Every path and every command here is runnable as printed.
        PullAction::Conflicted => {
            let v = s
                .merge
                .as_ref()
                .map(|m| short(&m.theirs_version_id))
                .unwrap_or("?");
            let g = if row_takes_g(s, scope) { " -g" } else { "" };
            let mut extra: Vec<String> = Vec::new();
            match s.merge.as_ref().map(|m| m.placements.as_slice()) {
                // No folder holds it any more. The reassuring line is the ONE thing this row must
                // not say by omission: a reader who saw "your agents are unaffected" last time
                // would read its absence as nothing having changed. Both exits below re-materialize
                // the bytes, so the fact is stated and the row's own commands carry the fix.
                Some([]) => extra.push(
                    "no agent folder holds this skill right now — either way out below puts it \
                     back"
                        .to_owned(),
                ),
                // The reassurance is only reassuring while it is TRUE of every folder — and it is
                // not, once a `--dest` reset has taken one copy or the person has kept working in
                // another. So the aggregate sentence is spoken only over a set that is wholly this
                // person's version: one folder named inline, several counted and then spelled out
                // (a bare "2 folders" is not a place anyone can look).
                Some(all) if all.iter().all(|p| p.holds == ConflictHolds::Yours) => match all {
                    [one] => extra.push(format!(
                        "your agents are unaffected — {} still holds your version",
                        one.dir
                    )),
                    many => {
                        extra.push(format!(
                            "your agents are unaffected — {} folders still hold your version:",
                            many.len()
                        ));
                        extra.extend(many.iter().map(|p| format!("  {}", p.dir)));
                    }
                },
                // A MIXED set — no sentence can speak for all of it, so none tries: one line per
                // folder, each saying what is in that folder and nothing about any other. Each
                // says only what the bytes prove: a folder holding the team's version got there
                // by a reset in every case topos knows of, but a person can put the team's
                // version somewhere themselves, and a line that states the cause states a guess.
                // The marked-up tree gets its own line for the opposite reason: it is the one
                // thing in a folder that is nobody's version, so calling it "your newer edits"
                // would hand a person topos's own writing as their work.
                Some(mixed) => {
                    // The one lead-in a mixed set can honestly carry: it says what the lines
                    // under it ARE, and claims nothing about any folder. Without it the list
                    // starts mid-sentence, under a line about the team's version, and a reader
                    // has to infer that these are their own folders being described.
                    extra.push("what each folder holds:".to_owned());
                    extra.extend(mixed.iter().map(|p| match p.holds {
                        ConflictHolds::Yours => format!("  {} still holds your version", p.dir),
                        ConflictHolds::Theirs => format!("  {} holds the team's version", p.dir),
                        ConflictHolds::MarkedUp => format!(
                            "  {} holds both versions marked up — run one of the ways out below",
                            p.dir
                        ),
                        ConflictHolds::NewerEdits => format!("  {} holds your newer edits", p.dir),
                    }));
                }
                // No merge report at all: nothing provable about disk, so nothing claimed.
                None => {}
            }
            let workbench = s.merge.as_ref().and_then(|m| m.copy_dir.as_deref());
            if let Some(dir) = workbench {
                // The no-base fallback has no shared fork point, so it writes no markers — the
                // workbench holds your files with the team's beside them instead, and the line
                // names that convention rather than sending a reader to look for markers that
                // are not there. Discriminated on the RECORDED reason, so a re-disclosed block
                // says the same thing the first one did.
                if s.merge.as_ref().and_then(|m| m.reason)
                    == Some(topos_types::persisted::ConflictReason::NoBase)
                {
                    extra.push(
                        "to merge by hand, your files are here with the team's beside them \
                         (.topos-theirs):"
                            .to_owned(),
                    );
                } else {
                    extra.push("to merge by hand, both versions are marked up here:".to_owned());
                }
                extra.push(format!("  {dir}/"));
            }
            // The lead-in for the first exit points AT the workbench line above it — so it is
            // only written when that line printed. With no folder named, "that folder" referred
            // to nothing, and the exit says what it does on its own instead.
            extra.push(if workbench.is_some() {
                "commit your merge — or keep your version, by leaving that folder alone:".to_owned()
            } else {
                "keep your version:".to_owned()
            });
            extra.push(format!("  topos update{g} {name} --keep-mine"));
            extra.push("take the team's version instead, dropping yours:".to_owned());
            extra.push(format!("  topos update{g} {name} --reset"));
            extra.push("you cannot publish this skill until you pick one".to_owned());
            // WHY it stopped, in the reader's own terms — read off the RECORDED reason, exactly as
            // the by-hand line above is. Unrelated histories compared no line at all (there is no
            // fork point to compare from), so the lead may not say lines were changed twice; it
            // says the one thing that IS true of that state.
            let lead = if s.merge.as_ref().and_then(|m| m.reason)
                == Some(topos_types::persisted::ConflictReason::NoBase)
            {
                format!("the team published {v}, and it shares no history with your copy")
            } else {
                format!("the team published {v}, and it changes lines you also changed")
            };
            (lead, extra)
        }
        PullAction::DraftSynced => {
            let n = s.synced_placements.unwrap_or(0);
            let line = match s.destinations.as_slice() {
                // One landing names its path — the destination convention.
                [one] if n <= 1 => format!("synced your edits to {one}"),
                _ if n == 1 => "synced your edits to 1 other folder".to_owned(),
                _ => format!("synced your edits to {n} other folders"),
            };
            (line, Vec::new())
        }
        PullAction::Held => (
            format!(
                "held — pinned at a local go-back; run `topos update {name}` to resume following current"
            ),
            Vec::new(),
        ),
    }
}

/// The destination column of an installed/removed row: exactly ONE destination prints its path
/// (`installed (~/.codex/skills)`), several print a count in the bundle's own noun (`installed
/// (2 folders)` / `(2 config files)`), none prints the bare verb.
fn destination_column(verb: &str, s: &PullSkill) -> String {
    let subject = crate::actions::Subject::of_kind(s.kind.as_deref());
    // A COUNT HEADS THE LIST IT HEADS. A config-placed row's detail lines are its per-agent
    // lines — one per config file, whatever happened in each — while `destinations` holds only
    // the files this run WROTE. Counting the written subset printed "(5 config files)" above six
    // lines, and the reader was left to work out which of the two numbers was the lie.
    let listed = if s.harnesses.is_empty() {
        s.destinations.len()
    } else {
        s.harnesses.len()
    };
    match (listed, s.destinations.as_slice()) {
        (0, _) => verb.to_owned(),
        // One target names its path inline; the detail lines below then add nothing.
        (1, [one]) => format!("{verb} ({one})"),
        _ => format!("{verb} ({})", subject.targets(listed)),
    }
}

/// The destinations a counted row spells out beneath itself — one per line, in the order the
/// placement map holds them. Empty for a row whose column already named its single destination
/// (saying it twice is noise) and for a row with none at all.
///
/// A config-placed (mcp) row that carries per-agent lines spells nothing out here either: those
/// lines name the very same files AND say what happened in each, so the bare list beside them is
/// the same list twice. (A removal carries no per-agent lines and keeps its list.)
fn sub_destinations(s: &PullSkill) -> Vec<String> {
    if !s.harnesses.is_empty() {
        return Vec::new();
    }
    match s.destinations.as_slice() {
        [] | [_] => Vec::new(),
        many => many.to_vec(),
    }
}

/// The kept-line a removal prints for each locally-edited copy left in place — one line per
/// bundle, byte-stable, leading with the PLAIN name (`topos list` takes it back): `kept <name> —
/// <path> is edited (topos list <name>)` (several paths join with ", " and read "are edited").
fn kept_line(name: &str, kept: &[String]) -> String {
    let verb = if kept.len() == 1 {
        "is edited"
    } else {
        "are edited"
    };
    format!(
        "kept {name} — {} {verb} (topos list {name})",
        kept.join(", ")
    )
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

/// The line every `--keep-mine` row closes on. Finishing a stopped merge leaves ONE ordinary commit
/// on top of the team's version, and the thing a person most needs told is that nothing further is
/// asked of it: no extra flag, no extra confirmation, no disclosure — the same `topos publish` as
/// any other draft. (`publish` resolves the name across scopes on its own, so this command takes no
/// `-g`; the row's other commands do.)
fn ordinary_draft_line(name: &str) -> String {
    format!("it is an ordinary draft now — `topos publish {name}` ships it like any other")
}

/// Whether a row's next commands take `-g` — the machine-wide set's spelling. The ROW's own scope
/// answers (`person` is the machine's store, anything else a checkout's path), because one sweep
/// reports both sets; the receipt's scope answers for a producer that stamped none.
fn row_takes_g(s: &PullSkill, scope: &PullReceiptScope) -> bool {
    match s.scope.as_deref() {
        Some("person") => true,
        Some(_) => false,
        None => matches!(scope, PullReceiptScope::Machine),
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
    // AMBIGUITY IS AN ANSWER, not a failure: the statement alone here, and the runnable lines
    // under it from [`err_hint_tty`] — which is the surface that HOLDS the invocation those lines
    // are rebuilt from. No `error:` prefix: the reader is not being told something went wrong,
    // they are being asked which of these they meant.
    //
    // The name this scope already records is the same case — the add receipt's own two lines, in
    // the past tense, with nothing prefixed as though something had failed.
    if let ClientError::AmbiguousSource { .. } | ClientError::AlreadyAdded { .. } = err {
        return safe_message(err);
    }
    // A manifest refusal (a retired spelling, or any grammar fault in a user-authored file)
    // closes with the one line that says the load stopped BEFORE anything moved — the file was
    // only read (the `--json` envelope is untouched: its `message` stays the single-sentence
    // teaching). A refused `-a`/`--dest` selection closes the same way, for the same reason:
    // nothing was read past the argv.
    if let ClientError::ManifestMigration(_)
    | ClientError::ManifestInvalid(_)
    | ClientError::UnknownAgent { .. }
    | ClientError::SelectionRefused(_) = err
    {
        return format!("error: {}\nnothing changed", safe_message(err));
    }
    // The divergent-copies freeze names the copies FIRST — one per line, each with the read that
    // shows what is in it — and only then what to do about them. Choosing is the way out, and a
    // reader cannot choose between folders they have not read; three commands per copy made the
    // decision look like nine. The whole-bundle discard stays last: it is the widest loss on the
    // page, never the first thing to reach for.
    if let ClientError::PlacementsDiverged {
        skill,
        copies,
        global,
    } = err
    {
        // Every command is spelled for the scope the frozen copies live in: a machine-scope freeze
        // says `-g`, or the command, read from inside a checkout, would drive the PROJECT, find no
        // copy there, and refuse. Same discipline as a `list` row's `flag`.
        let flag = crate::error::scope_flag(*global);
        let mut out = format!(
            "error: '{skill}' has different edits in {}:",
            counted(copies.len() as u64, "folder")
        );
        for c in copies {
            out.push_str(&format!(
                "\n  {} — read it: topos diff{flag} {skill} --dest {}",
                c.display, c.dest
            ));
        }
        out.push_str(&format!(
            "\nname the one to work with (--dest <folder> on publish, diff, or update --reset), \
             or discard every copy's edits: topos update{flag} {skill} --reset (each copy is \
             snapshotted first)"
        ));
        return out;
    }
    // A copy behind the served `current` states the fact and then the one command that fixes it,
    // indented under it — the same shape as the refusals below: the copy IS the answer, so it carries
    // no `error:` prefix (the `--json` envelope keeps the single-sentence message, and the same command
    // rides `next_actions`).
    if let ClientError::PublishBehind { skill, global } = err {
        return format!(
            "{}\n  update first: topos update{} {skill}",
            safe_message(err),
            crate::error::scope_flag(*global)
        );
    }
    // The shared-copy narrowing refusal renders its ways out as ALIGNED command lines under the
    // statement — the copy is the answer, so it carries no `error:` prefix (the `--json`
    // envelope keeps the single-sentence message; the same two commands ride `next_actions`).
    if let ClientError::SharedCopyOnly {
        remove_argv,
        readd_argv,
        ..
    } = err
    {
        let remove_line = argv_line(remove_argv);
        let readd_line = argv_line(readd_argv);
        let pad = remove_line.len().max(readd_line.len());
        return format!(
            "{}\n  {remove_line:<pad$}   remove it for every agent\n  {readd_line:<pad$}   keep \
             it per-agent instead, then re-run",
            safe_message(err)
        );
    }
    format!("error: {}", safe_message(err))
}

/// The WAY OUT on the TTY — the same [`next_actions`] the `--json` envelope carries, spelled as
/// runnable command lines under one lead-in. There is no second, hand-written hint table: an
/// action the agent surface offers is exactly the command a human is shown, so the two can never
/// drift apart.
///
/// The lead-in is `try:` everywhere, prefixed with the transience clause when — and only when —
/// the typed [`ClientError::outcome`] says the failure is retryable. The retryable bit is READ,
/// never re-derived here: one classification serves the agent's `retryable` field and this line,
/// so a human and an agent are never told opposite things about the same failure. Actions with an
/// EMPTY argv (`REQUEST_ACCESS`, `CONTACT_ADMIN`, the bare `RETRY`) carry no command to print; a
/// retryable error with nothing runnable still says so, because re-running IS the way out.
/// `None` = there is genuinely nothing to add under the error line.
///
/// A command the error line ALREADY spells in backticks is not repeated underneath it — many
/// refusals teach their own fix in prose (that is how [`mirror_prose_commands`] derives the action
/// in the first place), and printing it twice, two lines apart, reads as two different steps. The
/// block is still built from `next_actions` alone; this only drops what the reader just read.
///
/// `command` and `argv` describe the invocation that refused; both pass straight through to
/// [`next_actions`] — the surfaces stay one computation, so a human and an agent are offered the
/// same commands, and withheld the same ones.
pub(crate) fn err_hint_tty(command: &str, argv: &[String], err: &ClientError) -> Option<String> {
    // The shared-copy refusal's TTY already prints its two ways out as aligned, annotated
    // command lines (see [`err_tty`]) — a `try:` block underneath would repeat them bare. The
    // divergent-copies menu is the same case, one step further: its last line IS the whole-bundle
    // discard the action offers, and repeating it under the menu would read as a fourth option. The
    // behind refusal is the same case at its simplest: it prints its one command already.
    // THE CHOOSER's lines: the same commands `next_actions` carries, under no lead-in at all —
    // they are the answer's own list, not a suggestion appended to a refusal. Each is spelled for
    // a READER: a folder under this home reads as `~/…`, which a shell expands back to exactly the
    // argv the agent surface got. One list, two spellings, never two lists.
    //
    // A FOLDER LISTING answers with its own document (`err_tty` prints it whole, ending in the one
    // command that adopts a folder), so nothing goes under it: re-listing the same folders as
    // commands would be the second listing that shape exists to replace.
    if let ClientError::AmbiguousSource { candidates, .. } = err {
        if crate::error::listed_as_folders(err) {
            return None;
        }
        let lines: Vec<String> = candidates
            .iter()
            .map(|c| {
                format!(
                    "  {}",
                    hint_line(&candidate_command(argv, err, &c.printed_tokens()))
                )
            })
            .collect();
        return (!lines.is_empty()).then(|| lines.join("\n"));
    }
    // The already-added answer carries the same FOLDER LISTING, ending in the same one command —
    // so nothing goes under it either. The per-folder claims stay on the agent surface, where a
    // list of argv is what a consumer wants and nothing has to be read.
    if let ClientError::AlreadyAdded { .. } = err {
        return None;
    }
    if matches!(
        err,
        ClientError::SharedCopyOnly { .. }
            | ClientError::PlacementsDiverged { .. }
            | ClientError::PublishBehind { .. }
    ) {
        return None;
    }
    let retryable = matches!(
        err.outcome(),
        TerminalOutcome::RetryableFailure | TerminalOutcome::Unavailable
    );
    let message = safe_message(err);
    let already_said = backtick_spans(&message);
    let mut lines: Vec<String> = Vec::new();
    for action in next_actions(command, argv, err) {
        let line = hint_line(&action.argv);
        if line.is_empty() || lines.contains(&line) || already_said.iter().any(|s| *s == line) {
            continue;
        }
        lines.push(line);
    }
    // An ambiguity whose rebuilt commands were withheld (an unfaithful verb, a multi-target
    // invocation) still SHOWS its ways out — as spellings, never offers: the person re-spells
    // their own invocation around one. "More than one answers" with no list would be a refusal
    // to say which. The spellings are byte-identical to the envelope's `data.candidates`.
    if lines.is_empty()
        && let ClientError::AmbiguousTarget { candidates, .. } = err
        && !candidates.is_empty()
    {
        let list: Vec<String> = candidates.iter().map(|c| c.spelling()).collect();
        return Some(format!("it matches:\n  {}", list.join("\n  ")));
    }
    match (retryable, lines.is_empty()) {
        (true, true) => Some("this is often transient — running it again is safe".to_owned()),
        (false, true) => None,
        (transient, false) => {
            let lead = if transient {
                "this is often transient — try:"
            } else {
                "try:"
            };
            Some(format!("{lead}\n  {}", lines.join("\n  ")))
        }
    }
}

/// One next-action argv as the human line the hint block prints: the machine surface's `--json`
/// tail is dropped (a person running this wants the readable output, and the flag is the agent
/// half of the same action), and each remaining token is [`shell_quote`]d so a value carrying
/// whitespace copy-pastes back as ONE argument. A `<placeholder>` token — which only ever comes
/// from the compile-time template table, never a runtime value — stays bare, so the line reads as
/// the fill-in-the-blank it is. Empty argv (an action with no command) yields an empty line.
/// Every backtick-quoted span in a message — the commands a refusal's prose already taught, so the
/// hint block below it can stay silent about them.
fn backtick_spans(message: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = message;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else { break };
        out.push(&after[..end]);
        rest = &after[end + 1..];
    }
    out
}

fn hint_line(argv: &[String]) -> String {
    let tokens: Vec<String> = argv
        .iter()
        .filter(|t| t.as_str() != "--json")
        .map(|t| {
            if is_placeholder(t) {
                t.clone()
            } else {
                shell_quote(t)
            }
        })
        .collect();
    tokens.join(" ")
}

/// A bare `<name>` template hole — the one token shape the hint line leaves unquoted.
fn is_placeholder(token: &str) -> bool {
    token.starts_with('<')
        && token.ends_with('>')
        && token.len() > 2
        && token[1..token.len() - 1]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A version id as every human surface spells it: the 12 chars that resolve everywhere a prefix is
/// accepted, so a version read off a receipt can be typed straight back into a command.
pub(crate) fn short(hex: &str) -> &str {
    hex.get(..12).unwrap_or(hex)
}

#[cfg(test)]
mod tests {
    use topos_types::persisted::ConflictPathKind;
    use topos_types::results::{
        AgentView, BehindElsewhere, ConflictHolds, ConflictPathReport, ListData, LogData,
        MergeReport, MergeResolution, ProposeData, PublishData, PullAction, PullData, PullSkill,
        ReceiptScope, RemoteSkill, RemoveData, RemoveItem, RemoveKind, SkillEntry, UntrackedEntry,
    };

    use crate::ops::ListOutcome;

    use super::{
        add_tty, auth_status_next_actions, auth_status_tty, list_tty, log_tty, propose_tty,
        publish_tty, pull_row, pull_tty, remove_applied_tty, resolution_next_actions, safe_message,
        status_tty, welcome_tty,
    };

    /// A synthetic invocation for the render tests: what a user typed past the binary name.
    fn typed(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| (*t).to_owned()).collect()
    }

    /// One copy of a placement freeze, spelled the way the producer spells it: the display path is
    /// the skill DIR, the `--dest` value the folder holding it.
    fn diverged_copy(dir: &str) -> crate::error::DivergedCopy {
        crate::error::DivergedCopy {
            display: dir.to_owned(),
            dest: std::path::Path::new(dir)
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        }
    }

    /// A retired-manifest-spelling refusal's TTY shows the teaching VERBATIM and closes with the
    /// one line that says the load stopped before anything moved — while an ordinary corrupt
    /// state keeps its redacted single line.
    #[test]
    fn a_manifest_migration_refusal_closes_the_tty_with_nothing_changed() {
        let err = crate::error::ClientError::ManifestMigration(
            "./topos.toml: `harness = [\"codex\"]` on \"topos.sh/acme/linear\" is now written \
             as dest — use dest = [\"~/.codex/config.toml\"]"
                .to_owned(),
        );
        let tty = super::err_tty(&err);
        assert_eq!(tty.lines().last(), Some("nothing changed"), "{tty}");
        assert!(
            tty.contains("use dest = [\"~/.codex/config.toml\"]"),
            "{tty}"
        );
        // The envelope's message stays the single-sentence teaching (no closing line).
        assert!(!safe_message(&err).contains("nothing changed"));
        // The GRAMMAR refusal family closes the same way, verbatim: a dialect fault names the
        // file, the entry, and the rule on both surfaces (never the corrupt-state fixed line).
        let err = crate::error::ClientError::ManifestInvalid(
            "~/.topos/topos.toml: dest entry `skills` is relative — the machine-wide file names \
             machine paths: `~/`-prefixed or absolute"
                .to_owned(),
        );
        assert_eq!(err.code(), "MANIFEST_INVALID");
        assert!(safe_message(&err).contains("dest entry `skills` is relative"));
        let tty = super::err_tty(&err);
        assert_eq!(tty.lines().last(), Some("nothing changed"), "{tty}");
        assert!(tty.contains("~/.topos/topos.toml"), "{tty}");
        // The ordinary corrupt refusal is untouched.
        let plain = super::err_tty(&crate::error::ClientError::Corrupt("x".into()));
        assert!(!plain.contains("nothing changed"), "{plain}");
    }

    /// A diff names WHICH copy it read only where more than one exists — the producer populates
    /// the pair on exactly that condition. A single-copy diff is byte-identical to what it always
    /// printed, which is why no golden fixture moves.
    #[test]
    fn a_diff_names_its_source_copy_only_when_there_is_more_than_one() {
        use topos_types::results::{DiffData, DiffSource};
        let body = "--- a/SKILL.md\n+++ b/SKILL.md\n@@ -1 +1 @@\n-old\n+new\n";
        let plain = DiffData {
            source: DiffSource::Local,
            version_id: format!("f154315d5fd9{}", "0".repeat(52)),
            bundle_digest: "cd".repeat(32),
            diff: body.to_owned(),
            truncated: false,
            files: Vec::new(),
            dest: None,
            skill: None,
        };
        assert_eq!(super::diff_tty(&plain), body.trim_end());

        let named = DiffData {
            dest: Some("project/.agents/skills/coolify-deploy".to_owned()),
            skill: Some("coolify-deploy".to_owned()),
            ..plain
        };
        assert_eq!(
            super::diff_tty(&named),
            format!(
                "coolify-deploy — project/.agents/skills/coolify-deploy vs applied \
                 f154315d5fd9\n{}",
                body.trim_end()
            )
        );
    }

    /// NOTHING TO SHOW still says which copy was read. "No changes" is a claim about ONE folder,
    /// and a bundle sitting in several that answered it bare left the reader unable to tell which
    /// of them is the clean one. A single-copy diff keeps its old, headerless answer.
    #[test]
    fn a_clean_diff_of_one_copy_among_several_still_names_the_copy() {
        use topos_types::results::{DiffData, DiffSource};
        let clean = DiffData {
            source: DiffSource::Local,
            version_id: format!("f154315d5fd9{}", "0".repeat(52)),
            bundle_digest: "cd".repeat(32),
            diff: String::new(),
            truncated: false,
            files: Vec::new(),
            dest: None,
            skill: None,
        };
        assert_eq!(
            super::diff_tty(&clean),
            "No changes — the draft matches current."
        );
        let named = DiffData {
            dest: Some("project/.agents/skills/coolify-deploy".to_owned()),
            skill: Some("coolify-deploy".to_owned()),
            ..clean
        };
        assert_eq!(
            super::diff_tty(&named),
            "coolify-deploy — project/.agents/skills/coolify-deploy vs applied f154315d5fd9\nNo \
             changes — the draft matches current."
        );
    }

    /// The divergent-copies freeze is byte-exact: the copies first, one per line, each with the
    /// read that shows what is in it — because choosing is the way out and nobody chooses between
    /// folders they have not read — then one sentence for what to do, ending at the discard that
    /// takes every copy's edits. That discard is last on purpose: it is the widest loss on the
    /// page, and it used to be the only exit offered.
    #[test]
    fn the_divergent_copies_refusal_is_a_per_copy_menu() {
        let err = crate::error::ClientError::PlacementsDiverged {
            skill: "coolify-deploy".to_owned(),
            copies: vec![
                crate::error::DivergedCopy {
                    display: "project/.agents/skills/coolify-deploy".to_owned(),
                    dest: ".agents/skills".to_owned(),
                },
                crate::error::DivergedCopy {
                    display: "project/.claude/skills/coolify-deploy".to_owned(),
                    dest: ".claude/skills".to_owned(),
                },
            ],
            global: false,
        };
        assert_eq!(
            super::err_tty(&err),
            "error: 'coolify-deploy' has different edits in 2 folders:\n  \
             project/.agents/skills/coolify-deploy — read it: topos diff coolify-deploy --dest \
             .agents/skills\n  project/.claude/skills/coolify-deploy — read it: topos diff \
             coolify-deploy --dest .claude/skills\nname the one to work with (--dest <folder> on \
             publish, diff, or update --reset), or discard every copy's edits: topos update \
             coolify-deploy --reset (each copy is snapshotted first)"
        );
        // The refusal IS the answer, so nothing is printed under it — a `try:` block would
        // re-offer the whole-bundle discard the last line already names, two lines apart.
        assert!(
            super::err_hint_tty("publish", &typed(&["publish", "coolify-deploy"]), &err).is_none()
        );
        // The `--json` message stays ONE sentence, and still names the folders — an agent reading
        // the envelope must not be told "two folders" without being told which.
        let message = safe_message(&err);
        assert_eq!(message.lines().count(), 1, "{message}");
        assert!(
            message.contains("project/.agents/skills/coolify-deploy")
                && message.contains("project/.claude/skills/coolify-deploy"),
            "{message}"
        );
        // Every command in the one-sentence message is RUNNABLE as printed — no elided middle a
        // person or an agent has to reconstruct.
        assert!(!message.contains('…'), "{message}");
    }

    /// A MACHINE-scope freeze spells every command it offers with `-g` — on the refusal and in the
    /// one-sentence `--json` message alike. A bare `update` or `diff`, read from inside any
    /// checkout, drives the PROJECT: it would find no copy of this bundle there and refuse, which
    /// is exactly the dead end the refusal exists to replace.
    #[test]
    fn a_machine_scope_freeze_spells_its_updates_with_the_scope_flag() {
        let err = crate::error::ClientError::PlacementsDiverged {
            skill: "coolify-deploy".to_owned(),
            copies: vec![
                crate::error::DivergedCopy {
                    display: "~/.agents/skills/coolify-deploy".to_owned(),
                    dest: "~/.agents/skills".to_owned(),
                },
                crate::error::DivergedCopy {
                    display: "~/.claude/skills/coolify-deploy".to_owned(),
                    dest: "~/.claude/skills".to_owned(),
                },
            ],
            global: true,
        };
        let tty = super::err_tty(&err);
        assert!(
            tty.contains(
                "~/.agents/skills/coolify-deploy — read it: topos diff -g coolify-deploy --dest \
                 ~/.agents/skills"
            ),
            "{tty}"
        );
        assert!(
            tty.ends_with(
                "or discard every copy's edits: topos update -g coolify-deploy --reset (each copy \
                 is snapshotted first)"
            ),
            "{tty}"
        );
        let message = safe_message(&err);
        assert!(
            message.contains("topos update -g coolify-deploy --reset"),
            "{message}"
        );
    }

    /// However many copies compete, the menu ends at the whole-bundle discard: it names the copies
    /// and what can be done to each, and predicts nothing about the aftermath. A sentence about
    /// what resolving one would leave has to be right for two copies AND for ten, and the next
    /// `list` says it better than a prediction can.
    #[test]
    fn the_menu_predicts_nothing_about_what_resolving_one_copy_leaves() {
        let copy = |n: &str| crate::error::DivergedCopy {
            display: format!("project/.{n}/skills/coolify-deploy"),
            dest: format!(".{n}/skills"),
        };
        for count in [2usize, 3, 4] {
            let copies: Vec<_> = ["agents", "claude", "cursor", "gemini"][..count]
                .iter()
                .map(|n| copy(n))
                .collect();
            let err = crate::error::ClientError::PlacementsDiverged {
                skill: "coolify-deploy".to_owned(),
                copies,
                global: false,
            };
            let tty = super::err_tty(&err);
            assert!(!tty.contains("single draft"), "{count}: {tty}");
            assert!(!tty.contains("still disagree"), "{count}: {tty}");
            // One copy line each, then the one sentence that ends at the whole-bundle way out.
            assert_eq!(tty.lines().count(), count + 2, "{count}: {tty}");
            assert!(
                tty.ends_with(
                    "or discard every copy's edits: topos update coolify-deploy --reset (each \
                     copy is snapshotted first)"
                ),
                "{count}: {tty}"
            );
        }
    }

    /// **A permanent delete names the folder it will empty, before consent.** The `--yes` gate is
    /// the whole disclosure for an act with no inverse, and a describe that said only "Would delete
    /// 'demo' PERMANENTLY" asked a person to approve bytes it declined to show — while the APPLY
    /// receipt afterwards named the path. Both tenses name it now, and both permanent shapes do.
    #[test]
    fn a_permanent_delete_names_its_folders_in_both_tenses() {
        let item = |kind| RemoveItem {
            name: "demo".to_owned(),
            kind,
            manifest: None,
            workspace_id: None,
            agent_dirs: vec!["~/work/demo".to_owned()],
            kept_dirs: Vec::new(),
            bytes_kept: false,
            note: None,
        };
        for kind in [
            RemoveKind::TrackedLocalPermanent,
            RemoveKind::UntrackedLocal,
        ] {
            let describe = super::remove_item_line(&item(kind), false);
            assert!(
                describe.starts_with("Would delete 'demo' from ~/work/demo")
                    && describe.contains("PERMANENTLY"),
                "the gate names the folder AND keeps the permanence teaching: {describe}"
            );
            let applied = super::remove_item_line(&item(kind), true);
            assert!(
                applied.starts_with("Deleted 'demo' from ~/work/demo"),
                "{applied}"
            );
        }
        // A removal that names no folder says nothing about one — no dangling `from`.
        let mut bare = item(RemoveKind::TrackedLocalPermanent);
        bare.agent_dirs = Vec::new();
        assert!(
            super::remove_item_line(&bare, false).starts_with("Would delete 'demo' PERMANENTLY"),
            "{}",
            super::remove_item_line(&bare, false)
        );
    }

    /// An APPLIED whole-row removal whose uninstall moved nothing must not claim the copies
    /// leave — the line states the row removal alone. The describe (a plan, not a claim) and an
    /// item with verifiably cleaned dirs keep the full sentence.
    #[test]
    fn an_applied_row_removal_with_no_uninstall_drops_the_copies_leave_claim() {
        let item = |agent_dirs: Vec<String>| RemoveItem {
            name: "deploy".to_owned(),
            kind: RemoveKind::ManifestRemoved,
            manifest: Some("~/.topos/topos.toml".to_owned()),
            workspace_id: None,
            agent_dirs,
            kept_dirs: Vec::new(),
            bytes_kept: true,
            note: None,
        };
        let data = |items: Vec<RemoveItem>| RemoveData {
            items,
            applied: true,
            undo: Vec::new(),
            uninstalled: Vec::new(),
        };
        // Applied, nothing uninstalled: the row removal alone.
        assert_eq!(
            remove_applied_tty(&data(vec![item(Vec::new())])),
            "~/.topos/topos.toml: removed 'deploy'."
        );
        // Applied with cleaned dirs on the item itself: the full sentence stands.
        let with_dirs = remove_applied_tty(&data(vec![item(vec!["~/.claude/skills".into()])]));
        assert!(
            with_dirs.contains("the copies it placed leave this machine now"),
            "{with_dirs}"
        );
        // The describe keeps the plan's sentence.
        let describe = super::remove_describe_tty(
            &RemoveData {
                items: vec![item(Vec::new())],
                applied: false,
                undo: Vec::new(),
                uninstalled: Vec::new(),
            },
            &[
                "topos".into(),
                "remove".into(),
                "deploy".into(),
                "--yes".into(),
            ],
        );
        assert!(
            describe.contains("the copies it placed leave this machine now"),
            "{describe}"
        );
    }

    /// A LOCAL folder adopted with `-a`/`--dest` states its destination like every other
    /// selected add. The receipt used to be gated on the workspace-qualified `display`, which a
    /// local source never has — so `topos add ./my-skill -a codex` answered with a bare
    /// `Adopted 'my-skill' @ <hash>` and never said where the copy went, the one thing the
    /// person had just asked for. The plain name carries the row; the column follows the same
    /// convention as everywhere else (one destination prints its path, several a count).
    #[test]
    fn a_local_add_with_a_destination_says_where_the_copy_went() {
        let local = |dest: Vec<String>| topos_types::results::AddData {
            skill_id: Some("topos_a9b7ee2b".to_owned()),
            name: "my-skill".to_owned(),
            version_id: Some("a".repeat(64)),
            bundle_digest: Some("b".repeat(64)),
            tracked: true,
            harness: None,
            harness_slug: None,
            currency: None,
            triggers: Vec::new(),
            origin: None,
            source: Some("/home/u/work/my-skill".to_owned()),
            manifest: Some("/home/u/.topos/topos.toml".to_owned()),
            scope: Some(ReceiptScope::Machine),
            reference: Some("~/work/my-skill".to_owned()),
            undo: vec![
                "topos".to_owned(),
                "remove".to_owned(),
                "-g".to_owned(),
                "my-skill".to_owned(),
            ],
            governed_copy: None,
            published_match: None,
            note: None,
            mcp: None,
            dest,
            dest_resolved: Vec::new(),
            dest_change: None,
            claim: None,
            unchanged: false,
            machine_copy: None,
            // The local source's tell: no workspace qualifies it.
            display: None,
        };
        let one = add_tty(&local(vec!["~/.codex/skills".to_owned()]));
        assert_eq!(
            one,
            "+ my-skill   installed (~/.codex/skills)\n\
             source: /home/u/work/my-skill\n\
             (undo: topos remove -g my-skill)"
        );
        // Several destinations count, in the skill's own noun.
        let many = add_tty(&local(vec![
            "~/.codex/skills".to_owned(),
            "~/.claude/skills".to_owned(),
        ]));
        assert!(
            many.contains("+ my-skill   installed (2 folders)"),
            "{many}"
        );
        // A selection-free adopt takes the ordinary lead — what was added, which file asks for it
        // now, and where it came from — and never claims a destination nobody named.
        let bare = add_tty(&local(Vec::new()));
        assert_eq!(
            bare,
            "added my-skill machine-wide (~/.topos/topos.toml) (undo: topos remove -g my-skill)\n\
             source: /home/u/work/my-skill"
        );

        // AN ENV OVERRIDE MOVED THE FILE. `dest` stays the row's portable spelling — that is what
        // belongs in a manifest — but the headline follows the BYTES. It used to print the
        // default, naming a path the reader could go and look at and find nothing, while the note
        // right beneath it named the real one.
        let mut moved = local(vec!["~/.codex/config.toml".to_owned()]);
        moved.dest_resolved = vec!["/opt/codex/config.toml".to_owned()];
        let text = add_tty(&moved);
        assert!(
            text.contains("+ my-skill   installed (/opt/codex/config.toml)"),
            "{text}"
        );
        assert!(!text.contains("~/.codex/config.toml"), "{text}");
        // Two moved files still count in the noun their kind owns.
        let mut moved_many = local(vec![
            "~/.codex/config.toml".to_owned(),
            "~/.cursor/mcp.json".to_owned(),
        ]);
        moved_many.mcp = Some(server_summary());
        moved_many.dest_resolved = vec![
            "/opt/codex/config.toml".to_owned(),
            "/opt/cursor/mcp.json".to_owned(),
        ];
        assert!(
            add_tty(&moved_many).contains("+ my-skill   installed (2 config files)"),
            "{}",
            add_tty(&moved_many)
        );

        // A row that ALREADY stood gained a destination — it did not arrive here for the first
        // time, so the change is the answer and `+ … installed (…)` never appears.
        let mut extended = local(vec!["~/.cursor/skills".to_owned()]);
        extended.name = "coolify-deploy".to_owned();
        extended.source = Some("topos.sh/ideamotive/coolify-deploy".to_owned());
        extended.undo = vec![
            "topos".to_owned(),
            "remove".to_owned(),
            "-g".to_owned(),
            "coolify-deploy".to_owned(),
            "--dest".to_owned(),
            "~/.cursor/skills".to_owned(),
        ];
        extended.dest_change = Some(topos_types::results::DestChange {
            added: vec!["~/.cursor/skills".to_owned()],
            frozen: Vec::new(),
        });
        assert_eq!(
            add_tty(&extended),
            "added ~/.cursor/skills to coolify-deploy's destinations\n\
             source: topos.sh/ideamotive/coolify-deploy\n\
             (undo: topos remove -g coolify-deploy --dest ~/.cursor/skills)"
        );

        // The FIRST destination on a row that had reached every agent says what it froze, one
        // folder per line — otherwise nobody could tell what quietly stopped being implied.
        let mut frozen = extended.clone();
        frozen.dest_change = Some(topos_types::results::DestChange {
            added: vec!["~/.codex/skills".to_owned()],
            frozen: vec![
                "~/.agents/skills".to_owned(),
                "~/.claude/skills".to_owned(),
                "~/.codex/skills".to_owned(),
            ],
        });
        assert!(
            add_tty(&frozen).starts_with(
                "coolify-deploy reached every agent — recorded its current destinations plus the \
                 new one:\n  ~/.agents/skills\n  ~/.claude/skills\n  ~/.codex/skills\n"
            ),
            "{}",
            add_tty(&frozen)
        );

        // THE SECOND CHECKOUT: a machine-wide add of a bundle the project already delivers says
        // so, and names the folder its own copy landed in — the same line on the plain receipt
        // and on the one that named destinations, because it is a fact about neither.
        let mut beside = local(Vec::new());
        beside.name = "coolify-deploy".to_owned();
        beside.machine_copy = Some("/home/u/.claude/skills/coolify-deploy".to_owned());
        assert!(
            add_tty(&beside).ends_with(
                "this project already delivers 'coolify-deploy' — this adds your machine copy \
                 (/home/u/.claude/skills/coolify-deploy)"
            ),
            "{}",
            add_tty(&beside)
        );
        let mut beside_dest = local(vec!["~/.codex/skills".to_owned()]);
        beside_dest.name = "coolify-deploy".to_owned();
        beside_dest.machine_copy = Some("/home/u/.claude/skills/coolify-deploy".to_owned());
        assert!(
            add_tty(&beside_dest).contains(
                "\nthis project already delivers 'coolify-deploy' — this adds your machine copy \
                 (/home/u/.claude/skills/coolify-deploy)\n"
            ),
            "{}",
            add_tty(&beside_dest)
        );
        // No second copy, no line — the add that creates one is the only one that says anything.
        assert!(!add_tty(&local(Vec::new())).contains("already delivers"));
    }

    /// EVERY add answer opens the same way: what was added and which of the two files asks for it
    /// now, then where it came from. The lead names the file by its one portable spelling — a
    /// reader's own absolute path is noise — and `source:` carries the reference where a
    /// workspace or a forge governs the bundle, the folder where the bytes are the person's own.
    #[test]
    fn every_add_receipt_leads_with_what_was_added_and_says_where_it_came_from() {
        let base = || topos_types::results::AddData {
            skill_id: Some("topos_a9b7ee2b".to_owned()),
            name: "coolify-deploy".to_owned(),
            version_id: Some("a".repeat(64)),
            bundle_digest: Some("b".repeat(64)),
            tracked: true,
            harness: None,
            harness_slug: None,
            currency: None,
            triggers: Vec::new(),
            origin: None,
            source: Some("/home/u/.claude/skills/gstack/coolify-deploy".to_owned()),
            manifest: Some("/work/api/topos.toml".to_owned()),
            scope: Some(ReceiptScope::Project),
            reference: Some("./tools/coolify-deploy".to_owned()),
            undo: Vec::new(),
            governed_copy: None,
            published_match: None,
            note: None,
            mcp: None,
            dest: Vec::new(),
            dest_resolved: Vec::new(),
            dest_change: None,
            claim: None,
            unchanged: false,
            machine_copy: None,
            display: None,
        };
        // A local folder adopted into this project: the row's own `./…` spelling never surfaces —
        // it means nothing away from the file that holds it.
        assert_eq!(
            add_tty(&base()),
            "added coolify-deploy to this project (./topos.toml)\n\
             source: /home/u/.claude/skills/gstack/coolify-deploy"
        );
        // The machine-wide file has one spelling too, and the inverse rides the same line.
        let machine = topos_types::results::AddData {
            scope: Some(ReceiptScope::Machine),
            source: Some("topos.sh/ideamotive/coolify-deploy".to_owned()),
            reference: Some("topos.sh/ideamotive/coolify-deploy".to_owned()),
            undo: vec![
                "topos".to_owned(),
                "remove".to_owned(),
                "-g".to_owned(),
                "topos.sh/ideamotive/coolify-deploy".to_owned(),
            ],
            ..base()
        };
        assert_eq!(
            add_tty(&machine),
            "added coolify-deploy machine-wide (~/.topos/topos.toml) (undo: topos remove -g \
             topos.sh/ideamotive/coolify-deploy)\n\
             source: topos.sh/ideamotive/coolify-deploy"
        );
        // A SET row has no bytes of its own — the closing sentence still comes after the same two
        // lines, and the source is the reference the row expands.
        let channel = topos_types::results::AddData {
            skill_id: None,
            version_id: None,
            bundle_digest: None,
            name: "backend".to_owned(),
            source: Some("topos.sh/acme/channels/backend".to_owned()),
            reference: Some("topos.sh/acme/channels/backend".to_owned()),
            ..base()
        };
        let text = add_tty(&channel);
        assert!(
            text.starts_with(
                "added backend to this project (./topos.toml)\n\
                 source: topos.sh/acme/channels/backend\n"
            ),
            "{text}"
        );
        assert!(text.contains("Added the 'backend' channel"), "{text}");
    }

    /// A minimal typed `mcp` block — enough for the receipt to know the bundle is a server (which
    /// decides its noun) and that it reached an agent.
    fn server_summary() -> topos_types::results::McpServerSummary {
        topos_types::results::McpServerSummary {
            server: "io.test/wx".to_owned(),
            description: "Weather.".to_owned(),
            version: "1.0.0".to_owned(),
            url: "https://wx.example/mcp".to_owned(),
            transport: "streamable-http".to_owned(),
            auth: None,
            headers: Vec::new(),
            bundle: None,
            agents: vec!["Codex".to_owned(), "Cursor".to_owned()],
        }
    }

    fn row(name: &str, action: PullAction) -> PullSkill {
        PullSkill {
            skill: name.to_owned(),
            workspace_id: None,
            observed: 2,
            applied: 2,
            action,
            merge: None,
            synced_placements: None,
            destinations: Vec::new(),
            kept: Vec::new(),
            display: None,
            note: None,
            scope: None,
            harnesses: Vec::new(),
            kind: None,
            draft: false,
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
            reason: None,
            resolved: None,
            took: Vec::new(),
            placements: Vec::new(),
            copy_dir: None,
        }
    }

    /// The teammate handoff exactly as `ops::publish` composes it — quoted verbatim because the
    /// receipt's job is to be paste-able.
    const INVITE: &str = "Ask your agent: \"Set up Topos for us: fetch https://topos.sh/agent and \
                          follow it. Our workspace: https://topos.sh/ideamotive\"";

    /// A LANDED publish's receipt payload, with every optional line off. The skill's `skill_id` and
    /// `name` deliberately DIFFER: a fixture where they coincide lets an id printed where a name
    /// belongs pass a whole suite.
    fn published() -> PublishData {
        PublishData {
            manifest: None,
            reference: None,
            converted_from: None,
            skill_id: "topos_a1b2c3".to_owned(),
            name: "coolify-deploy".to_owned(),
            version_id: format!("fed180d80b8a{}", "0".repeat(52)),
            bundle_digest: "c".repeat(64),
            current_generation: 3,
            added: None,
            placement_withheld: None,
            placement_missing: None,
            invite_line: None,
            origin_note: None,
            rewrite_pending: None,
            rewrite_skipped: None,
            kind: None,
            workspace_address: None,
            share_line: None,
            undo: None,
            from_placement: None,
            from_machine: false,
            other_scope_draft: None,
            other_edited: Vec::new(),
        }
    }

    /// A publish DESCRIBE payload at `gate`, carrying EVERY optional field the envelope can hold —
    /// so a test of what the TTY prints is also a test of what it deliberately withholds.
    fn describing(
        gate: topos_types::results::PublishGate,
    ) -> topos_types::results::PublishDescribeData {
        topos_types::results::PublishDescribeData {
            skill: "coolify-deploy".to_owned(),
            skill_id: "topos_a1b2c3".to_owned(),
            workspace_id: "w_ideamotive".to_owned(),
            workspace_display_name: Some("Ideamotive".to_owned()),
            workspace_address: Some("topos.sh/ideamotive".to_owned()),
            bundle_digest: "b".repeat(64),
            placements: Vec::new(),
            from_placement: None,
            from_machine: false,
            other_scope_draft: None,
            review: Some("topos diff coolify-deploy".to_owned()),
            other_edited: Vec::new(),
            gate,
            is_revert: false,
            share_line: Some("https://topos.sh/ideamotive/skills/coolify-deploy".to_owned()),
            invite_line: Some(INVITE.to_owned()),
            undo: Some(format!(
                "topos revert coolify-deploy --to {}",
                "a".repeat(64)
            )),
            origin_note: None,
            placement_note: None,
            merge_preview: None,
            manifest: None,
            reference: None,
            converted_from: None,
            kind: None,
        }
    }

    fn yes_argv() -> Vec<String> {
        ["topos", "publish", "coolify-deploy", "--yes"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }

    #[test]
    fn publish_describe_tty_names_the_address_and_says_what_the_gate_will_do() {
        use topos_types::results::PublishGate;
        // The whole describe, both gates: the destination is the workspace ADDRESS, and the gate is
        // stated as the sentence it means for the person about to run `--yes`.
        assert_eq!(
            super::publish_describe_tty(&describing(PublishGate::Lands), &yes_argv()),
            "Publish 'coolify-deploy' to topos.sh/ideamotive:\n  No review required — it becomes \
             immediately published in the workspace.\nreview: topos diff \
             coolify-deploy\nNothing has changed yet — apply with:\n  topos publish \
             coolify-deploy --yes"
        );
        assert_eq!(
            super::publish_describe_tty(&describing(PublishGate::Proposal), &yes_argv()),
            "Publish 'coolify-deploy' to topos.sh/ideamotive:\n  Review required — will require \
             approval by a reviewer after publishing.\nreview: topos diff \
             coolify-deploy\nNothing has changed yet — apply with:\n  topos publish \
             coolify-deploy --yes"
        );
    }

    #[test]
    fn publish_describe_tty_withholds_what_only_the_receipt_can_state() {
        use topos_types::results::PublishGate;
        // Every one of these fields is POPULATED on the payload above. None of them belongs on a
        // preview: they describe a publish that has not happened. The envelope keeps them all.
        let data = describing(PublishGate::Lands);
        let s = super::publish_describe_tty(&data, &yes_argv());
        for withheld in ["digest", "share:", "bring a teammate:", "undo:", "gate:"] {
            assert!(!s.contains(withheld), "{withheld:?} still prints: {s}");
        }
        assert!(data.undo.is_some() && data.share_line.is_some());
    }

    #[test]
    fn publish_describe_tty_falls_back_to_the_display_name_without_an_address() {
        use topos_types::results::PublishGate;
        // An unreadable address never becomes a broken one — the header degrades to the display
        // name, and to the opaque workspace id only when there is nothing else.
        let mut data = describing(PublishGate::Lands);
        data.workspace_address = None;
        let s = super::publish_describe_tty(&data, &yes_argv());
        assert!(
            s.starts_with("Publish 'coolify-deploy' to Ideamotive:"),
            "{s}"
        );
        data.workspace_display_name = None;
        let s = super::publish_describe_tty(&data, &yes_argv());
        assert!(
            s.starts_with("Publish 'coolify-deploy' to w_ideamotive:"),
            "{s}"
        );
    }

    #[test]
    fn protect_describe_tty_states_the_level_and_the_role_it_takes() {
        // The whole headline, both directions: the level being set and WHO can set it. No people
        // count — a number the client cannot know is not a fact a preview may state.
        let argv: Vec<String> = ["topos", "protect", "deploy", "--yes"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let mut data = topos_types::results::ProtectData {
            target: "deploy".to_owned(),
            kind: "skill".to_owned(),
            workspace_id: "w_acme".to_owned(),
            level: "reviewed".to_owned(),
            loosening: false,
            note: None,
            applied: false,
        };
        assert_eq!(
            super::protect_describe_tty(&data, &argv),
            "Tighten skill 'deploy' to `reviewed` (reviewer or owner)\nNothing has changed yet — \
             apply with:\n  topos protect deploy --yes"
        );
        data.level = "open".to_owned();
        data.loosening = true;
        data.note = Some("pending proposals survive a loosening".to_owned());
        assert_eq!(
            super::protect_describe_tty(&data, &argv),
            "Loosen skill 'deploy' to `open` (an owner act)\nnote: pending proposals survive a \
             loosening\nNothing has changed yet — apply with:\n  topos protect deploy --yes"
        );
    }

    #[test]
    fn publish_tty_leads_with_the_skill_name_never_the_opaque_id() {
        let line = publish_tty(&published());
        assert!(line.starts_with("Published coolify-deploy@"), "{line}");
        assert!(
            !line.contains("topos_a1b2c3"),
            "the internal bundle id must never surface on the TTY line: {line}"
        );
    }

    #[test]
    fn publish_tty_names_where_it_landed_and_leads_the_trailer_with_the_undo() {
        // The whole receipt: what shipped, where it went, the way back out, the members' link, and
        // the one line that brings in someone who is not a member yet.
        let data = PublishData {
            workspace_address: Some("topos.sh/ideamotive".to_owned()),
            share_line: Some("https://topos.sh/ideamotive/skills/coolify-deploy".to_owned()),
            undo: Some("topos revert coolify-deploy --to f154315d5fd9".to_owned()),
            invite_line: Some(INVITE.to_owned()),
            ..published()
        };
        assert_eq!(
            publish_tty(&data),
            format!(
                "Published coolify-deploy@fed180d80b8a to topos.sh/ideamotive\n\
                 undo: topos revert coolify-deploy --to f154315d5fd9\n\
                 share: https://topos.sh/ideamotive/skills/coolify-deploy\n\
                 bring a teammate: {INVITE}"
            )
        );
    }

    /// `--dest` publishes ONE of several edited copies. Both surfaces then say the two things the
    /// choice makes true: which folder these bytes came from, and that the copy left behind keeps
    /// its edits — the describe as a prediction, the receipt as a fact with the command that reads
    /// it. Both are byte-exact.
    #[test]
    fn a_per_copy_publish_names_the_folder_and_what_the_other_copy_becomes() {
        use topos_types::results::PublishGate;
        let mut described = describing(PublishGate::Lands);
        described.from_placement = Some("project/.agents/skills/coolify-deploy".to_owned());
        described.other_edited = vec!["project/.claude/skills/coolify-deploy".to_owned()];
        described.review = Some("topos diff coolify-deploy --dest .agents/skills".to_owned());
        let yes = typed(&[
            "topos",
            "publish",
            "coolify-deploy",
            "--dest",
            ".agents/skills",
            "--yes",
        ]);
        assert_eq!(
            super::publish_describe_tty(&described, &yes),
            "Publish 'coolify-deploy' to topos.sh/ideamotive:\n  from \
             project/.agents/skills/coolify-deploy (1 of 2 edited copies)\n  No review required — \
             it becomes immediately published in the workspace.\n  your other copy in \
             project/.claude/skills/coolify-deploy keeps its edits — it becomes a draft ahead of \
             this version.\nreview: topos diff coolify-deploy --dest \
             .agents/skills\nNothing has changed yet — apply with:\n  topos publish \
             coolify-deploy --dest .agents/skills --yes"
        );

        let landed = PublishData {
            workspace_address: Some("topos.sh/ideamotive".to_owned()),
            share_line: Some("https://topos.sh/ideamotive/skills/coolify-deploy".to_owned()),
            undo: Some("topos revert coolify-deploy --to f154315d5fd9".to_owned()),
            from_placement: Some("project/.agents/skills/coolify-deploy".to_owned()),
            other_edited: vec!["project/.claude/skills/coolify-deploy".to_owned()],
            ..published()
        };
        // The receipt states the SAME fact in the SAME words the describe just predicted — one
        // outcome, one sentence, wherever the reader meets it.
        assert_eq!(
            publish_tty(&landed),
            "Published coolify-deploy@fed180d80b8a to topos.sh/ideamotive (from \
             project/.agents/skills/coolify-deploy)\nyour other copy in \
             project/.claude/skills/coolify-deploy keeps its edits — it becomes a draft ahead of \
             this version.\nundo: topos revert coolify-deploy --to f154315d5fd9\nshare: \
             https://topos.sh/ideamotive/skills/coolify-deploy"
        );

        // And the plural reads as the plural of that one sentence, not as a second one.
        let many = PublishData {
            other_edited: vec![
                "project/.claude/skills/coolify-deploy".to_owned(),
                "project/.codex/skills/coolify-deploy".to_owned(),
            ],
            ..landed
        };
        assert!(
            publish_tty(&many).contains(
                "your other copies in project/.claude/skills/coolify-deploy, \
                 project/.codex/skills/coolify-deploy keep their edits — they become drafts ahead \
                 of this version."
            ),
            "{}",
            publish_tty(&many)
        );
    }

    /// A SINGLE edited copy was never a choice: `--dest` naming it does exactly what a bare publish
    /// does, so neither surface grows a line. The producer withholds both fields there; this pins
    /// that the renderer adds nothing of its own on top of that.
    #[test]
    fn one_edited_copy_names_no_folder_on_either_publish_surface() {
        use topos_types::results::PublishGate;
        let described = super::publish_describe_tty(&describing(PublishGate::Lands), &yes_argv());
        assert!(!described.contains("from "), "{described}");
        assert!(!described.contains("other copy"), "{described}");
        let landed = publish_tty(&published());
        assert!(!landed.contains("(from "), "{landed}");
        assert!(!landed.contains("other copy"), "{landed}");
    }

    /// A publish that ships the OTHER scope's copy says so on both surfaces, byte-exact: the
    /// describe names the folder AND why it is that one (the surprise a cross-scope ship carries),
    /// and the receipt names it beside the version that landed.
    #[test]
    fn a_cross_scope_publish_names_the_folder_and_why_it_was_that_one() {
        use topos_types::results::PublishGate;
        let mut described = describing(PublishGate::Lands);
        described.from_placement = Some("~/.claude/skills/coolify-deploy".to_owned());
        described.from_machine = true;
        described.review = Some("topos diff -g coolify-deploy".to_owned());
        assert!(
            super::publish_describe_tty(&described, &yes_argv()).contains(
                "\n  from ~/.claude/skills/coolify-deploy — the machine copy carries the edits; \
                 this project's copy has none."
            ),
            "{}",
            super::publish_describe_tty(&described, &yes_argv())
        );

        let landed = PublishData {
            workspace_address: Some("topos.sh/ideamotive".to_owned()),
            from_placement: Some("~/.claude/skills/coolify-deploy".to_owned()),
            from_machine: true,
            ..published()
        };
        assert!(
            publish_tty(&landed).starts_with(
                "Published coolify-deploy@fed180d80b8a to topos.sh/ideamotive (from \
                 ~/.claude/skills/coolify-deploy)"
            ),
            "{}",
            publish_tty(&landed)
        );
    }

    /// BOTH scopes drafted: the standing copy ships and the other one is disclosed — the same
    /// sentence the same-scope case uses, plus the two things a scope crossing adds (whose copy it
    /// is, and the one command that shares it). Both directions, byte-exact.
    #[test]
    fn a_double_draft_discloses_the_scope_it_left_alone() {
        use topos_types::results::{PublishGate, ScopeDraft};
        let machine = ScopeDraft {
            folder: "~/.claude/skills/coolify-deploy".to_owned(),
            machine: true,
        };
        let project = ScopeDraft {
            folder: "project/.agents/skills/coolify-deploy".to_owned(),
            machine: false,
        };
        // The bare publish from a checkout: the project's copy shipped, the machine's kept.
        let landed = PublishData {
            workspace_address: Some("topos.sh/ideamotive".to_owned()),
            other_scope_draft: Some(machine.clone()),
            ..published()
        };
        assert!(
            publish_tty(&landed).contains(
                "\nyour machine copy in ~/.claude/skills/coolify-deploy keeps its edits — it \
                 becomes a draft ahead of this version (topos publish -g coolify-deploy shares \
                 it)."
            ),
            "{}",
            publish_tty(&landed)
        );
        // The mirror — a `-g` publish while the checkout holds its own edits.
        let landed = PublishData {
            other_scope_draft: Some(project.clone()),
            ..landed
        };
        assert!(
            publish_tty(&landed).contains(
                "\nyour project copy in project/.agents/skills/coolify-deploy keeps its edits — \
                 it becomes a draft ahead of this version (topos publish coolify-deploy shares \
                 it)."
            ),
            "{}",
            publish_tty(&landed)
        );
        // The describe predicts it in the same words — and drops the "has none" clause, which
        // this state makes false.
        let mut described = describing(PublishGate::Lands);
        described.from_placement = Some("~/.claude/skills/coolify-deploy".to_owned());
        described.from_machine = true;
        described.other_scope_draft = Some(project);
        let text = super::publish_describe_tty(&described, &yes_argv());
        assert!(
            text.contains("\n  from ~/.claude/skills/coolify-deploy\n"),
            "{text}"
        );
        assert!(!text.contains("has none"), "{text}");
        assert!(
            text.contains(
                "\n  your project copy in project/.agents/skills/coolify-deploy keeps its edits — \
                 it becomes a draft ahead of this version (topos publish coolify-deploy shares \
                 it)."
            ),
            "{text}"
        );
    }

    /// The converged state is a SUCCESS document, not a refusal: one calm line, and — only when
    /// the edits are in the other scope's copy — the one command that shares them. The offered
    /// argv is the same command, so a person and an agent are told the same thing.
    #[test]
    fn an_already_published_copy_answers_calmly_and_points_at_the_edits() {
        use topos_types::results::{PublishNoChangesData, PublishResult, ScopeDraft};
        let plain = PublishNoChangesData {
            result: PublishResult::NoChanges,
            skill: "coolify-deploy".to_owned(),
            other_scope_draft: None,
        };
        assert_eq!(
            super::publish_no_changes_tty(&plain),
            "'coolify-deploy' is already published — your copy matches current"
        );
        assert!(super::publish_no_changes_actions(&plain).is_empty());

        let across = |machine: bool| PublishNoChangesData {
            other_scope_draft: Some(ScopeDraft {
                folder: if machine {
                    "~/.claude/skills/coolify-deploy".to_owned()
                } else {
                    "project/.agents/skills/coolify-deploy".to_owned()
                },
                machine,
            }),
            ..plain.clone()
        };
        assert_eq!(
            super::publish_no_changes_tty(&across(false)),
            "'coolify-deploy' is already published — your copy matches current\nyour edits are in \
             this project's copy — share them: topos publish coolify-deploy"
        );
        assert_eq!(
            super::publish_no_changes_tty(&across(true)),
            "'coolify-deploy' is already published — your copy matches current\nyour edits are in \
             your machine copy — share them: topos publish -g coolify-deploy"
        );
        assert_eq!(
            super::publish_no_changes_actions(&across(true))[0].argv,
            typed(&["topos", "publish", "-g", "coolify-deploy", "--json"])
        );
        assert_eq!(
            super::publish_no_changes_actions(&across(false))[0].argv,
            typed(&["topos", "publish", "coolify-deploy", "--json"])
        );
    }

    #[test]
    fn publish_tty_omits_a_destination_it_could_not_read() {
        // The post-write `me` read is best-effort. When it does not answer, the headline says less
        // — it never invents a destination out of a display name.
        let line = publish_tty(&published());
        assert_eq!(line, "Published coolify-deploy@fed180d80b8a");
    }

    #[test]
    fn publish_tty_discloses_a_withheld_curated_placement_next_to_the_success() {
        let line = publish_tty(&PublishData {
            current_generation: 1,
            placement_withheld: Some("everyone".to_owned()),
            ..published()
        });
        assert!(line.starts_with("Published coolify-deploy@"), "{line}");
        assert!(
            line.contains("channel 'everyone' is curated — the reference was NOT placed"),
            "the withheld placement is disclosed: {line}"
        );
        assert!(
            line.contains("a curator places it on the web"),
            "the curator's way in is named: {line}"
        );
    }

    #[test]
    fn publish_tty_carries_the_teammate_handoff_after_the_confirmation() {
        // The apply receipt hands the author the ONE line that brings a teammate in — after the
        // published confirmation, never before it (the success stays the lead).
        let line = publish_tty(&PublishData {
            invite_line: Some(INVITE.to_owned()),
            ..published()
        });
        assert!(line.starts_with("Published coolify-deploy@"), "{line}");
        let handoff = format!("\nbring a teammate: {INVITE}");
        assert!(
            line.ends_with(&handoff),
            "the handoff line follows the confirmation: {line}"
        );
    }

    /// A proposal receipt payload — the proposal handle carries the FULL candidate id, as the
    /// envelope's field does; the TTY is what shortens it.
    fn proposed() -> ProposeData {
        ProposeData {
            proposal: format!("coolify-deploy@a1b2c3d4e5f6{}", "0".repeat(52)),
            base_version_id: "9".repeat(64),
            title: "coolify-deploy".to_owned(),
            body: None,
            added: None,
            placement_withheld: None,
            placement_missing: None,
            manifest: None,
            reference: None,
            converted_from: None,
            rewrite_pending: None,
            rewrite_skipped: None,
            workspace_address: Some("topos.sh/ideamotive".to_owned()),
            share_line: Some("https://topos.sh/ideamotive/skills/coolify-deploy".to_owned()),
            from_placement: None,
            from_machine: false,
            other_scope_draft: None,
            other_edited: Vec::new(),
        }
    }

    #[test]
    fn propose_tty_names_both_verdicts_and_aligns_their_commands() {
        assert_eq!(
            propose_tty(&proposed()),
            "Published coolify-deploy@a1b2c3d4e5f6 to topos.sh/ideamotive for review.\n\
             Review required — a reviewer approves with: topos review coolify-deploy@a1b2c3d4e5f6 --approve\n\
             withdraw it yourself:                       topos review coolify-deploy@a1b2c3d4e5f6 --withdraw\n\
             share: https://topos.sh/ideamotive/skills/coolify-deploy"
        );
    }

    #[test]
    fn propose_tty_prints_the_short_handle_and_never_an_undo() {
        // The handle is SHORT (the verdict path resolves a unique 8+ char prefix), and no undo is
        // offered at all: `current` never moved, so there is no prior state a `revert` could put
        // back. The author's escape is the `--withdraw` line above.
        let s = propose_tty(&proposed());
        assert!(
            !s.contains(&"0".repeat(52)),
            "the full candidate id is not the handle: {s}"
        );
        assert!(!s.contains("undo"), "{s}");
        assert!(!s.contains("revert"), "{s}");
    }

    /// A proposal ships bytes, so it owes the copy question the same answer a landed publish
    /// gives: the folder chosen among several edited copies, and what the one left behind becomes
    /// — the direct receipt's parenthetical and the direct receipt's sentence, byte for byte.
    #[test]
    fn a_per_copy_proposal_names_the_folder_and_what_the_other_copy_becomes() {
        assert_eq!(
            propose_tty(&ProposeData {
                from_placement: Some("project/.agents/skills/coolify-deploy".to_owned()),
                other_edited: vec!["project/.claude/skills/coolify-deploy".to_owned()],
                ..proposed()
            }),
            "Published coolify-deploy@a1b2c3d4e5f6 to topos.sh/ideamotive (from \
             project/.agents/skills/coolify-deploy) for review.\n\
             Review required — a reviewer approves with: topos review coolify-deploy@a1b2c3d4e5f6 --approve\n\
             withdraw it yourself:                       topos review coolify-deploy@a1b2c3d4e5f6 --withdraw\n\
             your other copy in project/.claude/skills/coolify-deploy keeps its edits — it \
             becomes a draft ahead of this version.\n\
             share: https://topos.sh/ideamotive/skills/coolify-deploy"
        );
    }

    /// The same across SCOPES: the proposal names the folder it shipped from and the other scope's
    /// copy it left alone, with the one command that shares it — both directions, byte-exact.
    #[test]
    fn a_cross_scope_proposal_discloses_the_scope_it_left_alone() {
        use topos_types::results::ScopeDraft;
        let from_machine = ProposeData {
            from_placement: Some("~/.claude/skills/coolify-deploy".to_owned()),
            from_machine: true,
            other_scope_draft: Some(ScopeDraft {
                folder: "project/.agents/skills/coolify-deploy".to_owned(),
                machine: false,
            }),
            ..proposed()
        };
        assert_eq!(
            propose_tty(&from_machine),
            "Published coolify-deploy@a1b2c3d4e5f6 to topos.sh/ideamotive (from \
             ~/.claude/skills/coolify-deploy) for review.\n\
             Review required — a reviewer approves with: topos review coolify-deploy@a1b2c3d4e5f6 --approve\n\
             withdraw it yourself:                       topos review coolify-deploy@a1b2c3d4e5f6 --withdraw\n\
             your project copy in project/.agents/skills/coolify-deploy keeps its edits — it \
             becomes a draft ahead of this version (topos publish coolify-deploy shares it).\n\
             share: https://topos.sh/ideamotive/skills/coolify-deploy"
        );
        let from_project = ProposeData {
            from_placement: None,
            from_machine: false,
            other_scope_draft: Some(ScopeDraft {
                folder: "~/.claude/skills/coolify-deploy".to_owned(),
                machine: true,
            }),
            ..from_machine
        };
        assert_eq!(
            propose_tty(&from_project),
            "Published coolify-deploy@a1b2c3d4e5f6 to topos.sh/ideamotive for review.\n\
             Review required — a reviewer approves with: topos review coolify-deploy@a1b2c3d4e5f6 --approve\n\
             withdraw it yourself:                       topos review coolify-deploy@a1b2c3d4e5f6 --withdraw\n\
             your machine copy in ~/.claude/skills/coolify-deploy keeps its edits — it becomes a \
             draft ahead of this version (topos publish -g coolify-deploy shares it).\n\
             share: https://topos.sh/ideamotive/skills/coolify-deploy"
        );
    }

    #[test]
    fn propose_tty_omits_a_destination_it_could_not_read() {
        let s = propose_tty(&ProposeData {
            workspace_address: None,
            share_line: None,
            ..proposed()
        });
        assert!(
            s.starts_with("Published coolify-deploy@a1b2c3d4e5f6 for review.\n"),
            "{s}"
        );
    }

    #[test]
    fn pull_tty_renders_each_action_with_its_next_command() {
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
                merged,
                conflicted,
                row("pinned", PullAction::Held),
            ],
            proposals_awaiting: 2,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: None,
        };
        let out = pull_tty(&data, &[], &[], &[], &[], 0);

        // Fast-forwarded says only that it moved — the pointer's internal edition count is a
        // `--json` field, never the human line (versions are named by hash, git-style).
        assert!(out.contains("fast-forwarded"), "{out}");
        assert!(!out.contains("generation"), "{out}");
        // Merged points at the review-then-publish next step.
        assert!(out.contains("`topos diff runbook`"), "{out}");
        // Conflicted: both exits, spelled runnable, and the consequence of taking neither.
        assert!(
            out.contains("  topos update api-notes --keep-mine\n"),
            "{out}"
        );
        assert!(out.contains("  topos update api-notes --reset\n"), "{out}");
        assert!(
            out.contains("you cannot publish this skill until you pick one"),
            "{out}"
        );
        // Held says it is pinned by a local go-back and how to resume.
        assert!(out.contains("held — pinned at a local go-back"), "{out}");
        assert!(out.contains("`topos update pinned`"), "{out}");
        // Up-to-date rows stay compact: counted in the summary, no `style` action row.
        assert!(!out.contains("style  up to date"), "{out}");
        // The summary does the arithmetic, and it SUMS: a fast-forward and a merge both landed
        // bytes, so both read `updated`, and every remaining row is counted in the words its own
        // row prints — 2 + 1 + 1 + 1 = the 5 the line opens with.
        assert!(
            out.contains(
                "Checked 5 skills: 2 updated, 1 already up to date, 1 waiting on you, 1 held."
            ),
            "{out}"
        );
        // The reviewer-queue trailer.
        assert!(out.contains("2 proposal(s) awaiting review"), "{out}");
        assert!(
            out.contains("`topos review <skill>@<hash> --approve`"),
            "{out}"
        );
    }

    /// The receipt convention, byte for byte: destinations, never agents. A first
    /// materialization's `+` rows lead with the workspace-qualified name, align on a three-space
    /// gap, and count destinations in the bundle's own noun — folders for a skill, config files
    /// for an MCP server.
    #[test]
    fn installed_rows_read_plus_name_and_destination_counts() {
        let mut deploy = row("deploy-checklist", PullAction::Installed);
        deploy.display = Some("@acme/deploy-checklist".to_owned());
        deploy.destinations = vec![
            "~/.claude/skills/deploy-checklist".to_owned(),
            "~/.codex/skills/deploy-checklist".to_owned(),
        ];
        let mut linear = row("linear", PullAction::Installed);
        linear.display = Some("@acme/linear".to_owned());
        linear.kind = Some("mcp".to_owned());
        linear.destinations = vec![
            "~/.claude.json".to_owned(),
            "~/.codex/config.toml".to_owned(),
        ];
        let data = PullData {
            skills: vec![deploy, linear],
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: None,
        };
        let out = pull_tty(&data, &[], &[], &[], &[], 0);
        assert!(
            out.contains("+ @acme/deploy-checklist   installed (2 folders)\n"),
            "{out}"
        );
        assert!(
            out.contains("+ @acme/linear             installed (2 config files)\n"),
            "{out}"
        );
        assert!(!out.contains("agent"), "never an agent: {out}");
    }

    /// Exactly ONE destination prints its path instead of a count — for a skill folder and for a
    /// config file alike.
    #[test]
    fn a_single_destination_prints_its_path() {
        let mut one_folder = row("deploy", PullAction::Installed);
        one_folder.destinations = vec!["~/.codex/skills".to_owned()];
        assert_eq!(
            pull_row(&one_folder, &super::PullReceiptScope::Unstated).0,
            "installed (~/.codex/skills)"
        );

        let mut one_file = row("linear", PullAction::Installed);
        one_file.kind = Some("mcp".to_owned());
        one_file.destinations = vec!["~/.codex/config.toml".to_owned()];
        assert_eq!(
            pull_row(&one_file, &super::PullReceiptScope::Unstated).0,
            "installed (~/.codex/config.toml)"
        );

        let mut gone = row("x", PullAction::Removed);
        gone.destinations = vec!["~/.claude/skills/x".to_owned()];
        assert_eq!(
            pull_row(&gone, &super::PullReceiptScope::Unstated).0,
            "removed (~/.claude/skills/x)"
        );
    }

    /// A by-choice removal's `-` rows, kept line, and idempotent shape — the update receipt's
    /// half of the hand-edit ≡ `remove -g` equivalence.
    #[test]
    fn removed_rows_read_minus_name_kept_lines_and_counts() {
        let mut deploy = row("deploy-checklist", PullAction::Removed);
        deploy.display = Some("@acme/deploy-checklist".to_owned());
        deploy.destinations = vec![
            "~/.claude/skills/deploy-checklist".to_owned(),
            "~/.codex/skills/deploy-checklist".to_owned(),
        ];
        let mut linear = row("linear", PullAction::Removed);
        linear.display = Some("@acme/linear".to_owned());
        linear.kind = Some("mcp".to_owned());
        linear.destinations = vec![
            "~/.claude.json".to_owned(),
            "~/.codex/config.toml".to_owned(),
        ];
        // Every copy edited: NO `-` row (nothing was uninstalled) — the kept line says it all.
        let mut kept_only = row("coolify-deploy", PullAction::Removed);
        kept_only.display = Some("@acme/coolify-deploy".to_owned());
        kept_only.kept = vec!["~/.claude/skills/coolify-deploy".to_owned()];
        let data = PullData {
            skills: vec![deploy, linear, kept_only],
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: None,
        };
        let out = pull_tty(&data, &[], &[], &[], &[], 0);
        assert!(
            out.contains("- @acme/deploy-checklist   removed (2 folders)\n"),
            "{out}"
        );
        assert!(
            out.contains("- @acme/linear             removed (2 config files)\n"),
            "{out}"
        );
        assert!(
            out.contains(
                "kept coolify-deploy — ~/.claude/skills/coolify-deploy is edited (topos list \
                 coolify-deploy)\n"
            ),
            "{out}"
        );
        assert!(
            !out.contains("- @acme/coolify-deploy"),
            "an all-kept bundle earns no `-` row: {out}"
        );
    }

    /// The eager feed-drop receipt, byte for byte — the final copy: aligned `-` rows, the kept
    /// line, and the sugared `(undo: …)` trailer.
    #[test]
    fn the_feed_drop_receipt_prints_the_final_copy() {
        use topos_types::results::UninstalledBundle;
        let data = RemoveData {
            items: vec![RemoveItem {
                name: "acme".to_owned(),
                kind: RemoveKind::FeedRemoved,
                manifest: Some("~/.topos/topos.toml".to_owned()),
                workspace_id: None,
                agent_dirs: Vec::new(),
                kept_dirs: Vec::new(),
                bytes_kept: true,
                note: None,
            }],
            applied: true,
            undo: vec![
                "topos".to_owned(),
                "add".to_owned(),
                "-g".to_owned(),
                "@acme".to_owned(),
            ],
            uninstalled: vec![
                UninstalledBundle {
                    remaining: None,
                    name: "@acme/deploy-checklist".to_owned(),
                    destinations: vec![
                        "~/.claude/skills/deploy-checklist".to_owned(),
                        "~/.codex/skills/deploy-checklist".to_owned(),
                    ],
                    kind: None,
                    kept: Vec::new(),
                },
                UninstalledBundle {
                    remaining: None,
                    name: "@acme/linear".to_owned(),
                    destinations: vec![
                        "~/.claude.json".to_owned(),
                        "~/.codex/config.toml".to_owned(),
                    ],
                    kind: Some("mcp".to_owned()),
                    kept: Vec::new(),
                },
                UninstalledBundle {
                    remaining: None,
                    name: "@acme/coolify-deploy".to_owned(),
                    destinations: Vec::new(),
                    kind: None,
                    kept: vec!["~/.claude/skills/coolify-deploy".to_owned()],
                },
            ],
        };
        assert_eq!(
            remove_applied_tty(&data),
            "- @acme/deploy-checklist   removed (2 folders)\n\
             - @acme/linear             removed (2 config files)\n\
             kept coolify-deploy — ~/.claude/skills/coolify-deploy is edited (topos list \
             coolify-deploy)\n\
             (undo: topos add -g @acme)"
        );
    }

    /// ONE SUMMARY, TRUE FOR EVERY ROW IT COUNTS. A sweep whose rows are all skills says so —
    /// the ordinary word. The moment a bundle of another kind rides along, the count switches to
    /// the generic noun, because "1 managed skill" is simply false about an MCP server.
    #[test]
    fn the_summary_stops_calling_a_server_a_skill() {
        let server = |name: &str| {
            let mut r = row(name, PullAction::UpToDate);
            r.kind = Some("mcp".to_owned());
            r
        };
        let only_a_server = PullData {
            skills: vec![server("weather")],
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: None,
        };
        assert_eq!(
            pull_tty(&only_a_server, &[], &[], &[], &[], 0),
            "Checked 1 bundle: all up to date."
        );

        // Mixed: one word has to cover both, and the generic one does.
        let mixed = PullData {
            skills: vec![row("deploy", PullAction::UpToDate), server("weather")],
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: None,
        };
        assert_eq!(
            pull_tty(&mixed, &[], &[], &[], &[], 0),
            "Checked 2 bundles: all up to date."
        );

        // A row with a per-agent state to show takes the same noun into the counted summary.
        let mut noteworthy = server("weather");
        noteworthy.harnesses = vec![topos_types::results::McpAgentState {
            agent: "openclaw".to_owned(),
            state: topos_types::results::TargetOutcome::Withheld,
            note: Some("no project-level config".to_owned()),
            file: None,
        }];
        let surfaced = PullData {
            skills: vec![noteworthy],
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: None,
        };
        let out = pull_tty(&surfaced, &[], &[], &[], &[], 0);
        assert!(
            out.contains("Checked 1 bundle: 1 already up to date."),
            "{out}"
        );
        // …and the per-agent line reads in the shared vocabulary, never the raw engine token.
        assert!(
            out.contains("openclaw: not placed — no project-level config"),
            "{out}"
        );
        assert!(!out.contains("not-supported"), "{out}");
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
            behind_elsewhere: Vec::new(),
            scope: None,
        };
        assert_eq!(
            pull_tty(&clean, &[], &[], &[], &[], 0),
            "Checked 2 skills: all up to date."
        );
        // Nothing followed at all.
        let empty = PullData {
            skills: Vec::new(),
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: None,
        };
        assert_eq!(
            pull_tty(&empty, &[], &[], &[], &[], 0),
            "Nothing to update here — no manifest or profile demands anything in this directory."
        );
        // A failed skill renders visibly and is counted (even when every synced row was current).
        // The TTY prints the message's TEXT — the code rides `messages[].code`, never the prose.
        let warnings = vec![crate::message::failure(
            "IO_ERROR",
            "s_docs: a filesystem operation failed.".to_owned(),
        )];
        let out = pull_tty(&clean, &[], &warnings, &[], &[], warnings.len());
        assert!(
            out.contains("warning: s_docs: a filesystem operation failed."),
            "{out}"
        );
        assert!(!out.contains("IO_ERROR"), "no code reaches the TTY: {out}");
        assert!(
            out.contains("Checked 3 skills: 2 already up to date, 1 failed."),
            "{out}"
        );
        // A DISCLOSURE prints beside them and is counted by neither half of the summary — an
        // exchange that served nothing is a fact about a clean run, not a fault in it.
        let disclosures = vec![crate::message::disclosure(
            "NOTHING_ASSIGNED",
            "topos.sh/acme: reached, and nothing is assigned to you yet.".to_owned(),
        )];
        let out = pull_tty(&empty, &[], &[], &[], &disclosures, 0);
        assert!(
            out.contains("note: topos.sh/acme: reached, and nothing is assigned to you yet."),
            "{out}"
        );
        assert!(!out.contains("NOTHING_ASSIGNED"), "{out}");
        assert!(!out.contains("failed"), "{out}");
        assert!(!out.contains("warning:"), "{out}");
        // An ADVISORY prints as a warning but is counted by NEITHER half of the summary: the
        // bundle it annotates DELIVERED and has its own row — counting the line too would
        // invent a second, failed bundle.
        let advisories = vec![crate::message::advisory(
            "MCP_DEST_UNKNOWN",
            "\"linear\" (person): the dest entry `~/.x` in your topos.toml is not a known MCP \
             config file, so it was skipped."
                .to_owned(),
        )];
        let out = pull_tty(&clean, &[], &[], &advisories, &[], 0);
        assert!(out.contains("warning: \"linear\" (person):"), "{out}");
        assert!(!out.contains("MCP_DEST_UNKNOWN"), "{out}");
        assert!(out.contains("Checked 2 skills: all up to date."), "{out}");
        assert!(!out.contains("failed"), "{out}");
    }

    /// THE update receipt, byte for byte — the final copy. The lead line names the project folder
    /// ONCE and every path below it is written against that name; the counted row spells its
    /// folders out; the summary does the arithmetic; the trailer says what this run left alone and
    /// the one command that fixes it.
    #[test]
    fn the_update_receipt_prints_the_final_copy() {
        let mut updated = row("coolify-deploy", PullAction::Refreshed);
        updated.destinations = vec![
            "~/Forward/labs/topos_test/.agents/skills/coolify-deploy".to_owned(),
            "~/Forward/labs/topos_test/.claude/skills/coolify-deploy".to_owned(),
        ];
        let data = PullData {
            skills: vec![updated, row("style", PullAction::UpToDate)],
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: vec![BehindElsewhere {
                bundle: "coolify-deploy".to_owned(),
                project_dir: None,
            }],
            scope: Some("project ~/Forward/labs/topos_test".to_owned()),
        };
        assert_eq!(
            pull_tty(&data, &[], &[], &[], &[], 0),
            "updated project (~/Forward/labs/topos_test)\n\
             coolify-deploy   updated (2 folders)\n\
             \x20   project/.agents/skills/coolify-deploy\n\
             \x20   project/.claude/skills/coolify-deploy\n\
             Checked 2 skills: 1 updated, 1 already up to date.\n\
             1 bundle behind machine-wide — `topos update -g` updates it."
        );
    }

    /// One destination stays INLINE — a path spelled twice is noise — and it is written against
    /// the project the lead line named, exactly like the counted form's sub-lines.
    #[test]
    fn a_single_updated_folder_stays_on_the_row() {
        let mut updated = row("coolify-deploy", PullAction::Refreshed);
        updated.destinations =
            vec!["~/Forward/labs/topos_test/.claude/skills/coolify-deploy".to_owned()];
        let data = PullData {
            skills: vec![updated],
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: Some("project ~/Forward/labs/topos_test".to_owned()),
        };
        assert_eq!(
            pull_tty(&data, &[], &[], &[], &[], 0),
            "updated project (~/Forward/labs/topos_test)\n\
             coolify-deploy   updated (project/.claude/skills/coolify-deploy)\n\
             Checked 1 skill: 1 updated."
        );
    }

    /// Machine scope keeps the `~/` spelling: there is no project to be relative to, and rewriting
    /// a home path as `project/…` would name a folder the person is not standing in.
    #[test]
    fn machine_scope_paths_keep_their_home_spelling() {
        let mut updated = row("coolify-deploy", PullAction::Refreshed);
        updated.destinations = vec![
            "~/.agents/skills/coolify-deploy".to_owned(),
            "~/.claude/skills/coolify-deploy".to_owned(),
        ];
        let data = PullData {
            skills: vec![updated],
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: Some("machine".to_owned()),
        };
        assert_eq!(
            pull_tty(&data, &[], &[], &[], &[], 0),
            "updated machine-wide\n\
             coolify-deploy   updated (2 folders)\n\
             \x20   ~/.agents/skills/coolify-deploy\n\
             \x20   ~/.claude/skills/coolify-deploy\n\
             Checked 1 skill: 1 updated."
        );
    }

    /// The row's SECOND fact is a body line like any other: written against the project the lead
    /// named, and in the same vocabulary the action column uses.
    #[test]
    fn a_rows_second_fact_is_written_against_the_project_too() {
        let mut healed = row("coolify-deploy", PullAction::Installed);
        healed.destinations =
            vec!["~/Forward/labs/topos_test/.claude/skills/coolify-deploy".to_owned()];
        healed.note = Some(crate::ops::sync_engine::also_line(
            "updated",
            &["~/Forward/labs/topos_test/.agents/skills/coolify-deploy".to_owned()],
        ));
        let data = PullData {
            skills: vec![healed],
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: Some("project ~/Forward/labs/topos_test".to_owned()),
        };
        assert_eq!(
            pull_tty(&data, &[], &[], &[], &[], 0),
            "updated project (~/Forward/labs/topos_test)\n\
             + coolify-deploy   installed (project/.claude/skills/coolify-deploy)\n\
             \x20   also updated project/.agents/skills/coolify-deploy\n\
             Checked 1 skill: 1 installed."
        );
    }

    /// EVERY summary variant, by name. The reader never subtracts one count from another to learn
    /// what happened.
    #[test]
    fn the_summary_counts_every_action_by_name() {
        // `failed` is the count of BUNDLES the run could not carry forward — the sweep's own,
        // never `warnings.len()`.
        let counted = |skills: Vec<PullSkill>, warnings: &[topos_types::Message], failed: usize| {
            let data = PullData {
                skills,
                proposals_awaiting: 0,
                notices: Vec::new(),
                sync: Vec::new(),
                behind_elsewhere: Vec::new(),
                scope: None,
            };
            let out = pull_tty(&data, &[], warnings, &[], &[], failed);
            out.lines().last().unwrap_or_default().to_owned()
        };
        let summary = |skills: Vec<PullSkill>, warnings: &[topos_types::Message]| {
            let failed = warnings.len();
            counted(skills, warnings, failed)
        };
        let placed = |name: &str, action| {
            let mut r = row(name, action);
            r.destinations = vec![format!("~/.claude/skills/{name}")];
            r
        };
        assert_eq!(
            summary(
                vec![
                    placed("deploy", PullAction::Refreshed),
                    row("style", PullAction::UpToDate),
                ],
                &[]
            ),
            "Checked 2 skills: 1 updated, 1 already up to date."
        );
        assert_eq!(
            summary(
                vec![
                    placed("notes", PullAction::Installed),
                    placed("deploy", PullAction::Refreshed),
                    placed("gone", PullAction::Removed),
                    row("style", PullAction::UpToDate),
                ],
                &[]
            ),
            "Checked 4 skills: 1 installed, 1 updated, 1 removed, 1 already up to date."
        );
        assert_eq!(
            summary(
                vec![
                    placed("deploy", PullAction::Refreshed),
                    row("style", PullAction::UpToDate),
                ],
                &[crate::message::failure(
                    "IO_ERROR",
                    "s_docs: a filesystem operation failed.".to_owned()
                )]
            ),
            "Checked 3 skills: 1 updated, 1 already up to date, 1 failed."
        );
        assert_eq!(
            summary(
                vec![
                    row("style", PullAction::UpToDate),
                    row("deploy", PullAction::UpToDate),
                ],
                &[]
            ),
            "Checked 2 skills: all up to date."
        );
        // A non-skill kind riding along still switches the noun — and the counts stay explicit.
        let mut server = row("weather", PullAction::UpToDate);
        server.kind = Some("mcp".to_owned());
        assert_eq!(
            summary(vec![placed("deploy", PullAction::Refreshed), server], &[]),
            "Checked 2 bundles: 1 updated, 1 already up to date."
        );

        // A DRAFT IS NOT "ALREADY UP TO DATE". Delivery owed this bundle nothing, so its action
        // is `up_to_date` — and the receipt used to announce "all up to date" about the very
        // bundle `list` was calling a draft and `status` was counting as a draft ahead. Three
        // surfaces, one machine, three answers. The row now says both true things, and the
        // clause uses `status`'s own words.
        let mut drafted = row("deploy", PullAction::UpToDate);
        drafted.draft = true;
        assert_eq!(
            summary(
                vec![drafted.clone(), row("style", PullAction::UpToDate)],
                &[]
            ),
            "Checked 2 skills: 1 already up to date, 1 draft ahead."
        );
        // The compact "all up to date" shortcut cannot fire over a draft — it was the sentence
        // that did the contradicting.
        let whole = {
            let data = PullData {
                skills: vec![drafted.clone()],
                proposals_awaiting: 0,
                notices: Vec::new(),
                sync: Vec::new(),
                behind_elsewhere: Vec::new(),
                scope: None,
            };
            pull_tty(&data, &[], &[], &[], &[], 0)
        };
        assert!(!whole.contains("all up to date"), "{whole}");
        // The ROW says what `list` says, and names the one command that shares the work.
        assert!(
            whole.contains("up to date — your edits are not shared yet (topos publish deploy)"),
            "{whole}"
        );
        assert!(
            whole.ends_with("Checked 1 skill: 1 draft ahead."),
            "{whole}"
        );
        // Plural reads as a person would say it.
        let mut d2 = row("notes", PullAction::UpToDate);
        d2.draft = true;
        assert_eq!(
            summary(vec![drafted, d2], &[]),
            "Checked 2 skills: 2 drafts ahead."
        );

        // A WARNING IS NOT A BUNDLE. A scope-level fault — an unavailable lock, an unreadable
        // custody document — is one line about no bundle at all, and two lines can be about one
        // bundle. Counting LINES made this summary invent bundles the machine does not have and
        // then report them failed; the arithmetic now counts what the sweep actually failed on.
        assert_eq!(
            counted(
                vec![
                    placed("deploy", PullAction::Refreshed),
                    row("style", PullAction::UpToDate),
                ],
                &[
                    crate::message::failure(
                        "MCP_LOCK_UNAVAILABLE",
                        "no MCP config file was read or written this run.".to_owned(),
                    ),
                    crate::message::failure(
                        "MCP_SURFACE_UNPROVABLE",
                        "cursor's MCP config: the file could not be parsed.".to_owned(),
                    ),
                ],
                0,
            ),
            "Checked 2 skills: 1 updated, 1 already up to date.",
            "two scope-level lines are still printed, and counted as nothing"
        );
        // Two lines about ONE wedged bundle count that bundle once.
        assert_eq!(
            counted(
                vec![row("style", PullAction::UpToDate)],
                &[
                    crate::message::failure(
                        "IO_ERROR",
                        "docs: a filesystem operation failed.".to_owned(),
                    ),
                    crate::message::failure(
                        "IO_ERROR",
                        "docs: and again on the second placement.".to_owned(),
                    ),
                ],
                1,
            ),
            "Checked 2 skills: 1 already up to date, 1 failed."
        );
    }

    /// The clause exists so nobody has to subtract — so it must SUM. Over a payload holding every
    /// action the sweep can report, plus a failure, the parts add up to exactly the total the line
    /// opens with, and no row is silently uncounted.
    ///
    /// `bucket_word` is an EXHAUSTIVE match on purpose: a new `PullAction` cannot be added without
    /// coming through here and naming the word its rows will be counted in.
    #[test]
    fn every_action_lands_in_a_counted_bucket_and_the_parts_sum_to_the_total() {
        fn bucket_word(action: PullAction) -> &'static str {
            match action {
                PullAction::Installed => "installed",
                PullAction::FastForwarded
                | PullAction::Refreshed
                | PullAction::Merged
                | PullAction::DraftSynced => "updated",
                PullAction::Removed => "removed",
                PullAction::UpToDate => "already up to date",
                PullAction::Conflicted => "waiting on you",
                PullAction::Withdrawn | PullAction::Released => "no longer shared",
                PullAction::Held => "held",
            }
        }
        const EVERY: [PullAction; 11] = [
            PullAction::UpToDate,
            PullAction::FastForwarded,
            PullAction::Installed,
            PullAction::Refreshed,
            PullAction::Removed,
            PullAction::Merged,
            PullAction::Conflicted,
            PullAction::DraftSynced,
            PullAction::Held,
            PullAction::Withdrawn,
            PullAction::Released,
        ];

        let mut tally = super::PullTally::default();
        let skills: Vec<PullSkill> = EVERY
            .iter()
            .enumerate()
            .map(|(i, action)| {
                let mut r = row(&format!("skill{i}"), *action);
                tally.count(&r);
                r.destinations = vec![format!("~/.claude/skills/skill{i}")];
                // A released row states its whole fact in `note`; the sweep composes it.
                if *action == PullAction::Released {
                    r.note = Some(
                        "acme stopped sharing this — topos will not update it any more\nthe files \
                         stay where they are, and are yours to keep or delete: \
                         ~/.claude/skills/skill10"
                            .to_owned(),
                    );
                }
                // A conflicted row's two exits are commands, so its bucket word must not be read
                // off one of them by accident — the row states them in full.
                if *action == PullAction::Conflicted {
                    r.merge = Some(merge_report(false, Vec::new()));
                }
                r
            })
            .collect();
        let warnings = vec![crate::message::failure(
            "IO_ERROR",
            "s_docs: a filesystem operation failed.".to_owned(),
        )];
        tally.failed = warnings.len();
        assert_eq!(
            tally.total(),
            EVERY.len() + warnings.len(),
            "every action is counted exactly once"
        );

        let data = PullData {
            skills,
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: None,
        };
        let out = pull_tty(&data, &[], &warnings, &[], &[], warnings.len());
        let line = out
            .lines()
            .find(|l| l.starts_with("Checked "))
            .expect("the summary line");

        // `Checked <total> <noun>: <n> <word>, <n> <word>, ….`
        let (head, clauses) = line.split_once(": ").expect("counted clauses");
        let total: usize = head
            .trim_start_matches("Checked ")
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .expect("the total");
        assert_eq!(total, EVERY.len() + warnings.len(), "{line}");
        let parts: Vec<&str> = clauses.trim_end_matches('.').split(", ").collect();
        let summed: usize = parts
            .iter()
            .map(|p| {
                p.split_whitespace()
                    .next()
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or_else(|| panic!("a counted clause: {p}"))
            })
            .sum();
        assert_eq!(summed, total, "the parts must sum to the total: {line}");

        // And each row is counted in the word its OWN row prints, never a bucket invented here.
        for action in EVERY {
            let word = bucket_word(action);
            assert!(
                parts.iter().any(|p| p.ends_with(word)),
                "{action:?} is counted as `{word}`: {line}"
            );
        }
        assert!(parts.iter().any(|p| p.ends_with("failed")), "{line}");
    }

    /// The staleness trailer's every shape: singular vs plural machine-wide, and the mirror case
    /// that names a folder because no command from here reaches it.
    #[test]
    fn the_staleness_trailer_counts_bundles_and_names_the_command() {
        let trailer = |entries: Vec<BehindElsewhere>| {
            let data = PullData {
                skills: vec![row("style", PullAction::UpToDate)],
                proposals_awaiting: 0,
                notices: Vec::new(),
                sync: Vec::new(),
                behind_elsewhere: entries,
                scope: None,
            };
            let out = pull_tty(&data, &[], &[], &[], &[], 0);
            out.lines().skip(1).collect::<Vec<_>>().join("\n")
        };
        let machine = |name: &str| BehindElsewhere {
            bundle: name.to_owned(),
            project_dir: None,
        };
        let in_dir = |name: &str, dir: &str| BehindElsewhere {
            bundle: name.to_owned(),
            project_dir: Some(dir.to_owned()),
        };
        assert_eq!(
            trailer(vec![machine("deploy")]),
            "1 bundle behind machine-wide — `topos update -g` updates it."
        );
        assert_eq!(
            trailer(vec![
                machine("deploy"),
                machine("notes"),
                machine("runbook")
            ]),
            "3 bundles behind machine-wide — `topos update -g` updates them."
        );
        assert_eq!(
            trailer(vec![
                in_dir("deploy", "~/Forward/labs/topos_test"),
                in_dir("notes", "~/Forward/labs/topos_test"),
            ]),
            "2 bundles behind in ~/Forward/labs/topos_test — run `topos update` there."
        );
        // Nothing behind → nothing said. Silence is the healthy state.
        assert_eq!(trailer(Vec::new()), "");
    }

    /// A project run that moved nothing says so in three words — and still counts what stands
    /// behind machine-wide. The long sentence this replaced carried its own machine-wide pointer
    /// precisely because this path used to return before the trailer could speak; now the trailer
    /// does, with a number and a command instead of a standing reminder that everything is fine.
    #[test]
    fn an_empty_project_receipt_is_three_words_and_still_counts_the_other_scope() {
        let empty = |behind: Vec<BehindElsewhere>| PullData {
            skills: Vec::new(),
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: behind,
            scope: Some("project ~/Forward/labs/api".to_owned()),
        };
        assert_eq!(
            pull_tty(&empty(Vec::new()), &[], &[], &[], &[], 0),
            "Up to date."
        );
        assert_eq!(
            pull_tty(
                &empty(vec![
                    BehindElsewhere {
                        bundle: "deploy".to_owned(),
                        project_dir: None,
                    },
                    BehindElsewhere {
                        bundle: "notes".to_owned(),
                        project_dir: None,
                    },
                ]),
                &[],
                &[],
                &[],
                &[],
                0,
            ),
            "Up to date.\n2 bundles behind machine-wide — `topos update -g` updates them."
        );
        // The reviewer-queue trailer still closes the receipt, after the staleness line.
        let mut awaiting = empty(vec![BehindElsewhere {
            bundle: "deploy".to_owned(),
            project_dir: None,
        }]);
        awaiting.proposals_awaiting = 1;
        let out = pull_tty(&awaiting, &[], &[], &[], &[], 0);
        assert!(
            out.starts_with("Up to date.\n1 bundle behind machine-wide"),
            "{out}"
        );
        assert!(out.contains("1 proposal(s) awaiting review"), "{out}");
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
            behind_elsewhere: Vec::new(),
            scope: None,
        };
        let out = pull_tty(&data, &[], &[], &[], &[], 0);
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
    fn list_tty_sections_scopes_and_summarizes_what_it_does_not_show() {
        use topos_types::results::{ListScope, ListScopeSummary, SkillStatus, UntrackedSummary};
        let entry = |name: &str, version: &str, status| SkillEntry {
            skill: name.to_owned(),
            workspace_id: Some("w_acme".to_owned()),
            version_id: version.to_owned(),
            bundle_digest: "cd".repeat(32),
            draft: false,
            pending_proposals: Vec::new(),
            source: Some("topos.sh/acme".to_owned()),
            status,
            kind: None,
            source_health: None,
            draft_dir: None,
            draft_diverged: None,
            source_missing: false,
        };
        let out = ListOutcome {
            data: ListData {
                scopes: vec![ListScope {
                    scope: "project".to_owned(),
                    manifest: Some("./topos.toml".to_owned()),
                    rows: vec![
                        entry("deploy", &"ab".repeat(32), Some(SkillStatus::Current)),
                        // Never applied here: the all-zero identity renders as the honest note.
                        entry("fresh", &"0".repeat(64), None),
                    ],
                    orphans: Vec::new(),
                }],
                machine_summary: Some(ListScopeSummary {
                    skills: 2,
                    updates_pending: 1,
                    command: "topos list -g".to_owned(),
                }),
                untracked_summary: Some(UntrackedSummary {
                    skills: 3,
                    folders: 2,
                    command: "topos list --untracked".to_owned(),
                }),
                signed_in: true,
                ..ListData::default()
            },
            warnings: Vec::new(),
            untracked_view: false,
        };
        let text = list_tty(&out);
        // The section says what it holds and names the governing file ONCE.
        assert!(
            text.starts_with("Project bundles (./topos.toml):\n"),
            "{text}"
        );
        // Every row names its ORIGIN, not the file the header already named.
        assert!(
            text.contains("deploy  deploy@abababababab  [current]  from topos.sh/acme"),
            "{text}"
        );
        assert!(!text.contains("from ./topos.toml"), "{text}");
        assert!(
            text.contains("fresh  (not applied here yet — `topos update` applies it)"),
            "{text}"
        );
        assert!(!text.contains("fresh@000000"), "{text}");
        // EXACTLY one summary line each, ending in the backticked command.
        assert!(
            text.contains("2 skills machine-wide, 1 update pending — `topos list -g`"),
            "{text}"
        );
        assert!(
            text.contains(
                "3 untracked skills in 2 folders — `topos list --untracked` shows them; \
                 `topos add <name>` manages one"
            ),
            "{text}"
        );
        // Signed in: the STATIC remote pointer (no network, no cached counts).
        assert!(
            text.contains("See what your workspaces offer: `topos list --remote`"),
            "{text}"
        );

        // Signed out: the pointer disappears; an empty scope says so instead of vanishing.
        let out = ListOutcome {
            data: ListData {
                scopes: vec![ListScope {
                    scope: "machine".to_owned(),
                    manifest: None,
                    rows: Vec::new(),
                    orphans: Vec::new(),
                }],
                signed_in: false,
                ..ListData::default()
            },
            warnings: Vec::new(),
            untracked_view: false,
        };
        let text = list_tty(&out);
        assert!(
            text.starts_with("Machine-wide bundles (no ~/.topos/topos.toml):"),
            "{text}"
        );
        assert!(text.contains("(nothing installed in this scope)"), "{text}");
        assert!(!text.contains("workspaces offer"), "{text}");
    }

    /// Every command a ROW suggests is scope-exact: a machine-section row spells `topos update -g`
    /// (a bare `update` read from inside a project drives the PROJECT and would never touch it),
    /// while a project row keeps the bare spelling. `--all` renders both sections in one output,
    /// so the two spellings must appear side by side.
    #[test]
    fn list_tty_rows_spell_their_own_scopes_update_command() {
        use topos_types::results::ListScope;
        let never_applied = |name: &str| SkillEntry {
            skill: name.to_owned(),
            workspace_id: Some("w_acme".to_owned()),
            version_id: "0".repeat(64),
            bundle_digest: "0".repeat(64),
            draft: false,
            pending_proposals: Vec::new(),
            source: None,
            status: None,
            kind: None,
            source_health: None,
            draft_dir: None,
            draft_diverged: None,
            source_missing: false,
        };
        let out = ListOutcome {
            data: ListData {
                scopes: vec![
                    ListScope {
                        scope: "project".to_owned(),
                        manifest: Some("/repo/topos.toml".to_owned()),
                        rows: vec![never_applied("in-repo")],
                        orphans: Vec::new(),
                    },
                    ListScope {
                        scope: "machine".to_owned(),
                        manifest: None,
                        rows: vec![never_applied("machine-wide")],
                        orphans: Vec::new(),
                    },
                ],
                signed_in: false,
                ..ListData::default()
            },
            warnings: Vec::new(),
            untracked_view: false,
        };
        let text = list_tty(&out);
        assert!(
            text.contains("in-repo  (not applied here yet — `topos update` applies it)"),
            "{text}"
        );
        assert!(
            text.contains("machine-wide  (not applied here yet — `topos update -g` applies it)"),
            "{text}"
        );
    }

    /// A DRAFT row says where the edits are and the three things a person can do with them —
    /// byte-exact, the labels padded so the commands line up. The commands are scope-exact the way
    /// the row's own are: `update` drives a scope, so a machine row spells `-g`; `publish` and
    /// `diff` take a name and no scope, so neither ever grows one. A clean row gains nothing.
    #[test]
    fn draft_rows_name_their_folder_and_the_ways_out() {
        use topos_types::results::{ListScope, SkillStatus};
        let row = |name: &str, status, dir: Option<&str>, diverged| SkillEntry {
            skill: name.to_owned(),
            workspace_id: Some("w_ideamotive".to_owned()),
            version_id: format!("f154315d5fd9{}", "0".repeat(52)),
            bundle_digest: "cd".repeat(32),
            draft: matches!(status, Some(SkillStatus::Draft)),
            pending_proposals: Vec::new(),
            source: Some("topos.sh/ideamotive".to_owned()),
            status,
            kind: None,
            source_health: None,
            draft_dir: dir.map(str::to_owned),
            draft_diverged: diverged,
            source_missing: false,
        };
        let render = |scope: &str, entry| {
            list_tty(&ListOutcome {
                data: ListData {
                    scopes: vec![ListScope {
                        scope: scope.to_owned(),
                        manifest: Some("./topos.toml".to_owned()),
                        rows: vec![entry],
                        orphans: Vec::new(),
                    }],
                    signed_in: false,
                    ..ListData::default()
                },
                warnings: Vec::new(),
                untracked_view: false,
            })
        };

        let text = render(
            "project",
            row(
                "coolify-deploy",
                Some(SkillStatus::Draft),
                Some("project/.agents/skills/coolify-deploy"),
                None,
            ),
        );
        assert!(
            text.ends_with(
                "  coolify-deploy  coolify-deploy@f154315d5fd9  (draft)  from topos.sh/ideamotive\n\
                 \x20     draft in project/.agents/skills/coolify-deploy (not shared)\n\
                 \x20     to share:     topos publish coolify-deploy\n\
                 \x20     to view diff: topos diff coolify-deploy\n\
                 \x20     to drop:      topos update coolify-deploy --reset"
            ),
            "{text}"
        );

        // The machine section: only the scope-driven verb takes the flag.
        let text = render(
            "machine",
            row(
                "coolify-deploy",
                Some(SkillStatus::Draft),
                Some("~/.agents/skills/coolify-deploy"),
                None,
            ),
        );
        assert!(
            text.ends_with(
                "\x20     draft in ~/.agents/skills/coolify-deploy (not shared)\n\
                 \x20     to share:     topos publish coolify-deploy\n\
                 \x20     to view diff: topos diff coolify-deploy\n\
                 \x20     to drop:      topos update -g coolify-deploy --reset"
            ),
            "{text}"
        );

        // Copies that disagree: ONE line, no folder named, pointing at the deep dive — in the same
        // words the deep dive, the freeze menu and the escape's receipt use for this state.
        let text = render(
            "project",
            row("coolify-deploy", Some(SkillStatus::Draft), None, Some(2)),
        );
        assert!(
            text.ends_with(
                "  coolify-deploy  coolify-deploy@f154315d5fd9  (draft)  from topos.sh/ideamotive\n\
                 \x20     different edits in 2 folders (see: topos list coolify-deploy)"
            ),
            "{text}"
        );
        let text = render(
            "machine",
            row("coolify-deploy", Some(SkillStatus::Draft), None, Some(3)),
        );
        assert!(
            text.ends_with("different edits in 3 folders (see: topos list -g coolify-deploy)"),
            "{text}"
        );
        // The count is READ, never assumed: a one-element vector says `1 folder`.
        let text = render(
            "project",
            row("coolify-deploy", Some(SkillStatus::Draft), None, Some(1)),
        );
        assert!(
            text.ends_with("different edits in 1 folder (see: topos list coolify-deploy)"),
            "{text}"
        );

        // Not a draft: the row stands alone, whatever else it carries.
        let text = render(
            "project",
            row("coolify-deploy", Some(SkillStatus::Current), None, None),
        );
        assert!(
            text.ends_with(
                "  coolify-deploy  coolify-deploy@f154315d5fd9  [current]  from topos.sh/ideamotive"
            ),
            "{text}"
        );
        assert!(!text.contains("to share:"), "{text}");
        // An edited copy the scan could not place names nothing rather than guessing a folder.
        let text = render(
            "project",
            row("coolify-deploy", Some(SkillStatus::Draft), None, None),
        );
        assert!(
            text.ends_with("(draft)  from topos.sh/ideamotive"),
            "{text}"
        );
    }

    /// The deep dive's diverged-copies block is scope-exact like everything else it prints: the
    /// machine's answer spells the two `update`s with `-g`, while `publish` and `diff` — which take
    /// a name and no scope — never grow one.
    #[test]
    fn the_diverged_copies_block_spells_the_machine_scope() {
        use topos_types::results::{DivergedCopy, ListDetail, StatusItemState};
        let copy = |display: &str, dest: &str| DivergedCopy {
            display: display.to_owned(),
            dest: dest.to_owned(),
        };
        let out = ListOutcome {
            data: ListData {
                detail: Some(ListDetail {
                    name: "coolify-deploy".to_owned(),
                    scope: Some("machine".to_owned()),
                    source_file: None,
                    source_key: None,
                    feed: Some("topos.sh/acme".to_owned()),
                    attribution: None,
                    version: None,
                    pin: None,
                    placements: Vec::new(),
                    state: StatusItemState::LocalEdits,
                    kind: None,
                    harnesses: Vec::new(),
                    mcp_unreachable: None,
                    managed: true,
                    folders: Vec::new(),
                    diverged: vec![
                        copy("~/.claude/skills/coolify-deploy", "~/.claude/skills"),
                        copy("~/.agents/skills/coolify-deploy", "~/.agents/skills"),
                    ],
                    conflict_copy: None,
                    conflict_reason: None,
                    drafted: false,
                    twin: None,
                }),
                signed_in: false,
                ..ListData::default()
            },
            warnings: Vec::new(),
            untracked_view: false,
        };
        let text = list_tty(&out);
        assert!(
            text.ends_with(
                "  different edits in 2 folders — name the one to work with:\n\
                 \x20   ~/.claude/skills/coolify-deploy\n\
                 \x20     to share:     topos publish coolify-deploy --dest ~/.claude/skills\n\
                 \x20     to view diff: topos diff coolify-deploy --dest ~/.claude/skills\n\
                 \x20     to drop:      topos update -g coolify-deploy --dest ~/.claude/skills \
                 --reset\n\
                 \x20   ~/.agents/skills/coolify-deploy\n\
                 \x20     to share:     topos publish coolify-deploy --dest ~/.agents/skills\n\
                 \x20     to view diff: topos diff coolify-deploy --dest ~/.agents/skills\n\
                 \x20     to drop:      topos update -g coolify-deploy --dest ~/.agents/skills \
                 --reset\n\
                 \x20 to drop every copy's edits: topos update -g coolify-deploy --reset"
            ),
            "{text}"
        );
    }

    /// The count is READ off the vector, never assumed: one copy says `1 folder`, on the deep dive
    /// and on the freeze menu alike. Producers only ever hand these two surfaces ≥ 2 copies today,
    /// so a hard-coded `folders` would sit there being right by luck until the day it wasn't.
    #[test]
    fn one_competing_copy_is_named_in_the_singular() {
        use topos_types::results::{DivergedCopy, ListDetail, StatusItemState};
        let text = list_tty(&ListOutcome {
            data: ListData {
                detail: Some(ListDetail {
                    name: "coolify-deploy".to_owned(),
                    scope: Some("project".to_owned()),
                    source_file: None,
                    source_key: None,
                    feed: Some("topos.sh/acme".to_owned()),
                    attribution: None,
                    version: None,
                    pin: None,
                    placements: Vec::new(),
                    state: StatusItemState::LocalEdits,
                    kind: None,
                    harnesses: Vec::new(),
                    mcp_unreachable: None,
                    managed: true,
                    folders: Vec::new(),
                    diverged: vec![DivergedCopy {
                        display: "project/.claude/skills/coolify-deploy".to_owned(),
                        dest: ".claude/skills".to_owned(),
                    }],
                    conflict_copy: None,
                    conflict_reason: None,
                    drafted: false,
                    twin: None,
                }),
                signed_in: false,
                ..ListData::default()
            },
            warnings: Vec::new(),
            untracked_view: false,
        });
        assert!(
            text.contains("different edits in 1 folder — name the one to work with:"),
            "{text}"
        );

        let err = crate::error::ClientError::PlacementsDiverged {
            skill: "coolify-deploy".to_owned(),
            copies: vec![crate::error::DivergedCopy {
                display: "project/.claude/skills/coolify-deploy".to_owned(),
                dest: ".claude/skills".to_owned(),
            }],
            global: false,
        };
        let tty = super::err_tty(&err);
        assert!(
            tty.starts_with("error: 'coolify-deploy' has different edits in 1 folder:"),
            "{tty}"
        );
    }

    /// The deep dive's suggested command is scope-exact like the rows: a machine-scope answer
    /// spells `topos update -g` in every state that suggests an update, a project answer keeps
    /// the bare form.
    #[test]
    fn the_deep_dive_spells_its_own_scopes_update_command() {
        use topos_types::results::{ListDetail, StatusItemState as S};
        let detail = |scope: &str, state| ListDetail {
            name: "deploy".to_owned(),
            scope: Some(scope.to_owned()),
            source_file: None,
            source_key: None,
            feed: Some("topos.sh/acme".to_owned()),
            attribution: None,
            version: None,
            pin: None,
            placements: Vec::new(),
            state,
            kind: None,
            harnesses: Vec::new(),
            mcp_unreachable: None,
            managed: true,
            folders: Vec::new(),
            diverged: Vec::new(),
            conflict_copy: None,
            conflict_reason: None,
            drafted: false,
            twin: None,
        };
        let render = |d| {
            list_tty(&ListOutcome {
                data: ListData {
                    detail: Some(d),
                    signed_in: false,
                    ..ListData::default()
                },
                warnings: Vec::new(),
                untracked_view: false,
            })
        };
        let text = render(detail("machine", S::Behind));
        assert!(
            text.contains("behind — `topos update -g` lands the newer version"),
            "{text}"
        );
        let text = render(detail("project", S::Behind));
        assert!(
            text.contains("behind — `topos update` lands the newer version"),
            "{text}"
        );
        let text = render(detail("machine", S::NoDeliveryYet));
        assert!(
            text.contains("no delivery yet (`topos update -g` performs the first exchange)"),
            "{text}"
        );
        let text = render(detail("machine", S::Unknown));
        assert!(
            text.contains("not applied here yet (`topos update -g` applies it)"),
            "{text}"
        );
    }

    /// THE CHECKOUT MODEL, byte-exact on the deep dive: the scope on the name line, the draft
    /// after the version, and the other scope's copy last. The four states (draft × twin) in both
    /// directions, because a bare run and a `-g` run are answers about different copies and every
    /// command they spell has to say which.
    #[test]
    fn the_deep_dive_leads_with_the_checkout_and_names_the_other_scopes_copy() {
        use topos_types::results::{ListDetail, ScopeTwin, StatusItemState as S};
        let detail = |scope: &str, drafted, twin| ListDetail {
            name: "coolify-deploy".to_owned(),
            scope: Some(scope.to_owned()),
            source_file: None,
            source_key: None,
            feed: Some("topos.sh/acme".to_owned()),
            attribution: None,
            version: Some("a".repeat(64)),
            pin: None,
            placements: Vec::new(),
            state: S::Applied,
            kind: None,
            harnesses: Vec::new(),
            mcp_unreachable: None,
            managed: true,
            folders: Vec::new(),
            diverged: Vec::new(),
            conflict_copy: None,
            conflict_reason: None,
            drafted,
            twin,
        };
        let render = |d| {
            list_tty(&ListOutcome {
                data: ListData {
                    detail: Some(d),
                    signed_in: false,
                    ..ListData::default()
                },
                warnings: Vec::new(),
                untracked_view: false,
            })
        };
        // A project answer with neither: the headline alone says which copy this is about.
        assert_eq!(
            render(detail("project", false, None)),
            "coolify-deploy — in this project\n  from the topos.sh/acme feed\n  version \
             aaaaaaaaaaaa\n  applied"
        );
        // The same copy, drafted, with a clean machine twin.
        assert_eq!(
            render(detail(
                "project",
                true,
                Some(ScopeTwin {
                    machine: true,
                    drafted: false
                })
            )),
            "coolify-deploy — in this project\n  from the topos.sh/acme feed\n  version \
             aaaaaaaaaaaa\n  your draft: edited, not shared yet (topos diff coolify-deploy)\n  \
             applied\n  also on this machine: a separate copy, no edits (topos list -g \
             coolify-deploy)"
        );
        // The mirror: a `-g` answer, clean here, with an EDITED project copy — every command
        // spelled for the scope it drives.
        assert_eq!(
            render(detail(
                "machine",
                false,
                Some(ScopeTwin {
                    machine: false,
                    drafted: true
                })
            )),
            "coolify-deploy — on this machine\n  from the topos.sh/acme feed\n  version \
             aaaaaaaaaaaa\n  applied\n  also in this project: a separate copy, edited (topos list \
             coolify-deploy)"
        );
        // A machine draft with no twin: the read that shows it carries `-g`.
        assert!(
            render(detail("machine", true, None))
                .contains("your draft: edited, not shared yet (topos diff -g coolify-deploy)"),
            "{}",
            render(detail("machine", true, None))
        );
    }

    /// The NOT-MANAGED deep dive is the headline, the unmanaged folders one per line, and — when
    /// copies exist — the one command that brings them under management. Nothing else: no source
    /// line, no state sentence, no store mention.
    #[test]
    fn the_deep_dive_renders_the_not_managed_headline_and_folders() {
        use topos_types::results::{ListDetail, StatusItemState};
        let detail = |folders: Vec<String>| ListDetail {
            name: "deploy".to_owned(),
            scope: None,
            source_file: None,
            source_key: None,
            feed: None,
            attribution: None,
            version: None,
            pin: None,
            placements: Vec::new(),
            state: StatusItemState::Unknown,
            kind: None,
            harnesses: Vec::new(),
            mcp_unreachable: None,
            managed: false,
            folders,
            diverged: Vec::new(),
            conflict_copy: None,
            conflict_reason: None,
            drafted: false,
            twin: None,
        };
        let render = |d| {
            list_tty(&ListOutcome {
                data: ListData {
                    detail: Some(d),
                    signed_in: false,
                    ..ListData::default()
                },
                warnings: Vec::new(),
                untracked_view: false,
            })
        };
        // No copies: nothing to adopt, so the headline stays bare — no `to manage:` line.
        assert_eq!(
            render(detail(Vec::new())),
            "deploy — not managed on this machine"
        );
        assert_eq!(
            render(detail(vec![
                "/home/u/.claude/skills/deploy".to_owned(),
                "/home/u/.cursor/skills/deploy".to_owned(),
            ])),
            "deploy — not managed on this machine\n  /home/u/.claude/skills/deploy\n  \
             /home/u/.cursor/skills/deploy\nto manage: topos add -g deploy --dest <dest>"
        );
    }

    /// The `to manage:` line carries THIS bundle's name and leaves the destination a literal
    /// placeholder — the one choice the answer cannot make for the person.
    #[test]
    fn the_not_managed_answer_offers_the_adopt_with_a_dest_placeholder() {
        use topos_types::results::{ListDetail, StatusItemState};
        let out = ListOutcome {
            data: ListData {
                detail: Some(ListDetail {
                    name: "frontend-design".to_owned(),
                    scope: None,
                    source_file: None,
                    source_key: None,
                    feed: None,
                    attribution: None,
                    version: None,
                    pin: None,
                    placements: Vec::new(),
                    state: StatusItemState::Unknown,
                    kind: None,
                    harnesses: Vec::new(),
                    mcp_unreachable: None,
                    managed: false,
                    folders: vec!["/home/u/.claude/skills/frontend-design".to_owned()],
                    diverged: Vec::new(),
                    conflict_copy: None,
                    conflict_reason: None,
                    drafted: false,
                    twin: None,
                }),
                signed_in: false,
                ..ListData::default()
            },
            warnings: Vec::new(),
            untracked_view: false,
        };
        assert_eq!(
            list_tty(&out),
            "frontend-design — not managed on this machine\n  \
             /home/u/.claude/skills/frontend-design\nto manage: topos add -g frontend-design \
             --dest <dest>"
        );
    }

    /// A row whose ORIGIN has stopped answering says so itself, in the badge slot and next to the
    /// origin — byte-exact, because this is the line a person reads to learn their copy is frozen.
    /// `[not responding]` REPLACES `[current]`: `current` is measured against an answer nobody
    /// got. The timestamp is the last ANSWER, not the last check — the sweep retries on its own
    /// cadence and its clock advances on failure, so a "last checked" here would always be recent
    /// and read like a blip to ignore, when the point is how stale this copy has become.
    #[test]
    fn a_source_that_stopped_answering_is_said_on_the_rows_it_froze() {
        use topos_types::results::{ListScope, SkillStatus, SourceHealth};
        let row = |health: Option<SourceHealth>| SkillEntry {
            skill: "rust-skills".to_owned(),
            workspace_id: None,
            version_id: "3d".repeat(32),
            bundle_digest: "cd".repeat(32),
            draft: false,
            pending_proposals: Vec::new(),
            source: Some("github.com/leonardomso/rust-skills".to_owned()),
            status: Some(SkillStatus::Current),
            kind: None,
            source_health: health,
            draft_dir: None,
            draft_diverged: None,
            source_missing: false,
        };
        let render = |health: Option<SourceHealth>| {
            list_tty(&ListOutcome {
                data: ListData {
                    scopes: vec![ListScope {
                        scope: "project".to_owned(),
                        manifest: Some("./topos.toml".to_owned()),
                        rows: vec![row(health)],
                        orphans: Vec::new(),
                    }],
                    ..ListData::default()
                },
                warnings: Vec::new(),
                untracked_view: false,
            })
        };

        // Answering: the row is an ordinary current row, and says nothing about health.
        let ok = render(None);
        assert!(
            ok.contains(
                "  rust-skills  rust-skills@3d3d3d3d3d3d  [current]  \
                 from github.com/leonardomso/rust-skills"
            ),
            "{ok}"
        );
        assert!(!ok.contains("not responding"), "{ok}");

        // Stopped: the badge replaces the status, and the last ANSWER dates the staleness.
        let bad = render(Some(SourceHealth {
            answered_at: Some(1_786_000_000_000),
        }));
        assert!(
            bad.contains(
                "  rust-skills  rust-skills@3d3d3d3d3d3d  [not responding]  \
                 from github.com/leonardomso/rust-skills  last answered: 2026-08-06 07:06"
            ),
            "{bad}"
        );
        assert!(!bad.contains("[current]"), "{bad}");
        // No trailing block in ANY state — the listing is one list of bundles.
        assert!(!bad.contains("external sources:"), "{bad}");

        // A source that has NEVER answered cannot be dated, and does not pretend to be.
        let never = render(Some(SourceHealth { answered_at: None }));
        assert!(
            never.contains(
                "[not responding]  from github.com/leonardomso/rust-skills  never answered"
            ),
            "{never}"
        );
        assert!(!never.contains("last answered"), "{never}");
    }

    /// An `off` row is the file's own standing statement: the `[off]` badge, never the
    /// "not applied here yet" note (an off row is exactly the row `update` will not apply).
    #[test]
    fn off_rows_read_off_not_not_applied() {
        use topos_types::results::{ListScope, SkillStatus};
        let out = ListOutcome {
            data: ListData {
                scopes: vec![ListScope {
                    scope: "machine".to_owned(),
                    manifest: Some("~/.topos/topos.toml".to_owned()),
                    rows: vec![SkillEntry {
                        skill: "deploy".to_owned(),
                        workspace_id: None,
                        version_id: "0".repeat(64),
                        bundle_digest: "0".repeat(64),
                        draft: false,
                        pending_proposals: Vec::new(),
                        source: Some("topos.sh/acme".to_owned()),
                        status: Some(SkillStatus::Off),
                        kind: None,
                        source_health: None,
                        draft_dir: None,
                        draft_diverged: None,
                        source_missing: false,
                    }],
                    orphans: Vec::new(),
                }],
                signed_in: false,
                ..ListData::default()
            },
            warnings: Vec::new(),
            untracked_view: false,
        };
        let text = list_tty(&out);
        assert!(text.contains("deploy  [off]"), "{text}");
        assert!(!text.contains("not applied here yet"), "{text}");
    }

    /// The conflict row, byte-exact, in the scope a person is standing in and in the machine-wide
    /// set: what the team published, that every agent folder still holds the author's own version,
    /// where both versions are marked up, BOTH ways out spelled runnable as printed (`-g` for the
    /// machine's set), and the one consequence of taking neither. Project paths read against the
    /// folder the lead line named.
    #[test]
    fn the_conflict_row_names_the_untouched_folder_the_copy_and_both_ways_out() {
        let conflicted =
            |scope: &str, placements: &[(&str, ConflictHolds)], copy_dir: &str| PullSkill {
                merge: Some(MergeReport {
                    theirs_version_id: format!("fed180d80b8a{}", "0".repeat(52)),
                    placements: placements
                        .iter()
                        .map(|(dir, holds)| topos_types::results::ConflictPlacement {
                            dir: (*dir).to_owned(),
                            holds: *holds,
                        })
                        .collect(),
                    copy_dir: Some(copy_dir.to_owned()),
                    ..merge_report(
                        false,
                        vec![ConflictPathReport {
                            path: "SKILL.md".to_owned(),
                            kind: ConflictPathKind::Content,
                        }],
                    )
                }),
                scope: Some(scope.to_owned()),
                ..row("coolify-deploy", PullAction::Conflicted)
            };
        let receipt = |skill: PullSkill, scope: &str| {
            pull_tty(
                &PullData {
                    skills: vec![skill],
                    proposals_awaiting: 0,
                    notices: Vec::new(),
                    sync: Vec::new(),
                    behind_elsewhere: Vec::new(),
                    scope: Some(scope.to_owned()),
                },
                &[],
                &[],
                &[],
                &[],
                0,
            )
        };

        // The project scope: the checkout is named once, at the top, and every path below reads
        // against it.
        let project = "~/Forward/labs/api";
        assert_eq!(
            receipt(
                conflicted(
                    project,
                    &[(
                        "~/Forward/labs/api/.claude/skills/coolify-deploy",
                        ConflictHolds::Yours,
                    )],
                    "~/Forward/labs/api/.topos/state/robertkrajewski/conflicts/coolify-deploy",
                ),
                "project ~/Forward/labs/api"
            ),
            "checked project (~/Forward/labs/api)\n\
             coolify-deploy   the team published fed180d80b8a, and it changes lines you also \
             changed\n\
             \x20   your agents are unaffected — project/.claude/skills/coolify-deploy still holds \
             your version\n\
             \x20   to merge by hand, both versions are marked up here:\n\
             \x20     project/.topos/state/robertkrajewski/conflicts/coolify-deploy/\n\
             \x20   commit your merge — or keep your version, by leaving that folder alone:\n\
             \x20     topos update coolify-deploy --keep-mine\n\
             \x20   take the team's version instead, dropping yours:\n\
             \x20     topos update coolify-deploy --reset\n\
             \x20   you cannot publish this skill until you pick one\n\
             Checked 1 skill: 1 waiting on you."
        );

        // The machine-wide set: `~/` paths stand as they are, and both exits carry `-g` — the
        // command a person can run from anywhere, including inside some other checkout.
        assert_eq!(
            receipt(
                conflicted(
                    "person",
                    &[("~/.claude/skills/coolify-deploy", ConflictHolds::Yours)],
                    "~/.topos/conflicts/coolify-deploy",
                ),
                "machine"
            ),
            "checked machine-wide\n\
             coolify-deploy   the team published fed180d80b8a, and it changes lines you also \
             changed\n\
             \x20   your agents are unaffected — ~/.claude/skills/coolify-deploy still holds your \
             version\n\
             \x20   to merge by hand, both versions are marked up here:\n\
             \x20     ~/.topos/conflicts/coolify-deploy/\n\
             \x20   commit your merge — or keep your version, by leaving that folder alone:\n\
             \x20     topos update -g coolify-deploy --keep-mine\n\
             \x20   take the team's version instead, dropping yours:\n\
             \x20     topos update -g coolify-deploy --reset\n\
             \x20   you cannot publish this skill until you pick one\n\
             Checked 1 skill: 1 waiting on you."
        );

        // Several untouched folders are counted and then spelled out — a number is not a place a
        // person can go and look.
        let many = receipt(
            conflicted(
                "person",
                &[
                    ("~/.claude/skills/coolify-deploy", ConflictHolds::Yours),
                    ("~/.codex/skills/coolify-deploy", ConflictHolds::Yours),
                ],
                "~/.topos/conflicts/coolify-deploy",
            ),
            "machine",
        );
        assert!(
            many.contains(
                "    your agents are unaffected — 2 folders still hold your version:\n\
                 \x20     ~/.claude/skills/coolify-deploy\n\
                 \x20     ~/.codex/skills/coolify-deploy\n"
            ),
            "{many}"
        );

        // A MIXED set — a `--dest` reset took one copy, the person kept working in another, one
        // holds topos's own marked-up tree, and the last is untouched. The aggregate sentence is
        // FALSE of three of the four, so it is not spoken at all: each folder answers for itself,
        // and each line says only what the bytes prove.
        let mixed = receipt(
            conflicted(
                "person",
                &[
                    ("~/.claude/skills/coolify-deploy", ConflictHolds::Yours),
                    ("~/.codex/skills/coolify-deploy", ConflictHolds::Theirs),
                    ("~/.cursor/skills/coolify-deploy", ConflictHolds::NewerEdits),
                    ("~/.agents/skills/coolify-deploy", ConflictHolds::MarkedUp),
                ],
                "~/.topos/conflicts/coolify-deploy",
            ),
            "machine",
        );
        assert!(
            mixed.contains(
                "    what each folder holds:\n\
                 \x20     ~/.claude/skills/coolify-deploy still holds your version\n\
                 \x20     ~/.codex/skills/coolify-deploy holds the team's version\n\
                 \x20     ~/.cursor/skills/coolify-deploy holds your newer edits\n\
                 \x20     ~/.agents/skills/coolify-deploy holds both versions marked up — run one \
                 of the ways out below\n"
            ),
            "{mixed}"
        );
        assert!(
            !mixed.contains("your agents are unaffected"),
            "no sentence may speak for a set it is false of: {mixed}"
        );

        // NO folder holds it. The reassuring line's ABSENCE is what a reader would misread — it
        // looks like the row simply had nothing extra to say — so the row states the loss instead,
        // and the two exits below it are what put the bytes back.
        let none = receipt(
            conflicted("person", &[], "~/.topos/conflicts/coolify-deploy"),
            "machine",
        );
        assert!(
            none.contains(
                "    no agent folder holds this skill right now — either way out below puts it \
                 back\n"
            ),
            "{none}"
        );
        assert!(!none.contains("your agents are unaffected"), "{none}");
        // The exits are still there, and still runnable as printed.
        assert!(
            none.contains("      topos update -g coolify-deploy --keep-mine\n"),
            "{none}"
        );
        assert!(
            none.contains("      topos update -g coolify-deploy --reset\n"),
            "{none}"
        );

        // NO WORKBENCH NAMED. The first exit's usual lead-in points at the folder the line above
        // it names — with no such line, "that folder" pointed at nothing, so the exit says what it
        // does on its own.
        let workbenchless = pull_tty(
            &PullData {
                skills: vec![PullSkill {
                    merge: Some(MergeReport {
                        copy_dir: None,
                        ..conflicted("person", &[], "unused").merge.unwrap()
                    }),
                    ..conflicted("person", &[], "unused")
                }],
                proposals_awaiting: 0,
                notices: Vec::new(),
                sync: Vec::new(),
                behind_elsewhere: Vec::new(),
                scope: Some("machine".to_owned()),
            },
            &[],
            &[],
            &[],
            &[],
            0,
        );
        assert!(!workbenchless.contains("that folder"), "{workbenchless}");
        assert!(
            workbenchless.contains(
                "    keep your version:\n\
                 \x20     topos update -g coolify-deploy --keep-mine\n\
                 \x20   take the team's version instead, dropping yours:\n\
                 \x20     topos update -g coolify-deploy --reset\n"
            ),
            "{workbenchless}"
        );
    }

    /// The one-time orphan resolution's receipt rows, byte-exact: the row leads with its NAME (a
    /// `-` would mark bytes leaving this machine, and a retired record deletes nothing), states why
    /// the record resolved, and then — on its own indented line — where that leaves the files.
    /// Several folders are listed one per line; the no-copies-remain fact stays one line.
    #[test]
    fn released_rows_render_their_fact_byte_exact() {
        let released = |display: Option<&str>, name: &str, note: &str| PullSkill {
            skill: name.to_owned(),
            workspace_id: None,
            observed: 0,
            applied: 0,
            action: PullAction::Released,
            merge: None,
            synced_placements: None,
            scope: Some("person".to_owned()),
            destinations: Vec::new(),
            kept: Vec::new(),
            display: display.map(str::to_owned),
            note: Some(note.to_owned()),
            harnesses: Vec::new(),
            kind: None,
            draft: false,
        };
        // The final copy, composed by the sweep (`ops::orphan_fact`) exactly as it is asserted
        // there: the reason, then the files.
        let one_folder = "acme stopped sharing this — topos will not update it any more\nthe files \
                          stay where they are, and are yours to keep or delete: \
                          ~/.claude/skills/legacy-deploy";
        let data = PullData {
            skills: vec![released(None, "legacy-deploy", one_folder)],
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: Some("machine".to_owned()),
        };
        assert_eq!(
            pull_tty(&data, &[], &[], &[], &[], 0),
            "updated machine-wide\n\
             legacy-deploy   acme stopped sharing this — topos will not update it any more\n\
             \x20   the files stay where they are, and are yours to keep or delete: \
             ~/.claude/skills/legacy-deploy\n\
             Checked 1 skill: 1 no longer shared."
        );

        // Several folders: the same sentence, one folder per line beneath it. And the
        // no-copies-remain fact, which is one line and names no folder at all.
        let data = PullData {
            skills: vec![
                released(
                    Some("@acme/frontend-design"),
                    "frontend-design",
                    "acme stopped sharing this — topos will not update it any more\nthe files stay \
                     where they are, and are yours to keep or delete:\n  \
                     ~/.claude/skills/frontend-design\n  ~/.codex/skills/frontend-design",
                ),
                released(
                    None,
                    "deploy",
                    "its only copy, /private/tmp/deploy, no longer exists — nothing left to manage",
                ),
            ],
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: Some("machine".to_owned()),
        };
        assert_eq!(
            pull_tty(&data, &[], &[], &[], &[], 0),
            "updated machine-wide\n\
             @acme/frontend-design   acme stopped sharing this — topos will not update it any more\n\
             \x20   the files stay where they are, and are yours to keep or delete:\n\
             \x20     ~/.claude/skills/frontend-design\n\
             \x20     ~/.codex/skills/frontend-design\n\
             deploy                  its only copy, /private/tmp/deploy, no longer exists — \
             nothing left to manage\n\
             Checked 2 skills: 2 no longer shared."
        );
    }

    #[test]
    fn list_tty_renders_the_untracked_view_with_scope_summaries() {
        use topos_types::results::ListScope;
        let out = ListOutcome {
            data: ListData {
                scopes: vec![
                    ListScope {
                        scope: "project".to_owned(),
                        manifest: Some("/repo/topos.toml".to_owned()),
                        rows: Vec::new(),
                        orphans: Vec::new(),
                    },
                    ListScope {
                        scope: "machine".to_owned(),
                        manifest: None,
                        rows: Vec::new(),
                        orphans: Vec::new(),
                    },
                ],
                untracked: vec![
                    // A folder several installed agents share: every reader on the folder line.
                    UntrackedEntry {
                        name: "ant".to_owned(),
                        path: "/home/u/.agents/skills/ant".to_owned(),
                        folder: "/home/u/.agents/skills".to_owned(),
                        readers: vec!["warp".to_owned(), "zed".to_owned()],
                        scope: "user".to_owned(),
                        original: None,
                    },
                    UntrackedEntry {
                        name: "bee".to_owned(),
                        path: "/home/u/.agents/skills/bee".to_owned(),
                        folder: "/home/u/.agents/skills".to_owned(),
                        readers: vec!["warp".to_owned(), "zed".to_owned()],
                        scope: "user".to_owned(),
                        original: None,
                    },
                    // A LINK SHELL: the row says where its bytes really are.
                    UntrackedEntry {
                        name: "codex".to_owned(),
                        path: "/home/u/.agents/skills/codex".to_owned(),
                        folder: "/home/u/.agents/skills".to_owned(),
                        readers: vec!["warp".to_owned(), "zed".to_owned()],
                        scope: "user".to_owned(),
                        original: Some("/home/u/.claude/skills/gstack/codex".to_owned()),
                    },
                    UntrackedEntry {
                        name: "zebra".to_owned(),
                        path: "/home/u/.cursor/skills/zebra".to_owned(),
                        folder: "/home/u/.cursor/skills".to_owned(),
                        readers: vec!["cursor".to_owned()],
                        scope: "user".to_owned(),
                        original: None,
                    },
                ],
                signed_in: false,
                ..ListData::default()
            },
            warnings: Vec::new(),
            untracked_view: true,
        };
        let text = list_tty(&out);
        // One summary line per tracked scope — nothing invisible beside the listing.
        assert!(
            text.contains("0 skills tracked in this folder — `topos list`"),
            "{text}"
        );
        assert!(
            text.contains("0 skills machine-wide — `topos list -g`"),
            "{text}"
        );
        // The full listing, grouped by folder: the readers ride the FOLDER line (every installed
        // agent that reads it, comma-separated), and a row carries the skill's name and nothing else.
        assert!(
            text.contains("  /home/u/.agents/skills  [warp, zed]:\n    ant\n    bee\n"),
            "{text}"
        );
        // …and a LINK SHELL's row carries the folder an `add` of it would really take. The home
        // this test runs under is not `/home/u`, so the origin prints in full — the abbreviation
        // is `tty_path`'s one rule, exercised where a test can pin the home.
        assert!(
            text.contains("    codex  \u{2192} /home/u/.claude/skills/gstack/codex\n"),
            "{text}"
        );
        assert!(
            text.contains("  /home/u/.cursor/skills  [cursor]:\n    zebra"),
            "{text}"
        );
        // The one folder header per group — never reprinted per row.
        assert_eq!(
            text.matches("/home/u/.agents/skills  [").count(),
            1,
            "{text}"
        );
    }

    #[test]
    fn list_tty_renders_the_remote_catalog_with_adoption_markers() {
        use topos_types::results::{RemoteAdoption, RemoteChannel, RemoteWorkspace};
        let skill = |name: &str, state| RemoteSkill {
            name: name.to_owned(),
            kind: "skill".to_owned(),
            version_id: "ab".repeat(32),
            open_proposals: if name == "deploy" { 2 } else { 0 },
            state,
        };
        let out = ListOutcome {
            data: ListData {
                remote: vec![RemoteWorkspace {
                    host: "topos.sh".to_owned(),
                    workspace: "acme".to_owned(),
                    workspace_id: "w_acme".to_owned(),
                    channels: vec![
                        RemoteChannel {
                            name: "backend".to_owned(),
                            skills: 3,
                            adopted_in: Some("~/.topos/topos.toml".to_owned()),
                        },
                        RemoteChannel {
                            name: "everyone".to_owned(),
                            skills: 5,
                            adopted_in: None,
                        },
                    ],
                    skills: vec![
                        skill("deploy", RemoteAdoption::AdoptedHere),
                        skill("notes", RemoteAdoption::AdoptedOnMachine),
                        skill("audit", RemoteAdoption::UpdateAvailable),
                        skill("triage", RemoteAdoption::NotAdopted),
                    ],
                }],
                signed_in: true,
                ..ListData::default()
            },
            warnings: vec![crate::message::uncoded_failure(
                "beta: the server did not answer, so this listing skips it. Run the command again \
                 to retry."
                    .to_owned(),
            )],
            untracked_view: false,
        };
        let text = list_tty(&out);
        assert!(text.contains("topos.sh/acme:"), "{text}");
        // Channels: the count + the adopting file when a manifest row adopts one.
        assert!(
            text.contains("backend — 3 skills  (adopted in ~/.topos/topos.toml)"),
            "{text}"
        );
        assert!(text.contains("everyone — 5 skills"), "{text}");
        // The four adoption markers, each honest about the way forward.
        assert!(
            text.contains("deploy@abababababab  skill  (adopted here)  2 open proposal(s)"),
            "{text}"
        );
        assert!(
            text.contains("notes@abababababab  skill  (adopted machine-wide)"),
            "{text}"
        );
        assert!(
            text.contains("audit@abababababab  skill  (update available — `topos update audit`)"),
            "{text}"
        );
        assert!(
            text.contains("triage@abababababab  skill  (not adopted — `topos add triage`)"),
            "{text}"
        );
        // The dead verb never returns; the remote view carries no pointer at itself.
        assert!(!text.contains("topos follow"), "{text}");
        assert!(!text.contains("workspaces offer"), "{text}");
        // The per-workspace degradation warning surfaces.
        assert!(
            text.contains("warning: beta: the server did not answer, so this listing skips it."),
            "{text}"
        );
    }

    #[test]
    fn list_tty_renders_the_deep_dive_and_the_agent_view() {
        use topos_types::results::{
            AgentViewDir, AgentViewEntry, ListDetail, StatusItemState as S,
        };
        // The deep dive is the whole answer: file + line key, version, placements, state.
        let out = ListOutcome {
            data: ListData {
                detail: Some(ListDetail {
                    name: "deploy".to_owned(),
                    scope: Some("machine".to_owned()),
                    source_file: Some("~/.topos/topos.toml".to_owned()),
                    source_key: Some("topos.sh/acme/deploy".to_owned()),
                    feed: None,
                    attribution: Some("assigned by Dana".to_owned()),
                    version: Some("a".repeat(64)),
                    pin: None,
                    placements: vec![
                        "/home/dev/.claude/skills/deploy".to_owned(),
                        "/home/dev/.codex/skills/deploy".to_owned(),
                    ],
                    state: S::Applied,
                    kind: None,
                    harnesses: Vec::new(),
                    mcp_unreachable: None,
                    managed: true,
                    folders: Vec::new(),
                    diverged: Vec::new(),
                    conflict_copy: None,
                    conflict_reason: None,
                    drafted: false,
                    twin: None,
                }),
                signed_in: true,
                ..ListData::default()
            },
            warnings: Vec::new(),
            untracked_view: false,
        };
        let text = list_tty(&out);
        // The headline names the CHECKOUT this whole answer is about — the scope that answered,
        // beside the name, because every line under it belongs to that one copy.
        assert!(text.starts_with("deploy — on this machine\n"), "{text}");
        assert!(
            text.contains("from ~/.topos/topos.toml, line key topos.sh/acme/deploy"),
            "{text}"
        );
        assert!(text.contains("— assigned by Dana"), "{text}");
        assert!(text.contains("version aaaaaaaaaaaa"), "{text}");
        // Every placement on its OWN line — never a comma-joined run of absolute paths.
        assert!(
            text.contains(
                "placed in:\n    /home/dev/.claude/skills/deploy\n    /home/dev/.codex/skills/deploy"
            ),
            "{text}"
        );
        assert!(!text.contains("deploy, "), "{text}");
        assert!(text.ends_with("applied"), "{text}");

        // The agent-eye view: per-dir entries marked managed or untracked.
        let out = ListOutcome {
            data: ListData {
                agent_view: Some(AgentView {
                    agent: "cursor".to_owned(),
                    agent_name: "Cursor".to_owned(),
                    dirs: vec![
                        AgentViewDir {
                            path: "/home/u/.cursor/skills".to_owned(),
                            scope: "user".to_owned(),
                            entries: vec![
                                AgentViewEntry {
                                    name: "deploy".to_owned(),
                                    managed: Some(
                                        "~/.topos/topos.toml:topos.sh/acme/deploy".to_owned(),
                                    ),
                                },
                                AgentViewEntry {
                                    name: "notes".to_owned(),
                                    managed: Some("feed topos.sh/acme".to_owned()),
                                },
                                AgentViewEntry {
                                    name: "stray".to_owned(),
                                    managed: None,
                                },
                                AgentViewEntry {
                                    name: "topos".to_owned(),
                                    managed: Some("built-in".to_owned()),
                                },
                            ],
                        },
                        AgentViewDir {
                            path: "/repo/.cursor/skills".to_owned(),
                            scope: "project".to_owned(),
                            entries: Vec::new(),
                        },
                    ],
                }),
                signed_in: true,
                ..ListData::default()
            },
            warnings: Vec::new(),
            untracked_view: false,
        };
        let text = list_tty(&out);
        assert!(
            text.starts_with("Cursor — what it reads from this folder:"),
            "{text}"
        );
        assert!(text.contains("/home/u/.cursor/skills (user):"), "{text}");
        assert!(
            text.contains("deploy  topos: ~/.topos/topos.toml:topos.sh/acme/deploy"),
            "{text}"
        );
        assert!(text.contains("notes  feed topos.sh/acme"), "{text}");
        assert!(
            text.contains("stray  untracked (`topos add stray` manages it)"),
            "{text}"
        );
        // The placed built-in is topos-managed — never "untracked", never told to add itself.
        assert!(text.contains("topos  topos: built-in"), "{text}");
        assert!(!text.contains("`topos add topos`"), "{text}");
        assert!(text.contains("/repo/.cursor/skills (project):"), "{text}");
        assert!(text.contains("(empty)"), "{text}");
    }

    #[test]
    fn list_footprint_prints_each_path_not_just_a_count() {
        // `--footprint` reports WHAT topos owns (the set `uninstall` deletes), not merely how many.
        // And the header COUNTS, without claiming where: a trigger artifact lives in its harness's
        // own config dir, so "under the topos home" was false of the very rows it introduced.
        let out = ListOutcome {
            data: ListData {
                footprint: Some(vec![
                    "/home/x/.claude/settings.json".to_owned(),
                    "/home/x/.topos/identity".to_owned(),
                    "/home/x/.topos/skills/topos_s00".to_owned(),
                ]),
                ..ListData::default()
            },
            warnings: Vec::new(),
            untracked_view: false,
        };
        let text = list_tty(&out);
        assert!(text.contains("Footprint: 3 paths\n"), "{text}");
        assert!(!text.contains("under the topos home"), "{text}");
        assert!(
            text.contains("/home/x/.claude/settings.json"),
            "a harness-home row is listed too: {text}"
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
            sync_fault: None,
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
    fn status_tty_renders_both_connection_faces() {
        use topos_types::results::{
            AttentionCount, StatusData, StatusRegime, StatusScope, StatusScopeSummary,
            StatusSession, StatusTrigger,
        };
        let connected = StatusData {
            version: "0.1.0".to_owned(),
            server: Some("https://topos.sh/api".to_owned()),
            signed_in: true,
            sessions: vec![StatusSession {
                workspace_id: "w_demo".to_owned(),
                name: "demo".to_owned(),
                display_name: "Demo".to_owned(),
                host: "topos.sh".to_owned(),
                session_status: None,
            }],
            scopes: vec![StatusScope {
                scope: "project".to_owned(),
                manifest: Some("/repo/topos.toml".to_owned()),
                regimes: Vec::new(),
                notes: vec!["off — not currently assigned: topos.sh/demo/gone".to_owned()],
                attention: vec![
                    AttentionCount {
                        kind: "updates-pending".to_owned(),
                        count: 2,
                        command: "topos update".to_owned(),
                    },
                    AttentionCount {
                        kind: "drafts-ahead".to_owned(),
                        count: 1,
                        command: "topos list".to_owned(),
                    },
                ],
            }],
            machine_summary: Some(StatusScopeSummary {
                attention: vec![
                    AttentionCount {
                        kind: "updates-pending".to_owned(),
                        count: 1,
                        command: "topos update -g".to_owned(),
                    },
                    AttentionCount {
                        kind: "assignments-not-applied".to_owned(),
                        count: 3,
                        command: "topos update -g".to_owned(),
                    },
                ],
                command: "topos status -g".to_owned(),
            }),
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
            forge: Vec::new(),
        };
        let text = status_tty(&connected);
        assert!(text.contains("topos 0.1.0"), "{text}");
        assert!(
            text.contains("server: https://topos.sh/api — signed in"),
            "{text}"
        );
        assert!(text.contains("topos.sh/demo (Demo)"), "{text}");
        // The body is the here-scope, led by its governing file.
        assert!(text.contains("This folder — /repo/topos.toml"), "{text}");
        // The attention counts: one line each, ending in the exact command.
        assert!(
            text.contains("2 updates pending — `topos update`"),
            "{text}"
        );
        assert!(text.contains("1 draft ahead — `topos list`"), "{text}");
        // The notes ride verbatim.
        assert!(
            text.contains("off — not currently assigned: topos.sh/demo/gone"),
            "{text}"
        );
        // The machine summary: ITS counts on one line, ending in the expanding command.
        assert!(
            text.contains(
                "machine-wide: 1 update pending, 3 assignments not applied — `topos status -g`"
            ),
            "{text}"
        );
        // The trigger words say what the EVIDENCE supports: `status` only ever reads a footprint,
        // so a present artifact is REGISTERED — never `armed`, which the install receipt reserves
        // for a report that stated a live update moment.
        assert!(text.contains("claude-code: registered"), "{text}");
        assert!(!text.contains("claude-code: armed"), "{text}");
        assert!(
            text.contains("openclaw: not checked — presence needs"),
            "{text}"
        );
        // NO inventory here — health only; the dead vocabulary stays dead on this surface.
        assert!(!text.contains("assigned to you:"), "{text}");
        assert!(!text.contains("following:"), "{text}");
        assert!(!text.contains("your profile"), "{text}");
        assert!(!text.contains("auth login"), "{text}");

        // A machine body with nothing pending says so — and a regime line rides it.
        let quiet = StatusData {
            scopes: vec![StatusScope {
                scope: "machine".to_owned(),
                manifest: None,
                regimes: vec![StatusRegime {
                    host: "topos.sh".to_owned(),
                    workspace: "demo".to_owned(),
                    regime: "adopting all assigned, 1 off".to_owned(),
                }],
                notes: Vec::new(),
                attention: Vec::new(),
            }],
            machine_summary: None,
            ..connected.clone()
        };
        let text = status_tty(&quiet);
        assert!(
            text.contains("Machine-wide — no global manifest; nothing demanded machine-wide"),
            "{text}"
        );
        assert!(
            text.contains("topos.sh/demo — adopting all assigned, 1 off"),
            "{text}"
        );
        assert!(text.contains("nothing pending"), "{text}");

        // The unconnected face states the fix in prose (join by address, or create a workspace).
        let fresh = StatusData {
            server: None,
            signed_in: false,
            sessions: Vec::new(),
            scopes: Vec::new(),
            machine_summary: None,
            triggers: Vec::new(),
            ..connected
        };
        let text = status_tty(&fresh);
        assert!(text.contains("not connected"), "{text}");
        assert!(text.contains("`topos login <workspace-address>`"), "{text}");
        assert!(text.contains("https://topos.sh"), "{text}");

        // The bare-`topos` welcome stays three lines: what topos is + the two ways in.
        let welcome = welcome_tty(&fresh);
        assert_eq!(welcome.lines().count(), 3, "{welcome}");
        assert!(
            welcome.contains("topos login <workspace-address>"),
            "{welcome}"
        );
        assert!(welcome.contains("https://topos.sh"), "{welcome}");
    }

    #[test]
    fn not_enrolled_carries_the_join_template_with_its_needs() {
        let actions = super::next_actions(
            "update",
            &typed(&["update"]),
            &crate::error::ClientError::NotEnrolled,
        );
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "LOGIN_WORKSPACE");
        assert_eq!(
            actions[0].argv,
            vec!["topos", "login", "<workspace-address>", "--json"]
        );
        assert_eq!(actions[0].needs, vec!["workspace-address"]);
        // The prose states the same fix (never a structural mirror without the human half).
        let msg = safe_message(&crate::error::ClientError::NotEnrolled);
        assert!(msg.contains("`topos login <workspace-address>`"), "{msg}");
    }

    #[test]
    fn prose_named_commands_mirror_structurally_on_the_fall_through() {
        // PATH_NOT_NAME's fix is TYPED — the envelope carries the concrete `topos add ./<arg>`.
        let actions = super::next_actions(
            "add",
            &typed(&["add"]),
            &crate::error::ClientError::PathNotName {
                arg: "deploy".to_owned(),
                verb: "add".to_owned(),
            },
        );
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "RUN_COMMAND");
        assert_eq!(actions[0].argv, vec!["topos", "add", "./deploy", "--json"]);
        assert!(actions[0].needs.is_empty());

        // The fix re-runs the VERB THAT REFUSED. It used to say `add` whatever had refused, so a
        // person who typed `publish` was handed a different command than the one they were using.
        for verb in ["publish", "remove"] {
            let actions = super::next_actions(
                verb,
                &typed(&[verb]),
                &crate::error::ClientError::PathNotName {
                    arg: "deploy".to_owned(),
                    verb: verb.to_owned(),
                },
            );
            assert_eq!(
                actions[0].argv,
                vec!["topos", verb, "./deploy", "--json"],
                "{verb}"
            );
        }

        // And the suggestion stays RUNNABLE: a token that already spells a path is not prefixed
        // again. Blind prefixing produced `./~/deploy`, a path to nothing.
        for arg in ["~/deploy", "./deploy", "../deploy", "/opt/deploy"] {
            let err = crate::error::ClientError::PathNotName {
                arg: arg.to_owned(),
                verb: "add".to_owned(),
            };
            let actions = super::next_actions("add", &typed(&["add"]), &err);
            assert_eq!(
                actions[0].argv,
                vec!["topos", "add", arg, "--json"],
                "{arg}"
            );
            // The prose and the structured fix name the SAME command.
            let msg = safe_message(&err);
            assert!(msg.contains(&format!("`topos add {arg}`")), "{msg}");
        }

        // A SESSION-REQUIRED refusal carries its address as an EXECUTABLE argv, typed.
        let actions = super::next_actions(
            "publish",
            &typed(&["publish"]),
            &crate::error::ClientError::SessionRequired {
                address: "acme.test/eng".to_owned(),
                message: "not logged into acme.test/eng — run `topos login acme.test/eng` first"
                    .to_owned(),
            },
        );
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "LOGIN_WORKSPACE");
        assert_eq!(
            actions[0].argv,
            vec!["topos", "login", "acme.test/eng", "--json"]
        );
        assert!(actions[0].needs.is_empty());

        // An expired flow mirrors the join template, needs and all.
        let actions = super::next_actions(
            "login",
            &typed(&["login"]),
            &crate::error::ClientError::Enrollment(
                "this login attempt expired — start over with `topos login <workspace-address>`"
                    .to_owned(),
            ),
        );
        assert_eq!(actions[0].code.as_str(), "LOGIN_WORKSPACE");
        assert_eq!(actions[0].needs, vec!["workspace-address"]);

        // "upgrade topos" is `topos self-update`, structurally.
        let actions = super::next_actions(
            "list",
            &typed(&["list"]),
            &crate::error::ClientError::UnknownSchemaVersion { found: 9, max: 1 },
        );
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "UPDATE_CLI");

        // Divergent placements name the loss-led reset (a describe — nothing dropped yet).
        let actions = super::next_actions(
            "update",
            &typed(&["update"]),
            &crate::error::ClientError::PlacementsDiverged {
                skill: "deploy".to_owned(),
                copies: vec![diverged_copy("/a"), diverged_copy("/b")],
                global: false,
            },
        );
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "RESOLVE_DIVERGED_DRAFT");
        assert!(actions[0].argv.iter().any(|t| t == "--reset"));

        // A message without a backticked `topos …` command mirrors nothing.
        assert!(
            super::next_actions(
                "add",
                &typed(&["add"]),
                &crate::error::ClientError::EmptyBundle
            )
            .is_empty()
        );
    }

    #[test]
    fn a_hostile_value_can_never_become_extra_argv_tokens() {
        // The classic injection: a directory literally named "skill --yes". The typed arm pushes
        // the value as ONE argv element — never re-tokenized — so no `--yes` token exists and the
        // consent flag cannot be smuggled into an executable next action.
        let actions = super::next_actions(
            "add",
            &typed(&["add"]),
            &crate::error::ClientError::PathNotName {
                arg: "skill --yes".to_owned(),
                verb: "add".to_owned(),
            },
        );
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(
            actions[0].argv,
            vec!["topos", "add", "./skill --yes", "--json"],
            "the hostile name stays one inert token"
        );
        assert!(!actions[0].argv.iter().any(|t| t == "--yes"), "{actions:?}");
        // Same discipline on the other typed value arms.
        let actions = super::next_actions(
            "publish",
            &typed(&["publish"]),
            &crate::error::ClientError::HarnessMismatch {
                name: "pwn --yes".to_owned(),
                requested: "cursor".to_owned(),
                folders: vec!["/home/u/.claude/skills".to_owned()],
            },
        );
        assert_eq!(
            actions[0].argv,
            vec!["topos", "publish", "pwn --yes", "--json"]
        );
        let actions = super::next_actions(
            "add",
            &typed(&["add"]),
            &crate::error::ClientError::PlacementOccupied {
                path: "/tmp/x --yes".to_owned(),
            },
        );
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
        let actions = super::mirror_prose_commands("run `topos login <workspace-address>` first");
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0].argv,
            vec!["topos", "login", "<workspace-address>", "--json"]
        );
    }

    #[test]
    fn auth_status_signed_out_carries_the_matching_fix() {
        use crate::ops::AuthStatusData;
        let fresh = AuthStatusData {
            server: None,
            principal: None,
            signed_in: false,
            workspaces: Vec::new(),
            hook_armed: false,
            reporting: Vec::new(),
        };
        let actions = auth_status_next_actions(&fresh);
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "LOGIN_WORKSPACE");
        assert_eq!(actions[0].needs, vec!["workspace-address"]);
        let text = auth_status_tty(&fresh);
        assert!(text.contains("`topos login <workspace-address>`"), "{text}");

        // Previously connected but signed out: the same login template (the retired
        // `topos auth login` spelling names no real command).
        let signed_out = AuthStatusData {
            server: Some("https://topos.sh/api".to_owned()),
            ..fresh
        };
        let actions = auth_status_next_actions(&signed_out);
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "LOGIN_WORKSPACE");
        assert_eq!(actions[0].needs, vec!["workspace-address"]);
        let text = auth_status_tty(&signed_out);
        assert!(text.contains("`topos login <workspace-address>`"), "{text}");
        assert!(!text.contains("auth login"), "{text}");

        // Signed in: nothing to fix.
        let signed_in = AuthStatusData {
            signed_in: true,
            server: Some("https://topos.sh/api".to_owned()),
            principal: None,
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
        // actually clear the block: the `--keep-mine` escape and the `--reset` discard.
        let actions = super::next_actions(
            "publish",
            &typed(&["publish"]),
            &crate::error::ClientError::PublishBlocked {
                skill: "deploy-checklist".to_owned(),
                global: false,
            },
        );
        assert_eq!(actions.len(), 2, "{actions:?}");
        assert!(
            actions[0].argv.iter().any(|t| t == "--keep-mine"),
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

    /// THREE outcomes share the `merged` action and say different things about whose wording
    /// landed, so none of them may share a sentence.
    ///
    /// The `-X ours` exit says what it actually did — this person's wording on the lines both sides
    /// changed, everything else the team wrote taken — and NAMES the files it came in on, because a
    /// reader can only check that claim against files. The HAND exit claims none of that: topos
    /// chose nothing there, so it must not say your wording won (you may well have taken theirs)
    /// and it names no file as taken from the team. A real merge keeps the rebase sentence. Both
    /// exits close on the same fact: what is left is an ordinary draft, and an ordinary `publish`
    /// ships it.
    ///
    /// The discriminator is the row's own `resolved`. It used to be `drop_diff.is_some()`, which
    /// BOTH exits carry — so a hand resolution was announced as "kept your wording", about a tree
    /// the person may have written entirely the team's way.
    #[test]
    fn each_way_a_merge_finishes_says_only_what_it_did() {
        let render_one = |merge: MergeReport| {
            let data = PullData {
                skills: vec![PullSkill {
                    merge: Some(merge),
                    ..row("runbook", PullAction::Merged)
                }],
                proposals_awaiting: 0,
                notices: Vec::new(),
                sync: Vec::new(),
                behind_elsewhere: Vec::new(),
                scope: None,
            };
            pull_tty(&data, &[], &[], &[], &[], 0)
        };
        let kept_mine = |took: Vec<String>| MergeReport {
            drop_diff: Some("--- a\n+++ b\n".to_owned()),
            resolved: Some(MergeResolution::KeepMine),
            took,
            ..merge_report(true, Vec::new())
        };

        // ONE file came along: it is named inline, the destination convention over files.
        let out = render_one(kept_mine(vec!["docs/setup.md".to_owned()]));
        assert!(
            out.contains("kept your wording where you both changed the same lines"),
            "{out}"
        );
        assert!(
            out.contains("took everything else the team changed, in docs/setup.md"),
            "{out}"
        );
        assert!(
            !out.contains("rebased onto the new current"),
            "the rebase sentence belongs to a real merge: {out}"
        );

        // SEVERAL: counted, then spelled out — a bare "2 files" is not something to go and read.
        // And the list is the diffstat against THIS PERSON'S side, so the file they collided in is
        // in it too (their wording won those lines; the team's other changes to that same file
        // still landed). The wording has to carry that: "their other changes TO 2 files" reads as
        // "2 OTHER files", which the contested name then contradicts.
        let out = render_one(kept_mine(vec![
            "ROLLBACK.md".to_owned(),
            "SKILL.md".to_owned(),
        ]));
        assert!(
            out.contains("took everything else the team changed, in 2 files:")
                && out.contains("ROLLBACK.md")
                && out.contains("SKILL.md"),
            "{out}"
        );
        assert!(
            !out.contains("other changes to 2 files"),
            "the phrasing a reader takes as \"2 other files\": {out}"
        );

        // NOTHING of theirs came along — the row claims nothing rather than claiming an empty set.
        let out = render_one(kept_mine(Vec::new()));
        assert!(
            out.contains("kept your wording where you both changed the same lines"),
            "{out}"
        );
        assert!(!out.contains("the team changed, in"), "{out}");

        // THE HAND RESOLUTION: the person's own tree, committed unexamined. It claims nothing about
        // whose wording won and names nothing as taken from the team.
        let out = render_one(MergeReport {
            drop_diff: Some("--- a\n+++ b\n".to_owned()),
            resolved: Some(MergeResolution::ByHand),
            ..merge_report(true, Vec::new())
        });
        assert!(
            out.contains("committed the merge you made by hand, exactly as you left it"),
            "{out}"
        );
        assert!(!out.contains("kept your wording"), "{out}");
        assert!(!out.contains("the team changed"), "{out}");

        // BOTH exits name the one thing left to do, and say that it is nothing special.
        for merge in [
            kept_mine(vec!["docs/setup.md".to_owned()]),
            MergeReport {
                drop_diff: Some("--- a\n+++ b\n".to_owned()),
                resolved: Some(MergeResolution::ByHand),
                ..merge_report(true, Vec::new())
            },
        ] {
            let out = render_one(merge);
            assert!(
                out.contains(
                    "it is an ordinary draft now — `topos publish runbook` ships it like any other"
                ),
                "{out}"
            );
        }

        // A REAL MERGE keeps its own sentence — it finished on its own, so no exit is attributed.
        let out = render_one(merge_report(true, Vec::new()));
        assert!(out.contains("rebased onto the new current"), "{out}");
        assert!(!out.contains("kept your wording"), "{out}");
        assert!(!out.contains("by hand"), "{out}");
        assert!(!out.contains("an ordinary draft now"), "{out}");
    }

    /// A finished merge carries its one next step as runnable argv too, so an agent acts on the
    /// ordinary publish instead of parsing the sentence that names it. Both exits, and nothing from
    /// a merge that was never stopped.
    #[test]
    fn a_finished_merge_offers_the_ordinary_publish() {
        let row_with = |resolved: Option<MergeResolution>| PullSkill {
            merge: Some(MergeReport {
                resolved,
                ..merge_report(true, Vec::new())
            }),
            ..row("runbook", PullAction::Merged)
        };
        let data = |skills: Vec<PullSkill>| PullData {
            skills,
            proposals_awaiting: 0,
            notices: Vec::new(),
            sync: Vec::new(),
            behind_elsewhere: Vec::new(),
            scope: None,
        };

        for how in [MergeResolution::KeepMine, MergeResolution::ByHand] {
            let actions = resolution_next_actions(&data(vec![row_with(Some(how))]));
            assert_eq!(actions.len(), 1, "{how:?}: {actions:?}");
            assert_eq!(
                actions[0].argv,
                vec![
                    "topos".to_owned(),
                    "publish".to_owned(),
                    "runbook".to_owned()
                ],
                "{how:?}"
            );
            // The describe half of a two-phase publish: it dials, and it does not mutate.
            assert_eq!(actions[0].mutates, Some(false), "{how:?}");
            assert_eq!(actions[0].needs_network, Some(true), "{how:?}");
        }
        assert!(
            resolution_next_actions(&data(vec![row_with(None)])).is_empty(),
            "a merge that was never stopped had no exit to finish it"
        );
    }

    /// The reset receipt, whole, in both of its shapes. It states what it discarded and stops
    /// there: the snapshot it took lands in whichever store owns the copy — which is not `~/.topos`
    /// inside a checkout — and there is no command a person could type to get the edits back, so
    /// advertising recovery offered a handle that does not exist. Git says nothing either when it
    /// discards changes, and only names a recovery when it can hand you something to run.
    #[test]
    fn the_reset_receipt_names_what_it_discarded_and_promises_no_recovery() {
        use topos_types::results::ResetMergeOutcome;
        let item = |dest: Option<&str>,
                    others: &[&str],
                    hand_merge: Option<&str>|
         -> topos_types::results::ResetData {
            topos_types::results::ResetData {
                skill: "coolify-deploy".to_owned(),
                workspace_id: None,
                to_version: "abc1234def56".to_owned() + &"0".repeat(52),
                drop_diff: String::new(),
                applied: true,
                dest: dest.map(str::to_owned),
                others_kept: others.iter().map(|s| (*s).to_owned()).collect(),
                global: false,
                hand_merge: hand_merge.map(str::to_owned),
                merge: None,
            }
        };

        assert_eq!(
            super::reset_applied_tty(&[item(
                Some("~/.claude/skills/coolify-deploy"),
                &["~/.agents/skills/coolify-deploy"],
                None
            )]),
            "Reset 'coolify-deploy' in ~/.claude/skills/coolify-deploy to abc1234def56 — that \
             copy's local edits discarded.\nyour other copy in ~/.agents/skills/coolify-deploy \
             keeps its edits"
        );
        assert_eq!(
            super::reset_applied_tty(&[item(None, &[], None)]),
            "Reset 'coolify-deploy' to abc1234def56 — local edits discarded."
        );
        // The one thing a reset DOES hand back: a by-hand merge it never read, so never deleted.
        assert_eq!(
            super::reset_applied_tty(&[item(None, &[], Some("~/.topos/conflicts/coolify-deploy"))]),
            "Reset 'coolify-deploy' to abc1234def56 — local edits discarded.\nyour hand merge is \
             still in ~/.topos/conflicts/coolify-deploy"
        );

        // A NARROWED reset that met a stopped merge and did not end it. The per-copy lines say
        // what this copy lost and what the others keep; the merge line says the thing none of them
        // can — that the decision is still open — and names where to read it.
        let narrowed = topos_types::results::ResetData {
            dest: Some("~/.claude/skills/coolify-deploy".to_owned()),
            others_kept: vec!["~/.agents/skills/coolify-deploy".to_owned()],
            merge: Some(ResetMergeOutcome::StillStopped),
            ..item(None, &[], None)
        };
        assert_eq!(
            super::reset_applied_tty(std::slice::from_ref(&narrowed)),
            "Reset 'coolify-deploy' in ~/.claude/skills/coolify-deploy to abc1234def56 — that \
             copy's local edits discarded.\nyour other copy in ~/.agents/skills/coolify-deploy \
             keeps its edits\n  the merge on 'coolify-deploy' is still stopped (see: topos list \
             coolify-deploy)"
        );
        // Scope-exact, like every other command this CLI prints: a machine-scope reset points at
        // the machine-scope `list`, which is the one that answers about the copy it just took.
        assert_eq!(
            super::reset_applied_tty(&[topos_types::results::ResetData {
                global: true,
                ..narrowed.clone()
            }])
            .lines()
            .last()
            .unwrap(),
            "  the merge on 'coolify-deploy' is still stopped (see: topos list -g coolify-deploy)"
        );
        // The reset that DID end it. No pointer — there is nothing left to decide.
        assert_eq!(
            super::reset_applied_tty(&[topos_types::results::ResetData {
                merge: Some(ResetMergeOutcome::Concluded),
                ..item(None, &[], None)
            }]),
            "Reset 'coolify-deploy' to abc1234def56 — local edits discarded.\n  that was the last \
             copy holding the merge — the team's version stands everywhere"
        );

        // THE MACHINE HALF of the same sentence: a merge left standing carries the two acts that
        // end it, scope-exact and runnable as printed. Without them an agent that reset one copy
        // read `still_stopped` and had nothing to act on.
        assert_eq!(
            super::reset_next_actions(std::slice::from_ref(&narrowed))
                .iter()
                .map(|a| (a.code.as_str().to_owned(), a.argv.join(" ")))
                .collect::<Vec<_>>(),
            vec![
                (
                    "RESOLVE_DIVERGED_DRAFT".to_owned(),
                    "topos update coolify-deploy --keep-mine --json".to_owned()
                ),
                (
                    "RESOLVE_DIVERGED_DRAFT".to_owned(),
                    "topos update coolify-deploy --reset --json".to_owned()
                ),
            ]
        );
        assert_eq!(
            super::reset_next_actions(&[topos_types::results::ResetData {
                global: true,
                ..narrowed
            }])
            .iter()
            .map(|a| a.argv.join(" "))
            .collect::<Vec<_>>(),
            vec![
                "topos update -g coolify-deploy --keep-mine --json".to_owned(),
                "topos update -g coolify-deploy --reset --json".to_owned(),
            ]
        );
        // Nothing left to decide — nothing offered.
        assert!(
            super::reset_next_actions(&[
                topos_types::results::ResetData {
                    merge: Some(ResetMergeOutcome::Concluded),
                    ..item(None, &[], None)
                },
                item(None, &[], None),
            ])
            .is_empty()
        );
    }

    /// A blocked row sends a person to the workbench for what is ACTUALLY in it, and says the same
    /// thing every time it is re-disclosed. A three-way conflict marks both sides up inside the
    /// files; a no-base one has no fork point to mark against, so it writes the team's files beside
    /// this person's instead — and a sweep that re-states a standing block carries no `drop_diff`,
    /// which is what used to send its reader hunting for markers that were never written.
    #[test]
    fn a_blocked_row_names_what_the_workbench_actually_holds() {
        use topos_types::persisted::ConflictReason;

        let render_one = |reason: ConflictReason, drop_diff: Option<String>| {
            let data = PullData {
                skills: vec![PullSkill {
                    merge: Some(MergeReport {
                        reason: Some(reason),
                        drop_diff,
                        copy_dir: Some("~/.topos/conflicts/runbook".to_owned()),
                        ..merge_report(false, Vec::new())
                    }),
                    ..row("runbook", PullAction::Conflicted)
                }],
                proposals_awaiting: 0,
                notices: Vec::new(),
                sync: Vec::new(),
                behind_elsewhere: Vec::new(),
                scope: None,
            };
            pull_tty(&data, &[], &[], &[], &[], 0)
        };

        let marked = "to merge by hand, both versions are marked up here:";
        let beside = "to merge by hand, your files are here with the team's beside them \
                      (.topos-theirs):";
        // The LEAD branches on the same recorded reason the by-hand line does — unrelated
        // histories have no fork point, so no line was ever compared and the row may not say
        // otherwise.
        let collided = "the team published 1b1b1b1b1b1b, and it changes lines you also changed";
        let unrelated = "the team published 1b1b1b1b1b1b, and it shares no history with your copy";

        // The FIRST no-base disclosure, and every LATER one — the recorded reason is the same, so
        // the line is the same. The re-disclosure is the row with no `drop_diff`.
        for drop_diff in [Some("--- a\n+++ b\n".to_owned()), None] {
            let out = render_one(ConflictReason::NoBase, drop_diff);
            assert!(out.contains(beside), "{out}");
            assert!(!out.contains(marked), "{out}");
            assert!(out.contains(unrelated), "{out}");
            assert!(!out.contains(collided), "{out}");
        }
        // A three-way conflict does write markers, first disclosure and every one after.
        for drop_diff in [Some("--- a\n+++ b\n".to_owned()), None] {
            let out = render_one(ConflictReason::ThreeWay, drop_diff);
            assert!(out.contains(marked), "{out}");
            assert!(!out.contains(beside), "{out}");
            assert!(out.contains(collided), "{out}");
            assert!(!out.contains(unrelated), "{out}");
        }
    }

    /// `--keep-mine` with nothing stopped is a wrong command for the state, so the refusal names
    /// the merge — and the merge is what lands the team's non-conflicting changes.
    #[test]
    fn the_no_stopped_merge_refusal_names_the_merge() {
        let err = crate::error::ClientError::NoStoppedMerge {
            skill: "coolify-deploy".to_owned(),
            global: true,
        };
        assert_eq!(
            super::safe_message(&err),
            "no merge has stopped on 'coolify-deploy' — run `topos update -g coolify-deploy` to \
             merge the team's version in; it stops only where you both changed the same lines"
        );
        let actions = super::next_actions(
            "update",
            &typed(&["update", "-g", "coolify-deploy", "--keep-mine"]),
            &err,
        );
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "APPLY_WAITING_UPDATE");
        assert_eq!(
            actions[0].argv,
            vec!["topos", "update", "-g", "coolify-deploy", "--json"]
        );
    }

    /// The offered merge is spelled for the scope the REFUSAL names, never the one the invocation
    /// happened to stand in: a merge waiting machine-wide, offered bare from inside a checkout,
    /// drives the project's copy and refuses all over again — a loop the reader leaves only by
    /// guessing `-g`. Both surfaces carry the one scope, and the TTY says it exactly once.
    #[test]
    fn the_offered_merge_carries_the_scope_the_refusal_names() {
        let machine = crate::error::ClientError::NoStoppedMerge {
            skill: "coolify-deploy".to_owned(),
            global: true,
        };
        let here = crate::error::ClientError::NoStoppedMerge {
            skill: "coolify-deploy".to_owned(),
            global: false,
        };
        // The invocation stood in a project scope and spelled no flag: the machine-wide refusal
        // offers `-g` anyway, and the project-scoped one still offers none.
        let argv = typed(&["update", "coolify-deploy", "--keep-mine"]);
        assert_eq!(
            super::next_actions("update", &argv, &machine)[0].argv,
            typed(&["topos", "update", "-g", "coolify-deploy", "--json"])
        );
        assert_eq!(
            super::next_actions("update", &argv, &here)[0].argv,
            typed(&["topos", "update", "coolify-deploy", "--json"])
        );
        // And the fact wins in the other direction too — a `-g` in the argv never re-scopes an
        // offer for a copy the refusal itself places in this folder's scope.
        assert_eq!(
            super::next_actions(
                "update",
                &typed(&["update", "-g", "coolify-deploy", "--keep-mine"]),
                &here
            )[0]
            .argv,
            typed(&["topos", "update", "coolify-deploy", "--json"])
        );
        // The TTY half is the SAME command, and the sentence has already said it — so the `try:`
        // block stays empty rather than repeating it. That silence is the proof the two surfaces
        // agree byte for byte: an offer at the other scope would not match the sentence, and would
        // print under it as a second, contradicting command.
        for (err, line) in [
            (&machine, "topos update -g coolify-deploy"),
            (&here, "topos update coolify-deploy"),
        ] {
            let offered = super::hint_line(&super::next_actions("update", &argv, err)[0].argv);
            assert_eq!(offered, line);
            let message = super::safe_message(err);
            assert!(
                super::backtick_spans(&message).contains(&line),
                "the sentence must name the offered command verbatim: {message}"
            );
            assert_eq!(
                super::err_hint_tty("update", &argv, err),
                None,
                "the sentence already carries this command: {message}"
            );
        }
    }

    /// The behind refusal is two lines: the fact, then the one command that resolves it. It carries
    /// no `error:` prefix and no `try:` block — the copy IS the answer, and repeating the command
    /// under it would read as a second step. The envelope keeps the single sentence and offers the
    /// same command as its action.
    #[test]
    fn a_publish_behind_refusal_states_the_fact_then_the_one_way_out() {
        let err = crate::error::ClientError::PublishBehind {
            skill: "coolify-deploy".to_owned(),
            global: false,
        };
        assert_eq!(
            super::err_tty(&err),
            "coolify-deploy: your version is behind.\n  update first: topos update coolify-deploy"
        );
        assert_eq!(
            super::err_hint_tty("publish", &typed(&["publish", "coolify-deploy"]), &err),
            None,
            "the command is already printed"
        );
        let actions = super::next_actions("publish", &typed(&["publish", "coolify-deploy"]), &err);
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].code.as_str(), "REBASE_AND_RETRY");
        assert_eq!(
            actions[0].argv,
            vec!["topos", "update", "coolify-deploy", "--json"],
            "{actions:?}"
        );
    }

    /// Both publish refusals are spelled for the copy that REFUSED, not for the directory the
    /// reader happened to stand in. `publish` takes no scope flag and resolves a bare name
    /// home-first, so from inside a checkout the machine copy is what refuses — and an offered
    /// `topos update <skill>` would drive the project copy and refuse all over again. The screen
    /// and the machine-readable action say the same thing, because they are one computation.
    #[test]
    fn both_publish_refusals_carry_the_scope_of_the_copy_that_refused() {
        use crate::error::ClientError;

        // The invocation is identical in every case — a bare `publish` from inside a checkout.
        let argv = typed(&["publish", "coolify-deploy"]);

        let behind_here = ClientError::PublishBehind {
            skill: "coolify-deploy".to_owned(),
            global: false,
        };
        let behind_machine = ClientError::PublishBehind {
            skill: "coolify-deploy".to_owned(),
            global: true,
        };
        assert_eq!(
            super::err_tty(&behind_here),
            "coolify-deploy: your version is behind.\n  update first: topos update coolify-deploy"
        );
        assert_eq!(
            super::err_tty(&behind_machine),
            "coolify-deploy: your version is behind.\n  update first: topos update -g \
             coolify-deploy"
        );
        assert_eq!(
            super::next_actions("publish", &argv, &behind_here)[0].argv,
            vec!["topos", "update", "coolify-deploy", "--json"]
        );
        assert_eq!(
            super::next_actions("publish", &argv, &behind_machine)[0].argv,
            vec!["topos", "update", "-g", "coolify-deploy", "--json"]
        );

        // The block's two exits, both scopes: the flag rides right after the verb, ahead of the
        // name — the one order every offered command uses.
        let blocked_here = ClientError::PublishBlocked {
            skill: "coolify-deploy".to_owned(),
            global: false,
        };
        let blocked_machine = ClientError::PublishBlocked {
            skill: "coolify-deploy".to_owned(),
            global: true,
        };
        let argvs = |e: &ClientError| -> Vec<Vec<String>> {
            super::next_actions("publish", &argv, e)
                .into_iter()
                .map(|a| a.argv)
                .collect()
        };
        assert_eq!(
            argvs(&blocked_here),
            vec![
                typed(&["topos", "update", "coolify-deploy", "--keep-mine", "--json"]),
                typed(&["topos", "update", "coolify-deploy", "--reset", "--json"]),
            ]
        );
        assert_eq!(
            argvs(&blocked_machine),
            vec![
                typed(&[
                    "topos",
                    "update",
                    "-g",
                    "coolify-deploy",
                    "--keep-mine",
                    "--json"
                ]),
                typed(&[
                    "topos",
                    "update",
                    "-g",
                    "coolify-deploy",
                    "--reset",
                    "--json"
                ]),
            ]
        );
        // And the TTY hint block is the same argv, minus the `--json` tail — so a human and an
        // agent are handed the same scope.
        assert_eq!(
            super::err_hint_tty("publish", &argv, &blocked_machine)
                .expect("a blocked publish offers its two exits"),
            "try:\n  topos update -g coolify-deploy --keep-mine\n  topos update -g coolify-deploy \
             --reset"
        );
    }

    #[test]
    fn an_ambiguous_name_offers_one_runnable_command_per_candidate() {
        use crate::error::{ClientError, TargetCandidate};

        // The candidates ARE the ways out, so each becomes the FAILING invocation re-spelled: the
        // verb that refused (read from the command, never assumed) plus that candidate's tokens.
        let err = ClientError::AmbiguousTarget {
            name: "rust-skills".to_owned(),
            candidates: vec![
                TargetCandidate::plain("github.com/leonardomso/rust-skills"),
                // The set member: its reference and the line that delivers it, kept apart.
                TargetCandidate::via(
                    "github.com/leonardomso/rust-skills/rust-skills",
                    "github.com/leonardomso/rust-skills",
                ),
            ],
            // This folder's file — the refused `topos remove rust-skills` carried no `-g`.
            global: false,
        };
        let actions = super::next_actions("remove", &typed(&["remove", "rust-skills"]), &err);
        assert_eq!(actions.len(), 2, "{actions:?}");
        assert_eq!(
            actions[0].argv,
            vec![
                "topos",
                "remove",
                "github.com/leonardomso/rust-skills",
                "--json"
            ]
        );
        // A set-member spelling carries its own selector: it stays the SEVERAL tokens it is, so the
        // command runs — quoting it into one argument would hand back something unrunnable.
        assert_eq!(
            actions[1].argv,
            vec![
                "topos",
                "remove",
                "github.com/leonardomso/rust-skills/rust-skills",
                "--via",
                "github.com/leonardomso/rust-skills",
                "--json"
            ]
        );
        for a in &actions {
            assert_eq!(a.code.as_str(), "RUN_COMMAND");
            assert!(
                a.needs.is_empty(),
                "concrete argv, no template holes: {a:?}"
            );
        }

        // The TTY prints those same commands under the one lead-in — and the message above them no
        // longer inlines the list, so nothing is said twice.
        let hint = super::err_hint_tty("remove", &typed(&["remove", "rust-skills"]), &err)
            .expect("an ambiguity always has a way out");
        assert_eq!(
            hint,
            "try:\n  topos remove github.com/leonardomso/rust-skills\n  topos remove \
             github.com/leonardomso/rust-skills/rust-skills --via \
             github.com/leonardomso/rust-skills"
        );
        let message = safe_message(&err);
        assert_eq!(
            message,
            "'rust-skills' is ambiguous here — more than one reference answers to it"
        );
        assert!(
            !message.contains("github.com"),
            "the candidates ride the actions, not the sentence: {message}"
        );

        // The SAME refusal from another verb rebuilds THAT verb — `add`'s lift of a switched-off
        // row must never be answered with a `remove` — and it keeps the invocation's `-g`: the two
        // manifests are two files, so the bare form would edit the one nobody asked about.
        let lift = ClientError::AmbiguousTarget {
            name: "deploy".to_owned(),
            candidates: vec![
                TargetCandidate::plain("topos.sh/acme/deploy"),
                TargetCandidate::plain("topos.sh/beta/deploy"),
            ],
            // The switch it lifts lives in the machine-wide file, and the invocation said so.
            global: true,
        };
        let lift_argv = typed(&["add", "-g", "deploy"]);
        let actions = super::next_actions("add", &lift_argv, &lift);
        assert_eq!(
            actions[0].argv,
            vec!["topos", "add", "-g", "topos.sh/acme/deploy", "--json"]
        );
        assert_eq!(
            actions[1].argv,
            vec!["topos", "add", "-g", "topos.sh/beta/deploy", "--json"]
        );
        // The flag reaches the human line too — one surface never offers what the other withholds.
        assert_eq!(
            super::err_hint_tty("add", &lift_argv, &lift),
            Some(
                "try:\n  topos add -g topos.sh/acme/deploy\n  topos add -g topos.sh/beta/deploy"
                    .to_owned()
            )
        );
        // The scope rides the ERROR, not the verb: the same shape without `-g` stays bare.
        let local = ClientError::AmbiguousTarget {
            name: "deploy".to_owned(),
            candidates: vec![TargetCandidate::plain("topos.sh/acme/deploy")],
            global: false,
        };
        assert_eq!(
            super::next_actions("add", &typed(&["add", "deploy"]), &local)[0].argv,
            vec!["topos", "add", "topos.sh/acme/deploy", "--json"]
        );

        // The envelope keeps the machine-readable half unchanged (a consumer reading
        // `data.candidates` is untouched by the message losing the list), and mirrors the actions.
        let envelope = super::err_envelope("remove", &typed(&["remove", "rust-skills"]), &err);
        assert_eq!(
            envelope.data["candidates"],
            serde_json::json!([
                "github.com/leonardomso/rust-skills",
                "github.com/leonardomso/rust-skills/rust-skills --via \
                 github.com/leonardomso/rust-skills"
            ])
        );
        let wire = envelope.error.expect("a refusal carries its error");
        assert_eq!(wire.code, "AMBIGUOUS_NAME");
        assert_eq!(wire.next_actions.len(), 2);
    }

    #[test]
    fn an_ambiguity_offers_a_command_only_where_the_argv_is_fully_determined() {
        use crate::error::{ClientError, TargetCandidate};

        // `protect <target> [<level>]` is NOT reconstructible from verb + candidate: the optional
        // level positional decides WHICH ACT it is, so re-spelling `topos protect deploy open` as
        // `topos protect <candidate>` would offer the opposite (a tighten for a loosen). Nothing
        // is offered — the short refusal and `data.candidates` stand alone, exactly the surface
        // this verb had before commands were ever rebuilt.
        let err = ClientError::AmbiguousTarget {
            name: "deploy".to_owned(),
            candidates: vec![
                TargetCandidate::plain("acme/skills/deploy"),
                TargetCandidate::plain("beta/skills/deploy"),
            ],
            global: false,
        };
        let one_target = typed(&["protect", "deploy"]);
        assert!(super::next_actions("protect", &one_target, &err).is_empty());
        // No RUNNABLE offer — but the ways out are still SHOWN, as spellings (a rebuilt protect
        // could reverse the requested level, so a command is never promised; a bare "more than
        // one answers" that refused to say which would be worse than the old inline list).
        assert_eq!(
            super::err_hint_tty("protect", &one_target, &err),
            Some("it matches:\n  acme/skills/deploy\n  beta/skills/deploy".to_owned())
        );
        // The machine half is untouched by the silence: the candidates still ride the envelope.
        let envelope = super::err_envelope("protect", &one_target, &err);
        assert_eq!(
            envelope.data["candidates"],
            serde_json::json!(["acme/skills/deploy", "beta/skills/deploy"])
        );
        assert!(envelope.next_actions.is_empty());
        // The two verbs whose whole invocation IS the target keep their offers.
        for verb in ["remove", "add"] {
            assert_eq!(
                super::next_actions(verb, &typed(&[verb, "deploy"]), &err).len(),
                2,
                "{verb}"
            );
        }
        // A verb nobody has vetted stays silent rather than guessing at its argv.
        assert!(super::next_actions("publish", &typed(&["publish", "deploy"]), &err).is_empty());
    }

    #[test]
    fn an_ambiguity_offers_nothing_when_the_rebuild_would_not_be_the_whole_request() {
        use crate::error::{ClientError, TargetCandidate};

        // The verb gate is not enough: `remove` takes SEVERAL targets and flags, and a rebuild
        // says only "this verb, this candidate". Where the ambiguous token was not the whole
        // request, the rebuilt command would succeed while quietly doing less than was asked.
        let err = ClientError::AmbiguousTarget {
            name: "foo".to_owned(),
            candidates: vec![
                TargetCandidate::plain("github.com/o/r/foo"),
                TargetCandidate::plain("github.com/p/q/foo"),
            ],
            global: false,
        };
        let offered = |tokens: &[&str]| super::next_actions("remove", &typed(tokens), &err).len();

        // A BATCH: `topos remove foo <candidate>` abandons `bar` and still exits 0.
        assert_eq!(offered(&["remove", "foo", "bar"]), 0);
        // `--yes`: carrying it over would APPLY the removal of a candidate the caller has never
        // seen described — they consented to losing `foo`, not to losing whichever of these two
        // they are only now choosing between. Dropping it silently would be no better: the
        // rebuilt command would then describe where they asked to apply. Neither is offered.
        assert_eq!(offered(&["remove", "foo", "--yes"]), 0);
        // Any other flag is part of a request the rebuild cannot restate — including a
        // value-taking one, whose value would read as a second target.
        assert_eq!(offered(&["remove", "foo", "--via", "github.com/o/r"]), 0);
        assert_eq!(offered(&["remove", "--workspace", "acme", "foo"]), 0);
        // And with no invocation in hand at all, the gate fails toward silence.
        assert_eq!(offered(&[]), 0);

        // The shapes that ARE the whole request keep their offers — including the scope flag and
        // the machine half, which the rebuild restates itself.
        assert_eq!(offered(&["remove", "foo"]), 2);
        assert_eq!(offered(&["remove", "-g", "foo"]), 2);
        assert_eq!(offered(&["remove", "--global", "foo"]), 2);
        assert_eq!(offered(&["--json", "remove", "foo"]), 2);

        // Withheld does NOT mean uninformative: every way out still rides the envelope, and the
        // TTY lists the candidate SPELLINGS — as facts, never as commands it cannot honestly
        // promise (byte-identical to `data.candidates`, so both surfaces name the same ways out).
        let batch = typed(&["remove", "foo", "bar", "--yes"]);
        let envelope = super::err_envelope("remove", &batch, &err);
        assert_eq!(
            envelope.data["candidates"],
            serde_json::json!(["github.com/o/r/foo", "github.com/p/q/foo"])
        );
        assert!(envelope.next_actions.is_empty());
        assert_eq!(
            super::err_hint_tty("remove", &batch, &err),
            Some("it matches:\n  github.com/o/r/foo\n  github.com/p/q/foo".to_owned())
        );
    }

    #[test]
    fn a_candidate_reference_stays_one_token_however_it_is_spelled() {
        use crate::error::{ClientError, TargetCandidate};

        // A local reference is a PATH, and a path may hold whitespace — including bytes that read
        // as flags. The reference is carried as its own field and pushed as ONE argv element, so
        // no directory name can grow into argv of its own (least of all a consent flag).
        let hostile = TargetCandidate::via("./my skill --yes", "github.com/o/r");
        assert_eq!(
            hostile.argv_tokens(),
            vec!["./my skill --yes", "--via", "github.com/o/r"]
        );
        let err = ClientError::AmbiguousTarget {
            name: "my skill".to_owned(),
            candidates: vec![hostile.clone()],
            global: false,
        };
        let argv = typed(&["remove", "my skill --yes"]);
        let actions = super::next_actions("remove", &argv, &err);
        assert_eq!(
            actions[0].argv,
            vec![
                "topos",
                "remove",
                "./my skill --yes",
                "--via",
                "github.com/o/r",
                "--json"
            ]
        );
        assert!(
            !actions[0].argv.iter().any(|t| t == "--yes"),
            "the hostile path is one inert token: {actions:?}"
        );
        // The human line quotes it back into one argument, so the offered command still runs.
        assert_eq!(
            super::err_hint_tty("remove", &argv, &err),
            Some("try:\n  topos remove './my skill --yes' --via github.com/o/r".to_owned())
        );
        // And the wire spelling is the two parts as a command line reads them — unchanged shape.
        assert_eq!(hostile.spelling(), "./my skill --yes --via github.com/o/r");
    }

    #[test]
    fn the_login_receipt_is_signed_in_led_and_first_connection_disclosed() {
        use super::session_login_tty;
        use topos_types::results::{EnrollmentPending, LoginData};

        let connected = |feed_row_added: bool| LoginData {
            workspace_id: "w_acme".to_owned(),
            host: "topos.sh".to_owned(),
            name: "acme".to_owned(),
            display_name: Some("Acme".to_owned()),
            server: Some("https://topos.sh/api".to_owned()),
            session_id: Some("sn_1".to_owned()),
            session_status: "active".to_owned(),
            delivered: Some(2),
            delivered_names: vec!["deploy".to_owned(), "code-review".to_owned()],
            pending: None,
            currency: None,
            triggers: Vec::new(),
            manifest_note: None,
            user: Some("robert".to_owned()),
            feed_row_added,
            undo: if feed_row_added {
                vec!["remove".to_owned(), "-g".to_owned(), "@acme".to_owned()]
            } else {
                Vec::new()
            },
        };

        // The FIRST connection: who + where, what the feed line means, the paste-ready inverse —
        // byte-exact, the ONE shape for a browser grant and a lane connect alike.
        assert_eq!(
            session_login_tty(&connected(true)),
            "signed in to topos.sh/acme as robert\n\
             what acme delivers to you installs on this machine\n\
             (undo: topos remove -g @acme)"
        );

        // A RE-LOGIN prints the first line alone: the feed line's absence from the file is
        // deliberate, and no count or skill list dresses the receipt up.
        assert_eq!(
            session_login_tty(&connected(false)),
            "signed in to topos.sh/acme as robert"
        );

        // Without the identity read the suffix is omitted — never guessed.
        let mut nameless = connected(false);
        nameless.user = None;
        assert_eq!(session_login_tty(&nameless), "signed in to topos.sh/acme");

        // A failed feed-line write is disclosed, never a failed login.
        let mut refused = connected(false);
        refused.manifest_note = Some("~/.topos/topos.toml could not be written".to_owned());
        assert_eq!(
            session_login_tty(&refused),
            "signed in to topos.sh/acme as robert\n\
             manifest: ~/.topos/topos.toml could not be written"
        );

        // A pending session promises nothing until an owner acts (unchanged copy).
        let mut waiting = connected(false);
        waiting.name = "eng".to_owned();
        waiting.session_status = "pending".to_owned();
        let text = session_login_tty(&waiting);
        assert!(
            text.starts_with("Connected to eng — the session awaits an owner's approval"),
            "{text}"
        );
        assert!(text.ends_with("\nUndo: topos logout eng"), "{text}");

        // The awaiting-browser half: a BARE login has no workspace to name, so it leads with the
        // server and says where the choice happens; a named one spells the address.
        let pending = |name: &str| LoginData {
            workspace_id: String::new(),
            host: "topos.example.com".to_owned(),
            name: name.to_owned(),
            display_name: None,
            server: Some("https://topos.example.com/api".to_owned()),
            session_id: None,
            session_status: "awaiting-approval".to_owned(),
            delivered: None,
            delivered_names: Vec::new(),
            pending: Some(EnrollmentPending {
                verification_uri: "https://topos.example.com/verify".to_owned(),
                user_code: "AB12-CD34".to_owned(),
                expires_at: None,
                interval_secs: Some(5),
            }),
            currency: None,
            triggers: Vec::new(),
            manifest_note: None,
            user: None,
            feed_row_added: false,
            undo: Vec::new(),
        };
        let bare = session_login_tty(&pending(""));
        assert!(
            bare.starts_with(
                "Logging in to topos.example.com — choose your workspace in the browser."
            ),
            "{bare}"
        );
        assert!(
            bare.contains("Open: https://topos.example.com/verify"),
            "{bare}"
        );
        assert!(
            bare.contains("Then re-run `topos login` to finish."),
            "{bare}"
        );
        let named = session_login_tty(&pending("eng"));
        assert!(
            named.starts_with("Logging in to topos.example.com/eng."),
            "{named}"
        );
    }

    #[test]
    fn a_rebuilt_update_way_out_preserves_the_refused_invocations_scope() {
        use crate::error::ClientError;
        let err = ClientError::PlacementsDiverged {
            skill: "docs".into(),
            copies: vec![diverged_copy("/x/a"), diverged_copy("/x/b")],
            global: true,
        };
        // An `update -g` that refused is offered a `-g` resolution — the bare spelling would act
        // on the OTHER scope's copy from inside a checkout.
        let argv: Vec<String> = ["update", "-g", "docs", "--reset"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let actions = super::next_actions("update", &argv, &err);
        assert_eq!(
            actions[0].argv,
            vec!["topos", "update", "-g", "docs", "--reset", "--json"]
        );
        // A publish-origin refusal has no scope flag in its ARGV to preserve — `publish` takes
        // none — so its scope comes off the typed error instead: the copy that actually refused.
        let publish_argv: Vec<String> = ["publish", "docs"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let blocked = ClientError::PublishBlocked {
            skill: "docs".into(),
            global: false,
        };
        let actions = super::next_actions("publish", &publish_argv, &blocked);
        assert_eq!(
            actions[1].argv,
            vec!["topos", "update", "docs", "--reset", "--json"]
        );
        let blocked_machine = ClientError::PublishBlocked {
            skill: "docs".into(),
            global: true,
        };
        let actions = super::next_actions("publish", &publish_argv, &blocked_machine);
        assert_eq!(
            actions[1].argv,
            vec!["topos", "update", "-g", "docs", "--reset", "--json"]
        );
    }

    #[test]
    fn the_tty_error_hint_renders_the_json_next_actions_and_never_repeats_the_prose() {
        use crate::error::ClientError;

        // A refusal whose JSON carries a way out the PROSE never spells: both commands render,
        // indented, under the one lead-in — this is the gap the block exists to close.
        let blocked = ClientError::PublishBlocked {
            skill: "deploy".to_owned(),
            global: false,
        };
        let hint = super::err_hint_tty("publish", &typed(&["publish"]), &blocked)
            .expect("a blocked publish offers its two exits");
        assert_eq!(
            hint, "try:\n  topos update deploy --keep-mine\n  topos update deploy --reset",
            "the machine surface's argv, minus its `--json` tail"
        );

        // A refusal that already taught its fix in backticks adds nothing underneath it.
        assert_eq!(
            super::err_hint_tty(
                "upgrade",
                &typed(&["upgrade"]),
                &ClientError::UpgradeAmbiguous
            ),
            None
        );
        assert_eq!(
            super::err_hint_tty("update", &typed(&["update"]), &ClientError::NotEnrolled),
            None
        );

        // A transient failure says so, driven by the TYPED outcome — never re-classified here.
        assert_eq!(
            super::err_hint_tty(
                "add",
                &typed(&["add"]),
                &ClientError::RemoteFetch {
                    msg: "acme/skills".to_owned(),
                    fault: crate::error::FetchFault::Unreachable
                }
            ),
            Some("this is often transient — running it again is safe".to_owned())
        );

        // A PERMANENT fetch failure (a 404, a malformed reference) must NOT invite a retry — the
        // hint is driven by the same typed outcome, so it stays silent here.
        assert_eq!(
            super::err_hint_tty(
                "add",
                &typed(&["add"]),
                &ClientError::RemoteFetch {
                    msg: "acme/skills".to_owned(),
                    fault: crate::error::FetchFault::Gone
                }
            ),
            None
        );

        // A template hole stays bare (it comes from the compile-time table, never a runtime value),
        // while a runtime value carrying whitespace is quoted back into ONE argument.
        let session = ClientError::SessionRequired {
            address: "acme test/eng".to_owned(),
            message: "not logged in".to_owned(),
        };
        assert_eq!(
            super::err_hint_tty("publish", &typed(&["publish"]), &session),
            Some("try:\n  topos login 'acme test/eng'".to_owned())
        );

        // A permanent failure with nothing runnable adds no line at all.
        assert_eq!(
            super::err_hint_tty("add", &typed(&["add"]), &ClientError::EmptyBundle),
            None,
            "no invented hint where there is no way out"
        );
    }

    #[test]
    fn both_halves_of_the_version_floor_offer_the_binary_they_need() {
        use topos_types::ActionCode;

        use crate::error::ClientError;

        // The server refused this build (its 426): the message states the floor, the block states
        // the one command that clears it.
        let refused = ClientError::UpdateRequired {
            min: Some("0.1.15".to_owned()),
        };
        assert_eq!(
            super::safe_message(&refused),
            "this server no longer speaks to this topos version — it requires at least topos 0.1.15"
        );
        assert_eq!(
            super::err_hint_tty("update", &typed(&["update"]), &refused),
            Some("try:\n  topos self-update".to_owned())
        );
        // A refusal that named no floor still refuses, and still offers the same one way out.
        let bare = ClientError::UpdateRequired { min: None };
        assert!(super::safe_message(&bare).ends_with("it requires a newer topos"));
        assert_eq!(
            super::err_hint_tty("update", &typed(&["update"]), &bare),
            Some("try:\n  topos self-update".to_owned())
        );

        // The mirror image — THIS build is ahead of the server. The remedy that belongs to another
        // person rides the prose; the one this machine can run rides as an argv, pinned to the
        // server's own release.
        let ahead = ClientError::ServerTooOld {
            server_version: "0.1.9".to_owned(),
        };
        assert_eq!(
            super::err_hint_tty("login", &typed(&["login"]), &ahead),
            Some("try:\n  topos self-update --version v0.1.9".to_owned())
        );
        let actions = super::next_actions("login", &typed(&["login"]), &ahead);
        assert_eq!(actions[0].code, ActionCode::SelfUpdate);
        assert_eq!(actions[0].mutates, Some(true));
    }
}
