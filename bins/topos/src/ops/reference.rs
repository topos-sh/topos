//! Workspace-reference `add` / `remove` — the demand-side edits for CONNECTED content:
//! `add @acme/code-review` (or the canonical `host/acme/code-review`, or a bare catalog name
//! unique across the connected workspaces) records the reference in the NEAREST `topos.toml`
//! and delivers it immediately; `-g` edits the SERVER-STORED profile of the workspace the
//! reference resolves to instead (the person's set, on every machine they log in). `remove -g`
//! is the inverse — the server records an EXCLUDE line when a broader layer (a channel, the
//! baseline) still provides the bundle, and the receipt names which happened.
//!
//! Resolution is SHAPE-DETERMINED (the reference grammar): a spelled host/workspace resolves
//! through exactly that session (not logged in = a typed local line naming `topos login`); a
//! bare name must be unique across the connected catalogs or the refusal lists every candidate.

use topos_types::results::{AddData, RemoveData, RemoveItem, RemoveKind};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::manifest::refs::ParsedRef;
use crate::plane::ProfileRemoval;
use crate::sessions::{self, SESSION_ENDED, Session};

use super::reconcile::{SessionConnect, SessionTransports};

/// What a workspace reference resolved to, through which session.
struct ResolvedRef {
    session: Session,
    transports: SessionTransports,
    /// The canonical host-qualified spelling the manifest stores (pin-free).
    canonical: String,
    kind: ResolvedKind,
    pin: Option<String>,
}

enum ResolvedKind {
    Skill(topos_types::requests::WireSkillIndexEntry),
    Channel(String),
}

/// The live sessions (never ended).
fn live_sessions(ctx: &Ctx<'_>) -> Result<Vec<Session>, ClientError> {
    Ok(sessions::read_sessions(ctx.fs, &ctx.layout)?
        .sessions
        .into_iter()
        .filter(|s| s.status != SESSION_ENDED)
        .collect())
}

/// Resolve one parsed workspace reference (Skill / Channel / Bare) through the sessions.
fn resolve_ref(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    parsed: &ParsedRef,
) -> Result<ResolvedRef, ClientError> {
    let live = live_sessions(ctx)?;
    let by_address = |host: Option<&str>, ws: &str| -> Result<Session, ClientError> {
        let hits: Vec<&Session> = live
            .iter()
            .filter(|s| host.is_none_or(|h| s.host == h) && s.workspace_name == ws)
            .collect();
        match hits.as_slice() {
            [] => {
                let address = match host {
                    Some(h) => format!("{h}/{ws}"),
                    None => ws.to_owned(),
                };
                Err(ClientError::Enrollment(format!(
                    "not logged into {address} — run `topos login {address}` first"
                )))
            }
            [one] => Ok((*one).clone()),
            several => Err(ClientError::AmbiguousName {
                name: several
                    .iter()
                    .map(|s| format!("{}/{}", s.host, s.workspace_name))
                    .collect::<Vec<_>>()
                    .join(", "),
                count: several.len(),
            }),
        }
    };
    match parsed {
        ParsedRef::Skill {
            host,
            workspace,
            name,
            pin,
        } => {
            let session = by_address(host.as_deref(), workspace)?;
            let transports = connect(&session);
            let catalog = transports
                .directory
                .skills_index(&session.workspace_id)
                .map_err(|e| not_available(&session, name, &e))?;
            let entry = catalog
                .skills
                .iter()
                .find(|e| &e.name == name)
                .cloned()
                .ok_or_else(|| ClientError::TargetNotFound {
                    target: format!(
                        "'{name}' — not in {}'s catalog, or not visible with your current \
                             access",
                        session.workspace_name
                    ),
                })?;
            Ok(ResolvedRef {
                canonical: format!("{}/{}/{name}", session.host, session.workspace_name),
                session,
                transports,
                kind: ResolvedKind::Skill(entry),
                pin: pin.clone(),
            })
        }
        ParsedRef::Channel {
            host,
            workspace,
            name,
        } => {
            let session = by_address(host.as_deref(), workspace)?;
            let transports = connect(&session);
            let channels = transports
                .directory
                .channels_index(&session.workspace_id)
                .map_err(|e| not_available(&session, name, &e))?;
            if !channels.channels.iter().any(|c| &c.name == name) {
                return Err(ClientError::TargetNotFound {
                    target: format!(
                        "'{name}' — no such channel in {}, or not visible with your current access",
                        session.workspace_name
                    ),
                });
            }
            Ok(ResolvedRef {
                canonical: format!(
                    "{}/{}/channels/{name}",
                    session.host, session.workspace_name
                ),
                session,
                transports,
                kind: ResolvedKind::Channel(name.clone()),
                pin: None,
            })
        }
        ParsedRef::Bare { name, pin } => {
            // Unique across the connected catalogs, or an error listing every candidate.
            let mut hits: Vec<(
                Session,
                SessionTransports,
                topos_types::requests::WireSkillIndexEntry,
            )> = Vec::new();
            for s in &live {
                let transports = connect(s);
                if let Ok(catalog) = transports.directory.skills_index(&s.workspace_id)
                    && let Some(e) = catalog.skills.iter().find(|e| &e.name == name)
                {
                    hits.push((s.clone(), transports, e.clone()));
                }
            }
            match hits.len() {
                0 => {
                    if live.is_empty() {
                        Err(ClientError::Enrollment(format!(
                            "'{name}' is a catalog reference, but this installation is not \
                             logged into any workspace — `topos login <workspace-address>` first"
                        )))
                    } else {
                        Err(ClientError::TargetNotFound {
                            target: format!(
                                "'{name}' — not in any connected workspace's catalog, or not \
                                 visible with your current access"
                            ),
                        })
                    }
                }
                1 => {
                    let (session, transports, entry) = hits.remove(0);
                    Ok(ResolvedRef {
                        canonical: format!("{}/{}/{name}", session.host, session.workspace_name),
                        session,
                        transports,
                        kind: ResolvedKind::Skill(entry),
                        pin: pin.clone(),
                    })
                }
                n => {
                    let candidates: Vec<String> = hits
                        .iter()
                        .map(|(s, _, _)| format!("{}/{}/{name}", s.host, s.workspace_name))
                        .collect();
                    Err(ClientError::AmbiguousName {
                        name: candidates.join(", "),
                        count: n,
                    })
                }
            }
        }
        ParsedRef::GitHub { .. } | ParsedRef::LocalPath { .. } => Err(ClientError::Corrupt(
            "resolve_ref takes workspace references only".into(),
        )),
    }
}

