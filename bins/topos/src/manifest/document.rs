//! The NEW `topos.toml` document — one `[bundles]` namespace.
//!
//! THE JOIN RULE: an entry's reference is its TOML key path under `[bundles]`, segments joined
//! with `/`. A quoted key may carry slashes — opaque to TOML, transparent to the join — so these
//! three spell the SAME entry and parse identically:
//!
//! ```toml
//! [bundles]
//! "topos.sh/acme/code-review" = "*"
//! ```
//! ```toml
//! [bundles."topos.sh"]
//! "acme/code-review" = "*"
//! ```
//! ```toml
//! [bundles."topos.sh/acme"]
//! code-review = "*"
//! ```
//!
//! An ENTRY is always one line — `<ref> = <value>` where the value is a version string or an
//! inline table of fields (multiline inline tables included; TOML 1.1). A `[bundles.…]` section
//! header is always GROUPING, never an entry: a section opened per bundle (holding `version =`)
//! joins into a path no reference shape matches and refuses with the way back. Two spellings
//! joining to one reference are a parse error; a top-level table other than `bundles`
//! refuses too — this file is a complete recipe, and a typo must not silently drop demand.
//!
//! Values are validated BY SHAPE ([`KeyShape`]): things take a version (`"*"`, a 64-hex digest
//! for workspace bundles, a 7–40-hex commit for forge things) or fields; a channel takes no
//! `version`; a repo set takes `"*"`/a commit; the feed takes exactly `"*"`. `"off"` — the
//! per-bundle switch — is legal only on a workspace bundle row in the GLOBAL file, and feed rows
//! are global-only too (a project manifest is a repo fact, identical for every contributor).
//!
//! PLACEMENT is ONE field: `dest`, an array of destinations. A row without `dest` reaches every
//! agent this machine has, now and later (detection decides); a row with `dest` is FROZEN to
//! exactly those destinations. The machine file spells machine paths (`~/`-prefixed or
//! absolute); a project file spells relative paths inside the checkout. The RETIRED spellings —
//! `path`, `harness`, any `[defaults.<kind>]` table — refuse at load with the exact per-row
//! `dest` rewrite ([`ManifestError::migration`] errors close the TTY with `nothing changed`).
//!
//! A CHANNEL row carries members of BOTH kinds, so one array cannot speak for them: its `dest`
//! names placement FOLDERS for its skill members and `mcp_dest` names CONFIG FILES for its mcp
//! members. Each narrows only its own kind, and a channel with no `mcp_dest` does not narrow its
//! mcp members at all. A bundle row needs no such split — its kind already says which one `dest`
//! means.
//!
//! [`ManifestEditor`] edits format-preserving over `toml_edit` with a hard INVERSE property:
//! adding a row and removing it restores the input byte-for-byte, and vice versa — see
//! [`ManifestEditor::set_row`] / [`ManifestEditor::remove_row`]. Deterministic reorganization
//! (grouping, sorting) is the `fmt` normal form's job, never the editor's.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use toml_edit::{Array, Decor, DocumentMut, InlineTable, Item, Table, Value};

use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::manifest::keys::{KeyShape, classify_key};

/// Which manifest a text IS: the machine-personal global file (`~/.topos/topos.toml`) or a
/// project's committed `<project>/topos.toml`. Feed rows and `"off"` are global-only; everything
/// else reads identically in both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManifestScope {
    Global,
    Project,
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
    /// `"*"` — track what the reference currently serves.
    Star,
    /// `"off"` — the per-bundle switch (workspace bundles, global file only).
    Off,
    /// A version pin: a 64-hex digest (workspace bundle) or a 7–40-hex commit (forge thing).
    Pin(String),
    /// An inline table of fields.
    Fields(EntryFields),
}

impl EntryValue {
    /// The bundle kind this value DECLARES, against the closed vocabulary: an absent `kind` field
    /// (and every non-table spelling) is the default skill; `None` means the row names a kind this
    /// build does not own, which is not a skill either.
    pub(crate) fn declared_kind(&self) -> Option<crate::bundle_kind::BundleKind> {
        match self {
            EntryValue::Fields(f) => crate::bundle_kind::BundleKind::of_tag(f.kind.as_deref()),
            _ => Some(crate::bundle_kind::BundleKind::Skill),
        }
    }
}

/// What the editor writes for a row — the same shapes the parser reads back, rendered
/// deterministically (canonical field order, single-line inline tables).
pub(crate) type EntryValueSpelling = EntryValue;

/// One parsed `[bundles]` row: the joined reference, its shape, its validated value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BundleRow {
    pub reference: String,
    pub shape: KeyShape,
    pub value: EntryValue,
}

/// A parsed manifest: rows in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ManifestDoc {
    pub rows: Vec<BundleRow>,
}

/// A typed manifest refusal — names the specific fault AND the specific fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestError {
    pub message: String,
    /// The offending joined reference / key, when one exists.
    pub key: Option<String>,
    /// A RETIRED-spelling refusal (`path` / `harness` / `[defaults.<kind>]`) teaching the `dest`
    /// rewrite — surfaced as [`crate::error::ClientError::ManifestMigration`], whose TTY closes
    /// with `nothing changed`.
    pub migration: bool,
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
        migration: false,
    }
}

fn at(key: &str, message: impl Into<String>) -> ManifestError {
    ManifestError {
        message: message.into(),
        key: Some(key.to_string()),
        migration: false,
    }
}

fn migrate(key: Option<&str>, message: impl Into<String>) -> ManifestError {
    ManifestError {
        message: message.into(),
        key: key.map(str::to_string),
        migration: true,
    }
}

/// The complete field vocabulary — what a key must be to count as a mis-shelved FIELD (the
/// section-as-entry teaching) rather than an unknown word.
const FIELD_NAMES: [&str; 6] = ["version", "dest", "mcp_dest", "name", "subdir", "kind"];

/// The RETIRED field spellings — met at load, each refuses with its exact `dest` rewrite.
const RETIRED_FIELDS: [&str; 2] = ["path", "harness"];

/// The fields legal on each shape. The feed takes none (its value is exactly `"*"`).
pub(crate) fn legal_fields(shape: &KeyShape) -> &'static [&'static str] {
    match shape {
        KeyShape::WorkspaceBundle { .. } => &["version", "dest", "name"],
        KeyShape::RepoSkill { .. } => &["version", "dest", "name", "subdir", "kind"],
        KeyShape::LocalPath { .. } => &["dest", "name", "kind"],
        KeyShape::RepoSet { .. } => &["dest"],
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

fn joined(prefix: &[String], key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{}/{key}", prefix.join("/"))
    }
}

// ---------------------------------------------------------------------------
// Validation shared by the parser and the editor
// ---------------------------------------------------------------------------

fn feed_exact_star(reference: &str) -> ManifestError {
    at(
        reference,
        format!(
            "the feed takes exactly `\"*\"` — `{reference}` means whatever the workspace \
             currently serves; pin or configure individual bundles on their own rows",
        ),
    )
}

fn feed_in_project(reference: &str, shape: &KeyShape) -> ManifestError {
    let ws = shape.workspace_key().unwrap_or_default();
    at(
        reference,
        format!(
            "`{reference}` is a feed row — personal by nature, so it lives in the global \
             manifest (`~/.topos/topos.toml`) only; a project manifest is a repo fact, identical \
             for every contributor, and a channel (`{ws}/channels/<name>`) is the repo-shaped \
             set to name here",
        ),
    )
}

fn off_check(reference: &str, shape: &KeyShape, scope: ManifestScope) -> Result<(), ManifestError> {
    match (shape, scope) {
        (KeyShape::WorkspaceBundle { .. }, ManifestScope::Global) => Ok(()),
        (KeyShape::WorkspaceBundle { .. }, ManifestScope::Project) => Err(at(
            reference,
            "`\"off\"` is the personal switch and lives in the global manifest \
             (`~/.topos/topos.toml`) only — a project manifest is a repo fact, identical for \
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
                         64-character version digest; `\"*\"` tracks current",
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
                         hex characters); `\"*\"` tracks the default branch",
                    ),
                ))
            }
        }
        KeyShape::LocalPath { .. } => Err(at(
            reference,
            "a local folder has no versions to pin — its value is `\"*\"` or an inline table \
             of fields",
        )),
        KeyShape::Channel { .. } => Err(at(
            reference,
            "a channel takes no pin — its value is `\"*\"` or an inline table (without \
             `version`)",
        )),
        KeyShape::Feed { .. } => Err(feed_exact_star(reference)),
    }
}

/// DRY-RUN one row: classify the reference and validate the value against its shape + scope —
/// exactly what [`ManifestEditor::set_row`] would do, without a file, an editor, or a write. A
/// caller whose write is preceded by a durable side effect (a granted forge origin, a minted
/// store) proves the row is writable FIRST, so a refusal cannot land after the consent it was
/// supposed to gate.
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
            return Err(feed_in_project(reference, shape));
        }
        return match value {
            EntryValue::Star => Ok(()),
            _ => Err(feed_exact_star(reference)),
        };
    }
    match value {
        EntryValue::Star => Ok(()),
        EntryValue::Off => off_check(reference, shape, scope),
        EntryValue::Pin(p) => pin_check(reference, shape, p),
        EntryValue::Fields(f) => fields_check(reference, shape, scope, f),
    }
}

