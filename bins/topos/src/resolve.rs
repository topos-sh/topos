//! The ONE resolution grammar — how every verb turns an argv token into a workspace resource.
//!
//! **Parsing** ([`parse_target`]) classifies a token by SHAPE, network-free:
//! - a full ADDRESS (`https://topos.sh/acme`, `https://topos.sh/acme/channels/eng`) — scheme + host,
//!   the workspace name, and optionally a kind-scoped resource (the literal `channels`/`skills`
//!   middle segment is what makes it a resource path);
//! - a QUALIFIED path (`acme/channels/eng`, `acme/skills/deploy`) — the same three segments without
//!   a host (resolved on the enrolled plane);
//! - a BARE word (`eng`) — a workspace, channel, or skill name (resolution decides);
//! - the LOCAL domain (`deploy@cursor`) — an untracked harness-dir copy, never a plane resource;
//! - the `add` LOOKALIKE (`owner/repo` — exactly TWO segments, where a qualified path is three with
//!   a literal middle) — refused toward `topos add`, never half-resolved.
//!
//! **Resolution** matches parsed targets against the [`WorkspaceNames`] universe (the enrolled
//! workspaces' address names + channel names + catalog skills — assembled by the verb from the
//! directory reads, or from fixtures in tests). One name, one meaning: an ambiguous name is a typed
//! `AMBIGUOUS_NAME` refusal carrying PASTE-READY qualified paths (machine-readable on the envelope's
//! `data.candidates`), and a batch resolves ALL-OR-NONE — a multi-target invocation either resolves
//! every target or applies nothing. The uniform not-found ([`not_found`]) mirrors the plane's
//! deliberate non-answer: "not found, or not visible to you" — no existence oracle on either side.

use topos_types::requests::{WireChannelIndex, WireSkillIndex};

use crate::error::ClientError;

/// The kind-scoped resource segment — the literal middle of a qualified path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceKind {
    Channel,
    Skill,
}

impl ResourceKind {
    /// The literal path segment (`channels` / `skills`) — the qualified-path spelling.
    pub(crate) fn segment(self) -> &'static str {
        match self {
            ResourceKind::Channel => "channels",
            ResourceKind::Skill => "skills",
        }
    }

    /// The human noun for refusal messages.
    pub(crate) fn noun(self) -> &'static str {
        match self {
            ResourceKind::Channel => "channel",
            ResourceKind::Skill => "skill",
        }
    }
}

/// A parsed target — the SHAPE of one argv token, before any resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParsedTarget {
    /// A workspace address, optionally naming a kind-scoped resource. `host` is `Some` for a full
    /// URL (the card-fetch origin, scheme included); `None` for a bare qualified path.
    Address {
        host: Option<String>,
        workspace: String,
        resource: Option<(ResourceKind, String)>,
    },
    /// A single bare word — a workspace, channel, or skill name (resolution decides).
    Bare(String),
    /// `<name>@<agent>` — the LOCAL domain (an untracked copy in a harness dir).
    LocalAt { name: String, agent: String },
    /// Exactly two plain segments (`owner/repo`) — the `add` lookalike.
    RepoLike(String),
}

