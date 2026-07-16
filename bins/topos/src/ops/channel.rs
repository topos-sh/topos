//! `channel add|remove <channel> <skill>...` — place / remove skill references in a channel, two-phase.
//!
//! Channel-first argument shape (parsed in `cli.rs` as an arg vector). Resolve EVERY named skill through
//! the one grammar FIRST (all-or-none); a channel that does not exist yet is CREATED on the first `add`
//! placement (the describe says "creates `#<ch>`"). `--yes` runs `PUT`/`DELETE
//! channels/{ch}/skills/{skill}` per skill. A CURATED channel gates placement by role (reviewer+); the
//! server refuses a non-reviewer with `CURATED_ROLE_REQUIRED`, which surfaces as a typed refusal naming
//! who can. Per-skill outcomes are reported honestly if a later placement fails after an earlier landed.

use topos_types::results::{ChannelAction, ChannelData, ChannelItem, ChannelItemOutcome};

use super::follow::{DirectoryConnect, build_universe_via};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::resolve::{self, Resolution, ResourceKind, TargetSpec, WorkspaceNames};

/// The seams `channel` needs — the directory connector builds the universe and writes the placements.
pub(crate) struct ChannelConnectors<'a> {
    pub directory: &'a DirectoryConnect<'a>,
}

/// The verb's outcome — the two-phase pair.
#[derive(Debug)]
pub(crate) enum ChannelOutcome {
    Described {
        data: ChannelData,
        yes_argv: Vec<String>,
    },
    Applied(ChannelData),
}

/// One resolved skill placement, pre-apply.
struct Placement {
    skill_id: String,
    name: String,
}

/// Dispatch `channel add|remove <channel> <skill>...`.
///
/// # Errors
/// [`ClientError::InvalidArgument`] for a malformed arg vector or cross-workspace skill set;
/// [`ClientError::TargetNotFound`] for an unresolvable skill / a `remove` on a missing channel;
/// [`ClientError::Denied`] for a curated-channel role refusal; a transport failure otherwise.
pub(crate) fn channel(
    ctx: &Ctx<'_>,
    connectors: &ChannelConnectors<'_>,
    args: &[String],
    workspace: Option<&str>,
    yes: bool,
) -> Result<ChannelOutcome, ClientError> {
    // args = [action, channel, skill...] — action is guaranteed add|remove by the dispatcher.
    let action = match args.first().map(String::as_str) {
        Some("add") => ChannelAction::Add,
        Some("remove") => ChannelAction::Remove,
        _ => {
            return Err(ClientError::InvalidArgument(
                "usage: `topos channel add <channel> <skill>...` or `topos channel remove \
                 <channel> <skill>...`"
                    .into(),
            ));
        }
    };
    let channel_name = args.get(1).map(String::as_str).ok_or_else(|| {
        ClientError::InvalidArgument(
            "channel add/remove needs a channel name: `topos channel add <channel> <skill>...`"
                .into(),
        )
    })?;
    let skill_tokens = &args[2.min(args.len())..];
    if skill_tokens.is_empty() {
        return Err(ClientError::InvalidArgument(format!(
            "`channel {} {channel_name}` needs at least one skill to {}",
            action_word(action),
            action_word(action),
        )));
    }

    let (base_url, universe) = build_universe_via(ctx, connectors.directory)?;
    let base_url = base_url.ok_or_else(|| {
        ClientError::Enrollment("not enrolled; run `topos follow <link>` first".into())
    })?;

    // Resolve EVERY skill ALL-OR-NONE (SKILLS scope; `--workspace` narrows a shared name).
    let scope = workspace_scope(&universe, workspace);
    let specs: Vec<TargetSpec> = skill_tokens
        .iter()
        .map(|t| TargetSpec::kinded(t, ResourceKind::Skill))
        .collect();
    let resolutions = resolve::resolve_all(scope, &specs, resolve::KindScope::SKILLS)?;

    // Every skill must live in ONE workspace (a channel belongs to a workspace); the channel is resolved
    // there.
    let workspace_id = single_workspace(&resolutions)?;
    let placements: Vec<Placement> = resolutions
        .iter()
        .map(|r| match r {
            Resolution::Resource { skill_id, name, .. } => Ok(Placement {
                skill_id: skill_id.clone().ok_or_else(|| {
                    ClientError::WireInvalid("a resolved skill carried no id".into())
                })?,
                name: name.clone(),
            }),
            Resolution::Workspace { .. } => Err(ClientError::WireInvalid(
                "a skill selector resolved to a workspace".into(),
            )),
        })
        .collect::<Result<_, _>>()?;

    // Resolve the channel in that workspace: its accurate mode + whether it exists yet (the universe
    // carries names only, so read the channel index for the mode the describe's gate discloses).
    let directory = (connectors.directory)(&base_url);
    let index = directory.channels_index(&workspace_id)?;
    let existing = index.channels.iter().find(|c| c.name == channel_name);
    let creates = existing.is_none();
    let mode = existing.map_or_else(|| "open".to_owned(), |c| c.mode.clone());
    if creates && matches!(action, ChannelAction::Remove) {
        // Nothing to remove from a channel that does not exist.
        return Err(resolve::not_found(channel_name));
    }

    let describe_items: Vec<ChannelItem> = placements
        .iter()
        .map(|p| ChannelItem {
            skill: p.name.clone(),
            skill_id: p.skill_id.clone(),
            outcome: ChannelItemOutcome::Pending,
            detail: None,
        })
        .collect();

    if !yes {
        let mut yes_argv = vec![
            "topos".to_owned(),
            "channel".to_owned(),
            action_word(action).to_owned(),
            channel_name.to_owned(),
        ];
        yes_argv.extend(skill_tokens.iter().cloned());
        yes_argv.push("--yes".to_owned());
        return Ok(ChannelOutcome::Described {
            data: ChannelData {
                channel: channel_name.to_owned(),
                workspace_id,
                action,
                mode,
                creates,
                items: describe_items,
                applied: false,
            },
            yes_argv,
        });
    }

    // ---- APPLY (`--yes`) ---- (reuse the transport built for the channel-index read)
    let mut items = Vec::with_capacity(placements.len());
    let mut landed = 0usize;
    for p in &placements {
        let result = match action {
            ChannelAction::Add => directory.channel_place(&workspace_id, channel_name, &p.skill_id),
            ChannelAction::Remove => {
                directory.channel_unplace(&workspace_id, channel_name, &p.skill_id)
            }
        };
        match result {
            Ok(()) => {
                landed += 1;
                items.push(ChannelItem {
                    skill: p.name.clone(),
                    skill_id: p.skill_id.clone(),
                    outcome: match action {
                        ChannelAction::Add => ChannelItemOutcome::Placed,
                        ChannelAction::Remove => ChannelItemOutcome::Removed,
                    },
                    detail: None,
                });
            }
            // The FIRST failure with nothing landed is a clean typed refusal (a curated channel's role
            // gate hits every placement uniformly, so nothing partial happened) — name who can.
            Err(e) if landed == 0 && items.iter().all(|i| i.detail.is_none()) => {
                return Err(reword_refusal(e, channel_name));
            }
            // A later failure after an earlier landed: report honestly, per skill (never silently drop).
            Err(e) => {
                items.push(ChannelItem {
                    skill: p.name.clone(),
                    skill_id: p.skill_id.clone(),
                    outcome: ChannelItemOutcome::Failed,
                    detail: Some(refusal_detail(&e)),
                });
            }
        }
    }
    Ok(ChannelOutcome::Applied(ChannelData {
        channel: channel_name.to_owned(),
        workspace_id,
        action,
        mode,
        creates,
        items,
        applied: true,
    }))
}

