//! The Hermes `config.yaml` splicer — [`McpDialect::HermesYaml`], hand-rolled in the exact
//! idiom of the `hermes` adapter's hooks merge (`split_inclusive('\n')`, zero-indent anchors,
//! region scanning; no YAML dependency): total over exactly the shapes it can prove, fail-closed
//! on everything else, untouched lines preserved byte-for-byte.
//!
//! A managed entry is ONE line — a flow mapping ending with the ownership sentinel comment
//! ` # topos:mcp`:
//!
//! ```yaml
//! mcp_servers:
//!   topos-acme-linear: {url: "https://…", headers: {X-T: "v"}, auth: oauth}  # topos:mcp
//! ```
//!
//! `auth: oauth` is emitted ONLY for [`AuthHint::Oauth`] (Hermes needs the explicit opt-in but
//! must not be sent into OAuth for a no-auth server). The url and header values are ALWAYS
//! double-quoted (PyYAML is YAML 1.1 — an unquoted scalar can coerce); header names follow the
//! example spelling (bare for `[A-Za-z0-9_-]`, double-quoted otherwise); the entry key stays
//! bare (`[a-z0-9-]` is safe). Managed lines are identified by the sentinel suffix ALONE, and
//! must sit at the block's own child indent: a sentinel-bearing line at ANY other indent inside
//! the block fails the whole analysis closed — it may be OUR OWN entry re-indented (or a pasted
//! copy of one), and reasoning around it could re-insert a key the block already holds, minting
//! a duplicate YAML key that bricks the whole config.
//!
//! Unprovable (zero writes): a BOM; duplicate `mcp_servers:` keys; a flow-style value on the key
//! line (any non-comment remainder after the colon); quoted/alternate key spellings; tab
//! indentation in the block; a sentinel line at an indent other than the child indent; a
//! sentinel line whose KEY cannot even be read; a duplicate managed key. A sentinel line whose
//! VALUE fails the flow parse is `Drifted` — untouched, never removed. When the key is absent
//! the block is appended at EOF (one separating newline guaranteed); upserts land directly under
//! the key line; removes/updates touch only their own line. Verified by construction AND
//! assertion: the output minus sentinel lines must be byte-identical, in order, to the input
//! minus sentinel lines — any surprise downgrades to [`EditPlan::Unprovable`].

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use super::{
    ApplyOutcome, AuthHint, EditPlan, EntryState, FoundEntry, MANAGED_KEY_PREFIX, McpDialect,
    McpEntry, Observed, Reconcile, effectively_absent, entry_value, fingerprint_value, outcome,
    reconcile, unprovable, validate_desired,
};

/// The line-ownership sentinel (a YAML comment outside the flow mapping).
const SENTINEL: &str = "# topos:mcp";

/// The one top-level key this splicer may touch the block of (plain spelling only).
const KEY_PREFIX: &str = "mcp_servers:";

/// The child indent used when the block is empty (or freshly created).
const DEFAULT_INDENT: usize = 2;