/// Parse one target token by shape. Never touches the network and never consults local state — the
/// caller layers its own precedence (a known followed skill, a pending WAL, an `/i/` link) BEFORE
/// this grammar.
///
/// # Errors
/// [`ClientError::InvalidArgument`] for a path-shaped token that is neither a qualified path
/// (`<workspace>/channels/<name>` / `<workspace>/skills/<name>`), a workspace address, nor an
/// `owner/repo` lookalike — the usage message spells the accepted shapes.
pub(crate) fn parse_target(token: &str) -> Result<ParsedTarget, ClientError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(ClientError::InvalidArgument(
            "an empty target — pass a workspace address, `<workspace>/channels/<name>`, \
             `<workspace>/skills/<name>`, or a bare name"
                .into(),
        ));
    }
    // A full URL: split the scheme+host origin off, then parse the path as a qualified shape.
    if let Some(scheme_end) = token.find("://") {
        let after = &token[scheme_end + 3..];
        let Some(slash) = after.find('/') else {
            return Err(ClientError::InvalidArgument(format!(
                "'{token}' names a server, not a workspace — a workspace address is \
                 <server>/<workspace> (e.g. https://topos.sh/acme)"
            )));
        };
        let host = &token[..scheme_end + 3 + slash];
        let path = after[slash + 1..].trim_matches('/');
        return parse_address_path(Some(host.to_owned()), path, token);
    }
    if token.contains('/') {
        let segments: Vec<&str> = token.split('/').filter(|s| !s.is_empty()).collect();
        // Exactly two plain segments read as `owner/repo` — the `add` lookalike (a qualified path is
        // THREE segments with a literal `channels`/`skills` middle, so the shapes never collide).
        if segments.len() == 2 && !matches!(segments[1], "channels" | "skills") {
            return Ok(ParsedTarget::RepoLike(token.to_owned()));
        }
        return parse_address_path(None, token.trim_matches('/'), token);
    }
    // The local domain: `<name>@<agent>`. Callers that accept `<skill>@<digest>` strip the digest
    // BEFORE parsing (a digest suffix is a version reference, not an agent).
    if let Some((name, agent)) = token.rsplit_once('@')
        && !name.is_empty()
        && !agent.is_empty()
    {
        return Ok(ParsedTarget::LocalAt {
            name: name.to_owned(),
            agent: agent.to_owned(),
        });
    }
    Ok(ParsedTarget::Bare(token.to_owned()))
}

/// Parse the path half of an address (`<ws>`, `<ws>/channels/<name>`, `<ws>/skills/<name>`).
/// `original` is the user's whole token, echoed in usage errors.
fn parse_address_path(
    host: Option<String>,
    path: &str,
    original: &str,
) -> Result<ParsedTarget, ClientError> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        [ws] => Ok(ParsedTarget::Address {
            host,
            workspace: (*ws).to_owned(),
            resource: None,
        }),
        [ws, kind, name] => {
            let kind = match *kind {
                "channels" => ResourceKind::Channel,
                "skills" => ResourceKind::Skill,
                other => {
                    return Err(ClientError::InvalidArgument(format!(
                        "'{original}' has '{other}' where a resource kind belongs — a workspace \
                         resource is <workspace>/channels/<name> or <workspace>/skills/<name>"
                    )));
                }
            };
            Ok(ParsedTarget::Address {
                host,
                workspace: (*ws).to_owned(),
                resource: Some((kind, (*name).to_owned())),
            })
        }
        _ => Err(ClientError::InvalidArgument(format!(
            "'{original}' is not a workspace address — use <workspace>, \
             <workspace>/channels/<name>, or <workspace>/skills/<name>"
        ))),
    }
}

/// Whether `s` is shaped like a workspace ADDRESS name (the plane's slug rule: lowercase alnum +
/// interior dashes, ≤ 63 chars). The follow dispatch uses this to decide whether an unresolved bare
/// word can be TREATED as a workspace to enroll toward.
pub(crate) fn is_workspace_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

// =================================================================================================
// The resolution universe + the resolvers.
// =================================================================================================

/// One workspace's known names, as the resolver sees them. Assembled per enrolled workspace from
/// the directory reads (`/me` for the address name, `/channels`, the skill catalog) — or from a
/// fixture table in tests.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkspaceNames {
    pub workspace_id: String,
    /// The workspace's ADDRESS name (`topos.sh/<name>` minus the origin).
    pub name: String,
    pub channels: Vec<String>,
    /// The catalog's skill names, each with its custody id (`(name, skill_id)`).
    pub skills: Vec<(String, String)>,
}

impl WorkspaceNames {
    /// Build a universe entry from the wire reads.
    pub(crate) fn from_wire(
        workspace_id: &str,
        name: &str,
        channels: &WireChannelIndex,
        skills: &WireSkillIndex,
    ) -> Self {
        Self {
            workspace_id: workspace_id.to_owned(),
            name: name.to_owned(),
            channels: channels.channels.iter().map(|c| c.name.clone()).collect(),
            skills: skills
                .skills
                .iter()
                .map(|s| (s.name.clone(), s.skill_id.clone()))
                .collect(),
        }
    }
}

