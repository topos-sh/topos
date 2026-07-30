//! The `[bundles]` KEY grammar — a manifest entry's TOML key path under `[bundles]`, joined
//! with `/`, IS its reference, and the joined reference's SHAPE alone decides what it names
//! (no resolution order, no fallbacks, no questions):
//!
//! | Joined key shape                      | Names                                             |
//! |---------------------------------------|---------------------------------------------------|
//! | `./x` `../x` `/abs` `~/x`             | a local folder bundle (a thing)                   |
//! | `github.com/<owner>/<repo>`           | every skill discovered in the repo (a repo set)   |
//! | `github.com/<owner>/<repo>/<skill>`   | ONE discovered skill — its leaf directory name    |
//! | `<host>/<workspace>`                  | the workspace FEED — what it currently gives you  |
//! | `<host>/<workspace>/<bundle>`         | one workspace bundle (a thing)                    |
//! | `<host>/<workspace>/channels/<name>`  | a workspace channel (a set)                       |
//!
//! `bitbucket.org` rides the same two forge shapes as `github.com`; `gitlab.com` refuses
//! plainly (not supported yet). `channels` is RESERVED in the third position — a joined
//! `<host>/<ws>/channels` can never read as a bundle named `channels`. A forge key deeper than
//! four segments refuses toward the `subdir` field (the escape hatch for literal in-repo
//! paths). Anything else refuses typed, naming the nearest correct shape.
//!
//! [`parse_input`] additionally accepts the CLI-side sugar — `@ws`, `@ws/bundle`,
//! `@ws/channels/x`, the bare `owner/repo[/skill]` shorthand, pasted `https://` URLs, and a
//! trailing `@<pin>` — and resolves it to the same shapes; manifests store only the canonical
//! joined form.

use std::fmt;

/// A classified `[bundles]` key — the joined reference, read by shape alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyShape {
    /// `./x`, `../x`, `/abs/x`, `~/x` — a local folder bundle.
    LocalPath { raw: String },
    /// `github.com/<owner>/<repo>` / `bitbucket.org/<owner>/<repo>` — all skills the repo holds.
    RepoSet {
        host: String,
        owner: String,
        repo: String,
    },
    /// `github.com/<owner>/<repo>/<skill>` — ONE skill inside the repo; the fourth segment is
    /// the discovered skill directory's LEAF name.
    RepoSkill {
        host: String,
        owner: String,
        repo: String,
        skill: String,
    },
    /// `<host>/<workspace>` — the feed: whatever that workspace currently gives you.
    Feed { host: String, workspace: String },
    /// `<host>/<workspace>/<bundle>` — one workspace bundle.
    WorkspaceBundle {
        host: String,
        workspace: String,
        bundle: String,
    },
    /// `<host>/<workspace>/channels/<name>` — a workspace channel.
    Channel {
        host: String,
        workspace: String,
        channel: String,
    },
}

impl KeyShape {
    /// The canonical joined spelling — exactly the key a manifest stores.
    pub(crate) fn canonical(&self) -> String {
        match self {
            KeyShape::LocalPath { raw } => raw.clone(),
            KeyShape::RepoSet { host, owner, repo } => format!("{host}/{owner}/{repo}"),
            KeyShape::RepoSkill {
                host,
                owner,
                repo,
                skill,
            } => format!("{host}/{owner}/{repo}/{skill}"),
            KeyShape::Feed { host, workspace } => format!("{host}/{workspace}"),
            KeyShape::WorkspaceBundle {
                host,
                workspace,
                bundle,
            } => format!("{host}/{workspace}/{bundle}"),
            KeyShape::Channel {
                host,
                workspace,
                channel,
            } => format!("{host}/{workspace}/channels/{channel}"),
        }
    }