/// The field-legality half, on an already-typed [`EntryFields`] — including the `dest` rules the
/// scope decides: the machine file names machine paths (`~/`-prefixed or absolute), a project
/// file names relative paths contained inside the checkout, and a local MCP row's dest entries
/// must be config files the harness descriptor table knows (their format must be known to edit
/// them).
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
        && f.kind.as_deref() == Some(crate::bundle_kind::BundleKind::Mcp.as_str())
    {
        return Err(mcp_needs_a_workspace(reference));
    }
    // The kind VOCABULARY is closed in a hand-written file exactly as it is at the server door: a
    // word no kind of this build owns names delivery mechanics nothing here can run, so it refuses
    // at LOAD rather than parsing into a demand that would be placed as a skill.
    if let Some(word) = f.kind.as_deref()
        && crate::bundle_kind::BundleKind::parse(word).is_none()
    {
        return Err(unknown_kind(reference, word));
    }
    if let Some(v) = &f.version {
        if v == "off" {
            return Err(off_in_table(reference));
        }
        if v != "*" {
            pin_check(reference, shape, v)?;
        }
    }
    if let Some(dest) = &f.dest {
        dest_list_check(reference, "dest", scope, dest)?;
        // A local MCP row's kind is knowable at load — its dest entries are config FILES and
        // must come from the descriptor table (an unknown file's format cannot be edited).
        // Workspace rows learn their kind at delivery; the reconcile enforces the same rule.
        if matches!(shape, KeyShape::LocalPath { .. })
            && f.kind.as_deref() == Some(crate::bundle_kind::BundleKind::Mcp.as_str())
        {
            for entry in dest {
                if super::dest::mcp_slug_for_dest(entry, scope).is_none() {
                    return Err(at(reference, super::dest::unknown_mcp_file(entry, scope)));
                }
            }
        }
    }
    // A channel's `mcp_dest` takes the same path dialect and nothing more. Which files it names is
    // NOT settled here: a channel expands at delivery, so whether it has any mcp member at all is
    // unknowable at load — an entry no harness claims is the reconcile's to report, against the
    // members that actually resolved, rather than a refusal that would take the channel's skill
    // members down with it.
    if let Some(mcp_dest) = &f.mcp_dest {
        dest_list_check(reference, "mcp_dest", scope, mcp_dest)?;
    }
    Ok(())
}

