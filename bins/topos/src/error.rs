//! The client's one typed error family. The bin maps each variant to a stable wire `code` + a
//! [`TerminalOutcome`]; raw `gix`/io strings stay internal and never reach a user surface.

use topos_gitstore::{GitstoreError, VerifyError};
use topos_types::TerminalOutcome;

use topos_core::digest::RejectReason;

/// The same-name disclosure a local-ambiguity refusal carries when the name the user typed is ALSO
/// published in a connected workspace: the canonical references to subscribe to, and whether the
/// local copies are provably the same bytes as the one version those references serve.
///
/// `identical` is a claim, so it is made only on proof — exactly one reference, its live catalog
/// digest in hand, and every local candidate scanning to it. A cache-only match, several
/// workspaces, or an unreadable directory all leave it `false`: the disclosure still names the
/// team-managed spelling, it just does not say the bytes agree.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceHint {
    /// The canonical host-qualified references (`<host>/<workspace>/<name>`), sorted.
    pub references: Vec<String>,
    /// Every local candidate is byte-identical to the ONE reference's current version.
    pub identical: bool,
    /// The refused invocation carried `-g` — every subscribe this hint spells must carry it too,
    /// or following the guidance would silently move the row to the other manifest scope.
    pub global: bool,
}

/// The `topos add` spelling that preserves the refused invocation's scope.
fn add_spelling(global: bool) -> &'static str {
    if global { "topos add -g" } else { "topos add" }
}

/// ONE thing an ambiguous target could have meant, kept as STRUCTURE rather than as a sentence:
/// the reference itself, plus — for a member a set line delivers — the set that member removal
/// must name to select it.
///
/// The structure is the point. A reference is not always a tidy token: a local folder reference is
/// a PATH, and a path may contain whitespace. Rebuilding a command by splitting a rendered
/// spelling would let the bytes of a directory name become argv tokens of their own — a folder
/// called `my skill --yes` would smuggle a consent flag into an offered command. So the two
/// renderings are derived here, from the parts, and never re-parsed out of each other:
/// [`spelling`](Self::spelling) is the human/wire text, [`argv_tokens`](Self::argv_tokens) the
/// executable form in which the reference is ALWAYS exactly one token.
///
/// Ordering is derived over `(reference, via)` — which is the spelling order too, since a
/// reference holds no byte below `-`/space that could reorder the pair — so a sorted candidate
/// list reads the same on both surfaces.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TargetCandidate {
    /// The candidate's own reference — one argv token, whatever it contains.
    pub reference: String,
    /// The SET line whose member rewrite this candidate selects (`--via <set-reference>`), for a
    /// bundle no row of its own spells. `None` for a candidate that is a row (or a resource).
    pub via: Option<String>,
}

impl TargetCandidate {
    /// A candidate that stands on its own reference — no set line to name.
    pub(crate) fn plain(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            via: None,
        }
    }

    /// A candidate reached only THROUGH a set line: removing it rewrites that one line.
    pub(crate) fn via(reference: impl Into<String>, set: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            via: Some(set.into()),
        }
    }

    /// The rendered spelling — what the envelope's `data.candidates` carries. Exactly the text
    /// the two parts read as on a command line, so a consumer that has always read these strings
    /// sees no change.
    pub(crate) fn spelling(&self) -> String {
        match &self.via {
            Some(set) => format!("{} --via {set}", self.reference),
            None => self.reference.clone(),
        }
    }

    /// The EXECUTABLE form: the reference as ONE token, then the selector as its own two. This is
    /// what a rebuilt next action extends its argv with — never a split of the spelling.
    pub(crate) fn argv_tokens(&self) -> Vec<String> {
        match &self.via {
            Some(set) => vec![self.reference.clone(), "--via".to_owned(), set.clone()],
            None => vec![self.reference.clone()],
        }
    }
}

/// The clause an ambiguity refusal appends when a connected workspace publishes the same name —
/// empty when none does, so the two messages stay byte-identical to their pre-disclosure form.
/// Proven-identical bytes get the stronger reading (there is nothing to keep locally); otherwise
/// the clause states the choice without judging it.
fn workspace_sentence(name: &str, hint: Option<&WorkspaceHint>) -> String {
    let Some(hint) = hint else {
        return String::new();
    };
    let Some(first) = hint.references.first() else {
        return String::new();
    };
    let add = add_spelling(hint.global);
    if hint.identical {
        return format!(
            "; every local copy above is byte-identical to what `{first}` serves today — \
             `{add} {first}` subscribes to the team's copy, while adopting a path forks it \
             into a new, unmanaged skill"
        );
    }
    format!(
        "; '{name}' is also published in {} — `{add} {first}` subscribes to the team's copy \
         (delivered and kept current by topos), while the paths above adopt a local copy as a \
         new, unmanaged skill",
        hint.references.join(", ")
    )
}

