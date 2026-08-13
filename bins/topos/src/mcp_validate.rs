//! THE MCP SERVER-DOCUMENT GATE, client side — the exact mirror of the web tier's gate.
//!
//! What a shared MCP bundle IS, here: a server an agent can run from the exact bytes the workspace
//! holds — either a REMOTE address every machine reaches over `streamable-http`, or a PACKAGE every
//! machine installs at one pinned version — with no per-machine fill-in of the address itself and no
//! credential anywhere in the document. Everything this refuses refuses for one of those reasons,
//! and every refusal is TYPED ([`McpRefusalCode`]) so the caller says plainly what is wrong instead
//! of "invalid".
//!
//! **What a package may be is the FORMAT's question, not this build's.** `registryType` is
//! open-world — npm, pypi, oci, nuget, mcpb and whatever the registry adds next all publish — and a
//! package transport may be stdio, streamable-http or sse. Whether THIS machine can set a given
//! package up is answered where the entry is rendered, in plain words, not by refusing the document
//! for everyone. The one rule this product adds on top of the format: a package names ONE immutable
//! thing (an exact version, a digest, a hash), because "the same bytes everywhere" is the promise a
//! version range breaks.
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
//! Every shape in the list is `<literal prefix><character class>{min,…}`, optionally behind a
//! `(^|[^A-Za-z0-9])` left boundary. Existence of a match is therefore decidable by "find the
//! prefix, look at what precedes it, count the run" — no backtracking, no engine, and no
//! `regex` crate in the shipped binary (which would add four crates to a client whose whole point
//! is being a small static download). The engine is a DEV-dependency instead, used by the
//! equivalence test below: it compiles the JSON's own regex sources and asserts, over a cross
//! product of probes, that every matcher answers exactly what its regex answers.

use serde_json::Value;

/// A hard ceiling on the document itself — a server.json is a page of text, never a payload.
pub(crate) const MAX_SERVER_JSON_BYTES: usize = 256 * 1024;

/// The one transport a shared bundle can promise: the same URL works from every machine.
pub(crate) const STREAMABLE_HTTP: &str = "streamable-http";

/// The WHOLE file set an MCP candidate may carry: the document (required, at the root) and an
/// optional README. Anything else is refused by name — a bundle whose behavior is one JSON
/// document must not smuggle scripts or extra payloads beside it. Mirrors the web tier's
/// `MCP_ALLOWED_FILES`.
pub(crate) const MCP_ALLOWED_FILES: &[&str] = &["server.json", "README.md"];

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

/// The words that make a NAME say "a credential goes here" — read after folding the name to letters
/// and digits, so `GITHUB_TOKEN`, `github-token` and `githubToken` are one word to this.
///
/// A package's environment variables and command-line flags are SLOTS: the machine that runs the
/// server fills them from its own keychain, which is the arrangement this product wants. So a slot
/// named for a credential passes — with `isSecret` on it or not. What does not pass is such a slot
/// arriving with the VALUE already in it: those bytes travel to every machine in the workspace.
/// (Remote headers answer to their own, stricter rules: topos writes them into every agent's config
/// verbatim, so a slot there is unplaceable rather than merely unwise.) Mirrors the web tier exactly.
const CREDENTIAL_NAME_WORDS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "apikey",
    "credential",
    "privatekey",
    "accesskey",
];

/// Does this environment-variable / flag / package-header name say it holds a credential?
fn looks_like_credential_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if CREDENTIAL_HEADER_NAMES.contains(&lower.as_str()) {
        return true;
    }
    let folded: String = lower.chars().filter(char::is_ascii_alphanumeric).collect();
    CREDENTIAL_NAME_WORDS
        .iter()
        .any(|word| folded.contains(word))
}

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
    /// A package names no single immutable thing — a version range, `latest`, an OCI reference
    /// with no digest, an MCPB download with no hash.
    PackageUnpinned,
    /// Nothing to deliver: no `streamable-http` remote AND no packages.
    NoStreamableRemote,
    /// The endpoint is plain http.
    InsecureUrl,
    /// The endpoint — or a header value — carries a `{placeholder}`, so it is not a literal every
    /// machine can use.
    UrlTemplate,
    /// The document carries (or reserves a slot for) a credential.
    SecretRefused,
}