/// Compute the placement plan for a Hermes `config.yaml`. See the module doc for the contract.
#[must_use]
pub fn apply(
    current: Option<&[u8]>,
    desired: &[McpEntry],
    prior: &BTreeMap<String, String>,
) -> ApplyOutcome {
    if let Err(reason) = validate_desired(desired) {
        return unprovable(reason);
    }
    let desired_fps: Vec<String> = desired
        .iter()
        .map(|e| fingerprint_value(&entry_value(McpDialect::HermesYaml, e)))
        .collect();

    if current.is_none() {
        if desired.is_empty() {
            return outcome(EditPlan::Leave, Reconcile::default(), false);
        }
        let mut rec = Reconcile::default();
        for (i, e) in desired.iter().enumerate() {
            rec.states.push((e.key.clone(), EntryState::PlacedNew));
            rec.fingerprints
                .push((e.key.clone(), desired_fps[i].clone()));
        }
        return outcome(
            EditPlan::Write(creation_block(desired).into_bytes()),
            rec,
            true,
        );
    }
    let Ok(text) = std::str::from_utf8(current.unwrap_or_default()) else {
        return unprovable("Hermes config is not UTF-8");
    };
    let shape = match analyze(text) {
        Ok(shape) => shape,
        Err(reason) => return unprovable(reason),
    };
    let (found, region) = match &shape {
        Shape::NoKey => (BTreeMap::new(), None),
        Shape::Region(region) => {
            let mut found: BTreeMap<String, FoundEntry> = BTreeMap::new();
            for (_, key, value) in &region.managed {
                found.insert(
                    key.clone(),
                    match value {
                        Some(v) => FoundEntry::Value(fingerprint_value(v)),
                        None => FoundEntry::OpaqueDrifted,
                    },
                );
            }
            // A plain (non-sentinel) child key that is topos-looking or collides with a desired
            // key occupies the slot without provable provenance — Foreign; inserting beside it
            // would mint a duplicate YAML key.
            let desired_keys: BTreeSet<&str> = desired.iter().map(|e| e.key.as_str()).collect();
            for key in &region.plain_keys {
                if !(key.starts_with(MANAGED_KEY_PREFIX) || desired_keys.contains(key.as_str())) {
                    continue;
                }
                if found.contains_key(key) {
                    return unprovable(format!(
                        "duplicate `{key}` under mcp_servers (a sentinel line and a plain one)"
                    ));
                }
                found.insert(key.clone(), FoundEntry::OpaqueForeign);
            }
            (found, Some(region))
        }
    };
    let rec = reconcile(desired, &desired_fps, &found, prior);
    if rec.is_noop() {
        return outcome(EditPlan::Leave, rec, false);
    }

    let out_text = match region {
        None => append_block(text, desired, &rec.inserts),
        Some(region) => splice(text, region, desired, &rec),
    };
    if let Err(reason) = verify(text, &out_text, desired, &desired_fps, &rec) {
        return unprovable(reason);
    }
    outcome(EditPlan::Write(out_text.into_bytes()), rec, false)
}

/// Read the surface without writing: `key → fingerprint` for every parseable sentinel-marked
/// entry (a mangled sentinel line has no structural value to report; `apply` classifies it
/// `Drifted`).
#[must_use]
pub fn observe(current: Option<&[u8]>) -> Observed {
    let unreadable = Observed {
        entries: BTreeMap::new(),
        parseable: false,
    };
    if effectively_absent(current) {
        return Observed {
            entries: BTreeMap::new(),
            parseable: true,
        };
    }
    let Ok(text) = std::str::from_utf8(current.unwrap_or_default()) else {
        return unreadable;
    };
    let Ok(shape) = analyze(text) else {
        return unreadable;
    };
    let mut entries = BTreeMap::new();
    if let Shape::Region(region) = shape {
        for (_, key, value) in &region.managed {
            if let Some(v) = value {
                entries.insert(key.clone(), fingerprint_value(v));
            }
        }
    }
    Observed {
        entries,
        parseable: true,
    }
}

/// Whether the file holds content beyond what topos itself writes: anything but the one bare
/// `mcp_servers:` key line, blank lines, and sentinel-managed entry lines at the child indent.
/// Comments, plain child keys, other top-level keys, and every unprovable shape answer `true` —
/// the caller's whole-file-ownership reasoning fails TOWARD keeping the file.
#[must_use]
pub fn holds_unmanaged(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return true;
    };
    let Ok(shape) = analyze(text) else {
        return true;
    };
    let lines = split_lines(text);
    let (key_idx, managed_lines): (Option<usize>, BTreeSet<usize>) = match &shape {
        Shape::NoKey => (None, BTreeSet::new()),
        Shape::Region(region) => (
            Some(region.key_idx),
            region.managed.iter().map(|(i, _, _)| *i).collect(),
        ),
    };
    lines.iter().enumerate().any(|(i, line)| {
        if Some(i) == key_idx {
            // The key line must be EXACTLY the bare spelling topos writes (a trailing comment
            // is somebody's annotation).
            return line.trim_end() != "mcp_servers:";
        }
        if managed_lines.contains(&i) {
            return false;
        }
        !line.trim().is_empty()
    })
}

// ---------------------------------------------------------------------------------------------
// Analysis — the provable-shape gate.
// ---------------------------------------------------------------------------------------------

/// The parsed `mcp_servers` region.
struct Region {
    /// Index of the `mcp_servers:` key line.
    key_idx: usize,
    /// The block's child indent (first content line's, else 2).
    child_indent: usize,
    /// Sentinel-marked lines at the child indent: `(line_idx, key, parsed value)` — `None` value
    /// = unparsable (forced `Drifted`).
    managed: Vec<(usize, String, Option<Value>)>,
    /// Plain (non-sentinel) mapping keys at the child indent, for collision detection.
    plain_keys: Vec<String>,
}

