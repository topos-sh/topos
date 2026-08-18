//! WHAT A BUNDLE IS — the closed kind vocabulary, the ONE parse of a kind word, the durable
//! per-record marker, and the ONE chain every path asks before it decides between skill-dir
//! placement and the MCP config converge.
//!
//! Kind is an OPEN string on the wire and a CLOSED set in this binary: [`BundleKind`] holds exactly
//! the kinds this build can deliver, and [`BundleKind::parse`] is the only place a word becomes one
//! of them. A word no variant owns is a bundle from a newer server — never guessed into the nearest
//! kind, always refused where it arrives (the sweep skips the row; `add` refuses it) with
//! `topos self-update` as the way out.
//!
//! The chain is two rungs and fails CLOSED. The durable marker answers first — written at the first
//! sync of every bundle, never rewritten, never swept away — then the manifest local-path row that
//! demands the record's placements (an adopted folder has no catalog to ask, so its row is the only
//! record of its kind). When neither answers the result is [`RecordKind::Indeterminate`], and over
//! an EMPTY placement map that is a REFUSAL, not a default: a store-only record whose kind evidence
//! was lost must never have its `server.json` materialized into skill dirs on a guess.

use std::path::PathBuf;

use crate::ctx::Ctx;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// The bundle kinds THIS BINARY delivers. CLOSED: a kind joins the list in the release that ships
/// its delivery mechanics, so the enum IS the inventory of what this build knows how to place —
/// and a third kind becomes a compile error at every branch rather than a silent mis-placement.
///
/// It is also `add --kind`'s value enum, so the word the CLI accepts and the word this build can
/// deliver are the SAME list: a third kind extends the vocabulary, the flag, and its help in one
/// place, and can never be spelled at a door that cannot place it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub(crate) enum BundleKind {
    /// A skill bundle: its files are placed into each agent's skills folder.
    ///
    /// The `value(help)` is what `--kind`'s own help prints, and these doc comments are internal —
    /// clap would otherwise put "skill-dir placement must never engage" in front of a person.
    #[default]
    #[value(help = "a folder of instructions your agents read (the default)")]
    Skill,
    /// An MCP server: a catalog entry the workspace shares, converged into each agent's own MCP
    /// config. Skill-dir placement must never engage for it.
    #[value(
        help = "a server your workspace shares — name its registry entry (or link its \
                    server.json) and your agents get it as a tool endpoint"
    )]
    Mcp,
}

impl BundleKind {
    /// The word this kind is spelled with everywhere it is recorded — the wire, the catalog, a
    /// manifest row's `kind` field, the durable marker.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Mcp => "mcp",
        }
    }

    /// The kind as a REFUSAL reads it aloud — the indefinite noun phrase, so a sentence about two
    /// kinds ("is an MCP server, not a skill") reads as English rather than as two enum words.
    pub(crate) const fn noun_phrase(self) -> &'static str {
        match self {
            Self::Skill => "a skill",
            Self::Mcp => "an MCP server",
        }
    }

    /// Every kind this build delivers, in the order refusals list them — the ONE source for
    /// "known kinds: …" wherever a door has to teach the vocabulary.
    pub(crate) const ALL: [Self; 2] = [Self::Skill, Self::Mcp];

    /// The known kinds spelled for a refusal, in this repo's backtick idiom.
    pub(crate) fn known_list() -> String {
        Self::ALL
            .iter()
            .map(|k| format!("`{}`", k.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The kind a WORD names, or `None` when this build owns no such kind. THE one place a kind
    /// string becomes a kind: every wire field, manifest row, and marker enters through here, so
    /// there is exactly one answer to "is this deliverable" and exactly one place to extend.
    pub(crate) fn parse(word: &str) -> Option<Self> {
        match word {
            "skill" => Some(Self::Skill),
            "mcp" => Some(Self::Mcp),
            _ => None,
        }
    }

    /// The kind a RECORDED TAG names — the delivery cache's `kind`, a manifest row's `kind`, a
    /// publish request's. An ABSENT tag is the default skill (that is what absence means
    /// everywhere it is written); a present word this build does not own is `None`.
    pub(crate) fn of_tag(tag: Option<&str>) -> Option<Self> {
        match tag {
            None => Some(Self::Skill),
            Some(word) => Self::parse(word),
        }
    }

    /// Whether this bundle is CONFIG-PLACED (its bytes reach agents through the MCP converge,
    /// never through a skills folder).
    pub(crate) const fn is_mcp(self) -> bool {
        matches!(self, Self::Mcp)
    }

    /// The `kind` tag a record carries on the wire and in the delivery cache: spelled only when it
    /// is not the default, because an absent tag reads as `"skill"` by the request's own default.
    pub(crate) fn tag(self) -> Option<String> {
        match self {
            Self::Skill => None,
            Self::Mcp => Some(Self::Mcp.as_str().to_owned()),
        }
    }
}

/// What the durable chain could establish about a tracked record — [`classify`]'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordKind {
    /// Established: the record IS this kind, on evidence written when the kind was known.
    Known(BundleKind),
    /// NOTHING answered — no marker, no manifest row, or a word this build does not own. Over an
    /// EMPTY placement map the caller must FAIL CLOSED (refuse with a typed message); over a map
    /// that already holds placements the record is provably dir-placed and reads as a skill.
    Indeterminate,
}

impl RecordKind {
    /// A rung's WORD, parsed. A word this build does not own stops the chain HERE rather than
    /// falling through to a staler rung: the marker was written by a newer binary that knew more
    /// than this one, and an older row must not overrule it.
    fn of_word(word: &str) -> Self {
        BundleKind::parse(word).map_or(Self::Indeterminate, Self::Known)
    }

    /// Whether the record is provably CONFIG-PLACED. `Indeterminate` answers `false`; the paths
    /// that may not guess test [`RecordKind::Indeterminate`] first and refuse.
    pub(crate) const fn is_mcp(self) -> bool {
        matches!(self, Self::Known(BundleKind::Mcp))
    }

    /// The kind to act on, reading `Indeterminate` as the ordinary skill — for the paths where a
    /// wrong guess costs nothing (which dest vocabulary a selector resolves in, how a receipt line
    /// is worded) rather than bytes landing in a folder.
    pub(crate) const fn or_skill(self) -> BundleKind {
        match self {
            Self::Known(k) => k,
            Self::Indeterminate => BundleKind::Skill,
        }
    }
}

// =================================================================================================
// What a kind CANNOT serve — the one refusal construction
// =================================================================================================

/// A verb that acts on a bundle's PLACED FILES: it reads, compares, or discards the bytes sitting
/// in a folder an agent reads. That is the ONE axis a kind can fail to serve — a bundle whose bytes
/// reach agents as config ENTRIES has no such folder — so all of them refuse through one
/// construction rather than three ad-hoc sentences a person has to reconcile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileVerb {
    /// `topos diff` — the placed files against the version they came from.
    Diff,
    /// `topos update --reset` — discard local edits to the placed files.
    Reset,
    /// `topos update --keep-mine` — end a stopped merge over the placed files.
    KeepMine,
    /// `topos publish` — ship the placed files to the workspace.
    Publish,
}