impl McpRefusalCode {
    /// The wire spelling — the same string the web tier and the vectors use.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            McpRefusalCode::Invalid => "MCP_INVALID",
            McpRefusalCode::PackageUnpinned => "MCP_PACKAGE_UNPINNED",
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
///
/// `url` and `transport` are EMPTY when the document offers only packages: a bundle is allowed to
/// be a thing every machine installs rather than a thing every machine dials, so a caller that
/// prints an address asks which it is holding first. (Empty rather than an `Option` deliberately —
/// the receipt types this feeds spell both fields as plain strings, and a summary that changed
/// shape would be a wire change dressed up as a validator change.)
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

/// [`literal_run`] with a LEFT BOUNDARY: the literal must start the text or follow a character
/// outside `[A-Za-z0-9]` — the `(^|[^A-Za-z0-9])` prefix a shape wears when its literal is short
/// enough to fall inside an ordinary word. Without it, `sk-` matches in the middle of
/// `task-management-and-scheduling` and a server named for what it does reads as a credential.
fn boundary_literal_run(text: &str, lit: &str, class: fn(char) -> bool, min: usize) -> bool {
    debug_assert!(lit.is_ascii(), "literal prefixes are ASCII by construction");
    let mut from = 0usize;
    while let Some(off) = text[from..].find(lit) {
        let hit = from + off;
        let boundary = text[..hit].chars().next_back().is_none_or(|c| !alnum(c));
        if boundary && run_len(&text[hit + lit.len()..], class) >= min {
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
        regex: r"(^|[^A-Za-z0-9])sk-[A-Za-z0-9_-]{20,}",
        is_match: |t| boundary_literal_run(t, "sk-", alnum_us_dash, 20),
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

/// The maximal runs of [`token_char`] at least 8 long, each with the byte offset it starts at —
/// what the belt inspects. (The web tier's `[A-Za-z0-9_+=-]{8,}` global match yields exactly these,
/// and its `match.index` is this offset.)
fn token_runs(raw: &str) -> Vec<(usize, &str)> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in raw.char_indices() {
        if token_char(c) {
            start.get_or_insert(i);
        } else if let Some(from) = start.take() {
            let run = &raw[from..i];
            if run.chars().count() >= 8 {
                runs.push((from, run));
            }
        }
    }
    if let Some(from) = start {
        let run = &raw[from..];
        if run.chars().count() >= 8 {
            runs.push((from, run));
        }
    }
    runs
}

/// Is the run starting at `at` an INTEGRITY DIGEST rather than a credential?
///
/// A sha-256 is 64 hex characters, which is precisely the LONG HEX shape the entropy belt refuses —
/// and the registry format puts sha-256 digests in two ordinary, published places: an OCI reference
/// (`ghcr.io/acme/mcp@sha256:<hex>`) and an MCPB download's `fileSha256`. Both are the OPPOSITE of a
/// secret: they are how a reader proves the bytes it fetched are the bytes the publisher named.
/// Without this carve-out no pinned container image and no MCPB package could ever be published,
/// which would make the pinning rule below unsatisfiable.
///
/// THE RUN ITSELF MUST BE A WHOLE DIGEST — 64 lowercase hex characters, nothing more and nothing
/// less. Without that the carve-out is a prefix anyone can type: `sha256:` in front of a generated
/// key exempted the key. A digest is a fixed shape, so demanding the shape costs a publisher
/// nothing and closes the door.
///
/// Then the CONTEXT, read from the characters immediately BEFORE the run — two spellings and only
/// these two, so nothing about the run itself relaxes:
///
///  · `sha256:` directly in front of it (the OCI/registry digest spelling, wherever it appears);
///  · a JSON key ENDING in `sha256`, then `": "`, then the run (`"fileSha256": "<hex>"`).
fn is_integrity_digest(raw: &str, at: usize, run: &str) -> bool {
    if !is_file_sha256(run) {
        return false;
    }
    let before = &raw[..at];
    if before.len() >= 7 && before[before.len() - 7..].eq_ignore_ascii_case("sha256:") {
        return true;
    }
    // `… "fileSha256"  :  " ` — the quote that opens the value, the colon, the quoted key.
    let Some(rest) = before.strip_suffix('"') else {
        return false;
    };
    let Some(rest) = rest.trim_end().strip_suffix(':') else {
        return false;
    };
    let Some(rest) = rest.trim_end().strip_suffix('"') else {
        return false;
    };
    rest.len() >= 6 && rest[rest.len() - 6..].eq_ignore_ascii_case("sha256")
}

/// The first credential the raw text carries, named for the refusal message — or `None`.
pub(crate) fn find_secret(raw: &str) -> Option<&'static str> {
    for p in SECRET_PATTERNS {
        if (p.is_match)(raw) {
            return Some(p.name);
        }
    }
    if token_runs(raw)
        .into_iter()
        .any(|(at, run)| looks_random(run) && !is_integrity_digest(raw, at, run))
    {
        return Some("high-entropy value");
    }
    None
}

/// A JSON key whose VALUE is an integrity digest by name — the `fileSha256` family — holding a
/// value that IS one. Both halves, because the key alone is a name a publisher chooses: a slot
/// called `fileSha256` carrying anything but 64 lowercase hex is not a digest, and skipping it
/// would let the name exempt whatever was put in it.
fn is_digest_under_key(key: &str, value: &str) -> bool {
    key.len() >= 6 && key[key.len() - 6..].eq_ignore_ascii_case("sha256") && is_file_sha256(value)
}

/// The same scan over the PARSED document: every decoded string — keys and values, at any depth —
/// runs the pattern + entropy belt. This is what the raw pass cannot see: a token spelled in
/// `\uXXXX` escapes decodes to the credential the raw bytes never showed.
///
/// One question cannot be answered from a string alone: a 64-hex value under `fileSha256` is an
/// integrity digest, and the raw pass reads that from the bytes in front of it where this pass has
/// only the key. So a digest-keyed value that IS a whole digest is skipped here — key AND value,
/// exactly as the web tier skips it; the key alone is a name a publisher chooses.
fn find_secret_deep(value: &Value) -> Option<&'static str> {
    match value {
        Value::String(s) => find_secret(s),
        Value::Array(list) => list.iter().find_map(find_secret_deep),
        Value::Object(map) => map.iter().find_map(|(k, v)| {
            find_secret(k).or_else(|| {
                if v.as_str().is_some_and(|s| is_digest_under_key(k, s)) {
                    None
                } else {
                    find_secret_deep(v)
                }
            })
        }),
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

/// A `variables` block DECLARED at all — any non-null value, an empty `{}` included. A header
/// wearing one is a per-machine fill-in whether or not the slots are listed, and the placement
/// engine refuses exactly this shape when it renders a config entry: the gate refuses it here so
/// a bundle can never be publishable and permanently unplaceable. (The remote-level rule above
/// stays the narrower `has_entries` — nothing downstream re-checks it.)
fn declares_variables(v: Option<&Value>) -> bool {
    v.is_some_and(|v| !v.is_null())
}

/// The one sentence both tiers say when an endpoint is spelled in a way they would read
/// differently — same words, same code, either side of the wire.
const AMBIGUOUS_ENDPOINT_MESSAGE: &str = concat!(
    "its endpoint is not a plain address — a shared endpoint spells scheme://host, ",
    "with no backslash, tab or line break anywhere in it"
);

/// Is this endpoint spelled so that EVERY reader gets the same address out of it?
///
/// The web tier hands its URLs to a WHATWG parser and this tier hand-parses them, and the two
/// disagree on three spellings — each one a repair the WHATWG parser performs silently:
///
///  · **no `://`** — `https:example.com/mcp` gains its slashes from the special-scheme rule and
///    parses as `https://example.com/mcp`; the hand-parse sees no authority at all.
///  · **a backslash** — it ENDS the authority (`https://h\@x.example/mcp` is host `h`, no
///    userinfo); the hand-parse splits at the last `@` and reads `h\` as a user name, which its
///    own userinfo rule then refuses as a credential.
///  · **a tab, CR or LF** — deleted from the input BEFORE parsing, so `https://x<TAB>.example/mcp`
///    is the host `x.example`; the hand-parse refuses whitespace in a host.
///
/// None of the three is an address anyone means to write, and each one lands a different verdict
/// per language, so both tiers refuse the SHAPE up front rather than each parsing its own reading.
/// Mirrors the web tier's `endpointIsUnambiguous` exactly.
fn is_unambiguous_endpoint(url: &str) -> bool {
    url.contains("://") && !url.contains(['\\', '\t', '\r', '\n'])
}

/// The hygiene and credential rules ONE `remotes[]` entry answers to, whichever entry it is:
/// the address is a plain https URL with no template, no userinfo and a host that parses; the
/// entry reserves no per-installation fill-in; and every header carries a literal, credential-free
/// value. Returns that entry's headers, so the placed remote's survive into the summary.
///
/// # Errors
/// One [`McpRefusal`]; the check order is the web tier's, exactly.
fn check_remote(remote: &Value) -> Result<Vec<McpHeader>, McpRefusal> {
    // An entry with no string `url` names no address — there is nothing to judge but its headers.
    if let Some(url) = remote.get("url").and_then(Value::as_str) {
        // The template check comes before the URL parse: `https://{tenant}.example/mcp` PARSES,
        // and would otherwise pass as an https address whose host is a literal brace word.
        if url.contains('{') || url.contains('}') {
            return refuse(
                McpRefusalCode::UrlTemplate,
                "its endpoint carries a {placeholder} — that is a template, not an address every \
                 machine can use",
            );
        }
        // BEFORE the parse, and BEFORE the userinfo rule: the spellings the two tiers would read
        // DIFFERENTLY. Answering the same code on both is what this ordering buys — a document
        // refused here can never be published on one tier and then refuse forever on the other.
        if !is_unambiguous_endpoint(url) {
            return refuse(McpRefusalCode::Invalid, AMBIGUOUS_ENDPOINT_MESSAGE);
        }
        match parse_endpoint_url(url) {
            EndpointUrl::Invalid => {
                return refuse(McpRefusalCode::Invalid, "its endpoint is not a URL");
            }
            // userinfo in the address IS a credential — refused before the scheme is even judged,
            // so an http URL carrying one still names the real problem.
            EndpointUrl::Userinfo => {
                return refuse(
                    McpRefusalCode::SecretRefused,
                    "its endpoint carries credentials (user:password@) — a shared bundle never \
                     holds one",
                );
            }
            EndpointUrl::Scheme(scheme) if scheme == "https" => {}
            EndpointUrl::Scheme(_) => {
                return refuse(McpRefusalCode::InsecureUrl, "its endpoint must be https");
            }
        }
    }
    // A remote-level `variables` block only exists to fill a template in. There is no template
    // left by now, so it is a fill-in slot with nothing to fill — and the thing it would fill is
    // exactly what this gate exists to keep out.
    if has_entries(remote.get("variables")) {
        return refuse(
            McpRefusalCode::SecretRefused,
            "its endpoint declares per-installation variables — a shared bundle is the same on \
             every machine",
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
                return refuse(McpRefusalCode::Invalid, "one of its headers has no name");
            }
            // A credential-bearing header NAME refuses whatever the value or the flags say —
            // before isSecret, before the value shape, independent of entropy.
            if CREDENTIAL_HEADER_NAMES.contains(&hname.to_ascii_lowercase().as_str()) {
                return refuse(
                    McpRefusalCode::SecretRefused,
                    format!(
                        "its {hname} header carries a credential by definition — a shared bundle \
                         never holds one"
                    ),
                );
            }
            if entry.get("isSecret").and_then(Value::as_bool) == Some(true) {
                return refuse(
                    McpRefusalCode::SecretRefused,
                    format!(
                        "its {hname} header is declared secret — a shared bundle never carries a \
                         credential"
                    ),
                );
            }
            if declares_variables(entry.get("variables")) {
                return refuse(
                    McpRefusalCode::SecretRefused,
                    format!("its {hname} header is assembled from per-installation variables"),
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
                        "its {hname} header has no value — that is a slot for a per-machine \
                         credential"
                    ),
                );
            }
            // A brace pair in the value is the header's own template: something fills it in per
            // machine, which is the one thing a shared bundle cannot carry. The placement engine
            // refuses this shape too — the gate refuses it first, so the refusal reaches the
            // author instead of every member's sweep.
            if value.contains('{') && value.contains('}') {
                return refuse(
                    McpRefusalCode::UrlTemplate,
                    format!(
                        "its {hname} header carries a {{placeholder}} — that is a template, not \
                         a value every machine can send"
                    ),
                );
            }
            headers.push(McpHeader {
                name: hname.to_owned(),
                value: value.to_owned(),
            });
        }
    }
    Ok(headers)
}

// =================================================================================================
// Packages: one immutable thing per entry
// =================================================================================================

/// The moving pointer the registry reserves — never a version anyone can pin to.
const RESERVED_VERSION: &str = "latest";

/// The transports a PACKAGE may declare (the schema's `LocalTransport`).
const PACKAGE_TRANSPORTS: &[&str] = &["stdio", STREAMABLE_HTTP, "sse"];

/// A dotted version-ish token: `1`, `1.2`, `1.2.3`, `v1.2`, `1.x`, `1.*` — with an optional
/// `-prerelease` tail. The three range shapes below are built out of it.
fn is_version_atom(token: &str, allow_wildcards: bool) -> bool {
    let token = token.strip_prefix('v').unwrap_or(token);
    let (core, pre) = match token.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (token, None),
    };
    if let Some(pre) = pre
        && (pre.is_empty()
            || !pre
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')))
    {
        return false;
    }
    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return false;
    }
    parts.iter().all(|part| {
        !part.is_empty()
            && (part.chars().all(|c| c.is_ascii_digit())
                || (allow_wildcards && matches!(*part, "x" | "X" | "*")))
    })
}