enum Shape {
    /// No top-level `mcp_servers:` key — a block can be appended at EOF.
    NoKey,
    Region(Region),
}

fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// A zero-indent line that spells the `mcp_servers` key in a form OTHER than the one canonical
/// `mcp_servers:` — quoted, or with whitespace before the colon. Such a file already HAS the
/// key; reasoning about the canonical spelling beside it would be wrong, so it forces the
/// fail-closed path.
fn is_alternate_spelling(line: &str) -> bool {
    line.starts_with("\"mcp_servers\"")
        || line.starts_with("'mcp_servers'")
        || (line.starts_with("mcp_servers")
            && matches!(line.as_bytes().get("mcp_servers".len()), Some(b' ' | b'\t')))
}

/// The exclusive end of the zero-indent key's region (blank lines and comments stay in-region —
/// they cannot end a YAML block).
fn region_end(lines: &[&str], start: usize) -> usize {
    let mut end = start + 1;
    while end < lines.len() {
        let line = lines[end];
        let indented = matches!(line.chars().next(), Some(' ' | '\t'));
        if !indented && !is_blank_or_comment(line) {
            break;
        }
        end += 1;
    }
    end
}

fn analyze(text: &str) -> Result<Shape, String> {
    // A byte-order mark hides the first line's true column 0 from every zero-indent anchor.
    if text.starts_with('\u{feff}') {
        return Err("Hermes config carries a BOM; refusing to reason about column 0".to_owned());
    }
    let lines = split_lines(text);
    if lines.iter().any(|l| is_alternate_spelling(l)) {
        return Err("mcp_servers is spelled in a form this merge never writes".to_owned());
    }
    let key_lines: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with(KEY_PREFIX))
        .map(|(i, _)| i)
        .collect();
    let key_idx = match key_lines.as_slice() {
        [] => return Ok(Shape::NoKey),
        [one] => *one,
        _ => return Err("duplicate top-level mcp_servers keys".to_owned()),
    };
    // Any non-comment remainder after the colon (a flow value, an anchor, a tag) is a shape this
    // splicer cannot own lines under.
    let remainder = lines[key_idx][KEY_PREFIX.len()..].trim();
    if !remainder.is_empty() && !remainder.starts_with('#') {
        return Err(
            "mcp_servers carries an inline value; only a block form is provable".to_owned(),
        );
    }
    let end = region_end(&lines, key_idx);
    for line in &lines[key_idx + 1..end] {
        let leading = &line[..line.len() - line.trim_start().len()];
        if leading.contains('\t') {
            return Err("tab indentation inside the mcp_servers block".to_owned());
        }
    }
    let child_indent = (key_idx + 1..end)
        .map(|i| lines[i])
        .find(|l| !is_blank_or_comment(l))
        .map_or(DEFAULT_INDENT, indent_of);

    let mut managed: Vec<(usize, String, Option<Value>)> = Vec::new();
    let mut plain_keys = Vec::new();
    for (i, line) in lines.iter().enumerate().take(end).skip(key_idx + 1) {
        let trimmed = line.trim();
        if trimmed.ends_with(SENTINEL) {
            if indent_of(line) != child_indent {
                // FAIL-CLOSED, never skipped: an off-indent sentinel line may be OUR entry
                // re-indented (or a pasted copy). Treating it as invisible would let the splice
                // re-insert a key the block already holds — a duplicate YAML key that can brick
                // the whole config — or silently drop the original from every report.
                return Err(
                    "a topos:mcp sentinel line sits at an unexpected indent under mcp_servers; \
                     refusing to touch the block"
                        .to_owned(),
                );
            }
            let Some((key, body)) = trimmed.split_once(':') else {
                return Err("a topos:mcp sentinel line has no readable key".to_owned());
            };
            let key = key.trim();
            if key.is_empty() || key.contains(char::is_whitespace) {
                return Err("a topos:mcp sentinel line has no readable key".to_owned());
            }
            if managed.iter().any(|(_, k, _)| k == key) {
                return Err(format!("duplicate managed key `{key}` under mcp_servers"));
            }
            let body = body.trim_end().strip_suffix(SENTINEL).map(str::trim_end);
            let value = body.and_then(parse_flow_mapping);
            managed.push((i, key.to_owned(), value));
        } else if !is_blank_or_comment(line) && indent_of(line) == child_indent {
            // A plain mapping key at the child indent (`key: …`) — recorded for collision
            // detection only; its content is never read further.
            if let Some((key, _)) = line.trim().split_once(':')
                && !key.is_empty()
                && !key.contains(char::is_whitespace)
                && !key.starts_with('-')
            {
                plain_keys.push(key.to_owned());
            }
        }
    }
    Ok(Shape::Region(Region {
        key_idx,
        child_indent,
        managed,
        plain_keys,
    }))
}

