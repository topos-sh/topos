//! The reference grammar — SHAPE-DETERMINED: what you typed decides what it means, with no
//! resolution order, no fallbacks, and no questions.
//!
//! | You type                     | Means                                                     |
//! |------------------------------|-----------------------------------------------------------|
//! | `code-review`                | a bundle in your connected workspaces (unique or error)   |
//! | `@acme/code-review`          | workspace acme's bundle — `@` = a workspace, always       |
//! | `@acme/channels/backend`     | workspace acme's channel (the `/channels/` path segment)  |
//! | `vercel-labs/skills`         | `github.com/vercel-labs/skills` — slash without `@` = GitHub |
//! | `./my-skill`                 | a local folder (path shape)                               |
//! | `topos.sh/acme/code-review`  | explicit host — the canonical form manifests store        |
//!
//! Pinned rules: bare `@name` (no slash) is INVALID SYNTAX (also closes the PowerShell
//! splatting edge); the `#` sigil is BANNED everywhere; everything after `@` is lowercase
//! letters, digits, hyphens, and slashes. A trailing `@<pin>` names a version — the FULL
//! 64-hex content digest for workspace bundles, a 7–40-hex commit for GitHub refs.
//!
//! Manifests always store the CANONICAL host-qualified form ([`ParsedRef::canonical`] — the
//! CLI canonicalizes on write), so a `topos.toml` read anywhere is never ambiguous, and
//! canonical refs double as web URL paths: paste a skill's or channel's page URL into
//! `topos add` and it parses here.

use std::fmt;

/// A reference, parsed by SHAPE alone (no network, no local state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParsedRef {
    /// `code-review` — a bare catalog name, resolved against connected workspaces later.
    Bare { name: String, pin: Option<String> },
    /// `@acme/code-review` or `host/acme/code-review` — a workspace's bundle. `host` is
    /// `None` for the `@` spelling (the connected server supplies it at canonicalization).
    Skill {
        host: Option<String>,
        workspace: String,
        name: String,
        pin: Option<String>,
    },
    /// `@acme/channels/backend` or `host/acme/channels/backend` — a workspace's channel.
    Channel {
        host: Option<String>,
        workspace: String,
        name: String,
    },
    /// `owner/repo[/sub/dir]` or `github.com/owner/repo[...]` — GitHub, always (the
    /// `npx skills add` shape). `pin` is a commit; external refs are PINNED by default at add.
    GitHub {
        owner: String,
        repo: String,
        /// The subdirectory inside the repo ("" = the root).
        subdir: String,
        pin: Option<String>,
    },
    /// `./my-skill`, `../x`, `/abs/path`, `~/x` — a local folder.
    LocalPath { raw: String },
}

impl ParsedRef {
    /// The LAST-SEGMENT name a manifest dedupes on (nearest manifest wins per NAME).
    pub(crate) fn item_name(&self) -> &str {
        match self {
            ParsedRef::Bare { name, .. }
            | ParsedRef::Skill { name, .. }
            | ParsedRef::Channel { name, .. } => name,
            ParsedRef::GitHub { repo, subdir, .. } => {
                if subdir.is_empty() {
                    repo
                } else {
                    subdir.rsplit('/').next().unwrap_or(repo)
                }
            }
            ParsedRef::LocalPath { raw } => {
                raw.trim_end_matches('/').rsplit('/').next().unwrap_or(raw)
            }
        }
    }

    /// The canonical spelling a manifest STORES (host-qualified, pin-free — the pin is the
    /// entry's value, not part of the key). `default_host` fills the `@` spellings; a still
    /// host-less workspace ref cannot canonicalize (the caller resolves the session first).
    pub(crate) fn canonical(&self, default_host: Option<&str>) -> Option<String> {
        match self {
            ParsedRef::Bare { .. } => None,
            ParsedRef::Skill {
                host,
                workspace,
                name,
                ..
            } => {
                let host = host.as_deref().or(default_host)?;
                Some(format!("{host}/{workspace}/{name}"))
            }
            ParsedRef::Channel {
                host,
                workspace,
                name,
            } => {
                let host = host.as_deref().or(default_host)?;
                Some(format!("{host}/{workspace}/channels/{name}"))
            }
            ParsedRef::GitHub {
                owner,
                repo,
                subdir,
                ..
            } => {
                if subdir.is_empty() {
                    Some(format!("github.com/{owner}/{repo}"))
                } else {
                    Some(format!("github.com/{owner}/{repo}/{subdir}"))
                }
            }
            ParsedRef::LocalPath { raw } => Some(raw.clone()),
        }
    }

