//! The client's one typed error family. The bin maps each variant to a stable wire `code` + a
//! [`TerminalOutcome`]; raw `gix`/io strings stay internal and never reach a user surface.

use topos_gitstore::{GitstoreError, VerifyError};
use topos_types::TerminalOutcome;
use topos_types::results::ReceiptScope;

use topos_core::digest::RejectReason;

/// How a remote-source read failed — the ONE classification the retry hint, the update sweep's
/// per-round circuit breaker, and the auto-update clock all read.
///
/// It widens the old permanent/transient bit by ONE distinction the sweep genuinely needs: whether
/// the host answered at all. A sweep over N sources behind a dead network must cost ONE connect
/// timeout, not N — but a 500 about a single repo says nothing about the next one, so only the
/// never-reached case may short-circuit the rest. This mirrors [`crate::plane::PlaneError`]'s
/// `Unreachable` / `Unavailable` split, so both lanes classify a fault the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchFault {
    /// Connect-level: the host was never reached (DNS, connect, TLS, timeout).
    Unreachable,
    /// The host answered, but not usefully — a failure status, a rate-limit refusal, or a body
    /// that never fully arrived. Retry later; nothing here is about this source's existence.
    ///
    /// `retry_after_ms` is the instant (epoch millis) the host asked to be left alone until, when
    /// it said so. It rides the FAULT rather than a success path because the answers that carry it
    /// — 429, 503 — are exactly the ones that fail.
    Unavailable { retry_after_ms: Option<i64> },
    /// The host answered ABOUT this source, permanently: gone, invisible, or not a repo at all.
    /// Retrying the same reference gets the same answer.
    Gone,
}

/// ONE copy in a placement freeze ([`ClientError::PlacementsDiverged`]), spelled the two ways the
/// refusal needs it: the folder as a person reads it, and the `--dest` value that names it back to
/// a verb. Both come from the ONE spelling helper (`ops::dest_select::copy_spellings`), so the
/// folder the refusal prints and the folder its offered commands accept can never drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DivergedCopy {
    /// `project/.agents/skills/coolify-deploy` · `~/.claude/skills/coolify-deploy`.
    pub display: String,
    /// `.agents/skills` · `~/.claude/skills` — the `--dest` value naming this copy.
    pub dest: String,
}

/// The scope flag every offered `update` carries: ` -g` for the machine, nothing for a project.
/// The same discipline `list`'s rows keep — a command a refusal prints must act on the copy the
/// reader is looking at, whichever directory they happen to run it from.
pub(crate) fn scope_flag(global: bool) -> &'static str {
    if global { " -g" } else { "" }
}

impl FetchFault {
    /// The host answered unusefully and asked for nothing in particular.
    pub(crate) fn unavailable() -> Self {
        Self::Unavailable {
            retry_after_ms: None,
        }
    }

    /// Whether retrying the same reference can never help — what the terminal outcome and the
    /// envelope's `retryable` bit derive from.
    pub(crate) fn permanent(self) -> bool {
        matches!(self, Self::Gone)
    }

    /// Whether the host answered at all. `false` is the ONLY thing that may trip a round's
    /// circuit breaker.
    pub(crate) fn reached(self) -> bool {
        !matches!(self, Self::Unreachable)
    }

    /// When the host asked to be called back, if it did.
    pub(crate) fn retry_after_ms(self) -> Option<i64> {
        match self {
            Self::Unavailable { retry_after_ms } => retry_after_ms,
            _ => None,
        }
    }
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
/// THE REFERENCE IS ARGV, ALWAYS. It is the token this binary reads back unchanged — an absolute
/// path, never a `~/…` only a SHELL expands. An agent execs `argv` directly, so a home-abbreviated
/// token there resolves to nothing and the offered command fails as a missing source. Where a
/// person reads a shorter form better, that form rides [`display`](Self::display) and reaches the
/// TTY alone.
///
/// Ordering is derived over `(reference, via)` — which is the spelling order too, since a
/// reference holds no byte below `-`/space that could reorder the pair — so a sorted candidate
/// list reads the same on both surfaces.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TargetCandidate {
    /// The candidate's own reference — one argv token, whatever it contains, in the form this
    /// binary resolves without a shell's help.
    pub reference: String,
    /// How a PRINTED line spells it, when a person reads that better than the argv form (`~/…`
    /// for a folder under this machine's home). `None` ⇒ the reference itself. Never argv.
    pub display: Option<String>,
    /// The SET line whose member rewrite this candidate selects (`--via <set-reference>`), for a
    /// bundle no row of its own spells. `None` for a candidate that is a row (or a resource).
    pub via: Option<String>,
}