/// The honest transport-failure line (never an existence claim).
fn not_available(session: &Session, name: &str, e: &ClientError) -> ClientError {
    ClientError::Plane(format!(
        "could not read {}'s catalog for '{name}': {}",
        session.workspace_name,
        crate::render::safe_message(e)
    ))
}

/// A resolved SESSION write lane — which workspace a governance/write verb acts in, with the
/// transports built under that session's OWN credential.
pub(crate) struct WriteLane {
    pub host: String,
    pub workspace_name: String,
    pub workspace_id: String,
    pub base_url: String,
    pub transports: SessionTransports,
}

/// Resolve the session a write verb signs in: `None` when this installation runs on the legacy
/// enrollment (no sessions — the caller keeps its classic path). A DELIVERED skill's own
/// workspace wins (the pointer scope must be the skill's, never an ambient guess); else the one
/// live session, or the `--workspace`-selected one (several without a name refuse typed).
///
/// # Errors
/// [`ClientError::Enrollment`] / [`ClientError::WorkspaceSelection`] from the session selection.
pub(crate) fn resolve_session_lane(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    explicit: Option<&str>,
    skill_id: Option<&str>,
) -> Result<Option<WriteLane>, ClientError> {
    let all = sessions::read_sessions(ctx.fs, &ctx.layout)?;
    if all.sessions.is_empty() {
        return Ok(None);
    }
    let delivered_ws = skill_id.and_then(|sid| {
        use crate::plane::FollowSource;
        super::reconcile::CacheFollow::load(ctx.fs, &ctx.layout)
            .followed()
            .into_iter()
            .find(|(id, _)| id == sid)
            .map(|(_, fc)| fc.workspace_id)
    });
    let session = match delivered_ws {
        // LIVE sessions only — an ended row must refuse toward `login`, never lend its dead
        // credential to a write (the non-delivered arm's resolve_target already filters).
        Some(ws) => all
            .sessions
            .iter()
            .find(|s| s.workspace_id == ws && s.status != sessions::SESSION_ENDED)
            .cloned()
            .ok_or_else(|| {
                ClientError::Enrollment(format!(
                    "this bundle's workspace has no live session on this installation — `topos \
                     login` it first (workspace id {ws})"
                ))
            })?,
        None => all.resolve_target(explicit)?.clone(),
    };
    let transports = connect(&session);
    Ok(Some(WriteLane {
        host: session.host.clone(),
        workspace_name: session.workspace_name.clone(),
        workspace_id: session.workspace_id.clone(),
        base_url: session.base_url.clone(),
        transports,
    }))
}

