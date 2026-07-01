//! `unfollow <skill>` — stop following `current`; KEEP the local bytes as a frozen copy.
//!
//! Local-only and byte-inert: it flips `following = false` in `follows.json` (retaining the workspace,
//! mode, and read credential so a later `follow` resumes) and touches NOTHING else — never the skill
//! bytes, never the sidecar sync state or a `held` pin, never the currency hook (the hook is
//! per-install; its sweep simply skips an unfollowed skill). "Frozen" means auto-updates stop; an
//! explicit local `pull <skill>@<hash>` (a user-initiated go-back on their own copy) remains available.
//! Idempotent: not-followed / already-unfollowed is the same clean success.

use topos_types::results::UnfollowData;

use crate::ctx::Ctx;
use crate::enroll;
use crate::error::ClientError;

/// Dispatch the `unfollow` verb.
///
/// # Errors
/// [`ClientError::NoSuchSkill`] / [`ClientError::AmbiguousName`] on an unresolvable name; otherwise an
/// io/doc failure reading or writing `follows.json`.
pub(crate) fn unfollow(ctx: &Ctx<'_>, name: &str) -> Result<UnfollowData, ClientError> {
    let (skill_id, _lock) = super::resolve_skill(ctx, name)?;

    // Flip `following` in place, entirely under the identity lock — only that one field moves, so the
    // entry stays a complete record (token / workspace / mode retained) a later follow resumes over, and
    // a concurrent enrollment writer's fresh row is never clobbered by a stale snapshot. A tracked skill
    // with no follow entry at all (e.g. adopted locally via `add`) is already not followed — the same
    // clean success, nothing written.
    enroll::set_following(ctx.fs, &ctx.layout, skill_id.as_str(), false)?;

    Ok(UnfollowData {
        skill_id: skill_id.into_string(),
        following: false,
        bytes_kept: true,
    })
}
