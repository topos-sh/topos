//! The manifest NORMAL FORM — the deterministic, idempotent reorganization `fmt` applies.
//!
//! Normal form: the file's leading comment block, then `schema = 1`, then the project's
//! `workspace = ` line (when one stands), then the sections in fixed order — `[skills]`,
//! `[mcp]`, `[channels]`, `[workspaces]` — each with its keys sorted. Sections the file does
//! not use are not written.
//!
//! Comments survive: the leading block stays at the top, a standalone comment above an entry
//! travels with the entry, an inline comment AFTER a value (`x = "latest"  # why`) stays on its
//! line, and a comment above a section header stays with the section. Values normalize too — a
//! fields table carrying ONLY `version` collapses to the plain string spelling, and a `dest`
//! naming nothing beyond the default-reach token collapses with it.
//!
//! Unknown sections (forward tolerance) are carried through VERBATIM, at the end, in file
//! order — `fmt` must never drop what a newer topos wrote.

use std::collections::HashMap;

use toml_edit::{Decor, DocumentMut, Item, Table};

use crate::bundle_kind::BundleKind;
use crate::manifest::document::{
    BundleRow, EntryValue, ManifestError, ManifestScope, SectionKind, parse_document, spell_row,
};

/// Rewrite a manifest text into normal form. Validates the WHOLE document first (the same rules
/// as parsing), so `fmt` never launders a malformed file.
///
/// # Errors
/// Whatever [`crate::manifest::document::parse_manifest`] would refuse, refused identically.
pub(crate) fn fmt_normal(text: &str, scope: ManifestScope) -> Result<String, ManifestError> {
    let doc: DocumentMut = text.parse().map_err(|e| ManifestError {
        message: format!("not valid TOML: {e}"),
        key: None,
    })?;
    let parsed = parse_document(&doc, scope)?;

    // Harvest the comments that must survive the rebuild.
    let mut entry_comments: HashMap<String, String> = HashMap::new();
    let mut entry_suffixes: HashMap<String, String> = HashMap::new();
    let mut section_comments: HashMap<&'static str, String> = HashMap::new();
    for (header, item) in doc.iter() {
        let Some(section) = section_of(header) else {
            continue;
        };
        let Item::Table(t) = item else { continue };
        let c = decor_comment(t.decor());
        if !c.is_empty() {
            section_comments.insert(section.header(), c);
        }
        for (key, val) in t.iter() {
            let Item::Value(v) = val else { continue };
            let Some(reference) = reference_of(&parsed.rows, section, key) else {
                continue;
            };
            if let Some(k) = t.key(key) {
                let c = comment_block(raw_prefix(k.leaf_decor()));
                if !c.is_empty() {
                    entry_comments.insert(reference.clone(), c);
                }
            }
            let s = suffix_comment(v.decor());
            if !s.is_empty() {
                entry_suffixes.insert(reference, s);
            }
        }
    }
    let header = extract_header(&doc);

    // Rebuild: header + schema (+ workspace), then the sections in fixed order, keys sorted.
    let mut out = String::new();
    out.push_str(&header);
    out.push_str("schema = 1\n");
    if let Some((host, ws)) = &parsed.workspace {
        out.push_str(&format!("workspace = \"{host}/{ws}\"\n"));
    }
    for section in [
        SectionKind::Skills,
        SectionKind::Mcp,
        SectionKind::Channels,
        SectionKind::Workspaces,
    ] {
        let mut rows: Vec<&BundleRow> = parsed
            .rows
            .iter()
            .filter(|r| r.section == section)
            .collect();
        if rows.is_empty() {
            continue;
        }
        let mut table = Table::new();
        table.set_implicit(false);
        let kind = match section {
            SectionKind::Mcp => BundleKind::Mcp,
            _ => BundleKind::Skill,
        };
        let mut spelled: Vec<(String, &BundleRow)> = Vec::with_capacity(rows.len());
        for row in rows.drain(..) {
            let s = spell_row(
                &row.reference,
                &row.shape,
                &row.value,
                kind,
                scope,
                parsed.workspace.as_ref(),
            )
            .map_err(|e| ManifestError {
                message: e.message,
                key: e.key,
            })?;
            spelled.push((s.key, row));
        }
        spelled.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, row) in &spelled {
            let value = normal_value(&row.value);
            let s = spell_row(
                &row.reference,
                &row.shape,
                &value,
                kind,
                scope,
                parsed.workspace.as_ref(),
            )
            .expect("spelled a moment ago");
            table.insert(key, s.item);
            if let Some(c) = entry_comments.get(&row.reference)
                && let Some(mut km) = table.key_mut(key)
            {
                km.leaf_decor_mut().set_prefix(c.clone());
            }
            if let Some(sfx) = entry_suffixes.get(&row.reference)
                && let Some(Item::Value(v)) = table.get_mut(key)
            {
                v.decor_mut().set_suffix(sfx.clone());
            }
        }
        out.push('\n');
        if let Some(c) = section_comments.get(section.header()) {
            out.push_str(c);
        }
        out.push_str(&format!("[{}]\n", section.header()));
        // Render the table body alone (its header was just written with its comment).
        let mut one = DocumentMut::new();
        table.decor_mut().set_prefix("");
        one.insert("x", Item::Table(table));
        let rendered = one.to_string();
        let body = rendered
            .split_once('\n')
            .map(|(_, rest)| rest)
            .unwrap_or("");
        out.push_str(body);
    }

    // Unknown sections carry through verbatim, at the end.
    for (header, item) in doc.iter() {
        if matches!(header, "schema" | "workspace") || section_of(header).is_some() {
            continue;
        }
        if let Item::Table(t) = item {
            let mut one = DocumentMut::new();
            let mut t = t.clone();
            t.decor_mut().set_prefix("");
            one.insert(header, Item::Table(t));
            out.push('\n');
            out.push_str(&one.to_string());
        }
    }
    Ok(out)
}