/// A resolved target — ONE meaning for one token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// The token named a whole WORKSPACE (an enrolled one — un-enrolled addresses never reach the
    /// resolver; the verb's enroll flow owns them).
    Workspace {
        workspace_id: String,
        workspace_name: String,
    },
    /// The token named a channel or a skill.
    Resource {
        workspace_id: String,
        workspace_name: String,
        kind: ResourceKind,
        name: String,
        /// The catalog skill id, for a skill resource (channels are addressed by name).
        skill_id: Option<String>,
    },
}

impl Resolution {
    /// The workspace the resolution lives in.
    pub(crate) fn workspace_id(&self) -> &str {
        match self {
            Resolution::Workspace { workspace_id, .. }
            | Resolution::Resource { workspace_id, .. } => workspace_id,
        }
    }
}

/// Which kinds a verb accepts — its kind scope. A match OUTSIDE the scope is refused toward the
/// right spelling (never silently dropped), so a channel name handed to a skill-only selector
/// answers "that's a channel", not "not found".
#[derive(Debug, Clone, Copy)]
pub(crate) struct KindScope {
    pub workspaces: bool,
    pub channels: bool,
    pub skills: bool,
}

impl KindScope {
    /// Everything — the `follow` scope (workspaces enroll, channels join, skills follow).
    pub(crate) const ALL: KindScope = KindScope {
        workspaces: true,
        channels: true,
        skills: true,
    };
    /// Channels + skills — the subscription verbs' dual kind (`unfollow` recognizes workspaces
    /// separately, to refuse them toward the web). Consumed by the batch resolver's callers.
    #[allow(dead_code)]
    pub(crate) const SUBSCRIBABLE: KindScope = KindScope {
        workspaces: false,
        channels: true,
        skills: true,
    };
    /// Channels only (`--channel` selectors; channel curation).
    pub(crate) const CHANNELS: KindScope = KindScope {
        workspaces: false,
        channels: true,
        skills: false,
    };
    /// Skills only (`--skill` selectors).
    pub(crate) const SKILLS: KindScope = KindScope {
        workspaces: false,
        channels: false,
        skills: true,
    };
}

/// The ONE uniform not-found — the client-side spelling of the plane's deliberate non-answer.
/// `target` is the user's own token, echoed verbatim.
pub(crate) fn not_found(target: &str) -> ClientError {
    ClientError::TargetNotFound {
        target: target.to_owned(),
    }
}

/// The paste-ready qualified path for a candidate (`<workspace>/channels/<name>` /
/// `<workspace>/skills/<name>`, or the bare workspace name).
fn qualified_path(ws_name: &str, kind: Option<ResourceKind>, name: &str) -> String {
    match kind {
        Some(k) => format!("{ws_name}/{}/{name}", k.segment()),
        None => ws_name.to_owned(),
    }
}

/// Resolve ONE parsed target against the universe, within a kind scope. Returns:
/// - `Ok(Some(resolution))` — exactly one in-scope meaning;
/// - `Ok(None)` — no match anywhere (the caller decides: `follow` may treat the token as a
///   workspace address to enroll toward; other verbs answer the uniform [`not_found`]);
/// - `Err(AMBIGUOUS_NAME)` — several in-scope meanings, with the paste-ready qualified paths;
/// - `Err(INVALID_ARGUMENT)` — an out-of-scope match (a kind mismatch, named toward the right
///   spelling), the local domain, or the `owner/repo` lookalike (refused toward `topos add`).
///
/// # Errors
/// As above.
pub(crate) fn resolve_one(
    universe: &[WorkspaceNames],
    target: &ParsedTarget,
    scope: KindScope,
) -> Result<Option<Resolution>, ClientError> {
    match target {
        ParsedTarget::RepoLike(token) => Err(ClientError::InvalidArgument(format!(
            "'{token}' looks like a repository (owner/repo) — import it with `topos add {token}`; \
             a workspace resource is <workspace>/channels/<name> or <workspace>/skills/<name>"
        ))),
        ParsedTarget::LocalAt { name, agent } => Err(ClientError::InvalidArgument(format!(
            "'{name}@{agent}' names a local copy in an agent's skill directory — that domain \
             belongs to `topos add` / `topos remove`, not a workspace resource"
        ))),
        ParsedTarget::Address {
            workspace,
            resource,
            ..
        } => {
            // The workspace must be enrolled to resolve here (the verb's enroll flow owns the
            // un-enrolled address BEFORE calling the resolver).
            let Some(ws) = universe.iter().find(|w| w.name == *workspace) else {
                return Ok(None);
            };
            match resource {
                None => {
                    if !scope.workspaces {
                        return Ok(None);
                    }
                    Ok(Some(Resolution::Workspace {
                        workspace_id: ws.workspace_id.clone(),
                        workspace_name: ws.name.clone(),
                    }))
                }
                Some((kind, name)) => {
                    // The qualified path names its kind explicitly — an out-of-scope kind is a
                    // mismatch refusal, an unknown name the uniform not-found (Ok(None)).
                    let in_scope = match kind {
                        ResourceKind::Channel => scope.channels,
                        ResourceKind::Skill => scope.skills,
                    };
                    if !in_scope {
                        return Err(kind_mismatch(*kind, name));
                    }
                    Ok(lookup(ws, *kind, name))
                }
            }
        }
        ParsedTarget::Bare(name) => resolve_bare(universe, name, scope),
    }
}