    /// The pin the spelling carried (`…@<pin>`), if any.
    pub(crate) fn pin(&self) -> Option<&str> {
        match self {
            ParsedRef::Bare { pin, .. }
            | ParsedRef::Skill { pin, .. }
            | ParsedRef::GitHub { pin, .. } => pin.as_deref(),
            ParsedRef::Channel { .. } | ParsedRef::LocalPath { .. } => None,
        }
    }
}

/// A typed grammar refusal — every message names the way back, never a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefError {
    pub message: String,
}

impl fmt::Display for RefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

fn err(message: impl Into<String>) -> RefError {
    RefError {
        message: message.into(),
    }
}

/// The one NAME charset (workspace slugs, bundle names, channel names): lowercase letters,
/// digits, hyphens; must start alphanumeric.
fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A GitHub owner/repo segment (their charset: alphanumerics, hyphens, dots, underscores).
fn is_github_segment(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
}

/// A host: has a dot (`topos.sh`, `github.com`, `topos.example.com`) or a port; localhost
/// counts. Distinguishes `topos.sh/acme/x` from the GitHub shorthand `owner/repo`.
fn is_host(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let bare = s.split(':').next().unwrap_or(s);
    (bare.contains('.') || bare == "localhost")
        && bare
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// Validate a pin's length for its arm: a workspace bundle pins the FULL 64-hex content
/// digest (an abbreviation could never be resolved consistently across surfaces); a GitHub
/// ref pins a commit (7–40 hex — git's own abbreviation rules).
fn check_pin(pin: &Option<String>, github: bool) -> Result<(), RefError> {
    if let Some(p) = pin {
        if github {
            if p.len() < 7 || p.len() > 40 {
                return Err(err(
                    "a GitHub pin is a commit hash — 7 to 40 hex characters",
                ));
            }
        } else if p.len() != 64 {
            return Err(err(
                "a skill pin is the full 64-character version digest (from `topos log`)",
            ));
        }
    }
    Ok(())
}

/// Validate a manifest ENTRY VALUE as a pin for its reference's kind — the same rules the
/// `@pin` spelling gets, applied to the `"<ref>" = "<pin>"` form (a handwritten manifest is
/// no back door): hex only; 64 for a workspace bundle, 7–40 for a GitHub commit; a channel
/// or path entry takes no pin at all.
pub(crate) fn entry_pin_error(parsed: &ParsedRef, pin: &str) -> Result<(), RefError> {
    if !pin.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(err("a pin is hex — a version digest or a commit hash"));
    }
    match parsed {
        ParsedRef::Bare { .. } | ParsedRef::Skill { .. } => {
            check_pin(&Some(pin.to_string()), false)
        }
        ParsedRef::GitHub { .. } => check_pin(&Some(pin.to_string()), true),
        ParsedRef::Channel { .. } => Err(err("a channel entry takes no version pin")),
        ParsedRef::LocalPath { .. } => Err(err("a local path entry takes no version pin")),
    }
}

/// Split a trailing `@<pin>` off a spelling (the version pin: `<ref>@<hex-digest>` /
/// `owner/repo@<commit>`). Only the LAST `@` counts, and only when what follows looks like a
/// pin (hex, 7–64 chars) — so the leading workspace sigil never collides.
fn split_pin(s: &str) -> (&str, Option<String>) {
    if let Some(at) = s.rfind('@')
        && at > 0
    {
        let candidate = &s[at + 1..];
        let hexish = candidate.len() >= 7
            && candidate.len() <= 64
            && candidate.bytes().all(|b| b.is_ascii_hexdigit());
        if hexish {
            return (&s[..at], Some(candidate.to_ascii_lowercase()));
        }
    }
    (s, None)
}

/// The one GitHub-arm constructor: strip a `.git` tail, refuse an empty repo (a bare
/// `owner/.git` would canonicalize to nonsense), validate the commit-shaped pin.
fn github_ref(
    owner: &str,
    repo: &str,
    subdir: &[&str],
    pin: Option<String>,
) -> Result<ParsedRef, RefError> {
    let repo = repo.trim_end_matches(".git");
    if repo.is_empty() {
        return Err(err(
            "a GitHub reference needs a repository name — `owner/repo`",
        ));
    }
    check_pin(&pin, true)?;
    Ok(ParsedRef::GitHub {
        owner: owner.to_string(),
        repo: repo.to_string(),
        subdir: subdir.join("/"),
        pin,
    })
}

