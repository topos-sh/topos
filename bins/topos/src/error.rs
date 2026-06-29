//! The client's one typed error family. The bin maps each variant to a stable wire `code` + a
//! [`TerminalOutcome`]; raw `gix`/io strings stay internal and never reach a user surface.

use topos_gitstore::{GitstoreError, VerifyError};
use topos_types::TerminalOutcome;

use topos_core::digest::RejectReason;

/// A local-core failure. `#[non_exhaustive]` so new verbs can add variants without breaking matches.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum ClientError {
    /// A filesystem operation failed.
    #[error("filesystem error: {0}")]
    Io(String),
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
}

impl ClientError {
    /// The stable, machine-branchable wire code (an open vocabulary).
    pub(crate) fn code(&self) -> &'static str {
        match self {
            ClientError::Io(_) => "IO_ERROR",
            ClientError::Gitstore(_) => "GIT_STORE_ERROR",
            ClientError::Verify(_) => "INTEGRITY_ERROR",
            ClientError::UnknownSchemaVersion { .. } => "UPGRADE_REQUIRED",
            ClientError::UnsupportedLegacy { .. } => "UNSUPPORTED_SCHEMA",
            ClientError::Corrupt(_) => "CORRUPT_STATE",
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
            _ => TerminalOutcome::PermanentFailure,
        }
    }
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Io(e.to_string())
    }
}

impl From<RejectReason> for ClientError {
    fn from(r: RejectReason) -> Self {
        ClientError::Scan(format!("{r:?}"))
    }
}
