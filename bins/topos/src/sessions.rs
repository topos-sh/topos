//! `identity/sessions.json` — this installation's SESSIONS: one per workspace, each carrying
//! its own WORKSPACE-SCOPED bearer credential (a session = user × workspace × installation;
//! `topos login <workspace-address>` mints one, `topos logout` ends one). A `0600` secret —
//! every credential in it authenticates as its person in its one workspace.
//!
//! This replaces the retired device model's three documents (the global `credentials.json`,
//! the membership half of `user.json`, and `follows.json`'s standing subscriptions): a session
//! row IS the membership record, and demand lives in manifests, not here.

use serde::{Deserialize, Serialize};
use topos_types::PERSISTED_SCHEMA_VERSION;

use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// A session's local status mirror. `pending` = awaiting an owner's approval (the workspace's
/// session-approval knob) — no data flows; `ended` is LOCAL-only: the server answered the
/// uniform 404 (revoked, rejected, or the workspace is gone) — recorded so the one typed line
/// prints once and the sweeps stop dialing; `topos login` replaces it.
pub(crate) const SESSION_ACTIVE: &str = "active";
pub(crate) const SESSION_PENDING: &str = "pending";
pub(crate) const SESSION_ENDED: &str = "ended";

/// One session: this installation logged into one workspace.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Session {
    /// The server host the workspace lives on (`topos.sh`, `topos.example.com`) — the manifest
    /// grammar's host half.
    pub host: String,
    /// The API base the transport dials (the protocol card's declared base).
    pub base_url: String,
    /// The workspace's opaque id (the wire/URL-path key).
    pub workspace_id: String,
    /// The workspace's ADDRESS slug (what a human types; the manifest grammar's middle).
    pub workspace_name: String,
    /// The workspace's display name, for receipts.
    pub display_name: String,
    /// The server-minted session id (`sn_…`) — non-secret; the sessions pages show it.
    pub session_id: String,
    /// **SECRET** — the session's bearer credential. Redacted in `Debug`.
    pub credential: String,
    /// [`SESSION_ACTIVE`] / [`SESSION_PENDING`] / [`SESSION_ENDED`].
    pub status: String,
    /// When the login completed, epoch-millis.
    pub logged_in_at: i64,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("host", &self.host)
            .field("workspace_id", &self.workspace_id)
            .field("workspace_name", &self.workspace_name)
            .field("session_id", &self.session_id)
            .field("credential", &"<redacted>")
            .field("status", &self.status)
            .finish()
    }
}

/// The whole document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Sessions {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub sessions: Vec<Session>,
}

impl Sessions {
    /// The sessions the network fan-outs dial: active + pending (a pending session's delivery
    /// answers typed), never ended.
    pub(crate) fn live(&self) -> impl Iterator<Item = &Session> {
        self.sessions.iter().filter(|s| s.status != SESSION_ENDED)
    }

    /// Find by workspace NAME or opaque id (the `--workspace` selector's grammar).
    pub(crate) fn find(&self, workspace: &str) -> Option<&Session> {
        self.sessions
            .iter()
            .find(|s| s.workspace_id == workspace)
            .or_else(|| self.sessions.iter().find(|s| s.workspace_name == workspace))
    }

    /// The session a HOST + workspace-name pair names (the canonical-ref lookup).
    pub(crate) fn find_on_host(&self, host: &str, workspace_name: &str) -> Option<&Session> {
        self.sessions
            .iter()
            .find(|s| s.host == host && s.workspace_name == workspace_name)
    }

    /// The one session an ambient act targets: exactly one live session, or a typed choice.
    ///
    /// # Errors
    /// [`ClientError::Enrollment`] with the login hint when there are none;
    /// [`ClientError::WorkspaceSelection`] naming the joined workspaces when there are several.
    pub(crate) fn resolve_target(&self, explicit: Option<&str>) -> Result<&Session, ClientError> {
        if let Some(ws) = explicit {
            let found = self.find(ws).ok_or_else(|| {
                ClientError::WorkspaceSelection(format!(
                    "not logged into workspace '{ws}'; logged-in workspaces: {}",
                    self.names().join(", ")
                ))
            })?;
            // An explicitly named workspace whose session ENDED refuses toward `login` — a dead
            // credential never rides a write.
            if found.status == SESSION_ENDED {
                return Err(ClientError::Enrollment(format!(
                    "the session for '{ws}' has ended — reconnect with `topos login {}/{}`",
                    found.host, found.workspace_name
                )));
            }
            return Ok(found);
        }
        let live: Vec<&Session> = self.live().collect();
        match live.as_slice() {
            [] => Err(ClientError::Enrollment(
                "not logged into any workspace; run `topos login <workspace-address>` first".into(),
            )),
            [only] => Ok(only),
            _ => Err(ClientError::WorkspaceSelection(format!(
                "logged into multiple workspaces ({}); pass `--workspace <name>` to choose one",
                self.names().join(", ")
            ))),
        }
    }