/// Narrow the universe to one workspace when `--workspace` is given (so a shared skill name resolves);
/// otherwise the whole universe.
fn workspace_scope<'a>(
    universe: &'a [WorkspaceNames],
    workspace: Option<&str>,
) -> &'a [WorkspaceNames] {
    match workspace {
        Some(ws) => {
            // A borrow of the single matching entry, or the whole universe if the id is unknown (the
            // resolver then answers the uniform not-found / ambiguity honestly).
            if let Some(pos) = universe.iter().position(|w| w.workspace_id == ws) {
                &universe[pos..=pos]
            } else {
                universe
            }
        }
        None => universe,
    }
}

/// The single workspace every resolved skill shares — a channel placement is workspace-scoped, so a set
/// spanning workspaces is a typed refusal (pass `--workspace`, or split the command).
fn single_workspace(resolutions: &[Resolution]) -> Result<String, ClientError> {
    let mut ws: Option<&str> = None;
    for r in resolutions {
        let this = r.workspace_id();
        match ws {
            None => ws = Some(this),
            Some(w) if w != this => {
                return Err(ClientError::InvalidArgument(
                    "those skills live in different workspaces — a channel belongs to one workspace; \
                     run one `channel` command per workspace (`--workspace <name>` narrows the set)"
                        .into(),
                ));
            }
            Some(_) => {}
        }
    }
    ws.map(str::to_owned).ok_or_else(|| {
        ClientError::InvalidArgument("channel add/remove needs at least one skill".into())
    })
}

fn action_word(action: ChannelAction) -> &'static str {
    match action {
        ChannelAction::Add => "add",
        ChannelAction::Remove => "remove",
    }
}

/// Reword a first-placement refusal into a typed answer. A curated channel's role gate names who can.
fn reword_refusal(e: ClientError, channel: &str) -> ClientError {
    if let ClientError::PlaneTerminal { code, .. } = &e
        && code.contains("ROLE")
    {
        return ClientError::Denied(format!(
            "curating #{channel} needs reviewer or owner — ask a reviewer to place the skill \
             (code {code})"
        ));
    }
    e
}

/// A per-skill failure's human detail (for the honest partial report).
fn refusal_detail(e: &ClientError) -> String {
    match e {
        ClientError::PlaneTerminal { code, .. } => format!("refused: {code}"),
        other => crate::render::safe_message(other),
    }
}