/// The kind-mismatch refusal — the target EXISTS but the verb's scope excludes its kind; name what
/// it is and the spelling that acts on it.
fn kind_mismatch(kind: ResourceKind, name: &str) -> ClientError {
    ClientError::InvalidArgument(format!(
        "'{name}' is a {noun} — this form does not take a {noun}; target it with `--{noun} \
         {name}` (or the {seg}/ qualified path) on a verb that does",
        noun = kind.noun(),
        seg = kind.segment(),
    ))
}

/// Look one kind-scoped name up in one workspace.
fn lookup(ws: &WorkspaceNames, kind: ResourceKind, name: &str) -> Option<Resolution> {
    let found = match kind {
        ResourceKind::Channel => ws.channels.iter().any(|c| c == name).then_some(None),
        ResourceKind::Skill => ws
            .skills
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, id)| Some(id.clone())),
    }?;
    Some(Resolution::Resource {
        workspace_id: ws.workspace_id.clone(),
        workspace_name: ws.name.clone(),
        kind,
        name: name.to_owned(),
        skill_id: found,
    })
}

/// Resolve a BARE word: collect every in-scope meaning across the universe (workspace names,
/// channels, skills), demand exactly one, and refuse several with paste-ready qualified paths. When
/// nothing matches in scope but something matches OUT of scope, the refusal names the kind mismatch
/// (never a false not-found).
fn resolve_bare(
    universe: &[WorkspaceNames],
    name: &str,
    scope: KindScope,
) -> Result<Option<Resolution>, ClientError> {
    let mut matches: Vec<(Resolution, String)> = Vec::new();
    let mut out_of_scope: Option<ResourceKind> = None;
    for ws in universe {
        if ws.name == name && scope.workspaces {
            matches.push((
                Resolution::Workspace {
                    workspace_id: ws.workspace_id.clone(),
                    workspace_name: ws.name.clone(),
                },
                qualified_path(&ws.name, None, name),
            ));
        }
        if ws.channels.iter().any(|c| c == name) {
            if scope.channels {
                matches.push((
                    Resolution::Resource {
                        workspace_id: ws.workspace_id.clone(),
                        workspace_name: ws.name.clone(),
                        kind: ResourceKind::Channel,
                        name: name.to_owned(),
                        skill_id: None,
                    },
                    qualified_path(&ws.name, Some(ResourceKind::Channel), name),
                ));
            } else {
                out_of_scope = Some(ResourceKind::Channel);
            }
        }
        if let Some((_, id)) = ws.skills.iter().find(|(n, _)| n == name) {
            if scope.skills {
                matches.push((
                    Resolution::Resource {
                        workspace_id: ws.workspace_id.clone(),
                        workspace_name: ws.name.clone(),
                        kind: ResourceKind::Skill,
                        name: name.to_owned(),
                        skill_id: Some(id.clone()),
                    },
                    qualified_path(&ws.name, Some(ResourceKind::Skill), name),
                ));
            } else {
                out_of_scope = Some(ResourceKind::Skill);
            }
        }
    }
    match matches.len() {
        0 => match out_of_scope {
            // Nothing in scope, but the name EXISTS as another kind — refuse toward the right verb.
            Some(kind) => Err(kind_mismatch(kind, name)),
            None => Ok(None),
        },
        1 => Ok(Some(matches.remove(0).0)),
        _ => Err(ClientError::AmbiguousTarget {
            name: name.to_owned(),
            candidates: matches.into_iter().map(|(_, path)| path).collect(),
        }),
    }
}

