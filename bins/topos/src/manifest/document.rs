//! The `topos.toml` document — the v2 sectioned grammar.
//!
//! A manifest is a short, readable statement of intent. Top level: `schema = 1`, an optional
//! `workspace = "<host>/<workspace>"` line (project files), and the kind sections:
//!
//! ```toml
//! schema = 1
//! workspace = "topos.sh/acme"
//!
//! [skills]
//! code-review     = "latest"                        # follows the team's current version
//! release-process = "3f9c…(64 hex)"                 # pinned
//! find-skills     = "github:vercel-labs/skills"     # a repo skill; the key is the skill name
//! sql-style       = "github:acme/tools#9d0e8c17"    # pinned to a commit
//! local-notes     = "./tools/local-notes"           # a folder in this repo
//!
//! [mcp]
//! linear = "latest"
//!
//! [channels]
//! backend = "latest"
//! ```
//!
//! THE KEY IS THE NAME. In a PROJECT file every workspace-bundle and channel key is the BARE
//! name, resolved against the file's one `workspace =` line — a project uses one workspace, the
//! way a checkout has one origin. In the MACHINE file (`~/.topos/topos.toml`) there is no
//! workspace line: workspace keys are spelled in full (`"topos.sh/acme/code-review"`), and one
//! extra section exists — `[workspaces]`, the feed rows `topos login` writes
//! (`"<host>/<ws>" = "latest"`: everything that workspace currently gives you).
//!
//! VALUES are the same everywhere: `"latest"` (follow), a version pin (64-hex digest for
//! workspace bundles; `github:owner/repo#<commit>` pins a repo skill), `"off"` (machine file
//! only — the personal switch), a path (`"./tools/x"` — a local folder), or an inline table of
//! fields (`version`, `path`, `source`, `dest`, `mcp_dest`, `name`, `subdir`, `kind`).
//!
//! FORWARD TOLERANCE: an unknown top-level section is SKIPPED with one warning naming it (a
//! newer topos wrote it; updating gets it) — never a refusal, so a future kind cannot brick an
//! older CLI. Garbage INSIDE a known section still refuses with the way back. A `schema` newer
//! than this build refuses toward `topos self-update`.
//!
//! The `[mcp]` and `[skills]` sections carry the same row shapes; the section names the KIND the
//! row's bundle is delivered as, and each parsed row remembers its section so the editor and the
//! normal form spell it back where it came from.
//!
//! [`ManifestEditor`] edits format-preserving over `toml_edit` with the INVERSE property: adding
//! a row and removing it restores the input byte-for-byte, and vice versa. Deterministic
//! reorganization (sorting, section order) is the `fmt` normal form's job, never the editor's.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use toml_edit::{Array, Decor, DocumentMut, InlineTable, Item, Table, Value};

use crate::bundle_kind::BundleKind;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::manifest::keys::{KeyShape, classify_key};

/// The manifest format this build reads and writes.
pub(crate) const SCHEMA: i64 = 1;

/// Which manifest a text IS: the machine-personal global file (`~/.topos/topos.toml`) or a
/// project's committed `<project>/topos.toml`. Feed rows (`[workspaces]`) and `"off"` are
/// machine-only; bare workspace keys are project-only (they need the `workspace =` line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManifestScope {
    Global,
    Project,
}

/// Which section a row is spelled in — remembered from the parse so edits and the normal form
/// put a row back where it belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SectionKind {
    Skills,
    Mcp,
    Channels,
    Workspaces,
}

impl SectionKind {
    pub(crate) fn header(self) -> &'static str {
        match self {
            SectionKind::Skills => "skills",
            SectionKind::Mcp => "mcp",
            SectionKind::Channels => "channels",
            SectionKind::Workspaces => "workspaces",
        }
    }

    fn of(header: &str) -> Option<Self> {
        match header {
            "skills" => Some(SectionKind::Skills),
            "mcp" => Some(SectionKind::Mcp),
            "channels" => Some(SectionKind::Channels),
            "workspaces" => Some(SectionKind::Workspaces),
            _ => None,
        }
    }

    /// The section a row of this shape + kind belongs in.
    pub(crate) fn for_row(shape: &KeyShape, kind: BundleKind) -> Self {
        match shape {
            KeyShape::Feed { .. } => SectionKind::Workspaces,
            KeyShape::Channel { .. } => SectionKind::Channels,
            _ => match kind {
                BundleKind::Skill => SectionKind::Skills,
                BundleKind::Mcp => SectionKind::Mcp,
            },
        }
    }
}

/// The inline-table form of an entry value — every field optional, legality decided per shape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EntryFields {
    pub version: Option<String>,
    /// The row's destinations — dialect-legal directory strings (config FILES for an MCP row).
    /// `Some` freezes placement to exactly these; absent means every agent, now and later.
    pub dest: Option<Vec<String>>,
    /// A CHANNEL row's MCP-member destinations — the config files its `kind = "mcp"` members are
    /// narrowed to. Legal on a channel alone, because it is the only row whose members can be of
    /// both kinds; absent means its mcp members reach every MCP-capable agent.
    pub mcp_dest: Option<Vec<String>>,
    pub name: Option<String>,
    pub subdir: Option<String>,
    pub kind: Option<String>,
}

/// One entry's parsed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EntryValue {
    /// `"latest"` — track what the reference currently serves.
    Star,
    /// `"off"` — the per-bundle switch (workspace bundles, machine file only).
    Off,
    /// A version pin: a 64-hex digest (workspace bundle) or a 7–40-hex commit (repo skill).
    Pin(String),
    /// An inline table of fields.
    Fields(EntryFields),
}

impl EntryValue {
    /// The value as FIELDS — a plain version string reads as `version` alone, and the two
    /// value-only spellings as nothing at all. The one place a value is widened to the table form
    /// every field-reading caller wants.
    pub(crate) fn fields(&self) -> EntryFields {
        match self {
            EntryValue::Fields(f) => f.clone(),
            EntryValue::Pin(p) => EntryFields {
                version: Some(p.clone()),
                ..EntryFields::default()
            },
            EntryValue::Star | EntryValue::Off => EntryFields::default(),
        }
    }

    /// The bundle kind this value DECLARES, against the closed vocabulary: an absent `kind` field
    /// (and every non-table spelling) is the default skill; `None` means the row names a kind this
    /// build does not own, which is not a skill either.
    pub(crate) fn declared_kind(&self) -> Option<BundleKind> {
        match self {
            EntryValue::Fields(f) => BundleKind::of_tag(f.kind.as_deref()),
            _ => Some(BundleKind::Skill),
        }
    }
}

/// What the editor writes for a row — the same shapes the parser reads back, rendered
/// deterministically (canonical field order, single-line inline tables).
pub(crate) type EntryValueSpelling = EntryValue;

/// One parsed row: the canonical joined reference, its shape, its validated value, and the
/// section it is spelled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BundleRow {
    pub reference: String,
    pub shape: KeyShape,
    pub value: EntryValue,
    pub section: SectionKind,
}

/// A parsed manifest: rows in file order, the workspace line (when one stands), and the
/// forward-tolerance warnings (unknown sections skipped).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ManifestDoc {
    pub rows: Vec<BundleRow>,
    /// The `workspace = "<host>/<ws>"` line, split — `None` in the machine file, and in a
    /// project file holding only repo/path rows.
    pub workspace: Option<(String, String)>,
    /// One line per unknown top-level section skipped — surfaced, never fatal.
    pub warnings: Vec<String>,
}

/// A typed manifest refusal — names the specific fault AND the specific fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestError {
    pub message: String,
    /// The offending joined reference / key, when one exists.
    pub key: Option<String>,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

fn plain(message: impl Into<String>) -> ManifestError {
    ManifestError {
        message: message.into(),
        key: None,
    }
}

fn at(key: &str, message: impl Into<String>) -> ManifestError {
    ManifestError {
        message: message.into(),
        key: Some(key.to_string()),
    }
}

/// The complete field vocabulary — what a key must be to count as a mis-shelved FIELD rather
/// than an unknown word. `path` and `source` are spelling (they pick the shape), the rest ride
/// [`EntryFields`].
const FIELD_NAMES: [&str; 8] = [
    "version", "path", "source", "dest", "mcp_dest", "name", "subdir", "kind",
];

/// The fields legal on each shape (`path`/`source` excluded — they ARE the shape).
pub(crate) fn legal_fields(shape: &KeyShape) -> &'static [&'static str] {
    match shape {
        KeyShape::WorkspaceBundle { .. } => &["version", "dest", "name"],
        KeyShape::RepoSkill { .. } => &["version", "dest", "name", "subdir", "kind"],
        KeyShape::LocalPath { .. } => &["dest", "name", "kind"],
        KeyShape::RepoSet { .. } => &[],
        KeyShape::Channel { .. } => &["dest", "mcp_dest"],
        KeyShape::Feed { .. } => &[],
    }
}

