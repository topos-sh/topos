//! The NEW `topos.toml` document — one `[bundles]` namespace plus `[defaults.<kind>]` sections.
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
//! joining to one reference are a parse error; a top-level table other than `bundles`/`defaults`
//! refuses too — this file is a complete recipe, and a typo must not silently drop demand.
//!
//! Values are validated BY SHAPE ([`KeyShape`]): things take a version (`"*"`, a 64-hex digest
//! for workspace bundles, a 7–40-hex commit for forge things) or fields; a channel takes no
//! `version`; a repo set takes `"*"`/a commit; the feed takes exactly `"*"`. `"off"` — the
//! per-bundle switch — is legal only on a workspace bundle row in the GLOBAL file, and feed rows
//! are global-only too (a project manifest is a repo fact, identical for every contributor).
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

/// A `path` field: one directory for every harness, or a per-harness table with an optional
/// `default` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathSpec {
    One(String),
    PerHarness {
        default: Option<String>,
        per: Vec<(String, String)>,
    },
}

/// The inline-table form of an entry value — every field optional, legality decided per shape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EntryFields {
    pub version: Option<String>,
    pub path: Option<PathSpec>,
    pub harness: Option<Vec<String>>,
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

/// One `[defaults.<kind>]` section — kind-level defaults carry ONLY `harness` and `path`
/// (a version, name, or subdir is incoherent kind-wide).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct KindDefaults {
    pub harness: Option<Vec<String>>,
    pub path: Option<PathSpec>,
}

/// A parsed manifest: rows in file order, then the kind defaults in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ManifestDoc {
    pub rows: Vec<BundleRow>,
    pub defaults: Vec<(String, KindDefaults)>,
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

/// The complete field vocabulary — what a key must be to count as a mis-shelved FIELD (the
/// section-as-entry teaching) rather than an unknown word.
const FIELD_NAMES: [&str; 6] = ["version", "path", "harness", "name", "subdir", "kind"];

/// The fields legal on each shape. The feed takes none (its value is exactly `"*"`).
fn legal_fields(shape: &KeyShape) -> &'static [&'static str] {
    match shape {
        KeyShape::WorkspaceBundle { .. } => &["version", "path", "harness", "name"],
        KeyShape::RepoSkill { .. } => &["version", "path", "harness", "name", "subdir", "kind"],
        KeyShape::LocalPath { .. } => &["version", "path", "harness", "name", "kind"],
        KeyShape::RepoSet { .. } => &["harness"],
        KeyShape::Channel { .. } => &["path", "harness"],
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
        EntryValue::Fields(f) => fields_check(reference, shape, f),
    }
}

