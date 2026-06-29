//! `pull` — the session-start currency entry point + the targeted accept / go-back.
//!
//! The bare `topos pull` (the installed session-start hook) sweeps every followed skill toward its
//! `current`. A targeted `topos pull <skill>` accepts a pending update for one skill (the explicit
//! command supplies the consent a confirm-each offer solicited); `topos pull <skill>@<hash>` goes back to
//! a specific local version. The per-skill engine (check → plan → apply) lives in
//! [`super::sync_engine`]; this module is the scope dispatch + aggregation.
//!
//! In production nothing is followed yet (no enrollment), so the inert follow source yields an empty
//! work-list and the bare sweep reports an honestly empty state — exactly what the installed hook runs.
//! The engine, the anti-rollback floor, the crash-safe materializer, and the four-state machine are all
//! real and exercised by the fixture-driven tests.

use topos_types::results::PullData;

use crate::ctx::Ctx;
use crate::error::ClientError;

use super::sync_engine;

/// What a `pull` invocation targets.
pub(crate) enum PullScope {
    /// The bare session-start sweep — every followed skill.
    AllFollowed,
    /// One skill, by name, in a targeted mode.
    One { name: String, mode: TargetMode },
}

/// How a targeted single-skill pull behaves.
pub(crate) enum TargetMode {
    /// `topos pull <skill>` — accept a pending update / resume a held skill / resolve a divergence (no `@hash`).
    AcceptPending,
    /// `topos pull <skill> --onto-current` — the disclosed escape: commit MY bytes on top of `current`,
    /// dropping the merge (a 2-way diff of what is dropped is surfaced). Resolves a divergence without merging.
    OntoCurrent,
    /// `topos pull <skill>@<hash>` — install an older version's bytes locally (a deliberate go-back).
    GoBack([u8; 32]),
}

/// Run the currency check for `scope`.
///
/// # Errors
/// A hard failure resolving a targeted skill, or (for a targeted pull) a plane-read failure; the bare
/// sweep isolates per-skill failures instead of erroring.
pub(crate) fn pull(ctx: &Ctx<'_>, scope: PullScope) -> Result<PullData, ClientError> {
    let proposals_awaiting = ctx.follow.proposals_awaiting();
    match scope {
        PullScope::AllFollowed => {
            let mut skills = Vec::new();
            for (skill_id, follow) in ctx.follow.followed() {
                if !follow.following {
                    continue;
                }
                match sync_engine::sync_one(ctx, &skill_id, &follow, sync_engine::Invocation::Sweep)
                {
                    Ok(row) => skills.push(row),
                    // A hard per-skill failure (corrupt docs, store/io) must not abort the whole sweep —
                    // diagnose on stderr (never stdout, which the hook injects) and leave that skill put.
                    Err(e) => eprintln!("topos pull: skill {skill_id}: {e}"),
                }
            }
            Ok(PullData {
                skills,
                proposals_awaiting,
            })
        }
        PullScope::One { name, mode } => {
            let (skill_id, _lock) = super::resolve_skill(ctx, &name)?;
            let row = match mode {
                TargetMode::GoBack(hash) => sync_engine::go_back(ctx, &skill_id, hash)?,
                TargetMode::AcceptPending | TargetMode::OntoCurrent => {
                    let inv = match mode {
                        TargetMode::OntoCurrent => sync_engine::Invocation::Escape,
                        _ => sync_engine::Invocation::Accept,
                    };
                    match ctx
                        .follow
                        .followed()
                        .into_iter()
                        .find(|(id, _)| *id == skill_id)
                    {
                        Some((_, follow)) if follow.following => {
                            sync_engine::sync_one(ctx, &skill_id, &follow, inv)?
                        }
                        // Tracked but not followed → there is no `current` to pull; report the local state.
                        _ => sync_engine::current_state(ctx, &skill_id)?,
                    }
                }
            };
            Ok(PullData {
                skills: vec![row],
                proposals_awaiting,
            })
        }
    }
}
