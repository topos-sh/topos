//! The MANIFEST half of `add` / `remove` / `init` — which `topos.toml` a verb edits, and the edit
//! itself. A scope IS a manifest: `add` records what a folder's agents should have, `remove` is its
//! inverse (recording an EXCLUDE line when a broader layer still provides the name — the one
//! negative state), and every receipt NAMES the manifest edited, first line, with the inverse.
//!
//! The manifest an edit lands in is the NEAREST `topos.toml` covering the working directory
//! (walking up, like git discovering its repository); with none in reach the edit creates one at
//! the enclosing git root (npm-init precedent — the manifest should travel with the repo), else the
//! working directory itself. The `-g` path-ref arm targets the LOCAL PERSONAL manifest
//! (`~/.topos/topos.toml`) instead — machine-local personal bundles with no workspace behind them.

use std::path::{Path, PathBuf};

use topos_types::results::{AddData, RemoveData, RemoveItem, RemoveKind};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::manifest::file::{MANIFEST_FILE, Manifest, ManifestEditor, read_manifest};
use crate::manifest::walk;

/// Where a manifest edit lands: the file, the folder it governs, and whether the edit creates it.
pub(crate) struct EditTarget {
    pub path: PathBuf,
    /// The folder the manifest governs (relative path entries resolve against it).
    pub dir: PathBuf,
    /// `true` when the file does not exist yet (the edit writes the init template first).
    pub created: bool,
}

/// Resolve the manifest an `add`/`remove` edits. `personal = true` (the `-g` path-ref arm) targets
/// `~/.topos/topos.toml`; otherwise the NEAREST project manifest covering the cwd, else a fresh one
/// at the enclosing git root (or the cwd itself). `None` when no working directory is known (no
/// machine roots — e.g. `$HOME` unset): the caller skips the manifest edit honestly rather than
/// guessing a folder.
pub(crate) fn edit_target(
    ctx: &Ctx<'_>,
    personal: bool,
) -> Result<Option<EditTarget>, ClientError> {
    if personal {
        let path = ctx.layout.home().join(MANIFEST_FILE);
        return Ok(Some(EditTarget {
            created: !ctx.fs.exists(&path),
            dir: ctx.layout.home().to_path_buf(),
            path,
        }));
    }
    let Some(roots) = &ctx.roots else {
        return Ok(None);
    };
    let Some(cwd) = roots.cwd.as_deref() else {
        return Ok(None);
    };
    let layers = walk::project_layers(ctx.fs, cwd, Some(&roots.home))?;
    if let Some(nearest) = layers.first() {
        return Ok(Some(EditTarget {
            path: nearest.dir.join(MANIFEST_FILE),
            dir: nearest.dir.clone(),
            created: false,
        }));
    }
    let dir = walk::init_dir(ctx.fs, cwd);
    Ok(Some(EditTarget {
        path: dir.join(MANIFEST_FILE),
        dir,
        created: true,
    }))
}

/// The manifest spelling of an adopted local path: relative to the manifest's folder when the
/// source sits under it (`./tools/my-skill` — the committed, travels-with-the-repo form), else the
/// absolute path (an out-of-tree source is machine-local by nature).
pub(crate) fn path_reference(manifest_dir: &Path, source_abs: &Path) -> String {
    match source_abs.strip_prefix(manifest_dir) {
        Ok(rel) => format!("./{}", rel.display()),
        Err(_) => source_abs.display().to_string(),
    }
}

/// Record an include line for a just-adopted skill and stamp the receipt: `data.manifest` names the
/// file edited, `data.reference` the stored reference, `data.undo` the paste-ready inverse. An
/// existing exclude of the same reference/name is LIFTED (adding back is the exclude's inverse).
/// Best-effort by contract of the caller: the adoption already landed, so a manifest-write failure
/// surfaces as the error it is (nothing is rolled back — the adopt is real either way).
pub(crate) fn note_added(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    reference: &str,
    pin: Option<&str>,
    personal: bool,
) -> Result<(), ClientError> {
    note_added_table(ctx, data, "skills", reference, pin, personal)
}

/// [`note_added`] with the include TABLE chosen (`skills` / `channels` — a channel reference
/// records in its own table).
pub(crate) fn note_added_table(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    table: &str,
    reference: &str,
    pin: Option<&str>,
    personal: bool,
) -> Result<(), ClientError> {
    let Some(target) = edit_target(ctx, personal)? else {
        return Ok(());
    };
    note_added_at(ctx, data, table, &target, reference, pin)
}