fn section_of(header: &str) -> Option<SectionKind> {
    match header {
        "skills" => Some(SectionKind::Skills),
        "mcp" => Some(SectionKind::Mcp),
        "channels" => Some(SectionKind::Channels),
        "workspaces" => Some(SectionKind::Workspaces),
        _ => None,
    }
}

/// Find the canonical reference a section key resolved to (the parse already did the work).
fn reference_of(rows: &[BundleRow], section: SectionKind, key: &str) -> Option<String> {
    rows.iter()
        .find(|r| {
            r.section == section
                && (r.reference == key
                    || r.reference.ends_with(&format!("/{key}"))
                    || r.shape.leaf_name() == key)
        })
        .map(|r| r.reference.clone())
}

/// The value in normal spelling: a fields table carrying ONLY `version` collapses to the plain
/// string spelling, and a `dest` that names nothing but the DEFAULT-REACH token collapses with
/// it.
fn normal_value(v: &EntryValue) -> EntryValue {
    if let EntryValue::Fields(f) = v
        && f.dest
            .as_deref()
            .is_none_or(|d| crate::manifest::dest::named_entries(d).is_empty())
        && f.mcp_dest.is_none()
        && f.name.is_none()
        && f.subdir.is_none()
        && f.kind.is_none()
    {
        return match f.version.as_deref() {
            Some("latest") | None => EntryValue::Star,
            Some(ver) => EntryValue::Pin(ver.to_string()),
        };
    }
    v.clone()
}

