//! The client's one typed error family. The bin maps each variant to a stable wire `code` + a
//! [`TerminalOutcome`]; raw `gix`/io strings stay internal and never reach a user surface.

use topos_gitstore::{GitstoreError, VerifyError};
use topos_types::{Generation, TerminalOutcome};

use topos_core::digest::RejectReason;

/// A local-core failure. `#[non_exhaustive]` so new verbs can add variants without breaking matches.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum ClientError {
    /// A filesystem operation failed, wrapped with call-site context only (the OS `ErrorKind` was not
    /// retained, so it classifies retryable). Where the `io::Error` is in hand, prefer the `?`-ridden
    /// [`ClientError::IoKind`] path so permanence can be told apart.
    #[error("filesystem error: {0}")]
    Io(String),
    /// A filesystem operation failed, carrying the OS [`std::io::ErrorKind`] — so `outcome()` can tell a
    /// permanent local failure (permission denied / read-only filesystem / disk full) from a transient
    /// one instead of inviting the agent to retry-loop forever. Every `std::io::Error` that rides `?`
    /// lands here via `From`; the wire code stays `IO_ERROR` (the kind refines only the outcome).
    #[error("filesystem error: {context}")]
    IoKind {
        kind: std::io::ErrorKind,
        context: String,
    },
    /// A malformed command-line argument (a bad hash shape, a malformed `<skill>@<hash>` target, an
    /// invalid flag combination). The message is usage guidance this code wrote — it describes the
    /// expected shape (or echoes the user's own argv token, never wire/document bytes) and is shown
    /// VERBATIM on both surfaces: a usage error is the one family where redaction would hide exactly
    /// what the user needs. Sidecar-document parse failures stay [`ClientError::Corrupt`].
    #[error("{0}")]
    InvalidArgument(String),
    /// A write-side git store failure.
    #[error("git store error: {0}")]
    Gitstore(#[from] GitstoreError),
    /// A read-side integrity failure (verify-on-read).
    #[error("integrity error: {0}")]
    Verify(#[from] VerifyError),
    /// A persisted document carries an unknown/newer `schema_version` — fail closed; the doc is **never**
    /// handed to serde and **never** deleted (an upgrade is required, not a corruption).
    #[error(
        "document schema_version {found} is newer than this build supports (max {max}); upgrade topos"
    )]
    UnknownSchemaVersion { found: u32, max: u32 },
    /// A persisted document carries a `schema_version` below the supported floor.
    #[error("document schema_version {found} is no longer supported")]
    UnsupportedLegacy { found: u32 },
    /// A persisted document could not be parsed or is internally inconsistent (genuine corruption — not a
    /// mere version mismatch). Recovery reports it; it never fabricates the missing state.
    #[error("corrupt sidecar state: {0}")]
    Corrupt(String),
    /// A PLANE RESPONSE failed client-side validation (a wire-boundary id/shape check) — the remote
    /// counterpart of [`ClientError::Corrupt`]. Same `CORRUPT_STATE` wire code (the vocabulary stays
    /// closed), but the safe surface says the plane's response failed validation instead of falsely
    /// blaming a local sidecar document. Persisted-doc failures (a `follows.json` load) stay `Corrupt`.
    #[error("plane response failed validation: {0}")]
    WireInvalid(String),
    /// The scan of a real skill dir hit a filesystem-level reject (symlink / device / non-regular file /
    /// non-UTF-8 name) or a kernel path reject (absolute / `..` / NUL / collision).
    #[error("skill directory rejected: {0}")]
    Scan(String),
    /// The bundle has no files (after excluding `.git/` + `.DS_Store`) — not a skill.
    #[error("the skill directory has no files to adopt")]
    EmptyBundle,
    /// The source path overlaps `~/.topos/` (equal / ancestor / descendant) — refused so uninstall can
    /// never delete user bytes and the footprint oracle never collapses.
    #[error("the source path overlaps the topos home directory")]
    SourceOverlap,
    /// A skill with this id already exists on disk — `add` fails closed rather than overwrite/merge.
    #[error("a skill with this id already exists")]
    SkillExists,
    /// The directory is already tracked in place (same canonical path) — re-adopting would mint a second
    /// record for one mutable dir, so `add` refuses and points at the existing skill (edits already
    /// surface as a draft via `diff`).
    #[error("this directory is already tracked as skill '{skill_id}'")]
    AlreadyTracked { skill_id: String },
    /// A name resolved to more than one tracked skill; the caller must disambiguate by id.
    #[error("the name '{name}' is ambiguous across {count} tracked skills")]
    AmbiguousName { name: String, count: usize },
    /// No tracked skill matches the given name.
    #[error("no tracked skill named '{name}'")]
    NoSuchSkill { name: String },
    /// The placement cannot be materialized safely (a non-directory sits where a skill dir belongs, a
    /// symlink cannot be resolved to a directory, or the filesystem supports no safe swap) — refused
    /// rather than risk clobbering or a torn write.
    #[error("the skill placement cannot be materialized safely: {reason}")]
    PlacementUnsupported { reason: String },
    /// The plane could not be read for an explicitly-targeted skill (unreachable, not served, or a
    /// malformed response). A bare currency sweep isolates such failures per skill instead of erroring.
    #[error("plane read failed: {0}")]
    Plane(String),
    /// A go-back (`pull <skill>@<hash>`) named a version this client cannot anchor — it is absent from
    /// the local history, so its generation is unknown and it cannot be installed without a fabricated
    /// floor. Refused.
    #[error("cannot go back to version '{version}': not in this skill's local history")]
    UnknownGoBackVersion { version: String },
    /// An enrollment step could not complete (a missing/expired session, a denied verification, a
    /// malformed link). The message is fixed text or a user-supplied token-free description.
    #[error("enrollment failed: {0}")]
    Enrollment(String),
    /// The plane at the already-pinned base URL presented a DIFFERENT signing key than the one this client
    /// TOFU-pinned. A continuity-signed rotation is not yet supported, so the follow is refused rather than
    /// silently trusting a new key — the human must re-pin out of band.
    #[error("the plane's signing key differs from the pinned key; re-pin required")]
    KeyRepinRequired,
    /// The optional `@<digest>` consent pin did not match the digest recomputed over the bytes being
    /// shipped — refused BEFORE signing or sending (the disclosure/integrity gate; never a silent
    /// mode-flip). The agent re-discloses (via `diff`) and re-pins the exact digest.
    #[error(
        "the pinned @<digest> does not match the bytes: pinned {got}, bytes hash to {expected}"
    )]
    ApprovalMismatch {
        skill: String,
        expected: String,
        got: String,
    },
    /// A direct `publish` under `review-required`: the plane refused it closed (`APPROVAL_REQUIRED`),
    /// ingesting nothing. The agent re-runs it as `publish --propose` (NEVER an auto-flip).
    #[error("this workspace requires review; re-run as `publish --propose`")]
    ApprovalRequired { skill: String, digest: String },
    /// The compare-and-set saw a base the team has moved past (`CONFLICT`) — the local view is stale. The
    /// agent pulls (rebases) and re-shows the diff before retrying; never a silent retry.
    #[error("the team moved past your base; pull to rebase, then retry")]
    Conflict {
        skill: String,
        current: Option<Generation>,
    },
    /// The plane denied the op (`DENIED`) — not rostered, four-eyes self-approve, or an already-resolved
    /// proposal. Carries the wire code for the agent to branch on; never a secret.
    #[error("the plane denied this operation ({0})")]
    Denied(String),
    /// An enrollment REDEEM came back DENIED — on a hosted plane this is the authenticated-but-uninvited
    /// case (a confirmed identity that is not on the workspace roster), so the guidance is ask-an-owner:
    /// the message tells the human exactly what to request, and the envelope carries `REQUEST_ACCESS`.
    /// Carries the wire code (never a secret; the plane's denial is deliberately uniform).
    #[error(
        "the workspace did not admit this enrollment ({code}) — ask a workspace owner to run \
         `topos invite <your-email>`, then re-run `topos follow`"
    )]
    RedeemDenied { code: String },
    /// A `publish` is blocked because an unresolved author-merge conflict (`conflict.json`) is present —
    /// the draft must be resolved first. Refused before any build / WAL / send (the publish guard).
    #[error("publish is blocked: resolve the merge conflict in this skill first")]
    PublishBlocked { skill: String },
    /// A `revert` needs `--confirm` to proceed (a degenerate/no-op revert — e.g. `--to` names the version
    /// that is ALREADY `current`). Re-run with `--confirm`, or pick a different good version.
    #[error("revert needs --confirm: {reason}")]
    ConfirmRequired { reason: String },
    /// A crashed prior write for this skill is still in-flight and DIFFERS from the command just issued
    /// (a different digest / mode / target). Settle it first (re-run the original command, which replays
    /// its `op_id`), then re-issue this change — never silently replay a different intent.
    #[error("an in-flight write for '{skill}' must settle first: {detail}")]
    PendingOp { skill: String, detail: String },
    /// A verb that must act in ONE workspace could not choose one: this install has joined multiple
    /// workspaces and none was named (pass `--workspace <id>`), or a named `--workspace` id is not one
    /// this install has joined. The message is usage guidance shown VERBATIM — it names the joined
    /// workspaces (or the missing id); a workspace id is a path-safe identifier, never a secret.
    #[error("{0}")]
    WorkspaceSelection(String),
    /// A definitive, NON-retryable rejection from the plane on a non-2xx status (a 4xx other than 429 — the
    /// op provably did NOT land), so its op-WAL is dropped rather than replayed forever.
    #[error("the plane rejected the request (HTTP {0})")]
    PlaneRejected(u16),
    /// A terminal protocol outcome the verb does not special-case (e.g. the plane's `RetryableFailure` /
    /// `Unavailable` / `PermanentFailure`), carried verbatim so the agent branches on the TRUE outcome
    /// (not a generic transport error). `retryable` selects a Retry next-action + the outcome class.
    #[error("the plane returned {code} ({outcome:?})")]
    PlaneTerminal {
        outcome: TerminalOutcome,
        code: String,
        retryable: bool,
    },
}