/// [`note_added`] with the target already resolved (the path arm resolves it first to compute the
/// dir-relative spelling).
fn note_added_at(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    table: &str,
    target: &EditTarget,
    reference: &str,
    pin: Option<&str>,
) -> Result<(), ClientError> {
    if target.created {
        // Seed the fresh file with the init template's header so a created manifest self-describes
        // (the personal manifest's `~/.topos/` may not exist yet on a fresh install).
        if let Some(parent) = target.path.parent() {
            ctx.fs.create_dir_all(parent)?;
        }
        crate::atomic::atomic_write(
            ctx.fs,
            &target.path,
            ManifestEditor::init_template().as_bytes(),
        )?;
    }
    let mut ed = ManifestEditor::open(ctx.fs, &target.path)?;
    // Adding back lifts a standing exclude — by the full reference and by the bare name (both
    // spellings claim the same name at resolution).
    ed.remove_exclude(reference);
    let name = crate::manifest::refs::parse_ref(reference)
        .map(|p| p.item_name().to_owned())
        .unwrap_or_else(|_| reference.to_owned());
    ed.remove_exclude(&name);
    ed.set_entry(table, reference, pin);
    ed.write(ctx.fs, &target.path)?;
    data.manifest = Some(target.path.display().to_string());
    data.reference = Some(reference.to_owned());
    data.undo = vec![
        "topos".to_owned(),
        "remove".to_owned(),
        reference.to_owned(),
    ];
    Ok(())
}

/// Record a PATH-adopted skill in the right manifest: the project manifest with the dir-relative
/// spelling when the source sits inside its folder (the committed, travels-with-the-repo form);
/// else — an out-of-tree source, or an explicit `-g` — the LOCAL PERSONAL manifest with the
/// absolute path (machine-local by nature; no workspace roams it).
pub(crate) fn note_added_path(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    source: &Path,
    personal: bool,
) -> Result<(), ClientError> {
    // Canonicalize best-effort (symlinks resolve; a vanished dir keeps the typed spelling).
    let source_abs = source.canonicalize().unwrap_or_else(|_| {
        if source.is_absolute() {
            source.to_path_buf()
        } else {
            ctx.roots
                .as_ref()
                .and_then(|r| r.cwd.as_ref())
                .map(|c| c.join(source))
                .unwrap_or_else(|| source.to_path_buf())
        }
    });
    if !personal
        && let Some(target) = edit_target(ctx, false)?
        && source_abs.starts_with(&target.dir)
    {
        let reference = path_reference(&target.dir, &source_abs);
        return note_added_at(ctx, data, "skills", &target, &reference, None);
    }
    let reference = source_abs.display().to_string();
    note_added(ctx, data, &reference, None, true)
}

/// Record a REMOTE-imported skill: the canonical GitHub reference (host/owner/repo[/subdir]) —
/// PINNED to the resolved commit when the archive disclosed one (lockfile logic: no governance
/// rail sits behind an external origin). `-g` (a home-scoped landing) records in the personal
/// manifest; a project landing records in the project manifest.
pub(crate) fn note_added_remote(
    ctx: &Ctx<'_>,
    data: &mut AddData,
    personal: bool,
) -> Result<(), ClientError> {
    let Some(origin) = data.origin.clone() else {
        return Ok(());
    };
    let reference = match origin.subdir.as_deref() {
        Some(sub) if !sub.is_empty() => format!("{}/{sub}", origin.source),
        _ => origin.source.clone(),
    };
    // A pin must be commit-shaped (7–40 hex) — the same rule the grammar applies to the entry value.
    let pin = origin
        .commit
        .as_deref()
        .filter(|c| (7..=40).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_hexdigit()));
    note_added(ctx, data, &reference, pin, personal)
}

/// One manifest layer as the `remove` arm reads it (path + parsed content), nearest first, the
/// personal manifest LAST (the broadest local layer).
pub(super) fn local_layers(ctx: &Ctx<'_>) -> Result<Vec<(PathBuf, Manifest)>, ClientError> {
    let mut out = Vec::new();
    if let Some(roots) = &ctx.roots
        && let Some(cwd) = roots.cwd.as_deref()
    {
        for layer in walk::project_layers(ctx.fs, cwd, Some(&roots.home))? {
            out.push((layer.dir.join(MANIFEST_FILE), layer.manifest));
        }
    }
    let personal = ctx.layout.home().join(MANIFEST_FILE);
    if let Some(m) = read_manifest(ctx.fs, &personal)? {
        out.push((personal, m));
    }
    Ok(out)
}