impl TargetCandidate {
    /// A candidate that stands on its own reference — no set line to name.
    pub(crate) fn plain(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            display: None,
            via: None,
        }
    }

    /// A FOLDER candidate: the absolute path a command must carry, and the `~/`-abbreviated form a
    /// printed line reads better as.
    pub(crate) fn folder(absolute: impl Into<String>, display: impl Into<String>) -> Self {
        let absolute = absolute.into();
        let display = display.into();
        Self {
            display: (display != absolute).then_some(display),
            reference: absolute,
            via: None,
        }
    }

    /// A candidate reached only THROUGH a set line: removing it rewrites that one line.
    pub(crate) fn via(reference: impl Into<String>, set: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            display: None,
            via: Some(set.into()),
        }
    }

    /// The rendered spelling — what the envelope's `data.candidates` carries. Exactly the text
    /// the two parts read as on a command line, so a consumer that has always read these strings
    /// sees no change. It is the ARGV form: a machine reading this field is going to run it.
    pub(crate) fn spelling(&self) -> String {
        match &self.via {
            Some(set) => format!("{} --via {set}", self.reference),
            None => self.reference.clone(),
        }
    }

    /// The candidate's tokens as a PRINTED line spells them — [`argv_tokens`](Self::argv_tokens)
    /// with the folder abbreviation applied. A shell expands the `~/` back to exactly the argv
    /// form, so the two lines run the same command.
    pub(crate) fn printed_tokens(&self) -> Vec<String> {
        let mut tokens = self.argv_tokens();
        if let (Some(display), Some(first)) = (&self.display, tokens.first_mut()) {
            *first = display.clone();
        }
        tokens
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

/// The floor clause of an update-required refusal: the version the server named, when it named one
/// this build could read. A 426 with an unreadable body still refuses — the status alone is the
/// whole signal, and "a newer topos" is the honest thing to say about a floor nobody stated.
fn required_version(min: Option<&str>) -> String {
    match min {
        Some(v) => format!("at least topos {v}"),
        None => "a newer topos".to_owned(),
    }
}

/// WHAT already claims a directory an `add` was asked to adopt: the manifest FILE holding the row
/// that installs it, and whether that file is the machine one — which the drop command has to
/// spell with `-g` or it would act on the other scope.
#[derive(Debug, Clone)]
pub(crate) struct TrackedBy {
    /// The `topos.toml` whose row resolves to the folder.
    pub manifest: String,
    /// The row lives in the MACHINE file (`~/.topos/topos.toml`), so `remove` needs `-g`.
    pub global: bool,
}

/// The [`ClientError::AlreadyTracked`] sentence. Two shapes, and the difference is honesty about
/// what was found: with the claiming row in hand the refusal names the file and the exact drop
/// command; without it (a retained record no reachable manifest spells) it names the folder and
/// the exits that need no scope. Neither shape ever prints the store id.
fn already_tracked_message(name: &str, dir: &str, claim: &Option<TrackedBy>) -> String {
    match claim {
        Some(TrackedBy { manifest, global }) => format!(
            "'{name}' already tracks {dir} — the row in {manifest} is what installs it; `topos \
             remove {}{name}` drops that row, or edit the row to change where it lands",
            if *global { "-g " } else { "" }
        ),
        None => format!(
            "'{name}' already tracks {dir} — edit it in place (`topos diff {name}` shows your \
             changes), or drop it first with `topos remove {name}`"
        ),
    }
}

/// WHICH of the two manifests an add answer is about, in the one PORTABLE spelling both the
/// receipt and the already-added refusal name it by. The absolute path a reader's own cwd would
/// make of it is noise: there is exactly one project file and exactly one machine file, and this
/// is what each is called.
pub(crate) fn scope_file(scope: ReceiptScope) -> &'static str {
    match scope {
        ReceiptScope::Project => "to this project (./topos.toml)",
        ReceiptScope::Machine => "machine-wide (~/.topos/topos.toml)",
    }
}

/// The [`ClientError::AlreadyAdded`] answer — the add receipt's own two lines, in the past tense.
fn already_added_message(name: &str, scope: ReceiptScope, from: &Option<String>) -> String {
    let lead = format!("{name} is already added {}", scope_file(scope));
    match from {
        Some(source) => format!("{lead}\nsource: {source}"),
        None => lead,
    }
}

