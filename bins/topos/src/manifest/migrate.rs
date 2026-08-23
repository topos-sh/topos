//! ONE-SHOT v1 → v2 migration — the pre-schema `[bundles]` file rewritten into the sectioned
//! grammar, once, by the sweep.
//!
//! A v1 file is recognized by its top-level `[bundles]` table (the v2 grammar never writes one).
//! The mapping is total where the FILE decides, and honest where it cannot:
//!
//! - a feed row (`<host>/<ws>` = `"*"`) → `[workspaces]` `"<host>/<ws>" = "latest"`;
//! - a workspace bundle → `[skills]` or `[mcp]` — the row's own `kind` field first, else the
//!   delivery cache's word (the caller passes that lookup in), else `[skills]` with a note;
//! - a channel → `[channels]`;
//! - a repo skill (`github.com/<o>/<r>/<skill>`) → `<skill> = "github:<o>/<r>[#pin]"`;
//! - a local path key → `<leaf> = "<path>"` (the v2 key is the NAME; the path moves into the
//!   value);
//! - a repo SET row (`github.com/<o>/<r>`) has NO v2 spelling — dropped with a note naming the
//!   re-add (`topos add` expands a repo to per-skill rows now);
//! - `"*"` → `"latest"`; pins and `"off"` carry; `dest`/`mcp_dest`/`subdir` fields carry; a
//!   `name` field on a repo/path row becomes the v2 KEY (that is what the key means now).
//!
//! A PROJECT file migrates only when its workspace rows agree on ONE workspace — that is the v2
//! rule, not the migration's; several workspaces refuse with the file left untouched. Comments
//! do not survive (only the file's leading comment block is carried); pre-1.0, unceremonious.
//!
//! This module is PURE text→text; the sweep owns the lock, the re-read, and the atomic write.

use std::collections::BTreeMap;

use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

use crate::bundle_kind::BundleKind;
use crate::manifest::document::ManifestScope;
use crate::manifest::keys::{KeyShape, classify_key};

/// The rewritten file plus the notes a receipt surfaces (dropped rows, guessed kinds).
#[derive(Debug)]
pub(crate) struct Migration {
    pub text: String,
    pub notes: Vec<String>,
}

/// Whether `text` is a v1 manifest — parseable TOML whose top level holds a `[bundles]` table.
/// Anything unparseable is NOT v1 (the ordinary parse owns that refusal).
pub(crate) fn is_v1(text: &str) -> bool {
    text.parse::<DocumentMut>()
        .map(|doc| doc.get("bundles").is_some_and(|i| i.as_table().is_some()))
        .unwrap_or(false)
}

/// One flattened v1 row: the joined reference and its raw value.
struct V1Row {
    reference: String,
    value: Value,
}

/// Flatten `[bundles]` (and its grouping sub-tables) into joined-reference rows.
fn flatten(table: &Table, prefix: &mut Vec<String>, out: &mut Vec<V1Row>) {
    for (key, item) in table.iter() {
        match item {
            Item::Table(t) => {
                prefix.push(key.to_string());
                flatten(t, prefix, out);
                prefix.pop();
            }
            Item::Value(v) => {
                let mut segments = prefix.clone();
                segments.push(key.to_string());
                out.push(V1Row {
                    reference: segments.join("/"),
                    value: v.clone(),
                });
            }
            Item::None | Item::ArrayOfTables(_) => {}
        }
    }
}

/// The v1 fields a row's inline table may carry, read leniently (migration never refuses on a
/// field it can carry or drop with a note).
#[derive(Default)]
struct V1Fields {
    version: Option<String>,
    dest: Option<Array>,
    mcp_dest: Option<Array>,
    name: Option<String>,
    subdir: Option<String>,
    kind: Option<String>,
}

fn read_fields(v: &Value) -> (Option<String>, V1Fields) {
    match v {
        Value::String(s) => (Some(s.value().clone()), V1Fields::default()),
        Value::InlineTable(t) => {
            let mut f = V1Fields::default();
            for (k, val) in t.iter() {
                match k {
                    "version" => f.version = val.as_str().map(str::to_string),
                    "dest" => f.dest = val.as_array().cloned(),
                    "mcp_dest" => f.mcp_dest = val.as_array().cloned(),
                    "name" => f.name = val.as_str().map(str::to_string),
                    "subdir" => f.subdir = val.as_str().map(str::to_string),
                    "kind" => f.kind = val.as_str().map(str::to_string),
                    _ => {}
                }
            }
            (None, f)
        }
        _ => (None, V1Fields::default()),
    }
}

