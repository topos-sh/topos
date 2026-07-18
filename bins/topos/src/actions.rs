//! The ONE owner of next-action construction + safety metadata.
//!
//! Every `next_actions` entry the CLI (and the fixture generator — xtask calls this same module)
//! emits is built by [`next_action`](crate::actions::next_action), which fills the envelope's optional `mutates` /
//! `needs_network` / `risk_note` fields from the single rules table below. No call site carries a
//! safety literal, so the classification cannot drift between surfaces — and a new action code
//! gets its safety story HERE or not at all (absent = unknown, the honest default).
//!
//! The table keys on the ACTION CODE. For the codes whose argv is the whole story
//! (`APPLY_DESCRIBED` — the paste-ready `--yes` of any two-phase verb; the diff/paging codes),
//! the code alone cannot answer "does this dial the plane?", so the rule refines by the argv's
//! VERB — still inside this one module, never at a call site.

use topos_types::{ActionCode, NextAction};

/// Build a [`NextAction`], filling the safety metadata from the one rules table.
#[must_use]
pub fn next_action(code: ActionCode, argv: Vec<String>) -> NextAction {
    let Safety {
        mutates,
        needs_network,
        risk_note,
    } = safety(&code, &argv);
    NextAction {
        code,
        argv,
        mutates,
        needs_network,
        risk_note,
    }
}

/// The classified safety of one action. `None` = unknown (deliberately absent from the envelope).
struct Safety {
    mutates: Option<bool>,
    needs_network: Option<bool>,
    risk_note: Option<String>,
}

impl Safety {
    fn new(mutates: Option<bool>, needs_network: Option<bool>, risk_note: Option<&str>) -> Self {
        Self {
            mutates,
            needs_network,
            risk_note: risk_note.map(str::to_owned),
        }
    }
}

/// The rules table. Total over the KNOWN vocabulary (`topos_types::KNOWN_ACTION_CODES`) plus the
/// open codes this binary emits; anything else stays fully unknown (all three fields absent).
fn safety(code: &ActionCode, argv: &[String]) -> Safety {
    match code.as_str() {
        // A read that helps resolve a name — no writes, no plane.
        "DISAMBIGUATE_NAME" => Safety::new(Some(false), Some(false), None),
        // Update-shaped actions: they land bytes on this machine from the plane.
        "REBASE_AND_RETRY" | "APPLY_WAITING_UPDATE" | "UPDATE_SKILLS" => {
            Safety::new(Some(true), Some(true), None)
        }
        "RESOLVE_DIVERGED_DRAFT" => resolve_diverged_draft(argv),
        "PROPOSE_PUBLISH" => Safety::new(
            Some(true),
            Some(true),
            Some("opens a proposal visible to the whole workspace"),
        ),
        // Not self-service: no argv executes — asking a human changes nothing here.
        "REQUEST_ACCESS" | "CONTACT_ADMIN" => Safety::new(Some(false), Some(false), None),
        // RETRY re-runs the CALLER's own previous command (the argv is empty by design), so whether
        // it mutates is that command's story — unknown here. It only ever follows a plane-retryable
        // outcome, so the retry certainly dials.
        "RETRY" => Safety::new(None, Some(true), None),
        // The byte-budget escape: re-run the same diff uncapped. A read; the plane is dialed only
        // when an endpoint is plane-side (a `<ref>` on `diff`, always for a proposal review).
        "FETCH_FULL_DIFF" => Safety::new(Some(false), Some(diff_dials_plane(argv)), None),
        // The row-page escape: the same enumeration, next page. A read; `list` dials only under
        // `--remote`, `log` only when the skill is followed on an enrolled install (unknowable
        // from the argv alone — left absent there).
        "NEXT_PAGE" => Safety::new(Some(false), page_dials_plane(argv), None),
        // The pending device flow's resume: polling promotes the approval into a stored credential.
        "ENROLL_RESUME" => Safety::new(
            Some(true),
            Some(true),
            Some("completes device enrollment — a credential is stored on this machine"),
        ),
        // The review inbox is a pure plane read.
        "REVIEW_INBOX" => Safety::new(Some(false), Some(true), None),
        // The `topos upgrade` disambiguation pair.
        "UPDATE_CLI" => Safety::new(
            Some(true),
            Some(true),
            Some("replaces the topos binary with the latest release"),
        ),
        // The keep-as-yours salvage: re-adopt the RETAINED local copy — offline by construction.
        "KEEP_AS_YOURS" => Safety::new(
            Some(true),
            Some(false),
            Some("re-adopts the retained copy as a new local skill with no upstream"),
        ),
        // The paste-ready `--yes` of a two-phase describe: applying ALWAYS mutates (that is its
        // point); whether it dials — and what deserves a caution — is the verb's.
        "APPLY_DESCRIBED" => apply_described(argv),
        // An unrecognized code stays fully unknown — never guessed.
        _ => Safety::new(None, None, None),
    }
}