    /// The reference's LAST meaningful segment — what a receipt calls the item.
    pub(crate) fn leaf_name(&self) -> &str {
        match self {
            KeyShape::LocalPath { raw } => {
                raw.trim_end_matches('/').rsplit('/').next().unwrap_or(raw)
            }
            KeyShape::RepoSet { repo, .. } => repo,
            KeyShape::RepoSkill { skill, .. } => skill,
            KeyShape::Feed { workspace, .. } => workspace,
            KeyShape::WorkspaceBundle { bundle, .. } => bundle,
            KeyShape::Channel { channel, .. } => channel,
        }
    }

    /// ONE deliverable item — a local folder, a repo skill, a workspace bundle. The shapes a
    /// version value fits.
    pub(crate) fn is_thing(&self) -> bool {
        matches!(
            self,
            KeyShape::LocalPath { .. }
                | KeyShape::RepoSkill { .. }
                | KeyShape::WorkspaceBundle { .. }
        )
    }

    /// A SET something else expands — the feed, a channel, a whole repo.
    pub(crate) fn is_set(&self) -> bool {
        !self.is_thing()
    }

    /// A git-forge shape (repo set or repo skill) — pins are commits, not version digests.
    pub(crate) fn is_forge(&self) -> bool {
        matches!(self, KeyShape::RepoSet { .. } | KeyShape::RepoSkill { .. })
    }

    /// The `<host>/<workspace>` prefix of a workspace-shaped reference (feed, bundle, channel).
    pub(crate) fn workspace_key(&self) -> Option<String> {
        match self {
            KeyShape::Feed { host, workspace }
            | KeyShape::WorkspaceBundle {
                host, workspace, ..
            }
            | KeyShape::Channel {
                host, workspace, ..
            } => Some(format!("{host}/{workspace}")),
            _ => None,
        }
    }

    /// The key this row takes INSIDE its workspace's `[bundles."<host>/<ws>"]` grouping — the
    /// feed has no tail (it is the workspace itself), so it always spells flat.
    pub(crate) fn section_tail(&self) -> Option<String> {
        match self {
            KeyShape::WorkspaceBundle { bundle, .. } => Some(bundle.clone()),
            KeyShape::Channel { channel, .. } => Some(format!("channels/{channel}")),
            _ => None,
        }
    }

    /// The shape's noun for error messages ("a channel takes no pin").
    pub(crate) fn noun(&self) -> &'static str {
        match self {
            KeyShape::LocalPath { .. } => "local folder",
            KeyShape::RepoSet { .. } => "repo set",
            KeyShape::RepoSkill { .. } => "repo skill",
            KeyShape::Feed { .. } => "feed",
            KeyShape::WorkspaceBundle { .. } => "workspace bundle",
            KeyShape::Channel { .. } => "channel",
        }
    }
}

/// A typed key-grammar refusal — every message names the way back, never a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyError {
    pub message: String,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

fn err(message: impl Into<String>) -> KeyError {
    KeyError {
        message: message.into(),
    }
}