/// `arg` spelled so a shell reads it as a PATH — the one form every path-taking verb accepts.
/// A token that already says "path" is handed back untouched; only a bare relative one is
/// prefixed. Blind prefixing turned `~/deploy` into `./~/deploy`, a path to nothing.
pub(crate) fn path_token(arg: &str) -> String {
    if arg.starts_with("./")
        || arg.starts_with("../")
        || arg.starts_with('/')
        || arg.starts_with("~/")
    {
        arg.to_owned()
    } else {
        format!("./{arg}")
    }
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
    /// non-UTF-8 name) or a kernel path reject (absolute / `..` / NUL / collision). The reason is
    /// a bundle-relative name or the kernel's own fixed words, so the whole `Display` is a safe
    /// terminal line (see [`crate::render::safe_message`]).
    #[error("the skill directory was rejected — {0}")]
    Scan(String),
    /// The folder `add` was pointed at is a LINK SHELL — its root `SKILL.md` links elsewhere —
    /// and the folder it stands for could not be resolved: the link is broken, or the shell also
    /// holds files of its own, which makes "the original" a question only the person can answer.
    /// The `SCAN_REJECTED` family: like every other scan reject, the folder as it stands is not
    /// adoptable.
    #[error("can't add {name} — {reason}")]
    LinkedSource { name: String, reason: String },
    /// The bundle has no files (after excluding `.git/` + `.DS_Store`) — not a skill.
    #[error("the skill directory has no files to adopt")]
    EmptyBundle,
    /// The source path overlaps `~/.topos/` (equal / ancestor / descendant) — refused so uninstall can
    /// never delete user bytes and the footprint oracle never collapses.
    #[error("the source path overlaps the topos home directory")]
    SourceOverlap,
    /// `add <path>` named a folder that is not on this machine. PERMANENT and refused BEFORE any
    /// store work: the same command will meet the same absence, so an agent that read `retryable`
    /// would loop on it — which is exactly what the generic canonicalize failure this replaces
    /// invited. The `path` is the user's own token, shown VERBATIM.
    #[error("{path} does not exist — nothing was added.")]
    SourceMissing { path: String },
    /// A skill with this id already exists on disk — `add` fails closed rather than overwrite/merge.
    #[error("a skill with this id already exists")]
    SkillExists,
    /// The directory is already tracked in place (same canonical path) — re-adopting would mint a second
    /// record for one mutable dir, so `add` refuses and points at what already claims it (edits already
    /// surface as a draft via `diff`).
    ///
    /// Everything the refusal shows is something a person can act on: the tracked skill's own NAME, the
    /// FOLDER, and — when the producer could resolve it — the manifest FILE holding the row that installs
    /// it, with the drop command spelled for that file's scope. The internal store id never appears here;
    /// it names nothing anyone can type, and a refusal that ends in `topos_a9b7ee2b…` leaves the reader
    /// with no next step at all.
    #[error("{}", already_tracked_message(.name, .dir, .claim))]
    AlreadyTracked {
        name: String,
        dir: String,
        claim: Option<TrackedBy>,
    },
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
    /// A bare `add <name>` whose name ALREADY stands in the scope the invocation is acting in —
    /// the answer to "add this", not a failure to do it. It reads exactly like the add receipt it
    /// mirrors: what is added and which of the two files asks for it, then where that bundle comes
    /// from, in the one spelling that stays runnable.
    ///
    /// `from` — the value the `source:` line carries — is absent only where the record names no
    /// reference this machine can spell (no workspace, no upstream, and no folder left on disk):
    /// better no line than a vague one. The code stays `ALREADY_TRACKED`, so an agent branches on
    /// it exactly as it always has.
    #[error("{}", already_added_message(.name, *.scope, .from))]
    AlreadyAdded {
        name: String,
        scope: ReceiptScope,
        from: Option<String>,
    },
    /// `publish <skill>@<harness>` named an agent that reads NONE of the folders the ALREADY-TRACKED
    /// skill of that name stands in. The auto-add convenience cannot re-add a tracked name, and a
    /// mismatched `@<harness>` suffix likely means a DIFFERENT copy was intended — refused so a stray
    /// suffix never silently publishes the tracked bytes. The folders are the user's own directories
    /// and the slug is the user's own token, both shown VERBATIM.
    #[error(
        "skill '{name}' is tracked in {}, which '{requested}' does not read — publish it as \
         `topos publish {name}`",
        if folders.is_empty() { "no agent folder".to_owned() } else { folders.join(", ") }
    )]
    HarnessMismatch {
        name: String,
        requested: String,
        folders: Vec<String>,
    },
    /// ONE shape for every ambiguity a SOURCE positional meets: the name resolved to more than one
    /// thing — several untracked folders on this machine, several connected workspaces publishing
    /// it, several records standing under it, or any mix. The refusal is an ANSWER, not a failure:
    /// it states the choice and then spells one RUNNABLE command per candidate, so the reader
    /// picks a line instead of translating prose into an invocation.
    ///
    /// The message carries the statement alone. The candidates are argv, so they ride the surfaces
    /// AS argv — the envelope's `data.candidates` and one `next_actions` entry each — and the TTY
    /// prints exactly those commands under the statement.
    ///
    /// AN OFFERED COMMAND IS THE WHOLE REQUEST. The rebuild is the refused invocation with the
    /// ambiguous positional swapped for one candidate and NOTHING else touched, so a
    /// `publish foo --propose` offers a proposal and an `add foo -a cursor` still installs for
    /// cursor alone — dropping those flags would offer an act nobody asked for. `token` is the
    /// positional as the person typed it (`foo`, `foo@codex`), which is what the swap finds.
    ///
    /// `verb` and `global` serve the FALLBACK spelling, for the surface that has no argv to rebuild
    /// from: the verb that refused (so a `publish` never sends its reader to `add`) and the `-g`
    /// that decides which of the two manifests the offered command would edit.
    #[error("{name} is ambiguous, pick one:")]
    AmbiguousSource {
        name: String,
        token: String,
        verb: String,
        candidates: Vec<TargetCandidate>,
        global: bool,
    },
    /// `add <skill>@<harness>` named a harness that holds no untracked skill of that name. The message
    /// names where the skill IS found (if anywhere), all shown VERBATIM (slugs + the user's own tokens).
    #[error("{0}")]
    HarnessNotFound(String),
    /// A verb resolving a NAME was given a bare token that turned out to name an existing path on
    /// disk (a `~`-prefixed token, or a cwd entry) rather than a discovered skill name — the user
    /// likely meant a path. Usage guidance shown VERBATIM (the arg is the user's own token).
    ///
    /// `verb` is the verb that REFUSED — the fix re-runs that same command, never a different one:
    /// a person who typed `publish` is not asking to `add`, and being told to is a dead end they
    /// have to translate. `path_token` keeps the suggestion runnable: a token that already spells
    /// a path is left alone, because prefixing one blindly produced `./~/deploy`, which names
    /// nothing at all.
    #[error(
        "'{arg}' looks like a path, not a skill name — spell it as a path: `topos {verb} {}`",
        path_token(arg)
    )]
    PathNotName { arg: String, verb: String },
    /// The placement cannot be materialized safely (a non-directory sits where a skill dir belongs, a
    /// symlink cannot be resolved to a directory, or the filesystem supports no safe swap) — refused
    /// rather than risk clobbering or a torn write.
    #[error("the skill's files cannot be written safely — {reason}")]
    PlacementUnsupported { reason: String },
    /// MORE than one of a skill's placements holds a DIFFERENT local edit — there is no single draft
    /// to sync, diff, or publish, and nothing is overwritten (the typed freeze).
    ///
    /// Each copy is carried twice over — the folder as a reader sees it, and the `--dest` spelling
    /// that names it back to a verb — because the way out is CHOOSING one copy: publish it, read
    /// it, or drop it, and the survivor then becomes the single ordinary draft. Discarding every
    /// copy's edits stays available, and stays last: it is the widest loss on offer, not the first
    /// thing to reach for.
    ///
    /// `global` is the SCOPE the frozen copies live in, and every command this refusal offers is
    /// spelled for it — a machine-scope freeze says `topos update -g …`, because a bare `update`
    /// read from inside a checkout drives the PROJECT and would find no copy to act on.
    #[error(
        "'{skill}' has different edits in {} folders ({}) — name the one to work with (`--dest \
         <folder>` on `topos publish {skill}`, `topos diff {skill}`, or `topos update{} {skill} \
         --reset`), or discard every copy's edits with `topos update{} {skill} --reset` (each copy \
         is snapshotted first)",
        copies.len(),
        copies.iter().map(|c| c.display.as_str()).collect::<Vec<_>>().join(", "),
        scope_flag(*global),
        scope_flag(*global)
    )]
    PlacementsDiverged {
        skill: String,
        copies: Vec<DivergedCopy>,
        global: bool,
    },
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
    /// The server holds no outgoing mail and invitations travel by mail (`MAIL_NOT_CONFIGURED`) —
    /// a denial with a describable way out, so it gets a variant of its own rather than riding
    /// [`ClientError::Denied`]'s wire-code-in-the-message shape. THE CODE RIDES THE CODE FIELD:
    /// the sentence a person reads never repeats it, so no surface prints a SCREAMING_SNAKE word
    /// before the first English one.
    #[error(
        "the server has no outgoing mail configured, and invitations travel by mail — ask an \
         admin to configure SMTP on the server, or manage invitations from the workspace's \
         People page on the web"
    )]
    MailNotConfigured,
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
    ///
    /// `global` is the scope the blocked copy lives in. `publish` resolves a bare name home-first,
    /// so the copy it refused over is not always the one the reader's directory would pick — the
    /// offered exits are spelled for the copy that is actually blocked, or following them lands on
    /// the other scope and refuses again.
    #[error("publish is blocked — resolve the merge conflict in this skill first")]
    PublishBlocked { skill: String, global: bool },
    /// A `publish` is refused because this copy does not include the served `current`: the team moved
    /// ahead and this machine has not taken it (the ordinary behind state). Shipping from here would
    /// land bytes with the team's change nowhere inside — and the plane refuses it anyway, since a
    /// candidate's first parent must be the version `current` names — so the update comes first.
    /// Refused before any build / WAL / send, and before the describe reads a byte of the network, so
    /// it costs nothing and mutates nothing. A `--propose` is exempt: it moves no pointer, so it is
    /// the one act a behind copy can always safely perform.
    ///
    /// The TTY renders the way out under the sentence ([`crate::render::err_tty`]); the envelope keeps
    /// the sentence and carries the same command as its next action. `global` is the scope the
    /// behind copy lives in, for the same reason [`ClientError::PublishBlocked`] carries one.
    #[error("{skill}: your version is behind.")]
    PublishBehind { skill: String, global: bool },
    /// `--keep-mine` was typed for a bundle where no merge has stopped — an up-to-date copy, a plain
    /// draft with nothing pending, a clean copy that is merely behind, or a divergence the merge has
    /// not been run on. There is nothing to finish, and every one of those states has a different
    /// silent wrong answer (report success, apply the team's version, drop what the merge was about
    /// to land), so all four refuse toward the merge instead.
    #[error(
        "no merge has stopped on '{skill}' — run `topos update{} {skill}` to merge the team's \
         version in; it stops only where you both changed the same lines",
        scope_flag(*global)
    )]
    NoStoppedMerge { skill: String, global: bool },
    /// `--keep-mine` found the merge's workbench folder PRESENT but unreadable as a bundle (a
    /// symlink or file where the folder belongs, a symlink or non-regular file inside it, a
    /// non-UTF-8 name, an emptied folder, a read failure). Refused with nothing committed and
    /// nothing placed, because the folder is the ONLY copy of a hand resolution — it sits outside
    /// the placement map, so no snapshot rail holds it, and treating it as absent would commit the
    /// original draft over every placement and then delete it.
    ///
    /// `global` is the scope the bundle lives in, so the offered `--reset` is spelled for the copy
    /// the reader is looking at.
    #[error(
        "'{skill}' cannot be resolved from {path} — that folder is not readable as a bundle \
         ({reason}); fix it and re-run, or take the team's version with `topos update{} {skill} \
         --reset`",
        scope_flag(*global)
    )]
    ConflictCopyUnreadable {
        skill: String,
        path: String,
        reason: String,
        global: bool,
    },
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
    /// The SERVER's half of the version floor: it answered HTTP 426 — it no longer speaks to a
    /// topos this old. Not transient (the same request refuses until this binary is replaced), and
    /// not the caller's mistake, so it reads as its own dead end rather than a rejected request.
    /// `min` is the floor the refusal named, when it named one this build could read; the fix
    /// rides structurally as the `SELF_UPDATE` next action.
    #[error(
        "this server no longer speaks to this topos version — it requires {}",
        required_version(min.as_deref())
    )]
    UpdateRequired { min: Option<String> },
    /// The CLIENT's half of the same floor, read off the protocol card before a login commits: the
    /// server is older than the oldest release this build speaks to. Both remedies are real, so
    /// both are named — the person who runs the server can update it, or this machine can go back
    /// to the server's release (the runnable pin rides the `SELF_UPDATE` next action).
    #[error(
        "that server is too old for this topos — it runs {server_version}, and this build speaks \
         to {} and later; ask whoever runs the server to update it, or pin this machine back to \
         the server's release",
        crate::compat::MIN_SERVER_VERSION
    )]
    ServerTooOld { server_version: String },
    /// The MCP corner of the same client-side floor, checked only when a publish would record a
    /// `kind = "mcp"` bundle: the workspace's server predates MCP bundle kinds, so its catalog
    /// would silently record a SKILL while the client receipt claimed otherwise. Refused BEFORE
    /// the op WAL — nothing is minted, nothing is sent — with the server-too-old shape (the same
    /// wire code + `server_version` field), so an agent branches exactly as it does on the login
    /// floor. The one remedy is the server's, so only that is named.
    #[error(
        "that server does not record MCP bundles yet — it runs {server_version}, and publishing \
         one needs {} or later; ask whoever runs the server to update it",
        crate::compat::MCP_MIN_SERVER_VERSION
    )]
    McpServerTooOld { server_version: String },
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
    /// A remote source (an `add <owner/repo>` import, or an auto-update check of a tracked one)
    /// could not be read. `fault` separates the causes retrying can never fix (a not-found
    /// repo/ref, a malformed reference) from the transport faults a retry may clear — the outcome
    /// and the envelope's `retryable` both derive from it, so a 404 never invites a retry loop —
    /// and, within the retryable half, whether the host was reached at all. The message is
    /// all-public (the source + a humanized reason), so it is shown VERBATIM.
    #[error("could not fetch {msg}")]
    RemoteFetch { msg: String, fault: FetchFault },
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
    /// An MCP server document the gate refused ([`crate::mcp_validate`]) — the shared, two-language
    /// rule set at `tests/fixtures/mcp/`. The code is the vector's own
    /// (`MCP_INVALID` · `MCP_LOCAL_REFUSED` · `MCP_NO_STREAMABLE_REMOTE` · `MCP_INSECURE_URL` ·
    /// `MCP_URL_TEMPLATE` · `MCP_SECRET_REFUSED`), so an agent branches on the same word both
    /// tiers use; the message is this code's own sentence and is shown VERBATIM. It never quotes
    /// the document — a refusal for carrying a credential must not echo the credential.
    #[error("{message}")]
    McpRefused {
        code: crate::mcp_validate::McpRefusalCode,
        message: String,
    },
    /// A plain `add`/`publish` named a folder whose kind the door cannot read off it, in either
    /// of the two ways that happens: the root holds a `server.json` and no `SKILL.md` (a server
    /// bundle the skill door would silently mis-kind, delivering raw JSON into skills dirs), or it
    /// holds BOTH markers (nothing in the folder says which one it is). Each carries its own
    /// sentence and both name the `--kind` words; `dir` is the user's own spelling, shown
    /// VERBATIM. Constructed only by [`ClientError::kind_required`] / [`ClientError::kind_ambiguous`].
    ///
    /// The refusal ARMS ON SILENCE only — an explicit `--kind` word always wins, because the guard
    /// exists to stop a silent mis-kind, not to overrule someone who said what they meant.
    #[error("{message}")]
    KindRequired {
        dir: String,
        message: String,
        /// Whether BOTH markers stand (the ambiguous shape) — the renderer offers both `--kind`
        /// words for it, and one for the server-folder shape.
        ambiguous: bool,
    },
    /// A manifest spells a RETIRED placement field (`path` / `harness` / a `[defaults.<kind>]`
    /// table) — the load refuses and the message teaches the exact per-row `dest` rewrite. Shown
    /// VERBATIM (the message is this code's own teaching over the user's own file content, like
    /// `InvalidArgument`); the TTY rendering closes with `nothing changed`, because the file was
    /// only read.
    #[error("{0}")]
    ManifestMigration(String),
    /// A manifest a verb had to READ refuses the grammar (bad TOML, an unknown field, an illegal
    /// value, a `dest` entry in the wrong scope's dialect). The fault is the FILE's — a
    /// user-authored `topos.toml`, never topos's own state — so the message (the file, the
    /// offending entry, the rule: the grammar's own teaching) is shown VERBATIM like the
    /// retired-spelling family, and the TTY closes with `nothing changed`, because the file was
    /// only read. Retired spellings keep their own [`ClientError::ManifestMigration`] shape;
    /// genuinely unreadable topos state stays [`ClientError::Corrupt`].
    #[error("{0}")]
    ManifestInvalid(String),
    /// A `-a` selector naming no known agent. `known` is the real registry's slugs, alphabetical,
    /// an ellipsis past a handful — so the fix is a copy-paste. Nothing was read past the argv;
    /// the TTY closes with `nothing changed`.
    #[error("unknown agent: {agent} — known: {known}")]
    UnknownAgent { agent: String, known: String },
    /// A `-a`/`--dest` selection an arm refuses whole (a feed reaches every agent by nature; the
    /// built-in manages its own placement) — one teaching sentence, shown VERBATIM; the TTY
    /// closes with `nothing changed`, because nothing was read past the argv.
    #[error("{0}")]
    SelectionRefused(String),
    /// A per-agent removal whose only copy is a SHARED folder several agents read — subtracting
    /// one agent cannot narrow one copy. The two ways out ride as structured `next_actions`
    /// (and the TTY renders them as aligned command lines): remove the copy for every agent, or
    /// re-add per-agent for the others and re-run.
    #[error("{name} has no {agent}-only copy — its one copy is {copy}, which several agents read")]
    SharedCopyOnly {
        name: String,
        agent: String,
        copy: String,
        /// `topos remove [-g] <name>` — remove it for every agent.
        remove_argv: Vec<String>,
        /// `topos add [-g] <ref> -a <slug>…` — keep it per-agent instead, then re-run.
        readd_argv: Vec<String>,
    },
}