/// The field-legality half, on an already-typed [`EntryFields`].
fn fields_check(reference: &str, shape: &KeyShape, f: &EntryFields) -> Result<(), ManifestError> {
    let legal = legal_fields(shape);
    let present: [(&str, bool); 6] = [
        ("version", f.version.is_some()),
        ("path", f.path.is_some()),
        ("harness", f.harness.is_some()),
        ("name", f.name.is_some()),
        ("subdir", f.subdir.is_some()),
        ("kind", f.kind.is_some()),
    ];
    for (field, is_present) in present {
        if is_present && !legal.contains(&field) {
            return Err(illegal_field(reference, shape, field));
        }
    }
    if let Some(v) = &f.version {
        if v == "off" {
            return Err(off_in_table(reference));
        }
        if v != "*" {
            pin_check(reference, shape, v)?;
        }
    }
    if let Some(h) = &f.harness
        && h.iter().any(|x| x == "default")
    {
        return Err(default_in_harness(reference));
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

fn off_in_table(reference: &str) -> ManifestError {
    at(
        reference,
        "`off` is a whole value (`\"<ref>\" = \"off\"`) — never a field",
    )
}

fn default_in_harness(reference: &str) -> ManifestError {
    at(
        reference,
        "`default` is reserved — in a `path` table it names the default directory; it is never \
         a harness",
    )
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// A uniform view over the two TOML table spellings (a section [`Table`], an inline table) and
/// plain values — so `path`/`harness`/defaults read identically however they were written.
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
                "unknown top-level `{key}` — a manifest holds `[bundles]` and \
                 `[defaults.<kind>]` only; a typo here would silently drop what it names, so it \
                 refuses instead",
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
    let defaults = match doc.get("defaults") {
        Some(item) => parse_defaults(item)?,
        None => Vec::new(),
    };
    Ok(ManifestDoc { rows, defaults })
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
            && FIELD_NAMES.contains(&leaf)
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
            let fields = fields_of(reference, shape, t)?;
            fields_check(reference, shape, &fields)?;
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

/// Read an inline table's fields, refusing unknown keys and per-field type faults; the
/// per-shape legality runs after, in [`fields_check`].
fn fields_of(
    reference: &str,
    shape: &KeyShape,
    t: &InlineTable,
) -> Result<EntryFields, ManifestError> {
    if matches!(shape, KeyShape::Feed { .. }) {
        return Err(feed_exact_star(reference));
    }
    let legal = legal_fields(shape);
    let mut f = EntryFields::default();
    for (k, v) in t.iter() {
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
            "path" => f.path = Some(path_spec_of(reference, &Node::Value(v))?),
            "harness" => f.harness = Some(harness_of(reference, &Node::Value(v))?),
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

fn path_spec_of(reference: &str, node: &Node<'_>) -> Result<PathSpec, ManifestError> {
    if let Some(s) = node.as_str() {
        return Ok(PathSpec::One(s.to_string()));
    }
    if let Some(pairs) = node.pairs() {
        let mut default = None;
        let mut per = Vec::new();
        for (k, v) in pairs {
            let s = v
                .as_str()
                .ok_or_else(|| at(reference, "`path` directories are strings"))?;
            if k == "default" {
                default = Some(s.to_string());
            } else {
                per.push((k.to_string(), s.to_string()));
            }
        }
        return Ok(PathSpec::PerHarness { default, per });
    }
    Err(at(
        reference,
        "`path` is a directory string, or a table `{ default = \"…\", <harness> = \"…\" }`",
    ))
}

fn harness_of(reference: &str, node: &Node<'_>) -> Result<Vec<String>, ManifestError> {
    let arr = node.as_array().ok_or_else(|| {
        at(
            reference,
            "`harness` is an array of harness names (e.g. `[\"claude-code\"]`)",
        )
    })?;
    let mut out = Vec::new();
    for v in arr.iter() {
        let s = v.as_str().ok_or_else(|| {
            at(
                reference,
                "`harness` is an array of harness names (e.g. `[\"claude-code\"]`)",
            )
        })?;
        if s == "default" {
            return Err(default_in_harness(reference));
        }
        out.push(s.to_string());
    }
    Ok(out)
}

fn parse_defaults(item: &Item) -> Result<Vec<(String, KindDefaults)>, ManifestError> {
    let pairs = Node::Item(item).pairs().ok_or_else(|| {
        plain("`[defaults]` holds `[defaults.<kind>]` sections, each carrying `harness` and `path`")
    })?;
    let mut out = Vec::new();
    for (kind, node) in pairs {
        let context = format!("defaults.{kind}");
        let kpairs = node.pairs().ok_or_else(|| {
            plain(format!(
                "`defaults.{kind}` is a section — spell it `[defaults.{kind}]` with `harness` \
                 and `path` inside",
            ))
        })?;
        let mut kd = KindDefaults::default();
        for (k, v) in kpairs {
            match k {
                "harness" => kd.harness = Some(harness_of(&context, &v)?),
                "path" => kd.path = Some(path_spec_of(&context, &v)?),
                "version" | "name" | "subdir" => {
                    return Err(plain(format!(
                        "a kind-wide default cannot carry `{k}` — `{k}` is a per-bundle fact; \
                         set it on the bundle's own row (`[defaults.{kind}]` takes `harness` \
                         and `path`)",
                    )));
                }
                _ => {
                    return Err(plain(format!(
                        "unknown default `{k}` — `[defaults.<kind>]` takes `harness` and `path`",
                    )));
                }
            }
        }
        out.push((kind.to_string(), kd));
    }
    Ok(out)
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
        })
    }

    /// A fresh, empty document (no file yet).
    pub(crate) fn open_or_new(scope: ManifestScope) -> Self {
        Self {
            doc: DocumentMut::new(),
            scope,
            preexisting: HashSet::new(),
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

    /// Persist atomically through the one crash-safe write.
    ///
    /// # Errors
    /// Propagates the underlying filesystem failure.
    pub(crate) fn write(&self, fs: &dyn FsOps, path: &Path) -> Result<(), ClientError> {
        crate::atomic::atomic_write(fs, path, self.rendered().as_bytes())
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
/// table in canonical order (version, path, harness, name, subdir, kind).
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
            if let Some(p) = &f.path {
                t.insert("path", path_value(p));
            }
            if let Some(h) = &f.harness {
                let mut a = Array::new();
                for x in h {
                    a.push(x.as_str());
                }
                t.insert("harness", Value::Array(a));
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

pub(crate) fn path_value(p: &PathSpec) -> Value {
    match p {
        PathSpec::One(s) => s.as_str().into(),
        PathSpec::PerHarness { default, per } => {
            let mut t = InlineTable::new();
            if let Some(d) = default {
                t.insert("default", d.as_str().into());
            }
            for (k, v) in per {
                t.insert(k, v.as_str().into());
            }
            Value::InlineTable(t)
        }
    }
}

// ---------------------------------------------------------------------------
// File birth
// ---------------------------------------------------------------------------

/// The GLOBAL file's birth content: the header states the contract, then one feed row per
/// connected `(host, workspace)`. Parses clean as [`ManifestScope::Global`].
pub(crate) fn materialized_global(workspaces: &[(String, String)]) -> String {
    let mut doc = DocumentMut::new();
    let mut t = Table::new();
    t.set_implicit(false);
    t.decor_mut().set_prefix(
        "# topos.toml — the complete recipe for what lands on this machine's personal scope.\n\
         # Managed by the topos CLI; hand-edits are welcome. Each feed row below tracks whatever\n\
         # that workspace currently serves you — with no file here at all, these feed rows are\n\
         # exactly what happens anyway. Delete a feed row to take explicit, line-by-line control\n\
         # of that workspace.\n",
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
     #   \"./tools/<dir>\" = { harness = [\"claude-code\"] }  # a folder in this repo\n"
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
"~/dev/notes" = { kind = "knowledge" }

[bundles."topos.sh/acme"]
perf-review = "*"
"channels/frontend" = "*"
noisy-skill = "off"
deploy-guide = {
version = "DIGEST",
harness = ["claude-code"],
}
db-conventions = { path = { default = "docs/ai/", claude-code = ".claude/knowledge/" } }

[defaults.knowledge]
path = { default = "docs/ai/" }
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
                "~/dev/notes",
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
        // The local folder carries its kind.
        assert!(matches!(doc.rows[4].shape, KeyShape::LocalPath { .. }));
        assert_eq!(
            doc.rows[4].value,
            EntryValue::Fields(EntryFields {
                kind: Some("knowledge".into()),
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
                harness: Some(vec!["claude-code".into()]),
                ..EntryFields::default()
            })
        );
        // The per-harness path table.
        assert_eq!(
            doc.rows[9].value,
            EntryValue::Fields(EntryFields {
                path: Some(PathSpec::PerHarness {
                    default: Some("docs/ai/".into()),
                    per: vec![("claude-code".into(), ".claude/knowledge/".into())],
                }),
                ..EntryFields::default()
            })
        );
        // The kind defaults.
        assert_eq!(
            doc.defaults,
            vec![(
                "knowledge".to_string(),
                KindDefaults {
                    harness: None,
                    path: Some(PathSpec::PerHarness {
                        default: Some("docs/ai/".into()),
                        per: vec![],
                    }),
                }
            )]
        );
    }

    #[test]
    fn the_project_reference_file_parses() {
        let text = r#"[bundles]
"topos.sh/acme/channels/backend" = "*"
"topos.sh/acme/code-review" = "DIGEST"
"github.com/vercel-labs/skills" = "*"
"github.com/mattpocock/skills/grill-me" = "*"
"./tools/release-checklist" = { harness = ["claude-code", "codex"] }

[defaults.skill]
path = { default = ".agents/skills", claude-code = ".claude/skills" }
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
                harness: Some(vec!["claude-code".into(), "codex".into()]),
                ..EntryFields::default()
            })
        );
        assert_eq!(doc.defaults[0].0, "skill");
        assert_eq!(
            doc.defaults[0].1.path,
            Some(PathSpec::PerHarness {
                default: Some(".agents/skills".into()),
                per: vec![("claude-code".into(), ".claude/skills".into())],
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
            "[bundles]\n\"topos.sh/acme/x\" = { pathh = \"y\" }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("unknown field `pathh`"), "{e}");
        assert!(
            e.message.contains("`version`, `path`, `harness`, `name`"),
            "{e}"
        );
        // A known field on the wrong shape is its own refusal (subdir fits git things only).
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = { subdir = \"y\" }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("`subdir` does not fit"), "{e}");
        // A repo set takes `harness` only.
        let e = parse_manifest(
            "[bundles]\n\"github.com/o/r\" = { name = \"y\" }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("`name` does not fit a repo set"), "{e}");
        // `default` is never a harness.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme/x\" = { harness = [\"default\"] }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("`default` is reserved"), "{e}");
        // A feed row takes no fields at all.
        let e = parse_manifest(
            "[bundles]\n\"topos.sh/acme\" = { harness = [\"x\"] }\n",
            ManifestScope::Global,
        )
        .unwrap_err();
        assert!(e.message.contains("exactly `\"*\"`"), "{e}");
    }

    #[test]
    fn kind_defaults_take_harness_and_path_only() {
        // version / name / subdir are incoherent kind-wide.
        for field in ["version", "name", "subdir"] {
            let e = parse_manifest(
                &format!("[defaults.skill]\n{field} = \"x\"\n"),
                ManifestScope::Global,
            )
            .unwrap_err();
            assert!(e.message.contains("kind-wide default"), "{field}: {e}");
        }
        // Anything else is unknown.
        let e =
            parse_manifest("[defaults.skill]\npin = \"x\"\n", ManifestScope::Global).unwrap_err();
        assert!(e.message.contains("unknown default `pin`"), "{e}");
        // The two legal keys parse.
        let doc = parse_global(
            "[defaults.skill]\nharness = [\"claude-code\"]\npath = \".agents/skills\"\n",
        );
        assert_eq!(
            doc.defaults,
            vec![(
                "skill".to_string(),
                KindDefaults {
                    harness: Some(vec!["claude-code".into()]),
                    path: Some(PathSpec::One(".agents/skills".into())),
                }
            )]
        );
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
            "[bundles]\n\"topos.sh/beta\" = \"*\"\n\n[defaults.skill]\npath = \".agents/skills\"\n"
                .to_string(),
        ];
        let values = [
            ("topos.sh/gamma/new-skill", EntryValue::Star),
            ("topos.sh/acme/new-skill", EntryValue::Pin(d.clone())),
            ("github.com/new/repo", EntryValue::Star),
            (
                "./tools/x",
                EntryValue::Fields(EntryFields {
                    harness: Some(vec!["claude-code".into()]),
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
        // The deterministic spelling the inverse property is defined against.
        let mut ed = ManifestEditor::open_or_new(ManifestScope::Global);
        ed.set_row(
            "./tools/x",
            &EntryValue::Fields(EntryFields {
                version: Some("*".into()),
                path: Some(PathSpec::PerHarness {
                    default: Some("docs/ai/".into()),
                    per: vec![("claude-code".into(), ".claude/knowledge/".into())],
                }),
                harness: Some(vec!["claude-code".into()]),
                kind: Some("knowledge".into()),
                ..EntryFields::default()
            }),
        )
        .unwrap();
        assert_eq!(
            ed.rendered(),
            "[bundles]\n\"./tools/x\" = { version = \"*\", path = { default = \"docs/ai/\", \
             claude-code = \".claude/knowledge/\" }, harness = [\"claude-code\"], kind = \
             \"knowledge\" }\n"
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
        assert!(doc.defaults.is_empty());
        assert!(text.starts_with("# topos.toml"), "{text}");
    }
}