impl ClientError {
    /// The stable, machine-branchable wire code (an open vocabulary).
    pub(crate) fn code(&self) -> &'static str {
        match self {
            // One wire code for both filesystem shapes — the kind refines `outcome()`, never the code
            // set (which is closed on the client side; agents branch on `outcome`/`retryable`).
            ClientError::Io(_) | ClientError::IoKind { .. } => "IO_ERROR",
            ClientError::InvalidArgument(_) => "INVALID_ARGUMENT",
            ClientError::Gitstore(_) => "GIT_STORE_ERROR",
            ClientError::Verify(_) => "INTEGRITY_ERROR",
            ClientError::UnknownSchemaVersion { .. } => "UPGRADE_REQUIRED",
            ClientError::UnsupportedLegacy { .. } => "UNSUPPORTED_SCHEMA",
            ClientError::Corrupt(_) => "CORRUPT_STATE",
            // Same closed-vocabulary code; only the safe MESSAGE differs (wire, not sidecar).
            ClientError::WireInvalid(_) => "CORRUPT_STATE",
            ClientError::Scan(_) => "SCAN_REJECTED",
            ClientError::EmptyBundle => "EMPTY_BUNDLE",
            ClientError::SourceOverlap => "SOURCE_OVERLAP",
            ClientError::SkillExists => "SKILL_EXISTS",
            ClientError::AlreadyTracked { .. } => "ALREADY_TRACKED",
            ClientError::AmbiguousName { .. } => "AMBIGUOUS_NAME",
            ClientError::NoSuchSkill { .. } => "NO_SUCH_SKILL",
            ClientError::PlacementUnsupported { .. } => "PLACEMENT_UNSUPPORTED",
            ClientError::UnknownGoBackVersion { .. } => "UNKNOWN_GOBACK_VERSION",
            ClientError::Plane(_) => "PLANE_ERROR",
            ClientError::Enrollment(_) => "ENROLLMENT_FAILED",
            ClientError::KeyRepinRequired => "KEY_REPIN_REQUIRED",
            ClientError::ApprovalMismatch { .. } => "CONSENT_MISMATCH",
            ClientError::ApprovalRequired { .. } => "APPROVAL_REQUIRED",
            ClientError::Conflict { .. } => "CONFLICT",
            ClientError::Denied(_) => "DENIED",
            // The same closed DENIED code — only the guidance message differs (enrollment ask-an-owner).
            ClientError::RedeemDenied { .. } => "DENIED",
            ClientError::PublishBlocked { .. } => "PUBLISH_BLOCKED",
            ClientError::ConfirmRequired { .. } => "CONFIRM_REQUIRED",
            ClientError::PendingOp { .. } => "PENDING_OP",
            ClientError::WorkspaceSelection(_) => "WORKSPACE_SELECTION",
            ClientError::PlaneRejected(_) => "PLANE_REJECTED",
            // The plane's fine code rides the Display message + context; the agent branches on `outcome`.
            ClientError::PlaneTerminal { .. } => "PLANE_TERMINAL",
        }
    }

    /// The terminal outcome the agent branches on.
    pub(crate) fn outcome(&self) -> TerminalOutcome {
        match self {
            ClientError::AmbiguousName { .. } => TerminalOutcome::AmbiguousName,
            // A transient filesystem or plane-read failure is retryable — whether it surfaced
            // client-side, in the store, or reading the plane.
            ClientError::Io(_)
            | ClientError::Gitstore(GitstoreError::Io(_))
            | ClientError::Plane(_) => TerminalOutcome::RetryableFailure,
            // With the OS kind in hand, permanence is decidable: permission-denied, a read-only
            // filesystem, and disk-full will NOT heal on a retry — the retryable bit is the load-bearing
            // part of the machine contract, so it must not steer the agent into a loop. Everything else
            // keeps the transient-until-proven-otherwise default above.
            ClientError::IoKind { kind, .. } => match kind {
                std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::ReadOnlyFilesystem
                | std::io::ErrorKind::StorageFull => TerminalOutcome::PermanentFailure,
                _ => TerminalOutcome::RetryableFailure,
            },
            // The contribute typed outcomes carry their own terminal classification (the plane's verdict,
            // surfaced 1:1 so the agent branches on the same outcome it would on the wire).
            ClientError::ApprovalRequired { .. } => TerminalOutcome::ApprovalRequired,
            ClientError::Conflict { .. } => TerminalOutcome::Conflict,
            ClientError::Denied(_) | ClientError::RedeemDenied { .. } => TerminalOutcome::Denied,
            ClientError::PublishBlocked { .. } => TerminalOutcome::Diverged,
            // An in-flight op must be settled, then the command retried.
            ClientError::PendingOp { .. } => TerminalOutcome::RetryableFailure,
            // A definitive 4xx rejection — the op cannot succeed as-is.
            ClientError::PlaneRejected(_) => TerminalOutcome::PermanentFailure,
            // The plane's terminal outcome, surfaced verbatim (not flattened to a transport error).
            ClientError::PlaneTerminal { outcome, .. } => *outcome,
            _ => TerminalOutcome::PermanentFailure,
        }
    }

    /// The live `(epoch, seq)` to carry on a `CONFLICT` envelope (the rebase target the agent pulls to) —
    /// `None` for every other error.
    pub(crate) fn current_generation(&self) -> Option<Generation> {
        match self {
            ClientError::Conflict { current, .. } => *current,
            _ => None,
        }
    }

    /// The FULL diagnostic detail: the `Display` chain — this error, then each `source()` link that adds
    /// text (a `#[from]` source the top `Display` already embeds is not repeated). This is what the
    /// append-only diagnostics log (and stderr under `TOPOS_DEBUG=1`) receives; user surfaces show the
    /// redacted [`crate::render::safe_message`] instead. Error `Display`s are secret-free by
    /// construction (tokens/keys are redacted at their type), so the chain is safe to persist.
    pub(crate) fn detail(&self) -> String {
        let mut out = self.to_string();
        let mut source = std::error::Error::source(self);
        while let Some(e) = source {
            let text = e.to_string();
            if !out.contains(&text) {
                out.push_str(": ");
                out.push_str(&text);
            }
            source = e.source();
        }
        out
    }
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        // Keep the kind — it is the only signal that lets `outcome()` refuse to call EACCES/ENOSPC
        // retryable. (The Display carries the OS message; call sites that want a path add context via
        // `map_err` before the conversion.)
        ClientError::IoKind {
            kind: e.kind(),
            context: e.to_string(),
        }
    }
}