/// Parse ONE reference by shape. See the module table; every arm is total — a spelling either
/// parses to exactly one meaning or refuses typed.
pub(crate) fn parse_ref(raw: &str) -> Result<ParsedRef, RefError> {
    let token = raw.trim();
    if token.is_empty() {
        return Err(err("empty reference"));
    }
    if token.contains('#') {
        return Err(err(
            "the '#' sigil is not part of topos references — a channel is `@<workspace>/channels/<name>`",
        ));
    }

    // Path shape first: `./`, `../`, `/`, `~/`.
    if token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with('/')
        || token.starts_with("~/")
        || token == "."
    {
        return Ok(ParsedRef::LocalPath {
            raw: token.to_string(),
        });
    }

    // A pasted URL: strip the scheme, then fall through to the host-qualified arm.
    let (token, had_scheme) = if let Some(rest) = token.strip_prefix("https://") {
        (rest, true)
    } else if let Some(rest) = token.strip_prefix("http://") {
        (rest, true)
    } else {
        (token, false)
    };

    // `@workspace/…` — a workspace, always.
    if let Some(rest) = token.strip_prefix('@') {
        let (rest, pin) = split_pin(rest);
        let segments: Vec<&str> = rest.split('/').collect();
        match segments.as_slice() {
            [only] => {
                let hint = if only.is_empty() { "<workspace>" } else { only };
                return Err(err(format!(
                    "`@{hint}` alone names nothing — a workspace reference is `@<workspace>/<skill>` or `@<workspace>/channels/<name>`",
                )));
            }
            [ws, name] => {
                if !is_name(ws) || !is_name(name) {
                    return Err(err(format!(
                        "`@{rest}` is not a workspace reference — lowercase letters, digits, and hyphens only",
                    )));
                }
                check_pin(&pin, false)?;
                return Ok(ParsedRef::Skill {
                    host: None,
                    workspace: (*ws).to_string(),
                    name: (*name).to_string(),
                    pin,
                });
            }
            [ws, "channels", name] => {
                if !is_name(ws) || !is_name(name) {
                    return Err(err(format!(
                        "`@{rest}` is not a channel reference — lowercase letters, digits, and hyphens only",
                    )));
                }
                if pin.is_some() {
                    return Err(err("a channel reference takes no version pin"));
                }
                return Ok(ParsedRef::Channel {
                    host: None,
                    workspace: (*ws).to_string(),
                    name: (*name).to_string(),
                });
            }
            _ => {
                return Err(err(format!(
                    "`@{rest}` is not a reference — `@<workspace>/<skill>` or `@<workspace>/channels/<name>`",
                )));
            }
        }
    }

    // No slash: a bare catalog name.
    if !token.contains('/') {
        let (name, pin) = split_pin(token);
        if !is_name(name) {
            return Err(err(format!(
                "`{name}` is not a skill name — lowercase letters, digits, and hyphens only",
            )));
        }
        check_pin(&pin, false)?;
        return Ok(ParsedRef::Bare {
            name: name.to_string(),
            pin,
        });
    }

    // Slash without `@`: host-qualified canonical (first segment is a host) or GitHub.
    let (token, pin) = split_pin(token);
    let mut segments = token.split('/').filter(|s| !s.is_empty());
    let first = segments.next().unwrap_or_default();
    let rest: Vec<&str> = segments.collect();

    if is_host(first) && first != "github.com" {
        // `host/workspace/name` or `host/workspace/channels/name` — a canonical workspace ref
        // (also exactly a skill/channel page URL on that host).
        return match rest.as_slice() {
            [ws, name] if is_name(ws) && is_name(name) => {
                check_pin(&pin, false)?;
                Ok(ParsedRef::Skill {
                    host: Some(first.to_string()),
                    workspace: (*ws).to_string(),
                    name: (*name).to_string(),
                    pin,
                })
            }
            // The skill page URL shape (`/skills/<name>`) parses too — paste-a-URL works.
            [ws, "skills", name] if is_name(ws) && is_name(name) => {
                check_pin(&pin, false)?;
                Ok(ParsedRef::Skill {
                    host: Some(first.to_string()),
                    workspace: (*ws).to_string(),
                    name: (*name).to_string(),
                    pin,
                })
            }
            [ws, "channels", name] if is_name(ws) && is_name(name) => {
                if pin.is_some() {
                    return Err(err("a channel reference takes no version pin"));
                }
                Ok(ParsedRef::Channel {
                    host: Some(first.to_string()),
                    workspace: (*ws).to_string(),
                    name: (*name).to_string(),
                })
            }
            _ => Err(err(format!(
                "`{token}` is not a reference — `<host>/<workspace>/<skill>` or `<host>/<workspace>/channels/<name>`",
            ))),
        };
    }

    if first == "github.com" || had_scheme {
        // `github.com/owner/repo[/sub/dir]` (or any scheme-carrying URL that got here) — the
        // SAME constructor as the bare shorthand, so the URL spelling gets the same guards.
        return match rest.as_slice() {
            [owner, repo, subdir @ ..] if is_github_segment(owner) && is_github_segment(repo) => {
                github_ref(owner, repo, subdir, pin)
            }
            _ => Err(err(format!(
                "`{token}` is not a GitHub reference — `github.com/<owner>/<repo>[/<subdir>]`",
            ))),
        };
    }

    // The bare GitHub shorthand: `owner/repo[/sub/dir]` — slash without `@` is GitHub, always.
    let mut parts = vec![first];
    parts.extend(rest);
    match parts.as_slice() {
        [owner, repo, subdir @ ..] if is_github_segment(owner) && is_github_segment(repo) => {
            github_ref(owner, repo, subdir, pin)
        }
        _ => Err(err(format!(
            "`{token}` is not a reference — `owner/repo` reads as GitHub; a workspace is `@<workspace>/<skill>`",
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(host: Option<&str>, ws: &str, name: &str, pin: Option<&str>) -> ParsedRef {
        ParsedRef::Skill {
            host: host.map(String::from),
            workspace: ws.into(),
            name: name.into(),
            pin: pin.map(String::from),
        }
    }

    #[test]
    fn the_grammar_table() {
        // The artifact table, row by row.
        assert_eq!(
            parse_ref("code-review").unwrap(),
            ParsedRef::Bare {
                name: "code-review".into(),
                pin: None
            }
        );
        assert_eq!(
            parse_ref("@acme/code-review").unwrap(),
            skill(None, "acme", "code-review", None)
        );
        assert_eq!(
            parse_ref("@acme/channels/backend").unwrap(),
            ParsedRef::Channel {
                host: None,
                workspace: "acme".into(),
                name: "backend".into()
            }
        );
        assert_eq!(
            parse_ref("vercel-labs/skills").unwrap(),
            ParsedRef::GitHub {
                owner: "vercel-labs".into(),
                repo: "skills".into(),
                subdir: String::new(),
                pin: None
            }
        );
        assert_eq!(
            parse_ref("./my-skill").unwrap(),
            ParsedRef::LocalPath {
                raw: "./my-skill".into()
            }
        );
        assert_eq!(
            parse_ref("topos.sh/acme/code-review").unwrap(),
            skill(Some("topos.sh"), "acme", "code-review", None)
        );
    }

    #[test]
    fn bare_at_name_is_invalid_syntax() {
        assert!(parse_ref("@acme").is_err());
        assert!(parse_ref("@").is_err());
        // The refusal teaches the shape.
        let e = parse_ref("@acme").unwrap_err();
        assert!(e.message.contains("@<workspace>/<skill>"), "{e}");
    }

    #[test]
    fn the_hash_sigil_is_banned() {
        for bad in [
            "#backend",
            "@acme/#backend",
            "acme#backend",
            "topos.sh/acme/x#y",
        ] {
            let e = parse_ref(bad).unwrap_err();
            assert!(e.message.contains("channels"), "{bad}: {e}");
        }
    }

    #[test]
    fn pins_split_off_the_tail() {
        let digest = "0123456789abcdef".repeat(4);
        assert_eq!(
            parse_ref(&format!("@acme/deploy@{digest}")).unwrap(),
            skill(None, "acme", "deploy", Some(&digest))
        );
        assert_eq!(
            parse_ref(&format!("code-review@{digest}")).unwrap(),
            ParsedRef::Bare {
                name: "code-review".into(),
                pin: Some(digest.clone())
            }
        );
        match parse_ref("vercel-labs/skills@deadbeef00").unwrap() {
            ParsedRef::GitHub { pin, .. } => assert_eq!(pin.as_deref(), Some("deadbeef00")),
            other => panic!("{other:?}"),
        }
        // A skill pin must be the FULL digest — an abbreviation refuses typed …
        let e = parse_ref("@acme/deploy@abc1234").unwrap_err();
        assert!(e.message.contains("64-character"), "{e}");
        // … and a GitHub pin is commit-shaped (7–40 hex), never the 64-hex digest length.
        assert!(parse_ref(&format!("vercel-labs/skills@{digest}")).is_err());
        // A channel takes no pin.
        assert!(parse_ref(&format!("@acme/channels/backend@{digest}")).is_err());
    }

    #[test]
    fn an_empty_repo_after_the_git_strip_refuses() {
        assert!(parse_ref("owner/.git").is_err());
        // The URL spellings run the SAME constructor — same guards.
        assert!(parse_ref("github.com/owner/.git").is_err());
        assert!(parse_ref("https://github.com/owner/.git").is_err());
        assert!(parse_ref(&format!("https://github.com/owner/repo@{}", "1".repeat(64))).is_err());
        // A real repo's `.git` tail still strips cleanly.
        match parse_ref("owner/repo.git").unwrap() {
            ParsedRef::GitHub { repo, .. } => assert_eq!(repo, "repo"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn urls_paste_cleanly() {
        assert_eq!(
            parse_ref("https://topos.sh/acme/code-review").unwrap(),
            skill(Some("topos.sh"), "acme", "code-review", None)
        );
        // The skill PAGE URL parses to the same ref.
        assert_eq!(
            parse_ref("https://topos.sh/acme/skills/code-review").unwrap(),
            skill(Some("topos.sh"), "acme", "code-review", None)
        );
        assert_eq!(
            parse_ref("https://topos.sh/acme/channels/backend").unwrap(),
            ParsedRef::Channel {
                host: Some("topos.sh".into()),
                workspace: "acme".into(),
                name: "backend".into()
            }
        );
        match parse_ref("https://github.com/vercel-labs/skills/tools/find-skills").unwrap() {
            ParsedRef::GitHub {
                owner,
                repo,
                subdir,
                ..
            } => {
                assert_eq!(owner, "vercel-labs");
                assert_eq!(repo, "skills");
                assert_eq!(subdir, "tools/find-skills");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn self_hosted_hosts_work() {
        assert_eq!(
            parse_ref("topos.example.com/eng/deploy").unwrap(),
            skill(Some("topos.example.com"), "eng", "deploy", None)
        );
        assert_eq!(
            parse_ref("localhost:3000/eng/deploy").unwrap(),
            skill(Some("localhost:3000"), "eng", "deploy", None)
        );
    }

    #[test]
    fn item_names_dedupe_on_the_last_segment() {
        assert_eq!(
            parse_ref("@acme/code-review").unwrap().item_name(),
            "code-review"
        );
        assert_eq!(
            parse_ref("vercel-labs/skills").unwrap().item_name(),
            "skills"
        );
        assert_eq!(
            parse_ref("github.com/o/r/tools/find-skills")
                .unwrap()
                .item_name(),
            "find-skills"
        );
        assert_eq!(
            parse_ref("./tools/my-skill").unwrap().item_name(),
            "my-skill"
        );
    }

    #[test]
    fn canonicalization_is_host_qualified() {
        assert_eq!(
            parse_ref("@acme/code-review")
                .unwrap()
                .canonical(Some("topos.sh")),
            Some("topos.sh/acme/code-review".into())
        );
        // Already host-qualified: the spelled host wins over the default.
        assert_eq!(
            parse_ref("topos.example.com/eng/deploy")
                .unwrap()
                .canonical(Some("topos.sh")),
            Some("topos.example.com/eng/deploy".into())
        );
        assert_eq!(
            parse_ref("@acme/channels/backend")
                .unwrap()
                .canonical(Some("topos.sh")),
            Some("topos.sh/acme/channels/backend".into())
        );
        assert_eq!(
            parse_ref("vercel-labs/skills").unwrap().canonical(None),
            Some("github.com/vercel-labs/skills".into())
        );
        // A host-less workspace ref cannot canonicalize without a session.
        assert_eq!(parse_ref("@acme/x").unwrap().canonical(None), None);
        // Bare names never canonicalize (they resolve first).
        assert_eq!(
            parse_ref("code-review")
                .unwrap()
                .canonical(Some("topos.sh")),
            None
        );
    }

    #[test]
    fn near_miss_refusals_are_typed() {
        assert!(parse_ref("").is_err());
        assert!(parse_ref("Bad_Name").is_err());
        assert!(parse_ref("@acme/Bad_Name").is_err());
        assert!(parse_ref("@acme/channels/x/y").is_err());
    }
}