/// Which v2 section a migrated row lands in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    Skills,
    Mcp,
    Channels,
    Workspaces,
}

impl Section {
    fn header(self) -> &'static str {
        match self {
            Section::Skills => "skills",
            Section::Mcp => "mcp",
            Section::Channels => "channels",
            Section::Workspaces => "workspaces",
        }
    }
}

/// Migrate a v1 manifest text to the v2 grammar.
///
/// `kind_of(host, workspace, bundle)` answers a workspace bundle's kind from local knowledge
/// (the delivery cache); `None` files the row under `[skills]` with a note.
///
/// # Errors
/// One human sentence when the file cannot migrate mechanically (a project file naming several
/// workspaces); the caller leaves the file untouched and surfaces it.
pub(crate) fn migrate_v1(
    text: &str,
    scope: ManifestScope,
    kind_of: &dyn Fn(&str, &str, &str) -> Option<BundleKind>,
) -> Result<Migration, String> {
    let doc: DocumentMut = text
        .parse()
        .map_err(|e| format!("not valid TOML, so it cannot be migrated: {e}"))?;
    let Some(bundles) = doc.get("bundles").and_then(Item::as_table) else {
        return Err("no `[bundles]` table — not a v1 file".to_string());
    };

    let mut rows = Vec::new();
    flatten(bundles, &mut Vec::new(), &mut rows);

    let mut notes: Vec<String> = Vec::new();
    // (section, key) → value; BTreeMap gives the deterministic order the lock format also uses.
    let mut out_rows: BTreeMap<(Section, String), Value> = BTreeMap::new();
    // The distinct workspaces the file's workspace rows name (project one-workspace rule).
    let mut workspaces_seen: Vec<(String, String)> = Vec::new();

    let mut push =
        |section: Section, key: String, value: Value, notes: &mut Vec<String>, reference: &str| {
            if out_rows.contains_key(&(section, key.clone())) {
                notes.push(format!(
                "dropped `{reference}` — another row already migrated to `[{}] {key}`; re-add it \
                 with a distinct `name` field",
                section.header()
            ));
            } else {
                out_rows.insert((section, key), value);
            }
        };

    for row in rows {
        let (plain_value, fields) = read_fields(&row.value);
        let shape = match classify_key(&row.reference) {
            Ok(s) => s,
            Err(_) => {
                notes.push(format!(
                    "dropped `{}` — not a reference this topos reads; re-add it by hand",
                    row.reference
                ));
                continue;
            }
        };
        match shape {
            KeyShape::Feed { host, workspace } => {
                if scope == ManifestScope::Project {
                    notes.push(format!(
                        "dropped `{host}/{workspace}` — a project file holds no workspace feed",
                    ));
                    continue;
                }
                push(
                    Section::Workspaces,
                    format!("{host}/{workspace}"),
                    Value::from("latest"),
                    &mut notes,
                    &row.reference,
                );
            }
            KeyShape::WorkspaceBundle {
                host,
                workspace,
                bundle,
            } => {
                let section = match fields.kind.as_deref() {
                    Some("mcp") => Section::Mcp,
                    Some(_) => Section::Skills,
                    None => match kind_of(&host, &workspace, &bundle) {
                        Some(BundleKind::Mcp) => Section::Mcp,
                        Some(BundleKind::Skill) => Section::Skills,
                        None => {
                            if plain_value.as_deref() != Some("off") {
                                notes.push(format!(
                                    "`{}` — kind unknown on this machine; filed under [skills] \
                                     (move the line to [mcp] if it is an MCP server)",
                                    row.reference
                                ));
                            }
                            Section::Skills
                        }
                    },
                };
                if !workspaces_seen.contains(&(host.clone(), workspace.clone())) {
                    workspaces_seen.push((host.clone(), workspace.clone()));
                }
                let key = match scope {
                    ManifestScope::Global => format!("{host}/{workspace}/{bundle}"),
                    ManifestScope::Project => bundle.clone(),
                };
                let mut value = migrate_ws_value(plain_value.as_deref(), &fields);
                // An [mcp] row takes no version pin in v2 — a carried one is dropped, said.
                // BOTH spellings: a plain pinned string, and an inline table whose `version`
                // field carries the pin (leaving either would rewrite the file into one the
                // v2 parser then refuses — a migration must never strand its own output).
                if section == Section::Mcp {
                    let is_pin = |v: &str| v != "latest" && v != "off";
                    let plain_pin = value.as_str().is_some_and(is_pin);
                    let table_pin = value
                        .as_inline_table()
                        .and_then(|t| t.get("version"))
                        .and_then(Value::as_str)
                        .is_some_and(is_pin);
                    if plain_pin || table_pin {
                        notes.push(format!(
                            "`{}` — an MCP server takes no version pin in v2; the pin was \
                             dropped (the lock records the delivered revision)",
                            row.reference
                        ));
                        if plain_pin {
                            value = Value::from("latest");
                        } else if let Some(t) = value.as_inline_table_mut() {
                            t.insert("version", Value::from("latest"));
                        }
                    }
                }
                push(section, key, value, &mut notes, &row.reference);
            }
            KeyShape::Channel {
                host,
                workspace,
                channel,
            } => {
                if !workspaces_seen.contains(&(host.clone(), workspace.clone())) {
                    workspaces_seen.push((host.clone(), workspace.clone()));
                }
                let key = match scope {
                    ManifestScope::Global => format!("{host}/{workspace}/{channel}"),
                    ManifestScope::Project => channel.clone(),
                };
                let value = migrate_channel_value(&fields);
                push(Section::Channels, key, value, &mut notes, &row.reference);
            }
            KeyShape::RepoSkill {
                host,
                owner,
                repo,
                skill,
            } => {
                let source_word = source_prefix(&host);
                let pin = plain_pin(plain_value.as_deref()).or(fields.version.clone());
                let source = match pin {
                    Some(p) => format!("{source_word}:{owner}/{repo}#{p}"),
                    None => format!("{source_word}:{owner}/{repo}"),
                };
                let key = fields.name.clone().unwrap_or(skill);
                let value = repo_value(&source, &fields);
                push(Section::Skills, key, value, &mut notes, &row.reference);
            }
            KeyShape::RepoSet { host, owner, repo } => {
                notes.push(format!(
                    "dropped `{host}/{owner}/{repo}` — a whole-repo row has no v2 spelling; \
                     `topos add {host}/{owner}/{repo}` re-adds each of its skills on its own row",
                ));
            }
            KeyShape::LocalPath { raw } => {
                let leaf = raw
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or(&raw)
                    .to_string();
                let key = fields.name.clone().unwrap_or(leaf);
                let section = match fields.kind.as_deref() {
                    Some("mcp") => Section::Mcp,
                    _ => Section::Skills,
                };
                let value = path_value(&raw, &fields);
                push(section, key, value, &mut notes, &row.reference);
            }
        }
    }

    // The project one-workspace rule: the migration cannot pick between several.
    let workspace_line = match scope {
        ManifestScope::Global => None,
        ManifestScope::Project => match workspaces_seen.len() {
            0 => None,
            1 => Some(format!("{}/{}", workspaces_seen[0].0, workspaces_seen[0].1)),
            _ => {
                let list = workspaces_seen
                    .iter()
                    .map(|(h, w)| format!("{h}/{w}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "this project file names bundles from several workspaces ({list}) — a v2 \
                     project uses one; edit the file down to one workspace, then rerun",
                ));
            }
        },
    };

    // Emit: leading comment block, `schema = 1`, the workspace line, then the sections.
    let mut out = DocumentMut::new();
    out.insert("schema", toml_edit::value(1));
    if let Some(ws) = &workspace_line {
        out.insert("workspace", toml_edit::value(ws.as_str()));
    }
    for section in [
        Section::Skills,
        Section::Mcp,
        Section::Channels,
        Section::Workspaces,
    ] {
        let mut table = Table::new();
        table.set_implicit(false);
        let mut any = false;
        for ((s, key), value) in &out_rows {
            if *s == section {
                table.insert(key, Item::Value(value.clone()));
                any = true;
            }
        }
        if any {
            out.insert(section.header(), Item::Table(table));
        }
    }

    let mut text_out = String::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            text_out.push_str(line);
            text_out.push('\n');
        } else if t.is_empty() && text_out.is_empty() {
            continue;
        } else {
            break;
        }
    }
    if !text_out.is_empty() {
        text_out.push('\n');
    }
    text_out.push_str(&out.to_string());
    Ok(Migration {
        text: text_out,
        notes,
    })
}