/// Does this version string spell a RANGE rather than a version? Reimplemented from the official
/// registry's own rules (modelcontextprotocol/registry — read, not vendored), because a range is a
/// promise this product cannot keep: "the same bytes everywhere" ends the moment a machine resolves
/// `^1.2.3` on the day it happens to install.
///
///  · comparator ranges — `^1.2.3` · `~1.2` · `>=1.0.0` · `<2` · `=1.0.0`
///  · hyphen ranges — `1.2.3 - 1.4.0`
///  · OR ranges — `1.2 || 1.3`
///  · wildcards in a dotted version — `1.x` · `1.2.*` · `1.X`
fn looks_like_version_range(version: &str) -> bool {
    let trimmed = version.trim();
    if trimmed.is_empty() {
        return false;
    }
    for op in ["^", "~", ">=", "<=", ">", "<", "="] {
        if let Some(rest) = trimmed.strip_prefix(op)
            && is_version_atom(rest.trim_start(), false)
        {
            return true;
        }
    }
    if let Some((left, right)) = trimmed.split_once(" - ")
        && is_version_atom(left.trim(), false)
        && is_version_atom(right.trim(), false)
    {
        return true;
    }
    if trimmed.contains("||")
        && trimmed
            .split("||")
            .all(|part| is_version_atom(part.trim(), false))
    {
        return true;
    }
    // A wildcard inside a dotted version implies range-like intent; a bare `x` does not (it is
    // not a dotted version at all).
    trimmed.contains('.')
        && is_version_atom(trimmed, true)
        && (trimmed.contains('x') || trimmed.contains('X') || trimmed.contains('*'))
}

// ── One exact version, per registry ──────────────────────────────────────────────────────────
//
// ONE EXACT VERSION, read POSITIVELY — the grammar the registry itself publishes under, not a list
// of the range spellings anyone thought of.
//
// A blacklist of ranges is the wrong shape for this rule and was: `*`, `x` and a dist-tag like
// `next` are none of the listed range spellings, so they passed as "exact" — and npm resolves each
// of them to whatever is current on the day a given machine installs, which is precisely the thing
// "the same bytes everywhere" forbids. For the two registries this build knows the version grammar
// of, the version must therefore MATCH that grammar and nothing else is accepted. Every other
// `registryType` stays open-world and keeps the range belt above — this build cannot claim to know
// a grammar it has never read.
//
// Hand-parsed rather than pattern-matched, character for character, because no regex engine ships
// in this binary and the two tiers have to answer identically over the shared vectors.

/// A semver numeric identifier: digits, and no leading zero past a bare `0`.
fn is_numeric_identifier(part: &str) -> bool {
    !part.is_empty()
        && part.bytes().all(|b| b.is_ascii_digit())
        && (part == "0" || !part.starts_with('0'))
}

/// A dot-separated run of `[0-9A-Za-z-]` identifiers, each non-empty. A pre-release identifier that
/// is all digits is a NUMBER, and semver forbids leading zeros on one (`numeric_rule`); build
/// metadata has no such rule — `+build.007` is legal.
fn is_dot_separated_identifiers(text: &str, numeric_rule: bool) -> bool {
    !text.is_empty()
        && text.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && (!numeric_rule
                    || !part.bytes().all(|b| b.is_ascii_digit())
                    || is_numeric_identifier(part))
        })
}

/// `X.Y.Z`, optionally `-prerelease` and `+build` — strict semver, exactly what npm publishes.
fn is_exact_semver(version: &str) -> bool {
    let (before_build, build) = match version.split_once('+') {
        Some((head, tail)) => (head, Some(tail)),
        None => (version, None),
    };
    if let Some(build) = build
        && !is_dot_separated_identifiers(build, false)
    {
        return false;
    }
    let (core, pre) = match before_build.split_once('-') {
        Some((head, tail)) => (head, Some(tail)),
        None => (before_build, None),
    };
    if let Some(pre) = pre
        && !is_dot_separated_identifiers(pre, true)
    {
        return false;
    }
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3 && parts.iter().all(|part| is_numeric_identifier(part))
}