/// The one NAME charset (workspace slugs, bundle names, channel names): lowercase letters,
/// digits, hyphens; must start alphanumeric. (Kept deliberately tiny and local.)
fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A forge owner/repo/skill segment (the GitHub charset: alphanumerics, hyphens, dots,
/// underscores).
fn is_forge_segment(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
}

/// A host: has a dot (`topos.sh`, `topos.example.com`) or is `localhost`, optionally with a
/// port. What distinguishes a canonical reference from anything else.
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

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The two forge hosts a key may name today.
fn is_forge_host(s: &str) -> bool {
    s == "github.com" || s == "bitbucket.org"
}

/// Classify one joined `[bundles]` key by shape. Total: a key either classifies to exactly one
/// meaning or refuses typed.
pub(crate) fn classify_key(key: &str) -> Result<KeyShape, KeyError> {
    if key.is_empty() {
        return Err(err("an empty key names nothing"));
    }
    if key.starts_with("./")
        || key.starts_with("../")
        || key.starts_with('/')
        || key.starts_with("~/")
    {
        return Ok(KeyShape::LocalPath {
            raw: key.to_string(),
        });
    }
    let segments: Vec<&str> = key.split('/').collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(err(format!(
            "`{key}` has an empty path segment — segments join with exactly one `/` each",
        )));
    }
    let first = segments[0];
    if first == "gitlab.com" {
        return Err(err(
            "gitlab.com sources are not supported yet — github.com and bitbucket.org are",
        ));
    }
    if is_forge_host(first) {
        return match segments.as_slice() {
            [host, owner, repo] if is_forge_segment(owner) && is_forge_segment(repo) => {
                Ok(KeyShape::RepoSet {
                    host: (*host).to_string(),
                    owner: (*owner).to_string(),
                    repo: (*repo).to_string(),
                })
            }
            [host, owner, repo, skill]
                if is_forge_segment(owner) && is_forge_segment(repo) && is_forge_segment(skill) =>
            {
                Ok(KeyShape::RepoSkill {
                    host: (*host).to_string(),
                    owner: (*owner).to_string(),
                    repo: (*repo).to_string(),
                    skill: (*skill).to_string(),
                })
            }
            _ if segments.len() >= 5 => Err(err(format!(
                "`{key}` is too deep for a key — a repo key is `{first}/<owner>/<repo>` (all its \
                 skills) or `{first}/<owner>/<repo>/<skill>` (one, by its leaf directory name); \
                 a literal path inside the repo rides the `subdir` field (`{{ subdir = \"…\" }}`)",
            ))),
            _ => Err(err(format!(
                "`{key}` is not a repo reference — `{first}/<owner>/<repo>` (all its skills) or \
                 `{first}/<owner>/<repo>/<skill>` (one of them)",
            ))),
        };
    }
    if is_host(first) {
        return match segments.as_slice() {
            [host, ws] if is_name(ws) => Ok(KeyShape::Feed {
                host: (*host).to_string(),
                workspace: (*ws).to_string(),
            }),
            // `channels` is reserved in the third position: never a bundle of that name.
            [host, ws, "channels"] if is_name(ws) => Err(err(format!(
                "`{key}` names no channel — a channel is `{host}/{ws}/channels/<name>`",
            ))),
            [host, ws, bundle] if is_name(ws) && is_name(bundle) => Ok(KeyShape::WorkspaceBundle {
                host: (*host).to_string(),
                workspace: (*ws).to_string(),
                bundle: (*bundle).to_string(),
            }),
            [host, ws, "channels", name] if is_name(ws) && is_name(name) => Ok(KeyShape::Channel {
                host: (*host).to_string(),
                workspace: (*ws).to_string(),
                channel: (*name).to_string(),
            }),
            [host, ws, mid, _] if is_name(ws) && *mid != "channels" => Err(err(format!(
                "`{key}` is not a reference — only `channels` sits third \
                 (`{host}/{ws}/channels/<name>`); a bundle is `{host}/{ws}/<bundle>`",
            ))),
            _ => Err(err(format!(
                "`{key}` is not a reference — `<host>/<workspace>` (the feed), \
                 `<host>/<workspace>/<bundle>`, or `<host>/<workspace>/channels/<name>`; names \
                 are lowercase letters, digits, and hyphens",
            ))),
        };
    }
    Err(err(format!(
        "`{key}` is not a manifest reference — a key is host-qualified \
         (`<host>/<workspace>[/<bundle>]`, `github.com/<owner>/<repo>[/<skill>]`) or a local \
         path (`./…`, `~/…`)",
    )))
}

/// One CLI-side input reference: the classified shape plus what the spelling carried alongside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InputRef {
    pub shape: KeyShape,
    /// A trailing `@<pin>` — a 64-hex version digest (workspace things) or a 7–40-hex commit
    /// (forge shapes).
    pub pin: Option<String>,
    /// From a pasted `github.com/<o>/<r>/tree/<ref>/<path>` URL: the literal in-repo path — the
    /// caller records it as the entry's `subdir` field.
    pub subdir_hint: Option<String>,
    /// From the same URL shape: the `<ref>` segment (a branch, tag, or commit) for the caller
    /// to resolve.
    pub ref_hint: Option<String>,
}

