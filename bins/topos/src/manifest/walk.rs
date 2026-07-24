//! Layer discovery — which manifests cover a working directory.
//!
//! `update` and `status` resolve FROM THE CURRENT DIRECTORY: walk up from cwd collecting every
//! folder that holds a `topos.toml` (nearest first — monorepo nesting), like git discovering
//! its repository or direnv its `.envrc` chain. There is NO machine registry of projects —
//! projects refresh lazily when agents visit them (staleness where nobody is looking is fine,
//! like an unfetched git clone).
//!
//! The personal manifest (`~/.topos/topos.toml`) is NOT part of this walk — it is its own,
//! broadest layer, read from the sidecar by the resolver's caller.

use std::path::{Path, PathBuf};

use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::manifest::file::{MANIFEST_FILE, Manifest, read_manifest};

/// One discovered project layer: the folder holding the manifest + its parsed content.
#[derive(Debug, Clone)]
pub(crate) struct ProjectLayer {
    /// The folder containing `topos.toml` — project-scope placement roots here.
    pub dir: PathBuf,
    pub manifest: Manifest,
}

/// Every project manifest covering `cwd`, nearest first. `home` (when known) is excluded from
/// the walk — a manifest AT the home directory would shadow the personal layer confusingly, so
/// the walk stops below it; the filesystem root otherwise ends it.
pub(crate) fn project_layers(
    fs: &dyn FsOps,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<Vec<ProjectLayer>, ClientError> {
    let mut layers = Vec::new();
    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        if home.is_some_and(|h| d == h) {
            break;
        }
        let candidate = d.join(MANIFEST_FILE);
        if let Some(manifest) = read_manifest(fs, &candidate)? {
            layers.push(ProjectLayer {
                dir: d.clone(),
                manifest,
            });
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    Ok(layers)
}

/// The folder a fresh manifest should be created in when `add` runs with none in reach: the
/// enclosing git repository's root when there is one (npm-init precedent — the manifest should
/// travel with the repo), else the working directory itself.
pub(crate) fn init_dir(fs: &dyn FsOps, cwd: &Path) -> PathBuf {
    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        if fs.exists(&d.join(".git")) {
            return d;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    cwd.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-walk-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn walks_up_nearest_first_and_stops_at_home() {
        let root = scratch("walk");
        let repo = root.join("repo");
        let nested = repo.join("services/api");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            repo.join(MANIFEST_FILE),
            "[skills]\n\"topos.sh/acme/repo-wide\" = \"*\"\n",
        )
        .unwrap();
        std::fs::write(
            nested.join(MANIFEST_FILE),
            "[skills]\n\"topos.sh/acme/api-only\" = \"*\"\n",
        )
        .unwrap();
        // A manifest at "home" itself must NOT appear (the personal layer is read separately).
        std::fs::write(
            root.join(MANIFEST_FILE),
            "[skills]\n\"topos.sh/acme/x\" = \"*\"\n",
        )
        .unwrap();

        let layers = project_layers(&RealFs, &nested, Some(&root)).unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].dir, nested);
        assert_eq!(
            layers[0].manifest.skills[0].reference,
            "topos.sh/acme/api-only"
        );
        assert_eq!(layers[1].dir, repo);
    }

    #[test]
    fn no_manifests_resolves_empty() {
        let root = scratch("none");
        let deep = root.join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        assert!(
            project_layers(&RealFs, &deep, Some(&root))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn init_dir_prefers_the_git_root() {
        let root = scratch("init");
        let repo = root.join("repo");
        let nested = repo.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert_eq!(init_dir(&RealFs, &nested), repo);
        // Outside any repo: the cwd itself.
        let stray = root.join("stray");
        std::fs::create_dir_all(&stray).unwrap();
        assert_eq!(init_dir(&RealFs, &stray), stray);
    }
}
