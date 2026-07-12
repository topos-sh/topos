//! `auth` — the sign-in maintenance group: `login` / `logout` / `status`.
//!
//! **`login [server]`** runs the LOGIN device flow (default server `https://topos.sh`,
//! `TOPOS_PLANE_URL` override, or the enrolled plane): card fetch → re-root onto the declared API
//! base → the one-plane-per-install guard (the wrong-server refusal names the `TOPOS_HOME`
//! second-install hatch) → `device/authorize` with `intent = "login"` → the shared WAL/poll/resume
//! idiom → `POST /v1/login`, which re-mints ONE workspace credential per confirmed seat. A DIFFERENT
//! account than `user.json`'s principal requires `--yes` to replace the stored credentials
//! wholesale; a same-account re-login is an idempotent re-mint.
//!
//! **`logout`** is two-phase: describe, then `--yes` best-effort revokes THIS device in every
//! enrolled workspace (the governance revoke with the device's own key id as the target) and
//! deletes `identity/credentials.json`. Skills, follows, and drafts stay; `user.json` keeps the
//! principal for the re-login UX — no credentials IS the signed-out state.
//!
//! **`status`** is side-effect-free: whoami, per-workspace credential health (a `GET /me` probe —
//! the uniform 404 reads "no access — revoked or removed"), hook health, reporting posture
//! (`state/sync_status.json`), and the server base.

use serde::Serialize;

use crate::ctx::Ctx;
use crate::device_signer::DeviceSigner;
use crate::enroll::{self, CredentialEntry, Membership};
use crate::error::ClientError;
use crate::plane::{Card, GovernanceSource, LoginRedeem, TokenPoll};
use crate::sync_status;

use super::follow::{
    DirectoryConnect, EnrollConnect, complete_uri, device_fingerprint, machine_name,
    resolve_api_base, wrong_server_refusal,
};
use topos_types::PERSISTED_SCHEMA_VERSION;
use topos_types::bootstrap::VerifiedDomainStatus;

/// Builds the governance transport (the logout self-revoke) for a base URL, with fresh credentials.
pub(crate) type GovernanceConnect<'a> =
    dyn Fn(&str) -> Box<dyn crate::plane::GovernanceSource> + 'a;

/// The network seams the auth group needs (mirrors `FollowConnectors` — base URLs are known only
/// after the card / the on-disk instance is read).
pub(crate) struct AuthConnectors<'a> {
    pub enroll: &'a EnrollConnect<'a>,
    pub directory: &'a DirectoryConnect<'a>,
    pub governance: &'a GovernanceConnect<'a>,
    /// The default WEB origin (`TOPOS_PLANE_URL`, else the hosted default) a fresh install dials.
    pub web_origin: String,
}

// =================================================================================================
// login
// =================================================================================================

/// One workspace seat a login reported.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthLoginMembership {
    pub workspace_id: String,
    /// The workspace's ADDRESS name.
    pub name: String,
    pub display_name: String,
    pub role: String,
    /// Whether a fresh credential was minted for this device here.
    pub minted: bool,
    /// Why not, when it wasn't (e.g. this device is revoked there).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

/// The completed login's `--json` data.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthLoginData {
    /// The API base the login ran against (now the pinned plane).
    pub server: String,
    /// The proven principal.
    pub principal: String,
    /// The principal the login REPLACED (`--yes` on a different account); absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaced_principal: Option<String>,
    /// One entry per confirmed seat (minted or blocked).
    pub memberships: Vec<AuthLoginMembership>,
}

/// A pending login's disclosure (the device-flow wait).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthLoginPending {
    pub server: String,
    pub verification_uri_complete: String,
    pub user_code: String,
    pub device_fingerprint: String,
}

/// The login outcome: still waiting on the browser, or done.
#[derive(Debug)]
pub(crate) enum AuthLoginOutcome {
    Pending(AuthLoginPending),
    Done(AuthLoginData),
}

