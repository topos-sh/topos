//! `init` — create THIS folder's `topos.toml` (the project manifest). Any folder, git or not;
//! outside a shared repo the receipt notes the file won't travel. Idempotent: an existing manifest
//! is a clean no-op receipt (`created: false`), never an error and never an overwrite.

use topos_types::results::InitData;

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::manifest::file::{MANIFEST_FILE, ManifestEditor};
use crate::manifest::walk;

/// Create `topos.toml` in the CURRENT directory (the folder IS the scope — `init` never walks up;
/// `add` with no manifest in reach is what prefers the git root).
///
/// # Errors
/// [`ClientError::InvalidArgument`] when the working directory is unknown; an io failure.
pub(crate) fn init(ctx: &Ctx<'_>) -> Result<InitData, ClientError> {
    let cwd = ctx
        .roots
        .as_ref()
        .and_then(|r| r.cwd.clone())
        .ok_or_else(|| {
            ClientError::InvalidArgument(
                "cannot resolve the current directory — run `topos init` inside the folder the \
                 manifest should govern"
                    .into(),
            )
        })?;
    let path = cwd.join(MANIFEST_FILE);
    let note = if walk::init_dir(ctx.fs, &cwd) == cwd && !ctx.fs.exists(&cwd.join(".git")) {
        Some(
            "outside a git repository — this manifest stays local to this folder (it won't \
             travel with a repo)"
                .to_owned(),
        )
    } else {
        None
    };
    if ctx.fs.exists(&path) {
        return Ok(InitData {
            manifest: path.display().to_string(),
            created: false,
            note,
        });
    }
    crate::atomic::atomic_write(ctx.fs, &path, ManifestEditor::init_template().as_bytes())?;
    Ok(InitData {
        manifest: path.display().to_string(),
        created: true,
        note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::AgentRoots;
    use crate::fs_seam::RealFs;
    use crate::ids::{RealClock, RealIds};
    use crate::plane::{InertFollow, InertPlane};
    use crate::sidecar::Layout;
    use std::path::{Path, PathBuf};
    use topos_harness::ClaudeCode;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-init-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn with_ctx<R>(home: &Path, cwd: Option<&Path>, f: impl FnOnce(&Ctx<'_>) -> R) -> R {
        let fs = RealFs;
        let ids = RealIds;
        let clock = RealClock;
        let plane = InertPlane;
        let follow = InertFollow;
        let harness = ClaudeCode::new(scratch("adapter"), &fs);
        let ctx = Ctx {
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: String::new(),
            layout: Layout::new(&home.join(".topos")),
            harness: &harness,
            plane: &plane,
            follow: &follow,
            roots: cwd.map(|c| AgentRoots {
                home: home.to_path_buf(),
                cwd: Some(c.to_path_buf()),
            }),
        };
        f(&ctx)
    }

    #[test]
    fn init_creates_once_and_reports_the_no_op() {
        let home = scratch("create");
        let repo = home.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        with_ctx(&home, Some(&repo), |ctx| {
            let first = init(ctx).unwrap();
            assert!(first.created);
            assert!(first.note.is_none(), "inside a repo — no travel note");
            assert!(std::path::Path::new(&first.manifest).exists());
            // The created file parses as an EMPTY manifest (the template is comments only).
            let m =
                crate::manifest::file::read_manifest(ctx.fs, std::path::Path::new(&first.manifest))
                    .unwrap()
                    .unwrap();
            assert!(m.is_empty());
            // Idempotent — the second run is a no-op receipt, never an overwrite.
            std::fs::write(
                &first.manifest,
                "# hand-edited\n[skills]\n\"./a\" = \"*\"\n",
            )
            .unwrap();
            let second = init(ctx).unwrap();
            assert!(!second.created);
            let text = std::fs::read_to_string(&first.manifest).unwrap();
            assert!(text.contains("hand-edited"), "{text}");
        });
    }

    #[test]
    fn init_outside_a_repo_notes_the_file_wont_travel() {
        let home = scratch("norepo");
        let stray = home.join("stray");
        std::fs::create_dir_all(&stray).unwrap();
        with_ctx(&home, Some(&stray), |ctx| {
            let out = init(ctx).unwrap();
            assert!(out.created);
            assert!(
                out.note.as_deref().is_some_and(|n| n.contains("git")),
                "{out:?}"
            );
        });
    }

    #[test]
    fn init_without_a_working_directory_refuses_typed() {
        let home = scratch("nocwd");
        with_ctx(&home, None, |ctx| {
            let err = init(ctx).unwrap_err();
            assert_eq!(err.code(), "INVALID_ARGUMENT");
        });
    }
}