/// Whether `token` names `entry_ref` — by the exact reference, or by the reference's last-segment
/// NAME (the dedupe key both `resolve_layers` and the exclude matching use).
fn token_matches(token: &str, entry_ref: &str) -> bool {
    if token == entry_ref {
        return true;
    }
    let entry_name = entry_ref.trim_end_matches('/').rsplit('/').next();
    entry_name == Some(token)
}

/// The manifest arm of `topos remove`: when EVERY token names a manifest entry (or a name a broader
/// local layer provides), edit the NEAREST manifest — delete its own include line, and record an
/// EXCLUDE line when a broader layer still provides the name. Returns `None` when no token matches
/// any manifest line (the caller falls through to the tracked/untracked removal). Immediate — a
/// manifest edit is reversible (`--yes` an accepted no-op); the receipt names the manifest first.
///
/// # Errors
/// [`ClientError::InvalidArgument`] when SOME tokens match manifests and some do not (a mixed batch
/// would half-apply); a manifest read/write failure.
pub(crate) fn remove_from_manifests(
    ctx: &Ctx<'_>,
    tokens: &[String],
    profile_provided: &[(String, String)],
) -> Result<Option<RemoveData>, ClientError> {
    let layers = local_layers(ctx)?;
    if tokens.is_empty() {
        return Ok(None);
    }
    // Which tokens a local layer's include lines mention, and which the PROFILE delivers (the
    // broader person layer — `(name, canonical)` pairs from the offline delivery cache).
    let mentioned = |token: &str| {
        layers.iter().any(|(_, m)| {
            m.skills
                .iter()
                .chain(m.channels.iter())
                .any(|e| token_matches(token, &e.reference))
        })
    };
    let provided = |token: &str| {
        profile_provided
            .iter()
            .find(|(name, canonical)| name == token || canonical == token)
    };
    let hits: Vec<bool> = tokens
        .iter()
        .map(|t| mentioned(t) || provided(t).is_some())
        .collect();
    if hits.iter().all(|h| !h) {
        return Ok(None);
    }
    if !hits.iter().all(|h| *h) {
        return Err(ClientError::InvalidArgument(
            "some targets are manifest entries and some are not — remove them in separate \
             invocations"
                .into(),
        ));
    }

    // The NEAREST manifest takes the edit; with none in reach (a profile-provided name removed
    // from a bare folder) one is created at the git root — the exclude needs a manifest to live in.
    let (nearest_path, created) = match layers.first() {
        Some((path, _)) => (path.clone(), false),
        None => {
            let Some(target) = edit_target(ctx, false)? else {
                return Ok(None);
            };
            if target.created {
                if let Some(parent) = target.path.parent() {
                    ctx.fs.create_dir_all(parent)?;
                }
                crate::atomic::atomic_write(
                    ctx.fs,
                    &target.path,
                    ManifestEditor::init_template().as_bytes(),
                )?;
            }
            (target.path, true)
        }
    };
    let mut ed = ManifestEditor::open(ctx.fs, &nearest_path)?;
    let mut items = Vec::with_capacity(tokens.len());
    let mut undo = Vec::new();
    for token in tokens {
        // The entry the token names, searched nearest-first (the nearest mention wins) — else the
        // profile-provided pair.
        let local = layers.iter().enumerate().find_map(|(i, (_, m))| {
            m.skills
                .iter()
                .find(|e| token_matches(token, &e.reference))
                .map(|e| (i, "skills", e.reference.clone()))
                .or_else(|| {
                    m.channels
                        .iter()
                        .find(|e| token_matches(token, &e.reference))
                        .map(|e| (i, "channels", e.reference.clone()))
                })
        });
        let (layer_idx, table, entry_ref, profile_only) = match local {
            Some((i, table, entry_ref)) => (i, table, entry_ref, false),
            None => {
                let (_, canonical) = provided(token).expect("every token matched above");
                (usize::MAX, "skills", canonical.clone(), true)
            }
        };
        let name = entry_ref
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(entry_ref.as_str())
            .to_owned();
        // Does any BROADER layer than the nearest still provide the name — a broader local
        // manifest, or the person's profile itself?
        let broader_provides = layers.iter().skip(1).any(|(_, m)| {
            m.skills
                .iter()
                .chain(m.channels.iter())
                .any(|e| token_matches(&name, &e.reference))
        }) || provided(&name).is_some();
        let removed_here =
            !profile_only && layer_idx == 0 && !created && ed.remove_entry(table, &entry_ref);
        let kind = if broader_provides || !removed_here {
            // A broader layer provides it (or the nearest never carried the line): the one
            // negative state — an exclude line in the nearest manifest.
            ed.add_exclude(&entry_ref);
            RemoveKind::ManifestExcluded
        } else {
            RemoveKind::ManifestRemoved
        };
        undo = vec!["topos".to_owned(), "add".to_owned(), entry_ref.clone()];
        items.push(RemoveItem {
            name,
            kind,
            manifest: Some(nearest_path.display().to_string()),
            workspace_id: None,
            agent_dirs: Vec::new(),
            bytes_kept: true,
            note: profile_only.then(|| {
                format!(
                    "your profile still delivers it elsewhere — `topos remove -g {entry_ref}` \
                     stops it everywhere"
                )
            }),
        });
    }
    ed.write(ctx.fs, &nearest_path)?;
    Ok(Some(RemoveData {
        items,
        applied: true,
        undo,
    }))
}