// ---------------------------------------------------------------------------------------------
// The flow-mapping hand parser — sufficient for OUR OWN emitted grammar (and tolerant of
// whitespace variation within it). Anything else answers `None` → `Drifted`.
// ---------------------------------------------------------------------------------------------

/// Parse `{k: v, …}` (values: double-quoted strings, one-level nested mappings, bare words)
/// into a structural [`Value`]. `s` must be exactly one flow mapping.
fn parse_flow_mapping(s: &str) -> Option<Value> {
    let mut chars = s.char_indices().peekable();
    let value = parse_mapping(s, &mut chars)?;
    skip_ws(&mut chars);
    chars.next().is_none().then_some(value)
}

type Chars<'s> = std::iter::Peekable<std::str::CharIndices<'s>>;

fn skip_ws(chars: &mut Chars<'_>) {
    while matches!(chars.peek(), Some((_, c)) if c.is_whitespace()) {
        chars.next();
    }
}

fn parse_mapping(src: &str, chars: &mut Chars<'_>) -> Option<Value> {
    skip_ws(chars);
    let (_, open) = chars.next()?;
    if open != '{' {
        return None;
    }
    let mut map = Map::new();
    skip_ws(chars);
    if matches!(chars.peek(), Some((_, '}'))) {
        chars.next();
        return Some(Value::Object(map));
    }
    loop {
        skip_ws(chars);
        let key = match chars.peek() {
            Some((_, '"')) => parse_quoted(src, chars)?,
            _ => {
                let mut key = String::new();
                while let Some((_, c)) = chars.peek() {
                    if *c == ':' {
                        break;
                    }
                    key.push(*c);
                    chars.next();
                }
                let key = key.trim().to_owned();
                if key.is_empty() {
                    return None;
                }
                key
            }
        };
        skip_ws(chars);
        if !matches!(chars.next(), Some((_, ':'))) {
            return None;
        }
        skip_ws(chars);
        let value = match chars.peek() {
            Some((_, '"')) => Value::String(parse_quoted(src, chars)?),
            Some((_, '{')) => parse_mapping(src, chars)?,
            _ => {
                let mut word = String::new();
                while let Some((_, c)) = chars.peek() {
                    if matches!(c, ',' | '}') {
                        break;
                    }
                    word.push(*c);
                    chars.next();
                }
                let word = word.trim();
                if word.is_empty() {
                    return None;
                }
                Value::String(word.to_owned())
            }
        };
        map.insert(key, value);
        skip_ws(chars);
        match chars.next() {
            Some((_, ',')) => {}
            Some((_, '}')) => return Some(Value::Object(map)),
            _ => return None,
        }
    }
}