impl From<crate::mcp_validate::McpRefusal> for ClientError {
    fn from(r: crate::mcp_validate::McpRefusal) -> Self {
        ClientError::McpRefused {
            code: r.code,
            message: r.message,
        }
    }
}

impl ClientError {
    /// A folder whose root holds a `server.json` and no `SKILL.md`: it is a server bundle, and the
    /// skill door would deliver raw JSON into skills dirs. The answer is definite, so the sentence
    /// leads with it.
    pub(crate) fn kind_required(dir: &str) -> ClientError {
        ClientError::KindRequired {
            dir: dir.to_owned(),
            message: format!(
                "{dir} looks like an MCP server (its root holds server.json and no SKILL.md) — \
                 `topos add --kind mcp {dir}` adds it as one, and `--kind skill` adds it as a skill"
            ),
            ambiguous: false,
        }
    }

    /// A folder holding BOTH markers. Its sibling above can say what the folder is; this one
    /// cannot, and must not guess: adopting it as a skill (which is what silence used to do) lands
    /// the SKILL.md and quietly drops the server — a person gets half of what they pointed at and
    /// no line saying so. Both words are named because both are real answers here.
    pub(crate) fn kind_ambiguous(dir: &str) -> ClientError {
        ClientError::KindRequired {
            dir: dir.to_owned(),
            message: format!(
                "{dir} holds both a SKILL.md and a root server.json, so nothing in the folder \
                 says which one it is — `topos add --kind skill {dir}` adopts it as a skill, and \
                 `topos add --kind mcp {dir}` adds it as an MCP server"
            ),
            ambiguous: true,
        }
    }

