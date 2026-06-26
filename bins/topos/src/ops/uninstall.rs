//! `uninstall [--footprint]` — remove the binary + `~/.topos/`, touching **no** skill bytes. The user's
//! source directories live outside `~/.topos/` and are never referenced for deletion.

use std::path::Path;

use serde::Serialize;

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
}

/// Remove `~/.topos/` (and, if given, the binary). `binary` is injected so a test removes a fake target,
/// never the test runner.
///
/// # Errors
/// An [`FsOps`](crate::fs_seam::FsOps) removal failure.
pub(crate) fn uninstall(
    ctx: &Ctx<'_>,
    want_footprint: bool,
    binary: Option<&Path>,
) -> Result<UninstallOutcome, ClientError> {
    // Capture the owned set before removal (so `--footprint` reports what is being torn down).
    let footprint = if want_footprint {
        Some(footprint(ctx.fs, &ctx.layout)?)
    } else {
        None
    };

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
    })
}