/// A double-quoted scalar, unescaped through the JSON reader (JSON string escapes are valid
/// YAML double-quote escapes — the same subset [`render_line`] emits).
fn parse_quoted(src: &str, chars: &mut Chars<'_>) -> Option<String> {
    let (start, quote) = chars.next()?;
    if quote != '"' {
        return None;
    }
    let mut escaped = false;
    for (i, c) in chars.by_ref() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => return serde_json::from_str::<String>(&src[start..=i]).ok(),
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Emission + splicing.
// ---------------------------------------------------------------------------------------------

/// Double-quote a scalar via the JSON writer — a valid YAML 1.1 double-quoted scalar, immune to
/// coercion.
fn quote(s: &str) -> String {
    Value::String(s.to_owned()).to_string()
}

/// A header NAME: bare where the example grammar's charset allows, double-quoted otherwise (the
/// parser accepts both, so the round trip holds for any gate-validated name).
fn header_name(name: &str) -> String {
    let bare_safe = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare_safe {
        name.to_owned()
    } else {
        quote(name)
    }
}

/// The one-line managed entry at `indent`.
fn render_line(indent: usize, entry: &McpEntry) -> String {
    let mut line = " ".repeat(indent);
    line.push_str(&entry.key);
    line.push_str(": {url: ");
    line.push_str(&quote(&entry.url));
    if !entry.headers.is_empty() {
        line.push_str(", headers: {");
        let sorted: BTreeMap<&str, &str> = entry
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let mut first = true;
        for (name, value) in sorted {
            if !first {
                line.push_str(", ");
            }
            first = false;
            line.push_str(&header_name(name));
            line.push_str(": ");
            line.push_str(&quote(value));
        }
        line.push('}');
    }
    if entry.auth == AuthHint::Oauth {
        line.push_str(", auth: oauth");
    }
    line.push_str("}  ");
    line.push_str(SENTINEL);
    line.push('\n');
    line
}

/// The fresh-file content: the key line + the managed lines, nothing else.
fn creation_block(desired: &[McpEntry]) -> String {
    let mut out = String::from("mcp_servers:\n");
    for entry in desired {
        out.push_str(&render_line(DEFAULT_INDENT, entry));
    }
    out
}

/// Append the whole block at EOF (the key was absent), guaranteeing exactly one newline before
/// it.
fn append_block(text: &str, desired: &[McpEntry], inserts: &[usize]) -> String {
    let mut out = text.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("mcp_servers:\n");
    for &i in inserts {
        out.push_str(&render_line(DEFAULT_INDENT, &desired[i]));
    }
    out
}

/// The line-surgical splice: managed lines are removed/replaced in place; inserts land directly
/// under the key line; every other line is re-emitted verbatim.
fn splice(text: &str, region: &Region, desired: &[McpEntry], rec: &Reconcile) -> String {
    let lines = split_lines(text);
    let line_of = |key: &str| -> Option<usize> {
        region
            .managed
            .iter()
            .find(|(_, k, _)| k == key)
            .map(|(i, _, _)| *i)
    };
    let removed: BTreeSet<usize> = rec.removes.iter().filter_map(|k| line_of(k)).collect();
    let updated: BTreeMap<usize, usize> = rec
        .updates
        .iter()
        .filter_map(|&i| line_of(&desired[i].key).map(|line| (line, i)))
        .collect();
    let mut out = String::with_capacity(text.len());
    for (i, line) in lines.iter().enumerate() {
        if removed.contains(&i) {
            continue;
        }
        if let Some(&d) = updated.get(&i) {
            out.push_str(&render_line(region.child_indent, &desired[d]));
            continue;
        }
        out.push_str(line);
        if i == region.key_idx {
            // The key line might be the last line of a newline-less file — the inserts need
            // their own lines.
            if !line.ends_with('\n') {
                out.push('\n');
            }
            for &d in &rec.inserts {
                out.push_str(&render_line(region.child_indent, &desired[d]));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Verification.
// ---------------------------------------------------------------------------------------------

/// By construction the splice touches only sentinel lines; this asserts it: the output minus
/// sentinel lines must be byte-identical, in order, to the input minus sentinel lines (final
/// newlines normalized), and the output must re-analyze with every planned entry landed.
fn verify(
    input: &str,
    output: &str,
    desired: &[McpEntry],
    desired_fps: &[String],
    rec: &Reconcile,
) -> Result<(), String> {
    let surprise =
        |what: &str| format!("post-edit verification failed ({what}); refusing to write");
    let non_sentinel = |text: &str| -> Vec<String> {
        split_lines(text)
            .into_iter()
            .filter(|l| !l.trim().ends_with(SENTINEL))
            .map(|l| l.strip_suffix('\n').unwrap_or(l).to_owned())
            .collect()
    };
    // Compare all-but-the-appended-key-line when the block was newly appended.
    let mut in_lines = non_sentinel(input);
    let out_lines = non_sentinel(output);
    if out_lines.len() == in_lines.len() + 1
        && out_lines.last().map(String::as_str) == Some("mcp_servers:")
    {
        in_lines.push("mcp_servers:".to_owned());
    }
    if in_lines != out_lines {
        return Err(surprise("a non-managed line changed"));
    }
    // Re-analyze: every planned entry landed at its desired value; every removed key is gone.
    let Ok(Shape::Region(region)) = analyze(output) else {
        return Err(surprise("output does not re-analyze"));
    };
    let managed_fp = |key: &str| -> Option<String> {
        region
            .managed
            .iter()
            .find(|(_, k, _)| k == key)
            .and_then(|(_, _, v)| v.as_ref().map(fingerprint_value))
    };
    for &i in rec.inserts.iter().chain(rec.updates.iter()) {
        if managed_fp(&desired[i].key).as_ref() != Some(&desired_fps[i]) {
            return Err(surprise("an entry did not land at the desired value"));
        }
    }
    for key in &rec.removes {
        if region.managed.iter().any(|(_, k, _)| k == key) {
            return Err(surprise("a removed entry is still present"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{entry, entry_with_headers};
    use super::*;

    fn fp(e: &McpEntry) -> String {
        fingerprint_value(&entry_value(McpDialect::HermesYaml, e))
    }

    fn write_of(out: &ApplyOutcome) -> String {
        match &out.plan {
            EditPlan::Write(bytes) => String::from_utf8(bytes.clone()).unwrap(),
            other => panic!("expected Write, got {other:?}"),
        }
    }

    #[test]
    fn fresh_file_creation_golden_bytes() {
        let out = apply(
            None,
            &[
                McpEntry {
                    auth: AuthHint::Oauth,
                    ..entry_with_headers(
                        "topos-acme-linear",
                        "https://mcp.example/l",
                        &[("X-T", "v")],
                    )
                },
                entry("topos-plain", "https://p.example"),
            ],
            &BTreeMap::new(),
        );
        assert!(out.created_file);
        assert_eq!(
            write_of(&out),
            "mcp_servers:\n  topos-acme-linear: {url: \"https://mcp.example/l\", headers: {X-T: \"v\"}, auth: oauth}  # topos:mcp\n  topos-plain: {url: \"https://p.example\"}  # topos:mcp\n"
        );
    }

    #[test]
    fn lifecycle_amid_comments_anchors_and_sibling_keys() {
        const USER: &str = "# hermes config\nmodel: &m gpt-9\nhooks: {}\nmcp_servers:\n  their-server: {url: \"https://theirs\"}\npersonalities:\n  default: *m\n";
        let v1 = entry("topos-x", "https://one.example");
        let v2 = entry("topos-x", "https://two.example");

        // Add: lands directly under the key line; every other line verbatim.
        let out = apply(
            Some(USER.as_bytes()),
            std::slice::from_ref(&v1),
            &BTreeMap::new(),
        );
        let after_add = write_of(&out);
        assert_eq!(
            after_add,
            "# hermes config\nmodel: &m gpt-9\nhooks: {}\nmcp_servers:\n  topos-x: {url: \"https://one.example\"}  # topos:mcp\n  their-server: {url: \"https://theirs\"}\npersonalities:\n  default: *m\n"
        );
        let ledger1: BTreeMap<String, String> = out.fingerprints.iter().cloned().collect();

        // Idempotent re-apply.
        let again = apply(
            Some(after_add.as_bytes()),
            std::slice::from_ref(&v1),
            &ledger1,
        );
        assert_eq!(again.plan, EditPlan::Leave);
        assert_eq!(
            again.states,
            vec![("topos-x".to_owned(), EntryState::Current)]
        );

        // Update in place.
        let out = apply(
            Some(after_add.as_bytes()),
            std::slice::from_ref(&v2),
            &ledger1,
        );
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Updated)]
        );
        let after_update = write_of(&out);
        assert!(after_update.contains("topos-x: {url: \"https://two.example\"}  # topos:mcp"));
        let ledger2: BTreeMap<String, String> = out.fingerprints.iter().cloned().collect();

        // Remove: back to the user-only original, byte-for-byte.
        let out = apply(Some(after_update.as_bytes()), &[], &ledger2);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Removed)]
        );
        assert_eq!(write_of(&out), USER);
    }

    #[test]
    fn append_at_eof_when_the_key_is_absent() {
        // No trailing newline on the last line — the appended block still separates cleanly.
        const USER: &str = "model: gpt-9";
        let out = apply(
            Some(USER.as_bytes()),
            &[entry("topos-x", "https://x")],
            &BTreeMap::new(),
        );
        assert!(!out.created_file, "the file existed");
        assert_eq!(
            write_of(&out),
            "model: gpt-9\nmcp_servers:\n  topos-x: {url: \"https://x\"}  # topos:mcp\n"
        );
    }

    #[test]
    fn drift_and_mangled_sentinel_lines_are_left_alone() {
        let placed = entry("topos-x", "https://placed");
        let prior: BTreeMap<String, String> =
            [("topos-x".to_owned(), fp(&placed))].into_iter().collect();

        // Hand-edited value → Drifted; removal skips it; ledger keeps the prior fingerprint.
        const DRIFTED: &str =
            "mcp_servers:\n  topos-x: {url: \"https://hand-edited\"}  # topos:mcp\n";
        let out = apply(Some(DRIFTED.as_bytes()), &[], &prior);
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Drifted)]
        );
        assert_eq!(out.fingerprints, vec![("topos-x".to_owned(), fp(&placed))]);

        // A sentinel line whose value no longer parses → Drifted, untouched, never removed.
        const MANGLED: &str = "mcp_servers:\n  topos-x: {url: [broken  # topos:mcp\n";
        let out = apply(Some(MANGLED.as_bytes()), &[], &prior);
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Drifted)]
        );
        // Even when desired: never replaced, never duplicated.
        let out = apply(Some(MANGLED.as_bytes()), &[placed], &prior);
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Drifted)]
        );
    }

    #[test]
    fn foreign_occupants_block_placement() {
        // A sentinel entry topos never recorded → Foreign.
        const SENTINEL_FOREIGN: &str =
            "mcp_servers:\n  topos-y: {url: \"https://theirs\"}  # topos:mcp\n";
        let out = apply(
            Some(SENTINEL_FOREIGN.as_bytes()),
            &[entry("topos-y", "https://ours")],
            &BTreeMap::new(),
        );
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-y".to_owned(), EntryState::Foreign)]
        );

        // A PLAIN (no sentinel) child key colliding with a desired key: inserting would mint a
        // duplicate YAML key → Foreign, no insert.
        const PLAIN: &str = "mcp_servers:\n  topos-z: {url: \"https://theirs\"}\n";
        let out = apply(
            Some(PLAIN.as_bytes()),
            &[entry("topos-z", "https://ours")],
            &BTreeMap::new(),
        );
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-z".to_owned(), EntryState::Foreign)]
        );
    }

    #[test]
    fn unprovable_shapes_fail_closed() {
        let e = [entry("topos-a", "https://a")];
        let none = BTreeMap::new();
        let unprovable_on = |text: &str, needle: &str| {
            let out = apply(Some(text.as_bytes()), &e, &none);
            let EditPlan::Unprovable(reason) = &out.plan else {
                panic!("expected Unprovable for {text:?}");
            };
            assert!(reason.contains(needle), "{reason} vs {needle}");
            assert!(out.states.is_empty() && out.fingerprints.is_empty());
        };
        unprovable_on("\u{feff}mcp_servers:\n", "BOM");
        unprovable_on(
            "mcp_servers:\n  a: {}\nmcp_servers:\n",
            "duplicate top-level",
        );
        unprovable_on("mcp_servers: {x: {url: \"u\"}}\n", "inline value");
        unprovable_on("mcp_servers: {}\n", "inline value");
        unprovable_on("mcp_servers:\n\ta: {}\n", "tab indentation");
        unprovable_on("\"mcp_servers\":\n", "spelled");
        unprovable_on("mcp_servers :\n", "spelled");
        // Deliberately NOT unprovable: a trailing comment on the key line.
        let out = apply(Some(b"mcp_servers: # servers\n"), &e, &none);
        assert!(matches!(out.plan, EditPlan::Write(_)));
    }

    #[test]
    fn off_indent_sentinel_lines_fail_the_whole_block_closed() {
        let unprovable_zero_writes = |text: &str| {
            let out = apply(
                Some(text.as_bytes()),
                &[entry("topos-x", "https://ours")],
                &BTreeMap::new(),
            );
            let EditPlan::Unprovable(reason) = &out.plan else {
                panic!("expected Unprovable for {text:?}, got {:?}", out.plan);
            };
            assert!(
                reason.contains("indent") || reason.contains("duplicate managed key"),
                "{reason}"
            );
            assert!(out.states.is_empty() && out.fingerprints.is_empty());
            // And observe is honest about the same shape: unknowable, never a guess.
            let o = observe(Some(text.as_bytes()));
            assert!(!o.parseable && o.entries.is_empty(), "{text:?}");
        };
        // THE REVIEWER'S REPRODUCTION: a pasted sentinel entry at indent 4 sits ABOVE ours at
        // indent 2. The first content line sets the detected child indent to 4, which used to
        // make OUR indent-2 line invisible — a re-apply then minted a DUPLICATE `topos-x:` line
        // (malformed YAML Hermes may reject whole, the original orphaned). Now: Unprovable,
        // zero writes.
        unprovable_zero_writes(
            "mcp_servers:\n    topos-y: {url: \"https://pasted\"}  # topos:mcp\n  topos-x: {url: \"https://ours\"}  # topos:mcp\n",
        );
        // The same key on TWO sentinel lines at different indents.
        unprovable_zero_writes(
            "mcp_servers:\n    topos-x: {url: \"https://pasted\"}  # topos:mcp\n  topos-x: {url: \"https://ours\"}  # topos:mcp\n",
        );
        // A duplicate-same-key sentinel pair at the SAME indent.
        unprovable_zero_writes(
            "mcp_servers:\n  topos-x: {url: \"https://one\"}  # topos:mcp\n  topos-x: {url: \"https://two\"}  # topos:mcp\n",
        );
        // A deeper-indented look-alike inside a nested value fails closed too — it may equally
        // be a re-indented managed line, and nothing is provable around it.
        unprovable_zero_writes(
            "mcp_servers:\n  their: {url: \"https://x\"}\n    deep: {url: \"u\"}  # topos:mcp\n",
        );
    }

    #[test]
    fn holds_unmanaged_separates_topos_only_files_from_user_content() {
        // A wholly topos-created file (key line + sentinel entries) holds nothing unmanaged;
        // neither does the post-removal skeleton or an empty file.
        let ours = write_of(&apply(
            None,
            &[entry("topos-x", "https://x")],
            &BTreeMap::new(),
        ));
        assert!(!holds_unmanaged(ours.as_bytes()));
        assert!(!holds_unmanaged(b"mcp_servers:\n"));
        assert!(!holds_unmanaged(b""));
        // User content answers true: a plain child entry, another top-level key, a comment, a
        // trailing comment on the key line, and every unprovable shape.
        assert!(holds_unmanaged(b"mcp_servers:\n  their: {url: \"u\"}\n"));
        assert!(holds_unmanaged(b"model: gpt-9\nmcp_servers:\n"));
        assert!(holds_unmanaged(b"# my config\nmcp_servers:\n"));
        assert!(holds_unmanaged(b"mcp_servers: # servers\n"));
        assert!(holds_unmanaged("\u{feff}mcp_servers:\n".as_bytes()));
    }

    #[test]
    fn header_and_auth_rendering_round_trips() {
        // A header value needing escapes + a name outside the bare charset.
        let e = McpEntry {
            auth: AuthHint::Oauth,
            ..entry_with_headers(
                "topos-h",
                "https://h.example/path?q=1",
                &[("X-Quote", "say \"hi\""), ("odd name", "v")],
            )
        };
        let out = apply(None, std::slice::from_ref(&e), &BTreeMap::new());
        let text = write_of(&out);
        assert!(text.contains("\"odd name\": \"v\""), "{text}");
        assert!(text.contains("X-Quote: \"say \\\"hi\\\"\""), "{text}");
        assert!(text.ends_with("auth: oauth}  # topos:mcp\n"));

        // The emitted line parses back to the exact desired fingerprint → a re-apply is Current.
        let ledger: BTreeMap<String, String> = out.fingerprints.iter().cloned().collect();
        let again = apply(Some(text.as_bytes()), std::slice::from_ref(&e), &ledger);
        assert_eq!(again.plan, EditPlan::Leave);
        assert_eq!(
            again.states,
            vec![("topos-h".to_owned(), EntryState::Current)]
        );
    }

    #[test]
    fn observe_reads_without_writing() {
        let o = observe(None);
        assert!(o.parseable && o.entries.is_empty());
        let placed = entry("topos-x", "https://x");
        let text = write_of(&apply(
            None,
            std::slice::from_ref(&placed),
            &BTreeMap::new(),
        ));
        let o = observe(Some(text.as_bytes()));
        assert!(o.parseable);
        assert_eq!(o.entries.get("topos-x"), Some(&fp(&placed)));
        let o = observe(Some("\u{feff}mcp_servers:\n".as_bytes()));
        assert!(!o.parseable && o.entries.is_empty());
    }
}
