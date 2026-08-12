//! `fmt [-g]` — rewrite a manifest into the NORMAL FORM: the same rows, grouped and sorted the
//! one deterministic way, comments carried with what they annotate. Meaning never changes, so the
//! verb is safe to run on a file someone else wrote — and it validates the WHOLE document first,
//! so it can never launder a malformed manifest into a tidy one.
//!
//! The write is ATOMIC and happens only when the bytes actually move (`changed: false` is the
//! honest no-op receipt on an already-normal file).

use topos_types::results::FmtData;

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::manifest::MANIFEST_FILE;
use crate::manifest::document::ManifestScope;
use crate::manifest::normal::fmt_normal;
use crate::manifest::scopes;

/// Format the machine-wide file (`global`) or the nearest project manifest covering the working
/// directory.
///
/// # Errors
/// [`ClientError::InvalidArgument`] when no manifest covers the directory (naming the verb that
/// creates one); the typed manifest refusal ([`ClientError::ManifestInvalid`]) when the file does
/// not parse (refused exactly as the reader refuses it); an io failure.
pub(crate) fn fmt_manifest(ctx: &Ctx<'_>, global: bool) -> Result<FmtData, ClientError> {
    let (path, scope) = if global {
        (ctx.layout.home().join(MANIFEST_FILE), ManifestScope::Global)
    } else {
        let cwd = ctx
            .roots
            .as_ref()
            .and_then(|r| r.cwd.clone())
            .ok_or_else(|| {
                ClientError::InvalidArgument(
                    "cannot resolve the current directory — run `topos fmt` inside the folder \
                     whose manifest should be formatted, or `topos fmt -g` for your machine-wide \
                     file"
                        .into(),
                )
            })?;
        let home = ctx.roots.as_ref().map(|r| r.home.clone());
        let dir = scopes::nearest_manifest_dir(ctx.fs, &cwd, home.as_deref()).ok_or_else(|| {
            ClientError::InvalidArgument(
                "no topos.toml covers this directory — `topos init` creates one".into(),
            )
        })?;
        (dir.join(MANIFEST_FILE), ManifestScope::Project)
    };

    // `fmt` is a read-modify-write like every other manifest mutation: it takes the same writer
    // lock, so a concurrent `add`'s row can never be normalized away by a document read before it.
    let _guard = super::manifest_edit::lock_manifest(ctx, &path)?;
    let Some(bytes) = ctx.fs.read_opt(&path)? else {
        return Err(ClientError::InvalidArgument(if global {
            "no machine-wide manifest — `topos init -g` materializes one from the workspaces you \
             are connected to"
                .to_owned()
        } else {
            "no topos.toml covers this directory — `topos init` creates one".to_owned()
        }));
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| ClientError::ManifestInvalid(format!("{}: not UTF-8", path.display())))?;
    // The same MANIFEST-family classification every reader uses: a grammar fault is the file's
    // own refusal — never corrupt-state, which blames topos for a hand-edit.
    let formatted = fmt_normal(&text, scope)
        .map_err(|e| ClientError::ManifestInvalid(format!("{}: {e}", path.display())))?;
    let changed = formatted != text;
    if changed {
        // The same compare-and-swap every editor write rides: the lock fences topos's own
        // writers, and the pre-rename re-read refuses an outside editor's bytes rather than
        // normalizing over them.
        match crate::atomic::atomic_write_cas(
            ctx.fs,
            &path,
            formatted.as_bytes(),
            Some(text.as_bytes()),
        )? {
            crate::atomic::CasOutcome::Written => {}
            crate::atomic::CasOutcome::Changed => {
                return Err(ClientError::ManifestChanged {
                    path: path.display().to_string(),
                });
            }
        }
    }
    Ok(FmtData {
        manifest: path.display().to_string(),
        changed,
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
        let dir = std::env::temp_dir().join(format!("topos-fmt-{tag}-{}-{n}", std::process::id()));
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
        let harness = ClaudeCode::new(scratch("adapter"));
        let ctx = Ctx {
            progress: crate::progress::silent(),
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: String::new(),
            layout: Layout::new(&home.join(".topos")),
            harness: &harness,
            triggers: crate::ops::Triggers::active_only(&crate::ops::INERT_TRIGGER),
            plane: &plane,
            follow: &follow,
            roots: Some(AgentRoots {
                home: home.to_path_buf(),
                cwd: cwd.map(Path::to_path_buf),
            }),
        };
        f(&ctx)
    }

    #[test]
    fn fmt_normalizes_then_is_a_no_op() {
        let home = scratch("proj");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let manifest = repo.join(MANIFEST_FILE);
        std::fs::write(
            &manifest,
            "[bundles]\n\"topos.sh/acme/deploy\" = \"*\"\n\"github.com/o/r\" = \"*\"\n",
        )
        .unwrap();
        with_ctx(&home, Some(&repo), |ctx| {
            let first = fmt_manifest(ctx, false).unwrap();
            assert!(first.changed, "the scrambled file moves");
            assert_eq!(first.manifest, manifest.display().to_string());
            let text = std::fs::read_to_string(&manifest).unwrap();
            assert!(text.contains("[bundles.\"topos.sh/acme\"]"), "{text}");
            // Idempotent — a normal file is an honest no-op.
            let second = fmt_manifest(ctx, false).unwrap();
            assert!(!second.changed);
            assert_eq!(std::fs::read_to_string(&manifest).unwrap(), text);
        });
    }

    #[test]
    fn fmt_refuses_a_missing_file_and_a_malformed_one() {
        let home = scratch("missing");
        let bare = home.join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        with_ctx(&home, Some(&bare), |ctx| {
            let err = fmt_manifest(ctx, false).unwrap_err();
            assert_eq!(err.code(), "INVALID_ARGUMENT");
            assert!(err.to_string().contains("topos init"), "{err}");
            let err = fmt_manifest(ctx, true).unwrap_err();
            assert!(err.to_string().contains("topos init -g"), "{err}");
        });
        // A file that does not parse refuses AS ITSELF — never silently rewritten, and never
        // blamed on topos's own state: the grammar refusal is the MANIFEST family, verbatim.
        let home = scratch("bad");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join(MANIFEST_FILE), "[stray]\nx = 1\n").unwrap();
        with_ctx(&home, Some(&repo), |ctx| {
            let err = fmt_manifest(ctx, false).unwrap_err();
            assert_eq!(err.code(), "MANIFEST_INVALID");
            assert!(err.to_string().contains("unknown top-level"), "{err}");
            assert!(
                crate::render::safe_message(&err).contains("unknown top-level"),
                "the teaching reaches the user surfaces verbatim: {err}"
            );
        });
        // A stale placement spelling refuses through `fmt` the same way a reader refuses it.
        let home = scratch("stale");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join(MANIFEST_FILE),
            "[bundles]\n\"./tools/x\" = { path = \".claude/skills\" }\n",
        )
        .unwrap();
        with_ctx(&home, Some(&repo), |ctx| {
            let err = fmt_manifest(ctx, false).unwrap_err();
            assert_eq!(err.code(), "MANIFEST_INVALID");
            assert!(err.to_string().contains("unknown field `path`"), "{err}");
        });
    }

    #[test]
    fn fmt_g_targets_the_machine_file() {
        let home = scratch("global");
        with_ctx(&home, None, |ctx| {
            std::fs::create_dir_all(ctx.layout.home()).unwrap();
            let path = ctx.layout.home().join(MANIFEST_FILE);
            std::fs::write(&path, "[bundles]\n\"topos.sh/acme\" = \"*\"\n").unwrap();
            let out = fmt_manifest(ctx, true).unwrap();
            assert_eq!(out.manifest, path.display().to_string());
            assert!(!out.changed, "already normal");
        });
    }
}