impl FileVerb {
    /// The clause naming what this verb does, spelled as the flag the person typed. Reads directly
    /// into the refusal sentence, so the message says the verb's own job back to them.
    const fn acts_on(self) -> &'static str {
        match self {
            Self::Diff => "`diff` compares",
            Self::Reset => "`--reset` discards your edits to",
            Self::KeepMine => "`--keep-mine` ends a stopped merge over",
            Self::Publish => "`publish` ships",
        }
    }
}

/// The refusal `verb` earns over a bundle of `kind` — `None` when the kind serves it. THE one
/// construction site: a verb asks here instead of inventing a sentence, so every file verb says the
/// same true thing about the same bundle, and a third kind answers all of them in one edit.
///
/// The alternative each of these used to take — a fake empty diff, a reset that discarded nothing
/// and reported a discard — is worse than a refusal by exactly the amount a person trusts it.
pub(crate) fn refuse_file_verb(
    verb: FileVerb,
    name: &str,
    kind: BundleKind,
) -> Option<crate::error::ClientError> {
    match kind {
        // A skill IS its placed files — every file verb is precisely what it exists for.
        BundleKind::Skill => None,
        BundleKind::Mcp => Some(crate::error::ClientError::InvalidArgument(format!(
            "'{name}' is an MCP server bundle — {} a skill's placed files, and a server bundle \
             places none. What it puts on this machine is one entry per agent MCP config, and an \
             entry you edited by hand is left exactly as you left it, never overwritten. \
             `topos list {name}` names the files carrying it",
            verb.acts_on()
        ))),
    }
}

/// A verb that acts on a bundle's VERSION HISTORY — the versions this machine has held, the one it
/// would put back, the proposal it would decide. That is the SECOND axis a kind can fail to serve:
/// a connected server holds no history here at all. What it holds is one catalog revision, and
/// every version behind that one is the catalog's, published and withdrawn by whoever publishes
/// the server. A machine that never received them has nothing to show, nothing to go back to, and
/// nothing to review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VersionVerb {
    /// `topos log` — the versions this machine has held.
    Log,
    /// `topos update <name>@<version>` — put an older version's bytes back here.
    GoBack,
    /// `topos revert` — publish an older version forward as the new current.
    Revert,
    /// `topos review` — decide a proposal.
    Review,
}

impl VersionVerb {
    /// The clause naming what this verb does, spelled as the person typed it.
    const fn acts_on(self) -> &'static str {
        match self {
            Self::Log => "`log` lists the versions of",
            Self::GoBack => "`update <name>@<version>` puts an earlier version of",
            Self::Revert => "`revert` publishes an earlier version of",
            Self::Review => "`review` decides a proposed version of",
        }
    }
}