impl InputRef {
    fn plain(shape: KeyShape, pin: Option<String>) -> Self {
        InputRef {
            shape,
            pin,
            subdir_hint: None,
            ref_hint: None,
        }
    }
}

/// Split a trailing `@<pin>` off a spelling. Only the LAST `@` counts, and only when what
/// follows is pin-shaped (hex, 7–64 chars) — so the leading workspace sigil never collides.
fn split_pin(s: &str) -> (&str, Option<String>) {
    if let Some(at) = s.rfind('@')
        && at > 0
    {
        let candidate = &s[at + 1..];
        if (7..=64).contains(&candidate.len()) && is_hex(candidate) {
            return (&s[..at], Some(candidate.to_ascii_lowercase()));
        }
    }
    (s, None)
}

/// Validate a pin's shape for the reference it rides: a workspace bundle pins the FULL 64-hex
/// version digest; a forge shape pins a commit (7–40 hex, git's own abbreviation rules); the
/// rest take no pin at all.
fn check_pin(shape: &KeyShape, pin: Option<&str>) -> Result<(), KeyError> {
    let Some(p) = pin else { return Ok(()) };
    match shape {
        KeyShape::WorkspaceBundle { .. } => {
            if p.len() == 64 && is_hex(p) {
                Ok(())
            } else {
                Err(err(
                    "a workspace bundle pin is the full 64-character version digest",
                ))
            }
        }
        KeyShape::RepoSet { .. } | KeyShape::RepoSkill { .. } => {
            if (7..=40).contains(&p.len()) && is_hex(p) {
                Ok(())
            } else {
                Err(err("a git pin is a commit hash — 7 to 40 hex characters"))
            }
        }
        KeyShape::Feed { .. } => Err(err(
            "the feed takes no pin — it always tracks what the workspace serves",
        )),
        KeyShape::Channel { .. } => Err(err("a channel takes no version pin")),
        KeyShape::LocalPath { .. } => Err(err("a local folder takes no version pin")),
    }
}

/// Strip a `.git` tail from a forge shape's repo; refuse the repo that strips to nothing.
fn degit(shape: KeyShape) -> Result<KeyShape, KeyError> {
    let strip = |repo: String| -> Result<String, KeyError> {
        let stripped = repo.trim_end_matches(".git");
        if stripped.is_empty() {
            return Err(err(
                "a repo reference needs a repository name — `<owner>/<repo>`",
            ));
        }
        Ok(stripped.to_string())
    };
    Ok(match shape {
        KeyShape::RepoSet { host, owner, repo } => KeyShape::RepoSet {
            host,
            owner,
            repo: strip(repo)?,
        },
        KeyShape::RepoSkill {
            host,
            owner,
            repo,
            skill,
        } => KeyShape::RepoSkill {
            host,
            owner,
            repo: strip(repo)?,
            skill,
        },
        other => other,
    })
}