    fn names(&self) -> Vec<String> {
        self.sessions
            .iter()
            .map(|s| s.workspace_name.clone())
            .collect()
    }
}

/// Canonicalize the global `--workspace` flag: the ADDRESS name → the session's workspace id
/// (best-effort — an unknown name passes through for the downstream selection error to name).
pub(crate) fn canonicalize_workspace_flag(
    fs: &dyn FsOps,
    layout: &Layout,
    flag: Option<String>,
) -> Option<String> {
    let flag = flag?;
    let all = read_sessions(fs, layout).unwrap_or_default();
    Some(
        all.sessions
            .iter()
            .find(|s| s.workspace_name == flag)
            .map(|s| s.workspace_id.clone())
            .unwrap_or(flag),
    )
}

/// Fold an email/principal to its canonical ASCII-lowercase form (the invite wire's one identity
/// per human).
pub(crate) fn canonical_principal(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

/// Read `identity/sessions.json` (a `0600` secret). Absent = signed into nothing.
pub(crate) fn read_sessions(fs: &dyn FsOps, layout: &Layout) -> Result<Sessions, ClientError> {
    Ok(doc::read_doc_private(fs, &layout.sessions_path())?.unwrap_or_default())
}

/// Upsert ONE session (keyed by host + workspace id) under the identity lock — a re-login
/// replaces the workspace's credential wholesale.
pub(crate) fn upsert_session(
    fs: &dyn FsOps,
    layout: &Layout,
    session: Session,
) -> Result<(), ClientError> {
    let _guard = fs.lock_exclusive(&layout.identity_lock_file())?;
    let mut all = read_sessions(fs, layout)?;
    all.schema_version = PERSISTED_SCHEMA_VERSION;
    all.sessions
        .retain(|s| !(s.host == session.host && s.workspace_id == session.workspace_id));
    all.sessions.push(session);
    all.sessions.sort_by(|a, b| {
        (a.host.as_str(), a.workspace_name.as_str())
            .cmp(&(b.host.as_str(), b.workspace_name.as_str()))
    });
    write_sessions_locked(fs, layout, &all)
}

/// Flip one session's LOCAL status (active↔pending, or the local `ended` mark), keyed by the
/// SAME composite identity the upsert uses — (host, workspace id) — so a same-id workspace on
/// another server is never touched. No-op when the session is unknown.
pub(crate) fn set_session_status(
    fs: &dyn FsOps,
    layout: &Layout,
    host: &str,
    workspace_id: &str,
    status: &str,
) -> Result<(), ClientError> {
    let _guard = fs.lock_exclusive(&layout.identity_lock_file())?;
    let mut all = read_sessions(fs, layout)?;
    let mut touched = false;
    for s in &mut all.sessions {
        if s.host == host && s.workspace_id == workspace_id && s.status != status {
            s.status = status.to_string();
            touched = true;
        }
    }
    if touched {
        write_sessions_locked(fs, layout, &all)?;
    }
    Ok(())
}

/// Delete one session's row (a settled logout) — the (host, workspace id) composite, like
/// every mutation here. No-op when unknown.
pub(crate) fn remove_session(
    fs: &dyn FsOps,
    layout: &Layout,
    host: &str,
    workspace_id: &str,
) -> Result<(), ClientError> {
    let _guard = fs.lock_exclusive(&layout.identity_lock_file())?;
    let mut all = read_sessions(fs, layout)?;
    let before = all.sessions.len();
    all.sessions
        .retain(|s| !(s.host == host && s.workspace_id == workspace_id));
    if all.sessions.len() != before {
        write_sessions_locked(fs, layout, &all)?;
    }
    Ok(())
}

fn write_sessions_locked(
    fs: &dyn FsOps,
    layout: &Layout,
    all: &Sessions,
) -> Result<(), ClientError> {
    fs.create_dir_all(&layout.identity_dir())?;
    doc::write_doc_private(fs, &layout.sessions_path(), all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;

    fn scratch(tag: &str) -> Layout {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-sess-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Layout::new(&dir)
    }

    fn session(ws: &str, name: &str, status: &str) -> Session {
        Session {
            host: "topos.sh".into(),
            base_url: "https://topos.sh/api".into(),
            workspace_id: ws.into(),
            workspace_name: name.into(),
            display_name: name.into(),
            session_id: format!("sn_{ws}"),
            credential: format!("cred-{ws}"),
            status: status.into(),
            logged_in_at: 1,
        }
    }

    #[test]
    fn upsert_replaces_per_workspace_and_survives_reload() {
        let fs = RealFs;
        let layout = scratch("upsert");
        upsert_session(&fs, &layout, session("w_a", "acme", SESSION_ACTIVE)).unwrap();
        upsert_session(&fs, &layout, session("w_b", "beta", SESSION_PENDING)).unwrap();
        // A re-login to acme REPLACES its credential, never duplicates.
        let mut relogin = session("w_a", "acme", SESSION_ACTIVE);
        relogin.credential = "cred-fresh".into();
        upsert_session(&fs, &layout, relogin).unwrap();
        let all = read_sessions(&fs, &layout).unwrap();
        assert_eq!(all.sessions.len(), 2);
        assert_eq!(all.find("acme").unwrap().credential, "cred-fresh");
        assert_eq!(all.find("w_b").unwrap().status, SESSION_PENDING);
        assert_eq!(
            all.find_on_host("topos.sh", "beta").unwrap().workspace_id,
            "w_b"
        );
        assert!(all.find_on_host("elsewhere.dev", "beta").is_none());
    }

    #[test]
    fn resolve_target_is_never_a_silent_guess() {
        let fs = RealFs;
        let layout = scratch("target");
        let all = read_sessions(&fs, &layout).unwrap();
        assert!(all.resolve_target(None).is_err());

        upsert_session(&fs, &layout, session("w_a", "acme", SESSION_ACTIVE)).unwrap();
        let all = read_sessions(&fs, &layout).unwrap();
        assert_eq!(all.resolve_target(None).unwrap().workspace_id, "w_a");

        upsert_session(&fs, &layout, session("w_b", "beta", SESSION_ACTIVE)).unwrap();
        let all = read_sessions(&fs, &layout).unwrap();
        assert!(all.resolve_target(None).is_err());
        assert_eq!(
            all.resolve_target(Some("beta")).unwrap().workspace_id,
            "w_b"
        );
        assert_eq!(
            all.resolve_target(Some("w_a")).unwrap().workspace_name,
            "acme"
        );
        assert!(all.resolve_target(Some("nope")).is_err());
    }

    #[test]
    fn status_flips_and_removal_settle_locally() {
        let fs = RealFs;
        let layout = scratch("flip");
        upsert_session(&fs, &layout, session("w_a", "acme", SESSION_PENDING)).unwrap();
        set_session_status(&fs, &layout, "topos.sh", "w_a", SESSION_ACTIVE).unwrap();
        assert_eq!(
            read_sessions(&fs, &layout)
                .unwrap()
                .find("w_a")
                .unwrap()
                .status,
            SESSION_ACTIVE
        );
        // Another host's same-id workspace is a different session — untouched.
        set_session_status(&fs, &layout, "elsewhere.dev", "w_a", SESSION_PENDING).unwrap();
        assert_eq!(
            read_sessions(&fs, &layout)
                .unwrap()
                .find("w_a")
                .unwrap()
                .status,
            SESSION_ACTIVE
        );
        set_session_status(&fs, &layout, "topos.sh", "w_a", SESSION_ENDED).unwrap();
        // An ended session leaves the fan-out but stays listed (status shows it once).
        let all = read_sessions(&fs, &layout).unwrap();
        assert_eq!(all.live().count(), 0);
        assert_eq!(all.sessions.len(), 1);
        remove_session(&fs, &layout, "elsewhere.dev", "w_a").unwrap();
        assert_eq!(read_sessions(&fs, &layout).unwrap().sessions.len(), 1);
        remove_session(&fs, &layout, "topos.sh", "w_a").unwrap();
        assert!(read_sessions(&fs, &layout).unwrap().sessions.is_empty());
    }

    #[test]
    fn the_document_is_a_0600_secret_and_debug_redacts() {
        let fs = RealFs;
        let layout = scratch("secret");
        let s = session("w_a", "acme", SESSION_ACTIVE);
        assert!(!format!("{s:?}").contains("cred-"), "{s:?}");
        upsert_session(&fs, &layout, s).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(layout.sessions_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "sessions.json must be 0600");
        }
    }
}