/// A local-core failure. `#[non_exhaustive]` so new verbs can add variants without breaking matches.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum ClientError {
    /// A filesystem operation failed, wrapped with call-site context only (the OS `ErrorKind` was not
    /// retained, so it classifies retryable). Where the `io::Error` is in hand, prefer the `?`-ridden
    /// [`ClientError::IoKind`] path so permanence can be told apart.
    #[error("a filesystem operation failed — {0}")]
    Io(String),
    /// A filesystem operation failed, carrying the OS [`std::io::ErrorKind`] — so `outcome()` can tell a
    /// permanent local failure (permission denied / read-only filesystem / disk full) from a transient
    /// one instead of inviting the agent to retry-loop forever. Every `std::io::Error` that rides `?`
    /// lands here via `From`; the wire code stays `IO_ERROR` (the kind refines only the outcome).
    #[error("a filesystem operation failed — {context}")]
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
    /// A `publish` whose draft is byte-identical to `current` — there is nothing to ship. Distinct from a
    /// usage error so an agent can branch on "already published" without treating it as a mistake.
    #[error("'{skill}' has no changes to publish — the draft matches current")]
    NoChanges { skill: String },
    /// A write-side git store failure.
    #[error("the local skill store reported an error — {0}")]
    Gitstore(#[from] GitstoreError),
    /// A read-side integrity failure (verify-on-read).
    #[error("an integrity check failed — {0}")]
    Verify(#[from] VerifyError),
    /// A persisted document carries an unknown/newer `schema_version` — fail closed; the doc is **never**
    /// handed to serde and **never** deleted (an upgrade is required, not a corruption).
    #[error(
        "this machine's topos state was written by a newer topos (format {found}; this build \
         reads up to {max}) — update this install with `topos self-update`"
    )]
    UnknownSchemaVersion { found: u32, max: u32 },
    /// A persisted document carries a `schema_version` below the supported floor.
    #[error(
        "this machine's topos state is in a format this topos no longer reads (format {found})"
    )]
    UnsupportedLegacy { found: u32 },
    /// A persisted document could not be parsed or is internally inconsistent (genuine corruption — not a
    /// mere version mismatch). Recovery reports it; it never fabricates the missing state.
    #[error("topos's own state on this machine is unreadable — {0}")]
    Corrupt(String),
    /// A PLANE RESPONSE failed client-side validation (a wire-boundary id/shape check) — the remote
    /// counterpart of [`ClientError::Corrupt`]. Same `CORRUPT_STATE` wire code (the vocabulary stays
    /// closed), but the safe surface blames the server's answer instead of falsely blaming this
    /// machine's own state. Persisted-doc failures (a `follows.json` load) stay `Corrupt`.
    #[error("the server sent a response topos could not read — {0}")]
    WireInvalid(String),
    /// The scan of a real skill dir hit a filesystem-level reject (symlink / device / non-regular file /
    /// non-UTF-8 name) or a kernel path reject (absolute / `..` / NUL / collision).
    #[error("the skill directory was rejected — {0}")]
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
    /// `add <skill>` resolved a name against discovery and found nothing adoptable — no untracked skill of
    /// that name sits in any known harness dir. Usage guidance shown VERBATIM (the name is the user's own
    /// argv token). Distinct from [`ClientError::NoSuchSkill`] (which is about *tracked* skills).
    #[error(
        "no untracked skill named '{name}' — run `topos list` to see what's adoptable, or adopt a \
         directory by path (`topos add ./<dir>`)"
    )]
    NoUntrackedSkill { name: String },
    /// `add <skill>` named a skill that is already tracked (so discovery excludes it) — the right move is
    /// to edit it in place, not re-adopt. The name-oriented twin of [`ClientError::AlreadyTracked`]; both
    /// carry the `ALREADY_TRACKED` code so an agent branches the same on either.
    #[error(
        "a skill named '{name}' is already tracked — edit it in place (`topos diff {name}`), or adopt a \
         different directory by path (`topos add ./<dir>`)"
    )]
    AlreadyTrackedName { name: String },
    /// `publish <skill>@<harness>` named a harness that differs from the one the ALREADY-TRACKED skill of
    /// that name was adopted from. The auto-add convenience cannot re-adopt a tracked name, and a mismatched
    /// `@<harness>` suffix likely means a DIFFERENT copy was intended — refused so a stray suffix never
    /// silently publishes the tracked bytes. Slugs shown VERBATIM (the tracked one, and the user's token).
    #[error(
        "skill '{name}' is already tracked from harness '{tracked}', not '{requested}' — publish it as \
         `topos publish {name}`, or adopt the '{requested}' copy under a different name first"
    )]
    HarnessMismatch {
        name: String,
        requested: String,
        tracked: String,
    },
    /// `add <skill>` resolved a name that sits under more than one harness's skill dir. The caller must
    /// disambiguate with `<skill>@<harness>`. The `harnesses` are the registry slugs, shown VERBATIM.
    /// `workspace` carries the same-name disclosure when a connected workspace publishes the name too.
    #[error(
        "the skill name '{name}' is found in {} harnesses ({}) — disambiguate with `topos add {name}@<harness>`{}",
        harnesses.len(), harnesses.join(", "), workspace_sentence(name, workspace.as_ref())
    )]
    AmbiguousHarness {
        name: String,
        harnesses: Vec<String>,
        workspace: Option<WorkspaceHint>,
    },
    /// `add <skill>[@<harness>]` resolved to more than one directory within a SINGLE harness (a name in
    /// both the user- and project-scope dir, say). `@<harness>` cannot split them, so the caller adopts one
    /// explicitly by path. The `paths` are the user's own directories, shown VERBATIM. `workspace` carries
    /// the same-name disclosure when a connected workspace publishes the name too.
    #[error(
        "the skill '{name}' in harness '{harness}' matches {} directories ({}) — adopt one by path (`topos add <dir>`){}",
        paths.len(), paths.join(", "), workspace_sentence(name, workspace.as_ref())
    )]
    AmbiguousScope {
        name: String,
        harness: String,
        paths: Vec<String>,
        workspace: Option<WorkspaceHint>,
    },
    /// `add <name>` found no untracked local skill of that name, and MORE than one connected workspace
    /// publishes it — the machine cannot pick between two teams' copies, so it names both spellings and
    /// refuses. The `references` are the canonical host-qualified forms, shown VERBATIM (a workspace
    /// address is a path-safe identifier, never a secret); they also ride the envelope's
    /// `data.references` and one `next_actions` entry each. `global` preserves the refused
    /// invocation's `-g` in every spelled follow-up, so the row lands in the scope that was asked.
    #[error(
        "'{name}' is published in {} of the workspaces this machine is connected to ({}) — name the one \
         you mean (`{} <reference>`)",
        references.len(), references.join(", "), add_spelling(*global)
    )]
    AmbiguousWorkspace {
        name: String,
        references: Vec<String>,
        global: bool,
    },
    /// `add <skill>@<harness>` named a harness that holds no untracked skill of that name. The message
    /// names where the skill IS found (if anywhere), all shown VERBATIM (slugs + the user's own tokens).
    #[error("{0}")]
    HarnessNotFound(String),
    /// `add <arg>` was given a bare token that turned out to name an existing path on disk (a `~`-prefixed
    /// token, or a cwd entry) rather than a discovered skill name — the user likely meant a path adopt.
    /// Usage guidance shown VERBATIM (the arg is the user's own token).
    #[error("'{arg}' looks like a path — to adopt a directory, prefix it (`topos add ./{arg}`)")]
    PathNotName { arg: String },
    /// The placement cannot be materialized safely (a non-directory sits where a skill dir belongs, a
    /// symlink cannot be resolved to a directory, or the filesystem supports no safe swap) — refused
    /// rather than risk clobbering or a torn write.
    #[error("the skill's files cannot be written safely — {reason}")]
    PlacementUnsupported { reason: String },
    /// MORE than one of a skill's placements holds a DIFFERENT local edit — there is no single draft
    /// to sync, diff, or publish, and nothing is overwritten (the typed freeze). The paths name every
    /// edited copy; the way out is reconciling them by hand into one, or discarding the edits with
    /// `update --reset` (every edited copy is snapshotted into the sidecar store first).
    #[error(
        "'{skill}' has divergent local edits in more than one placement ({}) — reconcile the copies \
         by hand, or discard the edits with `topos update {skill} --reset` (each copy is snapshotted \
         first)",
        paths.join(", ")
    )]
    PlacementsDiverged { skill: String, paths: Vec<String> },
    /// The server could not be read for an explicitly-targeted skill (unreachable, not served, or a
    /// malformed response). A bare update sweep isolates such failures per skill instead of erroring.
    ///
    /// The message is SELF-AUTHORED and shown verbatim: every construction site writes one complete
    /// lowercase phrase naming the server (or the thing that could not be read) — a transport fault
    /// goes through [`crate::plane_http::transport_reason`] first, so no `ureq` tail ever lands here.
    #[error("{0}")]
    Plane(String),
    /// A go-back (`pull <skill>@<hash>`) named a version this client cannot anchor — it is absent from
    /// the local store, so its bytes are unavailable and it cannot be installed. Refused.
    #[error("cannot go back to version '{version}' — it is not in this skill's local history")]
    UnknownGoBackVersion { version: String },
    /// A LOGIN-FLOW step could not complete (an expired/denied browser approval, a malformed
    /// invitation link, a conflicting pending flow). The message is self-contained guidance —
    /// fixed text or a user-supplied token-free description.
    #[error("{0}")]
    Enrollment(String),
    /// A verb needs a live SESSION this installation does not hold (not logged in at all, an
    /// ended session, a workspace this install never connected to). The message is self-authored
    /// guidance naming the concrete fix; `address` ALSO rides structurally — the envelope's
    /// `LOGIN_WORKSPACE` next action carries `topos login <address>` as an executable argv (the
    /// address as one token, straight from this field), never prose alone.
    #[error("{message}")]
    SessionRequired { address: String, message: String },
    /// The ONE not-enrolled refusal every credentialed verb shares: this install has no plane (or
    /// no membership) to act against. The fix is stated in prose AND mirrored structurally — the
    /// envelope carries a `LOGIN_WORKSPACE` next action whose argv template `needs` the
    /// workspace address. Same wire code as the enrollment family (agents branch the same).
    #[error(
        "not connected — run `topos login <workspace-address>` first (ask a teammate for your \
         workspace address, or create a workspace at https://topos.sh)"
    )]
    NotEnrolled,
    /// The optional `@<digest>` consent pin did not match the digest recomputed over the bytes being
    /// shipped — refused BEFORE signing or sending (the disclosure/integrity gate; never a silent
    /// mode-flip). The agent re-discloses (via `diff`) and re-pins the exact digest.
    #[error(
        "the pinned @<digest> does not match the bytes — you pinned {got}, these bytes hash to \
         {expected}"
    )]
    ApprovalMismatch {
        skill: String,
        expected: String,
        got: String,
    },
    /// The compare-and-set saw a base the team has moved past (`CONFLICT`) — the local view is stale. The
    /// agent pulls (rebases) and re-shows the diff before retrying; never a silent retry.
    #[error("the team moved past the version you started from — update to rebase, then retry")]
    Conflict { skill: String, current: Option<u64> },
    /// The plane denied the op (`DENIED`) — not rostered, four-eyes self-approve, or an already-resolved
    /// proposal. Carries the wire code for the agent to branch on; never a secret.
    #[error("the server refused this operation ({0})")]
    Denied(String),
    /// A `review` verdict (`--approve`/`--reject`/`--withdraw`) targeted a proposal that is no longer OPEN
    /// at the live `current`: an already-resolved proposal moved `current` past its base, so the
    /// fresh-current `expected` matches no open proposal and the plane answers a terminal
    /// `PERMANENT_FAILURE` (its only signal is a prose message; there is NO distinguishing wire code, and it
    /// does not name who resolved it). Rendered as an HONEST domain refusal shown VERBATIM — not the
    /// transport-fault-shaped "the plane returned PERMANENT_FAILURE". The message is self-authored guidance.
    #[error("{0}")]
    ReviewNotOpen(String),
    /// A `publish` is blocked because an unresolved author-merge conflict (`conflict.json`) is present —
    /// the draft must be resolved first. Refused before any build / WAL / send (the publish guard).
    #[error("publish is blocked — resolve the merge conflict in this skill first")]
    PublishBlocked { skill: String },
    /// A crashed prior write for this skill is still in-flight and DIFFERS from the command just issued
    /// (a different digest / mode / target). Settle it first (re-run the original command, which replays
    /// its `op_id`), then re-issue this change — never silently replay a different intent.
    #[error("an earlier write for '{skill}' is still in flight and must settle first — {detail}")]
    PendingOp { skill: String, detail: String },
    /// A verb that must act in ONE workspace could not choose one: this install has joined multiple
    /// workspaces and none was named (pass `--workspace <name>`), or a named `--workspace` (address
    /// name or opaque id) is not one this install has joined. The message is usage guidance shown
    /// VERBATIM — it names the joined workspace ADDRESSES; an address slug or workspace id is a
    /// path-safe identifier, never a secret.
    #[error("{0}")]
    WorkspaceSelection(String),
    /// A definitive, NON-retryable rejection from the plane on a non-2xx status (a 4xx other than 429 — the
    /// op provably did NOT land), so its op-WAL is dropped rather than replayed forever.
    #[error("the server rejected this request (HTTP {0})")]
    PlaneRejected(u16),
    /// A self-update download (`topos upgrade`) did not match the sha256 the release `SHA256SUMS` lists —
    /// refused BEFORE the binary is touched (the mandatory, never-skippable integrity gate). The message is
    /// all-public (the asset name + the two hashes), so it is safe to show verbatim.
    #[error(
        "{asset} does not match its published checksum (expected {expected}, got {actual}) — \
         refusing to install"
    )]
    ChecksumMismatch {
        asset: String,
        expected: String,
        actual: String,
    },
    /// `topos upgrade` could not replace the running binary because its install location is not
    /// writable — typically a package-managed or read-only install. The message is actionable guidance
    /// (the binary's path + how to reinstall); a path is not a secret, so it is shown VERBATIM.
    #[error("{0}")]
    UpgradeUnwritable(String),
    /// A self-update download failed minisign verification against this build's COMPILED-IN release
    /// public key — the `.minisig` was missing, unreadable, or did not verify over the downloaded
    /// bytes. With a key compiled in, signature verification is mandatory and fail-closed: refused
    /// BEFORE the checksum step and BEFORE the binary is touched. The message is all-public (the
    /// asset name + a non-secret reason), so it is safe to show verbatim.
    #[error("the release signature for {asset} {reason} — refusing to install")]
    SignatureInvalid { asset: String, reason: String },
    /// `add <owner/repo>` (a remote import) could not fetch the source. `permanent` separates the
    /// causes retrying can never fix (a not-found repo/ref, a malformed reference) from the
    /// transport faults a retry may clear (unreachable host, a cut-short download, a server
    /// error) — the outcome and the envelope's `retryable` both derive from it, so a 404 never
    /// invites a retry loop. The message is all-public (the source + a humanized reason), so it is
    /// shown VERBATIM.
    #[error("could not fetch {msg}")]
    RemoteFetch { msg: String, permanent: bool },
    /// A fetched remote source (a repo, or the `#<ref>`/`/tree/` subtree named) contained no `SKILL.md` —
    /// there is no skill to adopt. Usage guidance shown VERBATIM (the source is the user's own token).
    /// (`src`, not `source` — `thiserror` reserves a field named `source` for an error cause.)
    #[error(
        "no skill found in {src} — a skill is a directory with a SKILL.md (looked at the root and \
         under skills/)"
    )]
    NoSkillInSource { src: String },
    /// `add <owner/repo> --skill <name>` named a skill the fetched source does not contain. The message
    /// names the skills that ARE present (VERBATIM — repo skill names are public), so the agent re-picks.
    #[error(
        "no skill named '{skill}' in {src} — it has: {} (pick one with `--skill <name>`)",
        available.join(", ")
    )]
    SkillNotInRepo {
        skill: String,
        src: String,
        available: Vec<String>,
    },
    /// `add <owner/repo>` fetched a source holding MORE than one skill and none was named — the agent
    /// disambiguates with `--skill <name>`. The `skills` are the repo's skill names, shown VERBATIM.
    #[error(
        "{src} has {} skills ({}) — adopt one with `--skill <name>`",
        skills.len(), skills.join(", ")
    )]
    AmbiguousSkillInRepo { src: String, skills: Vec<String> },
    /// A repo holds MORE than one skill dir with the SAME basename (e.g. `skills/.curated/foo` and
    /// `skills/.experimental/foo`) — `--skill <name>` cannot pick between them, so the import refuses to
    /// guess. The `paths` are the colliding repo-relative dirs; the fix is a subdir-exact `/tree/<ref>/`
    /// URL. Shares the `AMBIGUOUS_SKILL` code (an agent branches the same).
    #[error(
        "the name '{name}' maps to {} directories in {src} ({}) — import one by its \
         https://github.com/…/tree/<ref>/<subdir> URL",
        paths.len(), paths.join(", ")
    )]
    DuplicateSkillName {
        src: String,
        name: String,
        paths: Vec<String>,
    },
    /// The destination skill directory a remote import would land in already exists and is NOT tracked by
    /// topos (a foreign, non-empty dir) — refused rather than clobber it. The agent picks another
    /// `--harness`/scope, or clears the dir. The `path` is the user's own directory, shown VERBATIM.
    #[error(
        "'{path}' already exists and is not tracked by topos — refusing to overwrite it; adopt it in \
         place (`topos add {path}`), or land elsewhere with `--harness <slug>`/`--global`"
    )]
    PlacementOccupied { path: String },
    /// The target manifest CHANGED between the read that decided an edit and the write that would
    /// apply it (a person's editor, another tool — topos's own writers serialize on the file's
    /// lock). Every arm of an edit is a claim about a document; applying a plan built from bytes
    /// that are gone is how a concurrent row disappears with no error anywhere. Nothing was
    /// written — the re-run reads the file as it now is.
    #[error(
        "'{path}' changed while this edit was being prepared — nothing was written; re-run the \
         command to act on the file as it is now"
    )]
    ManifestChanged { path: String },
    /// The manifest a verb was about to BIRTH appeared on disk between the absence check and the
    /// exclusive create (an outside editor writing the same path — topos's own writers serialize
    /// on the file's lock). A birth is a claim the file does not exist; landing it anyway would
    /// overwrite the outside writer's document. Nothing was written — the re-run reads the file
    /// as it now is.
    #[error(
        "'{path}' appeared while topos was creating it — nothing was written; re-run the command \
         to act on the file as it is now"
    )]
    ManifestExists { path: String },
    /// A project-scoped `add`/`remove` found NO `topos.toml` covering the working directory.
    /// Creation is `init`'s job (the cargo/pnpm/git precedent) — the edit is refused, never
    /// rerouted to another scope, and the two ways out ride as executable next actions.
    #[error(
        "no topos.toml covers this folder — `topos init` creates one here, or add `-g` to act on \
         your machine-wide file (~/.topos/topos.toml)"
    )]
    NoManifest,
    /// A terminal protocol outcome the verb does not special-case (e.g. the server's `RetryableFailure` /
    /// `Unavailable` / `PermanentFailure`), carried verbatim so the agent branches on the TRUE outcome
    /// (not a generic transport error). `retryable` selects a Retry next-action + the outcome class.
    ///
    /// The MESSAGE carries only the server's fine code: `outcome` already rides the envelope
    /// structurally (and drives the TTY's transient/permanent phrasing), so repeating its Rust
    /// spelling in prose only leaked an internal name into a human line.
    #[error("the server could not complete this operation ({code})")]
    PlaneTerminal {
        outcome: TerminalOutcome,
        code: String,
        retryable: bool,
    },
    /// `topos upgrade` is ambiguous under the reshaped verbs — it could mean "update my skills" (now
    /// `topos update`) or "update the topos CLI itself" (now `topos self-update`). Refuse and disambiguate
    /// rather than silently pick one; the `next_actions` carry both concrete commands.
    #[error(
        "`topos upgrade` is ambiguous — run `topos update` to update your skills, or `topos self-update` \
         to update the topos CLI itself"
    )]
    UpgradeAmbiguous,
    /// The ONE uniform not-found: a workspace / channel / skill address that resolved to nothing — locally
    /// AND on the plane, whose 404 deliberately does not distinguish "does not exist" from "not yours".
    /// The client mirrors that non-answer (no enumeration oracle on either side); the guidance is the
    /// fixed dual reading. `target` is the user's own argv token, shown verbatim — a bare token ONLY,
    /// never a phrase (a context-phrased miss uses [`ClientError::NotAvailable`] instead, so the two
    /// templates never nest).
    #[error(
        "'{target}' was not found, or is not visible to you — check the address; if you were invited, \
         confirm with your inviter"
    )]
    TargetNotFound { target: String },
    /// A CONTEXT-PHRASED miss with the same non-oracle discipline and the same `NOT_FOUND` code: the
    /// message is ONE complete self-authored sentence (which catalog was consulted, the dual
    /// existence/visibility reading) shown verbatim — never nested inside the fixed
    /// [`ClientError::TargetNotFound`] template.
    #[error("{0}")]
    NotAvailable(String),
    /// A name resolved to MORE than one thing the invocation could have meant — several workspace
    /// resources (across workspaces, or across the channel/skill kinds), or several lines of one
    /// manifest. Each way out rides as a STRUCTURED [`TargetCandidate`]
    /// (`<workspace>/channels/<name>` / `<workspace>/skills/<name>`, a manifest reference, or a
    /// reference plus the set line that delivers it), never as text a surface has to re-parse.
    ///
    /// The MESSAGE states the refusal only. The candidates are argv, so they ride the surfaces AS
    /// argv: the envelope's `data.candidates` (machine-readably, unchanged) and one runnable
    /// `next_actions` entry each, rebuilt around the verb that refused
    /// ([`crate::render::next_actions`]) — which is what the TTY's `try:` block prints. Inlining
    /// them into this sentence made a paste-ready command read as prose and hid the `--via` form's
    /// token boundary.
    ///
    /// `global` preserves the refused invocation's `-g` in every rebuilt command, exactly as
    /// [`WorkspaceHint::global`] does for the spellings it names: the two manifests are two
    /// different files, so an offered command that drops the flag edits the OTHER one. A wrong
    /// offered command is worse than none.
    #[error("'{name}' is ambiguous here — more than one reference answers to it")]
    AmbiguousTarget {
        name: String,
        candidates: Vec<TargetCandidate>,
        global: bool,
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
            ClientError::NoChanges { .. } => "NO_CHANGES",
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
            ClientError::NoUntrackedSkill { .. } => "NO_UNTRACKED_SKILL",
            // The name-oriented twin of AlreadyTracked shares its code (agents branch the same on either).
            ClientError::AlreadyTrackedName { .. } => "ALREADY_TRACKED",
            ClientError::HarnessMismatch { .. } => "HARNESS_MISMATCH",
            ClientError::AmbiguousHarness { .. } => "AMBIGUOUS_HARNESS",
            ClientError::AmbiguousScope { .. } => "AMBIGUOUS_SCOPE",
            ClientError::AmbiguousWorkspace { .. } => "AMBIGUOUS_WORKSPACE",
            ClientError::HarnessNotFound(_) => "HARNESS_NOT_FOUND",
            ClientError::PathNotName { .. } => "PATH_NOT_NAME",
            ClientError::PlacementUnsupported { .. } => "PLACEMENT_UNSUPPORTED",
            ClientError::PlacementsDiverged { .. } => "PLACEMENTS_DIVERGED",
            ClientError::UnknownGoBackVersion { .. } => "UNKNOWN_GOBACK_VERSION",
            ClientError::Plane(_) => "PLANE_ERROR",
            // The login-flow failures speak session language (the retired enrollment vocabulary
            // is gone from the wire).
            ClientError::Enrollment(_) => "LOGIN_FAILED",
            // The session-required family: the shared refusal and the address-carrying form.
            ClientError::NotEnrolled => "SESSION_REQUIRED",
            ClientError::SessionRequired { .. } => "SESSION_REQUIRED",
            ClientError::ApprovalMismatch { .. } => "CONSENT_MISMATCH",
            ClientError::Conflict { .. } => "CONFLICT",
            ClientError::Denied(_) => "DENIED",
            // A review verdict on a no-longer-open proposal — an open code, its own domain refusal.
            ClientError::ReviewNotOpen(_) => "REVIEW_NOT_OPEN",
            ClientError::PublishBlocked { .. } => "PUBLISH_BLOCKED",
            ClientError::PendingOp { .. } => "PENDING_OP",
            ClientError::WorkspaceSelection(_) => "WORKSPACE_SELECTION",
            ClientError::PlaneRejected(_) => "PLANE_REJECTED",
            // A self-update checksum mismatch is an integrity failure (same family as verify-on-read).
            ClientError::ChecksumMismatch { .. } => "INTEGRITY_ERROR",
            // A failed (or missing) release signature is the same integrity family — same code, so an
            // agent branches identically on either self-update integrity refusal.
            ClientError::SignatureInvalid { .. } => "INTEGRITY_ERROR",
            // A not-writable install location is a filesystem-shaped, permanent failure.
            ClientError::UpgradeUnwritable(_) => "IO_ERROR",
            // The remote-import family (each machine-branchable; a fetch fault is retryable, the rest are
            // permanent selection/placement errors the agent resolves by changing its argv).
            ClientError::RemoteFetch { .. } => "REMOTE_FETCH",
            ClientError::NoSkillInSource { .. } => "NO_SKILL_IN_SOURCE",
            ClientError::SkillNotInRepo { .. } => "SKILL_NOT_IN_REPO",
            ClientError::AmbiguousSkillInRepo { .. } => "AMBIGUOUS_SKILL",
            ClientError::DuplicateSkillName { .. } => "AMBIGUOUS_SKILL",
            ClientError::PlacementOccupied { .. } => "PLACEMENT_OCCUPIED",
            // A concurrent edit of the same document — the file-scoped twin of a pointer CAS loss.
            ClientError::ManifestChanged { .. } => "MANIFEST_CHANGED",
            // A concurrent BIRTH of the same document (the file appeared after the absence check).
            ClientError::ManifestExists { .. } => "MANIFEST_EXISTS",
            // No file to edit at the scope the invocation asked for — never a reroute to the other.
            ClientError::NoManifest => "NO_MANIFEST",
            // The plane's fine code rides the Display message + context; the agent branches on `outcome`.
            ClientError::PlaneTerminal { .. } => "PLANE_TERMINAL",
            ClientError::UpgradeAmbiguous => "UPGRADE_AMBIGUOUS",
            ClientError::TargetNotFound { .. } => "NOT_FOUND",
            // The context-phrased miss shares the uniform code (agents branch the same).
            ClientError::NotAvailable(_) => "NOT_FOUND",
            // The address-grammar ambiguity shares the tracked-name ambiguity's code (agents branch the
            // same); the candidates additionally ride the envelope's `data.candidates`.
            ClientError::AmbiguousTarget { .. } => "AMBIGUOUS_NAME",
        }
    }

    /// The terminal outcome the agent branches on.
    pub(crate) fn outcome(&self) -> TerminalOutcome {
        match self {
            // Every name-ambiguity shape (across tracked skills, across harnesses, across scopes) is the
            // same terminal class — the agent disambiguates and retries.
            ClientError::AmbiguousName { .. }
            | ClientError::AmbiguousHarness { .. }
            | ClientError::AmbiguousScope { .. }
            // A name several connected workspaces publish is the same class — pick a spelling, retry.
            | ClientError::AmbiguousWorkspace { .. }
            // A repo holding several skills (or several dirs of one name) is the same "disambiguate and
            // retry" class — as is a workspace-resource name matching across workspaces or kinds.
            | ClientError::AmbiguousSkillInRepo { .. }
            | ClientError::DuplicateSkillName { .. }
            | ClientError::AmbiguousTarget { .. } => TerminalOutcome::AmbiguousName,
            // A network fetch fault is transient — the agent may retry the same import.
            ClientError::RemoteFetch { permanent, .. } => {
                if *permanent {
                    TerminalOutcome::PermanentFailure
                } else {
                    TerminalOutcome::RetryableFailure
                }
            }
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
            ClientError::Conflict { .. } => TerminalOutcome::Conflict,
            // A document that moved under a prepared edit is the same class: re-read, re-decide,
            // then act — never a blind retry of the same plan. So is a document that APPEARED
            // under a prepared birth.
            ClientError::ManifestChanged { .. } | ClientError::ManifestExists { .. } => {
                TerminalOutcome::Conflict
            }
            ClientError::Denied(_) => TerminalOutcome::Denied,
            ClientError::PublishBlocked { .. } => TerminalOutcome::Diverged,
            // Divergent per-placement edits are the same class as a diverged draft: local
            // reconciliation (or the disclosed reset) resolves it, never a blind retry.
            ClientError::PlacementsDiverged { .. } => TerminalOutcome::Diverged,
            // An in-flight op must be settled, then the command retried.
            ClientError::PendingOp { .. } => TerminalOutcome::RetryableFailure,
            // A definitive 4xx rejection — the op cannot succeed as-is.
            ClientError::PlaneRejected(_) => TerminalOutcome::PermanentFailure,
            // A tampered/corrupt download will not heal on a retry against the same bad bytes.
            ClientError::ChecksumMismatch { .. } => TerminalOutcome::PermanentFailure,
            // A release that fails (or lacks) its mandatory signature will not heal on a retry either —
            // fail closed, never a retry loop against the same unverifiable bytes.
            ClientError::SignatureInvalid { .. } => TerminalOutcome::PermanentFailure,
            // A read-only / package-managed install location will not heal on a retry.
            ClientError::UpgradeUnwritable(_) => TerminalOutcome::PermanentFailure,
            // The plane's terminal outcome, surfaced verbatim (not flattened to a transport error).
            ClientError::PlaneTerminal { outcome, .. } => *outcome,
            _ => TerminalOutcome::PermanentFailure,
        }
    }

    /// The live generation to carry on a `CONFLICT` envelope (the rebase target the agent pulls to) —
    /// `None` for every other error.
    pub(crate) fn current_generation(&self) -> Option<u64> {
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