/// The per-shape refinement for `RESOLVE_DIVERGED_DRAFT` — WHICH act resolves the divergence is the
/// argv's story. The `--onto-current` escape commits YOUR bytes onto current and clears a recorded
/// conflict (a local resolution — no plane call in the blocked state this action is emitted for); a
/// bare `--reset` only DESCRIBES the loss-led discard (its own describe carries the `--yes`); the
/// plain targeted update runs the three-way merge.
fn resolve_diverged_draft(argv: &[String]) -> Safety {
    if has_flag(argv, "--onto-current") {
        return Safety::new(
            Some(true),
            Some(false),
            Some(
                "commits YOUR bytes (your edited resolution, or your original draft) onto current, \
                 dropping the team's side of the merge",
            ),
        );
    }
    if has_flag(argv, "--reset") {
        return Safety::new(
            Some(false),
            Some(false),
            Some(
                "describes the discard; applying it drops your local draft so the team's version \
                 wins (a snapshot is kept in the sidecar store)",
            ),
        );
    }
    Safety::new(
        Some(true),
        Some(true),
        Some("runs the three-way merge over your local draft (the draft is snapshotted first)"),
    )
}

/// The verb token of a `topos …` argv (`argv[1]`, skipping the binary name), if present.
fn verb(argv: &[String]) -> Option<&str> {
    argv.get(1).map(String::as_str)
}

fn has_flag(argv: &[String], flag: &str) -> bool {
    argv.iter().any(|a| a == flag)
}

/// Whether a `FETCH_FULL_DIFF` argv reaches the plane: a bare `topos diff <skill>` is the local
/// draft↔current read; any `<ref>` endpoint (`current..<hash>` / `<hash>` / `<a>..<b>`) fetches +
/// re-verifies plane bytes. The ref is the second positional after the verb — value-taking flags
/// (`--max-bytes <n>`) are skipped WITH their value, so a numeric value never reads as a ref.
fn diff_dials_plane(argv: &[String]) -> bool {
    let mut positionals = 0usize;
    let mut it = argv.iter().skip(2); // "topos", "diff"
    while let Some(a) = it.next() {
        if a == "--max-bytes" {
            it.next(); // its value
            continue;
        }
        if a.starts_with('-') {
            continue;
        }
        positionals += 1;
    }
    positionals >= 2 // <skill> + a <ref>
}

/// Whether a `NEXT_PAGE` argv reaches the plane, when the argv alone can answer: `list` dials only
/// under `--remote`; `log` depends on enrollment + follow state → unknown.
fn page_dials_plane(argv: &[String]) -> Option<bool> {
    match verb(argv) {
        Some("list") => Some(has_flag(argv, "--remote")),
        _ => None,
    }
}

/// The per-verb refinement for `APPLY_DESCRIBED` — the argv IS the executable, so the verb decides
/// the network story and the caution. Every apply mutates.
fn apply_described(argv: &[String]) -> Safety {
    let (needs_network, risk_note): (Option<bool>, Option<&str>) = match verb(argv) {
        // Team-visible writes.
        Some("publish") => (
            Some(true),
            Some(if has_flag(argv, "--propose") {
                "opens a proposal visible to the whole workspace"
            } else {
                "ships these bytes to the team — every follower receives them"
            }),
        ),
        Some("revert") => (
            Some(true),
            Some("moves the team's current for every follower (a forward move; nothing deleted)"),
        ),
        Some("review") => (Some(true), Some("settles the proposal for the whole team")),
        Some("protect") | Some("channel") | Some("invite") => (Some(true), None),
        // Subscription rows + delivery.
        Some("follow") => (Some(true), None),
        Some("unfollow") => (
            Some(true),
            Some("delivery stops on every device of yours (local copies stay, frozen)"),
        ),
        // `update --reset --yes` is the loss-led local discard; a plain `update … --yes` syncs.
        Some("update") if has_flag(argv, "--reset") => (
            Some(false),
            Some("discards your local edits (a snapshot is kept in the sidecar store)"),
        ),
        Some("update") => (Some(true), None),
        // `remove --yes`: a followed skill's exclusion is a plane row; a local copy's delete is
        // offline. The argv cannot tell them apart → network unknown.
        Some("remove") => (
            None,
            Some(
                "removes the skill from this machine (a followed skill keeps its canonical bytes)",
            ),
        ),
        // Local-only applies.
        Some("add") => (Some(false), None),
        Some("uninstall") => (
            Some(false),
            Some("deletes the ~/.topos sidecar tree (the stored credential goes with it)"),
        ),
        Some("auth") => (Some(true), None),
        _ => (None, None),
    };
    Safety::new(Some(true), needs_network, risk_note)
}

#[cfg(test)]
mod tests {
    use super::*;
    use topos_types::KNOWN_ACTION_CODES;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn every_known_code_gets_a_deliberate_classification() {
        // The rules table is total over the advertised vocabulary: every KNOWN code answers at
        // least one safety field (RETRY deliberately leaves `mutates` unknown but pins the
        // network fact). An agent therefore never sees a fully-blank known action.
        for code in KNOWN_ACTION_CODES {
            let a = next_action(ActionCode::from(code.to_owned()), Vec::new());
            assert!(
                a.mutates.is_some() || a.needs_network.is_some(),
                "{code} has no classification"
            );
        }
    }