/// `"*"` → follow; anything else is already a pin word.
fn plain_pin(v: Option<&str>) -> Option<String> {
    match v {
        Some("*") | Some("off") | None => None,
        Some(p) => Some(p.to_string()),
    }
}

fn source_prefix(host: &str) -> &'static str {
    if host == "bitbucket.org" {
        "bitbucket"
    } else {
        "github"
    }
}

/// A workspace bundle's migrated value: plain word, or the carried fields.
fn migrate_ws_value(plain: Option<&str>, fields: &V1Fields) -> Value {
    let version = match plain {
        Some("*") | None => fields
            .version
            .clone()
            .unwrap_or_else(|| "latest".to_string()),
        Some("off") => "off".to_string(),
        Some(p) => p.to_string(),
    };
    if fields.dest.is_none() && fields.mcp_dest.is_none() && fields.name.is_none() {
        return Value::from(version);
    }
    let mut t = InlineTable::new();
    t.insert("version", Value::from(version));
    carry_shared(&mut t, fields, true);
    Value::InlineTable(t)
}

fn migrate_channel_value(fields: &V1Fields) -> Value {
    if fields.dest.is_none() && fields.mcp_dest.is_none() {
        return Value::from("latest");
    }
    let mut t = InlineTable::new();
    t.insert("version", Value::from("latest"));
    carry_shared(&mut t, fields, false);
    Value::InlineTable(t)
}