/// One target of a multi-target batch: the positional token as typed (for errors + argv rebuilds)
/// and the kind its selector forces (`--channel` / `--skill`), or `None` for a free positional.
#[derive(Debug, Clone)]
pub(crate) struct TargetSpec {
    pub token: String,
    pub forced: Option<ResourceKind>,
}

impl TargetSpec {
    pub(crate) fn free(token: &str) -> Self {
        Self {
            token: token.to_owned(),
            forced: None,
        }
    }
    pub(crate) fn kinded(token: &str, kind: ResourceKind) -> Self {
        Self {
            token: token.to_owned(),
            forced: Some(kind),
        }
    }
}

/// Resolve a whole batch ALL-OR-NONE: every target resolves, or the FIRST failure aborts the batch
/// and nothing is applied. A selector-forced target narrows the scope to its kind; an unresolved
/// target is the uniform [`not_found`] here (verbs that fold an enroll flow in — `follow` — run
/// their single-target dispatch BEFORE this batch resolver).
///
/// # Errors
/// The first target's resolution error, or [`not_found`] for the first unresolved one.
#[allow(dead_code)] // The batch entry point for the multi-target verbs (remove / channel add / protect).
pub(crate) fn resolve_all(
    universe: &[WorkspaceNames],
    specs: &[TargetSpec],
    scope: KindScope,
) -> Result<Vec<Resolution>, ClientError> {
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        let narrowed = match spec.forced {
            Some(ResourceKind::Channel) => KindScope {
                workspaces: false,
                skills: false,
                ..scope
            },
            Some(ResourceKind::Skill) => KindScope {
                workspaces: false,
                channels: false,
                ..scope
            },
            None => scope,
        };
        // A selector value is a NAME (optionally qualified); a URL there resolves like any address.
        let parsed = parse_target(&spec.token)?;
        match resolve_one(universe, &parsed, narrowed)? {
            Some(r) => out.push(r),
            None => return Err(not_found(&spec.token)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn universe() -> Vec<WorkspaceNames> {
        vec![
            WorkspaceNames {
                workspace_id: "w_acme".into(),
                name: "acme".into(),
                channels: vec!["everyone".into(), "eng".into(), "release".into()],
                skills: vec![
                    ("deploy".into(), "s_deploy".into()),
                    ("docs".into(), "s_docs".into()),
                    // A skill sharing a channel's name INSIDE one workspace (kind collision).
                    ("release".into(), "s_release".into()),
                ],
            },
            WorkspaceNames {
                workspace_id: "w_beta".into(),
                name: "beta".into(),
                channels: vec!["everyone".into(), "design".into()],
                skills: vec![("deploy".into(), "s_deploy_beta".into())],
            },
        ]
    }

    #[test]
    fn parse_classifies_the_target_shapes() {
        // A full address, workspace-only and resource-scoped.
        assert_eq!(
            parse_target("https://topos.sh/acme").unwrap(),
            ParsedTarget::Address {
                host: Some("https://topos.sh".into()),
                workspace: "acme".into(),
                resource: None,
            }
        );
        assert_eq!(
            parse_target("https://topos.sh/acme/channels/eng").unwrap(),
            ParsedTarget::Address {
                host: Some("https://topos.sh".into()),
                workspace: "acme".into(),
                resource: Some((ResourceKind::Channel, "eng".into())),
            }
        );
        // A qualified path (no host) — three segments with the literal middle.
        assert_eq!(
            parse_target("acme/skills/deploy").unwrap(),
            ParsedTarget::Address {
                host: None,
                workspace: "acme".into(),
                resource: Some((ResourceKind::Skill, "deploy".into())),
            }
        );
        // TWO plain segments are the `add` lookalike — owner/repo, never half a qualified path.
        assert_eq!(
            parse_target("vercel-labs/agent-skills").unwrap(),
            ParsedTarget::RepoLike("vercel-labs/agent-skills".into())
        );
        // The local domain and the bare word.
        assert_eq!(
            parse_target("deploy@cursor").unwrap(),
            ParsedTarget::LocalAt {
                name: "deploy".into(),
                agent: "cursor".into(),
            }
        );
        assert_eq!(
            parse_target("eng").unwrap(),
            ParsedTarget::Bare("eng".into())
        );
        // A three-segment path WITHOUT the literal middle is a usage error, not a guess.
        assert!(parse_target("acme/things/eng").is_err());
        // A URL with no path names a server, not a workspace.
        assert!(parse_target("https://topos.sh").is_err());
        // `owner/channels` (two segments where the SECOND is a kind literal) is not a repo — it is
        // a malformed qualified path, refused with the usage shapes.
        assert!(parse_target("acme/channels").is_err());
    }

    #[test]
    fn workspace_name_shape_gate() {
        assert!(is_workspace_name("acme"));
        assert!(is_workspace_name("acme-2"));
        assert!(is_workspace_name("0day"));
        assert!(!is_workspace_name("-acme"));
        assert!(!is_workspace_name("Acme"));
        assert!(!is_workspace_name(""));
        assert!(!is_workspace_name(&"a".repeat(64)));
    }

    #[test]
    fn bare_names_resolve_uniquely_in_scope() {
        let u = universe();
        // A channel unique across the universe.
        let r = resolve_one(&u, &ParsedTarget::Bare("eng".into()), KindScope::ALL)
            .unwrap()
            .expect("eng resolves");
        assert!(matches!(
            r,
            Resolution::Resource { kind: ResourceKind::Channel, ref workspace_id, .. }
                if workspace_id == "w_acme"
        ));
        // A skill unique to one workspace.
        let r = resolve_one(&u, &ParsedTarget::Bare("docs".into()), KindScope::ALL)
            .unwrap()
            .expect("docs resolves");
        assert!(matches!(
            r,
            Resolution::Resource { kind: ResourceKind::Skill, ref skill_id, .. }
                if skill_id.as_deref() == Some("s_docs")
        ));
        // A workspace name resolves as a workspace (follow's enroll-less join).
        let r = resolve_one(&u, &ParsedTarget::Bare("beta".into()), KindScope::ALL)
            .unwrap()
            .expect("beta resolves");
        assert!(
            matches!(r, Resolution::Workspace { ref workspace_id, .. } if workspace_id == "w_beta")
        );
        // Nothing anywhere → Ok(None) (the caller maps its own not-found / enroll flow).
        assert!(
            resolve_one(&u, &ParsedTarget::Bare("nope".into()), KindScope::ALL)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cross_workspace_collision_is_ambiguous_with_paste_ready_paths() {
        let u = universe();
        // "deploy" is a skill in BOTH workspaces → typed AMBIGUOUS_NAME with qualified candidates.
        let err =
            resolve_one(&u, &ParsedTarget::Bare("deploy".into()), KindScope::ALL).unwrap_err();
        let ClientError::AmbiguousTarget { name, candidates } = err else {
            panic!("expected AmbiguousTarget");
        };
        assert_eq!(name, "deploy");
        assert_eq!(
            candidates,
            vec![
                "acme/skills/deploy".to_owned(),
                "beta/skills/deploy".to_owned()
            ]
        );
        // The wire code is the shared AMBIGUOUS_NAME.
        assert_eq!(
            ClientError::AmbiguousTarget {
                name,
                candidates: Vec::new()
            }
            .code(),
            "AMBIGUOUS_NAME"
        );
    }

    #[test]
    fn kind_collision_inside_one_workspace_is_ambiguous_across_kinds() {
        let u = universe();
        // "release" is a channel AND a skill in acme → two candidates, both paths spelled.
        let err =
            resolve_one(&u, &ParsedTarget::Bare("release".into()), KindScope::ALL).unwrap_err();
        let ClientError::AmbiguousTarget { candidates, .. } = err else {
            panic!("expected AmbiguousTarget");
        };
        assert_eq!(
            candidates,
            vec![
                "acme/channels/release".to_owned(),
                "acme/skills/release".to_owned()
            ]
        );
        // A kind-forced scope disambiguates the same name.
        let r = resolve_one(
            &u,
            &ParsedTarget::Bare("release".into()),
            KindScope::CHANNELS,
        )
        .unwrap()
        .expect("channel-scoped release resolves");
        assert!(matches!(
            r,
            Resolution::Resource {
                kind: ResourceKind::Channel,
                ..
            }
        ));
    }

    #[test]
    fn kind_mismatch_is_refused_toward_the_right_spelling_never_not_found() {
        let u = universe();
        // A skill name under a channel-only scope: the refusal SAYS it is a skill.
        let err =
            resolve_one(&u, &ParsedTarget::Bare("docs".into()), KindScope::CHANNELS).unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
        assert!(err.to_string().contains("is a skill"), "{err}");
        // A qualified path whose kind the scope excludes refuses the same way.
        let err = resolve_one(
            &u,
            &parse_target("acme/channels/eng").unwrap(),
            KindScope::SKILLS,
        )
        .unwrap_err();
        assert!(err.to_string().contains("is a channel"), "{err}");
    }

    #[test]
    fn qualified_paths_resolve_exactly_and_miss_uniformly() {
        let u = universe();
        let r = resolve_one(
            &u,
            &parse_target("beta/skills/deploy").unwrap(),
            KindScope::ALL,
        )
        .unwrap()
        .expect("qualified deploy resolves");
        assert!(matches!(
            r,
            Resolution::Resource { ref skill_id, .. } if skill_id.as_deref() == Some("s_deploy_beta")
        ));
        // An unknown name under a known workspace → Ok(None): ONE uniform not-found downstream.
        assert!(
            resolve_one(
                &u,
                &parse_target("acme/skills/nope").unwrap(),
                KindScope::ALL
            )
            .unwrap()
            .is_none()
        );
        // An unknown WORKSPACE → Ok(None) too (follow treats it as an address to enroll toward).
        assert!(
            resolve_one(
                &u,
                &parse_target("ghost/skills/deploy").unwrap(),
                KindScope::ALL
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn repo_lookalike_and_local_domain_are_refused_typed() {
        let u = universe();
        let err = resolve_one(
            &u,
            &ParsedTarget::RepoLike("owner/repo".into()),
            KindScope::ALL,
        )
        .unwrap_err();
        assert!(err.to_string().contains("topos add owner/repo"), "{err}");
        let err = resolve_one(
            &u,
            &ParsedTarget::LocalAt {
                name: "deploy".into(),
                agent: "cursor".into(),
            },
            KindScope::ALL,
        )
        .unwrap_err();
        assert!(err.to_string().contains("local copy"), "{err}");
    }

    #[test]
    fn resolve_all_is_all_or_none() {
        let u = universe();
        // A batch with one unresolvable target fails WHOLE — nothing to apply.
        let specs = vec![TargetSpec::free("eng"), TargetSpec::free("nope")];
        let err = resolve_all(&u, &specs, KindScope::SUBSCRIBABLE).unwrap_err();
        assert_eq!(err.code(), "NOT_FOUND");
        assert!(
            err.to_string().contains("not found, or is not visible"),
            "{err}"
        );
        // A fully-resolvable batch answers every target, selectors narrowing per spec.
        let specs = vec![
            TargetSpec::kinded("release", ResourceKind::Channel),
            TargetSpec::kinded("docs", ResourceKind::Skill),
        ];
        let out = resolve_all(&u, &specs, KindScope::SUBSCRIBABLE).unwrap();
        assert_eq!(out.len(), 2);
        assert!(matches!(
            out[0],
            Resolution::Resource {
                kind: ResourceKind::Channel,
                ..
            }
        ));
        assert!(matches!(
            out[1],
            Resolution::Resource {
                kind: ResourceKind::Skill,
                ..
            }
        ));
    }

    #[test]
    fn the_uniform_not_found_spelling_is_the_one_helper() {
        let err = not_found("acme/skills/ghost");
        assert_eq!(err.code(), "NOT_FOUND");
        assert_eq!(
            err.to_string(),
            "'acme/skills/ghost' was not found, or is not visible to you — check the address; if \
             you were invited, confirm with your inviter"
        );
    }
}