/// `auth login [server]` — begin (no WAL) or resume (a pending login WAL) the sign-in.
///
/// # Errors
/// [`ClientError::PlacementUnsupported`] for a different server than the enrolled plane (the
/// wrong-server refusal); [`ClientError::ConfirmFirst`] when a different account needs `--yes`;
/// [`ClientError::Enrollment`] for a denied/expired/refused session or another in-flight enrollment;
/// otherwise transport / io failures.
pub(crate) fn login(
    ctx: &Ctx<'_>,
    connectors: &AuthConnectors<'_>,
    server: Option<&str>,
    yes: bool,
) -> Result<AuthLoginOutcome, ClientError> {
    // A pending WAL first: a live LOGIN session resumes; any other in-flight enrollment owns the
    // shared WAL slot (typed guidance, never a clobbered secret).
    if let Some(wal) = enroll::read_wal(ctx.fs, &ctx.layout)? {
        return match wal.state {
            enroll::EnrollPhase::AuthorizingLogin {
                base_url,
                device_code,
                user_code,
                verification_uri_complete,
                ..
            } => resume_login(
                ctx,
                connectors,
                yes,
                &base_url,
                &device_code,
                &user_code,
                verification_uri_complete,
            ),
            enroll::EnrollPhase::AuthorizingStandup { .. } => Err(ClientError::Enrollment(
                "a workspace standup is in progress; re-run the `topos publish …` command that \
                 started it first"
                    .into(),
            )),
            _ => Err(ClientError::Enrollment(
                "an enrollment is in progress; re-run `topos follow` to finish it first".into(),
            )),
        };
    }

    // Begin: resolve the server origin, card-fetch it for the API base, guard one-plane.
    let origin = server
        .map(|s| s.trim_end_matches('/').to_owned())
        .or(match enroll::read_instance(ctx.fs, &ctx.layout)? {
            Some(i) => Some(i.base_url),
            None => None,
        })
        .unwrap_or_else(|| connectors.web_origin.trim_end_matches('/').to_owned());
    let base_url = match (connectors.enroll)(&origin).fetch_card(&origin)? {
        Card::Protocol(card) => resolve_api_base(&origin, &card.api_base_url)?,
        Card::Claim(_) => {
            return Err(ClientError::Enrollment(
                "this address answered a claim bootstrap — pass the /i/ claim link to `topos \
                 follow` instead of signing in"
                    .into(),
            ));
        }
    };
    if let Some(instance) = enroll::read_instance(ctx.fs, &ctx.layout)?
        && instance.base_url != base_url
    {
        return Err(wrong_server_refusal(&instance.base_url));
    }

    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
    let auth = (connectors.enroll)(&base_url)
        .device_authorize_login(signer.public_key(), &machine_name(&signer))?;
    let complete = auth
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| complete_uri(&auth.verification_uri, &auth.user_code));
    let now = i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX);
    let expires_at =
        now.saturating_add(i64::try_from(auth.expires_in.saturating_mul(1000)).unwrap_or(i64::MAX));
    enroll::write_wal(
        ctx.fs,
        &ctx.layout,
        &enroll::PendingEnrollment {
            schema_version: PERSISTED_SCHEMA_VERSION,
            state: enroll::EnrollPhase::AuthorizingLogin {
                base_url: base_url.clone(),
                device_code: auth.device_code.clone(),
                user_code: auth.user_code.clone(),
                verification_uri_complete: complete.clone(),
                expires_at_millis: expires_at,
            },
        },
    )?;
    Ok(AuthLoginOutcome::Pending(AuthLoginPending {
        server: base_url,
        verification_uri_complete: complete,
        user_code: auth.user_code,
        device_fingerprint: device_fingerprint(&signer),
    }))
}

/// Resume a live login WAL: poll once; granted ⇒ redeem at `POST /v1/login` and finalize.
fn resume_login(
    ctx: &Ctx<'_>,
    connectors: &AuthConnectors<'_>,
    yes: bool,
    base_url: &str,
    device_code: &str,
    user_code: &str,
    verification_uri_complete: String,
) -> Result<AuthLoginOutcome, ClientError> {
    let enroll_src = (connectors.enroll)(base_url);
    match enroll_src.poll_token(device_code)? {
        TokenPoll::Pending | TokenPoll::SlowDown => {
            let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
            Ok(AuthLoginOutcome::Pending(AuthLoginPending {
                server: base_url.to_owned(),
                verification_uri_complete,
                user_code: user_code.to_owned(),
                device_fingerprint: device_fingerprint(&signer),
            }))
        }
        TokenPoll::Denied => {
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::Enrollment(
                "the sign-in was denied at the verification page".into(),
            ))
        }
        TokenPoll::Expired => {
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::Enrollment(
                "the sign-in session expired; start over with `topos auth login`".into(),
            ))
        }
        TokenPoll::Granted(granted) => {
            let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
            let redeem = enroll_src.login_redeem(granted.grant.as_str(), signer.public_key())?;
            finalize_login(ctx, base_url, redeem, yes)
        }
    }
}

