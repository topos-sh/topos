//! `auth status` — the one remaining `auth` subcommand (sessions are managed by the top-level
//! `login`/`logout`): side-effect-free — per-SESSION access health (a member-scoped `me` probe
//! under that session's own credential — a pending session reads "awaiting owner approval"; the
//! uniform 404 reads "no access — ended, removed, or gone"), hook health, and the reporting
//! posture (`state/sync_status.json`).

use serde::Serialize;

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::sessions::{self, SESSION_ENDED};
use crate::sync_status;

use super::connect::DirectoryConnect;
use super::reconcile::SessionConnect;

/// The network seams `auth status` needs: the per-session transports (the probes ride each
/// session's OWN credential). The legacy directory connector rides along until the composed rigs
/// finish their session migration.
pub(crate) struct AuthConnectors<'a> {
    #[allow(dead_code)]
    pub directory: &'a DirectoryConnect<'a>,
    pub session: &'a SessionConnect<'a>,
}

/// One session's access health.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthWorkspaceStatus {
    pub workspace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Whether this session's credential is stored locally (always true for a live session row).
    pub credential: bool,
    /// The probe verdict: `healthy` / `pending — awaiting owner approval` / `no access — ended,
    /// removed, or gone` / `unreachable` / `ended`.
    pub health: String,
    /// The role the probe answered (healthy only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// One workspace's reporting posture (from `state/sync_status.json`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthReportingStatus {
    pub workspace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_delivery_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_report_at: Option<i64>,
    pub staleness_window_ms: u64,
    /// Whether the last delivery is older than the window (the sessions page shows the same).
    pub stale: bool,
}

/// `auth status`'s `--json` data — side-effect-free.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthStatusData {
    /// The server base of the first live session, for orientation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// Whether any live session exists (the signed-in state).
    pub signed_in: bool,
    pub workspaces: Vec<AuthWorkspaceStatus>,
    /// Whether the session-start auto-update hook is armed in the harness config.
    pub hook_armed: bool,
    pub reporting: Vec<AuthReportingStatus>,
}

/// `auth status` — per-session access probes + hook health + reporting posture.
///
/// # Errors
/// An io/doc failure reading the local documents (the probes themselves degrade per session).
pub(crate) fn status(
    ctx: &Ctx<'_>,
    connectors: &AuthConnectors<'_>,
) -> Result<AuthStatusData, ClientError> {
    let all = sessions::read_sessions(ctx.fs, &ctx.layout)?;
    let mut probed_principal = None;
    let mut server = None;
    let mut workspaces = Vec::new();
    for s in &all.sessions {
        if server.is_none() {
            server = Some(s.base_url.clone());
        }
        let (health, role) = if s.status == SESSION_ENDED {
            (
                format!(
                    "ended — reconnect with `topos login {}/{}`",
                    s.host, s.workspace_name
                ),
                None,
            )
        } else {
            let transports = (connectors.session)(s);
            match transports.directory.me(&s.workspace_id) {
                Ok(me) => {
                    probed_principal.get_or_insert(me.principal);
                    if me.link_status == "pending" {
                        (
                            "pending — awaiting owner approval".to_owned(),
                            Some(me.role),
                        )
                    } else {
                        ("healthy".to_owned(), Some(me.role))
                    }
                }
                // The uniform 404: the session, the seat, or the workspace is gone —
                // indistinguishable by design; `topos login <address>` reconnects.
                Err(ClientError::TargetNotFound { .. }) => {
                    ("no access — ended, removed, or gone".to_owned(), None)
                }
                Err(_) => ("unreachable".to_owned(), None),
            }
        };
        workspaces.push(AuthWorkspaceStatus {
            workspace_id: s.workspace_id.clone(),
            display_name: Some(s.display_name.clone()),
            credential: true,
            health,
            role,
        });
    }

    let status_doc = sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();
    let now = i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX);
    let reporting = status_doc
        .workspaces
        .iter()
        .map(|(ws, e)| AuthReportingStatus {
            workspace_id: ws.clone(),
            last_delivery_at: e.last_delivery_at,
            last_report_at: e.last_report_at,
            staleness_window_ms: e.staleness_window_ms,
            stale: sync_status::is_stale(Some(e), now),
        })
        .collect();

    let signed_in = all.live().count() > 0;
    Ok(AuthStatusData {
        server,
        principal: probed_principal,
        signed_in,
        workspaces,
        // The same probe `list`'s header uses: the adapter's own trigger-health answer.
        hook_armed: ctx.harness.trigger_present(),
        reporting,
    })
}
