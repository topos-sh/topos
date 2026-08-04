//! THE MCP SERVER-DOCUMENT GATE, client side — the exact mirror of the web tier's gate.
//!
//! What a shared MCP bundle IS, here: a REMOTE server an agent can reach over `streamable-http`
//! with the exact bytes the workspace holds — no local install, no per-machine fill-in, and no
//! credential anywhere in the document. Everything this refuses refuses for one of those three
//! reasons, and every refusal is TYPED ([`McpRefusalCode`]) so the caller says plainly what is
//! wrong instead of "invalid".
//!
//! **One source of truth, two languages.** The rules, the refusal codes, and the credential shapes
//! live at the repo root — `tests/fixtures/mcp/{vectors,secret-patterns}.json`. The web tier
//! generates its pattern module from that file; THIS module compiles the same list in, and its
//! tests read the JSON back and prove both the SPELLING (every regex source, byte-for-byte, in
//! order) and the BEHAVIOUR (every matcher against the real regex, over a probe corpus) still
//! agree. A rule cannot change on one side without the vectors — and therefore the other side —
//! changing too.
//!
//! **ORDER MATTERS.** The credential scan runs FIRST, over the whole raw text, immediately after
//! the JSON parses. A document carrying somebody's token must never be read further, previewed,
//! or echoed back in a field-level error message — the refusal comes before anything is taken out
//! of it.
//!
//! ## Why matcher functions instead of a regex engine
//!
//! Every shape in the list is `<literal prefix><character class>{min,…}`. Existence of a match is
//! therefore decidable by "find the prefix, count the run" — no backtracking, no engine, and no
//! `regex` crate in the shipped binary (which would add four crates to a client whose whole point
//! is being a small static download). The engine is a DEV-dependency instead, used by the
//! equivalence test below: it compiles the JSON's own regex sources and asserts, over a cross
//! product of probes, that every matcher answers exactly what its regex answers.

use serde_json::Value;

/// A hard ceiling on the document itself — a server.json is a page of text, never a payload.
pub(crate) const MAX_SERVER_JSON_BYTES: usize = 256 * 1024;

/// The one transport a shared bundle can promise: the same URL works from every machine.
pub(crate) const STREAMABLE_HTTP: &str = "streamable-http";

/// The registry's name grammar: `<reverse.dns.namespace>/<server-name>`, exactly one slash.
const NAME_MIN: usize = 3;
const NAME_MAX: usize = 200;
const DESCRIPTION_MAX: usize = 100;
const VERSION_MAX: usize = 255;

// =================================================================================================
// The typed refusal
// =================================================================================================

/// The six ways a server document is refused. An OPEN-ended type in spirit but CLOSED in code: a
/// new reason gets a code here, a vector at the repo root, and a web-tier twin — never a folded
/// "invalid".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpRefusalCode {
    /// Not JSON, or the required registry fields are missing / malformed.
    Invalid,
    /// A non-empty `packages[]`: the server installs and runs locally.
    LocalRefused,
    /// No `streamable-http` remote (sse-only, or no remotes at all).
    NoStreamableRemote,
    /// The endpoint is plain http.
    InsecureUrl,
    /// The endpoint carries a `{placeholder}`, so it is not an address.
    UrlTemplate,
    /// The document carries (or reserves a slot for) a credential.
    SecretRefused,
}

impl McpRefusalCode {
    /// The wire spelling — the same string the web tier and the vectors use.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            McpRefusalCode::Invalid => "MCP_INVALID",
            McpRefusalCode::LocalRefused => "MCP_LOCAL_REFUSED",
            McpRefusalCode::NoStreamableRemote => "MCP_NO_STREAMABLE_REMOTE",
            McpRefusalCode::InsecureUrl => "MCP_INSECURE_URL",
            McpRefusalCode::UrlTemplate => "MCP_URL_TEMPLATE",
            McpRefusalCode::SecretRefused => "MCP_SECRET_REFUSED",
        }
    }
}

/// One refused document: the machine-branchable code plus the sentence a person reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpRefusal {
    pub code: McpRefusalCode,
    pub message: String,
}

fn refuse<T>(code: McpRefusalCode, message: impl Into<String>) -> Result<T, McpRefusal> {
    Err(McpRefusal {
        code,
        message: message.into(),
    })
}

// =================================================================================================
// What survives the gate
// =================================================================================================

/// A header that survived the gate: a literal name and a literal value, nothing to fill in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpHeader {
    pub name: String,
    pub value: String,
}

