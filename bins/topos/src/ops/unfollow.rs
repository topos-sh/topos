//! `unfollow` — stop following a channel or skill.
//!
//! A SKILL unfollow applies immediately and prints an undo-led receipt (`topos follow <skill>`
//! re-attaches; local copies stay frozen — the act is self-scoped-reversible, and `--yes` is an
//! accepted no-op). A CHANNEL unfollow stays two-phase (describe → `--yes`): which skills stop
//! is computed breadth (the union math over channels and direct follows), so the disclosure comes
//! first. A mixed invocation describes — the gated arm sets the bar for the whole batch.
//!
//! The detach is PERSON-scoped and server-recorded: a skill unfollow writes the standing
//! `skill_unfollows` row (delivery ends on EVERY device; the entitlement predicate subtracts it even
//! where a channel still references it), a channel unfollow leaves the channel's membership. Local
//! copies are KEPT as frozen copies everywhere — nothing is deleted, and `follow` re-attaches. The
//! local `follows.json` pause flag flips alongside a skill detach, so `list`'s cause column reads the
//! frozen copy correctly even offline.
//!
//! Refusals live at the grammar edge: a WORKSPACE target is a web action ("leave" is a roster
//! change, and people ops beyond invite are web-only); the structural `everyone` cannot be left at
//! all (the alternatives are spelled). Un-enrolled (or for a purely local skill) the verb keeps its
//! graceful local path: the pause flag flips, bytes stay, nothing needs a server.
//!
//! `unfollow --agent <slug>` never reaches this module — it dispatches to the offline per-agent
//! exclusion ([`super::agent_scope`], the SAME implementation `remove --agent` runs): the
//! subscription is untouched and no server row moves.

use serde::Serialize;

use crate::ctx::Ctx;
use crate::enroll;
use crate::error::ClientError;
use crate::plane::{DeliverySnapshot, PlaneError};
use crate::resolve::{self, Resolution, ResourceKind};

use super::follow::{DeliveryConnect, DirectoryConnect};

/// The network seams `unfollow` needs (a subset of `follow`'s — no enrollment door here).
pub(crate) struct UnfollowConnectors<'a> {
    pub directory: &'a DirectoryConnect<'a>,
    pub delivery: &'a DeliveryConnect<'a>,
}

/// One detach item, on both the describe and the apply.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UnfollowItem {
    /// `channel` / `skill`.
    pub kind: String,
    pub name: String,
    /// The workspace the detach is recorded in; absent for a purely local pause (un-enrolled, or a
    /// skill the plane no longer serves).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The skills whose delivery ENDS with this detach (frozen in place on every device).
    pub stops: Vec<String>,
    /// The skills that KEEP arriving (another channel or a direct follow still delivers them).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keeps: Vec<String>,
}

/// The two-phase describe — what stops where, and what never changes.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UnfollowDescribe {
    pub items: Vec<UnfollowItem>,
    /// The detach is person-scoped: it ends delivery on EVERY device of yours.
    pub all_devices_note: String,
    /// Local copies stay frozen in place — nothing is deleted; `follow` re-attaches.
    pub bytes_note: String,
    /// The workspace records the detach (who acted, and when) — the final detach record.
    pub record_note: String,
}

/// The apply report (a skill unfollow's immediate apply, or a channel unfollow's `--yes`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UnfollowApplied {
    pub items: Vec<UnfollowItem>,
    /// Always true — an unfollow never touches a byte.
    pub bytes_kept: bool,
    /// The literal inverse command (paste-ready argv) — re-follow what this stopped.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub undo: Vec<String>,
}

/// The verb's outcome — the two-phase pair.
#[derive(Debug)]
pub(crate) enum UnfollowOutcome {
    Described {
        describe: UnfollowDescribe,
        yes_argv: Vec<String>,
    },
    Applied(UnfollowApplied),
}

/// One resolved detach, pre-apply.
enum Detach {
    Channel {
        workspace_id: String,
        name: String,
    },
    Skill {
        workspace_id: String,
        skill_id: String,
        name: String,
    },
    /// A purely local pause (un-enrolled, or a tracked skill with no workspace resolution).
    LocalSkill {
        skill_id: String,
        name: String,
    },
}