/// One destination array's shape rules: it names at least one entry, and every entry speaks the
/// scope's path dialect. `field` is the spelling the refusal quotes, so `dest` and `mcp_dest`
/// teach with their own word.
fn dest_list_check(
    reference: &str,
    field: &str,
    scope: ManifestScope,
    entries: &[String],
) -> Result<(), ManifestError> {
    if entries.is_empty() {
        // `mcp_dest` opens on a vowel SOUND ("em-cee-pee"), so the article is spelled for the word
        // rather than for its first letter.
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

/// The per-scope `dest` path dialect (the two-sided rule, enforced at load like feed/`"off"`):
/// the machine file names machine paths, a project file names checkout-contained relative paths.
/// The dialect check for ONE dest entry, reference-free — the `--dest` literal's validation (the
/// same rules [`check_row`] runs over a whole row, so a selector can refuse before any side
/// effect with the grammar's own words).
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

/// The refusal a repo row tagged `kind = "mcp"` earns — the one place that teaches where an MCP
/// bundle comes from.
fn mcp_needs_a_workspace(reference: &str) -> ManifestError {
    at(
        reference,
        format!(
            "`kind = \"mcp\"` does not fit a repo skill — `{reference}` cannot deliver an MCP \
             server; publish the bundle to a workspace and name it here as \
             `<host>/<workspace>/<bundle>`"
        ),
    )
}

/// The refusal a row naming a kind topos does not deliver earns — the same teaching the server
/// door gives, in this file's idiom: what was written, and the whole vocabulary that exists.
fn unknown_kind(reference: &str, word: &str) -> ManifestError {
    at(
        reference,
        format!(
            "`kind = \"{word}\"` on `{reference}` is not a kind topos delivers — known kinds: {}",
            crate::bundle_kind::BundleKind::known_list()
        ),
    )
}

fn off_in_table(reference: &str) -> ManifestError {
    at(
        reference,
        "`off` is a whole value (`\"<ref>\" = \"off\"`) — never a field",
    )
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// A uniform view over the two TOML table spellings (a section [`Table`], an inline table) and
/// plain values — so a retired `[defaults.<kind>]` table reads identically however written.
enum Node<'a> {
    Item(&'a Item),
    Value(&'a Value),
}

impl<'a> Node<'a> {
    fn as_str(&self) -> Option<&'a str> {
        match self {
            Node::Item(i) => i.as_str(),
            Node::Value(v) => v.as_str(),
        }
    }

    fn as_array(&self) -> Option<&'a Array> {
        match self {
            Node::Item(i) => i.as_array(),
            Node::Value(v) => v.as_array(),
        }
    }

    fn pairs(&self) -> Option<Vec<(&'a str, Node<'a>)>> {
        match self {
            Node::Item(Item::Table(t)) => Some(t.iter().map(|(k, i)| (k, Node::Item(i))).collect()),
            Node::Item(Item::Value(v)) => Node::Value(v).pairs(),
            Node::Value(Value::InlineTable(t)) => {
                Some(t.iter().map(|(k, v)| (k, Node::Value(v))).collect())
            }
            _ => None,
        }
    }
}

/// Parse + validate a manifest text. Rows come back in file order; every refusal names the
/// specific fault and the specific fix.
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
    for (key, _) in doc.iter() {
        if key != "bundles" && key != "defaults" {
            return Err(plain(format!(
                "unknown top-level `{key}` — a manifest holds `[bundles]` only; a typo here \
                 would silently drop what it names, so it refuses instead",
            )));
        }
    }
    let mut rows = Vec::new();
    if let Some(item) = doc.get("bundles") {
        let Item::Table(t) = item else {
            return Err(plain(
                "`bundles` is a section — spell it `[bundles]` with one entry per line",
            ));
        };
        let mut prefix = Vec::new();
        let mut seen = HashSet::new();
        collect_rows(t, &mut prefix, scope, &mut seen, &mut rows)?;
    }
    // `[defaults.<kind>]` is a RETIRED spelling: refuse with the per-row rewrite (the rows above
    // parsed first, so an mcp defaults table can name every row the file marks as mcp).
    if let Some(item) = doc.get("defaults") {
        return Err(defaults_migration(item, scope, &rows));
    }
    Ok(ManifestDoc { rows })
}

/// The refusal a present `[defaults.<kind>]` table earns. Kind `mcp` carrying `harness` prints
/// the per-row rewrite for every row the file marks as mcp (local rows with `kind = "mcp"`), or
/// the general teaching with the descriptor file for each slug when none exist; every other kind
/// teaches "set dest on each row".
fn defaults_migration(item: &Item, scope: ManifestScope, rows: &[BundleRow]) -> ManifestError {
    let Some(pairs) = Node::Item(item).pairs() else {
        return migrate(
            None,
            "`[defaults]` is no longer read — placement is the `dest` field, set on each row",
        );
    };
    // ONE refusal at a time: the first kind the table spells carries the teaching.
    if let Some((kind, node)) = pairs.into_iter().next() {
        let key = format!("defaults.{kind}");
        if kind == crate::bundle_kind::BundleKind::Mcp.as_str()
            && let Some(kpairs) = node.pairs()
            && let Some((_, hv)) = kpairs.into_iter().find(|(k, _)| *k == "harness")
        {
            let slugs: Vec<String> = hv
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let (rewrite, unmapped) =
                super::dest::rewrite_slugs(&slugs, |s| super::dest::mcp_dest_spelling(s, scope));
            let mcp_rows: Vec<&str> = rows
                .iter()
                .filter(|r| {
                    matches!(r.shape, KeyShape::LocalPath { .. })
                        && r.value.declared_kind() == Some(crate::bundle_kind::BundleKind::Mcp)
                })
                .map(|r| r.reference.as_str())
                .collect();
            let spelled = old_spelling("harness", &hv);
            let mut message = if mcp_rows.is_empty() {
                match &rewrite {
                    Some(list) => format!(
                        "`{spelled}` in [defaults.mcp] is now written as dest on each mcp row — \
                         use dest = [{list}]",
                    ),
                    None => format!(
                        "`{spelled}` in [defaults.mcp] is now written as dest on each mcp row — \
                         name each config file directly",
                    ),
                }
            } else {
                let per_row: Vec<String> = mcp_rows
                    .iter()
                    .map(|r| match &rewrite {
                        Some(list) => format!("on \"{r}\" use dest = [{list}]"),
                        None => format!("on \"{r}\" name each config file directly"),
                    })
                    .collect();
                format!(
                    "`{spelled}` in [defaults.mcp] is now written as dest — {}",
                    per_row.join("; ")
                )
            };
            if !unmapped.is_empty() {
                message.push_str(&format!(
                    "; {} to no destination here",
                    super::dest::maps_clause(&unmapped)
                ));
            }
            return migrate(Some(&key), message);
        }
        return migrate(
            Some(&key),
            format!("`[{key}]` is no longer read — placement is the `dest` field, set on each row"),
        );
    }
    migrate(
        None,
        "`[defaults]` is no longer read — placement is the `dest` field, set on each row",
    )
}

/// A retired field's refusal half: the field in its ORIGINAL value spelling (decor trimmed).
fn old_spelling(field: &str, v: &Node<'_>) -> String {
    let raw = match v {
        Node::Item(i) => i
            .as_value()
            .map(|val| val.to_string())
            .unwrap_or_else(|| "…".to_owned()),
        Node::Value(val) => val.to_string(),
    };
    format!("{field} = {}", raw.trim())
}

fn collect_rows(
    table: &Table,
    prefix: &mut Vec<String>,
    scope: ManifestScope,
    seen: &mut HashSet<String>,
    out: &mut Vec<BundleRow>,
) -> Result<(), ManifestError> {
    for (key, item) in table.iter() {
        match item {
            Item::None => {}
            Item::Value(v) => {
                let reference = joined(prefix, key);
                let shape = classify_entry(&reference)?;
                if matches!(shape, KeyShape::Feed { .. }) && scope == ManifestScope::Project {
                    return Err(feed_in_project(&reference, &shape));
                }
                if !seen.insert(reference.clone()) {
                    return Err(at(
                        &reference,
                        format!(
                            "`{reference}` is spelled twice — two keys join to the same \
                             reference; keep one line",
                        ),
                    ));
                }
                let value = value_of(&reference, &shape, scope, v)?;
                out.push(BundleRow {
                    reference,
                    shape,
                    value,
                });
            }
            Item::Table(t) => {
                prefix.push(key.to_string());
                collect_rows(t, prefix, scope, seen, out)?;
                prefix.pop();
            }
            Item::ArrayOfTables(_) => {
                return Err(at(
                    &joined(prefix, key),
                    "`[[…]]` array tables are not manifest entries — an entry is one line: \
                     `<ref> = <value>`",
                ));
            }
        }
    }
    Ok(())
}

/// Classify a joined key; a failure whose leaf is a FIELD name on a parent that IS a valid
/// reference gets the section-as-entry teaching (a section per bundle holding `version = …`
/// joins into `<ref>/version`).
fn classify_entry(reference: &str) -> Result<KeyShape, ManifestError> {
    classify_key(reference).map_err(|e| {
        let leaf = reference.rsplit('/').next().unwrap_or(reference);
        if leaf != reference
            && (FIELD_NAMES.contains(&leaf) || RETIRED_FIELDS.contains(&leaf))
            && classify_key(&reference[..reference.len() - leaf.len() - 1]).is_ok()
        {
            let parent = &reference[..reference.len() - leaf.len() - 1];
            at(
                reference,
                format!(
                    "`{reference}` reads as field `{leaf}` on `{parent}` — an entry is one \
                     line: `<ref> = <value>`; use an inline table for fields \
                     (`\"{parent}\" = {{ {leaf} = … }}`)",
                ),
            )
        } else {
            at(reference, e.message)
        }
    })
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
    reference: &str,
    shape: &KeyShape,
    scope: ManifestScope,
    v: &Value,
) -> Result<EntryValue, ManifestError> {
    match v {
        Value::String(s) => string_value(reference, shape, scope, s.value()),
        Value::InlineTable(t) => {
            let fields = fields_of(reference, shape, scope, t)?;
            fields_check(reference, shape, scope, &fields)?;
            Ok(EntryValue::Fields(fields))
        }
        Value::Array(_) => Err(at(
            reference,
            "an array is not an entry value — an entry is `<ref> = \"<version>\"` or \
             `<ref> = { <fields> }`",
        )),
        other => Err(at(
            reference,
            format!(
                "a {} is not an entry value — an entry is `<ref> = \"<version>\"` or \
                 `<ref> = {{ <fields> }}`",
                value_type_noun(other)
            ),
        )),
    }
}

fn string_value(
    reference: &str,
    shape: &KeyShape,
    scope: ManifestScope,
    s: &str,
) -> Result<EntryValue, ManifestError> {
    if matches!(shape, KeyShape::Feed { .. }) {
        return if s == "*" {
            Ok(EntryValue::Star)
        } else {
            Err(feed_exact_star(reference))
        };
    }
    match s {
        "*" => Ok(EntryValue::Star),
        "off" => off_check(reference, shape, scope).map(|()| EntryValue::Off),
        _ => pin_check(reference, shape, s).map(|()| EntryValue::Pin(s.to_string())),
    }
}

/// Read an inline table's fields, refusing unknown keys, per-field type faults, and the RETIRED
/// spellings (`path` / `harness` — each refused with its exact `dest` rewrite); the per-shape
/// legality runs after, in [`fields_check`].
fn fields_of(
    reference: &str,
    shape: &KeyShape,
    scope: ManifestScope,
    t: &InlineTable,
) -> Result<EntryFields, ManifestError> {
    if matches!(shape, KeyShape::Feed { .. }) {
        return Err(feed_exact_star(reference));
    }
    let legal = legal_fields(shape);
    // The row's own `kind` decides which rewrite table a retired `harness` maps through — read
    // it FIRST so field order in the file cannot change the teaching.
    let row_kind = t.get("kind").and_then(Value::as_str);
    let mut f = EntryFields::default();
    for (k, v) in t.iter() {
        if RETIRED_FIELDS.contains(&k) {
            return Err(retired_field(reference, shape, scope, row_kind, k, v));
        }
        if !legal.contains(&k) {
            return Err(if FIELD_NAMES.contains(&k) {
                illegal_field(reference, shape, k)
            } else {
                at(
                    reference,
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
                let s = v
                    .as_str()
                    .ok_or_else(|| at(reference, "`version` is a string"))?;
                if s == "off" {
                    return Err(off_in_table(reference));
                }
                f.version = Some(s.to_string());
            }
            "dest" => f.dest = Some(dest_of(reference, "dest", v)?),
            "mcp_dest" => f.mcp_dest = Some(dest_of(reference, "mcp_dest", v)?),
            "name" => {
                f.name = Some(
                    v.as_str()
                        .ok_or_else(|| at(reference, "`name` is a string"))?
                        .to_string(),
                );
            }
            "subdir" => {
                f.subdir = Some(
                    v.as_str()
                        .ok_or_else(|| at(reference, "`subdir` is a string"))?
                        .to_string(),
                );
            }
            "kind" => {
                f.kind = Some(
                    v.as_str()
                        .ok_or_else(|| at(reference, "`kind` is a string"))?
                        .to_string(),
                );
            }
            _ => {}
        }
    }
    Ok(f)
}

/// Parse a `dest`/`mcp_dest` value: an array of destination strings (emptiness and dialect are
/// [`fields_check`]'s judgment — this is only the type gate).
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

/// The refusal a RETIRED field earns — the exact per-row `dest` rewrite, scope-correct:
///
/// - `harness` on an MCP-shaped row (a local row with `kind = "mcp"`, a workspace bundle, a
///   channel — where it only ever drove MCP narrowing) maps each slug through the MCP
///   descriptor table's config-file paths; on a forge row (repo skill / repo set) through the
///   harness registry's skills roots. A slug that maps nowhere is named as unmapped. On a CHANNEL
///   the rewrite lands in `mcp_dest`, the field that does that narrowing now.
/// - `path` carries its directory value(s) over verbatim.
fn retired_field(
    reference: &str,
    shape: &KeyShape,
    scope: ManifestScope,
    row_kind: Option<&str>,
    field: &str,
    v: &Value,
) -> ManifestError {
    let spelled = old_spelling(field, &Node::Value(v));
    if field == "path" {
        let dirs: Vec<String> = match (v.as_str(), v.as_inline_table()) {
            (Some(s), _) => vec![s.to_owned()],
            // The per-harness table form: each directory it names, `default` first.
            (None, Some(t)) => {
                let mut out: Vec<String> = t
                    .get("default")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .into_iter()
                    .collect();
                out.extend(t.iter().filter_map(|(k, pv)| {
                    (k != "default")
                        .then(|| pv.as_str().map(str::to_owned))
                        .flatten()
                }));
                out
            }
            (None, None) => Vec::new(),
        };
        let message = if dirs.is_empty() {
            format!("`{spelled}` on \"{reference}\" is now written as dest — name each destination")
        } else {
            format!(
                "`{spelled}` on \"{reference}\" is now written as dest — use dest = [{}]",
                super::dest::quoted_list(&dirs)
            )
        };
        return migrate(Some(reference), message);
    }
    // `harness`: map each named slug through the STATIC tables, default spellings.
    let slugs: Vec<String> = v
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let forge = matches!(shape, KeyShape::RepoSet { .. } | KeyShape::RepoSkill { .. });
    let mcp_shaped = matches!(
        shape,
        KeyShape::WorkspaceBundle { .. } | KeyShape::Channel { .. }
    ) || (matches!(shape, KeyShape::LocalPath { .. })
        && row_kind == Some(crate::bundle_kind::BundleKind::Mcp.as_str()));
    // A channel's `harness` only ever narrowed its MCP members, and that job is `mcp_dest`'s now —
    // its `dest` speaks for the skill members. The rewrite names the field that does the same work.
    let into = if matches!(shape, KeyShape::Channel { .. }) {
        "mcp_dest"
    } else {
        "dest"
    };
    let map_slug = |s: &str| -> Option<String> {
        if forge {
            super::dest::skills_dest_spelling(s, scope)
        } else if mcp_shaped {
            super::dest::mcp_dest_spelling(s, scope)
        } else {
            // A local skill row's harness never did placement work; the skills root is still
            // the honest destination for the slug it names.
            super::dest::skills_dest_spelling(s, scope)
        }
    };
    let (rewrite, unmapped) = super::dest::rewrite_slugs(&slugs, map_slug);
    let mut message = match &rewrite {
        Some(list) => {
            format!(
                "`{spelled}` on \"{reference}\" is now written as {into} — use {into} = [{list}]"
            )
        }
        None => format!(
            "`{spelled}` on \"{reference}\" is now written as {into} — name each destination \
             directly"
        ),
    };
    if !unmapped.is_empty() {
        message.push_str(&format!(
            "; {} to no destination here",
            super::dest::maps_clause(&unmapped)
        ));
    }
    migrate(Some(reference), message)
}

// ---------------------------------------------------------------------------
// The editor
// ---------------------------------------------------------------------------

/// The format-preserving editor with the INVERSE property: for a row that did not pre-exist,
/// `set_row` then `remove_row` restores the input byte-for-byte; for one that did (spelled the
/// way [`value_item`] spells it), `remove_row` then `set_row` does too. The editor never creates
/// a grouping section (rows land in an EXISTING workspace section, else flat) and never deletes
/// a section header it did not itself mint — an empty section is harmless grouping; the `fmt`
/// normal form prunes and reorganizes.
pub(crate) struct ManifestEditor {
    doc: DocumentMut,
    scope: ManifestScope,
    /// Every section path present at open — `remove_row` prunes only tables THIS editor minted,
    /// so a hand-authored grouping header always survives.
    preexisting: HashSet<Vec<String>>,
    /// The EXACT text this editor was built from (`None` = a fresh document; the file is
    /// expected absent). [`Self::write`]'s compare-and-swap re-reads the file immediately before
    /// its rename and refuses on any drift — an outside editor's bytes are never overwritten by
    /// a document prepared from an older reading.
    opened_from: Option<String>,
}

impl ManifestEditor {
    /// Open a manifest text for editing; the WHOLE document is validated first, so the edit
    /// methods never meet a shape the parser would refuse.
    pub(crate) fn open(text: &str, scope: ManifestScope) -> Result<Self, ManifestError> {
        let doc: DocumentMut = text
            .parse()
            .map_err(|e| plain(format!("not valid TOML: {e}")))?;
        parse_document(&doc, scope)?;
        let mut preexisting = HashSet::new();
        record_tables(doc.as_table(), &mut Vec::new(), &mut preexisting);
        Ok(Self {
            doc,
            scope,
            preexisting,
            opened_from: Some(text.to_owned()),
        })
    }

    /// A fresh, empty document (no file yet).
    pub(crate) fn open_or_new(scope: ManifestScope) -> Self {
        Self {
            doc: DocumentMut::new(),
            scope,
            preexisting: HashSet::new(),
            opened_from: None,
        }
    }

    /// Upsert one row. An existing spelling (any grouping level) is edited IN PLACE — never
    /// re-homed. A new row lands in its workspace's `[bundles."<host>/<ws>"]` section when one
    /// already exists, else flat under `[bundles]`; a feed row always lands flat. A new section
    /// header is never created here — only `fmt` reorganizes.
    ///
    /// # Errors
    /// The reference must classify and the value must be legal for its shape and this file's
    /// scope — the same rules [`parse_manifest`] enforces, so the editor cannot write a file
    /// `open` would refuse.
    pub(crate) fn set_row(
        &mut self,
        reference: &str,
        value: &EntryValueSpelling,
    ) -> Result<(), ManifestError> {
        let shape = classify_key(reference).map_err(|e| at(reference, e.message))?;
        check_value(reference, &shape, self.scope, value)?;

        if let Some((group, leaf)) = self.locate(reference) {
            let mut path = vec!["bundles".to_string()];
            path.extend(group);
            let table =
                table_at_mut(self.doc.as_table_mut(), &path).expect("row located a moment ago");
            if let Some(item) = table.get_mut(&leaf) {
                *item = value_item(value);
            }
            return Ok(());
        }

        // TOML cannot hold a value and a table at one key path — a workspace's feed row and its
        // grouping section are that collision, so the feed refuses while the group stands.
        if matches!(shape, KeyShape::Feed { .. })
            && self
                .bundles()
                .is_some_and(|b| b.get(reference).is_some_and(Item::is_table))
        {
            return Err(at(
                reference,
                format!(
                    "`{reference}` has grouped rows (`[bundles.\"{reference}\"]`) — TOML cannot \
                     hold the feed row and the grouping at one key; spell the grouped rows flat \
                     under `[bundles]` first",
                ),
            ));
        }

        let target = match (shape.workspace_key(), shape.section_tail()) {
            (Some(ws), Some(tail))
                if self
                    .bundles()
                    .is_some_and(|b| b.get(&ws).is_some_and(Item::is_table)) =>
            {
                Some((ws, tail))
            }
            _ => None,
        };
        self.ensure_bundles();
        let bundles = self.doc["bundles"].as_table_mut().expect("just ensured");
        match target {
            Some((ws, tail)) => {
                bundles
                    .get_mut(&ws)
                    .and_then(Item::as_table_mut)
                    .expect("section just probed")
                    .insert(&tail, value_item(value));
            }
            None => {
                bundles.insert(reference, value_item(value));
            }
        }
        Ok(())
    }

    /// Remove one row, wherever it is spelled. A preceding standalone comment SURVIVES (only
    /// the line's own trailing comment may go with it); a grouping header this editor did not
    /// mint survives too, even empty.
    pub(crate) fn remove_row(&mut self, reference: &str) -> bool {
        let Some((group, leaf)) = self.locate(reference) else {
            return false;
        };
        let mut path = vec!["bundles".to_string()];
        path.extend(group);
        let mut orphan: Option<String> = None;
        {
            let table =
                table_at_mut(self.doc.as_table_mut(), &path).expect("row located a moment ago");
            let keys: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();
            let idx = keys
                .iter()
                .position(|k| *k == leaf)
                .expect("row located a moment ago");
            let prefix = table
                .key(&leaf)
                .map(|k| decor_prefix(k.leaf_decor()))
                .unwrap_or_default();
            table.remove(&leaf);
            if prefix.contains('#') {
                // The standalone comment above the removed line moves onto whatever follows.
                match keys.get(idx + 1) {
                    Some(next) => match table.get_mut(next) {
                        Some(Item::Table(t)) => {
                            let existing = decor_prefix(t.decor());
                            t.decor_mut().set_prefix(prefix + &existing);
                        }
                        Some(_) => {
                            if let Some(mut km) = table.key_mut(next) {
                                let existing = decor_prefix(km.leaf_decor());
                                km.leaf_decor_mut().set_prefix(prefix + &existing);
                            }
                        }
                        None => orphan = Some(prefix),
                    },
                    None => orphan = Some(prefix),
                }
            }
        }
        if let Some(comment) = orphan {
            append_trailing(&mut self.doc, &comment);
        }
        self.prune(path);
        true
    }

    /// Look one row up by reference, wherever it is spelled.
    pub(crate) fn row(&self, reference: &str) -> Option<BundleRow> {
        let (group, leaf) = self.locate(reference)?;
        let mut path = vec!["bundles".to_string()];
        path.extend(group);
        let table = table_at(self.doc.as_table(), &path)?;
        let Item::Value(v) = table.get(&leaf)? else {
            return None;
        };
        let shape = classify_key(reference).ok()?;
        let value = value_of(reference, &shape, self.scope, v).ok()?;
        Some(BundleRow {
            reference: reference.to_string(),
            shape,
            value,
        })
    }

    /// The serialized document (what [`Self::write`] persists).
    pub(crate) fn rendered(&self) -> String {
        self.doc.to_string()
    }

    /// Persist atomically through the one crash-safe write — as a COMPARE-AND-SWAP against the
    /// text this editor was opened from (absence, for a fresh document). The manifest lock
    /// serializes topos's own writers, but it cannot fence a person's editor or a `sed`: the
    /// file is re-read immediately before the atomic rename and byte-compared; any drift refuses
    /// with the typed [`ClientError::ManifestChanged`] — staged document discarded, the outside
    /// writer's bytes untouched, the re-run reads the file as it now is. The syscall pair
    /// between that final compare and the rename is the accepted residual (documented in
    /// `crate::ops::manifest_edit`'s module doc).
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

    fn bundles(&self) -> Option<&Table> {
        self.doc.get("bundles").and_then(Item::as_table)
    }

    fn locate(&self, reference: &str) -> Option<(Vec<String>, String)> {
        let mut prefix = Vec::new();
        find_row_in(self.bundles()?, &mut prefix, reference)
    }

    fn ensure_bundles(&mut self) {
        if self.doc.get("bundles").is_some() {
            return;
        }
        let mut t = Table::new();
        t.set_implicit(false);
        // A comment-only document parks its header in the TRAILING decor, which would serialize
        // BELOW an inserted table — move it onto the first table's prefix so the header stays at
        // the top (and `prune` moves it back if this table empties out again).
        let trailing = self.doc.trailing().as_str().unwrap_or("").to_owned();
        if !trailing.trim().is_empty() && self.doc.as_table().is_empty() {
            self.doc.set_trailing("");
            let mut prefix = trailing;
            if !prefix.ends_with('\n') {
                prefix.push('\n');
            }
            t.decor_mut().set_prefix(prefix);
        }
        self.doc.insert("bundles", Item::Table(t));
    }

    /// Delete now-empty grouping tables the editor itself minted (dotted-key chains always —
    /// an empty dotted table cannot render); a PREEXISTING section header is never deleted.
    fn prune(&mut self, mut path: Vec<String>) {
        while !path.is_empty() {
            let Some(t) = table_at(self.doc.as_table(), &path) else {
                break;
            };
            if !t.is_empty() {
                break;
            }
            if !t.is_dotted() && self.preexisting.contains(&path) {
                break;
            }
            let prefix = decor_prefix(t.decor());
            let leaf = path.pop().expect("loop guard");
            {
                let parent = if path.is_empty() {
                    Some(self.doc.as_table_mut())
                } else {
                    table_at_mut(self.doc.as_table_mut(), &path)
                };
                let Some(parent) = parent else { break };
                parent.remove(&leaf);
            }
            if !prefix.trim().is_empty() {
                // The header comment `ensure_bundles` moved onto this table goes back where it
                // came from.
                append_trailing(&mut self.doc, &prefix);
            }
        }
    }
}

fn record_tables(table: &Table, prefix: &mut Vec<String>, out: &mut HashSet<Vec<String>>) {
    for (k, item) in table.iter() {
        if let Item::Table(t) = item {
            prefix.push(k.to_string());
            out.insert(prefix.clone());
            record_tables(t, prefix, out);
            prefix.pop();
        }
    }
}

fn find_row_in(
    table: &Table,
    prefix: &mut Vec<String>,
    want: &str,
) -> Option<(Vec<String>, String)> {
    for (k, item) in table.iter() {
        match item {
            Item::Value(_) => {
                if joined(prefix, k) == want {
                    return Some((prefix.clone(), k.to_string()));
                }
            }
            Item::Table(t) => {
                prefix.push(k.to_string());
                if let Some(hit) = find_row_in(t, prefix, want) {
                    return Some(hit);
                }
                prefix.pop();
            }
            _ => {}
        }
    }
    None
}

fn table_at<'a>(mut t: &'a Table, path: &[String]) -> Option<&'a Table> {
    for k in path {
        t = t.get(k)?.as_table()?;
    }
    Some(t)
}

fn table_at_mut<'a>(mut t: &'a mut Table, path: &[String]) -> Option<&'a mut Table> {
    for k in path {
        t = t.get_mut(k)?.as_table_mut()?;
    }
    Some(t)
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