/// The publisher's own `_meta["sh.topos/auth"]` word, when it says something this tier
/// understands. `None` means the document declared nothing — which is NOT the same claim as
/// `"none"`, so it is never upgraded to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpAuthHint {
    /// The agent will be sent through an authorization dance on first use.
    Oauth,
    /// The publisher stated outright that no credential is needed.
    None,
}

impl McpAuthHint {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            McpAuthHint::Oauth => "oauth",
            McpAuthHint::None => "none",
        }
    }
}

/// What a receipt shows and what a describe reports — DERIVED, never the document echoed whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpSummary {
    pub name: String,
    pub description: String,
    pub version: String,
    pub url: String,
    pub transport: &'static str,
    pub headers: Vec<McpHeader>,
    pub auth_hint: Option<McpAuthHint>,
}

// =================================================================================================
// The credential scan
// =================================================================================================

/// One named credential shape: the JSON's own regex SOURCE (kept for the drift assert) plus the
/// matcher that decides it. Both halves come from `tests/fixtures/mcp/secret-patterns.json`.
struct SecretPattern {
    name: &'static str,
    /// The ECMAScript regex source, byte-for-byte as the JSON spells it. Nothing at RUNTIME reads
    /// it — it is the drift gate's evidence, and it lives beside the matcher precisely so the two
    /// cannot be edited apart.
    #[cfg_attr(not(test), allow(dead_code))]
    regex: &'static str,
    is_match: fn(&str) -> bool,
}

/// The entropy belt below the named shapes — the JSON's `entropy` block.
const ENTROPY_MIN_LENGTH: usize = 24;
const ENTROPY_THRESHOLD: f64 = 4.2;

/// `[A-Za-z0-9]`
fn alnum(c: char) -> bool {
    c.is_ascii_alphanumeric()
}
/// `[A-Za-z0-9_]`
fn alnum_us(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}
/// `[A-Za-z0-9_-]` (also spelled `[0-9A-Za-z_-]`)
fn alnum_us_dash(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}
/// `[A-Za-z0-9-]`
fn alnum_dash(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}
/// `[0-9A-Z]`
fn upper_digit(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit()
}
/// `[A-Za-z0-9._~+/-]`
fn bearer_class(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '+' | '/' | '-')
}

/// How many leading chars of `s` belong to `class`.
fn run_len(s: &str, class: fn(char) -> bool) -> usize {
    s.chars().take_while(|c| class(*c)).count()
}

/// Does `text` hold `lit` followed by at least `min` chars of `class`?
///
/// This is the existence question a `{min,max}` (or `{min,}`, or exact `{n}`) quantifier answers
/// when unanchored: the engine may always take exactly `min`, so "at least `min` follow" is
/// necessary and sufficient. `lit` is ASCII by construction, which keeps every index below on a
/// char boundary.
fn literal_run(text: &str, lit: &str, class: fn(char) -> bool, min: usize) -> bool {
    debug_assert!(lit.is_ascii(), "literal prefixes are ASCII by construction");
    let mut from = 0usize;
    while let Some(off) = text[from..].find(lit) {
        let hit = from + off;
        if run_len(&text[hit + lit.len()..], class) >= min {
            return true;
        }
        from = hit + 1;
    }
    false
}

/// `gh[pousr]_[A-Za-z0-9]{36,255}` — the bracket is five literals.
fn github_token(text: &str) -> bool {
    ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"]
        .iter()
        .any(|lit| literal_run(text, lit, alnum, 36))
}

/// `xox[bp]-[A-Za-z0-9-]{10,}` — likewise two literals.
fn slack_token(text: &str) -> bool {
    ["xoxb-", "xoxp-"]
        .iter()
        .any(|lit| literal_run(text, lit, alnum_dash, 10))
}

/// `Bearer\s+[A-Za-z0-9._~+/-]{20,}=*`
///
/// The `\s+` is greedy and the token class holds no whitespace, so backtracking can never help:
/// consuming ALL the whitespace is the only way the class run can start. The trailing `=*` is
/// optional and cannot change whether a match exists.
fn bearer_credential(text: &str) -> bool {
    const LIT: &str = "Bearer";
    let mut from = 0usize;
    while let Some(off) = text[from..].find(LIT) {
        let hit = from + off;
        let rest = &text[hit + LIT.len()..];
        let ws: usize = rest
            .chars()
            .take_while(|c| c.is_whitespace())
            .map(char::len_utf8)
            .sum();
        if ws > 0 && run_len(&rest[ws..], bearer_class) >= 20 {
            return true;
        }
        from = hit + 1;
    }
    false
}