    /// The gate's refusal, told with the SUBJECT it judged in front of it — the folder or bundle
    /// name, in the same `<subject>: <sentence>` shape every other coded line in this CLI uses.
    ///
    /// The gate's sentences are about a document ("description is required and must be 1–500
    /// characters") and never about WHICH document, because the validator is shared with the web
    /// tier and knows only bytes. On the CLI that left a person staring at a rule with no way to
    /// tell which of their folders broke it. The sentence itself is still verbatim — nothing is
    /// reworded, and the document is still never quoted.
    pub(crate) fn mcp_refused_about(
        r: crate::mcp_validate::McpRefusal,
        subject: &str,
    ) -> ClientError {
        ClientError::McpRefused {
            code: r.code,
            message: format!("{subject}: {}", r.message),
        }
    }
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
            // An unresolvable link shell is the same family: the folder as it stands cannot be
            // scanned, so an agent branches on one code for every "this folder is not adoptable".
            ClientError::Scan(_) | ClientError::LinkedSource { .. } => "SCAN_REJECTED",
            ClientError::EmptyBundle => "EMPTY_BUNDLE",
            ClientError::SourceOverlap => "SOURCE_OVERLAP",
            // Its own word beside SOURCE_OVERLAP: both are refusals ABOUT the source a path-shaped
            // add named, and an agent branches on which one it met.
            ClientError::SourceMissing { .. } => "SOURCE_MISSING",
            ClientError::SkillExists => "SKILL_EXISTS",
            ClientError::AlreadyTracked { .. } => "ALREADY_TRACKED",
            // ONE code for the whole ambiguity family: the count-only form, the chooser every
            // add-side resolution collapses into, and the workspace-resource one below all read
            // AMBIGUOUS_NAME, so an agent branches once and reads `data.candidates` for the rest.
            ClientError::AmbiguousName { .. } | ClientError::AmbiguousSource { .. } => {
                "AMBIGUOUS_NAME"
            }
            ClientError::NoSuchSkill { .. } => "NO_SUCH_SKILL",
            ClientError::NoUntrackedSkill { .. } => "NO_UNTRACKED_SKILL",
            // The name-oriented twins of AlreadyTracked share its code (agents branch the same on any).
            ClientError::AlreadyTrackedName { .. } | ClientError::AlreadyAdded { .. } => {
                "ALREADY_TRACKED"
            }
            ClientError::HarnessMismatch { .. } => "HARNESS_MISMATCH",
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
            // Its own code, so an agent branches on WHY the invite was refused; the sentence it
            // carries never spells that code out loud.
            ClientError::MailNotConfigured => "MAIL_NOT_CONFIGURED",
            // A review verdict on a no-longer-open proposal — an open code, its own domain refusal.
            ClientError::ReviewNotOpen(_) => "REVIEW_NOT_OPEN",
            ClientError::PublishBlocked { .. } => "PUBLISH_BLOCKED",
            // A copy behind the served `current` is the SAME state the plane's compare-and-set names
            // CONFLICT, caught locally one step earlier — so it carries that code, and an agent that
            // already branches on a stale base needs no new arm.
            ClientError::PublishBehind { .. } => "CONFLICT",
            // No merge stopped, so nothing is blocked — this is a wrong command for the state,
            // and the argv is the fix.
            ClientError::NoStoppedMerge { .. } => "INVALID_ARGUMENT",
            // The workbench folder an exit reads is unreadable — the same "resolve the divergence
            // locally" family as the publish block, so agents branch on one code for both.
            ClientError::ConflictCopyUnreadable { .. } => "PUBLISH_BLOCKED",
            ClientError::PendingOp { .. } => "PENDING_OP",
            ClientError::WorkspaceSelection(_) => "WORKSPACE_SELECTION",
            ClientError::PlaneRejected(_) => "PLANE_REJECTED",
            // The two halves of the version floor. The server-refused half carries the SERVER's own
            // code verbatim, so an agent branches identically whether it read the 426 body or not.
            ClientError::UpdateRequired { .. } => "CLI_UPDATE_REQUIRED",
            ClientError::ServerTooOld { .. } => "SERVER_TOO_OLD",
            // The MCP corner shares the shape and the code — agents branch identically.
            ClientError::McpServerTooOld { .. } => "SERVER_TOO_OLD",
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
            // The MCP gate's own vocabulary, carried through unflattened — the client and the web
            // tier refuse the same document with the same word.
            ClientError::McpRefused { code, .. } => code.as_str(),
            // A folder whose kind the door cannot read: the fix is the `--kind` word,
            // machine-runnable. The code names the WORD that is missing, not the flag spelling
            // that used to carry it.
            ClientError::KindRequired { .. } => "KIND_REQUIRED",
            // A retired manifest spelling: the fix is the row's `dest` rewrite in the message.
            ClientError::ManifestMigration(_) => "MANIFEST_FIELD_RETIRED",
            // A user-authored manifest the grammar refuses: the message names the file, the
            // entry, and the rule — the same word the reconcile's freeze warnings use.
            ClientError::ManifestInvalid(_) => "MANIFEST_INVALID",
            // A `-a` slug the registry does not know: the fix is a listed slug.
            ClientError::UnknownAgent { .. } => "UNKNOWN_AGENT",
            // A selection the arm refuses whole shares the argument-shaped code.
            ClientError::SelectionRefused(_) => "INVALID_ARGUMENT",
            // The shared-copy narrowing refusal: the ways out ride as next_actions.
            ClientError::SharedCopyOnly { .. } => "SHARED_COPY_ONLY",
        }
    }

    /// The terminal outcome the agent branches on.
    pub(crate) fn outcome(&self) -> TerminalOutcome {
        match self {
            // Every name-ambiguity shape (across tracked skills, and the one chooser every add-side
            // ambiguity collapses into) is the same terminal class — the agent picks and retries.
            ClientError::AmbiguousName { .. }
            | ClientError::AmbiguousSource { .. }
            // A repo holding several skills (or several dirs of one name) is the same "disambiguate and
            // retry" class — as is a workspace-resource name matching across workspaces or kinds.
            | ClientError::AmbiguousSkillInRepo { .. }
            | ClientError::DuplicateSkillName { .. }
            | ClientError::AmbiguousTarget { .. } => TerminalOutcome::AmbiguousName,
            // A network fetch fault is transient — the agent may retry the same import. Only the
            // forge's answer ABOUT the source (gone / invisible) is permanent.
            ClientError::RemoteFetch { fault, .. } => {
                if fault.permanent() {
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
            ClientError::Denied(_) | ClientError::MailNotConfigured => TerminalOutcome::Denied,
            ClientError::PublishBlocked { .. } => TerminalOutcome::Diverged,
            // Behind is the local half of the stale-base CONFLICT: update to rebase, then retry —
            // never a blind re-run of the same publish.
            ClientError::PublishBehind { .. } => TerminalOutcome::Conflict,
            // A wrong command for the state: re-running it changes nothing, so it is the
            // permanent-as-issued class every other usage refusal takes.
            ClientError::NoStoppedMerge { .. } => TerminalOutcome::PermanentFailure,
            // An unreadable workbench folder is the same class: a local reconciliation the person
            // performs, never a blind retry of the same command against the same folder.
            ClientError::ConflictCopyUnreadable { .. } => TerminalOutcome::Diverged,
            // Divergent per-placement edits are the same class as a diverged draft: local
            // reconciliation (or the disclosed reset) resolves it, never a blind retry.
            ClientError::PlacementsDiverged { .. } => TerminalOutcome::Diverged,
            // An in-flight op must be settled, then the command retried.
            ClientError::PendingOp { .. } => TerminalOutcome::RetryableFailure,
            // A definitive 4xx rejection — the op cannot succeed as-is.
            ClientError::PlaneRejected(_) => TerminalOutcome::PermanentFailure,
            // Neither side of the version floor heals on a retry: the same binary meets the same
            // server and gets the same answer. The way out is a different binary, never a loop —
            // and for the MCP corner, a newer SERVER.
            ClientError::UpdateRequired { .. }
            | ClientError::ServerTooOld { .. }
            | ClientError::McpServerTooOld { .. } => TerminalOutcome::PermanentFailure,
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
        // The kernel's own words, not its variant names: the whole `Scan` Display reaches the
        // terminal, and `CaseFoldCollision` is not a sentence.
        ClientError::Scan(r.to_string())
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