/// Dispatch the `unfollow` verb: resolve every target dual-kind (all-or-none), refuse workspace /
/// `everyone` targets typed; a skill-only invocation applies immediately (undo-led receipt;
/// `--yes` an accepted no-op), a channel target keeps the two-phase describe → `--yes`.
///
/// # Errors
/// [`ClientError::InvalidArgument`] for a workspace / `everyone` / local-domain target;
/// [`ClientError::TargetNotFound`] for an unresolvable one; [`ClientError::AmbiguousTarget`] /
/// [`ClientError::AmbiguousName`] for an ambiguous one; otherwise a transport / io failure.
pub(crate) fn unfollow(
    ctx: &Ctx<'_>,
    connectors: &UnfollowConnectors<'_>,
    targets: &[String],
    channels: &[String],
    skills: &[String],
    yes: bool,
) -> Result<UnfollowOutcome, ClientError> {
    // The built-in `topos` skill is not a subscription — there is nothing to unfollow; taking it
    // off this machine is `remove` (recognized before the grammar; the name is reserved).
    if targets
        .iter()
        .chain(skills.iter())
        .any(|t| super::builtin::is_builtin(t))
    {
        return Err(ClientError::InvalidArgument(
            "`topos` is the built-in skill — nothing is followed; `topos remove topos` takes it \
             off this machine"
                .into(),
        ));
    }
    let mut specs: Vec<resolve::TargetSpec> = targets
        .iter()
        .map(|t| resolve::TargetSpec::free(t))
        .collect();
    specs.extend(
        channels
            .iter()
            .map(|c| resolve::TargetSpec::kinded(c, ResourceKind::Channel)),
    );
    specs.extend(
        skills
            .iter()
            .map(|s| resolve::TargetSpec::kinded(s, ResourceKind::Skill)),
    );
    if specs.is_empty() {
        return Err(ClientError::InvalidArgument(
            "unfollow needs a channel or skill (a name, `--channel <name>`, or `--skill <name>`)"
                .into(),
        ));
    }

    let (base_url, universe) = super::follow::build_universe_via(ctx, connectors.directory)?;

    // Resolve ALL-OR-NONE. The workspace kind stays IN scope so a workspace target is recognized
    // and refused toward the web — never mistaken for not-found.
    let mut detaches = Vec::with_capacity(specs.len());
    for spec in &specs {
        let parsed = resolve::parse_target(&spec.token)?;
        let scope = match spec.forced {
            Some(ResourceKind::Channel) => resolve::KindScope::CHANNELS,
            Some(ResourceKind::Skill) => resolve::KindScope::SKILLS,
            None => resolve::KindScope::ALL,
        };
        match resolve::resolve_one(&universe, &parsed, scope)? {
            Some(Resolution::Workspace { workspace_name, .. }) => {
                return Err(ClientError::InvalidArgument(format!(
                    "leaving a workspace is a web action — unfollow its channels or skills to \
                     stop deliveries (e.g. `topos unfollow --channel <name>`); '{workspace_name}' \
                     itself is managed on the web"
                )));
            }
            Some(Resolution::Resource {
                workspace_id,
                kind: ResourceKind::Channel,
                name,
                ..
            }) => {
                if name == "everyone" {
                    return Err(ClientError::InvalidArgument(
                        "`everyone` is structural — every member is in it, and it cannot be left; \
                         unfollow specific skills instead (`topos unfollow --skill <name>`), or \
                         take one off this device with `topos remove <name>`"
                            .into(),
                    ));
                }
                detaches.push(Detach::Channel { workspace_id, name });
            }
            Some(Resolution::Resource {
                workspace_id,
                kind: ResourceKind::Skill,
                name,
                skill_id,
                ..
            }) => {
                let skill_id = skill_id.ok_or_else(|| {
                    ClientError::WireInvalid("a resolved skill carried no id".into())
                })?;
                detaches.push(Detach::Skill {
                    workspace_id,
                    skill_id,
                    name,
                });
            }
            // Unresolved against the plane: the graceful LOCAL path — a tracked skill (followed or
            // not) pauses locally; anything else is the uniform not-found.
            None => match super::resolve_skill(ctx, &spec.token) {
                Ok((sid, lock)) => detaches.push(Detach::LocalSkill {
                    skill_id: sid.into_string(),
                    name: lock.name,
                }),
                Err(ClientError::NoSuchSkill { .. }) => {
                    return Err(resolve::not_found(&spec.token));
                }
                Err(e) => return Err(e),
            },
        }
    }

    // The describe facts: per workspace, what stops vs what keeps arriving.
    let items = describe_items(connectors, base_url.as_deref(), &detaches)?;

    // A CHANNEL detach stays two-phase: which skills stop is computed breadth (union math), so
    // the disclosure gates the apply. A skill-only invocation applies immediately (`--yes` is an
    // accepted no-op) — the explicit command is the consent, and the receipt leads with its undo.
    let gated = detaches.iter().any(|d| matches!(d, Detach::Channel { .. }));
    if gated && !yes {
        let mut yes_argv = vec!["topos".to_owned(), "unfollow".to_owned()];
        for item in &items {
            yes_argv.push(format!("--{}", item.kind));
            yes_argv.push(item.name.clone());
        }
        yes_argv.push("--yes".to_owned());
        return Ok(UnfollowOutcome::Described {
            describe: UnfollowDescribe {
                items,
                all_devices_note: "the detach is person-scoped — delivery ends on every device \
                                   enrolled as you"
                    .to_owned(),
                bytes_note: "local copies stay frozen in place (nothing is deleted); `topos \
                             follow` re-attaches"
                    .to_owned(),
                record_note: "the workspace records the detach — who stopped following, and when"
                    .to_owned(),
            },
            yes_argv,
        });
    }

    // ---- APPLY ----
    // The transport is built only when a SERVER row moves — a purely local pause (the graceful
    // offline path) never dials.
    let needs_server = detaches
        .iter()
        .any(|d| !matches!(d, Detach::LocalSkill { .. }));
    let directory = match (&base_url, needs_server) {
        (Some(b), true) => Some((connectors.directory)(b)),
        _ => None,
    };
    for detach in &detaches {
        match detach {
            Detach::Channel { workspace_id, name } => {
                let directory = directory.as_deref().ok_or_else(|| {
                    ClientError::Enrollment("not enrolled; nothing to leave".into())
                })?;
                directory.channel_leave(workspace_id, name)?;
            }
            Detach::Skill {
                workspace_id,
                skill_id,
                ..
            } => {
                let directory = directory.as_deref().ok_or_else(|| {
                    ClientError::Enrollment("not enrolled; nothing to unfollow".into())
                })?;
                directory.unfollow_skill(workspace_id, skill_id)?;
                // The local pause flips alongside the server row (the same identity-locked
                // read-modify-write the local-only verb always used) — `list`'s cause column and
                // the offline sweep read the frozen state without a network round trip.
                enroll::set_following(ctx.fs, &ctx.layout, skill_id, false)?;
            }
            Detach::LocalSkill { skill_id, .. } => {
                // The graceful local path: flip the durable pause flag; a missing entry (a purely
                // local, never-followed skill) is the same clean success — already not followed.
                enroll::set_following(ctx.fs, &ctx.layout, skill_id, false)?;
            }
        }
    }
    // The literal inverse: re-follow everything this invocation stopped (`follow` re-attaches a
    // paused skill and re-joins a channel).
    let mut undo = vec!["topos".to_owned(), "follow".to_owned()];
    for item in &items {
        match item.kind.as_str() {
            "channel" => {
                undo.push("--channel".to_owned());
                undo.push(item.name.clone());
            }
            _ => undo.push(item.name.clone()),
        }
    }
    Ok(UnfollowOutcome::Applied(UnfollowApplied {
        items,
        bytes_kept: true,
        undo,
    }))
}