/// The login promote: the different-account gate, then the sidecar writes (instance / credentials /
/// user) and the WAL delete.
fn finalize_login(
    ctx: &Ctx<'_>,
    base_url: &str,
    redeem: LoginRedeem,
    yes: bool,
) -> Result<AuthLoginOutcome, ClientError> {
    let prior = enroll::read_user(ctx.fs, &ctx.layout)?;
    let prior_principal = prior.as_ref().and_then(|u| u.principal.clone());
    let switching = prior_principal
        .as_deref()
        .is_some_and(|p| p != redeem.principal);
    if switching && !yes {
        // The grant is spent either way — the session settled; only the WRITES are gated. The WAL
        // is cleared so the re-run starts a fresh sign-in rather than re-polling a dead session.
        enroll::delete_wal(ctx.fs, &ctx.layout)?;
        return Err(ClientError::ConfirmFirst(format!(
            "this install is signed in as {} — signing in as {} replaces every stored workspace \
             credential; re-run `topos auth login --yes` to switch accounts (skills and drafts \
             stay)",
            prior_principal.as_deref().unwrap_or("(unknown)"),
            redeem.principal
        )));
    }

    // 1) instance.json — pin the plane (idempotent bytes when already pinned to this base).
    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.unwrap_or(enroll::Instance {
        schema_version: PERSISTED_SCHEMA_VERSION,
        base_url: base_url.to_owned(),
        deployment_mode: topos_types::bootstrap::DeploymentMode::SelfHost,
        enrollment_method: "device_code".to_owned(),
    });
    enroll::write_instance(ctx.fs, &ctx.layout, &instance)?;

    // 2) credentials.json — the minted seats. An account SWITCH replaces the set wholesale (the old
    //    account's credentials must not linger); a same-account re-login upserts per workspace (a
    //    seat the login did not name — e.g. a blocked one — keeps its old credential).
    let minted: Vec<CredentialEntry> = redeem
        .seats
        .iter()
        .filter_map(|s| {
            s.credential.as_ref().map(|c| CredentialEntry {
                workspace_id: s.workspace_id.clone(),
                credential: c.clone(),
            })
        })
        .collect();
    if switching {
        enroll::replace_credentials(ctx.fs, &ctx.layout, minted)?;
    } else {
        for e in &minted {
            enroll::write_credential(ctx.fs, &ctx.layout, &e.workspace_id, &e.credential)?;
        }
    }

    // 3) user.json — the proven principal + one membership per confirmed seat. A switch REPLACES
    //    the membership list (the seats are the new account's whole truth); a re-login upserts.
    let now = i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX);
    let mut user = if switching {
        enroll::UserDoc {
            schema_version: PERSISTED_SCHEMA_VERSION,
            email: None,
            principal: None,
            workspaces: Vec::new(),
        }
    } else {
        prior.unwrap_or(enroll::UserDoc {
            schema_version: PERSISTED_SCHEMA_VERSION,
            email: None,
            principal: None,
            workspaces: Vec::new(),
        })
    };
    user.schema_version = PERSISTED_SCHEMA_VERSION;
    user.principal = Some(redeem.principal.clone());
    if redeem.principal.contains('@') {
        user.email = Some(redeem.principal.clone());
    }
    for seat in &redeem.seats {
        enroll::upsert_membership(
            &mut user,
            Membership {
                workspace_id: seat.workspace_id.clone(),
                display_name: Some(seat.display_name.clone()),
                roles: vec![seat.role.clone()],
                verified_domain: None,
                verified_domain_status: VerifiedDomainStatus::Unverified,
                invite_rooted: false,
                enrolled_at: now,
            },
        );
    }
    enroll::write_user(ctx.fs, &ctx.layout, &user)?;
    enroll::delete_wal(ctx.fs, &ctx.layout)?;

    Ok(AuthLoginOutcome::Done(AuthLoginData {
        server: base_url.to_owned(),
        principal: redeem.principal,
        replaced_principal: switching.then_some(prior_principal).flatten(),
        memberships: redeem
            .seats
            .into_iter()
            .map(|s| AuthLoginMembership {
                minted: s.credential.is_some(),
                workspace_id: s.workspace_id,
                name: s.name,
                display_name: s.display_name,
                role: s.role,
                blocked: s.blocked,
            })
            .collect(),
    }))
}

// =================================================================================================
// logout
// =================================================================================================

/// The logout describe — what `--yes` would do (nothing has changed).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthLogoutDescribe {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// The workspaces whose credential would be deleted (this device revoked there, best-effort).
    pub workspaces: Vec<String>,
    /// What stays: skills, follows, drafts — signing out never touches a byte.
    pub keeps_note: String,
}

/// The applied logout's `--json` data.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthLogoutData {
    /// Workspaces where the self-revoke landed.
    pub revoked: Vec<String>,
    /// Workspaces where it did not (best-effort — the credential is deleted regardless).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub revoke_failed: Vec<String>,
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