/// How many digits `text` carries from `at` (ASCII, so every index stays on a char boundary).
fn digit_run(text: &str, at: usize) -> usize {
    text.as_bytes()[at.min(text.len())..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count()
}

/// The byte at `at`, or `None` past the end.
fn byte_at(text: &str, at: usize) -> Option<u8> {
    text.as_bytes().get(at).copied()
}

/// PEP 440's separator between a release and the segment that follows it.
fn is_pep440_separator(byte: Option<u8>) -> bool {
    matches!(byte, Some(b'-' | b'_' | b'.'))
}

/// One optional `<sep?><label><sep?><digits?>` segment; answers the position after it, or `at`.
fn read_labelled(text: &str, at: usize, labels: &[&str]) -> usize {
    let mut p = at;
    if is_pep440_separator(byte_at(text, p)) {
        p += 1;
    }
    for label in labels {
        if !text[p.min(text.len())..].starts_with(label) {
            continue;
        }
        let mut q = p + label.len();
        if is_pep440_separator(byte_at(text, q)) {
            q += 1;
        }
        return q + digit_run(text, q);
    }
    at
}

/// A PEP 440 public version, and only one: `[N!]N(.N)*[{a|b|rc}N][.postN][.devN][+local]`. No
/// comparison operator, no `*`, no dist-tag — those are the spellings that name a SET.
fn is_exact_pep440(version: &str) -> bool {
    let text = version.to_ascii_lowercase();
    let text = text.as_str();
    let mut pos = 0usize;
    if byte_at(text, pos) == Some(b'v') {
        pos += 1;
    }
    // An epoch, when one is spelled: digits then `!`.
    let epoch = digit_run(text, pos);
    if epoch > 0 && byte_at(text, pos + epoch) == Some(b'!') {
        pos += epoch + 1;
    }
    // The release segment: at least one number, then any run of `.N`.
    let first = digit_run(text, pos);
    if first == 0 {
        return false;
    }
    pos += first;
    while byte_at(text, pos) == Some(b'.') {
        let next = digit_run(text, pos + 1);
        if next == 0 {
            break;
        }
        pos += 1 + next;
    }
    pos = read_labelled(
        text,
        pos,
        &["preview", "pre", "alpha", "beta", "rc", "a", "b", "c"],
    );
    // The implicit post-release spelling (`1.0-1`) comes before the labelled one.
    let implicit_post = if byte_at(text, pos) == Some(b'-') {
        digit_run(text, pos + 1)
    } else {
        0
    };
    pos = if implicit_post > 0 {
        pos + 1 + implicit_post
    } else {
        read_labelled(text, pos, &["post", "rev", "r"])
    };
    pos = read_labelled(text, pos, &["dev"]);
    if byte_at(text, pos) == Some(b'+') {
        let local = &text[pos + 1..];
        return !local.is_empty()
            && local.split(['-', '_', '.']).all(|segment| {
                !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_alphanumeric())
            });
    }
    pos == text.len()
}

/// Whether this registry's version grammar is one this build reads — and how.
fn exact_version_of(registry_type: &str) -> Option<fn(&str) -> bool> {
    match registry_type {
        "npm" => Some(is_exact_semver),
        "pypi" => Some(is_exact_pep440),
        _ => None,
    }
}

/// 64 lowercase hex — the schema's own `fileSha256` shape.
fn is_file_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f'))
}

/// Does this OCI reference name ONE image? The version lives inside the identifier for this type
/// (the registry's own rule: an OCI package carries no `version` field), so the answer is a
/// `@sha256:` DIGEST and nothing else.
///
/// A TAG IS NOT A PIN. `ghcr.io/acme/mcp:1.0.0` reads like a version and is a movable label: the
/// publisher can point it at other bytes tomorrow, and two machines installing this bundle a week
/// apart would then run different code. `latest` is only the most obvious spelling of that; the
/// problem is the tag, not the word. A digest IS the bytes, which is the same promise a version
/// hash makes everywhere else in this product.
fn oci_reference_is_pinned(identifier: &str) -> bool {
    identifier
        .rsplit_once("@sha256:")
        .is_some_and(|(_, digest)| is_file_sha256(digest))
}

/// One `KeyValueInput` / `Argument` that arrives with its VALUE filled in — the shape the
/// credential rule judges. A slot with no value is a slot, which is fine.
fn literal_value_of(entry: &Value) -> Option<&str> {
    entry
        .get("value")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
}

/// The named-slot rule, one sentence for environment variables, flags and package headers alike.
///
/// A slot is judged by what the DOCUMENT says about it and by what its NAME says, in that order.
/// `isSecret: true` is the publisher stating outright that the slot holds a credential — a
/// declaration, and fine on its own, because a slot with no value is exactly the arrangement this
/// product wants. With a value already in it, the declaration names what is travelling: refused
/// whatever the slot is called, because a heuristic over names is a belt and the publisher's own
/// word is not.
///
/// # Errors
/// One [`McpRefusal`]; mirrors the web tier's `checkNamedSlot`.
fn check_named_slot(entry: &Value, what: &str) -> Result<(), McpRefusal> {
    let name = entry
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if name.is_empty() {
        return refuse(
            McpRefusalCode::Invalid,
            format!("every {what} needs a name"),
        );
    }
    let filled = literal_value_of(entry).is_some();
    if entry.get("isSecret").and_then(Value::as_bool) == Some(true) && filled {
        return refuse(
            McpRefusalCode::SecretRefused,
            format!(
                "its {what} {name} is declared secret and arrives with the value filled in — a \
                 shared bundle names the slot and lets each machine fill it"
            ),
        );
    }
    if looks_like_credential_name(name) && filled {
        return refuse(
            McpRefusalCode::SecretRefused,
            format!(
                "its {what} {name} arrives with the value filled in — a shared bundle names the \
                 slot and lets each machine fill it"
            ),
        );
    }
    Ok(())
}

/// The `arguments[]` rules: STRUCTURED objects, per the registry schema — never bare strings.
///
/// # Errors
/// One [`McpRefusal`]; mirrors the web tier's `checkArguments`.
fn check_arguments(value: Option<&Value>, what: &str) -> Result<(), McpRefusal> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let Some(list) = value.as_array() else {
        return refuse(
            McpRefusalCode::Invalid,
            format!("its {what} is a list of argument objects"),
        );
    };
    for entry in list {
        if !entry.is_object() {
            return refuse(
                McpRefusalCode::Invalid,
                format!(
                    "its {what} holds argument objects, not plain strings — each one states its \
                     type"
                ),
            );
        }
        match entry.get("type").and_then(Value::as_str) {
            Some("named") => check_named_slot(entry, "flag")?,
            Some("positional") => {
                let hint = entry
                    .get("valueHint")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if literal_value_of(entry).is_none() && hint.is_empty() {
                    return refuse(
                        McpRefusalCode::Invalid,
                        "one of its positional arguments needs a value or a valueHint",
                    );
                }
            }
            _ => {
                return refuse(
                    McpRefusalCode::Invalid,
                    format!("every {what} entry is a positional or a named argument"),
                );
            }
        }
    }
    Ok(())
}