/// THE list — same names, same order, same regex sources as
/// `tests/fixtures/mcp/secret-patterns.json`. The tests below prove both.
const SECRET_PATTERNS: &[SecretPattern] = &[
    SecretPattern {
        name: "github-token",
        regex: r"gh[pousr]_[A-Za-z0-9]{36,255}",
        is_match: github_token,
    },
    SecretPattern {
        name: "github-fine-grained-token",
        regex: r"github_pat_[A-Za-z0-9_]{22,255}",
        is_match: |t| literal_run(t, "github_pat_", alnum_us, 22),
    },
    SecretPattern {
        name: "openai-style-key",
        regex: r"sk-[A-Za-z0-9_-]{20,}",
        is_match: |t| literal_run(t, "sk-", alnum_us_dash, 20),
    },
    SecretPattern {
        name: "stripe-live-key",
        regex: r"sk_live_[A-Za-z0-9]{16,}",
        is_match: |t| literal_run(t, "sk_live_", alnum, 16),
    },
    SecretPattern {
        name: "stripe-test-key",
        regex: r"sk_test_[A-Za-z0-9]{16,}",
        is_match: |t| literal_run(t, "sk_test_", alnum, 16),
    },
    SecretPattern {
        name: "slack-token",
        regex: r"xox[bp]-[A-Za-z0-9-]{10,}",
        is_match: slack_token,
    },
    SecretPattern {
        name: "aws-access-key-id",
        regex: r"AKIA[0-9A-Z]{16}",
        is_match: |t| literal_run(t, "AKIA", upper_digit, 16),
    },
    SecretPattern {
        name: "gitlab-token",
        regex: r"glpat-[A-Za-z0-9_-]{16,}",
        is_match: |t| literal_run(t, "glpat-", alnum_us_dash, 16),
    },
    SecretPattern {
        name: "google-oauth-access-token",
        regex: r"ya29\.[A-Za-z0-9_-]{20,}",
        is_match: |t| literal_run(t, "ya29.", alnum_us_dash, 20),
    },
    SecretPattern {
        name: "google-api-key",
        regex: r"AIza[0-9A-Za-z_-]{35}",
        is_match: |t| literal_run(t, "AIza", alnum_us_dash, 35),
    },
    SecretPattern {
        name: "bearer-credential",
        regex: r"Bearer\s+[A-Za-z0-9._~+/-]{20,}=*",
        is_match: bearer_credential,
    },
];

/// Shannon entropy in bits per character.
fn entropy_of(token: &str) -> f64 {
    let mut counts: std::collections::BTreeMap<char, usize> = std::collections::BTreeMap::new();
    let mut total = 0usize;
    for c in token.chars() {
        *counts.entry(c).or_insert(0) += 1;
        total += 1;
    }
    if total == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let len = total as f64;
    let mut bits = 0.0f64;
    for n in counts.values() {
        #[allow(clippy::cast_precision_loss)]
        let p = *n as f64 / len;
        bits -= p * p.log2();
    }
    bits
}

/// The token alphabet the entropy belt walks: `[A-Za-z0-9_+=-]`. Deliberately NARROW — no dot, no
/// slash — so a hostname or a URL path splits into its parts instead of concatenating into one
/// long high-entropy-looking run. Real credentials are contiguous in this alphabet; addresses are
/// not.
fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '=' | '-')
}

/// Does this token READ as a random secret? Entropy alone does not separate `sk-…` from an
/// ordinary English phrase (both land near 4 bits/char), so two shapes qualify and nothing else:
///
/// - MIXED-CLASS — long enough, entropy at or past the threshold, AND lower + upper + digit all
///   present. Prose and slugs fail the class test; generated keys almost never do.
/// - LONG HEX — 32 or more characters of pure lowercase (or pure uppercase) hex. Its entropy is
///   only 4 bits/char by construction, so the threshold would miss it, and it is the other common
///   key spelling.
fn looks_random(token: &str) -> bool {
    let len = token.chars().count();
    if len >= 32
        && token
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f'))
    {
        return true;
    }
    if len >= 32
        && token
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, 'A'..='F'))
    {
        return true;
    }
    if len < ENTROPY_MIN_LENGTH {
        return false;
    }
    let mixed = token.chars().any(|c| c.is_ascii_lowercase())
        && token.chars().any(|c| c.is_ascii_uppercase())
        && token.chars().any(|c| c.is_ascii_digit());
    mixed && entropy_of(token) >= ENTROPY_THRESHOLD
}

