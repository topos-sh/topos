//! `protect <target> [<level>]` — set a skill's or channel's protection level, two-phase.
//!
//! Dual-kind target: a SKILL bundle tightens to `reviewed` (its members' publishes reroute into
//! proposals) and loosens back to `open`; a CHANNEL tightens to `curated` (placement takes reviewer+)
//! and loosens back to `open`. Bare (no level) TIGHTENS to the kind's protected level; an explicit
//! `open` LOOSENS it (an owner act). The describe carries, when LOOSENING a skill, the note that
//! pending proposals survive and still await their verdict. `--yes` applies via the protection
//! routes; a role refusal (`OWNER_ROLE_REQUIRED` / `REVIEWER_ROLE_REQUIRED`) surfaces typed, naming the
//! role that can.

use topos_types::results::ProtectData;

use super::reconcile::SessionConnect;
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::resolve::{self, Resolution, ResourceKind};

/// The seam `protect` needs — the per-session transports build the universe and write the
/// protection level.
pub(crate) struct ProtectConnectors<'a> {
    /// The per-session transports (each read/write rides its session's own credential).
    pub session: &'a SessionConnect<'a>,
}

/// The verb's outcome — the two-phase pair.
#[derive(Debug)]
pub(crate) enum ProtectOutcome {
    Described {
        data: ProtectData,
        yes_argv: Vec<String>,
    },
    Applied(ProtectData),
}

