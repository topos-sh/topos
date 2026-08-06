//! The manifest NORMAL FORM — the deterministic, idempotent reorganization `fmt` applies.
//!
//! Normal form: `[bundles]` first, holding feed rows (sorted), then local-path rows, then forge
//! rows (each group sorted); then one `[bundles."<host>/<ws>"]` section per workspace (sorted)
//! holding that workspace's explicit rows under workspace-relative keys (sorted; channels
//! spelled `"channels/<name>"`). Empty grouping
//! sections are pruned. ONE carve-out, forced by TOML itself: a value and a table cannot share
//! a key path, so a workspace that carries its FEED row keeps its explicit rows FLAT under
//! `[bundles]` instead of a grouping section.
//!
//! Comments survive: the file's leading comment block stays at the top, a standalone comment
//! above an entry travels with the entry, an inline comment AFTER a value (`"x" = "*"  # why`)
//! stays on its line — through regrouping and through the version-table collapse alike — and a
//! comment above a section header stays with it.
//! Values normalize too — a fields table carrying ONLY `version` collapses to the plain version
//! string, and inline tables render single-line in canonical field order (version, dest, name,
//! subdir, kind).

use std::collections::{BTreeMap, HashMap, HashSet};

use toml_edit::{Decor, DocumentMut, Item, Table};

use crate::manifest::document::{
    BundleRow, EntryValue, ManifestError, ManifestScope, parse_document, value_item,
};
use crate::manifest::keys::KeyShape;

/// Rewrite a manifest text into normal form. Validates the WHOLE document first (the same rules
/// as parsing), so `fmt` never launders a malformed file.
///
/// # Errors
/// Whatever [`crate::manifest::document::parse_manifest`] would refuse, refused identically.
pub(crate) fn fmt_normal(text: &str, scope: ManifestScope) -> Result<String, ManifestError> {
    let doc: DocumentMut = text.parse().map_err(|e| ManifestError {
        message: format!("not valid TOML: {e}"),
        key: None,
        migration: false,
    })?;
    let parsed = parse_document(&doc, scope)?;

    // Harvest the comments that must survive the rebuild.
    let mut entry_comments: HashMap<String, String> = HashMap::new();
    let mut entry_suffixes: HashMap<String, String> = HashMap::new();
    let mut group_comments: HashMap<String, String> = HashMap::new();
    if let Some(Item::Table(bundles)) = doc.get("bundles") {
        collect_comments(
            bundles,
            &mut Vec::new(),
            &mut entry_comments,
            &mut entry_suffixes,
            &mut group_comments,
        );
    }
    let header = extract_header(&doc, &mut group_comments);

    // Partition the rows: feeds / local paths / forge rows flat, workspace rows per workspace —
    // except workspaces whose FEED row is present, whose rows must stay flat (the carve-out).
    let mut feeds: Vec<&BundleRow> = Vec::new();
    let mut locals: Vec<&BundleRow> = Vec::new();
    let mut forges: Vec<&BundleRow> = Vec::new();
    let mut by_ws: BTreeMap<String, Vec<&BundleRow>> = BTreeMap::new();
    for row in &parsed.rows {
        match &row.shape {
            KeyShape::Feed { .. } => feeds.push(row),
            KeyShape::LocalPath { .. } => locals.push(row),
            KeyShape::RepoSet { .. } | KeyShape::RepoSkill { .. } => forges.push(row),
            KeyShape::WorkspaceBundle { .. } | KeyShape::Channel { .. } => by_ws
                .entry(row.shape.workspace_key().expect("workspace-shaped"))
                .or_default()
                .push(row),
        }
    }
    let feed_ws: HashSet<String> = feeds.iter().map(|r| r.reference.clone()).collect();
    let mut flat_ws: Vec<&BundleRow> = Vec::new();
    for ws in &feed_ws {
        if let Some(rows) = by_ws.remove(ws) {
            flat_ws.extend(rows);
        }
    }
    let by_ref = |a: &&BundleRow, b: &&BundleRow| a.reference.cmp(&b.reference);
    feeds.sort_by(by_ref);
    locals.sort_by(by_ref);
    forges.sort_by(by_ref);
    flat_ws.sort_by(by_ref);
    for rows in by_ws.values_mut() {
        rows.sort_by_key(|r| r.shape.section_tail().expect("workspace-shaped"));
    }

    // Rebuild.
    let mut out = DocumentMut::new();
    let mut first_block = true;
    let flat: Vec<&BundleRow> = feeds
        .into_iter()
        .chain(locals)
        .chain(forges)
        .chain(flat_ws)
        .collect();
    if !flat.is_empty() || !by_ws.is_empty() {
        let mut bundles = Table::new();
        bundles.set_implicit(flat.is_empty());
        if !flat.is_empty() {
            if !header.is_empty() {
                bundles.decor_mut().set_prefix(header.clone());
            }
            first_block = false;
            for row in flat {
                push_row(
                    &mut bundles,
                    &row.reference,
                    row,
                    &entry_comments,
                    &entry_suffixes,
                );
            }
        }
        for (ws, rows) in &by_ws {
            let mut section = Table::new();
            section.set_implicit(false);
            let mut comment = String::new();
            if first_block {
                comment.push_str(&header);
            }
            if let Some(c) = group_comments.get(ws) {
                comment.push_str(c);
            }
            set_block_prefix(&mut section, first_block, &comment);
            first_block = false;
            for row in rows {
                let tail = row.shape.section_tail().expect("workspace-shaped");
                push_row(&mut section, &tail, row, &entry_comments, &entry_suffixes);
            }
            bundles.insert(ws, Item::Table(section));
        }
        out.insert("bundles", Item::Table(bundles));
    }

    if out.as_table().is_empty() {
        // Nothing but comments: the header IS the file.
        return Ok(header);
    }
    Ok(out.to_string())
}

