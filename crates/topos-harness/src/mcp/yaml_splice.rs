//! The YAML `config.yaml` splicer — [`McpDialect::HermesYaml`] and [`McpDialect::GooseYaml`],
//! hand-rolled in the exact
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
//! TWO harnesses share it and differ in exactly three things, all of them held in one internal
//! spec: the top-level key (`mcp_servers` / `extensions`), the display name a refusal carries, and
//! the entry grammar the line renderer emits. Goose is the harder of the two, because the block
//! topos writes into is
//! the one goose's OWN bundled extensions live in — each of them a block mapping several lines
//! deep. They are read (a name, and no address this parser claims to have read) and never
//! touched; the sentinel is what separates them from topos's, and a name that merely LOOKS like
//! topos's without one is Foreign, exactly as everywhere else.
//!
//! `auth: oauth` is emitted ONLY for [`AuthHint::Oauth`] (Hermes needs the explicit opt-in but
//! must not be sent into OAuth for a no-auth server) and ONLY for Hermes, whose key it is; goose
//! runs its own sign-in and would find the word unreadable. The url and header values are ALWAYS
//! double-quoted (PyYAML is YAML 1.1 — an unquoted scalar can coerce); header names follow the
//! example spelling (bare for `[A-Za-z0-9_-]`, double-quoted otherwise); the entry key stays
//! bare (`[a-z0-9-]` is safe). Managed lines are identified by the sentinel suffix ALONE, and
//! must sit at the block's own child indent: a sentinel-bearing line at ANY other indent inside
//! the block fails the whole analysis closed — it may be OUR OWN entry re-indented (or a pasted
//! copy of one), and reasoning around it could re-insert a key the block already holds, minting
//! a duplicate YAML key that bricks the whole config.
//!
//! Unprovable (zero writes): a BOM; duplicate key lines; a flow-style value on the key
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

#[cfg(test)]
use super::entry_value;
use super::{
    ApplyOutcome, AuthHint, EditPlan, EntryState, FoundEntry, MANAGED_KEY_PREFIX, McpDialect,
    McpEntry, Observed, Reconcile, effectively_absent, fingerprint_value, outcome, reconcile,
    unprovable, validate_desired,
};

/// The line-ownership sentinel (a YAML comment outside the flow mapping).
const SENTINEL: &str = "# topos:mcp";

/// One YAML surface's parameters: the top-level key this splicer may touch the block of (plain
/// spelling only, colon included), the harness display name an honest refusal names, and the
/// dialect whose entry grammar [`render_line`] emits.
///
/// TWO harnesses share this driver and nothing else: Hermes lists its servers under
/// `mcp_servers`, Goose its extensions under `extensions`, and the one-line flow-mapping idiom is
/// legal YAML in both. Everything that differs between them is here.
#[derive(Debug, Clone, Copy)]
struct YamlSpec {
    /// The key line's exact spelling, colon included (`mcp_servers:`).
    key: &'static str,
    /// The bare key, for the refusals a person reads.
    word: &'static str,
    /// The harness's display name, same purpose.
    name: &'static str,
    dialect: McpDialect,
}

const HERMES: YamlSpec = YamlSpec {
    key: "mcp_servers:",
    word: "mcp_servers",
    name: "Hermes",
    dialect: McpDialect::HermesYaml,
};

/// Goose keeps its MCP servers among its EXTENSIONS, which is also where its own built-in ones
/// live — hence the sentinel, and hence the rule that nothing without it is ever touched.
const GOOSE: YamlSpec = YamlSpec {
    key: "extensions:",
    word: "extensions",
    name: "Goose",
    dialect: McpDialect::GooseYaml,
};

/// The surface `dialect` names — the one place this driver's two harnesses are told apart.
fn spec(dialect: McpDialect) -> YamlSpec {
    match dialect {
        McpDialect::GooseYaml => GOOSE,
        // UNREPRESENTABLE, not worded: `super::apply` is the one dispatcher and routes every other
        // dialect to its own editor before this is reached (`every_yaml_dialect_has_a_spec`).
        _ => HERMES,
    }
}

/// The child indent used when the block is empty (or freshly created).
const DEFAULT_INDENT: usize = 2;