    #[test]
    fn an_unknown_code_stays_fully_unknown() {
        let a = next_action(
            ActionCode::from("FUTURE_FLOW".to_owned()),
            argv(&["topos", "future"]),
        );
        assert!(a.mutates.is_none() && a.needs_network.is_none() && a.risk_note.is_none());
    }

    #[test]
    fn reads_are_marked_non_mutating() {
        let list = next_action(
            ActionCode::DisambiguateName,
            argv(&["topos", "list", "--json"]),
        );
        assert_eq!(
            (list.mutates, list.needs_network),
            (Some(false), Some(false))
        );
        let inbox = next_action(
            ActionCode::from("REVIEW_INBOX".to_owned()),
            argv(&["topos", "review", "--json"]),
        );
        assert_eq!(
            (inbox.mutates, inbox.needs_network),
            (Some(false), Some(true))
        );
    }

    #[test]
    fn fetch_full_diff_network_follows_the_ref_shape() {
        // A bare draft↔current diff is local; a <ref> endpoint fetches plane bytes.
        let local = next_action(
            ActionCode::FetchFullDiff,
            argv(&["topos", "diff", "deploy", "--max-bytes", "0", "--json"]),
        );
        assert_eq!(local.needs_network, Some(false));
        let plane = next_action(
            ActionCode::FetchFullDiff,
            argv(&[
                "topos",
                "diff",
                "deploy",
                "current..abc123",
                "--max-bytes",
                "0",
                "--json",
            ]),
        );
        assert_eq!(plane.needs_network, Some(true));
        assert_eq!(plane.mutates, Some(false));
    }

    #[test]
    fn next_page_knows_list_and_leaves_log_open() {
        let local = next_action(
            ActionCode::NextPage,
            argv(&["topos", "list", "--offset", "50", "--json"]),
        );
        assert_eq!(local.needs_network, Some(false));
        let remote = next_action(
            ActionCode::NextPage,
            argv(&["topos", "list", "--remote", "--offset", "50", "--json"]),
        );
        assert_eq!(remote.needs_network, Some(true));
        // `log`'s plane half depends on enrollment — the argv cannot answer, so it stays absent.
        let log = next_action(
            ActionCode::NextPage,
            argv(&["topos", "log", "deploy", "--offset", "20", "--json"]),
        );
        assert_eq!(log.needs_network, None);
        assert_eq!(log.mutates, Some(false));
    }

    #[test]
    fn resolve_diverged_draft_refines_by_the_resolution_shape() {
        // The `--onto-current` escape resolves a recorded conflict locally — your bytes win.
        let escape = next_action(
            ActionCode::ResolveDivergedDraft,
            argv(&["topos", "update", "deploy", "--onto-current", "--json"]),
        );
        assert_eq!(
            (escape.mutates, escape.needs_network),
            (Some(true), Some(false))
        );
        assert!(
            escape.risk_note.as_deref().unwrap_or("").contains("YOUR"),
            "{escape:?}"
        );
        // A bare `--reset` only DESCRIBES the loss-led discard (its describe carries the `--yes`).
        let reset = next_action(
            ActionCode::ResolveDivergedDraft,
            argv(&["topos", "update", "deploy", "--reset", "--json"]),
        );
        assert_eq!(
            (reset.mutates, reset.needs_network),
            (Some(false), Some(false))
        );
        // The plain targeted update keeps the three-way-merge story.
        let merge = next_action(
            ActionCode::ResolveDivergedDraft,
            argv(&["topos", "update", "deploy", "--json"]),
        );
        assert_eq!(
            (merge.mutates, merge.needs_network),
            (Some(true), Some(true))
        );
        assert!(
            merge
                .risk_note
                .as_deref()
                .unwrap_or("")
                .contains("three-way"),
            "{merge:?}"
        );
    }

    #[test]
    fn apply_described_refines_by_verb_inside_the_one_module() {
        // A publish apply is team-visible + networked.
        let publish = next_action(
            ActionCode::from("APPLY_DESCRIBED".to_owned()),
            argv(&["topos", "publish", "deploy", "--yes"]),
        );
        assert_eq!(
            (publish.mutates, publish.needs_network),
            (Some(true), Some(true))
        );
        assert!(publish.risk_note.as_deref().unwrap_or("").contains("team"));
        // A reset apply is a local discard — no network, a loss caution.
        let reset = next_action(
            ActionCode::from("APPLY_DESCRIBED".to_owned()),
            argv(&["topos", "update", "deploy", "--reset", "--yes"]),
        );
        assert_eq!(
            (reset.mutates, reset.needs_network),
            (Some(true), Some(false))
        );
        assert!(reset.risk_note.as_deref().unwrap_or("").contains("discard"));
        // A remove apply cannot know its network story from the argv — honest absence.
        let remove = next_action(
            ActionCode::from("APPLY_DESCRIBED".to_owned()),
            argv(&["topos", "remove", "deploy", "--yes"]),
        );
        assert_eq!(remove.needs_network, None);
        assert_eq!(remove.mutates, Some(true));
    }
}