/// ONE `packages[]` entry, judged: the registry's required fields, a transport this format knows,
/// structured arguments, credential-free filled-in slots, and — the rule this product adds — ONE
/// IMMUTABLE THING per entry.
///
/// The pinning rule is per registry type because the formats pin differently: npm and pypi carry a
/// `version` field, an OCI reference carries its digest inside the identifier, and an MCPB
/// download is identified by the hash of the file. A type this build has never heard of is still
/// publishable (the vocabulary is open-world) and is held to the one rule that reads the same
/// everywhere: a `version` it does state must name a single version.
///
/// # Errors
/// One [`McpRefusal`]; the check order is the web tier's, exactly.
fn check_package(entry: &Value) -> Result<(), McpRefusal> {
    if !entry.is_object() {
        return refuse(
            McpRefusalCode::Invalid,
            "every one of its packages is an object",
        );
    }
    let registry_type = entry
        .get("registryType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if registry_type.is_empty() {
        return refuse(
            McpRefusalCode::Invalid,
            "every package says which registry it comes from (registryType)",
        );
    }
    let identifier = entry
        .get("identifier")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if identifier.trim().is_empty() {
        return refuse(McpRefusalCode::Invalid, "every package needs an identifier");
    }
    if identifier.chars().any(char::is_whitespace) {
        return refuse(
            McpRefusalCode::Invalid,
            "a package identifier carries no spaces",
        );
    }
    let transport_type = entry
        .get("transport")
        .filter(|t| t.is_object())
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str);
    let Some(transport_type) = transport_type else {
        return refuse(
            McpRefusalCode::Invalid,
            "every package declares how it is spoken to (transport)",
        );
    };
    if !PACKAGE_TRANSPORTS.contains(&transport_type) {
        return refuse(
            McpRefusalCode::Invalid,
            format!(
                "a package transport is stdio, {STREAMABLE_HTTP} or sse — {transport_type} is none \
                 of those"
            ),
        );
    }
    let transport = entry.get("transport").unwrap_or(&Value::Null);
    if transport_type != "stdio"
        && transport
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .is_empty()
    {
        return refuse(
            McpRefusalCode::Invalid,
            "a package that speaks http declares the URL it listens on",
        );
    }
    // A package's own transport headers are the machine's to fill; a filled-in credential is not.
    if let Some(headers) = transport.get("headers").filter(|h| !h.is_null()) {
        let Some(list) = headers.as_array() else {
            return refuse(McpRefusalCode::Invalid, "a transport's headers are a list");
        };
        for header in list {
            if !header.is_object() {
                return refuse(McpRefusalCode::Invalid, "every header is an object");
            }
            check_named_slot(header, "header")?;
        }
    }
    if let Some(environment) = entry.get("environmentVariables").filter(|e| !e.is_null()) {
        let Some(list) = environment.as_array() else {
            return refuse(
                McpRefusalCode::Invalid,
                "its environmentVariables is a list of named variables",
            );
        };
        for variable in list {
            if !variable.is_object() {
                return refuse(
                    McpRefusalCode::Invalid,
                    "every environment variable is an object with a name",
                );
            }
            check_named_slot(variable, "environment variable")?;
        }
    }
    check_arguments(entry.get("runtimeArguments"), "runtimeArguments")?;
    check_arguments(entry.get("packageArguments"), "packageArguments")?;

    let raw_version = entry.get("version").filter(|v| !v.is_null());
    if let Some(v) = raw_version
        && !v.is_string()
    {
        return refuse(McpRefusalCode::Invalid, "a package version is a string");
    }
    let version = raw_version
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty());
    let lowered = registry_type.to_ascii_lowercase();
    let exact = exact_version_of(&lowered);
    match lowered.as_str() {
        "oci" => {
            if !oci_reference_is_pinned(identifier) {
                return refuse(
                    McpRefusalCode::PackageUnpinned,
                    format!(
                        "{identifier} names no one image — an OCI package pins itself with an \
                         @sha256: digest, because a tag can be moved to other bytes"
                    ),
                );
            }
        }
        "mcpb" => {
            if !entry
                .get("fileSha256")
                .and_then(Value::as_str)
                .is_some_and(is_file_sha256)
            {
                return refuse(
                    McpRefusalCode::PackageUnpinned,
                    "an MCPB package is identified by the sha-256 of its file (fileSha256) — \
                     without it nobody can tell which bytes arrived",
                );
            }
        }
        _ if exact.is_some() && version.is_none() => {
            return refuse(
                McpRefusalCode::PackageUnpinned,
                format!("a {lowered} package names the exact version every machine installs"),
            );
        }
        _ => {}
    }
    if let Some(version) = version {
        if version == RESERVED_VERSION {
            return refuse(
                McpRefusalCode::PackageUnpinned,
                format!(
                    "{RESERVED_VERSION} is not a version — it is whatever the registry serves today"
                ),
            );
        }
        match exact {
            // An open-world registry type: this build has not read its version grammar, so the
            // honest rule is the one that reads the same everywhere — a spelling that names a SET
            // refuses.
            None if looks_like_version_range(version) => {
                return refuse(
                    McpRefusalCode::PackageUnpinned,
                    format!(
                        "{version} is a range, not a version — a shared bundle installs the same \
                         bytes on every machine"
                    ),
                );
            }
            Some(is_exact) if !is_exact(version) => {
                return refuse(
                    McpRefusalCode::PackageUnpinned,
                    format!(
                        "{version} is not one exact version — a {lowered} package names the one \
                         version every machine installs"
                    ),
                );
            }
            _ => {}
        }
        if version.chars().count() > VERSION_MAX {
            return refuse(
                McpRefusalCode::Invalid,
                "a package version is too long to be one",
            );
        }
    }
    Ok(())
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
        return refuse(
            McpRefusalCode::Invalid,
            "its server.json is not valid UTF-8",
        );
    };
    if text.is_empty() {
        return refuse(McpRefusalCode::Invalid, "its server.json is empty");
    }
    let Ok(parsed) = serde_json::from_str::<Value>(text) else {
        return refuse(
            McpRefusalCode::Invalid,
            "its server.json is not JSON — a server.json is a JSON object",
        );
    };

    // FIRST, before anything is read out of the document: does it carry a credential? Two passes —
    // the raw text (nothing a caller strips can hide from it) and the DECODED strings (nothing a
    // publisher escapes can hide from that one).
    if let Some(kind) = find_secret(text).or_else(|| find_secret_deep(&parsed)) {
        return refuse(
            McpRefusalCode::SecretRefused,
            format!(
                "its server.json carries what looks like a credential ({kind}) — a shared bundle \
                 never holds one"
            ),
        );
    }

    let Some(root) = parsed.as_object() else {
        return refuse(
            McpRefusalCode::Invalid,
            "its server.json is not a JSON object",
        );
    };

    let name = root.get("name").and_then(Value::as_str).unwrap_or_default();
    if name.chars().count() < NAME_MIN || name.chars().count() > NAME_MAX {
        return refuse(
            McpRefusalCode::Invalid,
            format!("its server.json needs a name, {NAME_MIN}–{NAME_MAX} characters"),
        );
    }
    if !is_registry_name(name) {
        return refuse(
            McpRefusalCode::Invalid,
            "its name must be a reverse-DNS namespace and a server name with exactly one slash \
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
            format!("its server.json needs a description, 1–{DESCRIPTION_MAX} characters"),
        );
    }
    let version = root
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if version.is_empty() || version.chars().count() > VERSION_MAX {
        return refuse(McpRefusalCode::Invalid, "its server.json needs a version");
    }
    // ONE version, the server's own: `latest` names whatever is served today and a range names a
    // set, and neither is a thing a workspace can pin its members to.
    if version == RESERVED_VERSION || looks_like_version_range(version) {
        return refuse(
            McpRefusalCode::Invalid,
            format!(
                "its version names one version — {version} names whichever one a machine resolves"
            ),
        );
    }

    let raw_packages = root.get("packages").filter(|p| !p.is_null());
    if raw_packages.is_some_and(|p| !p.is_array()) {
        return refuse(McpRefusalCode::Invalid, "its packages is a list");
    }
    let raw_remotes = root.get("remotes").filter(|r| !r.is_null());
    if raw_remotes.is_some_and(|r| !r.is_array()) {
        return refuse(McpRefusalCode::Invalid, "its remotes is a list");
    }
    let packages: &[Value] = raw_packages
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let remotes: &[Value] = raw_remotes
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);

    // FIRST streamable-http wins: a document may offer several transports, and the ordering is the
    // publisher's own preference. Which one is PLACED is decided here; which ones are JUDGED is
    // every one of them, below.
    let placed = remotes.iter().position(|e| {
        e.get("type").and_then(Value::as_str) == Some(STREAMABLE_HTTP)
            && e.get("url").and_then(Value::as_str).is_some()
    });
    // NOTHING TO DELIVER is the refusal, not "no remote": a document may offer an address, a set
    // of packages, or both. Only a document offering neither leaves an agent with nothing to run.
    if placed.is_none() && packages.is_empty() {
        return refuse(
            McpRefusalCode::NoStreamableRemote,
            "it declares no streamable-http remote and no packages — there is nothing here for an \
             agent to run",
        );
    }

    // EVERY remote runs the hygiene and credential rules, in document order — not just the one
    // that gets placed. A sibling entry (an `sse` fallback, a second endpoint) travels in the same
    // bytes to the same machines, so a credential in its address or its headers is shared exactly
    // as widely as one in the placed remote's.
    let mut headers = Vec::new();
    for (i, entry) in remotes.iter().enumerate() {
        let found = check_remote(entry)?;
        if Some(i) == placed {
            headers = found;
        }
    }
    // …and every PACKAGE the same way, in document order. What a given machine can actually set up
    // is answered where the entry is rendered; publishable is the question here.
    for entry in packages {
        check_package(entry)?;
    }
    let url = placed.map_or_else(String::new, |i| {
        remotes[i]
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    });

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
        transport: if url.is_empty() { "" } else { STREAMABLE_HTTP },
        url,
        headers,
        auth_hint,
    })
}