/// Compute the placement plan for one YAML `config.yaml`. See the module doc for the contract.
#[must_use]
pub fn apply(
    dialect: McpDialect,
    current: Option<&[u8]>,
    desired: &[McpEntry],
    prior: &BTreeMap<String, String>,
) -> ApplyOutcome {
    let spec = spec(dialect);
    if let Err(reason) = validate_desired(desired) {
        return unprovable(reason);
    }
    // A PROGRAM-run server has no proven spelling in this file (see the module doc), so the whole
    // edit refuses before a line is planned rather than inventing one.
    let desired_fps: Vec<String> = match super::desired_fingerprints(dialect, desired) {
        Ok(fps) => fps,
        Err(reason) => return unprovable(reason),
    };

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
            EditPlan::Write(creation_block(spec, desired).into_bytes()),
            rec,
            true,
        );
    }
    let Ok(text) = std::str::from_utf8(current.unwrap_or_default()) else {
        return unprovable(format!("the {} config is not UTF-8", spec.name));
    };
    let shape = match analyze(spec, text) {
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
            for (key, _) in &region.plain {
                if !(key.starts_with(MANAGED_KEY_PREFIX) || desired_keys.contains(key.as_str())) {
                    continue;
                }
                if found.contains_key(key) {
                    return unprovable(format!(
                        "the {} config lists `{key}` twice under {}",
                        spec.name, spec.word
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
        None => append_block(spec, text, desired, &rec.inserts),
        Some(region) => splice(spec, text, region, desired, &rec),
    };
    if let Err(reason) = verify(spec, text, &out_text, desired, &desired_fps, &rec) {
        return unprovable(reason);
    }
    outcome(EditPlan::Write(out_text.into_bytes()), rec, false)
}

/// Read the surface without writing: `key → fingerprint` for every parseable sentinel-marked
/// entry (a mangled sentinel line has no structural value to report; `apply` classifies it
/// `Drifted`).
#[must_use]
pub fn observe(dialect: McpDialect, current: Option<&[u8]>) -> Observed {
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
    let Ok(shape) = analyze(spec(dialect), text) else {
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

/// Every entry under the dialect's own key — sentinel-marked and plain alike (see
/// [`super::observe_entries`]). A `selector` cannot be honoured in this dialect (the splicer reads
/// exactly one block, by construction), so one answers UNREADABLE rather than a wrong slot.
///
/// An entry whose line is outside the flow grammar this file parses keeps its NAME and answers no
/// address: a name collision is still provable, and an address that was never read is never
/// claimed to match.
#[must_use]
pub fn observe_entries(
    dialect: McpDialect,
    current: Option<&[u8]>,
    selector: Option<&str>,
) -> Option<Vec<super::SeenEntry>> {
    if selector.is_some() {
        return None;
    }
    if effectively_absent(current) {
        return Some(Vec::new());
    }
    let text = std::str::from_utf8(current.unwrap_or_default()).ok()?;
    let Shape::Region(region) = analyze(spec(dialect), text).ok()? else {
        return Some(Vec::new());
    };
    let seen = |(name, value): (&String, &Option<Value>)| super::SeenEntry {
        name: name.clone(),
        address: value.as_ref().and_then(super::entry_address),
    };
    Some(
        region
            .managed
            .iter()
            .map(|(_, key, value)| seen((key, value)))
            .chain(region.plain.iter().map(|(k, v)| seen((k, v))))
            .collect(),
    )
}

/// Whether the file holds content beyond what topos itself writes: anything but the one bare
/// key line, blank lines, and sentinel-managed entry lines at the child indent.
/// Comments, plain child keys, other top-level keys, and every unprovable shape answer `true` —
/// the caller's whole-file-ownership reasoning fails TOWARD keeping the file.
#[must_use]
pub fn holds_unmanaged(dialect: McpDialect, bytes: &[u8]) -> bool {
    let spec = spec(dialect);
    let Ok(text) = std::str::from_utf8(bytes) else {
        return true;
    };
    let Ok(shape) = analyze(spec, text) else {
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
            return line.trim_end() != spec.key;
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

/// The parsed key's region.
struct Region {
    /// Index of the key line.
    key_idx: usize,
    /// The block's child indent (first content line's, else 2).
    child_indent: usize,
    /// Sentinel-marked lines at the child indent: `(line_idx, key, parsed value)` — `None` value
    /// = unparsable (forced `Drifted`).
    managed: Vec<(usize, String, Option<Value>)>,
    /// Plain (non-sentinel) mapping entries at the child indent — the key, and its value where
    /// the line is a flow mapping this parser reads. The KEY is what collision detection needs;
    /// the value is read for one question only, "which server does this name point at"
    /// ([`observe_entries`]), and is `None` for any shape beyond the flow grammar.
    plain: Vec<(String, Option<Value>)>,
}

enum Shape {
    /// No top-level key line — a block can be appended at EOF.
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

/// A zero-indent line that spells the dialect's key in a form OTHER than the one canonical
/// spelling — quoted, or with whitespace before the colon. Such a file already HAS the
/// key; reasoning about the canonical spelling beside it would be wrong, so it forces the
/// fail-closed path.
fn is_alternate_spelling(spec: YamlSpec, line: &str) -> bool {
    let quoted = |q: char| {
        line.starts_with(q)
            && line[1..].starts_with(spec.word)
            && line[1 + spec.word.len()..].starts_with(q)
    };
    quoted('"')
        || quoted('\'')
        || (line.starts_with(spec.word)
            && matches!(line.as_bytes().get(spec.word.len()), Some(b' ' | b'\t')))
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

fn analyze(spec: YamlSpec, text: &str) -> Result<Shape, String> {
    // A byte-order mark hides the first line's true column 0 from every zero-indent anchor.
    if text.starts_with('\u{feff}') {
        return Err(format!(
            "{} config carries a BOM; refusing to reason about column 0",
            spec.name
        ));
    }
    let lines = split_lines(text);
    if lines.iter().any(|l| is_alternate_spelling(spec, l)) {
        return Err(format!(
            "{} is spelled in a form this merge never writes",
            spec.word
        ));
    }
    let key_lines: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with(spec.key))
        .map(|(i, _)| i)
        .collect();
    let key_idx = match key_lines.as_slice() {
        [] => return Ok(Shape::NoKey),
        [one] => *one,
        _ => return Err(format!("duplicate top-level {} keys", spec.word)),
    };
    // Any non-comment remainder after the colon (a flow value, an anchor, a tag) is a shape this
    // splicer cannot own lines under.
    let remainder = lines[key_idx][spec.key.len()..].trim();
    if !remainder.is_empty() && !remainder.starts_with('#') {
        return Err(format!(
            "{} carries an inline value; only a block form is provable",
            spec.word
        ));
    }
    let end = region_end(&lines, key_idx);
    for line in &lines[key_idx + 1..end] {
        let leading = &line[..line.len() - line.trim_start().len()];
        if leading.contains('\t') {
            return Err(format!("tab indentation inside the {} block", spec.word));
        }
    }
    let child_indent = (key_idx + 1..end)
        .map(|i| lines[i])
        .find(|l| !is_blank_or_comment(l))
        .map_or(DEFAULT_INDENT, indent_of);

    let mut managed: Vec<(usize, String, Option<Value>)> = Vec::new();
    let mut plain: Vec<(String, Option<Value>)> = Vec::new();
    for (i, line) in lines.iter().enumerate().take(end).skip(key_idx + 1) {
        let trimmed = line.trim();
        if trimmed.ends_with(SENTINEL) {
            if indent_of(line) != child_indent {
                // FAIL-CLOSED, never skipped: an off-indent sentinel line may be OUR entry
                // re-indented (or a pasted copy). Treating it as invisible would let the splice
                // re-insert a key the block already holds — a duplicate YAML key that can brick
                // the whole config — or silently drop the original from every report.
                return Err(format!(
                    "a topos:mcp sentinel line sits at an unexpected indent under {}; refusing \
                     to touch the block",
                    spec.word
                ));
            }
            let Some((key, body)) = trimmed.split_once(':') else {
                return Err("a topos:mcp sentinel line has no readable key".to_owned());
            };
            let key = key.trim();
            if key.is_empty() || key.contains(char::is_whitespace) {
                return Err("a topos:mcp sentinel line has no readable key".to_owned());
            }
            if managed.iter().any(|(_, k, _)| k == key) {
                return Err(format!("duplicate managed key `{key}` under {}", spec.word));
            }
            let body = body.trim_end().strip_suffix(SENTINEL).map(str::trim_end);
            let value = body.and_then(parse_flow_mapping);
            managed.push((i, key.to_owned(), value));
        } else if !is_blank_or_comment(line) && indent_of(line) == child_indent {
            // A plain mapping key at the child indent (`key: …`) — recorded for collision
            // detection only; its content is never read further.
            if let Some((key, body)) = line.trim().split_once(':')
                && !key.is_empty()
                && !key.contains(char::is_whitespace)
                && !key.starts_with('-')
            {
                plain.push((key.to_owned(), parse_flow_mapping(body.trim())));
            }
        }
    }
    Ok(Shape::Region(Region {
        key_idx,
        child_indent,
        managed,
        plain,
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
                // A bare `true`/`false` is a BOOLEAN in YAML, and reading it back as the string
                // "true" would make an entry topos itself just wrote fingerprint differently from
                // the value it rendered — permanent, silent drift on the one harness whose entry
                // carries a boolean at all. Every other bare word stays the text it is.
                match word {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    _ => Value::String(word.to_owned()),
                }
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

/// The one-line managed entry at `indent`. ADDRESS entries only — `apply` refuses a desired set
/// carrying a program-run server before any line is planned, so the address fields are total here.
fn render_line(spec: YamlSpec, indent: usize, entry: &McpEntry) -> String {
    let mut line = " ".repeat(indent);
    line.push_str(&entry.key);
    let (url, headers) = match &entry.target {
        super::McpTarget::Remote { url, headers } => (url.as_str(), headers.as_slice()),
        super::McpTarget::Local { .. } => ("", &[][..]),
    };
    line.push_str(": {");
    // Goose reads an extension, not a server: it needs the switch that turns one on, the name it
    // is filed under (which it also injects from the key, but writing it costs nothing and its
    // own tool writes it), and the transport word spelled the way its config enum reads. The
    // address key is `uri` there, `url` in Hermes.
    if spec.dialect == McpDialect::GooseYaml {
        line.push_str("enabled: true, name: ");
        line.push_str(&entry.key);
        line.push_str(", type: streamable_http, uri: ");
    } else {
        line.push_str("url: ");
    }
    line.push_str(&quote(url));
    if !headers.is_empty() {
        line.push_str(", headers: {");
        let sorted: BTreeMap<&str, &str> = headers
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
    // `auth: oauth` is HERMES's opt-in. Goose has no such key: it runs its own OAuth on a 401,
    // and a word its config enum does not know would make the whole entry unreadable to it.
    if entry.auth == AuthHint::Oauth && spec.dialect == McpDialect::HermesYaml {
        line.push_str(", auth: oauth");
    }
    line.push_str("}  ");
    line.push_str(SENTINEL);
    line.push('\n');
    line
}

/// The fresh-file content: the key line + the managed lines, nothing else.
fn creation_block(spec: YamlSpec, desired: &[McpEntry]) -> String {
    let mut out = format!("{}\n", spec.key);
    for entry in desired {
        out.push_str(&render_line(spec, DEFAULT_INDENT, entry));
    }
    out
}

/// Append the whole block at EOF (the key was absent), guaranteeing exactly one newline before
/// it.
fn append_block(spec: YamlSpec, text: &str, desired: &[McpEntry], inserts: &[usize]) -> String {
    let mut out = text.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(spec.key);
    out.push('\n');
    for &i in inserts {
        out.push_str(&render_line(spec, DEFAULT_INDENT, &desired[i]));
    }
    out
}

/// The line-surgical splice: managed lines are removed/replaced in place; inserts land directly
/// under the key line; every other line is re-emitted verbatim.
fn splice(
    spec: YamlSpec,
    text: &str,
    region: &Region,
    desired: &[McpEntry],
    rec: &Reconcile,
) -> String {
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
            out.push_str(&render_line(spec, region.child_indent, &desired[d]));
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
                out.push_str(&render_line(spec, region.child_indent, &desired[d]));
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
    spec: YamlSpec,
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
        && out_lines.last().map(String::as_str) == Some(spec.key)
    {
        in_lines.push(spec.key.to_owned());
    }
    if in_lines != out_lines {
        return Err(surprise("a non-managed line changed"));
    }
    // Re-analyze: every planned entry landed at its desired value; every removed key is gone.
    let Ok(Shape::Region(region)) = analyze(spec, output) else {
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
        fingerprint_value(
            &entry_value(McpDialect::HermesYaml, e).expect("this dialect renders an address"),
        )
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
            McpDialect::HermesYaml,
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
            McpDialect::HermesYaml,
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
            McpDialect::HermesYaml,
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
            McpDialect::HermesYaml,
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
        let out = apply(
            McpDialect::HermesYaml,
            Some(after_update.as_bytes()),
            &[],
            &ledger2,
        );
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
            McpDialect::HermesYaml,
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
        let out = apply(
            McpDialect::HermesYaml,
            Some(DRIFTED.as_bytes()),
            &[],
            &prior,
        );
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Drifted)]
        );
        assert_eq!(out.fingerprints, vec![("topos-x".to_owned(), fp(&placed))]);

        // A sentinel line whose value no longer parses → Drifted, untouched, never removed.
        const MANGLED: &str = "mcp_servers:\n  topos-x: {url: [broken  # topos:mcp\n";
        let out = apply(
            McpDialect::HermesYaml,
            Some(MANGLED.as_bytes()),
            &[],
            &prior,
        );
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Drifted)]
        );
        // Even when desired: never replaced, never duplicated.
        let out = apply(
            McpDialect::HermesYaml,
            Some(MANGLED.as_bytes()),
            &[placed],
            &prior,
        );
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
            McpDialect::HermesYaml,
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
            McpDialect::HermesYaml,
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
            let out = apply(McpDialect::HermesYaml, Some(text.as_bytes()), &e, &none);
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
        let out = apply(
            McpDialect::HermesYaml,
            Some(b"mcp_servers: # servers\n"),
            &e,
            &none,
        );
        assert!(matches!(out.plan, EditPlan::Write(_)));
    }

    #[test]
    fn off_indent_sentinel_lines_fail_the_whole_block_closed() {
        let unprovable_zero_writes = |text: &str| {
            let out = apply(
                McpDialect::HermesYaml,
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
            let o = observe(McpDialect::HermesYaml, Some(text.as_bytes()));
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
            McpDialect::HermesYaml,
            None,
            &[entry("topos-x", "https://x")],
            &BTreeMap::new(),
        ));
        assert!(!holds_unmanaged(McpDialect::HermesYaml, ours.as_bytes()));
        assert!(!holds_unmanaged(McpDialect::HermesYaml, b"mcp_servers:\n"));
        assert!(!holds_unmanaged(McpDialect::HermesYaml, b""));
        // User content answers true: a plain child entry, another top-level key, a comment, a
        // trailing comment on the key line, and every unprovable shape.
        assert!(holds_unmanaged(
            McpDialect::HermesYaml,
            b"mcp_servers:\n  their: {url: \"u\"}\n"
        ));
        assert!(holds_unmanaged(
            McpDialect::HermesYaml,
            b"model: gpt-9\nmcp_servers:\n"
        ));
        assert!(holds_unmanaged(
            McpDialect::HermesYaml,
            b"# my config\nmcp_servers:\n"
        ));
        assert!(holds_unmanaged(
            McpDialect::HermesYaml,
            b"mcp_servers: # servers\n"
        ));
        assert!(holds_unmanaged(
            McpDialect::HermesYaml,
            "\u{feff}mcp_servers:\n".as_bytes()
        ));
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
        let out = apply(
            McpDialect::HermesYaml,
            None,
            std::slice::from_ref(&e),
            &BTreeMap::new(),
        );
        let text = write_of(&out);
        assert!(text.contains("\"odd name\": \"v\""), "{text}");
        assert!(text.contains("X-Quote: \"say \\\"hi\\\"\""), "{text}");
        assert!(text.ends_with("auth: oauth}  # topos:mcp\n"));

        // The MANUAL hint is not a quieter oauth: Hermes's key opens a flow that registers a
        // client, and a manual server registers nobody — so the line carries no `auth` at all,
        // exactly like an unknown one.
        let manual = McpEntry {
            auth: AuthHint::Manual,
            ..entry("topos-m", "https://m.example/mcp")
        };
        let manual_text = write_of(&apply(
            McpDialect::HermesYaml,
            None,
            std::slice::from_ref(&manual),
            &BTreeMap::new(),
        ));
        assert!(!manual_text.contains("auth:"), "{manual_text}");

        // The emitted line parses back to the exact desired fingerprint → a re-apply is Current.
        let ledger: BTreeMap<String, String> = out.fingerprints.iter().cloned().collect();
        let again = apply(
            McpDialect::HermesYaml,
            Some(text.as_bytes()),
            std::slice::from_ref(&e),
            &ledger,
        );
        assert_eq!(again.plan, EditPlan::Leave);
        assert_eq!(
            again.states,
            vec![("topos-h".to_owned(), EntryState::Current)]
        );
    }

    #[test]
    fn observe_reads_without_writing() {
        let o = observe(McpDialect::HermesYaml, None);
        assert!(o.parseable && o.entries.is_empty());
        let placed = entry("topos-x", "https://x");
        let text = write_of(&apply(
            McpDialect::HermesYaml,
            None,
            std::slice::from_ref(&placed),
            &BTreeMap::new(),
        ));
        let o = observe(McpDialect::HermesYaml, Some(text.as_bytes()));
        assert!(o.parseable);
        assert_eq!(o.entries.get("topos-x"), Some(&fp(&placed)));
        let o = observe(
            McpDialect::HermesYaml,
            Some("\u{feff}mcp_servers:\n".as_bytes()),
        );
        assert!(!o.parseable && o.entries.is_empty());
    }
}