/// Set a rebuilt table's header decor: blocks after the first are separated by one blank line;
/// comment lines (the carried header and/or the block's own comment) ride above the header.
fn set_block_prefix(table: &mut Table, first: bool, comment: &str) {
    if comment.is_empty() {
        // The default table decor already renders "" for the first header and "\n" after.
        return;
    }
    let lead = if first { "" } else { "\n" };
    table.decor_mut().set_prefix(format!("{lead}{comment}"));
}

fn push_row(
    table: &mut Table,
    key: &str,
    row: &BundleRow,
    comments: &HashMap<String, String>,
    suffixes: &HashMap<String, String>,
) {
    table.insert(key, normal_value_item(&row.value));
    if let Some(c) = comments.get(&row.reference)
        && let Some(mut km) = table.key_mut(key)
    {
        km.leaf_decor_mut().set_prefix(c.clone());
    }
    // The inline comment after the value stays on its line — including through the
    // version-table collapse, whose rebuilt value it re-attaches to.
    if let Some(s) = suffixes.get(&row.reference)
        && let Some(Item::Value(v)) = table.get_mut(key)
    {
        v.decor_mut().set_suffix(s.clone());
    }
}

/// The value in normal spelling: a fields table carrying ONLY `version` collapses to the plain
/// version string (an empty fields table means track-current); everything else renders through
/// the one deterministic renderer.
fn normal_value_item(v: &EntryValue) -> Item {
    if let EntryValue::Fields(f) = v
        && f.dest.is_none()
        && f.name.is_none()
        && f.subdir.is_none()
        && f.kind.is_none()
    {
        return match f.version.as_deref() {
            Some("*") | None => value_item(&EntryValue::Star),
            Some(ver) => value_item(&EntryValue::Pin(ver.to_string())),
        };
    }
    value_item(v)
}

fn collect_comments(
    table: &Table,
    prefix: &mut Vec<String>,
    entries: &mut HashMap<String, String>,
    suffixes: &mut HashMap<String, String>,
    groups: &mut HashMap<String, String>,
) {
    for (k, item) in table.iter() {
        let path = if prefix.is_empty() {
            k.to_string()
        } else {
            format!("{}/{k}", prefix.join("/"))
        };
        match item {
            Item::Value(v) => {
                if let Some(key) = table.key(k) {
                    let c = comment_block(raw_prefix(key.leaf_decor()));
                    if !c.is_empty() {
                        entries.insert(path.clone(), c);
                    }
                }
                let s = suffix_comment(v.decor());
                if !s.is_empty() {
                    suffixes.insert(path, s);
                }
            }
            Item::Table(t) => {
                let c = decor_comment(t.decor());
                if !c.is_empty() {
                    groups.insert(path.clone(), c);
                }
                prefix.push(k.to_string());
                collect_comments(t, prefix, entries, suffixes, groups);
                prefix.pop();
            }
            _ => {}
        }
    }
}