fn repo_value(source: &str, fields: &V1Fields) -> Value {
    if fields.dest.is_none() && fields.subdir.is_none() {
        return Value::from(source);
    }
    let mut t = InlineTable::new();
    t.insert("source", Value::from(source));
    if let Some(subdir) = &fields.subdir {
        t.insert("subdir", Value::from(subdir.as_str()));
    }
    carry_shared(&mut t, fields, false);
    Value::InlineTable(t)
}

fn path_value(raw: &str, fields: &V1Fields) -> Value {
    if fields.dest.is_none() {
        return Value::from(raw);
    }
    let mut t = InlineTable::new();
    t.insert("path", Value::from(raw));
    carry_shared(&mut t, fields, false);
    Value::InlineTable(t)
}

/// The fields every shape may carry into v2 unchanged.
fn carry_shared(t: &mut InlineTable, fields: &V1Fields, name_survives: bool) {
    if let Some(dest) = &fields.dest {
        t.insert("dest", Value::Array(dest.clone()));
    }
    if let Some(mcp_dest) = &fields.mcp_dest {
        t.insert("mcp_dest", Value::Array(mcp_dest.clone()));
    }
    if name_survives && let Some(name) = &fields.name {
        t.insert("name", Value::from(name.as_str()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::document::parse_manifest;

    fn hex64() -> String {
        "3f9c1a2b".repeat(8)
    }

    fn kinds(host: &str, ws: &str, bundle: &str) -> Option<BundleKind> {
        match (host, ws, bundle) {
            ("topos.sh", "acme", "code-review") => Some(BundleKind::Skill),
            ("topos.sh", "acme", "linear") => Some(BundleKind::Mcp),
            _ => None,
        }
    }

    #[test]
    fn v1_is_recognized_and_v2_is_not() {
        assert!(is_v1("[bundles]\n\"topos.sh/acme\" = \"*\"\n"));
        assert!(!is_v1("schema = 1\n\n[skills]\nx = \"latest\"\n"));
        assert!(!is_v1("not toml ["));
    }

    #[test]
    fn a_global_file_migrates_whole_and_reparses() {
        let text = format!(
            "# my machine\n\n[bundles]\n\
             \"topos.sh/acme\" = \"*\"\n\
             \"topos.sh/acme/code-review\" = \"*\"\n\
             \"topos.sh/acme/linear\" = \"*\"\n\
             \"topos.sh/acme/pinned\" = \"{}\"\n\
             \"topos.sh/acme/muted\" = \"off\"\n\
             \"topos.sh/acme/channels/backend\" = \"*\"\n\
             \"github.com/vercel-labs/skills/find-skills\" = \"*\"\n\
             \"github.com/acme/tools/sql-style\" = \"9d0e8c17\"\n\
             \"~/dev/weather-server\" = {{ kind = \"mcp\" }}\n\
             \"github.com/acme/bigrepo\" = \"*\"\n",
            hex64()
        );
        let m = migrate_v1(&text, ManifestScope::Global, &kinds).unwrap();

        // The migrated text is a VALID v2 machine file — the real parser is the oracle.
        let doc = parse_manifest(&m.text, ManifestScope::Global).unwrap();
        assert!(
            doc.warnings.is_empty(),
            "no unknown sections: {:?}",
            doc.warnings
        );

        // The leading comment survives; the sections carry the mapped rows.
        assert!(m.text.starts_with("# my machine\n"));
        assert!(m.text.contains("schema = 1"));
        assert!(m.text.contains("[workspaces]"));
        assert!(m.text.contains("\"topos.sh/acme\" = \"latest\""));
        assert!(
            m.text
                .contains("\"topos.sh/acme/code-review\" = \"latest\"")
        );
        assert!(
            m.text
                .contains(&format!("\"topos.sh/acme/pinned\" = \"{}\"", hex64()))
        );
        assert!(m.text.contains("\"topos.sh/acme/muted\" = \"off\""));
        assert!(m.text.contains("[mcp]"));
        assert!(m.text.contains("\"topos.sh/acme/linear\" = \"latest\""));
        assert!(m.text.contains("weather-server = \"~/dev/weather-server\""));
        assert!(m.text.contains("[channels]"));
        assert!(m.text.contains("\"topos.sh/acme/backend\" = \"latest\""));
        assert!(
            m.text
                .contains("find-skills = \"github:vercel-labs/skills\"")
        );
        assert!(
            m.text
                .contains("sql-style = \"github:acme/tools#9d0e8c17\"")
        );

        // The set row dropped with the re-add note; the unknown kind noted.
        assert!(
            m.notes
                .iter()
                .any(|n| n.contains("github.com/acme/bigrepo"))
        );
        assert!(m.notes.iter().any(|n| n.contains("`topos.sh/acme/pinned`")));
    }

    #[test]
    fn a_pinned_v1_mcp_row_drops_its_pin_with_a_note() {
        let text = format!(
            "[bundles]\n\"topos.sh/acme/linear\" = \"{}\"\n",
            "ab".repeat(32)
        );
        let m = migrate_v1(&text, ManifestScope::Global, &kinds).unwrap();
        assert!(
            m.text.contains("\"topos.sh/acme/linear\" = \"latest\""),
            "{}",
            m.text
        );
        assert!(
            m.notes.iter().any(|n| n.contains("takes no version pin")),
            "{:?}",
            m.notes
        );
        parse_manifest(&m.text, ManifestScope::Global).unwrap();
    }

    #[test]
    fn a_grouped_v1_file_flattens_to_the_same_rows() {
        let text = "[bundles.\"topos.sh/acme\"]\n\"code-review\" = \"*\"\n";
        let m = migrate_v1(text, ManifestScope::Global, &kinds).unwrap();
        assert!(
            m.text
                .contains("\"topos.sh/acme/code-review\" = \"latest\"")
        );
        parse_manifest(&m.text, ManifestScope::Global).unwrap();
    }

    #[test]
    fn a_single_workspace_project_gains_the_workspace_line_and_bare_keys() {
        let text = "[bundles]\n\
                    \"topos.sh/acme/code-review\" = \"*\"\n\
                    \"topos.sh/acme/channels/backend\" = \"*\"\n";
        let m = migrate_v1(text, ManifestScope::Project, &kinds).unwrap();
        assert!(m.text.contains("workspace = \"topos.sh/acme\""));
        assert!(m.text.contains("code-review = \"latest\""));
        assert!(m.text.contains("backend = \"latest\""));
        let doc = parse_manifest(&m.text, ManifestScope::Project).unwrap();
        assert!(doc.warnings.is_empty());
    }

    #[test]
    fn a_multi_workspace_project_refuses_naming_both() {
        let text = "[bundles]\n\
                    \"topos.sh/acme/x\" = \"*\"\n\
                    \"topos.sh/other/y\" = \"*\"\n";
        let e = migrate_v1(text, ManifestScope::Project, &kinds).unwrap_err();
        assert!(
            e.contains("topos.sh/acme") && e.contains("topos.sh/other"),
            "{e}"
        );
        assert!(e.contains("one workspace") || e.contains("uses one"), "{e}");
    }

    #[test]
    fn colliding_names_keep_the_first_and_note_the_second() {
        let text = "[bundles]\n\
                    \"github.com/a/tools/style\" = \"*\"\n\
                    \"github.com/b/tools/style\" = \"*\"\n";
        let m = migrate_v1(text, ManifestScope::Global, &kinds).unwrap();
        assert!(m.text.contains("style = \"github:a/tools\""));
        assert!(!m.text.contains("github:b/tools"));
        assert!(
            m.notes
                .iter()
                .any(|n| n.contains("github.com/b/tools/style"))
        );
    }

    #[test]
    fn v1_fields_carry_dest_and_the_name_becomes_the_key() {
        let text = "[bundles]\n\
                    \"github.com/a/tools/style\" = { version = \"9d0e8c17\", \
                     name = \"our-style\", dest = [\"*\"] }\n";
        let m = migrate_v1(text, ManifestScope::Global, &kinds).unwrap();
        assert!(
            m.text
                .contains("our-style = { source = \"github:a/tools#9d0e8c17\", dest = [\"*\"] }"),
            "{}",
            m.text
        );
        parse_manifest(&m.text, ManifestScope::Global).unwrap();
    }
}