/// The file's leading comment block: whatever comment rides above the first thing in the file.
fn extract_header(doc: &DocumentMut) -> String {
    if let Some((first, item)) = doc.iter().next() {
        if let Item::Table(t) = item {
            return decor_comment(t.decor());
        }
        if let Some(k) = doc.as_table().key(first) {
            return comment_block(raw_prefix(k.leaf_decor()));
        }
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
/// the comment verbatim. Deterministic and idempotent.
fn suffix_comment(d: &Decor) -> String {
    let raw = d.suffix().and_then(|r| r.as_str()).unwrap_or("");
    match raw.find('#') {
        Some(i) => format!(" {}", raw[i..].trim_end()),
        None => String::new(),
    }
}

/// Normalize a decor prefix to its comment lines only — deterministic and idempotent.
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

    fn fmt_project(text: &str) -> String {
        fmt_normal(text, ManifestScope::Project).unwrap()
    }

    fn fmt_global(text: &str) -> String {
        fmt_normal(text, ManifestScope::Global).unwrap()
    }

    #[test]
    fn normal_form_orders_sections_and_sorts_keys() {
        let scrambled = r#"workspace = "topos.sh/acme"

[channels]
backend = "latest"

[skills]
zeta = "latest"
alpha = "latest"

[mcp]
linear = "latest"
"#;
        let expected = r#"schema = 1
workspace = "topos.sh/acme"

[skills]
alpha = "latest"
zeta = "latest"

[mcp]
linear = "latest"

[channels]
backend = "latest"
"#;
        assert_eq!(fmt_project(scrambled), expected);
    }

    #[test]
    fn normal_form_is_idempotent() {
        let inputs = [
            "workspace = \"topos.sh/acme\"\n[skills]\nb = \"latest\"\na = \"github:o/r\"\n",
            "[workspaces]\n\"topos.sh/acme\" = \"latest\"\n\n[skills]\n\"topos.sh/acme/x\" = \"off\"\n",
            "# header\nschema = 1\nworkspace = \"topos.sh/acme\"\n\n[skills]\n# why\nx = \"latest\"  # inline\n",
        ];
        for (i, input) in inputs.iter().enumerate() {
            let scope = if i == 1 {
                ManifestScope::Global
            } else {
                ManifestScope::Project
            };
            let once = fmt_normal(input, scope).unwrap();
            let twice = fmt_normal(&once, scope).unwrap();
            assert_eq!(once, twice, "case {i}:\n{once}");
        }
    }

    #[test]
    fn version_only_tables_collapse() {
        let d = "0123456789abcdef".repeat(4);
        let text = format!(
            "workspace = \"topos.sh/acme\"\n[skills]\nx = {{ version = \"{d}\" }}\ny = {{ version = \"latest\" }}\n"
        );
        let out = fmt_project(&text);
        assert!(out.contains(&format!("x = \"{d}\"")), "{out}");
        assert!(out.contains("y = \"latest\""), "{out}");
    }

    #[test]
    fn comments_survive_the_rebuild() {
        let text = "# the header\nworkspace = \"topos.sh/acme\"\n\n[skills]\n# above x\nx = \"latest\"  # inline\n";
        let out = fmt_project(text);
        assert!(out.starts_with("# the header\n"), "{out}");
        assert!(out.contains("# above x\nx = \"latest\" # inline"), "{out}");
    }

    #[test]
    fn unknown_sections_carry_through_verbatim() {
        let text = "workspace = \"topos.sh/acme\"\n[skills]\nx = \"latest\"\n\n[memories]\nctx = \"latest\"\n";
        let out = fmt_project(text);
        assert!(out.contains("[memories]\nctx = \"latest\"\n"), "{out}");
    }

    #[test]
    fn malformed_files_refuse_rather_than_launder() {
        assert!(fmt_normal("workspace = 3\n", ManifestScope::Project).is_err());
        assert!(
            fmt_normal("[skills]\nx = \"nope\"\n", ManifestScope::Global).is_err(),
            "a bad value refuses"
        );
    }

    #[test]
    fn machine_files_render_their_sections() {
        let text = "[skills]\n\"topos.sh/acme/x\" = \"latest\"\n\n[workspaces]\n\"topos.sh/acme\" = \"latest\"\n";
        let out = fmt_global(text);
        let skills_at = out.find("[skills]").unwrap();
        let ws_at = out.find("[workspaces]").unwrap();
        assert!(skills_at < ws_at, "{out}");
    }
}
