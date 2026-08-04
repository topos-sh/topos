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

/// The WHOLE file set an MCP candidate may carry: the document (required, at the root), an
/// optional README, and the reserved `topos-mcp.toml`. Anything else is refused by name — a
/// bundle whose behavior is one JSON document must not smuggle scripts or extra payloads beside
/// it. Mirrors the web tier's `MCP_ALLOWED_FILES`.
pub(crate) const MCP_ALLOWED_FILES: &[&str] = &["server.json", "README.md", "topos-mcp.toml"];

/// Header NAMES that carry a credential by definition — refused case-insensitively, independent
/// of `isSecret`, value shape, or entropy. A literal `Authorization: Basic …` is somebody's
/// credential whatever the flags say. Mirrors the web tier's list exactly.
const CREDENTIAL_HEADER_NAMES: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "x-auth-token",
    "x-access-token",
    "private-token",
];

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

/// The same scan over the PARSED document: every decoded string — keys and values, at any depth —
/// runs the pattern + entropy belt. This is what the raw pass cannot see: a token spelled in
/// `\uXXXX` escapes decodes to the credential the raw bytes never showed.
fn find_secret_deep(value: &Value) -> Option<&'static str> {
    match value {
        Value::String(s) => find_secret(s),
        Value::Array(list) => list.iter().find_map(find_secret_deep),
        Value::Object(map) => map
            .iter()
            .find_map(|(k, v)| find_secret(k).or_else(|| find_secret_deep(v))),
        _ => None,
    }
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
    // STRICT decode: an invalid byte refuses outright — a lossy replacement char could both hide
    // what the bytes spelled and let an unreadable document parse as if it were readable.
    let Ok(text) = std::str::from_utf8(raw) else {
        return refuse(McpRefusalCode::Invalid, "the document is not valid UTF-8");
    };
    if text.is_empty() {
        return refuse(McpRefusalCode::Invalid, "the document is empty");
    }
    let Ok(parsed) = serde_json::from_str::<Value>(text) else {
        return refuse(
            McpRefusalCode::Invalid,
            "that is not JSON — a server.json document is a JSON object",
        );
    };

    // FIRST, before anything is read out of the document: does it carry a credential? Two passes —
    // the raw text (nothing a caller strips can hide from it) and the DECODED strings (nothing a
    // publisher escapes can hide from that one).
    if let Some(kind) = find_secret(text).or_else(|| find_secret_deep(&parsed)) {
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
    match parse_endpoint_url(&url) {
        EndpointUrl::Invalid => {
            return refuse(McpRefusalCode::Invalid, "the endpoint is not a URL");
        }
        // userinfo in the address IS a credential — refused before the scheme is even judged, so
        // an http URL carrying one still names the real problem.
        EndpointUrl::Userinfo => {
            return refuse(
                McpRefusalCode::SecretRefused,
                "the endpoint URL carries credentials (user:password@) — a shared bundle never \
                 holds one",
            );
        }
        EndpointUrl::Scheme(scheme) if scheme == "https" => {}
        EndpointUrl::Scheme(_) => {
            return refuse(McpRefusalCode::InsecureUrl, "the endpoint must be https");
        }
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
            // A credential-bearing header NAME refuses whatever the value or the flags say —
            // before isSecret, before the value shape, independent of entropy.
            if CREDENTIAL_HEADER_NAMES.contains(&hname.to_ascii_lowercase().as_str()) {
                return refuse(
                    McpRefusalCode::SecretRefused,
                    format!(
                        "the header {hname} carries a credential by definition — a shared bundle \
                         never holds one"
                    ),
                );
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

/// What the endpoint-URL shape check answers. Mirrors the web gate's `new URL()` semantics on
/// the shared vectors: the scheme is case-insensitive; the host must be nonempty and free of
/// spaces / control characters (`https://not a url` and `https://?x` are NOT URLs); nonempty
/// userinfo is its own answer, checked only once the whole address parses (a malformed host
/// wins, exactly as the WHATWG parser throws before userinfo is ever reported).
enum EndpointUrl {
    /// Not a URL at all — refused as `MCP_INVALID`.
    Invalid,
    /// A parseable URL carrying a nonempty username or password — refused as a credential.
    Userinfo,
    /// A parseable, userinfo-free URL: its scheme, lowercased.
    Scheme(String),
}

fn parse_endpoint_url(url: &str) -> EndpointUrl {
    let Some((scheme, rest)) = url.split_once("://") else {
        return EndpointUrl::Invalid;
    };
    let mut scheme_chars = scheme.chars();
    let starts_alpha = scheme_chars.next().is_some_and(|c| c.is_ascii_alphabetic());
    if !starts_alpha
        || !scheme_chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
    {
        return EndpointUrl::Invalid;
    }
    // The authority runs to the first path/query/fragment delimiter (all ASCII, so every index
    // below stays on a char boundary).
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return EndpointUrl::Invalid;
    }
    // Userinfo splits at the LAST `@` (the WHATWG rule); the host is judged FIRST, so a broken
    // host answers Invalid even when userinfo rides along.
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
        None => (None, authority),
    };
    if hostport.starts_with('[') {
        // A bracketed v6 literal: nonempty hex/colon/dot inside, an optional numeric port after.
        let Some(close) = hostport.find(']') else {
            return EndpointUrl::Invalid;
        };
        let inside = &hostport[1..close];
        if inside.is_empty()
            || !inside
                .chars()
                .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.'))
        {
            return EndpointUrl::Invalid;
        }
        let after = &hostport[close + 1..];
        let port_ok = after.is_empty()
            || (after.starts_with(':') && after[1..].chars().all(|c| c.is_ascii_digit()));
        if !port_ok {
            return EndpointUrl::Invalid;
        }
    } else {
        let (host, port) = match hostport.rfind(':') {
            Some(i) => (&hostport[..i], Some(&hostport[i + 1..])),
            None => (hostport, None),
        };
        // An empty port (`https://host:/`) parses; a non-numeric one does not.
        if port.is_some_and(|p| !p.chars().all(|c| c.is_ascii_digit())) {
            return EndpointUrl::Invalid;
        }
        if host.is_empty()
            || host.contains(':')
            || host.chars().any(|c| {
                c.is_ascii_control()
                    || c.is_whitespace()
                    || matches!(
                        c,
                        '<' | '>' | '^' | '|' | '\\' | '"' | '[' | ']' | '@' | '/' | '?' | '#'
                    )
            })
        {
            return EndpointUrl::Invalid;
        }
    }
    if let Some(ui) = userinfo {
        let (user, password) = ui.split_once(':').unwrap_or((ui, ""));
        if !user.is_empty() || !password.is_empty() {
            return EndpointUrl::Userinfo;
        }
    }
    EndpointUrl::Scheme(scheme.to_ascii_lowercase())
}

/// Validate a WHOLE MCP candidate: the exact file set ([`MCP_ALLOWED_FILES`] — `server.json`
/// required, `README.md` and the reserved `topos-mcp.toml` optional), a credential scan over
/// EVERY allowed file's bytes (raw for the siblings; raw + decoded strings for the JSON
/// document, inside [`validate_server_json`]), and then the full document gate. The one gate the
/// publish preflight and the `add --mcp` local adopt answer to — the exact mirror of the web
/// tier's `validateCandidateFiles`.
///
/// # Errors
/// One [`McpRefusal`]; the check order is the web tier's, exactly.
pub(crate) fn validate_candidate_files(files: &[(&str, &[u8])]) -> Result<McpSummary, McpRefusal> {
    let Some((_, server)) = files.iter().find(|(path, _)| *path == "server.json") else {
        return refuse(
            McpRefusalCode::Invalid,
            "an MCP bundle carries server.json at its root — this candidate has none",
        );
    };
    for (path, _) in files {
        if !MCP_ALLOWED_FILES.contains(path) {
            return refuse(
                McpRefusalCode::Invalid,
                format!(
                    "an MCP bundle may hold only {} — {path} is not part of one",
                    MCP_ALLOWED_FILES.join(", ")
                ),
            );
        }
    }
    if server.len() > MAX_SERVER_JSON_BYTES {
        return refuse(
            McpRefusalCode::Invalid,
            "server.json is too large to be a server document",
        );
    }
    // Every sibling's bytes run the credential scan (the document runs its own, twice over,
    // inside the gate below).
    for (path, bytes) in files {
        if *path == "server.json" {
            continue;
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            return refuse(
                McpRefusalCode::Invalid,
                format!("{path} is not valid UTF-8"),
            );
        };
        if let Some(kind) = find_secret(text) {
            return refuse(
                McpRefusalCode::SecretRefused,
                format!(
                    "{path} carries what looks like a credential ({kind}) — a shared bundle \
                     never holds one"
                ),
            );
        }
    }
    validate_server_json(server)
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

    /// THE VECTORS: every shared refusal vector, driven through the real gate. Three entry
    /// shapes, all two-language: `file` (one document), `raw_base64` (bytes no text file can
    /// carry — the invalid-UTF-8 case), and `files` (a whole candidate, driven through
    /// [`validate_candidate_files`]). A rule cannot change here without the repo-root vector
    /// changing too.
    #[test]
    fn every_shared_vector_gets_its_verdict() {
        use base64::Engine as _;
        let root = fixtures_root();
        let text = std::fs::read_to_string(root.join("vectors.json")).expect("vectors.json");
        let vectors: serde_json::Value = serde_json::from_str(&text).expect("vectors.json is JSON");
        let list = vectors.as_array().expect("vectors.json is an array");
        assert!(!list.is_empty(), "the vector list must not be empty");
        for v in list {
            let want = v["verdict"].as_str().expect("a verdict");
            let (label, got) = if let Some(file) = v["file"].as_str() {
                let bytes = std::fs::read(root.join(file)).expect("a readable vector document");
                (file.to_owned(), validate_server_json(&bytes))
            } else if let Some(b64) = v["raw_base64"].as_str() {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .expect("raw_base64 decodes");
                ("raw_base64".to_owned(), validate_server_json(&bytes))
            } else if let Some(files) = v["files"].as_array() {
                let owned: Vec<(String, Vec<u8>)> = files
                    .iter()
                    .map(|f| {
                        let path = f["path"].as_str().expect("a candidate path").to_owned();
                        let bytes = match f["file"].as_str() {
                            Some(fixture) => {
                                std::fs::read(root.join(fixture)).expect("a readable fixture")
                            }
                            None => f["content"]
                                .as_str()
                                .expect("a candidate carries file or content")
                                .as_bytes()
                                .to_vec(),
                        };
                        (path, bytes)
                    })
                    .collect();
                let borrowed: Vec<(&str, &[u8])> = owned
                    .iter()
                    .map(|(p, b)| (p.as_str(), b.as_slice()))
                    .collect();
                (
                    format!("files[{}]", owned.len()),
                    validate_candidate_files(&borrowed),
                )
            } else {
                panic!("a vector entry needs file, raw_base64, or files");
            };
            match (want, &got) {
                ("ok", Ok(_)) => {}
                ("ok", Err(e)) => panic!(
                    "{label}: expected ok, got {} ({})",
                    e.code.as_str(),
                    e.message
                ),
                (code, Ok(s)) => panic!("{label}: expected {code}, got ok ({})", s.name),
                (code, Err(e)) => assert_eq!(
                    e.code.as_str(),
                    code,
                    "{label}: {} — {}",
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

    /// ITEM PAIR (deep nesting, the Rust half): a document nested past serde_json's default
    /// recursion limit is refused MCP_INVALID at the PARSE — no stack is consumed per level, so
    /// there is nothing to overflow. The boundary is pinned exactly (127 containers parse, the
    /// 128th refuses) because the web gate mirrors it with an explicit depth cap: the two gates
    /// must answer the same verdict for the same bytes.
    #[test]
    fn nesting_past_the_recursion_limit_is_mcp_invalid_at_the_parse() {
        let nested = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));

        // 127 levels PARSE — the refusal is the later shape check, proving the depth was fine.
        let e = validate_server_json(nested(127).as_bytes()).expect_err("an array is no document");
        assert_eq!(e.code, McpRefusalCode::Invalid);
        assert!(e.message.contains("JSON object"), "{}", e.message);

        // The 128th container is where serde stops — refused at the parse, typed, no panic.
        for n in [128usize, 200] {
            let e = validate_server_json(nested(n).as_bytes()).expect_err("too deep");
            assert_eq!(e.code, McpRefusalCode::Invalid, "depth {n}");
            assert!(e.message.contains("not JSON"), "depth {n}: {}", e.message);
        }
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

    /// The URL shape check mirrors the web gate's `new URL()` on every case the vectors pin: a
    /// spaced host and an empty host are NOT URLs, the scheme is case-insensitive, ports parse
    /// (empty included), and an unbracketed v6 host is refused like the WHATWG parser throws.
    #[test]
    fn url_validation_matches_the_web_gates_new_url_semantics() {
        let doc = |url: &str| {
            format!(
                r#"{{"name":"io.github.a/b","description":"d","version":"1","remotes":[{{"type":"streamable-http","url":"{url}"}}]}}"#
            )
        };
        let code = |url: &str| {
            validate_server_json(doc(url).as_bytes())
                .map(|_| ())
                .map_err(|e| e.code)
        };
        assert_eq!(code("https://not a url"), Err(McpRefusalCode::Invalid));
        assert_eq!(code("https://?x"), Err(McpRefusalCode::Invalid));
        assert_eq!(code("https://"), Err(McpRefusalCode::Invalid));
        assert_eq!(code("HTTPS://HOST/mcp"), Ok(()));
        assert_eq!(code("https://host:8080/mcp"), Ok(()));
        assert_eq!(code("https://host:/mcp"), Ok(()));
        assert_eq!(code("https://host:port/mcp"), Err(McpRefusalCode::Invalid));
        assert_eq!(code("https://::1/mcp"), Err(McpRefusalCode::Invalid));
        assert_eq!(code("https://[::1]/mcp"), Ok(()));
        // An EMPTY userinfo parses (the web gate passes `https://@host/`); a nonempty one is a
        // credential.
        assert_eq!(code("https://@host/mcp"), Ok(()));
        assert_eq!(
            code("https://alice:s3cret@host/mcp"),
            Err(McpRefusalCode::SecretRefused)
        );
        assert_eq!(
            code("https://:s3cret@host/mcp"),
            Err(McpRefusalCode::SecretRefused)
        );
    }

    /// ITEM PAIR (url-userinfo): a credential riding the address refuses as a secret — on the
    /// pre-fix gate this parsed as a plain https URL and PASSED.
    #[test]
    fn url_userinfo_refuses_as_a_secret() {
        let doc = r#"{"name":"io.github.a/b","description":"d","version":"1","remotes":[{"type":"streamable-http","url":"https://alice:s3cret@host/mcp"}]}"#;
        let e = validate_server_json(doc.as_bytes()).expect_err("refused");
        assert_eq!(e.code, McpRefusalCode::SecretRefused);
        assert!(e.message.contains("user:password@"), "{}", e.message);
    }

    /// ITEM PAIR (escaped credential): a token spelled entirely in \uXXXX escapes never shows in
    /// the raw text — the decoded-string walk catches it. The pre-fix gate (raw scan only)
    /// PASSED this document.
    #[test]
    fn escaped_credentials_are_caught_by_the_decoded_string_walk() {
        let token = format!(
            "ghp_{}",
            "A1b2C3d4E5".repeat(4).chars().take(36).collect::<String>()
        );
        let escaped: String = token
            .chars()
            .map(|c| format!("\\u{:04x}", c as u32))
            .collect();
        let doc = format!(
            r#"{{"name":"io.github.a/b","description":"d","version":"1","remotes":[{{"type":"streamable-http","url":"https://x.example/mcp","headers":[{{"name":"X-Extra","value":"{escaped}"}}]}}]}}"#
        );
        assert!(
            !doc.contains("ghp_"),
            "the raw text must not show the token"
        );
        let e = validate_server_json(doc.as_bytes()).expect_err("refused");
        assert_eq!(e.code, McpRefusalCode::SecretRefused);
        assert!(e.message.contains("github-token"), "{}", e.message);
        // …and the DECODED value never echoes back either.
        assert!(!e.message.contains("ghp_"), "{}", e.message);
    }

    /// ITEM PAIR (header names): a credential-bearing header NAME refuses whatever the value
    /// looks like — flags and entropy aside, case-insensitively. The pre-fix gate accepted a
    /// literal `Authorization: Basic …`.
    #[test]
    fn credential_bearing_header_names_refuse_independent_of_value() {
        let doc = |name: &str, value: &str| {
            format!(
                r#"{{"name":"io.github.a/b","description":"d","version":"1","remotes":[{{"type":"streamable-http","url":"https://x.example/mcp","headers":[{{"name":"{name}","value":"{value}"}}]}}]}}"#
            )
        };
        for name in [
            "Authorization",
            "proxy-authorization",
            "COOKIE",
            "Set-Cookie",
            "X-Api-Key",
            "api-key",
            "X-Auth-Token",
            "x-access-token",
            "Private-Token",
        ] {
            let e = validate_server_json(doc(name, "plain").as_bytes()).expect_err(name);
            assert_eq!(e.code, McpRefusalCode::SecretRefused, "{name}");
        }
        // The Basic shape neither pattern nor entropy would catch — the NAME is the evidence.
        let e = validate_server_json(doc("Authorization", "Basic Zm9vOmJhcg==").as_bytes())
            .expect_err("refused");
        assert_eq!(e.code, McpRefusalCode::SecretRefused);
        // An unlisted literal header still passes.
        assert!(validate_server_json(doc("X-Region", "eu-west-1").as_bytes()).is_ok());
    }

    /// ITEM PAIR (strict UTF-8): one invalid byte inside a string refuses INVALID. The pre-fix
    /// gate decoded lossily, so the replacement char parsed as JSON and the document PASSED.
    #[test]
    fn invalid_utf8_refuses_invalid() {
        let mut bytes = br#"{"name":"io.github.a/b","description":"caf"#.to_vec();
        bytes.push(0xE9);
        bytes.extend_from_slice(
            br#"","version":"1","remotes":[{"type":"streamable-http","url":"https://x.example/mcp"}]}"#,
        );
        let e = validate_server_json(&bytes).expect_err("refused");
        assert_eq!(e.code, McpRefusalCode::Invalid);
        assert!(e.message.contains("UTF-8"), "{}", e.message);
    }

    /// ITEM PAIR (sibling files): the candidate gate refuses any file outside the allowed trio
    /// (naming the set) and runs the credential scan over every allowed sibling's bytes. The
    /// pre-fix gates read server.json alone and let both candidates through.
    #[test]
    fn the_candidate_allowlist_refuses_strays_and_scans_sibling_bytes() {
        let server =
            std::fs::read(fixtures_root().join("valid/remote-no-auth.json")).expect("fixture");
        // The exact allowed trio passes.
        let readme = b"How to use this server.\n".to_vec();
        let toml = b"# reserved\n".to_vec();
        assert!(
            validate_candidate_files(&[
                ("server.json", server.as_slice()),
                ("README.md", readme.as_slice()),
                ("topos-mcp.toml", toml.as_slice()),
            ])
            .is_ok()
        );
        // A stray file refuses, naming the allowed set.
        let e = validate_candidate_files(&[
            ("server.json", server.as_slice()),
            ("evil.sh", b"#!/bin/sh\necho pwned\n".as_slice()),
        ])
        .expect_err("refused");
        assert_eq!(e.code, McpRefusalCode::Invalid);
        assert!(
            e.message.contains("server.json, README.md, topos-mcp.toml"),
            "{}",
            e.message
        );
        assert!(e.message.contains("evil.sh"), "{}", e.message);
        // A README carrying a token refuses exactly like the document would.
        let hot = format!("Set GITHUB_TOKEN to ghp_{}.\n", "A1b2C3d4E5".repeat(4));
        let e = validate_candidate_files(&[
            ("server.json", server.as_slice()),
            ("README.md", hot.as_bytes()),
        ])
        .expect_err("refused");
        assert_eq!(e.code, McpRefusalCode::SecretRefused);
        assert!(e.message.starts_with("README.md"), "{}", e.message);
        // No server.json at all is its own message.
        let e = validate_candidate_files(&[("README.md", readme.as_slice())]).expect_err("refused");
        assert_eq!(e.code, McpRefusalCode::Invalid);
        assert!(e.message.contains("has none"), "{}", e.message);
    }
}