/// Dispatch `protect <target> [<level>]`.
///
/// # Errors
/// [`ClientError::InvalidArgument`] for a level that does not apply to the target's kind;
/// [`ClientError::TargetNotFound`] for an unresolvable target; [`ClientError::Denied`] for a role
/// refusal; a transport failure otherwise.
pub(crate) fn protect(
    ctx: &Ctx<'_>,
    connectors: &ProtectConnectors<'_>,
    target: &str,
    level: Option<&str>,
    workspace: Option<&str>,
    yes: bool,
) -> Result<ProtectOutcome, ClientError> {
    let su = super::connect::session_universe(ctx, connectors.session)?;
    if su.universe.is_empty() {
        return Err(ClientError::SessionRequired {
            address: "<workspace-address>".to_owned(),
            message: "not connected to a workspace — run `topos login <workspace-address>` first"
                .into(),
        });
    }
    // The ambient narrowing every write verb honors: `--workspace` (or TOPOS_WORKSPACE) names
    // the ONE workspace this act may touch — by address name, or `<host>/<name>`. An unknown or
    // ambiguous naming refuses with the connected list; it is never ignored.
    let pick = workspace.map(str::to_owned).or_else(|| {
        std::env::var("TOPOS_WORKSPACE")
            .ok()
            .filter(|v| !v.trim().is_empty())
    });
    let narrowed: Vec<resolve::WorkspaceNames> = match pick.as_deref() {
        None => su.universe.clone(),
        Some(sel) => {
            let hits: Vec<_> = su
                .universe
                .iter()
                .filter(|w| w.name == sel || format!("{}/{}", w.host, w.name) == sel)
                .cloned()
                .collect();
            let spell = |ws: &[resolve::WorkspaceNames]| -> String {
                ws.iter()
                    .map(|w| format!("{}/{}", w.host, w.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            match hits.len() {
                1 => hits,
                0 => {
                    return Err(ClientError::InvalidArgument(format!(
                        "no connected workspace named '{sel}' — connected: {}",
                        spell(&su.universe)
                    )));
                }
                _ => {
                    return Err(ClientError::InvalidArgument(format!(
                        "'{sel}' names several connected workspaces — spell it                          `<host>/<name>`: {}",
                        spell(&hits)
                    )));
                }
            }
        }
    };
    let universe = &narrowed;

    let parsed = resolve::parse_target(target)?;
    let resolution = resolve::resolve_one(universe, &parsed, resolve::KindScope::SUBSCRIBABLE)?
        .ok_or_else(|| resolve::not_found(target))?;

    let (workspace_id, kind, name, skill_id) = match resolution {
        Resolution::Resource {
            workspace_id,
            kind,
            name,
            skill_id,
            ..
        } => (workspace_id, kind, name, skill_id),
        Resolution::Workspace { workspace_name, .. } => {
            return Err(ClientError::InvalidArgument(format!(
                "'{workspace_name}' is a workspace — `protect` sets a skill's or channel's level, not \
                 a workspace's"
            )));
        }
    };

    // Resolve the level per kind (bare = tighten to the protected default; an explicit level is
    // validated against the kind).
    let level = resolve_level(kind, level)?;
    let loosening = level == "open";

    let directory =
        su.directory_for(&workspace_id)
            .ok_or_else(|| ClientError::SessionRequired {
                address: "<workspace-address>".to_owned(),
                message:
                    "no session for this workspace — run `topos login <workspace-address>` first"
                        .into(),
            })?;
    let note = match (kind, loosening) {
        (ResourceKind::Skill, true) => {
            Some("pending proposals survive a loosening and still await their verdict".to_owned())
        }
        _ => None,
    };

    let workspace_address = universe
        .iter()
        .find(|w| w.workspace_id == workspace_id)
        .map(|w| format!("{}/{}", w.host, w.name));
    let data = ProtectData {
        target: target.to_owned(),
        kind: kind.noun().to_owned(),
        workspace_id: workspace_id.clone(),
        workspace_address,
        level: level.clone(),
        loosening,
        note,
        applied: false,
    };

    if !yes {
        let mut yes_argv = vec!["topos".to_owned(), "protect".to_owned(), target.to_owned()];
        if let Some(l) = level_argv(kind, &level) {
            yes_argv.push(l);
        }
        yes_argv.push("--yes".to_owned());
        return Ok(ProtectOutcome::Described { data, yes_argv });
    }

    // ---- APPLY (`--yes`) ----
    let result = match kind {
        ResourceKind::Skill => {
            let id = skill_id
                .ok_or_else(|| ClientError::WireInvalid("a resolved skill carried no id".into()))?;
            directory.protect_skill(&workspace_id, &id, &level)
        }
        ResourceKind::Channel => directory.protect_channel(&workspace_id, &name, &level),
    };
    result.map_err(reword_role_refusal)?;
    Ok(ProtectOutcome::Applied(ProtectData {
        applied: true,
        ..data
    }))
}

/// The level to set, per kind. Bare (no explicit level) TIGHTENS to the kind's protected default; an
/// explicit level is validated against the kind (a `curated` on a skill, or a `reviewed` on a channel,
/// is a typed usage error).
fn resolve_level(kind: ResourceKind, level: Option<&str>) -> Result<String, ClientError> {
    let Some(level) = level else {
        return Ok(match kind {
            ResourceKind::Skill => "reviewed".to_owned(),
            ResourceKind::Channel => "curated".to_owned(),
        });
    };
    let level = level.trim().to_ascii_lowercase();
    let ok = match kind {
        ResourceKind::Skill => matches!(level.as_str(), "reviewed" | "open"),
        ResourceKind::Channel => matches!(level.as_str(), "curated" | "open"),
    };
    if ok {
        Ok(level)
    } else {
        Err(ClientError::InvalidArgument(format!(
            "'{level}' is not a {noun} protection level — a {noun} is `{tight}` or `open`",
            noun = kind.noun(),
            tight = match kind {
                ResourceKind::Skill => "reviewed",
                ResourceKind::Channel => "curated",
            },
        )))
    }
}

/// The explicit level to echo into the `--yes` argv — omit it when it equals the kind's tighten default
/// (a bare `protect <target> --yes` tightens), else spell it (`open`, or the tighten level explicitly).
fn level_argv(kind: ResourceKind, level: &str) -> Option<String> {
    let default = match kind {
        ResourceKind::Skill => "reviewed",
        ResourceKind::Channel => "curated",
    };
    (level != default).then(|| level.to_owned())
}

/// Reword a role refusal into a typed answer naming the role that can act; everything else passes
/// through (a uniform not-found stays uniform).
fn reword_role_refusal(e: ClientError) -> ClientError {
    if let ClientError::PlaneTerminal { code, .. } = &e {
        if code.contains("OWNER") {
            return ClientError::Denied(format!(
                "loosening protection takes an owner — ask an owner (code {code})"
            ));
        }
        if code.contains("REVIEWER") || code.contains("ROLE") {
            return ClientError::Denied(format!(
                "tightening protection takes reviewer or owner — ask a reviewer (code {code})"
            ));
        }
    }
    e
}