/// The file's leading comment block: the first rendered table header's comment (an implicit
/// `[bundles]` hands the honor to its first child section), or a table-less file's trailing
/// decor. Taken OUT of the group comment map so it is emitted once, at the top.
fn extract_header(doc: &DocumentMut, group_comments: &mut HashMap<String, String>) -> String {
    if let Some(Item::Table(b)) = doc.get("bundles") {
        if !b.is_implicit() {
            return decor_comment(b.decor());
        }
        if let Some((k, _)) = b.iter().find(|(_, i)| i.is_table()) {
            return group_comments.remove(k).unwrap_or_default();
        }
        return String::new();
    }
    comment_block(doc.trailing().as_str().unwrap_or(""))
}

fn raw_prefix(d: &Decor) -> &str {
    d.prefix().and_then(|r| r.as_str()).unwrap_or("")
}

fn decor_comment(d: &Decor) -> String {
    comment_block(raw_prefix(d))
}

/// Normalize a value's SUFFIX decor to its inline comment, or empty: ` # why` — one space, then
/// the comment verbatim. Deterministic and idempotent (re-collecting a re-attached suffix yields
/// the same string).
fn suffix_comment(d: &Decor) -> String {
    let raw = d.suffix().and_then(|r| r.as_str()).unwrap_or("");
    match raw.find('#') {
        Some(i) => format!(" {}", raw[i..].trim_end()),
        None => String::new(),
    }
}