/// `auth logout` — describe, then `--yes`: best-effort self device-revoke per enrolled workspace,
/// then delete the stored credentials. Idempotent: signed-out already is a clean success.
///
/// # Errors
/// An io/doc failure reading or deleting the credential doc (the revokes themselves are
/// best-effort and never fail the logout).
pub(crate) fn logout(
    ctx: &Ctx<'_>,
    connectors: &AuthConnectors<'_>,
    yes: bool,
) -> Result<AuthLogoutOutcome, ClientError> {
    let user = enroll::read_user(ctx.fs, &ctx.layout)?;
    let creds = enroll::read_credentials(ctx.fs, &ctx.layout)?;
    let workspaces: Vec<String> = creds
        .as_ref()
        .map(|c| {
            c.credentials
                .iter()
                .map(|e| e.workspace_id.clone())
                .collect()
        })
        .unwrap_or_default();

    if !yes {
        return Ok(AuthLogoutOutcome::Described {
            describe: AuthLogoutDescribe {
                principal: user.and_then(|u| u.principal),
                workspaces,
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

    // Best-effort self-revoke per enrolled workspace, BEFORE the credential is deleted (the revoke
    // authenticates with it). A failure never blocks the local sign-out.
    let mut revoked = Vec::new();
    let mut revoke_failed = Vec::new();
    if !workspaces.is_empty()
        && let Some(instance) = enroll::read_instance(ctx.fs, &ctx.layout)?
    {
        let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
        let governance: Box<dyn GovernanceSource> = (connectors.governance)(&instance.base_url);
        for ws in &workspaces {
            let op_id = uuid::Uuid::from_bytes(ctx.ids.new_op_id())
                .hyphenated()
                .to_string();
            match governance.revoke_device(ws, signer.device_key_id(), &op_id) {
                Ok(()) => revoked.push(ws.clone()),
                Err(_) => revoke_failed.push(ws.clone()),
            }
        }
    }

    // Delete the credentials — the signed-out state IS their absence. `user.json` keeps the
    // principal (the re-login UX); follows/skills/drafts are untouched.
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
        revoke_failed,
        credentials_deleted,
        keeps_note: LOGOUT_KEEPS.to_owned(),
    }))
}

// =================================================================================================
// status
// =================================================================================================

/// One workspace's credential health.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthWorkspaceStatus {
    pub workspace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Whether a credential is stored for this workspace.
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Whether any workspace credential is stored (the signed-in state).
    pub signed_in: bool,
    pub workspaces: Vec<AuthWorkspaceStatus>,
    /// Whether the session-start currency hook is armed in the harness config.
    pub hook_armed: bool,
    pub reporting: Vec<AuthReportingStatus>,
}

/// `auth status` — whoami + per-workspace credential probes + hook health + reporting posture.
///
/// # Errors
/// An io/doc failure reading the local documents (the probes themselves degrade per workspace).
pub(crate) fn status(
    ctx: &Ctx<'_>,
    connectors: &AuthConnectors<'_>,
) -> Result<AuthStatusData, ClientError> {
    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?;
    let user = enroll::read_user(ctx.fs, &ctx.layout)?;
    let creds: std::collections::HashSet<String> = enroll::read_credentials(ctx.fs, &ctx.layout)?
        .map(|c| c.credentials.into_iter().map(|e| e.workspace_id).collect())
        .unwrap_or_default();

    let mut workspaces = Vec::new();
    if let (Some(instance), Some(user)) = (&instance, &user) {
        let directory = (connectors.directory)(&instance.base_url);
        for m in &user.workspaces {
            let has_cred = creds.contains(&m.workspace_id);
            let (health, role) = if !has_cred {
                ("no credential".to_owned(), None)
            } else {
                match directory.me(&m.workspace_id) {
                    Ok(me) => ("healthy".to_owned(), Some(me.role)),
                    // The uniform 404: this device (or the person) lost the workspace.
                    Err(ClientError::TargetNotFound { .. }) => {
                        ("no access — revoked or removed".to_owned(), None)
                    }
                    Err(_) => ("unreachable".to_owned(), None),
                }
            };
            workspaces.push(AuthWorkspaceStatus {
                workspace_id: m.workspace_id.clone(),
                display_name: m.display_name.clone(),
                credential: has_cred,
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
        principal: user.as_ref().and_then(|u| u.principal.clone()),
        email: user.as_ref().and_then(|u| u.email.clone()),
        signed_in: !creds.is_empty(),
        workspaces,
        // The same probe `list`'s enrollment header uses: the adapter holds a config entry iff the
        // trigger is armed.
        hook_armed: !ctx.harness.uninstall_footprint().is_empty(),
        reporting,
    })
}