fn legal_list(legal: &[&str]) -> String {
    legal
        .iter()
        .map(|f| format!("`{f}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------
// Validation shared by the parser and the editor
// ---------------------------------------------------------------------------

fn feed_exact_latest(reference: &str) -> ManifestError {
    at(
        reference,
        format!(
            "a workspace row takes exactly `\"latest\"` — `{reference}` means whatever the \
             workspace currently gives you; pin individual bundles on their own rows",
        ),
    )
}

fn feed_in_project(reference: &str) -> ManifestError {
    at(
        reference,
        "`[workspaces]` is personal by nature and lives in the machine file \
         (`~/.topos/topos.toml`) only — a project file is a repo fact, identical for every \
         contributor; name the bundles or channels this project uses on their own rows",
    )
}

fn off_check(reference: &str, shape: &KeyShape, scope: ManifestScope) -> Result<(), ManifestError> {
    match (shape, scope) {
        (KeyShape::WorkspaceBundle { .. }, ManifestScope::Global) => Ok(()),
        (KeyShape::WorkspaceBundle { .. }, ManifestScope::Project) => Err(at(
            reference,
            "`\"off\"` is the personal switch and lives in the machine file \
             (`~/.topos/topos.toml`) only — a project file is a repo fact, identical for \
             every contributor; to keep a bundle out of this project, remove the row that \
             brings it",
        )),
        _ => Err(at(
            reference,
            format!(
                "`\"off\"` switches a workspace bundle off — a {} row is dropped by deleting \
                 its line",
                shape.noun()
            ),
        )),
    }
}

fn pin_check(reference: &str, shape: &KeyShape, s: &str) -> Result<(), ManifestError> {
    match shape {
        KeyShape::WorkspaceBundle { .. } => {
            if s.len() == 64 && is_hex(s) {
                Ok(())
            } else {
                Err(at(
                    reference,
                    format!(
                        "`{s}` does not pin `{reference}` — a workspace bundle pins the full \
                         64-character version digest; `\"latest\"` follows the team's current",
                    ),
                ))
            }
        }
        KeyShape::RepoSet { .. } | KeyShape::RepoSkill { .. } => {
            if (7..=40).contains(&s.len()) && is_hex(s) {
                Ok(())
            } else {
                Err(at(
                    reference,
                    format!(
                        "`{s}` does not pin `{reference}` — a git pin is a commit hash (7 to 40 \
                         hex characters); without `#<commit>` the row follows the default branch",
                    ),
                ))
            }
        }
        KeyShape::LocalPath { .. } => Err(at(
            reference,
            "a local folder has no versions to pin — its value is its path, or an inline table \
             of fields",
        )),
        KeyShape::Channel { .. } => Err(at(
            reference,
            "a channel takes no pin — its value is `\"latest\"` or an inline table (without \
             `version`)",
        )),
        KeyShape::Feed { .. } => Err(feed_exact_latest(reference)),
    }
}

/// DRY-RUN one row: classify the reference and validate the value against its shape + scope —
/// exactly what [`ManifestEditor::set_row`] would do, without a file, an editor, or a write. A
/// caller whose write is preceded by a durable side effect proves the row is writable FIRST, so
/// a refusal cannot land after the consent it was supposed to gate. (What this cannot know is a
/// project file's one-workspace rule — that needs the document, and [`ManifestEditor::set_row`]
/// enforces it.)
///
/// # Errors
/// The reference must classify, and the value must be legal for its shape and this scope.
pub(crate) fn check_row(
    reference: &str,
    scope: ManifestScope,
    value: &EntryValue,
) -> Result<(), ManifestError> {
    let shape = classify_key(reference).map_err(|e| at(reference, e.message))?;
    check_value(reference, &shape, scope, value)
}

/// Validate a TYPED value (the editor's input) against its shape + scope — the same rules the
/// parser enforces reading a file, so an editor can never write what `open` would refuse.
fn check_value(
    reference: &str,
    shape: &KeyShape,
    scope: ManifestScope,
    value: &EntryValue,
) -> Result<(), ManifestError> {
    if matches!(shape, KeyShape::Feed { .. }) {
        if scope == ManifestScope::Project {
            return Err(feed_in_project(reference));
        }
        return match value {
            EntryValue::Star => Ok(()),
            _ => Err(feed_exact_latest(reference)),
        };
    }
    if matches!(shape, KeyShape::RepoSet { .. }) {
        return Err(repo_set_unspellable(reference));
    }
    match value {
        EntryValue::Star => Ok(()),
        EntryValue::Off => off_check(reference, shape, scope),
        EntryValue::Pin(p) => pin_check(reference, shape, p),
        EntryValue::Fields(f) => fields_check(reference, shape, scope, f),
    }
}

/// The refusal a repo-set row earns — v2 spells individual skills, and `topos add <repo>` writes
/// one row per skill it discovers.
fn repo_set_unspellable(reference: &str) -> ManifestError {
    at(
        reference,
        format!(
            "`{reference}` names a whole repo — a manifest row names ONE skill: \
             `<name> = \"github:<owner>/<repo>\"` (the key is the skill's name inside the repo); \
             `topos add {reference}` writes a row per skill it finds",
        ),
    )
}

/// The field-legality half, on an already-typed [`EntryFields`] — including the `dest` rules the
/// scope decides.
fn fields_check(
    reference: &str,
    shape: &KeyShape,
    scope: ManifestScope,
    f: &EntryFields,
) -> Result<(), ManifestError> {
    let legal = legal_fields(shape);
    let present: [(&str, bool); 6] = [
        ("version", f.version.is_some()),
        ("dest", f.dest.is_some()),
        ("mcp_dest", f.mcp_dest.is_some()),
        ("name", f.name.is_some()),
        ("subdir", f.subdir.is_some()),
        ("kind", f.kind.is_some()),
    ];
    for (field, is_present) in present {
        if is_present && !legal.contains(&field) {
            return Err(illegal_field(reference, shape, field));
        }
    }
    // A repo row cannot carry an MCP bundle: an MCP server is delivered from a workspace catalog,
    // so the row refuses at LOAD rather than parsing into a demand nothing could ever converge.
    if matches!(shape, KeyShape::RepoSkill { .. })
        && f.kind.as_deref() == Some(BundleKind::Mcp.as_str())
    {
        return Err(mcp_needs_a_workspace(reference));
    }
    // The kind VOCABULARY is closed in a hand-written file exactly as it is at the server door.
    if let Some(word) = f.kind.as_deref()
        && BundleKind::parse(word).is_none()
    {
        return Err(unknown_kind(reference, word));
    }
    if let Some(v) = &f.version {
        if v == "off" {
            return Err(off_in_table(reference));
        }
        if v != "latest" {
            pin_check(reference, shape, v)?;
        }
    }
    if let Some(dest) = &f.dest {
        dest_list_check(reference, "dest", scope, dest)?;
        // A local MCP row's kind is knowable at load — its dest entries are config FILES and
        // must come from the descriptor table (an unknown file's format cannot be edited).
        if matches!(shape, KeyShape::LocalPath { .. })
            && f.kind.as_deref() == Some(BundleKind::Mcp.as_str())
        {
            for entry in super::dest::named_entries(dest) {
                if super::dest::mcp_slug_for_dest(&entry, scope).is_none() {
                    return Err(at(reference, super::dest::unknown_mcp_file(&entry, scope)));
                }
            }
        }
    }
    // A channel's `mcp_dest` takes the same path dialect and nothing more; which files it names
    // resolves at delivery.
    if let Some(mcp_dest) = &f.mcp_dest {
        dest_list_check(reference, "mcp_dest", scope, mcp_dest)?;
        if super::dest::carries_default_reach(mcp_dest) {
            return Err(at(
                reference,
                format!(
                    "`mcp_dest` entry `{}` is not a config file — `{}` is a bundle row's `dest` \
                     token for its default reach; drop `mcp_dest` to reach every MCP-capable agent",
                    super::dest::DEFAULT_REACH,
                    super::dest::DEFAULT_REACH
                ),
            ));
        }
    }
    Ok(())
}

/// One destination array's shape rules.
fn dest_list_check(
    reference: &str,
    field: &str,
    scope: ManifestScope,
    entries: &[String],
) -> Result<(), ManifestError> {
    if entries.is_empty() {
        let article = if field == "mcp_dest" { "an" } else { "a" };
        return Err(at(
            reference,
            format!(
                "{article} {field} names at least one destination; drop the field to reach every \
                 agent"
            ),
        ));
    }
    for entry in entries {
        dest_entry_check(reference, field, scope, entry)?;
    }
    Ok(())
}

/// The dialect check for ONE dest entry, reference-free — the `--dest` literal's validation.
///
/// # Errors
/// As the grammar's per-entry rule: machine files name machine paths, project files relative
/// contained ones.
pub(crate) fn check_dest_entry(entry: &str, scope: ManifestScope) -> Result<(), ManifestError> {
    dest_entry_check("dest", "dest", scope, entry)
}

fn dest_entry_check(
    reference: &str,
    field: &str,
    scope: ManifestScope,
    entry: &str,
) -> Result<(), ManifestError> {
    if entry.trim().is_empty() {
        return Err(at(
            reference,
            format!("`{field}` entries are non-empty directory strings"),
        ));
    }
    if entry == super::dest::DEFAULT_REACH {
        return Ok(());
    }
    match scope {
        ManifestScope::Global => {
            if !(entry.starts_with("~/") || Path::new(entry).is_absolute()) {
                return Err(at(
                    reference,
                    format!(
                        "{field} entry `{entry}` is relative — the machine-wide file names machine \
                         paths: `~/`-prefixed or absolute",
                    ),
                ));
            }
        }
        ManifestScope::Project => {
            if !super::dest::safe_project_rel(entry) || entry.starts_with('~') {
                return Err(at(
                    reference,
                    format!(
                        "{field} entry `{entry}` leaves the checkout — a project file names \
                         relative paths inside it (no `..`, no `~`, not absolute)",
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn illegal_field(reference: &str, shape: &KeyShape, field: &str) -> ManifestError {
    let legal = legal_fields(shape);
    if field == "version" && matches!(shape, KeyShape::Channel { .. }) {
        return at(reference, "a channel takes no pin — drop `version`");
    }
    at(
        reference,
        format!(
            "`{field}` does not fit a {} — `{reference}` takes {}",
            shape.noun(),
            legal_list(legal)
        ),
    )
}

/// The refusal a repo row tagged `kind = "mcp"` earns.
fn mcp_needs_a_workspace(reference: &str) -> ManifestError {
    at(
        reference,
        format!(
            "`kind = \"mcp\"` does not fit a repo skill — `{reference}` cannot deliver an MCP \
             server; publish the bundle to a workspace and name it in `[mcp]`"
        ),
    )
}

/// The refusal a row naming a kind topos does not deliver earns.
fn unknown_kind(reference: &str, word: &str) -> ManifestError {
    at(
        reference,
        format!(
            "`kind = \"{word}\"` on `{reference}` is not a kind topos delivers — known kinds: {}",
            BundleKind::known_list()
        ),
    )
}

fn off_in_table(reference: &str) -> ManifestError {
    at(
        reference,
        "`off` is a whole value (`<name> = \"off\"`) — never a field",
    )
}

// ---------------------------------------------------------------------------
// The workspace line + the source-value grammar
// ---------------------------------------------------------------------------

/// Parse a `workspace = "<host>/<workspace>"` line's value.
pub(crate) fn parse_workspace_line(s: &str) -> Result<(String, String), ManifestError> {
    match classify_key(s) {
        Ok(KeyShape::Feed { host, workspace }) => Ok((host, workspace)),
        _ => Err(plain(format!(
            "`workspace = \"{s}\"` is not a workspace address — the line is \
             `workspace = \"<host>/<workspace>\"` (e.g. `topos.sh/acme`)",
        ))),
    }
}

/// A parsed `github:`/`bitbucket:` source value: the forge host, owner, repo, and the optional
/// `#<commit>` pin.
struct SourceValue {
    host: String,
    owner: String,
    repo: String,
    pin: Option<String>,
}

/// Parse a source value: `github:<owner>/<repo>[#<commit>]` (`bitbucket:` rides the same
/// shape). The commit is 7–40 hex; a branch or tag is not a pin.
fn parse_source_value(key: &str, s: &str) -> Result<SourceValue, ManifestError> {
    let (host, rest) = if let Some(rest) = s.strip_prefix("github:") {
        ("github.com", rest)
    } else if let Some(rest) = s.strip_prefix("bitbucket:") {
        ("bitbucket.org", rest)
    } else {
        return Err(at(
            key,
            format!("`{s}` is not a source — a repo skill is `github:<owner>/<repo>[#<commit>]`"),
        ));
    };
    let (path, pin) = match rest.split_once('#') {
        Some((path, p)) => {
            if (7..=40).contains(&p.len()) && is_hex(p) {
                (path, Some(p.to_ascii_lowercase()))
            } else {
                return Err(at(
                    key,
                    format!(
                        "`#{p}` does not pin `{s}` — a git pin is a commit hash (7 to 40 hex \
                         characters); without `#<commit>` the row follows the default branch",
                    ),
                ));
            }
        }
        None => (rest, None),
    };
    match path.split('/').collect::<Vec<_>>().as_slice() {
        [owner, repo] if !owner.is_empty() && !repo.is_empty() => {
            let repo = repo.trim_end_matches(".git");
            if repo.is_empty() {
                return Err(at(
                    key,
                    format!(
                        "`{s}` is not a source — a repo skill is \
                         `github:<owner>/<repo>[#<commit>]`"
                    ),
                ));
            }
            Ok(SourceValue {
                host: host.to_string(),
                owner: (*owner).to_string(),
                repo: repo.to_string(),
                pin,
            })
        }
        _ => Err(at(
            key,
            format!("`{s}` is not a source — a repo skill is `github:<owner>/<repo>[#<commit>]`"),
        )),
    }
}

/// Spell a repo skill's source value back: `github:<owner>/<repo>`.
fn spell_source(host: &str, owner: &str, repo: &str) -> String {
    let scheme = if host == "bitbucket.org" {
        "bitbucket"
    } else {
        "github"
    };
    format!("{scheme}:{owner}/{repo}")
}

/// The one NAME charset for section keys (bare names): lowercase letters, digits, hyphens,
/// starting alphanumeric — the same rule the reference grammar uses.
fn is_bare_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn is_path_key(s: &str) -> bool {
    s.starts_with("./") || s.starts_with("../") || s.starts_with('/') || s.starts_with("~/")
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse + validate a manifest text. Rows come back in file order; every refusal names the
/// specific fault and the specific fix; unknown top-level sections skip into `warnings`.
pub(crate) fn parse_manifest(
    text: &str,
    scope: ManifestScope,
) -> Result<ManifestDoc, ManifestError> {
    let doc: DocumentMut = text
        .parse()
        .map_err(|e| plain(format!("not valid TOML: {e}")))?;
    parse_document(&doc, scope)
}

/// The parse over an already-built document (the editor and the formatter reuse it).
pub(crate) fn parse_document(
    doc: &DocumentMut,
    scope: ManifestScope,
) -> Result<ManifestDoc, ManifestError> {
    let mut out = ManifestDoc::default();

    // Top-level scalars first: `schema` (forward-refusing) and `workspace` (project files).
    if let Some(item) = doc.get("schema") {
        let v = item.as_integer().ok_or_else(|| {
            plain("`schema` is a number — this build writes and reads `schema = 1`")
        })?;
        if v > SCHEMA {
            return Err(plain(format!(
                "this file is `schema = {v}` — written by a newer topos than this one reads; \
                 run `topos self-update`",
            )));
        }
    }
    if let Some(item) = doc.get("workspace") {
        let s = item.as_str().ok_or_else(|| {
            plain("`workspace` is a string — `workspace = \"<host>/<workspace>\"`")
        })?;
        if scope == ManifestScope::Global {
            return Err(plain(
                "the machine-wide file has no `workspace` line — it can hold bundles from any \
                 workspace, so every workspace key is spelled in full \
                 (`\"<host>/<workspace>/<name>\"`)",
            ));
        }
        out.workspace = Some(parse_workspace_line(s)?);
    }

    let mut seen: HashSet<String> = HashSet::new();
    for (key, item) in doc.iter() {
        match key {
            "schema" | "workspace" => {}
            _ => match SectionKind::of(key) {
                Some(section) => {
                    let Item::Table(t) = item else {
                        return Err(plain(format!(
                            "`{key}` is a section — spell it `[{key}]` with one entry per line",
                        )));
                    };
                    let ws = out.workspace.clone();
                    parse_section(t, section, scope, ws.as_ref(), &mut seen, &mut out)?;
                }
                None => {
                    // Forward tolerance: a section this build does not know is skipped whole,
                    // with one line saying so — never a refusal.
                    out.warnings.push(format!(
                        "this file also uses [{key}] — this topos does not read it; \
                         `topos self-update` gets it",
                    ));
                }
            },
        }
    }
    Ok(out)
}

/// Parse one known section's rows.
fn parse_section(
    table: &Table,
    section: SectionKind,
    scope: ManifestScope,
    workspace: Option<&(String, String)>,
    seen: &mut HashSet<String>,
    out: &mut ManifestDoc,
) -> Result<(), ManifestError> {
    if section == SectionKind::Workspaces && scope == ManifestScope::Project {
        return Err(feed_in_project(
            table
                .iter()
                .next()
                .map(|(k, _)| k)
                .unwrap_or("<host>/<workspace>"),
        ));
    }
    for (key, item) in table.iter() {
        let v = match item {
            Item::None => continue,
            Item::Value(v) => v,
            Item::Table(_) | Item::ArrayOfTables(_) => {
                return Err(at(
                    key,
                    format!(
                        "`[{}.{key}]` is not an entry — an entry is one line: \
                         `<name> = <value>`; use an inline table for fields \
                         (`{key} = {{ … }}`)",
                        section.header()
                    ),
                ));
            }
        };
        let row = parse_row(key, v, section, scope, workspace)?;
        if !seen.insert(row.reference.clone()) {
            return Err(at(
                &row.reference,
                format!(
                    "`{}` is spelled twice — two keys resolve to the same reference; keep one \
                     line",
                    row.reference
                ),
            ));
        }
        out.rows.push(row);
    }
    Ok(())
}

/// Resolve one section key + value into a row: the key names the thing, the value says which
/// version (and, for paths and repo skills, where it comes from).
fn parse_row(
    key: &str,
    v: &Value,
    section: SectionKind,
    scope: ManifestScope,
    workspace: Option<&(String, String)>,
) -> Result<BundleRow, ManifestError> {
    if section == SectionKind::Workspaces {
        // Feed rows: full `<host>/<ws>` keys, exactly `"latest"`.
        let shape = match classify_key(key) {
            Ok(shape @ KeyShape::Feed { .. }) => shape,
            _ => {
                return Err(at(
                    key,
                    format!(
                        "`{key}` is not a workspace address — a `[workspaces]` row is \
                         `\"<host>/<workspace>\" = \"latest\"`",
                    ),
                ));
            }
        };
        let value = match v.as_str() {
            Some("latest") => EntryValue::Star,
            _ => return Err(feed_exact_latest(key)),
        };
        return Ok(BundleRow {
            reference: shape.canonical(),
            shape,
            value,
            section,
        });
    }

    let shape = resolve_key(key, v, section, scope, workspace)?;
    let value = value_of(key, &shape, scope, v)?;
    check_value(&shape.canonical(), &shape, scope, &value)?;
    Ok(BundleRow {
        reference: shape.canonical(),
        shape,
        value,
        section,
    })
}

/// Classify one [skills]/[mcp]/[channels] key against its value into a canonical shape.
fn resolve_key(
    key: &str,
    v: &Value,
    section: SectionKind,
    scope: ManifestScope,
    workspace: Option<&(String, String)>,
) -> Result<KeyShape, ManifestError> {
    // The VALUE can settle the shape regardless of the key: a path value is a local folder, a
    // `github:` value a repo skill (the key is the name either way).
    let str_value = v.as_str();
    if let Some(s) = str_value
        && is_path_key(s)
    {
        return Ok(KeyShape::LocalPath { raw: s.to_string() });
    }
    if let Some(s) = str_value
        && (s.starts_with("github:") || s.starts_with("bitbucket:"))
    {
        let src = parse_source_value(key, s)?;
        return Ok(KeyShape::RepoSkill {
            host: src.host,
            owner: src.owner,
            repo: src.repo,
            skill: key.to_string(),
        });
    }
    if let Some(t) = v.as_inline_table() {
        if let Some(p) = t.get("path").and_then(Value::as_str) {
            return Ok(KeyShape::LocalPath { raw: p.to_string() });
        }
        if let Some(s) = t.get("source").and_then(Value::as_str) {
            let src = parse_source_value(key, s)?;
            return Ok(KeyShape::RepoSkill {
                host: src.host,
                owner: src.owner,
                repo: src.repo,
                skill: key.to_string(),
            });
        }
    }

    // No source in the value: the key itself names a workspace thing.
    if is_bare_name(key) {
        let Some((host, ws)) = workspace else {
            return Err(at(
                key,
                match scope {
                    ManifestScope::Project => format!(
                        "`{key}` is a bare name and this file has no `workspace = ` line — add \
                         `workspace = \"<host>/<workspace>\"` at the top, or spell the row's \
                         source in its value",
                    ),
                    ManifestScope::Global => format!(
                        "`{key}` is a bare name — the machine-wide file spells workspace keys in \
                         full: `\"<host>/<workspace>/{key}\"`",
                    ),
                },
            ));
        };
        return Ok(match section {
            SectionKind::Channels => KeyShape::Channel {
                host: host.clone(),
                workspace: ws.clone(),
                channel: key.to_string(),
            },
            _ => KeyShape::WorkspaceBundle {
                host: host.clone(),
                workspace: ws.clone(),
                bundle: key.to_string(),
            },
        });
    }

    // A full reference key.
    match classify_key(key) {
        Ok(shape @ KeyShape::WorkspaceBundle { .. }) if section != SectionKind::Channels => {
            project_one_workspace_check(key, &shape, scope, workspace)?;
            Ok(shape)
        }
        // In [channels], a three-segment `<host>/<ws>/<name>` key names the channel.
        Ok(KeyShape::WorkspaceBundle {
            host,
            workspace: ws,
            bundle,
        }) => {
            let shape = KeyShape::Channel {
                host,
                workspace: ws,
                channel: bundle,
            };
            project_one_workspace_check(key, &shape, scope, workspace)?;
            Ok(shape)
        }
        Ok(shape @ KeyShape::Channel { .. }) => {
            if section != SectionKind::Channels {
                return Err(at(
                    key,
                    format!("`{key}` is a channel — its row lives in `[channels]`"),
                ));
            }
            project_one_workspace_check(key, &shape, scope, workspace)?;
            Ok(shape)
        }
        Ok(KeyShape::LocalPath { raw }) => Err(at(
            &raw,
            format!(
                "a path is a VALUE, not a key — spell the row `<name> = \"{raw}\"` (the key is \
                 the bundle's name)",
            ),
        )),
        Ok(KeyShape::RepoSet { .. } | KeyShape::RepoSkill { .. }) => Err(at(
            key,
            "a repo is a VALUE, not a key — spell the row \
             `<name> = \"github:<owner>/<repo>\"` (the key is the skill's name)",
        )),
        Ok(KeyShape::Feed { .. }) => Err(at(
            key,
            format!(
                "`{key}` names a whole workspace — its row lives in `[workspaces]` (machine \
                 file); a project names the bundles and channels it uses on their own rows",
            ),
        )),
        Err(e) => Err(at(key, e.message)),
    }
}

/// A project file uses ONE workspace: a full workspace reference must match the `workspace =`
/// line (spelled bare), and without the line the file holds no workspace rows at all.
fn project_one_workspace_check(
    key: &str,
    shape: &KeyShape,
    scope: ManifestScope,
    workspace: Option<&(String, String)>,
) -> Result<(), ManifestError> {
    if scope != ManifestScope::Project {
        return Ok(());
    }
    let leaf = shape.leaf_name();
    match workspace {
        Some((host, ws)) if shape.workspace_key().as_deref() == Some(&format!("{host}/{ws}")) => {
            Err(at(
                key,
                format!(
                    "`{key}` spells this project's own workspace — the key is the bare name: \
                     `{leaf}`",
                ),
            ))
        }
        Some((host, ws)) => Err(at(
            key,
            format!(
                "`{key}` names another workspace — this project uses `{host}/{ws}` (its \
                 `workspace = ` line), and a project uses one workspace the way a checkout has \
                 one origin",
            ),
        )),
        None => Err(at(
            key,
            format!(
                "this file has no `workspace = ` line — add \
                 `workspace = \"{}\"` at the top and spell the key bare: `{leaf}`",
                shape.workspace_key().unwrap_or_default()
            ),
        )),
    }
}

fn value_type_noun(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "string",
        Value::Integer(_) | Value::Float(_) => "number",
        Value::Boolean(_) => "boolean",
        Value::Datetime(_) => "date",
        Value::Array(_) => "array",
        Value::InlineTable(_) => "table",
    }
}

fn value_of(
    key: &str,
    shape: &KeyShape,
    scope: ManifestScope,
    v: &Value,
) -> Result<EntryValue, ManifestError> {
    match v {
        Value::String(s) => string_value(key, shape, scope, s.value()),
        Value::InlineTable(t) => {
            let fields = fields_of(key, shape, t)?;
            Ok(EntryValue::Fields(fields))
        }
        Value::Array(_) => Err(at(
            key,
            "an array is not an entry value — an entry is `<name> = \"<version>\"` or \
             `<name> = { <fields> }`",
        )),
        other => Err(at(
            key,
            format!(
                "a {} is not an entry value — an entry is `<name> = \"<version>\"` or \
                 `<name> = {{ <fields> }}`",
                value_type_noun(other)
            ),
        )),
    }
}

fn string_value(
    key: &str,
    shape: &KeyShape,
    scope: ManifestScope,
    s: &str,
) -> Result<EntryValue, ManifestError> {
    // A path value IS the local row's identity; a source value carries its own pin.
    if matches!(shape, KeyShape::LocalPath { .. }) && is_path_key(s) {
        return Ok(EntryValue::Star);
    }
    if matches!(shape, KeyShape::RepoSkill { .. })
        && (s.starts_with("github:") || s.starts_with("bitbucket:"))
    {
        let src = parse_source_value(key, s)?;
        return Ok(match src.pin {
            Some(p) => EntryValue::Pin(p),
            None => EntryValue::Star,
        });
    }
    match s {
        "latest" => Ok(EntryValue::Star),
        "off" => off_check(key, shape, scope).map(|()| EntryValue::Off),
        "*" => Err(at(
            key,
            "`\"*\"` is not a value — `\"latest\"` follows the team's current version",
        )),
        _ => pin_check(key, shape, s).map(|()| EntryValue::Pin(s.to_string())),
    }
}

/// Read an inline table's fields, refusing unknown keys and per-field type faults; `path` and
/// `source` were already consumed by shape resolution and are checked for fit only.
fn fields_of(key: &str, shape: &KeyShape, t: &InlineTable) -> Result<EntryFields, ManifestError> {
    let legal = legal_fields(shape);
    let mut f = EntryFields::default();
    for (k, v) in t.iter() {
        if k == "path" || k == "source" {
            let fits = (matches!(shape, KeyShape::LocalPath { .. }) && k == "path")
                || (matches!(shape, KeyShape::RepoSkill { .. }) && k == "source");
            if !fits {
                return Err(at(
                    key,
                    format!("`{k}` does not fit a {} row", shape.noun()),
                ));
            }
            continue;
        }
        if !legal.contains(&k) {
            return Err(if FIELD_NAMES.contains(&k) {
                illegal_field(key, shape, k)
            } else {
                at(
                    key,
                    format!(
                        "unknown field `{k}` — a {} takes {}",
                        shape.noun(),
                        legal_list(legal)
                    ),
                )
            });
        }
        match k {
            "version" => {
                let s = v.as_str().ok_or_else(|| at(key, "`version` is a string"))?;
                if s == "off" {
                    return Err(off_in_table(key));
                }
                f.version = Some(s.to_string());
            }
            "dest" => f.dest = Some(dest_of(key, "dest", v)?),
            "mcp_dest" => f.mcp_dest = Some(dest_of(key, "mcp_dest", v)?),
            "name" => {
                f.name = Some(
                    v.as_str()
                        .ok_or_else(|| at(key, "`name` is a string"))?
                        .to_string(),
                );
            }
            "subdir" => {
                f.subdir = Some(
                    v.as_str()
                        .ok_or_else(|| at(key, "`subdir` is a string"))?
                        .to_string(),
                );
            }
            "kind" => {
                f.kind = Some(
                    v.as_str()
                        .ok_or_else(|| at(key, "`kind` is a string"))?
                        .to_string(),
                );
            }
            _ => {}
        }
    }
    // `version = "latest"` is the default spelled out — normalize away.
    if f.version.as_deref() == Some("latest") {
        f.version = None;
    }
    Ok(f)
}

/// Parse a `dest`/`mcp_dest` value: an array of destination strings.
fn dest_of(reference: &str, field: &str, v: &Value) -> Result<Vec<String>, ManifestError> {
    let example = if field == "mcp_dest" {
        "~/.cursor/mcp.json"
    } else {
        "~/.claude/skills"
    };
    let type_err = || {
        at(
            reference,
            format!("`{field}` is an array of destination paths (e.g. `[\"{example}\"]`)"),
        )
    };
    let arr = v.as_array().ok_or_else(type_err)?;
    let mut out = Vec::new();
    for item in arr.iter() {
        out.push(item.as_str().ok_or_else(type_err)?.to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Spelling — how a canonical (reference, value) renders into section + key + value
// ---------------------------------------------------------------------------

/// One row's spelling in the file: which section, what key, what value item.
pub(crate) struct RowSpelling {
    pub section: SectionKind,
    pub key: String,
    pub item: Item,
}

/// Spell one canonical row for a file of `scope` whose workspace line is `workspace`. The
/// project one-workspace rule is enforced here (the editor's write door).
///
/// # Errors
/// A repo set has no v2 spelling; a project row must belong to the file's workspace.
pub(crate) fn spell_row(
    reference: &str,
    shape: &KeyShape,
    value: &EntryValue,
    kind: BundleKind,
    scope: ManifestScope,
    workspace: Option<&(String, String)>,
) -> Result<RowSpelling, ManifestError> {
    let section = SectionKind::for_row(shape, kind);
    let key = match shape {
        KeyShape::Feed { host, workspace } => format!("{host}/{workspace}"),
        KeyShape::Channel {
            host,
            workspace: ws,
            channel,
        } => match scope {
            ManifestScope::Project => {
                require_own_workspace(reference, host, ws, workspace)?;
                channel.clone()
            }
            ManifestScope::Global => format!("{host}/{ws}/{channel}"),
        },
        KeyShape::WorkspaceBundle {
            host,
            workspace: ws,
            bundle,
        } => match scope {
            ManifestScope::Project => {
                require_own_workspace(reference, host, ws, workspace)?;
                bundle.clone()
            }
            ManifestScope::Global => format!("{host}/{ws}/{bundle}"),
        },
        KeyShape::LocalPath { raw } => value.fields().name.clone().unwrap_or_else(|| {
            raw.trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(raw)
                .to_string()
        }),
        KeyShape::RepoSkill { skill, .. } => {
            value.fields().name.clone().unwrap_or_else(|| skill.clone())
        }
        KeyShape::RepoSet { .. } => return Err(repo_set_unspellable(reference)),
    };
    Ok(RowSpelling {
        section,
        key,
        item: spell_value(shape, value),
    })
}

fn require_own_workspace(
    reference: &str,
    host: &str,
    ws: &str,
    workspace: Option<&(String, String)>,
) -> Result<(), ManifestError> {
    match workspace {
        Some((h, w)) if h == host && w == ws => Ok(()),
        Some((h, w)) => Err(at(
            reference,
            format!(
                "`{reference}` names another workspace — this project uses `{h}/{w}` (its \
                 `workspace = ` line), and a project uses one workspace the way a checkout \
                 has one origin",
            ),
        )),
        None => Err(at(
            reference,
            format!(
                "this file has no `workspace = ` line — `topos init` writes it; a project \
                 file needs `workspace = \"{host}/{ws}\"` before it can hold this row",
            ),
        )),
    }
}

/// Render a value deterministically for its shape: `"latest"` / a pin / `"off"` / the path or
/// source spelling, and fields as a single-line inline table in canonical order (version, path,
/// source, dest, mcp_dest, name, subdir, kind).
pub(crate) fn spell_value(shape: &KeyShape, v: &EntryValue) -> Item {
    let source = match shape {
        KeyShape::RepoSkill {
            host, owner, repo, ..
        } => Some(spell_source(host, owner, repo)),
        _ => None,
    };
    let path = match shape {
        KeyShape::LocalPath { raw } => Some(raw.clone()),
        _ => None,
    };
    match v {
        EntryValue::Star => match (&source, &path) {
            (Some(s), _) => toml_edit::value(s.as_str()),
            (_, Some(p)) => toml_edit::value(p.as_str()),
            _ => toml_edit::value("latest"),
        },
        EntryValue::Off => toml_edit::value("off"),
        EntryValue::Pin(p) => match &source {
            Some(s) => toml_edit::value(format!("{s}#{p}")),
            None => toml_edit::value(p.as_str()),
        },
        EntryValue::Fields(f) => {
            let mut t = InlineTable::new();
            if let Some(v) = &f.version {
                t.insert("version", v.as_str().into());
            }
            if let Some(p) = &path {
                t.insert("path", p.as_str().into());
            }
            if let Some(s) = &source {
                t.insert("source", s.as_str().into());
            }
            if let Some(d) = &f.dest {
                let mut a = Array::new();
                for x in super::dest::normal_dest(d) {
                    a.push(x.as_str());
                }
                t.insert("dest", Value::Array(a));
            }
            if let Some(d) = &f.mcp_dest {
                let mut a = Array::new();
                for x in d {
                    a.push(x.as_str());
                }
                t.insert("mcp_dest", Value::Array(a));
            }
            if let Some(s) = &f.name {
                t.insert("name", s.as_str().into());
            }
            if let Some(s) = &f.subdir {
                t.insert("subdir", s.as_str().into());
            }
            if let Some(s) = &f.kind {
                t.insert("kind", s.as_str().into());
            }
            Item::Value(Value::InlineTable(t))
        }
    }
}

// ---------------------------------------------------------------------------
// The editor
// ---------------------------------------------------------------------------

/// The format-preserving editor with the INVERSE property: for a row that did not pre-exist,
/// `set_row` then `remove_row` restores the input byte-for-byte; for one that did (spelled the
/// way [`spell_row`] spells it), `remove_row` then `set_row` does too. `remove_row` prunes only
/// section headers this editor itself minted — a hand-authored one always survives.
pub(crate) struct ManifestEditor {
    doc: DocumentMut,
    scope: ManifestScope,
    workspace: Option<(String, String)>,
    /// Every section header present at open — `remove_row` prunes only tables THIS editor
    /// minted, so a hand-authored header always survives.
    preexisting: HashSet<String>,
    /// The EXACT text this editor was built from (`None` = a fresh document). [`Self::write`]'s
    /// compare-and-swap re-reads the file immediately before its rename and refuses on drift.
    opened_from: Option<String>,
}

impl ManifestEditor {
    /// Open a manifest text for editing; the WHOLE document is validated first, so the edit
    /// methods never meet a shape the parser would refuse.
    pub(crate) fn open(text: &str, scope: ManifestScope) -> Result<Self, ManifestError> {
        let doc: DocumentMut = text
            .parse()
            .map_err(|e| plain(format!("not valid TOML: {e}")))?;
        let parsed = parse_document(&doc, scope)?;
        let mut preexisting = HashSet::new();
        for (k, item) in doc.iter() {
            if item.is_table() {
                preexisting.insert(k.to_string());
            }
        }
        Ok(Self {
            doc,
            scope,
            workspace: parsed.workspace,
            preexisting,
            opened_from: Some(text.to_owned()),
        })
    }

    /// The document's `workspace = ` line, when one stands.
    pub(crate) fn workspace(&self) -> Option<&(String, String)> {
        self.workspace.as_ref()
    }

    /// Set the project file's `workspace = ` line. Refused when rows already resolve against a
    /// DIFFERENT line — the bare keys would silently change meaning.
    ///
    /// # Errors
    /// Machine files take no workspace line; a standing different line refuses.
    pub(crate) fn set_workspace(&mut self, host: &str, ws: &str) -> Result<(), ManifestError> {
        if self.scope == ManifestScope::Global {
            return Err(plain("the machine-wide file has no `workspace` line"));
        }
        if let Some((h, w)) = &self.workspace
            && (h != host || w != ws)
        {
            return Err(plain(format!(
                "this file already uses `workspace = \"{h}/{w}\"` — its bare names resolve \
                 against that line; a project uses one workspace",
            )));
        }
        self.doc
            .insert("workspace", toml_edit::value(format!("{host}/{ws}")));
        self.workspace = Some((host.to_string(), ws.to_string()));
        Ok(())
    }

    /// Upsert one row. An existing spelling is edited IN PLACE; a new row lands at the end of
    /// its section (the section is minted when absent). `kind` picks `[skills]` vs `[mcp]` for
    /// bundle rows; channels and feeds have their own sections regardless.
    ///
    /// # Errors
    /// The reference must classify and the value must be legal for its shape, this file's scope,
    /// and — in a project — the file's one workspace.
    pub(crate) fn set_row(
        &mut self,
        reference: &str,
        value: &EntryValueSpelling,
        kind: BundleKind,
    ) -> Result<(), ManifestError> {
        let shape = classify_key(reference).map_err(|e| at(reference, e.message))?;
        check_value(reference, &shape, self.scope, value)?;
        let spelling = spell_row(
            reference,
            &shape,
            value,
            kind,
            self.scope,
            self.workspace.as_ref(),
        )?;

        // Edited in place, wherever it currently is spelled (its section may disagree with the
        // asked kind — the standing spelling wins; `fmt` reorganizes).
        if let Some((section, key)) = self.locate(reference)
            && let Some(Item::Table(t)) = self.doc.get_mut(section.header())
            && let Some(item) = t.get_mut(&key)
        {
            *item = spelling.item;
            return Ok(());
        }

        let header = spelling.section.header();
        // A NEW row landing on a key another row already holds would silently overwrite it —
        // two folders can share a basename, but two rows cannot share a name.
        if let Some(Item::Table(t)) = self.doc.get(header)
            && t.get(&spelling.key).is_some_and(Item::is_value)
        {
            return Err(at(
                &spelling.key,
                format!(
                    "`{}` already names another row in `[{header}]` — give this one its own \
                     name: `<name> = {{ … name = \"…\" }}` (the key is the name)",
                    spelling.key
                ),
            ));
        }
        if self.doc.get(header).is_none() {
            let mut t = Table::new();
            t.set_implicit(false);
            self.doc.insert(header, Item::Table(t));
        }
        let table = self.doc[header].as_table_mut().expect("just ensured");
        table.insert(&spelling.key, spelling.item);
        Ok(())
    }

    /// Remove one row, wherever it is spelled. A preceding standalone comment SURVIVES (only
    /// the line's own trailing comment may go with it); a section header this editor did not
    /// mint survives too, even empty.
    pub(crate) fn remove_row(&mut self, reference: &str) -> bool {
        let Some((section, key)) = self.locate(reference) else {
            return false;
        };
        let header = section.header();
        let mut orphan: Option<String> = None;
        {
            let Some(Item::Table(table)) = self.doc.get_mut(header) else {
                return false;
            };
            let keys: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();
            let Some(idx) = keys.iter().position(|k| *k == key) else {
                return false;
            };
            let prefix = table
                .key(&key)
                .map(|k| decor_prefix(k.leaf_decor()))
                .unwrap_or_default();
            table.remove(&key);
            if prefix.contains('#') {
                match keys.get(idx + 1) {
                    Some(next) => {
                        if let Some(mut km) = table.key_mut(next) {
                            let existing = decor_prefix(km.leaf_decor());
                            km.leaf_decor_mut().set_prefix(prefix + &existing);
                        }
                    }
                    None => orphan = Some(prefix),
                }
            }
        }
        if let Some(comment) = orphan {
            append_trailing(&mut self.doc, &comment);
        }
        // Prune an empty section this editor minted; a preexisting header stays.
        let empty = self
            .doc
            .get(header)
            .and_then(Item::as_table)
            .is_some_and(Table::is_empty);
        if empty && !self.preexisting.contains(header) {
            let prefix = self
                .doc
                .get(header)
                .and_then(Item::as_table)
                .map(|t| decor_prefix(t.decor()))
                .unwrap_or_default();
            self.doc.remove(header);
            if !prefix.trim().is_empty() {
                append_trailing(&mut self.doc, &prefix);
            }
        }
        true
    }

    /// Look one row up by canonical reference, wherever it is spelled.
    pub(crate) fn row(&self, reference: &str) -> Option<BundleRow> {
        let (section, key) = self.locate(reference)?;
        let Item::Table(t) = self.doc.get(section.header())? else {
            return None;
        };
        let Item::Value(v) = t.get(&key)? else {
            return None;
        };
        parse_row(&key, v, section, self.scope, self.workspace.as_ref()).ok()
    }

    /// The serialized document (what [`Self::write`] persists).
    pub(crate) fn rendered(&self) -> String {
        self.doc.to_string()
    }

    /// Persist atomically through the one crash-safe write — as a COMPARE-AND-SWAP against the
    /// text this editor was opened from (absence, for a fresh document).
    ///
    /// # Errors
    /// [`ClientError::ManifestChanged`] on the compare mismatch; otherwise the underlying
    /// filesystem failure.
    pub(crate) fn write(&self, fs: &dyn FsOps, path: &Path) -> Result<(), ClientError> {
        let expected = self.opened_from.as_ref().map(|t| t.as_bytes());
        match crate::atomic::atomic_write_cas(fs, path, self.rendered().as_bytes(), expected)? {
            crate::atomic::CasOutcome::Written => Ok(()),
            crate::atomic::CasOutcome::Changed => Err(ClientError::ManifestChanged {
                path: path.display().to_string(),
            }),
        }
    }

    /// Find a canonical reference's current spelling: (section, key).
    fn locate(&self, reference: &str) -> Option<(SectionKind, String)> {
        for (header, item) in self.doc.iter() {
            let Some(section) = SectionKind::of(header) else {
                continue;
            };
            let Item::Table(t) = item else { continue };
            for (key, val) in t.iter() {
                let Item::Value(v) = val else { continue };
                if let Ok(row) = parse_row(key, v, section, self.scope, self.workspace.as_ref())
                    && row.reference == reference
                {
                    return Some((section, key.to_string()));
                }
            }
        }
        None
    }
}

fn decor_prefix(d: &Decor) -> String {
    d.prefix()
        .and_then(|r| r.as_str())
        .map(str::to_string)
        .unwrap_or_default()
}

fn append_trailing(doc: &mut DocumentMut, extra: &str) {
    let cur = doc.trailing().as_str().unwrap_or("").to_string();
    doc.set_trailing(cur + extra);
}

// ---------------------------------------------------------------------------
// File birth
// ---------------------------------------------------------------------------

/// The MACHINE file's birth content: the header states the contract, then one `[workspaces]`
/// row per `(host, workspace)` given. Ordinary birth passes NO rows — `topos login` is the only
/// automatic feed-row author (it writes a workspace's row on this machine's first connection; a
/// row someone deleted stays deleted). Parses clean as [`ManifestScope::Global`].
pub(crate) fn materialized_global(workspaces: &[(String, String)]) -> String {
    let mut out = String::from(
        "# topos.toml — the complete recipe for what lands on this machine's personal scope.\n\
         # Managed by the topos CLI; hand-edits are welcome. A [workspaces] row\n\
         # (\"<host>/<workspace>\" = \"latest\") tracks whatever that workspace currently\n\
         # gives you; `topos login` adds one the first time this machine connects, and never\n\
         # re-adds one you delete. Only the rows here deliver.\n\
         schema = 1\n",
    );
    if !workspaces.is_empty() {
        out.push_str("\n[workspaces]\n");
        for (host, workspace) in workspaces {
            out.push_str(&format!("\"{host}/{workspace}\" = \"latest\"\n"));
        }
    }
    out
}

/// A commented PROJECT template. With a workspace the line is written; without, the line is
/// shown commented. Parses clean as [`ManifestScope::Project`].
pub(crate) fn project_template(workspace: Option<(&str, &str)>) -> String {
    let ws_line = match workspace {
        Some((host, ws)) => format!("workspace = \"{host}/{ws}\"\n"),
        None => "# workspace = \"topos.sh/<workspace>\"   # the one workspace bare names use\n"
            .to_string(),
    };
    format!(
        "# topos.toml — what this repo's agents use, identical for every contributor.\n\
         # Managed by the topos CLI; hand-edits are welcome. `topos install` places exactly\n\
         # what topos.lock records; `topos update` moves it and rewrites the lock.\n\
         schema = 1\n\
         {ws_line}\
         #\n\
         # [skills]\n\
         # code-review = \"latest\"                      # a team skill, following\n\
         # find-skills = \"github:vercel-labs/skills\"   # a public repo skill\n\
         #\n\
         # [channels]\n\
         # backend = \"latest\"\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> String {
        "0123456789abcdef".repeat(4)
    }

    fn parse_project(text: &str) -> ManifestDoc {
        parse_manifest(text, ManifestScope::Project).unwrap()
    }

    fn parse_global(text: &str) -> ManifestDoc {
        parse_manifest(text, ManifestScope::Global).unwrap()
    }

    /// The normative PROJECT reference file.
    fn project_reference_file() -> String {
        r#"# What this repo's agents use.
schema = 1
workspace = "topos.sh/acme"

[skills]
code-review = "latest"
release-process = "DIGEST"
find-skills = "github:vercel-labs/skills"
sql-style = "github:acme-corp/agent-skills#9d0e8c17"
local-notes = "./tools/local-notes"
deploy-guide = { version = "DIGEST", dest = [".claude/skills"] }

[mcp]
linear = "latest"

[channels]
backend = "latest"
"#
        .replace("DIGEST", &digest())
    }

    /// The normative MACHINE reference file.
    fn global_reference_file() -> String {
        r#"# What this machine's agents get.
schema = 1

[workspaces]
"topos.sh/acme" = "latest"
"topos.example.com/platform" = "latest"

[skills]
"topos.sh/acme/perf-review" = "latest"
"topos.sh/acme/noisy-skill" = "off"
"topos.sh/acme/deploy-guide" = { version = "DIGEST", dest = ["~/.claude/skills"] }
pdf-tools = "github:anthropics/skills#8c1f0a2"
weather-server = { path = "~/dev/weather-server", kind = "mcp" }

[channels]
"topos.sh/acme/frontend" = "latest"
"#
        .replace("DIGEST", &digest())
    }

    #[test]
    fn the_project_reference_file_parses_row_by_row() {
        let doc = parse_project(&project_reference_file());
        assert_eq!(
            doc.workspace,
            Some(("topos.sh".to_string(), "acme".to_string()))
        );
        let refs: Vec<&str> = doc.rows.iter().map(|r| r.reference.as_str()).collect();
        assert_eq!(
            refs,
            [
                "topos.sh/acme/code-review",
                "topos.sh/acme/release-process",
                "github.com/vercel-labs/skills/find-skills",
                "github.com/acme-corp/agent-skills/sql-style",
                "./tools/local-notes",
                "topos.sh/acme/deploy-guide",
                "topos.sh/acme/linear",
                "topos.sh/acme/channels/backend",
            ],
            "rows come back in file order; bare names resolve against the workspace line"
        );
        assert_eq!(doc.rows[0].value, EntryValue::Star);
        assert_eq!(doc.rows[1].value, EntryValue::Pin(digest()));
        assert_eq!(doc.rows[2].value, EntryValue::Star);
        assert_eq!(doc.rows[3].value, EntryValue::Pin("9d0e8c17".into()));
        assert!(matches!(
            &doc.rows[3].shape,
            KeyShape::RepoSkill { skill, .. } if skill == "sql-style"
        ));
        assert!(matches!(doc.rows[4].shape, KeyShape::LocalPath { .. }));
        assert_eq!(doc.rows[6].section, SectionKind::Mcp);
        assert!(matches!(
            &doc.rows[7].shape,
            KeyShape::Channel { channel, .. } if channel == "backend"
        ));
        assert!(doc.warnings.is_empty());
    }

    #[test]
    fn the_machine_reference_file_parses_row_by_row() {
        let doc = parse_global(&global_reference_file());
        assert_eq!(doc.workspace, None);
        let refs: Vec<&str> = doc.rows.iter().map(|r| r.reference.as_str()).collect();
        assert_eq!(
            refs,
            [
                "topos.sh/acme",
                "topos.example.com/platform",
                "topos.sh/acme/perf-review",
                "topos.sh/acme/noisy-skill",
                "topos.sh/acme/deploy-guide",
                "github.com/anthropics/skills/pdf-tools",
                "~/dev/weather-server",
                "topos.sh/acme/channels/frontend",
            ],
        );
        assert!(matches!(doc.rows[0].shape, KeyShape::Feed { .. }));
        assert_eq!(doc.rows[3].value, EntryValue::Off);
        assert_eq!(doc.rows[5].value, EntryValue::Pin("8c1f0a2".into()));
        assert!(matches!(doc.rows[6].shape, KeyShape::LocalPath { .. }));
        assert_eq!(
            doc.rows[6].value,
            EntryValue::Fields(EntryFields {
                kind: Some("mcp".into()),
                ..EntryFields::default()
            })
        );
    }

    #[test]
    fn unknown_sections_skip_with_a_warning() {
        let doc = parse_project(
            "schema = 1\nworkspace = \"topos.sh/acme\"\n\n[skills]\nx = \"latest\"\n\n\
             [memories]\nteam-context = \"latest\"\n",
        );
        assert_eq!(doc.rows.len(), 1, "the known section still parses");
        assert_eq!(doc.warnings.len(), 1);
        assert!(
            doc.warnings[0].contains("[memories]"),
            "{}",
            doc.warnings[0]
        );
        assert!(
            doc.warnings[0].contains("self-update"),
            "{}",
            doc.warnings[0]
        );
    }

    #[test]
    fn a_newer_schema_refuses_toward_self_update() {
        let e = parse_manifest("schema = 2\n", ManifestScope::Project).unwrap_err();
        assert!(e.message.contains("newer topos"), "{e}");
        assert!(e.message.contains("self-update"), "{e}");
        // Garbage inside a KNOWN section still refuses.
        assert!(
            parse_manifest(
                "workspace = \"topos.sh/acme\"\n[skills]\nx = 3\n",
                ManifestScope::Project
            )
            .is_err()
        );
    }

    #[test]
    fn bare_names_need_the_workspace_line() {
        let e = parse_manifest(
            "[skills]\ncode-review = \"latest\"\n",
            ManifestScope::Project,
        )
        .unwrap_err();
        assert!(e.message.contains("workspace = "), "{e}");
        // The machine file teaches the full spelling instead.
        let e = parse_manifest(
            "[skills]\ncode-review = \"latest\"\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("in full"), "{e}");
        // A repo/path row needs no workspace line in either file.
        let doc = parse_project("[skills]\nx = \"github:o/r\"\ny = \"./tools/y\"\n");
        assert_eq!(doc.rows.len(), 2);
    }

    #[test]
    fn a_project_uses_one_workspace() {
        // A full key naming ANOTHER workspace refuses.
        let e = parse_manifest(
            "workspace = \"topos.sh/acme\"\n[skills]\n\"topos.sh/other/x\" = \"latest\"\n",
            ManifestScope::Project,
        )
        .unwrap_err();
        assert!(e.message.contains("one workspace"), "{e}");
        // The project's OWN workspace spelled in full teaches the bare key.
        let e = parse_manifest(
            "workspace = \"topos.sh/acme\"\n[skills]\n\"topos.sh/acme/x\" = \"latest\"\n",
            ManifestScope::Project,
        )
        .unwrap_err();
        assert!(e.message.contains("bare name"), "{e}");
    }

    #[test]
    fn feeds_are_machine_only_and_exact() {
        let e = parse_manifest(
            "[workspaces]\n\"topos.sh/acme\" = \"latest\"\n",
            ManifestScope::Project,
        )
        .unwrap_err();
        assert!(e.message.contains("machine file"), "{e}");
        let e = parse_manifest(
            "[workspaces]\n\"topos.sh/acme\" = \"1234\"\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("latest"), "{e}");
        let e =
            parse_manifest("[workspaces]\nacme = \"latest\"\n", ManifestScope::Global).unwrap_err();
        assert!(e.message.contains("workspace address"), "{e}");
    }

    #[test]
    fn old_grammar_spellings_refuse_with_the_way_back() {
        // The v1 star value teaches "latest".
        let e = parse_manifest(
            "workspace = \"topos.sh/acme\"\n[skills]\nx = \"*\"\n",
            ManifestScope::Project,
        )
        .unwrap_err();
        assert!(e.message.contains("latest"), "{e}");
        // A v1 [bundles] section is an unknown section — skipped with the update-shaped warning,
        // so a machine mid-migration sees the message rather than a refusal. (The migration
        // itself is the sweep's rewrite.)
        let doc = parse_global("[bundles]\n\"topos.sh/acme\" = \"*\"\n");
        assert_eq!(doc.rows.len(), 0);
        assert_eq!(doc.warnings.len(), 1);
        // A path KEY teaches the value spelling.
        let e = parse_manifest(
            "[skills]\n\"./tools/x\" = \"latest\"\n",
            ManifestScope::Project,
        )
        .unwrap_err();
        assert!(e.message.contains("VALUE, not a key"), "{e}");
        // A repo KEY teaches the value spelling too.
        let e = parse_manifest(
            "[skills]\n\"github.com/o/r\" = \"latest\"\n",
            ManifestScope::Project,
        )
        .unwrap_err();
        assert!(e.message.contains("VALUE, not a key"), "{e}");
    }

    #[test]
    fn source_values_validate_their_pins() {
        let doc = parse_project("[skills]\nx = \"github:o/r#deadbeef00\"\n");
        assert_eq!(doc.rows[0].value, EntryValue::Pin("deadbeef00".into()));
        let e = parse_manifest(
            "[skills]\nx = \"github:o/r#main\"\n",
            ManifestScope::Project,
        )
        .unwrap_err();
        assert!(e.message.contains("commit hash"), "{e}");
        let e =
            parse_manifest("[skills]\nx = \"github:oops\"\n", ManifestScope::Project).unwrap_err();
        assert!(e.message.contains("github:<owner>/<repo>"), "{e}");
        // bitbucket rides the same shape; `.git` strips.
        let doc = parse_project("[skills]\nx = \"bitbucket:o/r.git\"\n");
        assert!(matches!(
            &doc.rows[0].shape,
            KeyShape::RepoSkill { host, repo, .. } if host == "bitbucket.org" && repo == "r"
        ));
    }

    #[test]
    fn off_is_machine_only() {
        let doc = parse_global("[skills]\n\"topos.sh/acme/x\" = \"off\"\n");
        assert_eq!(doc.rows[0].value, EntryValue::Off);
        let e = parse_manifest(
            "workspace = \"topos.sh/acme\"\n[skills]\nx = \"off\"\n",
            ManifestScope::Project,
        )
        .unwrap_err();
        assert!(e.message.contains("machine file"), "{e}");
    }

    #[test]
    fn duplicate_references_refuse() {
        let e = parse_manifest(
            "[skills]\n\"topos.sh/acme/x\" = \"latest\"\n\n[mcp]\n\"topos.sh/acme/x\" = \"latest\"\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("spelled twice"), "{e}");
    }

    #[test]
    fn editor_set_and_remove_are_exact_inverses() {
        let input = project_reference_file();
        let mut ed = ManifestEditor::open(&input, ManifestScope::Project).unwrap();
        ed.set_row(
            "topos.sh/acme/new-skill",
            &EntryValue::Star,
            BundleKind::Skill,
        )
        .unwrap();
        assert!(ed.rendered().contains("new-skill = \"latest\""));
        assert!(ed.remove_row("topos.sh/acme/new-skill"));
        assert_eq!(ed.rendered(), input, "add then remove restores the input");

        // Remove then re-add an existing row round-trips too.
        let mut ed = ManifestEditor::open(&input, ManifestScope::Project).unwrap();
        assert!(ed.remove_row("topos.sh/acme/linear"));
        ed.set_row("topos.sh/acme/linear", &EntryValue::Star, BundleKind::Mcp)
            .unwrap();
        assert_eq!(ed.rendered(), input);

        // And on the machine file, across value shapes.
        let input = global_reference_file();
        for (reference, value, kind) in [
            (
                "topos.sh/gamma/new-skill",
                EntryValue::Pin(digest()),
                BundleKind::Skill,
            ),
            (
                "github.com/new/repo/tool",
                EntryValue::Star,
                BundleKind::Skill,
            ),
            (
                "./tools/x",
                EntryValue::Fields(EntryFields {
                    dest: Some(vec!["~/.claude/skills".into()]),
                    ..EntryFields::default()
                }),
                BundleKind::Skill,
            ),
        ] {
            let mut ed = ManifestEditor::open(&input, ManifestScope::Global).unwrap();
            assert!(
                ed.row(reference).is_none(),
                "corpus rows must not pre-exist"
            );
            ed.set_row(reference, &value, kind).unwrap();
            assert!(ed.row(reference).is_some());
            assert!(ed.remove_row(reference));
            assert_eq!(ed.rendered(), input, "ref={reference}");
        }
    }

    #[test]
    fn editor_minted_sections_prune_and_preexisting_survive() {
        let input = "schema = 1\nworkspace = \"topos.sh/acme\"\n\n[skills]\nx = \"latest\"\n";
        let mut ed = ManifestEditor::open(input, ManifestScope::Project).unwrap();
        ed.set_row(
            "topos.sh/acme/channels/backend",
            &EntryValue::Star,
            BundleKind::Skill,
        )
        .unwrap();
        assert!(ed.rendered().contains("[channels]"));
        assert!(ed.remove_row("topos.sh/acme/channels/backend"));
        assert_eq!(ed.rendered(), input, "the minted [channels] pruned away");

        // A hand-authored empty section survives a remove that empties it.
        let with_empty = "workspace = \"topos.sh/acme\"\n\n[skills]\nx = \"latest\"\n";
        let mut ed = ManifestEditor::open(with_empty, ManifestScope::Project).unwrap();
        assert!(ed.remove_row("topos.sh/acme/x"));
        assert!(ed.rendered().contains("[skills]"), "{}", ed.rendered());
    }

    #[test]
    fn editor_enforces_the_one_workspace_rule() {
        let input = "workspace = \"topos.sh/acme\"\n[skills]\nx = \"latest\"\n";
        let mut ed = ManifestEditor::open(input, ManifestScope::Project).unwrap();
        let e = ed
            .set_row("topos.sh/other/y", &EntryValue::Star, BundleKind::Skill)
            .unwrap_err();
        assert!(e.message.contains("one workspace"), "{e}");
        // A project with no workspace line refuses a workspace row toward `init`.
        let mut ed = ManifestEditor::open("", ManifestScope::Project).unwrap();
        let e = ed
            .set_row("topos.sh/acme/x", &EntryValue::Star, BundleKind::Skill)
            .unwrap_err();
        assert!(e.message.contains("workspace = "), "{e}");
        ed.set_workspace("topos.sh", "acme").unwrap();
        ed.set_row("topos.sh/acme/x", &EntryValue::Star, BundleKind::Skill)
            .unwrap();
        assert!(ed.rendered().contains("x = \"latest\""));
        let e = ed.set_workspace("topos.sh", "other").unwrap_err();
        assert!(e.message.contains("already uses"), "{e}");
    }

    #[test]
    fn editor_spells_kinds_into_their_sections() {
        let mut ed =
            ManifestEditor::open("workspace = \"topos.sh/acme\"\n", ManifestScope::Project)
                .unwrap();
        ed.set_row("topos.sh/acme/linear", &EntryValue::Star, BundleKind::Mcp)
            .unwrap();
        ed.set_row("topos.sh/acme/review", &EntryValue::Star, BundleKind::Skill)
            .unwrap();
        let text = ed.rendered();
        let mcp_at = text.find("[mcp]").expect("mcp section");
        assert!(text[mcp_at..].contains("linear = \"latest\""));
        assert!(text.contains("[skills]"));
        // And the parse reads the sections back.
        let doc = parse_project(&text);
        assert_eq!(doc.rows.len(), 2);
        assert!(
            doc.rows
                .iter()
                .any(|r| r.section == SectionKind::Mcp && r.reference.ends_with("linear"))
        );
    }

    #[test]
    fn editor_row_lookup_reads_any_spelling() {
        let ed = ManifestEditor::open(&project_reference_file(), ManifestScope::Project).unwrap();
        let row = ed.row("topos.sh/acme/code-review").unwrap();
        assert_eq!(row.value, EntryValue::Star);
        let row = ed.row("github.com/vercel-labs/skills/find-skills").unwrap();
        assert!(matches!(row.shape, KeyShape::RepoSkill { .. }));
        assert!(ed.row("topos.sh/acme/absent").is_none());
    }

    #[test]
    fn a_new_row_never_overwrites_a_same_named_one() {
        let mut ed = ManifestEditor::open("", ManifestScope::Global).unwrap();
        ed.set_row("~/one/linear", &EntryValue::Star, BundleKind::Skill)
            .unwrap();
        let e = ed
            .set_row("~/two/linear", &EntryValue::Star, BundleKind::Skill)
            .unwrap_err();
        assert!(e.message.contains("already names another row"), "{e}");
        assert!(
            ed.row("~/one/linear").is_some(),
            "the standing row survives"
        );
        // A distinct `name` field resolves the collision.
        ed.set_row(
            "~/two/linear",
            &EntryValue::Fields(EntryFields {
                name: Some("linear-two".into()),
                ..EntryFields::default()
            }),
            BundleKind::Skill,
        )
        .unwrap();
        assert!(ed.row("~/two/linear").is_some());
    }

    #[test]
    fn repo_sets_have_no_spelling() {
        let mut ed = ManifestEditor::open("", ManifestScope::Global).unwrap();
        let e = ed
            .set_row("github.com/o/r", &EntryValue::Star, BundleKind::Skill)
            .unwrap_err();
        assert!(e.message.contains("row per skill"), "{e}");
    }

    #[test]
    fn templates_parse_clean() {
        parse_global(&materialized_global(&[]));
        let doc = parse_global(&materialized_global(&[(
            "topos.sh".to_string(),
            "acme".to_string(),
        )]));
        assert_eq!(doc.rows.len(), 1);
        assert!(matches!(doc.rows[0].shape, KeyShape::Feed { .. }));
        parse_project(&project_template(None));
        let doc = parse_project(&project_template(Some(("topos.sh", "acme"))));
        assert_eq!(
            doc.workspace,
            Some(("topos.sh".to_string(), "acme".to_string()))
        );
    }

    #[test]
    fn the_editor_writes_through_the_crash_safe_seam() {
        use crate::fs_seam::RealFs;
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-mdoc-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("topos.toml");
        std::fs::write(&path, "").unwrap();

        let mut ed = ManifestEditor::open("", ManifestScope::Global).unwrap();
        ed.set_row("topos.sh/acme", &EntryValue::Star, BundleKind::Skill)
            .unwrap();
        ed.write(&RealFs, &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, ed.rendered());
        assert!(ManifestEditor::open(&text, ManifestScope::Global).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_row_validates_canonical_rows() {
        assert!(check_row("topos.sh/acme/x", ManifestScope::Project, &EntryValue::Star).is_ok());
        assert!(
            check_row(
                "topos.sh/acme/x",
                ManifestScope::Global,
                &EntryValue::Pin(digest())
            )
            .is_ok()
        );
        assert!(
            check_row("topos.sh/acme/x", ManifestScope::Project, &EntryValue::Off).is_err(),
            "off is machine-only"
        );
        assert!(
            check_row("topos.sh/acme", ManifestScope::Project, &EntryValue::Star).is_err(),
            "feeds are machine-only"
        );
        assert!(
            check_row("github.com/o/r", ManifestScope::Global, &EntryValue::Star).is_err(),
            "repo sets have no v2 row"
        );
    }
}