/// Normalize a decor prefix to its comment lines only: each line trimmed of leading
/// whitespace, non-comment lines dropped, one `\n` after each — deterministic and idempotent.
fn comment_block(raw: &str) -> String {
    raw.lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt_global(text: &str) -> String {
        fmt_normal(text, ManifestScope::Global).unwrap()
    }

    #[test]
    fn normal_form_orders_and_groups() {
        let scrambled = r#"[bundles."topos.sh/acme"]
deploy = "*"
"channels/frontend" = "*"

[bundles]
"github.com/o/r" = "*"
"topos.example.com/platform" = "*"
"./tools/x" = "*"
"#;
        let expected = r#"[bundles]
"topos.example.com/platform" = "*"
"./tools/x" = "*"
"github.com/o/r" = "*"

[bundles."topos.sh/acme"]
"channels/frontend" = "*"
deploy = "*"
"#;
        assert_eq!(fmt_global(scrambled), expected);
    }

    #[test]
    fn normal_form_is_idempotent() {
        let inputs = [
            "[bundles]\n\"topos.sh/acme\" = \"*\"\n",
            "# header\n\n[bundles]\n# entry note\n\"github.com/o/r\" = \"*\"\n\n[bundles.\"topos.sh/acme\"]\ndeploy = \"*\"\n",
            "[bundles.\"topos.sh\"]\n\"acme/code-review\" = \"*\"\n",
            "[bundles]\n\"topos.sh/acme/x\" = { dest = [\"~/.claude/skills\"] }\n",
            "# only a comment\n",
        ];
        for input in inputs {
            let once = fmt_normal(input, ManifestScope::Global).unwrap();
            let twice = fmt_normal(&once, ManifestScope::Global).unwrap();
            assert_eq!(once, twice, "input={input:?}");
        }
    }

    #[test]
    fn normal_form_regroups_flat_workspace_rows_into_sections() {
        // Flat spellings of a feedless workspace regroup under its section, channels spelled
        // by their tail.
        let text = fmt_global(
            "[bundles]\n\"topos.sh/acme/deploy\" = \"*\"\n\"topos.sh/acme/channels/backend\" = \"*\"\n",
        );
        assert_eq!(
            text,
            "[bundles.\"topos.sh/acme\"]\n\"channels/backend\" = \"*\"\ndeploy = \"*\"\n"
        );
    }

    #[test]
    fn a_workspace_with_its_feed_row_stays_flat() {
        // TOML cannot hold the feed value and a grouping table at one key — the carve-out keeps
        // that workspace's rows flat, and the result must re-parse (regression: a grouped
        // rebuild would not even be valid TOML).
        let input = "[bundles]\n\"topos.sh/acme/noisy\" = \"off\"\n\"topos.sh/acme\" = \"*\"\n";
        let text = fmt_global(input);
        assert_eq!(
            text,
            "[bundles]\n\"topos.sh/acme\" = \"*\"\n\"topos.sh/acme/noisy\" = \"off\"\n"
        );
        assert!(fmt_normal(&text, ManifestScope::Global).is_ok());
    }

    #[test]
    fn comments_travel_with_their_blocks() {
        let input = r#"# header line one
# header line two

[bundles]
# why this repo
"github.com/o/r" = "*"

# the acme grouping
[bundles."topos.sh/acme"]
# why deploy is here
deploy = "*"
"#;
        let expected = r#"# header line one
# header line two
[bundles]
# why this repo
"github.com/o/r" = "*"

# the acme grouping
[bundles."topos.sh/acme"]
# why deploy is here
deploy = "*"
"#;
        let once = fmt_global(input);
        assert_eq!(once, expected);
        assert_eq!(fmt_global(&once), once, "comment handling is idempotent");
    }

    #[test]
    fn value_suffix_comments_survive_the_rebuild() {
        // Inline comments after a value — flat rows, sectioned rows, and the version-table
        // collapse — all keep their line's comment.
        let digest = "0123456789abcdef".repeat(4);
        let input = format!(
            "[bundles]\n\
             \"github.com/o/r\" = \"*\" # tracked read-only\n\
             \"topos.sh/acme/deploy\" = {{ version = \"{digest}\" }}   # pinned for the release\n"
        );
        let once = fmt_global(&input);
        assert!(
            once.contains("\"github.com/o/r\" = \"*\" # tracked read-only"),
            "{once}"
        );
        // The single-field version table COLLAPSED — and the comment rode onto the collapsed value.
        assert!(
            once.contains(&format!("deploy = \"{digest}\" # pinned for the release")),
            "{once}"
        );
        assert_eq!(fmt_global(&once), once, "suffix handling is idempotent");
    }

    #[test]
    fn single_field_version_tables_collapse_to_plain_strings() {
        let digest = "0123456789abcdef".repeat(4);
        let text = fmt_global(&format!(
            "[bundles]\n\"topos.sh/acme/a\" = {{ version = \"{digest}\" }}\n\"topos.sh/acme/b\" = {{ version = \"*\" }}\n",
        ));
        assert_eq!(
            text,
            format!("[bundles.\"topos.sh/acme\"]\na = \"{digest}\"\nb = \"*\"\n")
        );
        // A table with MORE than a version keeps the table (canonical field order).
        let text = fmt_global(&format!(
            "[bundles]\n\"topos.sh/acme/a\" = {{ dest = [\"~/.codex/skills\"], version = \"{digest}\" }}\n",
        ));
        assert_eq!(
            text,
            format!(
                "[bundles.\"topos.sh/acme\"]\na = {{ version = \"{digest}\", dest = [\"~/.codex/skills\"] }}\n"
            )
        );
    }

    #[test]
    fn empty_sections_are_pruned() {
        let text =
            fmt_global("[bundles]\n\"topos.sh/beta/x\" = \"*\"\n\n[bundles.\"topos.sh/acme\"]\n");
        assert!(!text.contains("acme"), "{text}");
        // A manifest that is ALL empty grouping renders empty.
        assert_eq!(fmt_global("[bundles.\"topos.sh/acme\"]\n"), "");
    }

    #[test]
    fn fmt_refuses_what_the_parser_refuses() {
        assert!(fmt_normal("[stray]\nx = 1\n", ManifestScope::Global).is_err());
        assert!(
            fmt_normal(
                "[bundles]\n\"topos.sh/acme\" = \"*\"\n",
                ManifestScope::Project
            )
            .is_err()
        );
    }
}