/// Build the per-detach describe items: a CHANNEL detach splits its skills into stops (delivered
/// via this channel alone, no direct follow) and keeps (another channel / direct still delivers); a
/// SKILL detach stops the skill outright (the unfollow row subtracts it from every channel's
/// delivery); a LOCAL pause stops this install's sweep only.
fn describe_items(
    connectors: &UnfollowConnectors<'_>,
    base_url: Option<&str>,
    detaches: &[Detach],
) -> Result<Vec<UnfollowItem>, ClientError> {
    // One delivery snapshot per touched workspace (read-only; the split needs the via attribution).
    let mut snapshots: std::collections::HashMap<String, DeliverySnapshot> =
        std::collections::HashMap::new();
    let needs_server = detaches
        .iter()
        .any(|d| !matches!(d, Detach::LocalSkill { .. }));
    if let (Some(base), true) = (base_url, needs_server) {
        let delivery = (connectors.delivery)(base);
        for detach in detaches {
            let ws = match detach {
                Detach::Channel { workspace_id, .. } | Detach::Skill { workspace_id, .. } => {
                    workspace_id.clone()
                }
                Detach::LocalSkill { .. } => continue,
            };
            if snapshots.contains_key(&ws) {
                continue;
            }
            let snapshot = delivery.fetch_delivery(&ws).map_err(|e| match e {
                PlaneError::NotFound => resolve::not_found(&ws),
                PlaneError::Unreachable(m) | PlaneError::Unavailable(m) => ClientError::Plane(m),
                PlaneError::Malformed(m) => ClientError::WireInvalid(m),
            })?;
            snapshots.insert(ws, snapshot);
        }
    }

    let mut items = Vec::with_capacity(detaches.len());
    for detach in detaches {
        items.push(match detach {
            Detach::Channel { workspace_id, name } => {
                let mut stops = Vec::new();
                let mut keeps = Vec::new();
                if let Some(snapshot) = snapshots.get(workspace_id) {
                    for ds in &snapshot.skills {
                        if !ds.via_channels.iter().any(|c| c == name) {
                            continue;
                        }
                        let still = ds.via_direct || ds.via_channels.iter().any(|c| c != name);
                        if still {
                            keeps.push(ds.name.clone());
                        } else {
                            stops.push(ds.name.clone());
                        }
                    }
                }
                UnfollowItem {
                    kind: "channel".to_owned(),
                    name: name.clone(),
                    workspace_id: Some(workspace_id.clone()),
                    stops,
                    keeps,
                }
            }
            Detach::Skill {
                workspace_id, name, ..
            } => UnfollowItem {
                kind: "skill".to_owned(),
                name: name.clone(),
                workspace_id: Some(workspace_id.clone()),
                // The unfollow row subtracts the skill from the WHOLE entitlement (channels
                // included) — it stops, full stop.
                stops: vec![name.clone()],
                keeps: Vec::new(),
            },
            Detach::LocalSkill { name, .. } => UnfollowItem {
                kind: "skill".to_owned(),
                name: name.clone(),
                workspace_id: None,
                stops: vec![name.clone()],
                keeps: Vec::new(),
            },
        });
    }
    Ok(items)
}