/// Parse ONE CLI input token into a reference: the canonical joined forms plus the sugar —
/// `@ws` (the feed at the connected host), `@ws/<bundle>`, `@ws/channels/<name>`, the bare
/// `owner/repo[/skill]` shorthand (github.com implied), pasted `https://` URLs (the
/// `/tree/<ref>/<path>` shape canonicalizes to the repo, the path and ref riding as hints),
/// and a trailing `@<pin>`.
pub(crate) fn parse_input(token: &str, default_host: Option<&str>) -> Result<InputRef, KeyError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(err("empty reference"));
    }

    // Path shape first — no pin splitting on paths (an `@` in a filename stays a filename).
    if token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with('/')
        || token.starts_with("~/")
    {
        return Ok(InputRef::plain(
            KeyShape::LocalPath {
                raw: token.to_string(),
            },
            None,
        ));
    }

    // A pasted URL: strip the scheme, then read the rest as its canonical shape.
    let token = token
        .strip_prefix("https://")
        .or_else(|| token.strip_prefix("http://"))
        .unwrap_or(token);

    // The `#` sigil is never a reference — the one channel spelling is the path segment.
    if token.starts_with('#') {
        return Err(err(
            "the `#` sigil is not a reference — a channel is `@<workspace>/channels/<name>`",
        ));
    }

    // `@workspace[/…]` — a workspace at the connected host, always. The SHAPE (and its pin
    // rules) are decided before the host resolves, so a malformed pin teaches its own rule even
    // on a machine with no connected host.
    if let Some(rest) = token.strip_prefix('@') {
        let (rest, pin) = split_pin(rest);
        let placeholder = default_host.unwrap_or_default();
        let (shape, spelled) = match rest.split('/').collect::<Vec<_>>().as_slice() {
            [ws] if is_name(ws) => (
                KeyShape::Feed {
                    host: placeholder.to_string(),
                    workspace: (*ws).to_string(),
                },
                (*ws).to_string(),
            ),
            [ws, "channels", name] if is_name(ws) && is_name(name) => (
                KeyShape::Channel {
                    host: placeholder.to_string(),
                    workspace: (*ws).to_string(),
                    channel: (*name).to_string(),
                },
                format!("{ws}/channels/{name}"),
            ),
            [ws, name] if is_name(ws) && is_name(name) => (
                KeyShape::WorkspaceBundle {
                    host: placeholder.to_string(),
                    workspace: (*ws).to_string(),
                    bundle: (*name).to_string(),
                },
                format!("{ws}/{name}"),
            ),
            _ => {
                return Err(err(format!(
                    "`@{rest}` is not a reference — `@<workspace>` (its feed), \
                     `@<workspace>/<bundle>`, or `@<workspace>/channels/<name>` (lowercase \
                     letters, digits, and hyphens)",
                )));
            }
        };
        check_pin(&shape, pin.as_deref())?;
        if default_host.is_none() {
            return Err(err(format!(
                "`@{rest}` names a workspace at your connected host, and none is connected \
                 here — spell it in full: `<host>/{spelled}`",
            )));
        }
        return Ok(InputRef::plain(shape, pin));
    }

    // No slash: not a reference this grammar owns (bare catalog names resolve elsewhere).
    if !token.contains('/') {
        return Err(err(format!(
            "`{token}` is not a reference — a workspace bundle is `@<workspace>/<bundle>`, a \
             repo is `<owner>/<repo>`, a folder is `./{token}`",
        )));
    }

    let (token, pin) = split_pin(token);
    let segments: Vec<&str> = token.split('/').filter(|s| !s.is_empty()).collect();
    let first = segments.first().copied().unwrap_or_default();

    // The pasted repo-subtree URL: `github.com/<o>/<r>/tree/<ref>[/<path>]` canonicalizes to
    // the repo; the ref and the literal path ride back as hints.
    if first == "github.com"
        && segments.len() >= 5
        && segments[3] == "tree"
        && is_forge_segment(segments[1])
        && is_forge_segment(segments[2])
    {
        let shape = degit(KeyShape::RepoSet {
            host: first.to_string(),
            owner: segments[1].to_string(),
            repo: segments[2].to_string(),
        })?;
        check_pin(&shape, pin.as_deref())?;
        let subdir = segments[5..].join("/");
        return Ok(InputRef {
            shape,
            pin,
            subdir_hint: (!subdir.is_empty()).then_some(subdir),
            ref_hint: Some(segments[4].to_string()),
        });
    }

    // Host-led: the canonical joined form (also exactly a bundle/channel page URL).
    if is_host(first) {
        let shape = degit(classify_key(&segments.join("/"))?)?;
        check_pin(&shape, pin.as_deref())?;
        return Ok(InputRef::plain(shape, pin));
    }

    // The bare forge shorthand: `owner/repo[/skill]` — github.com implied.
    let shape = match segments.as_slice() {
        [owner, repo] if is_forge_segment(owner) && is_forge_segment(repo) => {
            degit(KeyShape::RepoSet {
                host: "github.com".to_string(),
                owner: (*owner).to_string(),
                repo: (*repo).to_string(),
            })?
        }
        [owner, repo, skill]
            if is_forge_segment(owner) && is_forge_segment(repo) && is_forge_segment(skill) =>
        {
            degit(KeyShape::RepoSkill {
                host: "github.com".to_string(),
                owner: (*owner).to_string(),
                repo: (*repo).to_string(),
                skill: (*skill).to_string(),
            })?
        }
        _ => {
            return Err(err(format!(
                "`{token}` is not a reference — the repo shorthand is `<owner>/<repo>` or \
                 `<owner>/<repo>/<skill>`; a deeper literal path pins the repo and rides the \
                 `subdir` field",
            )));
        }
    };
    check_pin(&shape, pin.as_deref())?;
    Ok(InputRef::plain(shape, pin))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws_bundle(host: &str, ws: &str, bundle: &str) -> KeyShape {
        KeyShape::WorkspaceBundle {
            host: host.into(),
            workspace: ws.into(),
            bundle: bundle.into(),
        }
    }

    #[test]
    fn the_key_shape_table() {
        // Every documented key shape, row by row.
        assert_eq!(
            classify_key("./tools/release-checklist").unwrap(),
            KeyShape::LocalPath {
                raw: "./tools/release-checklist".into()
            }
        );
        assert_eq!(
            classify_key("~/dev/notes").unwrap(),
            KeyShape::LocalPath {
                raw: "~/dev/notes".into()
            }
        );
        assert_eq!(
            classify_key("github.com/vercel-labs/skills").unwrap(),
            KeyShape::RepoSet {
                host: "github.com".into(),
                owner: "vercel-labs".into(),
                repo: "skills".into()
            }
        );
        assert_eq!(
            classify_key("bitbucket.org/acme/tools").unwrap(),
            KeyShape::RepoSet {
                host: "bitbucket.org".into(),
                owner: "acme".into(),
                repo: "tools".into()
            }
        );
        assert_eq!(
            classify_key("github.com/anthropics/skills/pdf-tools").unwrap(),
            KeyShape::RepoSkill {
                host: "github.com".into(),
                owner: "anthropics".into(),
                repo: "skills".into(),
                skill: "pdf-tools".into()
            }
        );
        assert_eq!(
            classify_key("topos.sh/acme").unwrap(),
            KeyShape::Feed {
                host: "topos.sh".into(),
                workspace: "acme".into()
            }
        );
        assert_eq!(
            classify_key("localhost:3000/acme").unwrap(),
            KeyShape::Feed {
                host: "localhost:3000".into(),
                workspace: "acme".into()
            }
        );
        assert_eq!(
            classify_key("topos.sh/acme/code-review").unwrap(),
            ws_bundle("topos.sh", "acme", "code-review")
        );
        assert_eq!(
            classify_key("topos.sh/acme/channels/backend").unwrap(),
            KeyShape::Channel {
                host: "topos.sh".into(),
                workspace: "acme".into(),
                channel: "backend".into()
            }
        );
    }

    #[test]
    fn channels_is_reserved_in_the_third_position() {
        // `host/ws/channels` never classifies as a bundle named `channels` …
        let e = classify_key("topos.sh/acme/channels").unwrap_err();
        assert!(e.message.contains("channels/<name>"), "{e}");
        // … and only `channels` may sit third on a four-segment workspace key.
        let e = classify_key("topos.sh/acme/skills/deploy").unwrap_err();
        assert!(e.message.contains("only `channels` sits third"), "{e}");
        assert!(
            classify_key("topos.sh/acme/channels/backend").is_ok(),
            "the four-segment channel form stays valid"
        );
    }

    #[test]
    fn forge_depth_and_gitlab_refuse_typed() {
        // Five or more segments on a forge key point at the `subdir` escape.
        let e = classify_key("github.com/o/r/deep/path").unwrap_err();
        assert!(e.message.contains("subdir"), "{e}");
        // Too few teach the two valid forge forms.
        let e = classify_key("github.com/owner").unwrap_err();
        assert!(e.message.contains("<owner>/<repo>"), "{e}");
        // gitlab refuses plainly.
        let e = classify_key("gitlab.com/o/r").unwrap_err();
        assert!(e.message.contains("not supported yet"), "{e}");
    }

    #[test]
    fn classify_near_misses_refuse_typed() {
        assert!(classify_key("").is_err());
        // An empty segment is never silently collapsed.
        assert!(classify_key("topos.sh//acme").is_err());
        // A hostless first segment is not a manifest key (the bare shorthand is input sugar).
        assert!(classify_key("vercel-labs/skills").is_err());
        // Charset violations.
        assert!(classify_key("topos.sh/Acme/deploy").is_err());
        assert!(classify_key("topos.sh/acme/Bad_Name").is_err());
        // Five segments on a workspace host.
        assert!(classify_key("topos.sh/acme/channels/backend/extra").is_err());
    }

    #[test]
    fn the_shape_helpers_answer_by_category() {
        let feed = classify_key("topos.sh/acme").unwrap();
        let bundle = classify_key("topos.sh/acme/code-review").unwrap();
        let channel = classify_key("topos.sh/acme/channels/backend").unwrap();
        let repo = classify_key("github.com/o/r").unwrap();
        let skill = classify_key("github.com/o/r/s").unwrap();
        let local = classify_key("./tools/x").unwrap();

        for (shape, thing) in [
            (&feed, false),
            (&bundle, true),
            (&channel, false),
            (&repo, false),
            (&skill, true),
            (&local, true),
        ] {
            assert_eq!(shape.is_thing(), thing, "{shape:?}");
            assert_eq!(shape.is_set(), !thing, "{shape:?}");
        }
        assert!(repo.is_forge() && skill.is_forge() && !bundle.is_forge());

        assert_eq!(feed.leaf_name(), "acme");
        assert_eq!(bundle.leaf_name(), "code-review");
        assert_eq!(channel.leaf_name(), "backend");
        assert_eq!(repo.leaf_name(), "r");
        assert_eq!(skill.leaf_name(), "s");
        assert_eq!(local.leaf_name(), "x");

        // Canonical round-trips the joined spelling.
        for spelled in [
            "topos.sh/acme",
            "topos.sh/acme/code-review",
            "topos.sh/acme/channels/backend",
            "github.com/o/r",
            "github.com/o/r/s",
            "./tools/x",
        ] {
            assert_eq!(classify_key(spelled).unwrap().canonical(), spelled);
        }

        // The section tail: what the row is keyed by inside its workspace grouping.
        assert_eq!(bundle.section_tail().as_deref(), Some("code-review"));
        assert_eq!(channel.section_tail().as_deref(), Some("channels/backend"));
        assert_eq!(feed.section_tail(), None, "the feed always spells flat");
        assert_eq!(bundle.workspace_key().as_deref(), Some("topos.sh/acme"));
        assert_eq!(repo.workspace_key(), None);
    }

    #[test]
    fn input_sugar_resolves_at_the_default_host() {
        let host = Some("topos.sh");
        assert_eq!(
            parse_input("@acme", host).unwrap().shape,
            KeyShape::Feed {
                host: "topos.sh".into(),
                workspace: "acme".into()
            }
        );
        assert_eq!(
            parse_input("@acme/code-review", host).unwrap().shape,
            ws_bundle("topos.sh", "acme", "code-review")
        );
        assert_eq!(
            parse_input("@acme/channels/backend", host).unwrap().shape,
            KeyShape::Channel {
                host: "topos.sh".into(),
                workspace: "acme".into(),
                channel: "backend".into()
            }
        );
        // Without a connected host the sugar cannot resolve — the refusal teaches the full form.
        let e = parse_input("@acme", None).unwrap_err();
        assert!(e.message.contains("<host>/acme"), "{e}");
        let e = parse_input("@acme/deploy", None).unwrap_err();
        assert!(e.message.contains("<host>/acme/deploy"), "{e}");
        // Malformed sugar refuses typed.
        assert!(parse_input("@acme/a/b", host).is_err());
        assert!(parse_input("@Acme/deploy", host).is_err());
    }

    #[test]
    fn input_bare_shorthand_is_github() {
        assert_eq!(
            parse_input("vercel-labs/skills", None).unwrap().shape,
            KeyShape::RepoSet {
                host: "github.com".into(),
                owner: "vercel-labs".into(),
                repo: "skills".into()
            }
        );
        assert_eq!(
            parse_input("mattpocock/skills/grill-me", None)
                .unwrap()
                .shape,
            KeyShape::RepoSkill {
                host: "github.com".into(),
                owner: "mattpocock".into(),
                repo: "skills".into(),
                skill: "grill-me".into()
            }
        );
        // A `.git` tail strips; an empty repo after the strip refuses.
        match parse_input("owner/repo.git", None).unwrap().shape {
            KeyShape::RepoSet { repo, .. } => assert_eq!(repo, "repo"),
            other => panic!("{other:?}"),
        }
        assert!(parse_input("owner/.git", None).is_err());
        // Four bare segments teach the shorthand + the subdir escape.
        let e = parse_input("a/b/c/d", None).unwrap_err();
        assert!(e.message.contains("subdir"), "{e}");
    }

    #[test]
    fn input_urls_paste_cleanly() {
        assert_eq!(
            parse_input("https://topos.sh/acme/code-review", None)
                .unwrap()
                .shape,
            ws_bundle("topos.sh", "acme", "code-review")
        );
        assert_eq!(
            parse_input("https://github.com/vercel-labs/skills", None)
                .unwrap()
                .shape,
            KeyShape::RepoSet {
                host: "github.com".into(),
                owner: "vercel-labs".into(),
                repo: "skills".into()
            }
        );
        // The subtree URL canonicalizes to the repo; the ref + literal path ride as hints.
        let r = parse_input(
            "https://github.com/vercel-labs/skills/tree/main/tools/find-skills",
            None,
        )
        .unwrap();
        assert_eq!(
            r.shape,
            KeyShape::RepoSet {
                host: "github.com".into(),
                owner: "vercel-labs".into(),
                repo: "skills".into()
            }
        );
        assert_eq!(r.ref_hint.as_deref(), Some("main"));
        assert_eq!(r.subdir_hint.as_deref(), Some("tools/find-skills"));
        // A ref-only tree URL carries no subdir hint.
        let r = parse_input("https://github.com/o/r/tree/main", None).unwrap();
        assert_eq!(r.ref_hint.as_deref(), Some("main"));
        assert_eq!(r.subdir_hint, None);
    }

    #[test]
    fn input_pins_validate_by_shape() {
        let digest = "0123456789abcdef".repeat(4);
        let r = parse_input(&format!("@acme/deploy@{digest}"), Some("topos.sh")).unwrap();
        assert_eq!(r.pin.as_deref(), Some(digest.as_str()));
        let r = parse_input("vercel-labs/skills@deadbeef00", None).unwrap();
        assert_eq!(r.pin.as_deref(), Some("deadbeef00"));
        // The wrong length teaches the right rule for the shape.
        let e = parse_input("@acme/deploy@abc1234", Some("topos.sh")).unwrap_err();
        assert!(e.message.contains("64-character"), "{e}");
        let e = parse_input(&format!("vercel-labs/skills@{digest}"), None).unwrap_err();
        assert!(e.message.contains("7 to 40"), "{e}");
        // Feeds, channels, and local folders take no pin.
        assert!(parse_input(&format!("@acme@{digest}"), Some("topos.sh")).is_err());
        assert!(parse_input(&format!("@acme/channels/x@{digest}"), Some("topos.sh")).is_err());
        // The canonical spellings run the same checks.
        assert!(parse_input(&format!("topos.sh/acme/deploy@{digest}"), None).is_ok());
        assert!(parse_input("topos.sh/acme/deploy@abc1234", None).is_err());
    }
}