/// What the endpoint-URL shape check answers. Mirrors the web gate's `new URL()` semantics on
/// the shared vectors: the scheme is case-insensitive; the host must be nonempty and free of
/// spaces / control characters (`https://not a url` and `https://?x` are NOT URLs); nonempty
/// userinfo is its own answer, checked only once the whole address parses (a malformed host
/// wins, exactly as the WHATWG parser throws before userinfo is ever reported). The three
/// spellings the two parsers would read DIFFERENTLY never reach here — [`is_unambiguous_endpoint`]
/// refuses them first, on both tiers.
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
/// required, `README.md` optional), a credential scan over EVERY allowed file's bytes (raw for
/// the sibling; raw + decoded strings for the JSON document, inside [`validate_server_json`]),
/// and then the full document gate. The one gate the publish preflight and the `add --kind mcp` local
/// adopt answer to — the exact mirror of the web tier's `validateCandidateFiles`.
///
/// # Errors
/// One [`McpRefusal`]; the check order is the web tier's, exactly.
pub(crate) fn validate_candidate_files(files: &[(&str, &[u8])]) -> Result<McpSummary, McpRefusal> {
    let Some((_, server)) = files.iter().find(|(path, _)| *path == "server.json") else {
        return refuse(
            McpRefusalCode::Invalid,
            "an MCP bundle carries server.json at its root, and this one has none",
        );
    };
    for (path, _) in files {
        if !MCP_ALLOWED_FILES.contains(path) {
            return refuse(
                McpRefusalCode::Invalid,
                format!(
                    "an MCP bundle may hold only {}, and {path} is not one of those",
                    MCP_ALLOWED_FILES.join(", ")
                ),
            );
        }
    }
    if server.len() > MAX_SERVER_JSON_BYTES {
        return refuse(
            McpRefusalCode::Invalid,
            "its server.json is too large to be a server document",
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

    /// The VALID vectors are not just accepted — the summary they yield is the one the
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

        // Two remotes offered: the first streamable-http one is PLACED, and the clean sse sibling
        // is judged by the same rules without changing what the summary reports.
        let both = validate_server_json(&read("remote-sse-and-streamable.json")).expect("ok");
        assert_eq!(both.url, "https://calendar.acme.example/mcp");
    }

    /// EVERY `remotes[]` entry answers the hygiene and credential rules — not just the one that
    /// gets placed. A sibling entry travels in the same bytes to the same machines, so a password
    /// in its address or a credential in its headers is shared exactly as widely.
    #[test]
    fn every_remote_is_judged_not_only_the_one_that_gets_placed() {
        let doc = |sibling: &str| {
            format!(
                r#"{{"name":"io.github.a/b","description":"d","version":"1","remotes":[{sibling},{{"type":"streamable-http","url":"https://clean.example/mcp"}}]}}"#
            )
        };
        let code = |sibling: &str| {
            validate_server_json(doc(sibling).as_bytes())
                .map(|_| ())
                .map_err(|e| e.code)
        };
        // A fallback whose address carries a password.
        assert_eq!(
            code(r#"{"type":"sse","url":"https://svc:hunter2pw@internal.example/sse"}"#),
            Err(McpRefusalCode::SecretRefused)
        );
        // …one presenting a credential header.
        assert_eq!(
            code(
                r#"{"type":"sse","url":"https://x.example/sse","headers":[{"name":"Authorization","value":"Basic Zm9vOmJhcg=="}]}"#
            ),
            Err(McpRefusalCode::SecretRefused)
        );
        // …one whose header is a per-machine slot.
        assert_eq!(
            code(r#"{"type":"sse","url":"https://x.example/sse","headers":[{"name":"X-Key"}]}"#),
            Err(McpRefusalCode::SecretRefused)
        );
        // …one on the clear.
        assert_eq!(
            code(r#"{"type":"sse","url":"http://x.example/sse"}"#),
            Err(McpRefusalCode::InsecureUrl)
        );
        // …one that is still a template.
        assert_eq!(
            code(r#"{"type":"sse","url":"https://{tenant}.example/sse"}"#),
            Err(McpRefusalCode::UrlTemplate)
        );
        // …and one whose address is not an address at all.
        assert_eq!(
            code(r#"{"type":"sse","url":"not a url"}"#),
            Err(McpRefusalCode::Invalid)
        );
        // A clean sibling passes, as does an entry naming no address — there is nothing to judge
        // in one, and any `type` stays legal.
        assert_eq!(
            code(r#"{"type":"sse","url":"https://x.example/sse"}"#),
            Ok(())
        );
        assert_eq!(code(r#"{"type":"stdio"}"#), Ok(()));
        // The sibling is judged, never reported: the summary still carries the PLACED remote's
        // address and its headers alone.
        let summary = validate_server_json(
            doc(r#"{"type":"sse","url":"https://x.example/sse","headers":[{"name":"X-Old","value":"1"}]}"#)
                .as_bytes(),
        )
        .expect("ok");
        assert_eq!(summary.url, "https://clean.example/mcp");
        assert!(summary.headers.is_empty());
    }

    /// A PACKAGE-ONLY bundle is a publishable bundle, and it reports NO address: the two fields a
    /// caller must ask about before printing one come back empty rather than lying.
    #[test]
    fn a_package_only_bundle_publishes_and_names_no_address() {
        let root = fixtures_root();
        let summary = validate_server_json(
            &std::fs::read(root.join("valid/package-npm.json")).expect("a vector"),
        )
        .expect("a package-only bundle is publishable");
        assert_eq!(summary.name, "io.github.acme/files");
        assert_eq!(summary.url, "");
        assert_eq!(summary.transport, "");
        // …while a document offering BOTH still reports the remote it places.
        let both = validate_server_json(
            &std::fs::read(root.join("valid/package-and-remote.json")).expect("a vector"),
        )
        .expect("both ways at once");
        assert_eq!(both.url, "https://mail.acme.example/mcp");
        assert_eq!(both.transport, STREAMABLE_HTTP);
    }

    /// ONE IMMUTABLE THING per package — spelled differently per registry type, refused with one
    /// code. The type vocabulary itself stays OPEN: an unknown type publishes.
    #[test]
    fn a_package_that_names_no_one_thing_refuses_unpinned() {
        let doc = |package: &str| {
            format!(
                r#"{{"name":"io.github.a/b","description":"d","version":"1.0.0","packages":[{package}]}}"#
            )
        };
        let code = |package: &str| {
            validate_server_json(doc(package).as_bytes())
                .map(|_| ())
                .map_err(|e| e.code)
        };
        let npm = |version: &str| {
            format!(
                r#"{{"registryType":"npm","identifier":"@a/x","version":"{version}","transport":{{"type":"stdio"}}}}"#
            )
        };
        assert_eq!(code(&npm("1.2.3")), Ok(()));
        for unpinned in ["^1.2.3", "~1.2", ">=1.0.0", "1.x", "1.2.*", "latest"] {
            assert_eq!(
                code(&npm(unpinned)),
                Err(McpRefusalCode::PackageUnpinned),
                "{unpinned}"
            );
        }
        // npm and pypi carry the version in a FIELD, so its absence is the same refusal.
        assert_eq!(
            code(r#"{"registryType":"pypi","identifier":"a-b","transport":{"type":"stdio"}}"#),
            Err(McpRefusalCode::PackageUnpinned)
        );
        // An OCI package pins inside its identifier, and ONLY with a digest: every tag spelling —
        // a version-looking one included — is a label the publisher can move to other bytes.
        let oci = |identifier: &str| {
            format!(
                r#"{{"registryType":"oci","identifier":"{identifier}","transport":{{"type":"stdio"}}}}"#
            )
        };
        assert_eq!(
            code(&oci(&format!(
                "ghcr.io/a/x@sha256:{}",
                "0123456789abcdef".repeat(4)
            ))),
            Ok(())
        );
        for unpinned in [
            "ghcr.io/a/x:1.2.3",
            "ghcr.io/a/x:latest",
            "ghcr.io/a/x",
            "localhost:5000/a/x",
            // A digest-looking tail that is not a whole digest pins nothing either.
            "ghcr.io/a/x@sha256:0123456789abcdef",
        ] {
            assert_eq!(
                code(&oci(unpinned)),
                Err(McpRefusalCode::PackageUnpinned),
                "{unpinned}"
            );
        }
        // A type this build never heard of publishes — the vocabulary is the format's, not ours.
        assert_eq!(
            code(
                r#"{"registryType":"cargo","identifier":"acme-mcp","version":"0.3.1","transport":{"type":"stdio"}}"#
            ),
            Ok(())
        );
        // …and is still held to the one rule that reads the same everywhere.
        assert_eq!(
            code(
                r#"{"registryType":"cargo","identifier":"acme-mcp","version":"^0.3","transport":{"type":"stdio"}}"#
            ),
            Err(McpRefusalCode::PackageUnpinned)
        );
    }

    /// The two version grammars this build reads, probe for probe. The web tier's suite drives the
    /// SAME table: a grammar that answered differently on the two sides would publish a bundle no
    /// client could ever place, which is the whole failure the shared vectors exist to prevent.
    #[test]
    fn the_exact_version_grammars_take_one_version_and_nothing_else() {
        for one in [
            "1.2.3",
            "0.0.0",
            "10.20.30",
            "1.2.3-beta.1",
            "1.2.3-rc.1+build.5",
            "1.2.3+build.5",
            "1.0.0-alpha-1",
        ] {
            assert!(is_exact_semver(one), "npm: {one}");
        }
        for many in [
            "*", "x", "next", "latest", "1", "1.2", "v1.2.3", "1.2.3.4", "^1.2.3", ">=1.0.0", "1.x",
            "01.2.3", "1.2.3-", "1.2.3+", "1.2.3-01", " 1.2.3", "",
        ] {
            assert!(!is_exact_semver(many), "npm: {many}");
        }
        for one in [
            "1.0",
            "0.4.2",
            "2.0.0rc1",
            "1.0.dev1",
            "1!2.0",
            "1.0.post1",
            "1.0b2.post345.dev456",
            "1.0+ubuntu.1",
            "v1.0",
        ] {
            assert!(is_exact_pep440(one), "pypi: {one}");
        }
        for many in [
            "*", "latest", "next", "1.*", "1.0.*", ">=1.0", "==1.0", "~=1.0", "1.0+", "abc",
            "1.0 ", "",
        ] {
            assert!(!is_exact_pep440(many), "pypi: {many}");
        }
    }

    /// A package's environment variables and flags are SLOTS the running machine fills. Naming one
    /// for a credential is fine — `isSecret` and all; arriving with the value in it is not.
    #[test]
    fn a_package_slot_is_a_slot_until_somebody_fills_it_in() {
        let doc = |env: &str| {
            format!(
                r#"{{"name":"io.github.a/b","description":"d","version":"1.0.0","packages":[{{"registryType":"npm","identifier":"@a/x","version":"1.0.0","transport":{{"type":"stdio"}},"environmentVariables":[{env}]}}]}}"#
            )
        };
        let code = |env: &str| {
            validate_server_json(doc(env).as_bytes())
                .map(|_| ())
                .map_err(|e| e.code)
        };
        assert_eq!(
            code(r#"{"name":"GITHUB_TOKEN","isSecret":true,"isRequired":true}"#),
            Ok(())
        );
        assert_eq!(code(r#"{"name":"ACME_ROOT","value":"/srv"}"#), Ok(()));
        // The DOCUMENT'S OWN WORD outranks the name heuristic: a slot nothing about the name
        // suspects, declared secret, arriving with an ordinary-looking value in it.
        assert_eq!(
            code(r#"{"name":"SESSION","isSecret":true,"value":"changeme"}"#),
            Err(McpRefusalCode::SecretRefused)
        );
        for name in ["GITHUB_TOKEN", "github-token", "acmeApiKey", "DB_PASSWORD"] {
            assert_eq!(
                code(&format!(r#"{{"name":"{name}","value":"filled-in"}}"#)),
                Err(McpRefusalCode::SecretRefused),
                "{name}"
            );
        }
        // A nameless slot is malformed, not secret.
        assert_eq!(
            code(r#"{"value":"anything"}"#),
            Err(McpRefusalCode::Invalid)
        );
    }

    /// A sha-256 is 64 hex characters, which is exactly the shape the entropy belt refuses — and
    /// the format publishes digests in two ordinary places. Both are integrity, not credentials;
    /// a 64-hex run anywhere ELSE still refuses.
    #[test]
    fn integrity_digests_are_not_credentials() {
        let hex = "0123456789abcdef".repeat(4);
        assert_eq!(find_secret(&format!("ghcr.io/a/x@sha256:{hex}")), None);
        assert_eq!(find_secret(&format!(r#"{{"fileSha256": "{hex}"}}"#)), None);
        assert_eq!(
            find_secret(&format!(r#"{{"value":"{hex}"}}"#)),
            Some("high-entropy value")
        );
        // The DECODED walk answers the same way — a digest key spares its own value only.
        let root = fixtures_root();
        for vector in [
            "valid/package-oci-digest.json",
            "valid/package-mcpb-hash.json",
        ] {
            validate_server_json(&std::fs::read(root.join(vector)).expect("a vector"))
                .unwrap_or_else(|e| panic!("{vector}: {} — {}", e.code.as_str(), e.message));
        }
    }

    /// The three spellings a WHATWG parser SILENTLY REPAIRS and this hand-parse does not. Each
    /// one would otherwise be a document the web door publishes and every member's converge
    /// refuses on every sweep, forever — so the shape is refused up front, on both tiers, with
    /// the same code.
    ///
    /// The backslash case is what pins the ORDER: reaching the userinfo rule with it answers
    /// `MCP_SECRET_REFUSED` here (`h\` reads as a user name) while the web tier, whose parser
    /// ends the authority at the backslash, sees no userinfo at all. The pre-scan running first
    /// is what makes the two answers one answer.
    #[test]
    fn an_endpoint_the_two_parsers_read_apart_refuses_as_invalid() {
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
        // No `://` at all: the WHATWG parser supplies the slashes for a special scheme.
        assert_eq!(
            code("https:example.com/mcp"),
            Err(McpRefusalCode::Invalid),
            "a scheme with no slashes is not an address"
        );
        // A backslash ENDS the authority over there and reads as userinfo over here.
        assert_eq!(
            code(r"https://h\\@x.example/mcp"),
            Err(McpRefusalCode::Invalid),
            "the shape refuses before the userinfo rule can call it a credential"
        );
        // Tab, CR and LF are DELETED from the input before the WHATWG parse.
        for ws in [r"\t", r"\r", r"\n"] {
            assert_eq!(
                code(&format!("https://x{ws}.example/mcp")),
                Err(McpRefusalCode::Invalid),
                "whitespace one parser strips cannot decide a host"
            );
        }
        // The rule is the whole URL, not just its authority: a backslash in the path is a `/` to
        // the WHATWG parser and a literal byte here, so it is refused wherever it sits.
        assert_eq!(
            code(r"https://x.example\\mcp"),
            Err(McpRefusalCode::Invalid)
        );
        // …and an ordinary address is untouched by any of it.
        assert_eq!(code("https://x.example/mcp"), Ok(()));
        // The sentence names the shape rather than echoing the address back.
        let refusal = validate_server_json(doc("https:example.com/mcp").as_bytes())
            .expect_err("a scheme with no slashes refuses");
        assert!(refusal.message.contains("scheme://host"));
        assert!(!refusal.message.contains("example.com"));
    }

    /// The length ceilings count CHARACTERS on both tiers. A description of astral-plane text
    /// fits inside 100 of them while occupying 134 UTF-16 units — the unit the web tier used to
    /// count in, which refused what this one had already accepted.
    #[test]
    fn the_ceilings_count_characters_not_utf16_units() {
        let root = fixtures_root();
        let astral = validate_server_json(
            &std::fs::read(root.join("valid/astral-description.json")).expect("a vector"),
        )
        .expect("84 characters is inside the ceiling, whatever they encode to");
        assert_eq!(astral.description.chars().count(), 84);
        assert!(astral.description.encode_utf16().count() > DESCRIPTION_MAX);
    }

    /// The two rules the placement engine enforces when it renders a config entry, enforced HERE
    /// so a bundle can never be publishable and permanently unplaceable: a header `variables`
    /// block declared at all (an empty `{}` included), and a brace pair in a header VALUE.
    #[test]
    fn a_header_that_could_never_be_placed_refuses_at_the_gate() {
        let doc = |header: &str| {
            format!(
                r#"{{"name":"io.github.a/b","description":"d","version":"1","remotes":[{{"type":"streamable-http","url":"https://x.example/mcp","headers":[{header}]}}]}}"#
            )
        };
        let code = |header: &str| {
            validate_server_json(doc(header).as_bytes())
                .map(|_| ())
                .map_err(|e| e.code)
        };
        // An empty variables block lists no slot — it is a fill-in all the same.
        assert_eq!(
            code(r#"{"name":"X-T","value":"acme","variables":{}}"#),
            Err(McpRefusalCode::SecretRefused)
        );
        assert_eq!(
            code(r#"{"name":"X-T","value":"acme","variables":{"tenant":{"description":"which"}}}"#),
            Err(McpRefusalCode::SecretRefused)
        );
        // A brace pair in the value is the header's own template.
        assert_eq!(
            code(r#"{"name":"X-T","value":"{tenant}"}"#),
            Err(McpRefusalCode::UrlTemplate)
        );
        assert_eq!(
            code(r#"{"name":"X-T","value":"tenant-{id}-eu"}"#),
            Err(McpRefusalCode::UrlTemplate)
        );
        // A lone brace is not a template — and an explicit null declares nothing.
        assert_eq!(code(r#"{"name":"X-T","value":"a{b"}"#), Ok(()));
        assert_eq!(
            code(r#"{"name":"X-T","value":"acme","variables":null}"#),
            Ok(())
        );
    }

    /// The `sk-` shape wears a LEFT BOUNDARY: a key never starts mid-word, so hyphenated prose
    /// (`task-management-and-scheduling`) is not a credential — while a real key still is.
    #[test]
    fn the_openai_shape_reads_keys_and_not_hyphenated_prose() {
        assert_eq!(find_secret("task-management-and-scheduling"), None);
        assert_eq!(
            find_secret("io.github.acme/task-management-and-scheduling"),
            None
        );
        assert_eq!(
            find_secret(r#"{"value":"sk-proj-9fJ2mQ8xVb3nT7kLp0WzYcRd5AeHgUiO"}"#),
            Some("openai-style-key")
        );
        // At the very start of the text, and after a separator that is not alphanumeric.
        assert_eq!(
            find_secret(&format!("sk-{}", "a".repeat(20))),
            Some("openai-style-key")
        );
        assert_eq!(
            find_secret(&format!(" sk-{}", "a".repeat(20))),
            Some("openai-style-key")
        );
        // The whole document says the same thing, both ways round.
        let root = fixtures_root();
        let prose = validate_server_json(
            &std::fs::read(root.join("valid/hyphenated-prose-name.json")).expect("a vector"),
        )
        .expect("hyphenated prose is not a credential");
        assert_eq!(prose.name, "io.github.acme/task-management-and-scheduling");
        let key = validate_server_json(
            &std::fs::read(root.join("invalid/openai-key-header.json")).expect("a vector"),
        )
        .expect_err("a real key refuses");
        assert_eq!(key.code, McpRefusalCode::SecretRefused);
        assert!(key.message.contains("openai-style-key"), "{}", key.message);
        // …and the refusal never echoes the value back.
        assert!(!key.message.contains("sk-proj"), "{}", key.message);
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

    /// ITEM PAIR (sibling files): the candidate gate refuses any file outside the allowed pair
    /// (naming the set) and runs the credential scan over every allowed sibling's bytes. The
    /// pre-fix gates read server.json alone and let both candidates through.
    #[test]
    fn the_candidate_allowlist_refuses_strays_and_scans_sibling_bytes() {
        let server =
            std::fs::read(fixtures_root().join("valid/remote-no-auth.json")).expect("fixture");
        // The exact allowed pair passes.
        let readme = b"How to use this server.\n".to_vec();
        assert!(
            validate_candidate_files(&[
                ("server.json", server.as_slice()),
                ("README.md", readme.as_slice()),
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
            e.message.contains("server.json, README.md"),
            "{}",
            e.message
        );
        assert!(e.message.contains("evil.sh"), "{}", e.message);
        // A `topos-mcp.toml` is a stray like any other.
        let e = validate_candidate_files(&[
            ("server.json", server.as_slice()),
            ("topos-mcp.toml", b"# config\n".as_slice()),
        ])
        .expect_err("refused");
        assert_eq!(e.code, McpRefusalCode::Invalid);
        assert!(
            e.message.contains("server.json, README.md"),
            "{}",
            e.message
        );
        assert!(e.message.contains("topos-mcp.toml"), "{}", e.message);
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
