//! `auth` — the sign-in maintenance group: `login` / `logout` / `status`.
//!
//! **`login [server]`** re-runs the SAME device-authorization flow `follow <address>` uses, minus the
//! follow intent: card fetch (default server `https://topos.sh`, `TOPOS_PLANE_URL` override, or the
//! enrolled plane) → re-root onto the declared API base → the one-plane-per-install guard (the
//! wrong-server refusal names the `TOPOS_HOME` second-install hatch) → `POST /v1/device/authorize`
//! toward an enrolled workspace's ADDRESS → the shared WAL/poll/resume idiom → on the granted poll,
//! REPLACE this install's ONE device credential wholesale (a device holds exactly one; the identity
//! is whoever approved in the browser).
//!
//! **`logout`** is two-phase: describe (signing out revokes THIS device on the server — every
//! linked workspace at once), then `--yes` runs ONE global self-revoke (`DELETE /v1/device` — the
//! server deletes the device's links + reported state with it) and deletes
//! `identity/credentials.json`. A uniform-404 answer means the device is already revoked — the
//! local delete proceeds. Skills, follows, and drafts stay; `user.json` keeps the memberships for
//! the re-login UX — no credential IS the signed-out state.
//!
//! **`status`** is side-effect-free: whoami, per-workspace access health (a member-scoped `me`
//! probe — a pending link reads "awaiting owner approval"; the uniform 404 reads "no access —
//! unlinked, removed, or gone"), hook health, reporting posture (`state/sync_status.json`), and
//! the server base.

use serde::Serialize;

use crate::ctx::Ctx;
use crate::enroll;
use crate::error::ClientError;
use crate::plane::{DeviceAuthPoll, GovernanceSource};
use crate::sync_status;

use super::follow::{
    DirectoryConnect, EnrollConnect, guard_one_plane, machine_name, persist_enrollment,
    resolve_api_base,
};
use topos_types::PERSISTED_SCHEMA_VERSION;

/// Builds the governance transport (the logout self-revoke) for a base URL, with a fresh credential.
pub(crate) type GovernanceConnect<'a> =
    dyn Fn(&str) -> Box<dyn crate::plane::GovernanceSource> + 'a;

/// The network seams the auth group needs (mirrors `FollowConnectors` — base URLs are known only
/// after the card / the on-disk instance is read).
pub(crate) struct AuthConnectors<'a> {
    pub enroll: &'a EnrollConnect<'a>,
    pub directory: &'a DirectoryConnect<'a>,
    pub governance: &'a GovernanceConnect<'a>,
    /// The default WEB origin (`TOPOS_PLANE_URL`, else the hosted default) a login dials when no
    /// server is named and none is pinned.
    pub web_origin: String,
}

// =================================================================================================
// login
// =================================================================================================

/// The completed login's `--json` data.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthLoginData {
    /// The API base the login ran against (now the pinned plane).
    pub server: String,
    /// The workspace the approval ran through (the credential it minted serves EVERY workspace the
    /// approving person's seats reach).
    pub workspace_id: String,
    /// The workspace's ADDRESS name.
    pub workspace_name: String,
    pub workspace_display_name: String,
    /// The registered device id (non-secret — the self-revoke handle).
    pub device_id: String,
}

/// A pending login's disclosure (the device-flow wait).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthLoginPending {
    pub server: String,
    pub verification_uri: String,
    pub user_code: String,
    /// The minimum poll interval, in seconds.
    pub interval_secs: u64,
    /// When the device code expires (RFC 3339, UTC) — the honest wait ceiling. ADDITIVE.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// The login outcome: still waiting on the browser, or done.
#[derive(Debug)]
pub(crate) enum AuthLoginOutcome {
    Pending(AuthLoginPending),
    Done(AuthLoginData),
}