impl From<RejectReason> for ClientError {
    fn from(r: RejectReason) -> Self {
        ClientError::Scan(format!("{r:?}"))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Error as IoError, ErrorKind};

    use topos_types::TerminalOutcome;

    use super::ClientError;

    #[test]
    fn io_kind_classifies_permanent_local_failures() {
        // The three kinds that will not heal on a retry are PERMANENT — the agent must not loop.
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::ReadOnlyFilesystem,
            ErrorKind::StorageFull,
        ] {
            let err = ClientError::from(IoError::new(kind, "refused"));
            assert_eq!(err.outcome(), TerminalOutcome::PermanentFailure, "{kind:?}");
            assert_eq!(err.code(), "IO_ERROR");
        }
        // Everything else keeps the transient-until-proven-otherwise default.
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::Interrupted,
            ErrorKind::TimedOut,
            ErrorKind::Other,
        ] {
            let err = ClientError::from(IoError::new(kind, "flaky"));
            assert_eq!(err.outcome(), TerminalOutcome::RetryableFailure, "{kind:?}");
        }
        // The kindless context wrapper stays retryable (the kind is unknown by construction).
        assert_eq!(
            ClientError::Io("read /x: gone".into()).outcome(),
            TerminalOutcome::RetryableFailure
        );
    }

    #[test]
    fn invalid_argument_is_a_permanent_usage_error_shown_verbatim() {
        let err = ClientError::InvalidArgument(
            "`--to` must be a 64-char lowercase hex version id".into(),
        );
        assert_eq!(err.code(), "INVALID_ARGUMENT");
        assert_eq!(err.outcome(), TerminalOutcome::PermanentFailure);
        // Usage guidance is the one family never redacted — the surfaces show it verbatim.
        assert_eq!(
            crate::render::safe_message(&err),
            "`--to` must be a 64-char lowercase hex version id"
        );
    }

    #[test]
    fn detail_carries_the_context_the_surfaces_redact() {
        let err = ClientError::from(IoError::new(
            ErrorKind::PermissionDenied,
            "open /home/x/.topos/skills: denied",
        ));
        assert!(err.detail().contains("/home/x/.topos/skills"));
        assert_eq!(
            crate::render::safe_message(&err),
            "a filesystem operation failed"
        );
    }
}
