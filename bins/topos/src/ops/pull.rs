//! `pull [--quiet]` — the session-start currency entry point.
//!
//! This is a **no-op skeleton**: the currency engine (a conditional GET on each followed skill's signed
//! `current` pointer, verify, apply) lands later. Today nothing is followed and there is no plane, so
//! `pull` reports an honestly empty state and exits 0 — exactly what the installed session-start hook
//! runs. It performs no network I/O and never implies live sync; `--quiet` (handled at the dispatch
//! layer) keeps stdout byte-silent, since a SessionStart hook's stdout is injected into the session.

use topos_types::results::PullData;

use crate::ctx::Ctx;
use crate::error::ClientError;

/// Run the currency check. A no-op until the engine lands: nothing is followed yet, so the result is
/// honestly empty (no skills, no proposals awaiting review).
///
/// # Errors
/// None today — kept fallible for the sync engine that lands here later.
pub(crate) fn pull(_ctx: &Ctx<'_>) -> Result<PullData, ClientError> {
    Ok(PullData {
        skills: Vec::new(),
        proposals_awaiting: 0,
    })
}