/// The `(name, canonical)` pairs the person's profile currently delivers, from the OFFLINE
/// delivery cache — the "broader person layer" `remove`'s exclude semantics consult.
pub(crate) fn profile_provided_names(ctx: &Ctx<'_>) -> Vec<(String, String)> {
    let status = crate::sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();
    let mut out = Vec::new();
    for entry in status.workspaces.values() {
        let (Some(host), Some(ws)) = (entry.host.as_deref(), entry.workspace_name.as_deref())
        else {
            continue;
        };
        for ds in entry.delivered.values() {
            if !ds.withdrawn && !ds.name.is_empty() {
                out.push((ds.name.clone(), format!("{host}/{ws}/{}", ds.name)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::AgentRoots;
    use crate::fs_seam::RealFs;
    use crate::ids::{RealClock, RealIds};
    use crate::plane::{InertFollow, InertPlane};
    use crate::sidecar::Layout;
    use topos_harness::ClaudeCode;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("topos-medit-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A ctx whose machine roots point at `home`/`cwd` (the manifest walk's inputs).
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
            roots: Some(AgentRoots {
                home: home.to_path_buf(),
                cwd: cwd.map(Path::to_path_buf),
            }),
        };
        f(&ctx)
    }

    fn add_data() -> AddData {
        AddData {
            skill_id: "topos_x".into(),
            name: "my-skill".into(),
            version_id: "0".repeat(64),
            bundle_digest: "0".repeat(64),
            tracked: true,
            harness: None,
            harness_slug: None,
            currency: None,
            triggers: Vec::new(),
            origin: None,
            manifest: None,
            reference: None,
            undo: Vec::new(),
            governed_copy: None,
        }
    }

    #[test]
    fn add_creates_the_manifest_at_the_git_root_and_remove_inverts_it() {
        let home = scratch("add-rt");
        let repo = home.join("repo");
        let nested = repo.join("services/api");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        with_ctx(&home, Some(&nested), |ctx| {
            let mut data = add_data();
            note_added(ctx, &mut data, "./tools/my-skill", None, false).unwrap();
            // The manifest was created at the GIT ROOT (npm-init precedent), not the cwd.
            let manifest = repo.join(MANIFEST_FILE);
            assert_eq!(
                data.manifest.as_deref(),
                Some(&*manifest.display().to_string())
            );
            assert_eq!(data.reference.as_deref(), Some("./tools/my-skill"));
            assert_eq!(data.undo, vec!["topos", "remove", "./tools/my-skill"]);
            let m = read_manifest(ctx.fs, &manifest).unwrap().unwrap();
            assert_eq!(m.skills.len(), 1);
            assert_eq!(m.skills[0].reference, "./tools/my-skill");

            // Remove edits the SAME (nearest) manifest and deletes the line.
            let out = remove_from_manifests(ctx, &["./tools/my-skill".to_owned()], &[])
                .unwrap()
                .expect("the manifest arm claims it");
            assert!(out.applied);
            assert_eq!(out.items.len(), 1);
            assert!(matches!(out.items[0].kind, RemoveKind::ManifestRemoved));
            assert_eq!(
                out.items[0].manifest.as_deref(),
                Some(&*manifest.display().to_string())
            );
            assert_eq!(out.undo, vec!["topos", "add", "./tools/my-skill"]);
            let m = read_manifest(ctx.fs, &manifest).unwrap().unwrap();
            assert!(m.skills.is_empty() && m.exclude.is_empty());
        });
    }

    #[test]
    fn remove_by_bare_name_records_an_exclude_when_a_broader_layer_provides() {
        let home = scratch("excl");
        let repo = home.join("repo");
        let nested = repo.join("api");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            repo.join(MANIFEST_FILE),
            "[skills]\n\"topos.sh/acme/noisy\" = \"*\"\n",
        )
        .unwrap();
        std::fs::write(
            nested.join(MANIFEST_FILE),
            "[skills]\n\"topos.sh/acme/api\" = \"*\"\n",
        )
        .unwrap();

        with_ctx(&home, Some(&nested), |ctx| {
            // "noisy" is provided by the BROADER repo manifest; the nearest (api) manifest takes
            // the exclude line — the one negative state.
            let out = remove_from_manifests(ctx, &["noisy".to_owned()], &[])
                .unwrap()
                .expect("claimed");
            assert!(matches!(out.items[0].kind, RemoveKind::ManifestExcluded));
            let near = read_manifest(ctx.fs, &nested.join(MANIFEST_FILE))
                .unwrap()
                .unwrap();
            assert_eq!(near.exclude, vec!["topos.sh/acme/noisy".to_owned()]);
            // The broader line is untouched (the exclude shadows it; nothing else was edited).
            let broad = read_manifest(ctx.fs, &repo.join(MANIFEST_FILE))
                .unwrap()
                .unwrap();
            assert_eq!(broad.skills.len(), 1);

            // Adding it back LIFTS the exclude (the inverse the receipt named).
            let mut data = add_data();
            note_added(ctx, &mut data, "topos.sh/acme/noisy", None, false).unwrap();
            let near = read_manifest(ctx.fs, &nested.join(MANIFEST_FILE))
                .unwrap()
                .unwrap();
            assert!(near.exclude.is_empty());
            assert!(
                near.skills
                    .iter()
                    .any(|e| e.reference == "topos.sh/acme/noisy")
            );
        });
    }

    #[test]
    fn unmatched_tokens_fall_through_and_mixed_batches_refuse() {
        let home = scratch("fall");
        let cwd = home.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        with_ctx(&home, Some(&cwd), |ctx| {
            // No manifests at all → None (the classic removal path owns the token).
            assert!(
                remove_from_manifests(ctx, &["docs".to_owned()], &[])
                    .unwrap()
                    .is_none()
            );
        });
        std::fs::write(cwd.join(MANIFEST_FILE), "[skills]\n\"./a\" = \"*\"\n").unwrap();
        with_ctx(&home, Some(&cwd), |ctx| {
            assert!(
                remove_from_manifests(ctx, &["docs".to_owned()], &[])
                    .unwrap()
                    .is_none()
            );
            let err = remove_from_manifests(ctx, &["./a".to_owned(), "docs".to_owned()], &[])
                .unwrap_err();
            assert_eq!(err.code(), "INVALID_ARGUMENT");
        });
    }

    #[test]
    fn the_g_flag_targets_the_personal_manifest() {
        let home = scratch("perso");
        with_ctx(&home, None, |ctx| {
            let mut data = add_data();
            let abs = home.join("skills/my-skill").display().to_string();
            note_added(ctx, &mut data, &abs, None, true).unwrap();
            let personal = ctx.layout.home().join(MANIFEST_FILE);
            assert_eq!(
                data.manifest.as_deref(),
                Some(&*personal.display().to_string())
            );
            let m = read_manifest(ctx.fs, &personal).unwrap().unwrap();
            assert_eq!(m.skills.len(), 1);
        });
    }

    #[test]
    fn path_reference_is_relative_inside_the_manifest_dir_absolute_outside() {
        let dir = PathBuf::from("/repo");
        assert_eq!(
            path_reference(&dir, Path::new("/repo/tools/my-skill")),
            "./tools/my-skill"
        );
        assert_eq!(
            path_reference(&dir, Path::new("/elsewhere/x")),
            "/elsewhere/x"
        );
    }

    #[test]
    fn no_machine_roots_skips_the_project_manifest_edit_honestly() {
        let home = scratch("noroots");
        let fs = RealFs;
        let ids = RealIds;
        let clock = RealClock;
        let plane = InertPlane;
        let follow = InertFollow;
        let harness = ClaudeCode::new(scratch("adapter2"), &fs);
        let ctx = Ctx {
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: String::new(),
            layout: Layout::new(&home.join(".topos")),
            harness: &harness,
            plane: &plane,
            follow: &follow,
            roots: None,
        };
        assert!(edit_target(&ctx, false).unwrap().is_none());
        // The personal target resolves regardless (it lives in the sidecar, not the machine roots).
        assert!(edit_target(&ctx, true).unwrap().is_some());
    }
}