/// Render a value deterministically: version strings plain, fields as a single-line inline
/// table in canonical order (version, dest, name, subdir, kind).
pub(crate) fn value_item(v: &EntryValue) -> Item {
    match v {
        EntryValue::Star => toml_edit::value("*"),
        EntryValue::Off => toml_edit::value("off"),
        EntryValue::Pin(p) => toml_edit::value(p.as_str()),
        EntryValue::Fields(f) => {
            let mut t = InlineTable::new();
            if let Some(v) = &f.version {
                t.insert("version", v.as_str().into());
            }
            if let Some(d) = &f.dest {
                let mut a = Array::new();
                for x in d {
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
// File birth
// ---------------------------------------------------------------------------

/// The GLOBAL file's birth content: the header states the contract, then one feed row per
/// `(host, workspace)` given. Ordinary birth passes NO rows — the file is the machine's complete
/// recipe, and `topos login` is the only automatic feed-row author (it writes a workspace's row
/// on this machine's first connection; a row someone deleted stays deleted). The one caller that
/// passes rows is the upgrade migration, which spells out what the machine already received.
/// Parses clean as [`ManifestScope::Global`].
pub(crate) fn materialized_global(workspaces: &[(String, String)]) -> String {
    let mut doc = DocumentMut::new();
    let mut t = Table::new();
    t.set_implicit(false);
    t.decor_mut().set_prefix(
        "# topos.toml — the complete recipe for what lands on this machine's personal scope.\n\
         # Managed by the topos CLI; hand-edits are welcome. A feed row\n\
         # (\"<host>/<workspace>\" = \"*\") tracks whatever that workspace currently serves you;\n\
         # `topos login` adds one the first time this machine connects to a workspace, and never\n\
         # re-adds one you delete — a deleted line stays deleted. Only the rows here deliver.\n",
    );
    for (host, workspace) in workspaces {
        t.insert(&format!("{host}/{workspace}"), toml_edit::value("*"));
    }
    doc.insert("bundles", Item::Table(t));
    doc.to_string()
}

/// A commented, empty PROJECT template. Parses clean as [`ManifestScope::Project`].
pub(crate) fn project_template() -> String {
    "# topos.toml — the complete list of what this repo's agents use, identical for every\n\
     # contributor. Managed by the topos CLI; hand-edits are welcome. One line per entry\n\
     # under [bundles] — the key is the reference, the value a version or fields, e.g.\n\
     #\n\
     #   [bundles]\n\
     #   \"topos.sh/<workspace>/channels/<name>\" = \"*\"    # a workspace channel\n\
     #   \"github.com/<owner>/<repo>\" = \"*\"               # every skill in a repo\n\
     #   \"./tools/<dir>\" = { dest = [\".claude/skills\"] }  # a folder in this repo\n"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> String {
        "0123456789abcdef".repeat(4)
    }

    fn parse_global(text: &str) -> ManifestDoc {
        parse_manifest(text, ManifestScope::Global).unwrap()
    }

    /// The normative GLOBAL reference file. One adaptation from the ideal: TOML itself forbids
    /// a value and a section at one key path, so a workspace with a grouping section
    /// (`[bundles."topos.sh/acme"]`) cannot ALSO carry its feed row — the feeds here are other
    /// workspaces (the flat spelling combines a feed with explicit rows; see
    /// `a_feed_row_and_its_workspace_section_cannot_coexist`).
    fn global_reference_file() -> String {
        r#"# What this machine's agents get, beyond each workspace's feed.
# Managed by the topos CLI; hand-edits are welcome.

[bundles]
"topos.sh/beta" = "*"
"topos.example.com/platform" = "*"
"github.com/vercel-labs/skills" = "*"
"github.com/anthropics/skills/pdf-tools" = "8c1f0a2"
"~/dev/weather-server" = { kind = "mcp" }

[bundles."topos.sh/acme"]
perf-review = "*"
"channels/frontend" = "*"
noisy-skill = "off"
deploy-guide = {
version = "DIGEST",
dest = ["~/.claude/skills"],
}
db-conventions = { dest = ["~/.agents/skills", "~/.claude/knowledge"] }
"#
        .replace("DIGEST", &digest())
    }

    #[test]
    fn the_global_reference_file_parses_row_by_row() {
        let doc = parse_global(&global_reference_file());
        let refs: Vec<&str> = doc.rows.iter().map(|r| r.reference.as_str()).collect();
        assert_eq!(
            refs,
            [
                "topos.sh/beta",
                "topos.example.com/platform",
                "github.com/vercel-labs/skills",
                "github.com/anthropics/skills/pdf-tools",
                "~/dev/weather-server",
                "topos.sh/acme/perf-review",
                "topos.sh/acme/channels/frontend",
                "topos.sh/acme/noisy-skill",
                "topos.sh/acme/deploy-guide",
                "topos.sh/acme/db-conventions",
            ],
            "rows come back in file order"
        );

        // The two feed rows.
        for i in [0, 1] {
            assert!(matches!(doc.rows[i].shape, KeyShape::Feed { .. }), "{i}");
            assert_eq!(doc.rows[i].value, EntryValue::Star, "{i}");
        }
        // The repo set tracks the default branch; the 4-segment repo skill is pinned.
        assert!(matches!(doc.rows[2].shape, KeyShape::RepoSet { .. }));
        assert_eq!(doc.rows[2].value, EntryValue::Star);
        assert!(matches!(
            &doc.rows[3].shape,
            KeyShape::RepoSkill { skill, .. } if skill == "pdf-tools"
        ));
        assert_eq!(doc.rows[3].value, EntryValue::Pin("8c1f0a2".into()));
        // The local folder carries its kind — the one place the manifest records what a local
        // folder IS, and a closed vocabulary: `skill`, `mcp`.
        assert!(matches!(doc.rows[4].shape, KeyShape::LocalPath { .. }));
        assert_eq!(
            doc.rows[4].value,
            EntryValue::Fields(EntryFields {
                kind: Some("mcp".into()),
                ..EntryFields::default()
            })
        );
        // The sectioned workspace rows: bundle, channel (spelled by its tail), the off switch.
        assert!(matches!(
            &doc.rows[5].shape,
            KeyShape::WorkspaceBundle { bundle, .. } if bundle == "perf-review"
        ));
        assert!(matches!(
            &doc.rows[6].shape,
            KeyShape::Channel { channel, .. } if channel == "frontend"
        ));
        assert_eq!(doc.rows[7].value, EntryValue::Off);
        // The MULTILINE inline table (TOML 1.1) parses like the single-line spelling.
        assert_eq!(
            doc.rows[8].value,
            EntryValue::Fields(EntryFields {
                version: Some(digest()),
                dest: Some(vec!["~/.claude/skills".into()]),
                ..EntryFields::default()
            })
        );
        // A several-destination dest row.
        assert_eq!(
            doc.rows[9].value,
            EntryValue::Fields(EntryFields {
                dest: Some(vec![
                    "~/.agents/skills".into(),
                    "~/.claude/knowledge".into()
                ]),
                ..EntryFields::default()
            })
        );
    }

    #[test]
    fn the_project_reference_file_parses() {
        let text = r#"[bundles]
"topos.sh/acme/channels/backend" = "*"
"topos.sh/acme/code-review" = "DIGEST"
"github.com/vercel-labs/skills" = "*"
"github.com/mattpocock/skills/grill-me" = "*"
"./tools/release-checklist" = { dest = [".claude/skills", ".agents/skills"] }
"#
        .replace("DIGEST", &digest());
        let doc = parse_manifest(&text, ManifestScope::Project).unwrap();
        assert_eq!(doc.rows.len(), 5);
        assert!(matches!(doc.rows[0].shape, KeyShape::Channel { .. }));
        assert_eq!(doc.rows[1].value, EntryValue::Pin(digest()));
        assert!(matches!(doc.rows[2].shape, KeyShape::RepoSet { .. }));
        assert!(matches!(doc.rows[3].shape, KeyShape::RepoSkill { .. }));
        assert_eq!(
            doc.rows[4].value,
            EntryValue::Fields(EntryFields {
                dest: Some(vec![".claude/skills".into(), ".agents/skills".into()]),
                ..EntryFields::default()
            })
        );
    }

    #[test]
    fn feed_rows_and_off_are_global_only() {
        // A feed row in a project file teaches the repo-shaped alternative.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme\" = \"*\"\n",
            ManifestScope::Project,
        )
        .unwrap_err();
        assert!(e.message.contains("identical for every contributor"), "{e}");
        assert!(e.message.contains("channels/<name>"), "{e}");
        // `"off"` in a project file points at the global manifest.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/noisy\" = \"off\"\n",
            ManifestScope::Project,
        )
        .unwrap_err();
        assert!(e.message.contains("global manifest"), "{e}");
        // Both are fine in the global file.
        let doc = parse_global(
            "[bundles]\n\"topos.sh/acme\" = \"*\"\n\"topos.sh/acme/noisy\" = \"off\"\n",
        );
        assert_eq!(doc.rows[0].value, EntryValue::Star);
        assert_eq!(doc.rows[1].value, EntryValue::Off);
    }

    #[test]
    fn the_three_join_spellings_parse_identically() {
        let flat = parse_global("[bundles]\n\"topos.sh/acme/code-review\" = \"*\"\n");
        let host = parse_global("[bundles.\"topos.sh\"]\n\"acme/code-review\" = \"*\"\n");
        let ws = parse_global("[bundles.\"topos.sh/acme\"]\ncode-review = \"*\"\n");
        assert_eq!(flat.rows, host.rows);
        assert_eq!(flat.rows, ws.rows);
        assert_eq!(flat.rows[0].reference, "topos.sh/acme/code-review");
    }

    #[test]
    fn duplicate_joined_references_refuse() {
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = \"*\"\n\n[bundles.\"topos.sh/acme\"]\nx = \"*\"\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("spelled twice"), "{e}");
        assert_eq!(e.key.as_deref(), Some("topos.sh/acme/x"));
    }

    #[test]
    fn a_section_per_bundle_teaches_the_inline_table_form() {
        let e = parse_manifest(
            "[bundles.\"topos.sh/acme/deploy\"]\nversion = \"*\"\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("an entry is one line"), "{e}");
        assert!(e.message.contains("inline table"), "{e}");
    }

    #[test]
    fn a_feed_row_and_its_workspace_section_cannot_coexist() {
        // TOML itself rejects a value and a table at one key path — the parse refuses at the
        // TOML level, before any manifest rule runs.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme\" = \"*\"\n\n[bundles.\"topos.sh/acme\"]\nx = \"*\"\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("not valid TOML"), "{e}");
        // The FLAT spelling is how a feed and explicit rows combine.
        let doc = parse_global(
            "[bundles]\n\"topos.sh/acme\" = \"*\"\n\"topos.sh/acme/noisy\" = \"off\"\n",
        );
        assert_eq!(doc.rows.len(), 2);
    }

    #[test]
    fn value_shape_mismatches_refuse_typed() {
        let d = digest();
        // A feed with a pin.
        let e = parse_manifest(
            &format!("[bundles]\n\"topos.sh/acme\" = \"{d}\"\n"),
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("exactly `\"*\"`"), "{e}");
        // A channel with a version — string and field form both.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/channels/x\" = \"abc1234\"\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("channel takes no pin"), "{e}");
        let e = parse_manifest(
            &format!("[bundles]\n\"topos.sh/acme/channels/x\" = {{ version = \"{d}\" }}\n"),
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("drop `version`"), "{e}");
        // A 64-hex digest on a forge thing (commits are 7–40).
        let e = parse_manifest(
            &format!("[bundles]\n\"github.com/o/r/s\" = \"{d}\"\n"),
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("7 to 40"), "{e}");
        // A 7-hex commit on a workspace thing (digests are the full 64).
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = \"abc1234\"\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("64-character"), "{e}");
        // A local folder takes no pin at all.
        let e = parse_manifest(
            "[bundles]\n\"./tools/x\" = \"abc1234\"\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("no versions to pin"), "{e}");
        // Arrays and non-string scalars are typed refusals.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = [\"*\"]\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("array is not an entry value"), "{e}");
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = 3\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("number is not an entry value"), "{e}");
        // `off` never rides inside a table.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = { version = \"off\" }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("never a field"), "{e}");
        // `off` fits only workspace bundles.
        let e = parse_manifest(
            "[bundles]\n\"github.com/o/r/s\" = \"off\"\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("switches a workspace bundle"), "{e}");
    }

    #[test]
    fn key_shape_refusals_surface_through_the_parse() {
        // The 5-segment forge key points at the subdir escape.
        let e = parse_manifest(
            "[bundles]\n\"github.com/o/r/deep/nested-dir\" = \"*\"\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("subdir"), "{e}");
        // Unless the leaf reads as a FIELD on a valid parent reference — then the more precise
        // section-as-entry teaching wins.
        let e = parse_manifest(
            "[bundles.\"github.com/o/r/deep\"]\npath = \"x\"\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("an entry is one line"), "{e}");
        // gitlab refuses plainly.
        let e = parse_manifest(
            "[bundles]\n\"gitlab.com/o/r\" = \"*\"\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("not supported yet"), "{e}");
    }

    #[test]
    fn unknown_and_illegal_fields_refuse_typed() {
        // An unknown field names itself and the legal set for the shape.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = { destt = \"y\" }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("unknown field `destt`"), "{e}");
        assert!(e.message.contains("`version`, `dest`, `name`"), "{e}");
        // A known field on the wrong shape is its own refusal (subdir fits git things only).
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = { subdir = \"y\" }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("`subdir` does not fit"), "{e}");
        // A repo set takes `dest` only.
        let e = parse_manifest(
            "[bundles]\n\"github.com/o/r\" = { name = \"y\" }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("`name` does not fit a repo set"), "{e}");
        // A local row no longer takes a version — its folder has no versions to pin.
        let e = parse_manifest(
            "[bundles]\n\"./tools/x\" = { version = \"*\" }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("`version` does not fit"), "{e}");
        // A feed row takes no fields at all.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme\" = { dest = [\"~/x\"] }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("exactly `\"*\"`"), "{e}");
    }

    // -- the dest grammar ---------------------------------------------------

    #[test]
    fn an_empty_dest_refuses_toward_the_field_drop() {
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = { dest = [] }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(
            e.message.contains(
                "a dest names at least one destination; drop the field to reach every agent"
            ),
            "{e}"
        );
        assert!(!e.migration, "an empty dest is not a retired spelling");
        // A channel's second array refuses the same way, in its own word.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/channels/backend\" = { mcp_dest = [] }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(
            e.message.contains(
                "an mcp_dest names at least one destination; drop the field to reach every agent"
            ),
            "{e}"
        );
    }

    #[test]
    fn dest_entries_speak_the_scope_dialect() {
        // Machine file: `~/`-prefixed or absolute; a relative entry names itself and the rule.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = { dest = [\"skills\"] }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("dest entry `skills` is relative"), "{e}");
        assert!(e.message.contains("`~/`-prefixed or absolute"), "{e}");
        // Project file: relative and contained — absolute, `~`, and `..` all refuse.
        for entry in ["/abs/skills", "~/skills", "../out"] {
            let e = parse_manifest(
                &format!("[bundles]\n\"topos.sh/acme/x\" = {{ dest = [\"{entry}\"] }}\n"),
                ManifestScope::Project,
            )
            .unwrap_err();
            assert!(
                e.message
                    .contains(&format!("dest entry `{entry}` leaves the checkout")),
                "{entry}: {e}"
            );
        }
        // The legal spellings parse.
        parse_global("[bundles]\n\"topos.sh/acme/x\" = { dest = [\"~/.claude/skills\"] }\n");
        parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = { dest = [\".codex/skills\"] }\n",
            ManifestScope::Project,
        )
        .unwrap();
        // An empty string is not a destination.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = { dest = [\"\"] }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("non-empty"), "{e}");
    }

    #[test]
    fn a_local_mcp_rows_dest_must_name_a_known_config_file() {
        // Load-time: the row's kind is knowable, so an unknown file refuses with the list.
        let e = parse_manifest(
            "[bundles]\n\"./tools/linear\" = { dest = [\"~/.codex/config.yaml\"], kind = \"mcp\" }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(
            e.message
                .contains("dest entry `~/.codex/config.yaml` is not a known MCP config file"),
            "{e}"
        );
        assert!(e.message.contains("~/.codex/config.toml"), "{e}");
        // The known file parses — in both scopes, against the scope's own table.
        parse_global(
            "[bundles]\n\"./tools/linear\" = { dest = [\"~/.codex/config.toml\"], kind = \"mcp\" }\n",
        );
        parse_manifest(
            "[bundles]\n\"./tools/linear\" = { dest = [\".codex/config.toml\"], kind = \"mcp\" }\n",
            ManifestScope::Project,
        )
        .unwrap();
        // A skill folder is fine on a NON-mcp local row (free-form folders are legal).
        parse_global("[bundles]\n\"./tools/notes\" = { dest = [\"~/anywhere/at/all\"] }\n");
    }

    #[test]
    fn a_repo_row_cannot_carry_an_mcp_bundle() {
        // A repo SKILL row: `kind` is a legal field there, so the refusal is the kind VALUE's, and
        // it teaches where an MCP bundle comes from.
        for scope in [ManifestScope::Global, ManifestScope::Project] {
            let e = parse_manifest(
                "[bundles]\n\"github.com/o/r/tool\" = { version = \"*\", kind = \"mcp\" }\n",
                scope,
            )
            .unwrap_err();
            assert_eq!(e.key.as_deref(), Some("github.com/o/r/tool"), "{e}");
            assert!(
                e.message
                    .contains("`kind = \"mcp\"` does not fit a repo skill")
                    && e.message.contains("publish the bundle to a workspace"),
                "{e}"
            );
        }
        // A repo SET takes no `kind` at all — the field-legality refusal, unchanged.
        let e = parse_manifest(
            "[bundles]\n\"github.com/o/r\" = { kind = \"mcp\" }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("`kind` does not fit a repo set"), "{e}");
        // The mcp arm is reached FIRST, so its teaching survives; a repo skill naming any OTHER
        // unknown kind takes the vocabulary refusal below.
        let e = parse_manifest(
            "[bundles]\n\"github.com/o/r/tool\" = { kind = \"knowledge\" }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("known kinds: `skill`, `mcp`"), "{e}");
    }

    /// The kind VOCABULARY is closed in a hand-written file exactly as it is at the server door.
    /// A word this build does not own would otherwise parse into a demand and be delivered as a
    /// skill — the closed set is what makes that impossible, and the refusal names the whole
    /// vocabulary so the fix is readable without opening the docs.
    #[test]
    fn a_row_naming_a_kind_topos_does_not_deliver_refuses_at_load() {
        for scope in [ManifestScope::Global, ManifestScope::Project] {
            let e = parse_manifest(
                "[bundles]\n\"./tools/notes\" = { kind = \"knowledge\" }\n",
                scope,
            )
            .unwrap_err();
            assert_eq!(e.key.as_deref(), Some("./tools/notes"), "{e}");
            assert_eq!(
                e.message,
                "`kind = \"knowledge\"` on `./tools/notes` is not a kind topos delivers — known \
                 kinds: `skill`, `mcp`",
                "{e}"
            );
        }
        // Near-misses are words too: case and plurals name no kind either.
        for word in ["Skill", "MCP", "skills", ""] {
            parse_manifest(
                &format!("[bundles]\n\"./tools/notes\" = {{ kind = \"{word}\" }}\n"),
                ManifestScope::Global,
            )
            .expect_err(word);
        }
        // Both known kinds parse, and so does a row that spells no kind at all.
        parse_global("[bundles]\n\"./tools/notes\" = { kind = \"mcp\" }\n");
        parse_global("[bundles]\n\"./tools/notes\" = { kind = \"skill\" }\n");
        parse_global("[bundles]\n\"./tools/notes\" = \"*\"\n");
        parse_global("[bundles]\n\"./tools/notes\" = { dest = [\"~/.claude/skills\"] }\n");
        // A FILE WITH SEVERAL ROWS: the refusal names the row it is about, inline. Without the
        // reference in the message a reader is left grepping their own file for the bad word.
        let e = parse_manifest(
            "[bundles]\n\
             \"topos.sh/acme/deploy\" = \"*\"\n\
             \"./tools/notes\" = { kind = \"knowledge\" }\n\
             \"github.com/o/r\" = \"*\"\n\
             \"~/dev/runbooks\" = { kind = \"playbook\" }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("`./tools/notes`"), "{e}");
        assert_eq!(e.key.as_deref(), Some("./tools/notes"), "{e}");
    }

    // -- the retired spellings ----------------------------------------------

    #[test]
    fn a_retired_harness_field_teaches_the_exact_dest_rewrite() {
        // The GLOBAL file pairs the machine-scope config file (byte-exact).
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/linear\" = { harness = [\"codex\"] }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.migration);
        assert_eq!(
            e.message,
            "`harness = [\"codex\"]` on \"topos.sh/acme/linear\" is now written as dest — use \
             dest = [\"~/.codex/config.toml\"]"
        );
        // The PROJECT file pairs the project surface (scope-correct, byte-exact).
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/linear\" = { harness = [\"codex\"] }\n",
            ManifestScope::Project,
        )
        .unwrap_err();
        assert!(e.migration);
        assert_eq!(
            e.message,
            "`harness = [\"codex\"]` on \"topos.sh/acme/linear\" is now written as dest — use \
             dest = [\".codex/config.toml\"]"
        );
        // A forge row maps through the registry's skills roots instead.
        let e = parse_manifest(
            "[bundles]\n\"github.com/o/r\" = { harness = [\"codex\"] }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.migration);
        assert_eq!(
            e.message,
            "`harness = [\"codex\"]` on \"github.com/o/r\" is now written as dest — use \
             dest = [\"~/.codex/skills\"]"
        );
        // An unknown slug: rewrite what maps, name what doesn't.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/linear\" = { harness = [\"codex\", \"acme-cli\"] }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.migration);
        assert!(
            e.message.contains("use dest = [\"~/.codex/config.toml\"]"),
            "{e}"
        );
        assert!(
            e.message
                .contains("\"acme-cli\" maps to no destination here"),
            "{e}"
        );
    }

    #[test]
    fn a_retired_path_field_carries_its_value_into_dest() {
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = { path = \"x\" }\n",
            ManifestScope::Project,
        )
        .unwrap_err();
        assert!(e.migration);
        assert_eq!(
            e.message,
            "`path = \"x\"` on \"topos.sh/acme/x\" is now written as dest — use dest = [\"x\"]"
        );
        // A per-harness path table lists each directory.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = { path = { default = \"docs/ai/\", claude-code = \".claude/knowledge/\" } }\n",
            ManifestScope::Project,
        )
        .unwrap_err();
        assert!(e.migration);
        assert!(
            e.message
                .contains("use dest = [\"docs/ai/\", \".claude/knowledge/\"]"),
            "{e}"
        );
    }

    #[test]
    fn a_defaults_table_refuses_with_the_per_row_rewrite() {
        // `[defaults.mcp]` with `harness` and a local mcp row: the per-row rewrite.
        let e = parse_manifest(
            "[bundles]\n\"./tools/linear\" = { kind = \"mcp\" }\n\n\
             [defaults.mcp]\nharness = [\"codex\"]\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.migration);
        assert!(
            e.message
                .contains("on \"./tools/linear\" use dest = [\"~/.codex/config.toml\"]"),
            "{e}"
        );
        // No mcp rows: the general teaching with the descriptor file for each slug.
        let e = parse_manifest(
            "[defaults.mcp]\nharness = [\"cursor\"]\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.migration);
        assert!(e.message.contains("on each mcp row"), "{e}");
        assert!(e.message.contains("\"~/.cursor/mcp.json\""), "{e}");
        // Every other kind teaches dest-on-each-row, naming the table.
        let e = parse_manifest(
            "[defaults.skill]\npath = \".agents/skills\"\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.migration);
        assert!(
            e.message.contains("`[defaults.skill]` is no longer read"),
            "{e}"
        );
        assert!(e.message.contains("set on each row"), "{e}");
    }

    #[test]
    fn unknown_top_level_keys_refuse() {
        let e = parse_manifest("[skills]\n\"x\" = \"*\"\n", ManifestScope::Global).unwrap_err();
        assert!(e.message.contains("unknown top-level `skills`"), "{e}");
        let e = parse_manifest("exclude = []\n", ManifestScope::Global).unwrap_err();
        assert!(e.message.contains("unknown top-level `exclude`"), "{e}");
    }

    // -- the editor ---------------------------------------------------------

    #[test]
    fn set_then_remove_restores_the_file_byte_for_byte() {
        let d = digest();
        let files = [
            String::new(),
            "# the machine's manifest\n".to_string(),
            "[bundles]\n\"topos.sh/acme\" = \"*\"\n".to_string(),
            "# header\n\n[bundles]\n# why this repo set matters\n\"github.com/o/r\" = \"*\"\n"
                .to_string(),
            "[bundles.\"topos.sh/acme\"]\ndeploy = \"*\"\n".to_string(),
            "[bundles]\n\"topos.sh/beta\" = \"*\"\n\"topos.sh/beta/x\" = { dest = [\"~/.claude/skills\"] }\n"
                .to_string(),
        ];
        let values = [
            ("topos.sh/gamma/new-skill", EntryValue::Star),
            ("topos.sh/acme/new-skill", EntryValue::Pin(d.clone())),
            ("github.com/new/repo", EntryValue::Star),
            (
                "./tools/x",
                EntryValue::Fields(EntryFields {
                    dest: Some(vec!["~/.claude/skills".into()]),
                    ..EntryFields::default()
                }),
            ),
        ];
        for file in &files {
            for (reference, value) in &values {
                let mut ed = ManifestEditor::open(file, ManifestScope::Global).unwrap();
                assert!(
                    ed.row(reference).is_none(),
                    "corpus rows must not pre-exist"
                );
                ed.set_row(reference, value).unwrap();
                assert!(ed.row(reference).is_some());
                assert!(ed.remove_row(reference));
                assert_eq!(&ed.rendered(), file, "file={file:?} ref={reference}");
            }
        }
    }

    #[test]
    fn remove_then_set_restores_the_file_byte_for_byte() {
        let d = digest();
        let cases: Vec<(String, &str, EntryValue)> = vec![
            (
                "[bundles]\n\"topos.sh/acme/deploy\" = \"*\"\n".to_string(),
                "topos.sh/acme/deploy",
                EntryValue::Star,
            ),
            (
                format!(
                    "[bundles]\n\"topos.sh/acme\" = \"*\"\n\"topos.sh/acme/deploy\" = \"{d}\"\n"
                ),
                "topos.sh/acme/deploy",
                EntryValue::Pin(d.clone()),
            ),
            (
                // The preexisting section survives the remove (empty grouping is harmless), and
                // the re-add lands the tail key back inside it.
                "[bundles.\"topos.sh/acme\"]\ndeploy = \"*\"\n".to_string(),
                "topos.sh/acme/deploy",
                EntryValue::Star,
            ),
            (
                "[bundles.\"topos.sh/acme\"]\n\"channels/frontend\" = \"*\"\n".to_string(),
                "topos.sh/acme/channels/frontend",
                EntryValue::Star,
            ),
        ];
        for (file, reference, value) in &cases {
            let mut ed = ManifestEditor::open(file, ManifestScope::Global).unwrap();
            assert!(ed.remove_row(reference), "row pre-exists in {file:?}");
            ed.set_row(reference, value).unwrap();
            assert_eq!(&ed.rendered(), file, "file={file:?} ref={reference}");
        }
    }

    #[test]
    fn the_editor_spells_values_canonically() {
        // The deterministic spelling the inverse property is defined against — canonical field
        // order: version, dest, mcp_dest, name, subdir, kind.
        let mut ed = ManifestEditor::open_or_new(ManifestScope::Global);
        ed.set_row(
            "github.com/o/r/tools",
            &EntryValue::Fields(EntryFields {
                version: Some("8c1f0a2".into()),
                dest: Some(vec!["~/.claude/skills".into(), "~/.codex/skills".into()]),
                mcp_dest: None,
                name: Some("tooling".into()),
                subdir: Some("skills/tools".into()),
                kind: Some("skill".into()),
            }),
        )
        .unwrap();
        assert_eq!(
            ed.rendered(),
            "[bundles]\n\"github.com/o/r/tools\" = { version = \"8c1f0a2\", dest = \
             [\"~/.claude/skills\", \"~/.codex/skills\"], name = \"tooling\", subdir = \
             \"skills/tools\", kind = \"skill\" }\n"
        );
        // A channel spells BOTH arrays, in that order — `dest` for its skill members' folders,
        // `mcp_dest` for its mcp members' config files.
        let mut ed = ManifestEditor::open_or_new(ManifestScope::Global);
        ed.set_row(
            "topos.sh/acme/channels/backend",
            &EntryValue::Fields(EntryFields {
                dest: Some(vec!["~/.claude/skills".into()]),
                mcp_dest: Some(vec!["~/.cursor/mcp.json".into()]),
                ..EntryFields::default()
            }),
        )
        .unwrap();
        assert_eq!(
            ed.rendered(),
            "[bundles]\n\"topos.sh/acme/channels/backend\" = { dest = [\"~/.claude/skills\"], \
             mcp_dest = [\"~/.cursor/mcp.json\"] }\n"
        );
    }

    #[test]
    fn set_row_lands_in_an_existing_workspace_section() {
        let file = "[bundles.\"topos.sh/acme\"]\ndeploy = \"*\"\n";
        let mut ed = ManifestEditor::open(file, ManifestScope::Global).unwrap();
        ed.set_row("topos.sh/acme/perf-review", &EntryValue::Star)
            .unwrap();
        ed.set_row("topos.sh/acme/channels/backend", &EntryValue::Star)
            .unwrap();
        // A different workspace has no section — it lands flat.
        ed.set_row("topos.sh/beta/other", &EntryValue::Star)
            .unwrap();
        let text = ed.rendered();
        // (The original file had no explicit `[bundles]` header, so the flat row makes the
        // implicit table render its header directly above the section — minimal-edit output;
        // `fmt` owns the cosmetics.)
        assert_eq!(
            text,
            "[bundles]\n\"topos.sh/beta/other\" = \"*\"\n[bundles.\"topos.sh/acme\"]\n\
             deploy = \"*\"\nperf-review = \"*\"\n\"channels/backend\" = \"*\"\n"
        );
        // And the parse reads all four rows back.
        assert_eq!(parse_global(&text).rows.len(), 4);
    }

    #[test]
    fn set_row_replaces_in_place_wherever_spelled() {
        let d = digest();
        let file = "[bundles.\"topos.sh/acme\"]\n# pinned during the incident\ndeploy = \"*\"\n";
        let mut ed = ManifestEditor::open(file, ManifestScope::Global).unwrap();
        ed.set_row("topos.sh/acme/deploy", &EntryValue::Pin(d.clone()))
            .unwrap();
        let text = ed.rendered();
        // The row stays in its section, under its comment — only the value changed.
        assert_eq!(
            text,
            format!(
                "[bundles.\"topos.sh/acme\"]\n# pinned during the incident\ndeploy = \"{d}\"\n"
            )
        );
    }

    #[test]
    fn remove_row_keeps_a_preceding_standalone_comment() {
        let file = "[bundles]\n# keep me\n\"topos.sh/acme/deploy\" = \"*\"\n\"topos.sh/acme/stay\" = \"*\"\n";
        let mut ed = ManifestEditor::open(file, ManifestScope::Global).unwrap();
        assert!(ed.remove_row("topos.sh/acme/deploy"));
        let text = ed.rendered();
        assert!(text.contains("# keep me"), "{text}");
        assert!(text.contains("stay"), "{text}");
        assert!(!text.contains("deploy"), "{text}");
        // Removing the LAST row still keeps the comment (it moves to the tail).
        let file = "[bundles]\n\"topos.sh/acme/stay\" = \"*\"\n# keep me too\n\"topos.sh/acme/deploy\" = \"*\"\n";
        let mut ed = ManifestEditor::open(file, ManifestScope::Global).unwrap();
        assert!(ed.remove_row("topos.sh/acme/deploy"));
        assert!(ed.rendered().contains("# keep me too"), "{}", ed.rendered());
    }

    #[test]
    fn remove_row_never_deletes_a_preexisting_section_header() {
        let file = "[bundles.\"topos.sh/acme\"]\ndeploy = \"*\"\n";
        let mut ed = ManifestEditor::open(file, ManifestScope::Global).unwrap();
        assert!(ed.remove_row("topos.sh/acme/deploy"));
        assert_eq!(ed.rendered(), "[bundles.\"topos.sh/acme\"]\n");
    }

    #[test]
    fn a_feed_row_refuses_while_its_workspace_section_stands() {
        let file = "[bundles.\"topos.sh/acme\"]\ndeploy = \"*\"\n";
        let mut ed = ManifestEditor::open(file, ManifestScope::Global).unwrap();
        let e = ed.set_row("topos.sh/acme", &EntryValue::Star).unwrap_err();
        assert!(e.message.contains("flat"), "{e}");
        // The section is untouched.
        assert_eq!(ed.rendered(), file);
        // A feed for an UNGROUPED workspace lands flat, fine.
        ed.set_row("topos.sh/beta", &EntryValue::Star).unwrap();
        assert!(ed.rendered().contains("\"topos.sh/beta\" = \"*\""));
    }

    #[test]
    fn the_editor_refuses_what_the_parser_would_refuse() {
        let mut ed = ManifestEditor::open_or_new(ManifestScope::Project);
        // Feed rows and `off` are global-only — the editor holds the same line.
        assert!(ed.set_row("topos.sh/acme", &EntryValue::Star).is_err());
        assert!(
            ed.set_row("topos.sh/acme/x", &EntryValue::Off)
                .unwrap_err()
                .message
                .contains("global manifest")
        );
        // Shape/value mismatches refuse before anything lands.
        let mut ed = ManifestEditor::open_or_new(ManifestScope::Global);
        assert!(
            ed.set_row("topos.sh/acme/x", &EntryValue::Pin("abc1234".into()))
                .is_err()
        );
        assert!(
            ed.set_row(
                "github.com/o/r",
                &EntryValue::Fields(EntryFields {
                    name: Some("x".into()),
                    ..EntryFields::default()
                })
            )
            .is_err()
        );
        assert!(ed.set_row("not a ref", &EntryValue::Star).is_err());
        assert_eq!(ed.rendered(), "");
    }

    #[test]
    fn open_validates_the_whole_document() {
        assert!(ManifestEditor::open("[bundles\n", ManifestScope::Global).is_err());
        assert!(
            ManifestEditor::open(
                "[bundles]\n\"topos.sh/acme/x\" = 3\n",
                ManifestScope::Global
            )
            .is_err()
        );
        assert!(ManifestEditor::open("[stray]\nx = 1\n", ManifestScope::Global).is_err());
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

        let mut ed = ManifestEditor::open_or_new(ManifestScope::Global);
        ed.set_row("topos.sh/acme", &EntryValue::Star).unwrap();
        ed.write(&RealFs, &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, ed.rendered());
        // And the round trip re-opens clean.
        assert!(ManifestEditor::open(&text, ManifestScope::Global).is_ok());
    }

    // -- file birth ---------------------------------------------------------

    #[test]
    fn the_materialized_global_file_parses_clean() {
        let text = materialized_global(&[
            ("topos.sh".to_string(), "acme".to_string()),
            ("topos.example.com".to_string(), "platform".to_string()),
        ]);
        assert!(text.starts_with("# topos.toml"), "{text}");
        let doc = parse_global(&text);
        assert_eq!(doc.rows.len(), 2);
        assert!(matches!(doc.rows[0].shape, KeyShape::Feed { .. }));
        assert_eq!(doc.rows[0].reference, "topos.sh/acme");
        assert_eq!(doc.rows[1].reference, "topos.example.com/platform");
        for row in &doc.rows {
            assert_eq!(row.value, EntryValue::Star);
        }
    }

    #[test]
    fn the_project_template_parses_clean_and_empty() {
        let text = project_template();
        let doc = parse_manifest(&text, ManifestScope::Project).unwrap();
        assert!(doc.rows.is_empty());
        assert!(text.starts_with("# topos.toml"), "{text}");
    }
}