/// The governance-transfer rewrite a LANDED publish performs by default: the nearest manifest
/// line whose LOCAL PATH resolves to one of THIS skill's placement dirs is rewritten to the
/// canonical workspace reference — the local copy becomes a managed placement of the governed
/// bundle, one act. The match is by RESOLVED PATH, never by name (two dirs may share a basename;
/// the wrong line must never be rewritten — no path match, no rewrite). An already-present
/// canonical entry is kept untouched (its pin survives); only the path line is removed then.
/// Returns the facts for the receipt (`None` when no path line referenced it — an
/// already-governed republish, or a skill never recorded in a manifest).
pub(crate) struct GovernedRewrite {
    pub manifest: String,
    pub canonical: String,
    pub from: String,
}

pub(crate) fn rewrite_to_governed(
    ctx: &Ctx<'_>,
    skill_name: &str,
    host: &str,
    workspace_name: &str,
    skill_dirs: &[std::path::PathBuf],
) -> Result<Option<GovernedRewrite>, ClientError> {
    // Canonicalize the placement dirs once (macOS `/var` → `/private/var`); a dir that no longer
    // exists keeps its lexical form.
    let dirs: Vec<std::path::PathBuf> = skill_dirs
        .iter()
        .map(|d| d.canonicalize().unwrap_or_else(|_| d.clone()))
        .collect();
    let canonical = format!("{host}/{workspace_name}/{skill_name}");
    for (path, manifest) in super::manifest_edit::local_layers(ctx)? {
        let manifest_dir = path.parent().map(std::path::Path::to_path_buf);
        let Some(entry) = manifest.skills.iter().find(|e| {
            if !matches!(
                crate::manifest::refs::parse_ref(&e.reference),
                Ok(ParsedRef::LocalPath { .. })
            ) {
                return false;
            }
            let raw = std::path::Path::new(&e.reference);
            let resolved = if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                match &manifest_dir {
                    Some(dir) => dir.join(e.reference.trim_start_matches("./")),
                    None => return false,
                }
            };
            let resolved = resolved.canonicalize().unwrap_or(resolved);
            dirs.contains(&resolved)
        }) else {
            continue;
        };
        let from = entry.reference.clone();
        let already_governed = manifest.skills.iter().any(|e| e.reference == canonical);
        let mut ed = crate::manifest::file::ManifestEditor::open(ctx.fs, &path)?;
        ed.remove_entry("skills", &from);
        if !already_governed {
            ed.set_entry("skills", &canonical, None);
        }
        ed.write(ctx.fs, &path)?;
        return Ok(Some(GovernedRewrite {
            manifest: path.display().to_string(),
            canonical,
            from,
        }));
    }
    Ok(None)
}

