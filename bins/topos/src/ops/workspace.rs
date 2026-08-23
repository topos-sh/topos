//! `topos workspace` — which workspace ambient commands act on.
//!
//! The boring three-level rule (gcloud's shape): `--workspace` on the command wins, then the
//! `TOPOS_WORKSPACE` environment variable, then — inside a project — the file's `workspace = `
//! line, then the machine DEFAULT this verb manages. `list` shows every signed-in workspace with
//! a `*` on the default; `use <name>` moves the star. Login stars the first workspace a machine
//! joins, so a one-workspace machine never needs this verb at all.

use topos_types::results::{WorkspaceListData, WorkspaceListRow, WorkspaceUseData};

use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sessions;
use crate::sidecar::Layout;

/// `topos workspace list` — offline: the identity file is the whole answer.
///
/// # Errors
/// The identity file's own read failures.
pub(crate) fn list(fs: &dyn FsOps, layout: &Layout) -> Result<WorkspaceListData, ClientError> {
    let all = sessions::read_sessions(fs, layout)?;
    let default = all.default.clone();
    let workspaces = all
        .sessions
        .iter()
        .map(|s| {
            let address = format!("{}/{}", s.host, s.workspace_name);
            WorkspaceListRow {
                default: default.as_deref() == Some(address.as_str()),
                address,
                host: s.host.clone(),
                name: s.workspace_name.clone(),
                display_name: s.display_name.clone(),
                status: s.status.clone(),
            }
        })
        .collect();
    Ok(WorkspaceListData {
        workspaces,
        default,
    })
}

/// `topos workspace use <name>` — set the machine default.
///
/// # Errors
/// An unknown or ambiguous name, an ended session, the identity file's failures.
pub(crate) fn switch(
    fs: &dyn FsOps,
    layout: &Layout,
    name: &str,
) -> Result<WorkspaceUseData, ClientError> {
    let (previous, session) = sessions::set_default(fs, layout, name)?;
    let address = format!("{}/{}", session.host, session.workspace_name);
    Ok(WorkspaceUseData {
        previous: previous.filter(|p| *p != address),
        address,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;
    use crate::sidecar::Layout;

    fn scratch() -> (RealFs, Layout, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!("topos-wsverb-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        (RealFs, Layout::new(&home), home)
    }

    fn session(host: &str, name: &str) -> crate::sessions::Session {
        crate::sessions::Session {
            host: host.into(),
            base_url: format!("https://{host}"),
            workspace_id: format!("w_{name}"),
            workspace_name: name.into(),
            display_name: name.to_uppercase(),
            session_id: format!("sn_{name}"),
            credential: "secret".into(),
            status: crate::sessions::SESSION_ACTIVE.into(),
            logged_in_at: 1,
        }
    }

    #[test]
    fn login_stars_the_first_workspace_and_use_moves_the_star() {
        let (fs, layout, home) = scratch();
        crate::sessions::upsert_session(&fs, &layout, session("topos.sh", "acme")).unwrap();
        crate::sessions::upsert_session(&fs, &layout, session("topos.sh", "beta")).unwrap();

        let listed = list(&fs, &layout).unwrap();
        assert_eq!(listed.default.as_deref(), Some("topos.sh/acme"));
        assert_eq!(listed.workspaces.len(), 2);
        assert!(listed.workspaces[0].default, "sorted first, starred");

        let moved = switch(&fs, &layout, "beta").unwrap();
        assert_eq!(moved.address, "topos.sh/beta");
        assert_eq!(moved.previous.as_deref(), Some("topos.sh/acme"));
        let listed = list(&fs, &layout).unwrap();
        assert_eq!(listed.default.as_deref(), Some("topos.sh/beta"));

        // Setting the same default again reports no previous (nothing moved).
        let again = switch(&fs, &layout, "beta").unwrap();
        assert_eq!(again.previous, None);

        // An unknown name refuses with the roster.
        let e = switch(&fs, &layout, "nope").unwrap_err();
        assert!(format!("{e:?}").contains("acme"), "{e:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_default_steers_ambient_resolution() {
        let (fs, layout, home) = scratch();
        crate::sessions::upsert_session(&fs, &layout, session("topos.sh", "acme")).unwrap();
        crate::sessions::upsert_session(&fs, &layout, session("topos.sh", "beta")).unwrap();
        let all = crate::sessions::read_sessions(&fs, &layout).unwrap();
        // Two live sessions would refuse without a default; the star answers instead.
        let target = all.resolve_target(None).unwrap();
        assert_eq!(target.workspace_name, "acme");
        // An explicit choice still wins over the star.
        let target = all.resolve_target(Some("beta")).unwrap();
        assert_eq!(target.workspace_name, "beta");
        let _ = std::fs::remove_dir_all(&home);
    }
}
