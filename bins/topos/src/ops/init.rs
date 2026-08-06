//! `init [-g]` — write a `topos.toml` you can then edit by hand.
//!
//! Without `-g` it creates THIS folder's project manifest from the commented template (the folder
//! IS the scope — `init` never walks up). It is the ONE way a project manifest is born: `add`
//! records rows in a file that already exists and refuses when none covers the folder, so the
//! file always lands where a person put it. With `-g` it creates the machine-wide file with its
//! header alone — no feed rows (`topos login` writes a workspace's row on this machine's first
//! connection, and a row someone deleted must never come back through a birth). Idempotent either
//! way — an existing file is a clean no-op receipt (`created: false`), never an overwrite.

use std::path::Path;

use topos_types::results::InitData;

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::manifest::MANIFEST_FILE;
use crate::manifest::document::{materialized_global, project_template};

use super::manifest_edit as medit;

/// Whether any directory from `cwd` upward holds a `.git` — the one thing the travel note needs to
/// know. A manifest inside a repo travels with it; one outside stays local to the folder.
fn inside_a_git_repo(fs: &dyn crate::fs_seam::FsOps, cwd: &Path) -> bool {
    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        if fs.exists(&d.join(".git")) {
            return true;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    false
}

/// Create the folder's `topos.toml`, or (with `global`) materialize `~/.topos/topos.toml`.
///
/// # Errors
/// [`ClientError::InvalidArgument`] when a project `init` has no working directory; an io failure.
pub(crate) fn init(ctx: &Ctx<'_>, global: bool) -> Result<InitData, ClientError> {
    if global {
        return init_global(ctx);
    }
    let cwd = ctx
        .roots
        .as_ref()
        .and_then(|r| r.cwd.clone())
        .ok_or_else(|| {
            ClientError::InvalidArgument(
                "cannot resolve the current directory — run `topos init` inside the folder the \
                 manifest should govern (or `topos init -g` for your machine-wide file)"
                    .into(),
            )
        })?;
    let path = cwd.join(MANIFEST_FILE);
    let travel = (!inside_a_git_repo(ctx.fs, &cwd)).then(|| {
        "outside a git repository — this manifest stays local to this folder (it won't travel \
         with a repo)"
            .to_owned()
    });
    // The same writer lock every manifest mutation takes — `init` is check-then-write, so a
    // concurrent `add`'s row must not be overwritten by the template.
    let _guard = medit::lock_manifest(ctx, &path)?;
    if ctx.fs.exists(&path) {
        return Ok(InitData {
            manifest: path.display().to_string(),
            created: false,
            note: travel,
        });
    }
    // The lock fences topos's own writers; an OUTSIDE editor can still land the file between the
    // check above and the write — the exclusive create meets it as an existing file (the same
    // clean no-op receipt, the outside bytes standing), never an overwrite.
    let created =
        match crate::atomic::atomic_write_new(ctx.fs, &path, project_template().as_bytes())? {
            crate::atomic::NewOutcome::Written => true,
            crate::atomic::NewOutcome::Exists => false,
        };
    // A FRESH file says what having one MEANS, which the lead's "record skills with `topos add`"
    // does not: this is now the file every project add here writes to, and the only one — an
    // `add` records rows, it never creates a manifest of its own.
    let created_note = "this is the file every `topos add` in this folder records into — `add` \
                        writes rows, never a new manifest";
    let note = match (created, travel) {
        (true, Some(t)) => Some(format!("{created_note} · {t}")),
        (true, None) => Some(created_note.to_owned()),
        (false, t) => t,
    };
    Ok(InitData {
        manifest: path.display().to_string(),
        created,
        note,
    })
}

/// The `-g` arm: create this machine's own recipe, header only (login writes the feed rows).
fn init_global(ctx: &Ctx<'_>) -> Result<InitData, ClientError> {
    let path = ctx.layout.home().join(MANIFEST_FILE);
    ctx.fs.create_dir_all(ctx.layout.home())?;
    let _guard = medit::lock_manifest(ctx, &path)?;
    if ctx.fs.exists(&path) {
        return Ok(InitData {
            manifest: path.display().to_string(),
            created: false,
            note: Some(
                "it already exists and was left untouched — `topos fmt -g` tidies it, and \
                 `topos list -g` shows what it currently delivers"
                    .to_owned(),
            ),
        });
    }
    let note = Some(
        "the file starts with just its header — `topos login` adds a workspace's feed row on \
         its first connection, and `topos add -g` records lines by hand"
            .to_owned(),
    );
    ctx.fs.create_dir_all(ctx.layout.home())?;
    // Same discipline as the project arm: the exclusive create meets a file an outside writer
    // landed after the check as an EXISTING file — the no-op receipt, never an overwrite.
    match crate::atomic::atomic_write_new(ctx.fs, &path, materialized_global(&[]).as_bytes())? {
        crate::atomic::NewOutcome::Written => Ok(InitData {
            manifest: path.display().to_string(),
            created: true,
            note,
        }),
        crate::atomic::NewOutcome::Exists => Ok(InitData {
            manifest: path.display().to_string(),
            created: false,
            note: Some(
                "it already exists and was left untouched — `topos fmt -g` tidies it, and \
                 `topos list -g` shows what it currently delivers"
                    .to_owned(),
            ),
        }),
    }
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
            progress: crate::progress::silent(),
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
            let first = init(ctx, false).unwrap();
            assert!(first.created);
            // A fresh file says what it is for — and, inside a repo, nothing about travel.
            let note = first
                .note
                .clone()
                .expect("a created file says what it is for");
            assert!(note.contains("every `topos add`"), "{note}");
            assert!(
                !note.contains("git"),
                "inside a repo — no travel note: {note}"
            );
            assert!(std::path::Path::new(&first.manifest).exists());
            // The created file parses as an EMPTY project manifest (the template is comments only).
            let text = std::fs::read_to_string(&first.manifest).unwrap();
            let doc = crate::manifest::document::parse_manifest(
                &text,
                crate::manifest::document::ManifestScope::Project,
            )
            .unwrap();
            assert!(doc.rows.is_empty());
            // Idempotent — the second run is a no-op receipt, never an overwrite.
            std::fs::write(
                &first.manifest,
                "# hand-edited\n[bundles]\n\"./a\" = \"*\"\n",
            )
            .unwrap();
            let second = init(ctx, false).unwrap();
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
            let out = init(ctx, false).unwrap();
            assert!(out.created);
            // Both disclosures ride one note: what the file is for, and that it won't travel.
            let note = out.note.clone().expect("a created file carries its note");
            assert!(note.contains("every `topos add`"), "{note}");
            assert!(note.contains("git"), "{note}");
            // The idempotent no-op keeps ONLY the travel note (nothing was created to explain).
            let again = init(ctx, false).unwrap();
            assert!(!again.created);
            let again_note = again.note.clone().expect("the travel note stands");
            assert!(again_note.contains("git"), "{again_note}");
            assert!(!again_note.contains("every `topos add`"), "{again_note}");
        });
    }

    #[test]
    fn init_without_a_working_directory_refuses_typed() {
        let home = scratch("nocwd");
        with_ctx(&home, None, |ctx| {
            let err = init(ctx, false).unwrap_err();
            assert_eq!(err.code(), "INVALID_ARGUMENT");
        });
    }

    #[test]
    fn a_file_appearing_between_the_check_and_the_land_is_never_overwritten() {
        // The racer is an OUTSIDE editor (topos's own writers serialize on the manifest lock):
        // it lands the file at the last catchable instant — after the absence check, with the
        // template already staged, immediately before the no-replace rename. The exclusive
        // create answers it as an existing file: the no-op receipt, the outside bytes standing.
        let home = scratch("race");
        let repo = home.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let path = repo.join(MANIFEST_FILE);
        let tmp = crate::atomic::temp_path(&path);
        let racing = path.clone();
        let hook_fs = crate::fs_seam::HookFs::before_first_move_of(&tmp, move || {
            std::fs::write(&racing, b"# an outside editor's file\n").unwrap();
        });
        let rfs = RealFs;
        let ids = RealIds;
        let clock = RealClock;
        let plane = InertPlane;
        let follow = InertFollow;
        let harness = ClaudeCode::new(scratch("race-adapter"), &rfs);
        let ctx = Ctx {
            progress: crate::progress::silent(),
            fs: &hook_fs,
            ids: &ids,
            clock: &clock,
            device_id: String::new(),
            layout: Layout::new(&home.join(".topos")),
            harness: &harness,
            plane: &plane,
            follow: &follow,
            roots: Some(AgentRoots {
                home: home.clone(),
                cwd: Some(repo.clone()),
            }),
        };
        let out = init(&ctx, false).unwrap();
        assert!(
            !out.created,
            "the racer's file is an existing file — the receipt is the no-op, never an overwrite"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"# an outside editor's file\n",
            "the outside bytes stand byte-for-byte"
        );
        assert!(!tmp.exists(), "the staged template was discarded");
    }

    #[test]
    fn init_g_materializes_the_machine_file_and_is_idempotent() {
        let home = scratch("global");
        with_ctx(&home, None, |ctx| {
            // With no session, the file is born with just its header — and says so.
            let first = init(ctx, true).unwrap();
            assert!(first.created);
            assert!(
                first.note.as_deref().is_some_and(|n| n.contains("header")),
                "{first:?}"
            );
            let text = std::fs::read_to_string(&first.manifest).unwrap();
            assert!(text.contains("# topos.toml"), "{text}");
            // Idempotent: the second run leaves the bytes alone and says so.
            let second = init(ctx, true).unwrap();
            assert!(!second.created);
            assert!(
                second
                    .note
                    .as_deref()
                    .is_some_and(|n| n.contains("left untouched")),
                "{second:?}"
            );
            assert_eq!(std::fs::read_to_string(&second.manifest).unwrap(), text);
        });
    }
}
