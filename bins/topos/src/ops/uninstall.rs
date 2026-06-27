//! `uninstall [--footprint]` — scrub the harness currency hook, then remove the binary + `~/.topos/`,
//! touching **no** skill bytes. The user's source directories live outside `~/.topos/` and are never
//! referenced for deletion; the shared harness config is *scrubbed* of our hook entry, never removed.

use std::path::Path;

use serde::Serialize;
use topos_types::TriggerReport;

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::sidecar::footprint;

/// What `uninstall` did. Ad-hoc data — this verb has no frozen result schema; the envelope stays
/// schema-valid (a free-form `data`). The "touches no skill bytes" guarantee is structural (the user's
/// source dir is never referenced for deletion) and is asserted directly by the per-file-sha256 test.
#[derive(Debug, Serialize)]
pub(crate) struct UninstallOutcome {
    pub home_removed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footprint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_removed: Option<String>,
    /// The currency-trigger scrub outcome (the harness config edit) — disclosed so the user can see
    /// whether the hook was removed, was never present, or could not be safely scrubbed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<TriggerReport>,
}

/// Scrub the currency hook, then remove `~/.topos/` (and, if given, the binary). `binary` is injected so
/// a test removes a fake target, never the test runner.
///
/// The harness currency entry is scrubbed FIRST (so the step stays re-runnable and a leftover never
/// points at an already-removed binary); a scrub that degrades (e.g. an unparseable settings.json) is
/// reported honestly but never blocks the rest of the teardown — the `command -v topos` guard already
/// makes any leftover entry an inert no-op.
///
/// # Errors
/// An [`FsOps`](crate::fs_seam::FsOps) removal failure.
pub(crate) fn uninstall(
    ctx: &Ctx<'_>,
    want_footprint: bool,
    binary: Option<&Path>,
) -> Result<UninstallOutcome, ClientError> {
    // Capture the owned set before removal (so `--footprint` reports what is being torn down) — the
    // `~/.topos/` walk PLUS the harness config path topos holds an entry in (disclosed, never deleted).
    let footprint = if want_footprint {
        let mut paths = footprint(ctx.fs, &ctx.layout)?;
        paths.extend(
            ctx.harness
                .uninstall_footprint()
                .iter()
                .map(|p| p.to_string_lossy().into_owned()),
        );
        paths.sort();
        Some(paths)
    } else {
        None
    };

    // Scrub the harness currency hook (idempotent; a no-op when none is present).
    let currency = Some(ctx.harness.remove_currency_trigger());

    let home = ctx.layout.home();
    let home_removed = if ctx.fs.exists(home) {
        ctx.fs.remove_dir_all(home)?;
        true
    } else {
        false
    };

    let binary_removed = match binary {
        Some(path) if ctx.fs.exists(path) => {
            ctx.fs.remove_file(path)?;
            Some(path.display().to_string())
        }
        _ => None,
    };

    Ok(UninstallOutcome {
        home_removed,
        footprint,
        binary_removed,
        currency,
    })
}