/// The refusal `verb` earns over a bundle of `kind` — `None` when the kind serves it. The twin of
/// [`refuse_file_verb`], for the other axis, and constructed in one place for the same reason:
/// every version verb says the same true thing about the same bundle.
pub(crate) fn refuse_version_verb(
    verb: VersionVerb,
    name: &str,
    kind: BundleKind,
) -> Option<crate::error::ClientError> {
    match kind {
        BundleKind::Skill => None,
        BundleKind::Mcp => Some(crate::error::ClientError::InvalidArgument(format!(
            "'{name}' is an MCP server bundle — {} a bundle's own versions, and a server's \
             versions are the catalog's: this machine holds the one it was given and nothing \
             before it. `topos list {name}` shows what it holds",
            verb.acts_on()
        ))),
    }
}

// =================================================================================================
// The durable marker
// =================================================================================================

/// The tiny per-record `kind.json` marker (`skills/<id>/kind.json`, beside `map.json`): the ONE
/// durable record of what a bundle IS. Written at the first sync/adopt of EVERY bundle in each
/// scope store, never rewritten, never deleted by any sweep — so no later loss of a delivery cache
/// row or a config-custody document can make a config-placed record classify as a skill and get its
/// `server.json` materialized into skill dirs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct KindDoc {
    pub schema_version: u32,
    pub kind: String,
}

/// The marker's word for `sid` in `layout`'s store, when one is recorded. Best-effort read: an
/// unreadable marker answers `None` (classification falls to its other rung, and the empty-map arm
/// fails CLOSED).
pub(crate) fn kind_marker(
    fs: &dyn FsOps,
    layout: &Layout,
    sid: &crate::id::SkillId,
) -> Option<String> {
    crate::doc::read_doc::<KindDoc>(fs, &layout.published(sid).kind)
        .ok()
        .flatten()
        .map(|d| d.kind)
}

/// Record what `sid` IS in this scope's store — idempotent (first write wins, so a re-sync can
/// never re-label a record), best-effort (the marker is a belt: classification fails closed
/// without it, so a failed write degrades to a refusal, never to a wrong placement).
pub(crate) fn write_kind_marker(ctx: &Ctx<'_>, sid: &crate::id::SkillId, kind: BundleKind) {
    let path = ctx.layout.published(sid).kind;
    if matches!(crate::doc::read_doc::<KindDoc>(ctx.fs, &path), Ok(Some(_))) {
        return;
    }
    let _ = crate::doc::write_doc(
        ctx.fs,
        &path,
        &KindDoc {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            kind: kind.as_str().to_owned(),
        },
    );
}

// =================================================================================================
// THE classifier
// =================================================================================================

/// Classify a tracked record in THIS scope: the durable marker in `ctx`'s store, else the manifest
/// local-path row that demands one of `placements`. Each rung answers only when it genuinely
/// knows; nothing answering is [`RecordKind::Indeterminate`], which the caller reads per the module
/// contract — a refusal over an EMPTY placement set, an ordinary skill over one that already holds
/// dirs.
pub(crate) fn classify(ctx: &Ctx<'_>, skill_id: &str, placements: &[String]) -> RecordKind {
    if let Ok(sid) = crate::id::SkillId::parse(skill_id)
        && let Some(word) = kind_marker(ctx.fs, &ctx.layout, &sid)
    {
        return RecordKind::of_word(&word);
    }
    if !placements.is_empty() {
        let dirs: Vec<PathBuf> = placements.iter().map(PathBuf::from).collect();
        if let Ok(Some(word)) = crate::ops::path_row_kind(ctx, &dirs) {
            return RecordKind::of_word(&word);
        }
    }
    RecordKind::Indeterminate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vocabulary_is_closed_and_round_trips() {
        assert_eq!(BundleKind::parse("skill"), Some(BundleKind::Skill));
        assert_eq!(BundleKind::parse("mcp"), Some(BundleKind::Mcp));
        for kind in [BundleKind::Skill, BundleKind::Mcp] {
            assert_eq!(BundleKind::parse(kind.as_str()), Some(kind));
        }
        // Words a NEWER server may serve, and near-misses: none of them become a kind.
        for word in ["knowledge", "MCP", "Skill", "skills", "", "mcp-server"] {
            assert_eq!(BundleKind::parse(word), None, "{word} must not parse");
        }
    }

    #[test]
    fn an_unknown_word_stops_the_chain_rather_than_becoming_a_skill() {
        assert_eq!(RecordKind::of_word("knowledge"), RecordKind::Indeterminate);
        assert_eq!(
            RecordKind::of_word("mcp"),
            RecordKind::Known(BundleKind::Mcp)
        );
        assert!(!RecordKind::of_word("knowledge").is_mcp());
    }

    #[test]
    fn only_a_non_default_kind_is_tagged() {
        assert_eq!(BundleKind::Skill.tag(), None);
        assert_eq!(BundleKind::Mcp.tag(), Some("mcp".to_owned()));
    }
}