/// `topos add <workspace-ref>` — record the demand and deliver it now. `global` routes the edit
/// to the SERVER-STORED PROFILE of the workspace the reference resolves to (`-g`); otherwise the
/// NEAREST `topos.toml` takes the line (created at the git root when none is in reach). Either
/// way the targeted reconcile runs immediately — `add` chooses, the same sweep delivers.
///
/// # Errors
/// [`ClientError::Enrollment`] with no matching session; [`ClientError::TargetNotFound`] /
/// [`ClientError::AmbiguousName`] from resolution; a manifest/profile write failure.
pub(crate) fn add_reference(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    git: Option<&dyn crate::git_source::GitTarballSource>,
    raw: &str,
    global: bool,
) -> Result<AddData, ClientError> {
    let parsed = crate::manifest::refs::parse_ref(raw)
        .map_err(|e| ClientError::InvalidArgument(e.message))?;
    let resolved = resolve_ref(ctx, connect, &parsed)?;
    let (skill_id, name, version_id, bundle_digest) = match &resolved.kind {
        ResolvedKind::Skill(e) => (
            e.skill_id.clone(),
            e.name.clone(),
            e.version_id.clone(),
            e.bundle_digest.clone(),
        ),
        ResolvedKind::Channel(name) => {
            (String::new(), name.clone(), "0".repeat(64), "0".repeat(64))
        }
    };
    let mut data = AddData {
        skill_id,
        name: name.clone(),
        version_id,
        bundle_digest,
        tracked: true,
        harness: None,
        harness_slug: None,
        currency: None,
        triggers: Vec::new(),
        origin: None,
        manifest: None,
        reference: None,
        undo: Vec::new(),
    };
    if global {
        // The profile edit — the person's set, on every machine they log in.
        match &resolved.kind {
            ResolvedKind::Skill(e) => resolved.transports.directory.profile_include_skill(
                &resolved.session.workspace_id,
                &e.skill_id,
                resolved.pin.as_deref(),
            )?,
            ResolvedKind::Channel(ch) => resolved
                .transports
                .directory
                .profile_include_channel(&resolved.session.workspace_id, ch)?,
        }
        data.manifest = Some(format!(
            "your profile @ {}/{}",
            resolved.session.host, resolved.session.workspace_name
        ));
        data.reference = Some(resolved.canonical.clone());
        data.undo = vec![
            "topos".to_owned(),
            "remove".to_owned(),
            "-g".to_owned(),
            resolved.canonical.clone(),
        ];
    } else {
        let table = match &resolved.kind {
            ResolvedKind::Skill(_) => "skills",
            ResolvedKind::Channel(_) => "channels",
        };
        super::manifest_edit::note_added_table(
            ctx,
            &mut data,
            table,
            &resolved.canonical,
            resolved.pin.as_deref(),
            false,
        )?;
    }
    // Deliver NOW — the same reconcile the sweep runs, targeted at this name. Best-effort: the
    // demand is durably recorded above; a delivery hiccup surfaces on the next sweep too.
    let _ = super::reconcile::manifest_update(
        ctx,
        connect,
        git,
        &super::reconcile::ManifestUpdateOpts {
            targets: vec![name],
            ack_notices: false,
        },
    );
    Ok(data)
}

/// `topos remove -g <ref>` — the profile-side inverse: the server removes the include line, or
/// records the EXCLUDE line when a broader layer still provides the bundle; the receipt names
/// which happened. The follow-up sweep cleans the person-scope placements the drop ends.
///
/// # Errors
/// As [`add_reference`].
pub(crate) fn remove_reference_global(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    raw: &str,
) -> Result<RemoveData, ClientError> {
    let parsed = crate::manifest::refs::parse_ref(raw)
        .map_err(|e| ClientError::InvalidArgument(e.message))?;
    let resolved = resolve_ref(ctx, connect, &parsed)?;
    let (name, removal) = match &resolved.kind {
        ResolvedKind::Skill(e) => (
            e.name.clone(),
            resolved
                .transports
                .directory
                .profile_remove_skill(&resolved.session.workspace_id, &e.skill_id)?,
        ),
        ResolvedKind::Channel(ch) => (
            ch.clone(),
            resolved
                .transports
                .directory
                .profile_remove_channel(&resolved.session.workspace_id, ch)?,
        ),
    };
    let (kind, note) = match removal {
        ProfileRemoval::Removed => (RemoveKind::ManifestRemoved, None),
        ProfileRemoval::Excluded => (
            RemoveKind::ManifestExcluded,
            Some(
                "a broader layer still provides it — your profile now carries an exclude line"
                    .to_owned(),
            ),
        ),
        ProfileRemoval::NotInProfile => (
            RemoveKind::ManifestExcluded,
            Some("it was not in your profile — an exclude line now withholds it".to_owned()),
        ),
    };
    // The sweep cleans what the drop ended (person-scope placements) — best-effort.
    let _ = super::reconcile::manifest_update(
        ctx,
        connect,
        None,
        &super::reconcile::ManifestUpdateOpts::default(),
    );
    Ok(RemoveData {
        items: vec![RemoveItem {
            name,
            kind,
            manifest: Some(format!(
                "your profile @ {}/{}",
                resolved.session.host, resolved.session.workspace_name
            )),
            workspace_id: Some(resolved.session.workspace_id.clone()),
            agent_dirs: Vec::new(),
            bytes_kept: true,
            note,
        }],
        applied: true,
        undo: vec![
            "topos".to_owned(),
            "add".to_owned(),
            "-g".to_owned(),
            resolved.canonical,
        ],
    })
}