/// The maximal runs of [`token_char`] at least 8 long — what the belt inspects. (The web tier's
/// `[A-Za-z0-9_+=-]{8,}` global match yields exactly these.)
fn token_runs(raw: &str) -> impl Iterator<Item = &str> {
    raw.split(|c: char| !token_char(c))
        .filter(|run| run.chars().count() >= 8)
}

/// The first credential the raw text carries, named for the refusal message — or `None`.
pub(crate) fn find_secret(raw: &str) -> Option<&'static str> {
    for p in SECRET_PATTERNS {
        if (p.is_match)(raw) {
            return Some(p.name);
        }
    }
    if token_runs(raw).any(looks_random) {
        return Some("high-entropy value");
    }
    None
}

// =================================================================================================
// The gate
// =================================================================================================

/// The registry's name grammar, hand-checked: `^[a-zA-Z0-9.-]+/[a-zA-Z0-9._-]+$` — exactly one
/// slash, both halves non-empty.
pub(crate) fn is_registry_name(name: &str) -> bool {
    let mut parts = name.split('/');
    let (Some(ns), Some(server), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !ns.is_empty()
        && !server.is_empty()
        && ns
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        && server
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A non-empty object — the shape that makes `variables` a per-installation fill-in slot.
fn has_entries(v: Option<&Value>) -> bool {
    v.and_then(Value::as_object).is_some_and(|m| !m.is_empty())
}

/// Validate one server document. `raw` is the bytes as fetched or read from disk — the scan reads
/// THEM, not a re-serialization, so nothing a caller strips can hide a credential from it.
///
/// # Errors
/// One [`McpRefusal`] per the module docs; the order is the web tier's, exactly.
pub(crate) fn validate_server_json(raw: &[u8]) -> Result<McpSummary, McpRefusal> {
    let text = String::from_utf8_lossy(raw);
    if text.is_empty() {
        return refuse(McpRefusalCode::Invalid, "the document is empty");
    }
    let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
        return refuse(
            McpRefusalCode::Invalid,
            "that is not JSON — a server.json document is a JSON object",
        );
    };

    // FIRST, before anything is read out of the document: does it carry a credential?
    if let Some(kind) = find_secret(&text) {
        return refuse(
            McpRefusalCode::SecretRefused,
            format!(
                "the document carries what looks like a credential ({kind}) — a shared bundle \
                 never holds one"
            ),
        );
    }

    let Some(root) = parsed.as_object() else {
        return refuse(
            McpRefusalCode::Invalid,
            "a server.json document is a JSON object",
        );
    };

    let name = root.get("name").and_then(Value::as_str).unwrap_or_default();
    if name.chars().count() < NAME_MIN || name.chars().count() > NAME_MAX {
        return refuse(
            McpRefusalCode::Invalid,
            format!("name is required, {NAME_MIN}–{NAME_MAX} characters"),
        );
    }
    if !is_registry_name(name) {
        return refuse(
            McpRefusalCode::Invalid,
            "name must be a reverse-DNS namespace and a server name with exactly one slash \
             between them",
        );
    }
    let description = root
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if description.is_empty() || description.chars().count() > DESCRIPTION_MAX {
        return refuse(
            McpRefusalCode::Invalid,
            format!("description is required, 1–{DESCRIPTION_MAX} characters"),
        );
    }
    let version = root
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if version.is_empty() || version.chars().count() > VERSION_MAX {
        return refuse(McpRefusalCode::Invalid, "version is required");
    }

    // A non-empty packages[] is the registry's way of saying "install and run this locally". That
    // is a different kind of thing from a shared address, so it is refused rather than
    // half-supported.
    if root
        .get("packages")
        .and_then(Value::as_array)
        .is_some_and(|p| !p.is_empty())
    {
        return refuse(
            McpRefusalCode::LocalRefused,
            "this server installs and runs locally (packages[]) — Topos shares remote servers",
        );
    }

    // FIRST streamable-http wins: a document may offer several transports, and the ordering is the
    // publisher's own preference.
    let remote = root
        .get("remotes")
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter().find(|e| {
                e.get("type").and_then(Value::as_str) == Some(STREAMABLE_HTTP)
                    && e.get("url").and_then(Value::as_str).is_some()
            })
        });
    let Some(remote) = remote else {
        return refuse(
            McpRefusalCode::NoStreamableRemote,
            "no streamable-http remote — Topos places servers an agent reaches over that transport",
        );
    };
    let url = remote
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    // The template check comes before the URL parse: `https://{tenant}.example/mcp` PARSES, and
    // would otherwise pass as an https address whose host is a literal brace word.
    if url.contains('{') || url.contains('}') {
        return refuse(
            McpRefusalCode::UrlTemplate,
            "the endpoint carries a {placeholder} — it is a template, not an address every \
             machine can use",
        );
    }
    match url_scheme(&url) {
        None => return refuse(McpRefusalCode::Invalid, "the endpoint is not a URL"),
        Some("https") => {}
        Some(_) => return refuse(McpRefusalCode::InsecureUrl, "the endpoint must be https"),
    }
    // A remote-level `variables` block only exists to fill a template in. There is no template
    // left by now, so it is a fill-in slot with nothing to fill — and the thing it would fill is
    // exactly what this gate exists to keep out.
    if has_entries(remote.get("variables")) {
        return refuse(
            McpRefusalCode::SecretRefused,
            "the endpoint declares per-installation variables — a shared bundle carries the same \
             bytes everywhere",
        );
    }

    let mut headers = Vec::new();
    if let Some(list) = remote.get("headers").and_then(Value::as_array) {
        for entry in list {
            let hname = entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if hname.is_empty() {
                return refuse(McpRefusalCode::Invalid, "every header needs a name");
            }
            if entry.get("isSecret").and_then(Value::as_bool) == Some(true) {
                return refuse(
                    McpRefusalCode::SecretRefused,
                    format!(
                        "the header {hname} is declared secret — a shared bundle never carries a \
                         credential"
                    ),
                );
            }
            if has_entries(entry.get("variables")) {
                return refuse(
                    McpRefusalCode::SecretRefused,
                    format!("the header {hname} is assembled from per-installation variables"),
                );
            }
            // A header with no literal value is a slot somebody fills in on each machine — the
            // same thing `isSecret` names out loud, whether or not `isRequired` says so.
            let value = entry
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if value.is_empty() {
                return refuse(
                    McpRefusalCode::SecretRefused,
                    format!(
                        "the header {hname} has no value — it is a slot for a per-machine \
                         credential"
                    ),
                );
            }
            headers.push(McpHeader {
                name: hname.to_owned(),
                value: value.to_owned(),
            });
        }
    }

    let auth_hint = match root
        .get("_meta")
        .and_then(|m| m.get("sh.topos/auth"))
        .and_then(Value::as_str)
    {
        Some("oauth") => Some(McpAuthHint::Oauth),
        Some("none") => Some(McpAuthHint::None),
        _ => None,
    };

    Ok(McpSummary {
        name: name.to_owned(),
        description: description.to_owned(),
        version: version.to_owned(),
        url,
        transport: STREAMABLE_HTTP,
        headers,
        auth_hint,
    })
}