/// `auth login [server]` — begin (no WAL) or resume (a pending login WAL) the sign-in. The flow needs
/// a workspace ADDRESS to authorize toward; it comes from the enrolled memberships (`--workspace`
/// picks one when several are joined) — a never-enrolled install joins with `follow <address>`
/// instead (which is the same flow WITH a follow intent).
///
/// # Errors
/// [`ClientError::PlacementUnsupported`] for a different server than the enrolled plane (the
/// wrong-server refusal); [`ClientError::Enrollment`] for a never-enrolled install, a denied/expired
/// flow, or another verb's in-flight enrollment; otherwise transport / io failures.
pub(crate) fn login(
    ctx: &Ctx<'_>,
    connectors: &AuthConnectors<'_>,
    server: Option<&str>,
    workspace: Option<&str>,
) -> Result<AuthLoginOutcome, ClientError> {
    // A pending WAL first: a live LOGIN flow resumes; a follow-owned flow refuses toward `follow`
    // (typed guidance, never a clobbered secret).
    if let Some(wal) = enroll::read_wal(ctx.fs, &ctx.layout)? {
        return match wal.intent {
            enroll::EnrollIntentDoc::Login => resume_login(ctx, connectors, &wal),
            enroll::EnrollIntentDoc::Follow { .. } => Err(ClientError::Enrollment(
                "an enrollment is in progress; re-run `topos follow` to finish it first".into(),
            )),
            enroll::EnrollIntentDoc::Session => Err(ClientError::Enrollment(
                "a login is in progress; re-run `topos login` to finish it first".into(),
            )),
        };
    }

    // The flow authorizes toward a workspace ADDRESS — an enrolled membership supplies it. A fresh
    // install has none: `follow <address>` is the join door (the same flow with a follow intent).
    let user = enroll::read_user(ctx.fs, &ctx.layout)?.unwrap_or_default();
    let membership =
        user.resolve_write_workspace(workspace).map_err(|e| {
            match e {
        ClientError::Enrollment(_) => ClientError::Enrollment(
            "this install has not joined a workspace yet — sign in by joining one: `topos follow \
             <workspace-address>`"
                .into(),
        ),
        other => other,
    }
        })?;

    // Begin: resolve the server origin, card-fetch it for the API base, guard one-plane.
    let origin = server
        .map(|s| s.trim_end_matches('/').to_owned())
        .or(match enroll::read_instance(ctx.fs, &ctx.layout)? {
            Some(i) => Some(i.base_url),
            None => None,
        })
        .unwrap_or_else(|| connectors.web_origin.trim_end_matches('/').to_owned());
    let card = (connectors.enroll)(&origin).fetch_card(&origin)?;
    let base_url = resolve_api_base(&origin, &card.api_base_url)?;
    guard_one_plane(ctx, &base_url)?;

    let start = (connectors.enroll)(&base_url).device_auth_start(
        &membership.name,
        &machine_name(),
        None,
    )?;
    let now = i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX);
    let expires_at = now.saturating_add(
        i64::try_from(start.expires_in_secs.saturating_mul(1000)).unwrap_or(i64::MAX),
    );
    let wal = enroll::PendingEnrollment {
        schema_version: PERSISTED_SCHEMA_VERSION,
        host: String::new(),
        base_url: base_url.clone(),
        workspace_name: membership.name.clone(),
        intent: enroll::EnrollIntentDoc::Login,
        device_code: start.device_code,
        user_code: start.user_code.clone(),
        verification_uri: start.verification_uri.clone(),
        interval_secs: start.interval_secs,
        expires_at_millis: expires_at,
    };
    enroll::write_wal(ctx.fs, &ctx.layout, &wal)?;
    Ok(AuthLoginOutcome::Pending(AuthLoginPending {
        server: base_url,
        verification_uri: start.verification_uri,
        user_code: start.user_code,
        interval_secs: start.interval_secs,
        expires_at: Some(super::follow::fmt_rfc3339_millis(expires_at)),
    }))
}

/// Resume a live login WAL: poll once; granted ⇒ the poll carries the device's ONE credential —
/// persist it (replacing the stored one wholesale) and report.
fn resume_login(
    ctx: &Ctx<'_>,
    connectors: &AuthConnectors<'_>,
    wal: &enroll::PendingEnrollment,
) -> Result<AuthLoginOutcome, ClientError> {
    let enroll_src = (connectors.enroll)(&wal.base_url);
    match enroll_src.device_auth_poll(&wal.device_code)? {
        DeviceAuthPoll::Pending => Ok(AuthLoginOutcome::Pending(AuthLoginPending {
            server: wal.base_url.clone(),
            verification_uri: wal.verification_uri.clone(),
            user_code: wal.user_code.clone(),
            interval_secs: wal.interval_secs,
            expires_at: Some(super::follow::fmt_rfc3339_millis(wal.expires_at_millis)),
        })),
        DeviceAuthPoll::Denied => {
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::Enrollment(
                "the sign-in was denied at the approval page".into(),
            ))
        }
        DeviceAuthPoll::Expired => {
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::Enrollment(
                "the sign-in flow expired; start over with `topos auth login`".into(),
            ))
        }
        DeviceAuthPoll::Granted(grant) => {
            // The same persist the follow door runs (instance / the ONE credential / the membership /
            // the WAL delete / the auto-update trigger) — a login shares every durable step, it just
            // continues into no follow intent.
            persist_enrollment(ctx, &wal.base_url, &grant)?;
            Ok(AuthLoginOutcome::Done(AuthLoginData {
                server: wal.base_url.clone(),
                workspace_id: grant.workspace.workspace_id,
                workspace_name: grant.workspace.name,
                workspace_display_name: grant.workspace.display_name,
                device_id: grant.device_id,
            }))
        }
    }
}

