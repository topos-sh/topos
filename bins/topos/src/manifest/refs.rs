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
//! letters, digits, hyphens, and slashes. A trailing `@<pin>` names a version (a content
//! digest for workspace bundles, a commit for GitHub refs).
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
            [ws, name] if is_name(ws) && is_name(name) => Ok(ParsedRef::Skill {
                host: Some(first.to_string()),
                workspace: (*ws).to_string(),
                name: (*name).to_string(),
                pin,
            }),
            // The skill page URL shape (`/skills/<name>`) parses too — paste-a-URL works.
            [ws, "skills", name] if is_name(ws) && is_name(name) => Ok(ParsedRef::Skill {
                host: Some(first.to_string()),
                workspace: (*ws).to_string(),
                name: (*name).to_string(),
                pin,
            }),
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
        // `github.com/owner/repo[/sub/dir]` (or any scheme-carrying URL that got here).
        return match rest.as_slice() {
            [owner, repo, subdir @ ..] if is_github_segment(owner) && is_github_segment(repo) => {
                Ok(ParsedRef::GitHub {
                    owner: (*owner).to_string(),
                    repo: repo.trim_end_matches(".git").to_string(),
                    subdir: subdir.join("/"),
                    pin,
                })
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
            Ok(ParsedRef::GitHub {
                owner: (*owner).to_string(),
                repo: repo.trim_end_matches(".git").to_string(),
                subdir: subdir.join("/"),
                pin,
            })
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
        assert_eq!(
            parse_ref("@acme/deploy@abc1234").unwrap(),
            skill(None, "acme", "deploy", Some("abc1234"))
        );
        assert_eq!(
            parse_ref("code-review@0123456789abcdef").unwrap(),
            ParsedRef::Bare {
                name: "code-review".into(),
                pin: Some("0123456789abcdef".into())
            }
        );
        match parse_ref("vercel-labs/skills@deadbeef00").unwrap() {
            ParsedRef::GitHub { pin, .. } => assert_eq!(pin.as_deref(), Some("deadbeef00")),
            other => panic!("{other:?}"),
        }
        // A channel takes no pin.
        assert!(parse_ref("@acme/channels/backend@abc1234").is_err());
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