/// The URL's scheme, lowercased, when the string is shaped like one (`<scheme>://<rest>` with a
/// non-empty authority). `None` for anything else — the caller refuses it as "not a URL".
fn url_scheme(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() || rest.is_empty() {
        return None;
    }
    if !scheme
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
    {
        return None;
    }
    match scheme {
        "https" => Some("https"),
        "http" => Some("http"),
        _ => Some("other"),
    }
}

/// The bundle name a server document suggests: the tail segment of its registry name (the part
/// after the one slash), folded by the catalog's own birth-name rules — lowercase, every run of
/// non-alphanumerics collapsed to `-`, trimmed, capped at 64. The fallback is `"mcp-server"`,
/// which the name grammar can never produce an empty fold for in practice.
pub(crate) fn suggested_name_for(server_name: &str) -> String {
    let tail = server_name.rsplit('/').next().unwrap_or(server_name);
    let folded = fold_name(tail);
    if folded.is_empty() {
        "mcp-server".to_owned()
    } else {
        folded
    }
}

/// The catalog's birth-name fold, mirrored: lowercase → non-alphanumeric runs to `-` → trim `-` →
/// 64 chars → trim `-` again (a cut that lands mid-separator must not leave a trailing dash).
fn fold_name(input: &str) -> String {
    let lowered: String = input.to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut pending_dash = false;
    for c in lowered.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        } else {
            pending_dash = true;
        }
    }
    out.truncate(64);
    out.trim_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// The repo-root vectors both languages read.
    fn fixtures_root() -> PathBuf {
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/mcp"
        ))
        .canonicalize()
        .expect("the shared mcp fixtures live at the repo root")
    }

    fn patterns_json() -> serde_json::Value {
        let path = fixtures_root().join("secret-patterns.json");
        let text = std::fs::read_to_string(&path).expect("secret-patterns.json is readable");
        serde_json::from_str(&text).expect("secret-patterns.json is JSON")
    }

    /// THE DRIFT GATE, spelling half: the compiled list IS the JSON's list — same names, same
    /// regex sources, same order, same length — and the entropy block matches too. Editing the
    /// JSON without editing this module (or the reverse) fails here.
    #[test]
    fn compiled_pattern_list_matches_the_shared_json_exactly() {
        let json = patterns_json();
        let listed = json["patterns"].as_array().expect("patterns is an array");
        assert_eq!(
            listed.len(),
            SECRET_PATTERNS.len(),
            "the JSON lists {} shapes, this module compiles {}",
            listed.len(),
            SECRET_PATTERNS.len()
        );
        for (i, entry) in listed.iter().enumerate() {
            let compiled = &SECRET_PATTERNS[i];
            assert_eq!(
                entry["name"].as_str(),
                Some(compiled.name),
                "pattern {i}: name drifted"
            );
            assert_eq!(
                entry["regex"].as_str(),
                Some(compiled.regex),
                "pattern {i} ({}): regex source drifted",
                compiled.name
            );
        }
        assert_eq!(
            json["entropy"]["minLength"].as_u64(),
            Some(ENTROPY_MIN_LENGTH as u64),
            "the entropy minLength drifted"
        );
        assert!(
            (json["entropy"]["threshold"]
                .as_f64()
                .expect("threshold is a number")
                - ENTROPY_THRESHOLD)
                .abs()
                < f64::EPSILON,
            "the entropy threshold drifted"
        );
    }

    /// Every probe the equivalence test drives both engines over: hand-written boundary cases per
    /// shape (at the minimum, one below it, prefix-only, embedded in prose) plus every shared
    /// fixture document verbatim. Each probe is fed to EVERY pattern, so a matcher that fires on
    /// somebody else's shape is caught too.
    fn probe_corpus() -> Vec<String> {
        let mut probes: Vec<String> = vec![
            String::new(),
            "nothing to see here".to_owned(),
            // github-token: `gh[pousr]_` + 36
            format!("ghp_{}", "A1b2C3d4E5".repeat(4)), // 40 ≥ 36
            format!("ghp_{}", "a".repeat(36)),
            format!("ghp_{}", "a".repeat(35)),
            format!("ghq_{}", "a".repeat(40)), // not one of [pousr]
            "ghp_".to_owned(),
            format!("prefix ghs_{} suffix", "Z9".repeat(18)),
            format!("gho_{}", "a".repeat(300)), // past the {…,255} ceiling — still a match
            // github-fine-grained-token: 22 of [A-Za-z0-9_]
            format!("github_pat_{}", "a_1".repeat(8)),
            format!("github_pat_{}", "a".repeat(22)),
            format!("github_pat_{}", "a".repeat(21)),
            // openai-style-key: `sk-` + 20 of [A-Za-z0-9_-]
            format!("sk-{}", "a".repeat(20)),
            format!("sk-{}", "a".repeat(19)),
            "a task-management-and-scheduling-helper".to_owned(),
            // stripe
            format!("sk_live_{}", "a".repeat(16)),
            format!("sk_live_{}", "a".repeat(15)),
            format!("sk_test_{}", "0".repeat(16)),
            format!("sk_test_{}", "0".repeat(15)),
            // slack
            format!("xoxb-{}", "1".repeat(10)),
            format!("xoxb-{}", "1".repeat(9)),
            format!("xoxp-{}", "a-b".repeat(4)),
            format!("xoxa-{}", "1".repeat(20)),
            // aws
            format!("AKIA{}", "A1".repeat(8)),
            format!("AKIA{}", "A".repeat(15)),
            format!("AKIA{}", "a".repeat(20)), // lowercase is outside [0-9A-Z]
            // gitlab
            format!("glpat-{}", "a".repeat(16)),
            format!("glpat-{}", "a".repeat(15)),
            // google oauth
            format!("ya29.{}", "a".repeat(20)),
            format!("ya29.{}", "a".repeat(19)),
            "ya29Xaaaaaaaaaaaaaaaaaaaaaaa".to_owned(), // the dot is literal
            // google api key
            format!("AIza{}", "a".repeat(35)),
            format!("AIza{}", "a".repeat(34)),
            // bearer
            format!("Bearer {}", "a".repeat(20)),
            format!("Bearer {}", "a".repeat(19)),
            format!("Bearer\t\n  {}", "a.b~c+d/e-f".repeat(3)),
            format!("Bearer{}", "a".repeat(30)), // no whitespace at all
            format!("Bearer   {}==", "a".repeat(25)),
            "Bearer token".to_owned(),
            // multibyte before/after a hit — index arithmetic must stay on char boundaries
            format!("héllo ghp_{} wörld", "b".repeat(40)),
            format!("→→→ AKIA{}", "Z".repeat(16)),
        ];
        for dir in ["valid", "invalid"] {
            let root = fixtures_root().join(dir);
            let mut entries: Vec<PathBuf> = std::fs::read_dir(&root)
                .expect("the fixture dir is readable")
                .map(|e| e.expect("a dir entry").path())
                .collect();
            entries.sort();
            for path in entries {
                probes.push(std::fs::read_to_string(&path).expect("a readable fixture"));
            }
        }
        probes
    }

    /// THE DRIFT GATE, behaviour half: every compiled MATCHER answers exactly what its own JSON
    /// regex answers, over the whole probe corpus. This is what lets the shipped binary carry no
    /// regex engine — the engine is a dev-dependency, and it referees here.
    #[test]
    fn every_matcher_agrees_with_its_json_regex() {
        let json = patterns_json();
        let listed = json["patterns"].as_array().expect("patterns is an array");
        let probes = probe_corpus();
        for (i, compiled) in SECRET_PATTERNS.iter().enumerate() {
            let source = listed[i]["regex"].as_str().expect("a regex source");
            let re = regex::Regex::new(source)
                .unwrap_or_else(|e| panic!("{}: the JSON regex must compile ({e})", compiled.name));
            for probe in &probes {
                assert_eq!(
                    (compiled.is_match)(probe),
                    re.is_match(probe),
                    "{}: the matcher and `{source}` disagree on {:?}",
                    compiled.name,
                    &probe[..probe.len().min(120)]
                );
            }
        }
    }

    /// THE VECTORS: every shared refusal vector, driven through the real gate. A rule cannot
    /// change here without the repo-root vector changing too.
    #[test]
    fn every_shared_vector_gets_its_verdict() {
        let root = fixtures_root();
        let text = std::fs::read_to_string(root.join("vectors.json")).expect("vectors.json");
        let vectors: serde_json::Value = serde_json::from_str(&text).expect("vectors.json is JSON");
        let list = vectors.as_array().expect("vectors.json is an array");
        assert!(!list.is_empty(), "the vector list must not be empty");
        for v in list {
            let file = v["file"].as_str().expect("a file");
            let want = v["verdict"].as_str().expect("a verdict");
            let bytes = std::fs::read(root.join(file)).expect("a readable vector document");
            let got = validate_server_json(&bytes);
            match (want, &got) {
                ("ok", Ok(_)) => {}
                ("ok", Err(e)) => panic!(
                    "{file}: expected ok, got {} ({})",
                    e.code.as_str(),
                    e.message
                ),
                (code, Ok(s)) => panic!("{file}: expected {code}, got ok ({})", s.name),
                (code, Err(e)) => assert_eq!(
                    e.code.as_str(),
                    code,
                    "{file}: {} — {}",
                    v["note"].as_str().unwrap_or_default(),
                    e.message
                ),
            }
        }
    }

    /// The four VALID vectors are not just accepted — the summary they yield is the one the
    /// receipts print: the FIRST streamable-http remote, its literal headers, the auth hint only
    /// when the document actually declared one.
    #[test]
    fn valid_vectors_summarize_the_way_receipts_report_them() {
        let root = fixtures_root();
        let read = |name: &str| std::fs::read(root.join("valid").join(name)).expect("a vector");

        let plain = validate_server_json(&read("remote-no-auth.json")).expect("ok");
        assert_eq!(plain.name, "io.github.acme/weather");
        assert_eq!(plain.version, "1.4.0");
        assert_eq!(plain.url, "https://weather.acme.example/mcp");
        assert_eq!(plain.transport, STREAMABLE_HTTP);
        assert!(plain.headers.is_empty());
        // A document that declared nothing is NOT upgraded to "none".
        assert_eq!(plain.auth_hint, None);

        let literal = validate_server_json(&read("remote-literal-header.json")).expect("ok");
        assert_eq!(
            literal.headers,
            vec![
                McpHeader {
                    name: "X-Region".to_owned(),
                    value: "eu-west-1".to_owned()
                },
                McpHeader {
                    name: "X-Client".to_owned(),
                    value: "topos".to_owned()
                },
            ]
        );

        let oauth = validate_server_json(&read("remote-oauth-meta.json")).expect("ok");
        assert_eq!(oauth.auth_hint, Some(McpAuthHint::Oauth));

        // Two remotes offered: the first streamable-http one wins, the sse sibling is ignored.
        let both = validate_server_json(&read("remote-sse-and-streamable.json")).expect("ok");
        assert_eq!(both.url, "https://calendar.acme.example/mcp");
    }

    /// The scan reads the WHOLE raw text, so a credential anywhere in the document — not only in a
    /// header — refuses before a single field is read out of it.
    #[test]
    fn the_credential_scan_precedes_every_field_read() {
        // A document that is ALSO invalid in three other ways still answers SECRET_REFUSED: the
        // scan runs first, and a field-level message would echo part of the document back.
        let doc = format!(
            r#"{{"name":"bad","description":"","packages":[{{"x":1}}],"note":"ghp_{}"}}"#,
            "A1b2C3d4".repeat(5)
        );
        let e = validate_server_json(doc.as_bytes()).expect_err("refused");
        assert_eq!(e.code, McpRefusalCode::SecretRefused);
        assert!(e.message.contains("github-token"), "{}", e.message);
        // …and it names the SHAPE, never the value.
        assert!(!e.message.contains("ghp_"), "{}", e.message);
    }

    /// The entropy belt is the backstop under the named shapes; the narrow alphabet is what keeps
    /// ordinary URLs and prose out of it.
    #[test]
    fn the_entropy_belt_catches_random_tokens_and_spares_addresses() {
        assert_eq!(
            find_secret(
                r#"{"u":"https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json"}"#
            ),
            None,
            "a schema URL must not read as a secret"
        );
        assert_eq!(
            find_secret("Current conditions and forecasts for a named place."),
            None
        );
        // 32 lowercase hex — the entropy threshold would miss it; the hex rule does not.
        assert_eq!(
            find_secret("deadbeefcafef00d0123456789abcdef"),
            Some("high-entropy value")
        );
        // Mixed-class, 24+, high entropy.
        assert_eq!(
            find_secret("Xq7Tz2Rv9Kw4Nb8Hc3Ly6Md1Pf5"),
            Some("high-entropy value")
        );
        // 23 chars is one short of the floor.
        assert_eq!(find_secret("Xq7Tz2Rv9Kw4Nb8Hc3Ly6Md"), None);
    }

    /// The name grammar is exactly one slash between a reverse-DNS namespace and a server name.
    #[test]
    fn registry_name_shape_takes_exactly_one_slash() {
        assert!(is_registry_name("io.github.acme/weather"));
        assert!(is_registry_name("example-co/my_server.v2"));
        assert!(!is_registry_name("io.github.acme/team/server"));
        assert!(!is_registry_name("noslash"));
        assert!(!is_registry_name("/leading"));
        assert!(!is_registry_name("trailing/"));
        assert!(!is_registry_name("has space/server"));
        // The namespace half takes no underscore (the server half does).
        assert!(!is_registry_name("io_github/weather"));
    }

    /// The bundle folder a server name suggests — the catalog's fold, mirrored.
    #[test]
    fn suggested_name_folds_the_tail_segment() {
        assert_eq!(suggested_name_for("io.github.acme/weather"), "weather");
        assert_eq!(
            suggested_name_for("io.github.acme/My_Server.v2"),
            "my-server-v2"
        );
        assert_eq!(suggested_name_for("io.github.acme/---"), "mcp-server");
        assert_eq!(suggested_name_for("no-slash-at-all"), "no-slash-at-all");
        assert_eq!(
            suggested_name_for(&format!("ns/{}", "a".repeat(80))),
            "a".repeat(64)
        );
    }

    /// A URL the shape check cannot read is refused as INVALID, not silently treated as https.
    #[test]
    fn a_non_url_endpoint_refuses_invalid_and_a_non_https_one_refuses_insecure() {
        let doc = |url: &str| {
            format!(
                r#"{{"name":"io.github.a/b","description":"d","version":"1","remotes":[{{"type":"streamable-http","url":"{url}"}}]}}"#
            )
        };
        assert_eq!(
            validate_server_json(doc("not a url").as_bytes())
                .expect_err("refused")
                .code,
            McpRefusalCode::Invalid
        );
        assert_eq!(
            validate_server_json(doc("ftp://x.example/mcp").as_bytes())
                .expect_err("refused")
                .code,
            McpRefusalCode::InsecureUrl
        );
        assert!(validate_server_json(doc("https://x.example/mcp").as_bytes()).is_ok());
    }
}