// =================================================================================================
// logout
// =================================================================================================

/// The logout describe — what `--yes` would do (nothing has changed). Signing out is ONE act:
/// revoke this device on the server (every linked workspace at once) + delete the local credential.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthLogoutDescribe {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// The linked workspaces the ONE server-side revoke signs this device out of (disclosure —
    /// the revoke is global, not per-workspace).
    pub workspaces: Vec<String>,
    /// What stays: skills, follows, drafts — signing out never touches a byte.
    pub keeps_note: String,
}

/// The applied logout's `--json` data.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthLogoutData {
    /// Whether the ONE global self-revoke landed server-side (`false` = best-effort miss — the
    /// credential is deleted locally regardless; an already-revoked device counts as `true`).
    pub revoked: bool,
    /// Whether `credentials.json` was deleted (false = there was nothing to delete).
    pub credentials_deleted: bool,
    /// What stayed.
    pub keeps_note: String,
}

/// The logout outcome — the two-phase pair.
#[derive(Debug)]
pub(crate) enum AuthLogoutOutcome {
    Described {
        describe: AuthLogoutDescribe,
        yes_argv: Vec<String>,
    },
    Applied(AuthLogoutData),
}

const LOGOUT_KEEPS: &str =
    "skills, follows, and drafts stay on this machine; `topos auth login` signs back in";

/// `auth logout` — describe (signing out revokes THIS device server-side, across every linked
/// workspace at once), then `--yes`: ONE global self-revoke (`DELETE /v1/device`) + delete the
/// stored credential. A uniform-404 answer means the device is already revoked — the local delete
/// proceeds. Idempotent: signed-out already is a clean success.
///
/// # Errors
/// An io/doc failure reading or deleting the credential doc (the revoke itself is best-effort and
/// never fails the logout).
pub(crate) fn logout(
    ctx: &Ctx<'_>,
    connectors: &AuthConnectors<'_>,
    yes: bool,
) -> Result<AuthLogoutOutcome, ClientError> {
    let user = enroll::read_user(ctx.fs, &ctx.layout)?;
    let creds = enroll::read_credentials(ctx.fs, &ctx.layout)?;
    let workspaces: Vec<String> = user
        .as_ref()
        .map(|u| {
            u.workspaces
                .iter()
                .map(|m| m.workspace_id.clone())
                .collect()
        })
        .unwrap_or_default();

    if !yes {
        return Ok(AuthLogoutOutcome::Described {
            describe: AuthLogoutDescribe {
                principal: user.and_then(|u| u.principal),
                // No credential ⇒ nothing to revoke anywhere (already signed out).
                workspaces: if creds.is_some() {
                    workspaces
                } else {
                    Vec::new()
                },
                keeps_note: LOGOUT_KEEPS.to_owned(),
            },
            yes_argv: vec![
                "topos".to_owned(),
                "auth".to_owned(),
                "logout".to_owned(),
                "--yes".to_owned(),
            ],
        });
    }

    // The ONE global self-revoke, BEFORE the credential is deleted (the revoke authenticates with
    // it; the server deletes the device's links + reported state with the device). Best-effort: a
    // transport failure never blocks the local sign-out, and the uniform 404 means the device is
    // ALREADY revoked server-side — signed out is signed out.
    let mut revoked = false;
    if creds.is_some()
        && let Some(instance) = enroll::read_instance(ctx.fs, &ctx.layout)?
    {
        let governance: Box<dyn GovernanceSource> = (connectors.governance)(&instance.base_url);
        revoked = match governance.revoke_device() {
            Ok(()) => true,
            // Already revoked (or the credential is dead) — the server-side state is what a
            // revoke would have produced; proceed with the local delete.
            Err(ClientError::TargetNotFound { .. }) => true,
            Err(_) => false,
        };
    }

    // Delete the credential — the signed-out state IS its absence. `user.json` keeps the memberships
    // (the re-login UX); follows/skills/drafts are untouched.
    let path = ctx.layout.credentials_path();
    let credentials_deleted = if ctx.fs.exists(&path) {
        let _guard = ctx.fs.lock_exclusive(&ctx.layout.identity_lock_file())?;
        ctx.fs.remove_file(&path)?;
        true
    } else {
        false
    };

    Ok(AuthLogoutOutcome::Applied(AuthLogoutData {
        revoked,
        credentials_deleted,
        keeps_note: LOGOUT_KEEPS.to_owned(),
    }))
}

// =================================================================================================
// status
// =================================================================================================

/// One workspace's access health.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthWorkspaceStatus {
    pub workspace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Whether the device credential is stored (one credential serves every workspace).
    pub credential: bool,
    /// The probe verdict: `healthy` / `no access — revoked or removed` / `unreachable` /
    /// `no credential`.
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
    /// Whether the last delivery is older than the window (the fleet page shows the same verdict).
    pub stale: bool,
}

/// `auth status`'s `--json` data — side-effect-free.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthStatusData {
    /// The pinned plane base, when enrolled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// The registered device id (non-secret), when signed in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// Whether the device credential is stored (the signed-in state).
    pub signed_in: bool,
    pub workspaces: Vec<AuthWorkspaceStatus>,
    /// Whether the session-start auto-update hook is armed in the harness config.
    pub hook_armed: bool,
    pub reporting: Vec<AuthReportingStatus>,
}

/// `auth status` — whoami + per-workspace access probes + hook health + reporting posture. The probe
/// is the member-scoped `me` read under the ONE device credential.
///
/// # Errors
/// An io/doc failure reading the local documents (the probes themselves degrade per workspace).
pub(crate) fn status(
    ctx: &Ctx<'_>,
    connectors: &AuthConnectors<'_>,
) -> Result<AuthStatusData, ClientError> {
    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?;
    let user = enroll::read_user(ctx.fs, &ctx.layout)?;
    let creds = enroll::read_credentials(ctx.fs, &ctx.layout)?;
    let signed_in = creds.is_some();

    let mut probed_principal = None;
    let mut workspaces = Vec::new();
    if let (Some(instance), Some(user)) = (&instance, &user) {
        let directory = (connectors.directory)(&instance.base_url);
        for m in &user.workspaces {
            let (health, role) = if !signed_in {
                ("no credential".to_owned(), None)
            } else {
                match directory.me(&m.workspace_id) {
                    Ok(me) => {
                        // The healthy probe is also the freshest identity disclosure.
                        probed_principal.get_or_insert(me.principal);
                        // A PENDING device↔workspace link answers the member-scoped read but
                        // delivers nothing yet — the wait is the status, not a fault.
                        if me.link_status == "pending" {
                            (
                                "pending — awaiting owner approval".to_owned(),
                                Some(me.role),
                            )
                        } else {
                            ("healthy".to_owned(), Some(me.role))
                        }
                    }
                    // The uniform 404: this device's link, the seat, or the workspace is gone —
                    // indistinguishable by design; `topos follow <address>` relinks.
                    Err(ClientError::TargetNotFound { .. }) => {
                        ("no access — unlinked, removed, or gone".to_owned(), None)
                    }
                    Err(_) => ("unreachable".to_owned(), None),
                }
            };
            workspaces.push(AuthWorkspaceStatus {
                workspace_id: m.workspace_id.clone(),
                display_name: Some(m.display_name.clone()),
                credential: signed_in,
                health,
                role,
            });
        }
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

    Ok(AuthStatusData {
        server: instance.map(|i| i.base_url),
        principal: probed_principal.or(user.as_ref().and_then(|u| u.principal.clone())),
        device_id: creds.map(|c| c.device_id),
        signed_in,
        workspaces,
        // The same probe `list`'s enrollment header uses: the adapter's own trigger-health
        // answer (a config-entry check for the hook adapters; a live scheduler probe for
        // OpenClaw's cron — the footprint is a PATH disclosure, not health).
        hook_armed: ctx.harness.trigger_present(),
        reporting,
    })
}
